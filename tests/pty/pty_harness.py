"""Reusable real-PTY harness for the Remux client TUI.

Drives the actual client binary through a pseudo-terminal (pexpect) and reads
the rendered screen with pyte. Spins up an isolated throwaway server (the client
auto-spawns it) with its own XDG dirs and a short socket path.
"""
import os, shutil, time, pexpect, pyte

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
PREFIX = b"\x01"  # Ctrl-a


def _protocol_version():
    """Read `PROTOCOL_VERSION` out of the source rather than restating it here.

    The frame harness learned this the expensive way and the PTY side inherits
    the lesson: a hard-coded copy goes stale SILENTLY, because the server is
    deliberately lenient about skew (it logs the mismatch and proceeds, see
    CLAUDE.md). A harness announcing v6 against a v10 server therefore keeps
    passing while claiming to speak a protocol that no longer exists -- it is
    not testing the wire it says it is. Reading the number is what makes it
    true.
    """
    src = os.path.join(
        os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
        "src", "protocol.rs")
    for line in open(src):
        if "pub const PROTOCOL_VERSION" in line:
            return int(line.split("=")[1].strip().rstrip(";"))
    raise SystemExit("could not read PROTOCOL_VERSION from src/protocol.rs")


PROTOCOL_VERSION = _protocol_version()


class Tui:
    def __init__(self, rundir, cols=120, rows=40, config=None):
        self.rundir = rundir
        self.cols = cols
        self.rows = rows
        self.child = None
        # Optional `config.toml` body. Written into the isolated
        # XDG_CONFIG_HOME, which the client AND the server it auto-spawns both
        # read -- so one file themes both sides of a parity comparison.
        self.config = config

    def start(self):
        shutil.rmtree(self.rundir, ignore_errors=True)
        for s in ("run", "state", "data", "config"):
            os.makedirs(f"{self.rundir}/{s}", exist_ok=True)
        if self.config is not None:
            os.makedirs(f"{self.rundir}/config/remux", exist_ok=True)
            with open(f"{self.rundir}/config/remux/config.toml", "w") as f:
                f.write(self.config)
        env = {
            **os.environ,
            "XDG_RUNTIME_DIR": f"{self.rundir}/run",
            "XDG_STATE_HOME": f"{self.rundir}/state",
            "XDG_DATA_HOME": f"{self.rundir}/data",
            "XDG_CONFIG_HOME": f"{self.rundir}/config",
            "SHELL": "/bin/sh",
            "ENV": "/dev/null",
            "TERM": "xterm-256color",
            "PS1": "$ ",
            "REMUX_ALLOW_NESTED": "1",
        }
        self.screen = pyte.Screen(self.cols, self.rows)
        self.stream = pyte.ByteStream(self.screen)
        # Raw copy of everything the client wrote. pyte drops the sequences it
        # does not model, and OSC 52 (the clipboard write a yank performs) is one
        # of them -- so a test that wants to assert on the YANKED text has to
        # read it out of the byte stream itself. See `yanks()`.
        self.raw = bytearray()
        self.child = pexpect.spawn(
            BIN, [], env=env, dimensions=(self.rows, self.cols), encoding=None,
        )
        self.pump(1.2)
        return self

    def pump(self, t=0.5):
        end = time.time() + t
        while time.time() < end:
            try:
                data = self.child.read_nonblocking(65536, 0.1)
                if data:
                    self.raw.extend(data)
                    self.stream.feed(data)
            except Exception:
                pass

    def rows_text(self):
        return self.screen.display

    def dump(self, label=""):
        print(f"----- screen {label} -----")
        for i, r in enumerate(self.rows_text()):
            print(f"{i:2} |{r.rstrip()}")
        print("-------------------------")

    def send(self, data, t=0.4):
        if isinstance(data, str):
            data = data.encode()
        self.child.send(data)
        self.pump(t)

    def prefix(self, keys, t=0.4):
        """Send Ctrl-a then the given key bytes."""
        self.child.send(PREFIX)
        time.sleep(0.15)
        self.send(keys, t)

    def resize(self, cols, rows, t=1.2):
        """Resize the real pseudo-terminal (the client gets a genuine SIGWINCH).

        The pyte screen is resized with it, so `rows_text()`/`screen.buffer`
        keep matching what the client is now painting.
        """
        self.cols, self.rows = cols, rows
        self.child.setwinsize(rows, cols)
        self.screen.resize(rows, cols)
        self.pump(t)

    def alive(self):
        return self.child.isalive()

    def kill(self):
        try:
            self.child.terminate(force=True)
        except Exception:
            pass

    def log(self, which="client"):
        p = f"{self.rundir}/state/remux/{which}.log"
        return open(p).read() if os.path.exists(p) else ""

    def find_row(self, needle):
        for i, r in enumerate(self.rows_text()):
            if needle in r:
                return i
        return -1

    def has(self, needle):
        return any(needle in r for r in self.rows_text())

    def yanks(self):
        """Every clipboard write the client made, decoded, oldest first.

        `copy_to_clipboard` emits OSC 52 (`ESC ] 52 ; c ; <base64> BEL`), so this
        is what a real terminal would have put on the clipboard.
        """
        import base64, re
        out = []
        for m in re.finditer(rb"\x1b\]52;[^;]*;([A-Za-z0-9+/=]*)(?:\x07|\x1b\\)", bytes(self.raw)):
            try:
                out.append(base64.b64decode(m.group(1)).decode("utf-8", "replace"))
            except Exception:
                pass
        return out


# ---------------------------------------------------------------------------
# Session-manager navigation
# ---------------------------------------------------------------------------
#
# Sixteen View harnesses used to compose their View by firing a fixed keystroke
# sequence at the manager from an ASSUMED cursor position -- `Tab` out of the
# search bar, then a counted run of `j` from row 0. Both assumptions were
# properties of the overlay, not of the tree: the manager now opens ON the tree
# (so `Tab` is a no-op) with the highlight snapped to the current session (so
# the counts are off), and all sixteen silently marked the WRONG panes and
# built a View out of them. A count is the wrong primitive -- re-deriving it
# against today's snap position would break the same sixteen on the next
# selection change, and far from the cause.
#
# So: never assume where the cursor is, READ it. These helpers locate the popup
# on the rendered screen, identify the highlighted row by its inverted
# background (theme-independent -- it is the row whose background differs from
# its siblings'), and step `j` until the selection is on the row they were asked
# for, asserting what they landed on before acting on it. A future selection
# change makes them re-navigate rather than mis-mark; a genuine break names the
# row it is on.
#
# They take the client as their first argument rather than living on `Tui`
# because `shared_views_two_client.py` drives two clients through its own
# `Client` class. Everything here needs only `rows_text()`, `screen`, `rows`,
# `send()`, `pump()` and `prefix()`, which both provide.

BOX_TL = "╭"          # popup top-left corner
BOX_TR = "╮"          # popup top-right corner
BOX_V = "│"           # popup side border
MARK_GLYPH = "●"      # the filled circle a marked pane row carries
EXPANDED = "▼"        # an expanded node's triangle


class SmRow:
    """One rendered row of the session-manager tree."""

    def __init__(self, y, text, selected):
        self.y = y
        self.text = text
        self.selected = selected

    @property
    def blank(self):
        return not self.text.strip()

    @property
    def marked(self):
        return self.text.strip().startswith(MARK_GLYPH)

    @property
    def depth(self):
        """How deep this row sits in the tree, in screen columns.

        Leading blanks alone would be wrong: a marked pane spends its two
        indent blanks on the `[MARK] ` glyph, so it would read two columns
        SHALLOWER than its unmarked siblings and stop looking like a child of
        its tab. Adding those two back makes the number strictly increase with
        tree depth for every row shape -- server, session, tab, pane, marked
        pane alike -- which is all any caller here asks of it.
        """
        lead = len(self.text) - len(self.text.lstrip(" "))
        return lead + 2 if self.text.lstrip(" ").startswith(MARK_GLYPH + " ") else lead

    def __repr__(self):
        flag = "*" if self.selected else " "
        return f"<{flag}{self.y:2} {self.text.rstrip()!r}>"


def sm_title(t):
    """The manager's top border line, which carries the marked count."""
    for r in t.rows_text():
        if "Session Manager" in r:
            return r
    raise AssertionError("session manager is not on screen")


def sm_tree(t):
    """Every tree row the manager is currently painting, top to bottom.

    The popup is found by its title line, its columns by that line's corners --
    so this is immune to the popup moving or resizing with the terminal.
    """
    rows = t.rows_text()
    title_y = None
    for i, r in enumerate(rows):
        if "Session Manager" in r and BOX_TL in r:
            title_y = i
            break
    if title_y is None:
        raise AssertionError("session manager popup is not on screen")
    line = rows[title_y]
    x0 = line.index(BOX_TL)
    x1 = line.rindex(BOX_TR)
    out = []
    # +3: the top border, the search row and the search separator.
    y = title_y + 3
    while y < len(rows) and rows[y][x0] == BOX_V:
        out.append([y, rows[y][x0 + 1:x1], t.screen.buffer[y][x0 + 1].bg])
        y += 1
    if not out:
        raise AssertionError("session manager popup has no tree rows")
    # The highlighted row is the one whose background differs from the others'.
    # Reading it rather than hard-coding the selection color keeps this working
    # under the custom themes some harnesses configure.
    bgs = [bg for _, _, bg in out]
    normal = max(set(bgs), key=bgs.count)
    return [SmRow(y, text, bg != normal) for y, text, bg in out]


def sm_selected(t):
    """The highlighted tree row. Raises unless exactly one row is highlighted."""
    tree = sm_tree(t)
    hits = [r for r in tree if r.selected]
    if len(hits) != 1:
        raise AssertionError(
            f"expected exactly one highlighted row, found {len(hits)}: {tree}"
        )
    return hits[0]


def sm_open(t, timeout=6.0):
    """Open the manager (Prefix+x m) and wait for a tree with a live selection.

    The wait is not politeness. The overlay opens before it has any rows (they
    arrive on a server push) and the snap to the current session happens on the
    first push that carries one -- but ANY keystroke the overlay handles freezes
    the selection where it is. Typing into a not-yet-populated manager therefore
    pins the cursor at row 0, and the opening position becomes a race. Waiting
    for a painted tree is what makes it deterministic.
    """
    t.prefix(b"xm", 0.5)
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            tree = sm_tree(t)
            sm_selected(t)
        except AssertionError as e:
            last = e
        else:
            if any(not r.blank for r in tree):
                return tree
        t.pump(0.2)
    raise AssertionError(f"session manager never painted a tree ({last})")


def sm_goto(t, pred, what):
    """Press `j` until the highlighted row satisfies `pred`, and return it.

    One key at a time, re-reading the screen between presses, so a wrapped or
    scrolled tree is handled by construction and a miss reports the row it
    actually stopped on instead of marking it.
    """
    tree = sm_tree(t)
    for _ in range(len(tree) + 1):
        row = sm_selected(t)
        if pred(row):
            return row
        t.send("j", 0.15)
    raise AssertionError(
        f"never reached {what}; stopped on {sm_selected(t)!r}, tree was {sm_tree(t)}"
    )


def sm_goto_text(t, needle):
    """Move the highlight onto the first row whose text contains `needle`."""
    return sm_goto(t, lambda r: needle in r.text, f"a row containing {needle!r}")


def sm_children(t, parent):
    """The rows nested under `parent`: everything below it that is deeper."""
    tree = sm_tree(t)
    idx = next(i for i, r in enumerate(tree) if r.y == parent.y)
    kids = []
    for row in tree[idx + 1:]:
        if row.blank or row.depth <= parent.depth:
            break
        kids.append(row)
    return kids


def sm_expand(t, needle):
    """Put the highlight on `needle`, expand it, and return its child rows."""
    row = sm_goto_text(t, needle)
    if EXPANDED not in row.text:
        t.send("l", 0.4)
    row = sm_selected(t)
    assert EXPANDED in row.text, f"{needle!r} did not expand: {row!r}"
    kids = sm_children(t, row)
    assert kids, f"{needle!r} expanded to no children: {sm_tree(t)}"
    return kids


def sm_mark(t, row, expect_total):
    """Move onto `row`, check it is still that row, mark it, check the mark took.

    `expect_total` is the number of marks there should be afterwards -- the
    manager puts it in its own title, so the assertion reads the renderer's own
    account of what is marked rather than trusting the keystroke.
    """
    landed = sm_goto(t, lambda r: r.y == row.y, f"row {row.y} ({row.text.strip()!r})")
    assert landed.text == row.text, (
        f"row {row.y} was {row.text.strip()!r} and is now {landed.text.strip()!r} "
        "-- the tree moved under the cursor"
    )
    t.send(" ", 0.3)
    assert sm_selected(t).marked, f"space did not mark {landed!r}"
    want = f"({expect_total} marked)"
    assert want in sm_title(t), f"title is {sm_title(t).strip()!r}, wanted {want!r}"


def sm_mark_panes(t, tab="Tab 1", panes=(0, 1)):
    """Open the manager, expand `tab`, and mark the panes at those indices.

    Returns the marked rows, in the order they were marked.
    """
    sm_open(t)
    kids = sm_expand(t, tab)
    assert max(panes) < len(kids), (
        f"{tab!r} has {len(kids)} panes, cannot mark {panes}: {kids}"
    )
    marked = []
    for want in panes:
        sm_mark(t, kids[want], len(marked) + 1)
        marked.append(kids[want])
    return marked


def sm_add_to_view(t, existing=False, settle=1.0):
    """`v a` -> the view picker; Enter creates a new view, or joins the last one.

    The picker opens on its "New view" sentinel, so one `k` wraps to the last
    existing view -- which is what "add these panes to the view I already have"
    means here.
    """
    t.send("v", 0.2)
    t.send("a", 0.6)
    assert t.has("Add Pane to View"), "the view picker never opened"
    if existing:
        t.send("k", 0.3)
    t.send("\r", settle)


def sm_compose_view(t, tab="Tab 1", panes=(0, 1), existing=False, settle=1.0):
    """Compose a View out of the given panes of `tab` and enter it."""
    marked = sm_mark_panes(t, tab=tab, panes=panes)
    sm_add_to_view(t, existing=existing, settle=settle)
    return marked
