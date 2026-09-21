"""Disconnecting a remote from the session manager.

User ask: "in the session manager, the ability to disconnect a remote so that it
doesn't appear in the session switcher any longer."

Everything runs through the FAKE REMOTE recipe (a second isolated server plus an
`ssh` shim that execs `remux relay`), so no real SSH is involved. The shim
APPENDS a line to a file every time it is invoked, which is what makes "did this
dial the remote again?" an observation rather than an inference -- the whole point
of `RemoteState::Disconnected` is that nothing redials behind the user's back, and
a screen label cannot tell you that.

Eight cases, in this order on purpose:

  (1) `d` on the connected remote's server row arms ` Disconnect mini? (y/n) `;
      `n` cancels and the remote is still connected.
  (2) `d` then `y`: the remote's subtree goes, the row reads `(disconnected)`,
      and the quick switcher stops listing the remote's sessions while still
      listing the local one (the positive control -- an empty switcher would pass
      a bare "rbox is absent" check).
  (3) the ssh pipe's EOF lands a moment later as an `Incoming::Closed`: the row
      must STILL read `(disconnected)`, not `(failed: connection lost)`.
  (4) Enter on the row reconnects: the subtree returns and the shim's dial count
      goes 1 -> 2. This is what proves the counter can move at all, so it must
      come before case 5 leans on the count NOT moving.
  (5) the Views hazard: a view cell aliasing a pane on the remote is re-subscribed
      on every layout/focus pass, which is exactly where a lazy redial would come
      from. After a disconnect the cell must show a disconnected label and the
      dial count must not budge.
  (6) the remote is the FOREGROUND when it is disconnected: the client falls back
      to a local session and stays alive.
  (7) the `RemoteDisconnect` command reaches the same body through the palette.
  (8) `d`, `y`, Enter with nothing between them: the reconnect installs a new
      transport while the killed ssh is still reporting EOF, and that superseded
      `Closed` must not tear down the live connection.

Which assertion guards what, established by breaking each guard and watching this
run -- not by reading the code:

  * `disconnect_remote` made a no-op still leaves case 2's "subtree is gone" and
    "switcher no longer lists it" GREEN. Those two are the user's stated ask, but
    they do not discriminate the new state: the shared cleanup runs either way and
    `fail_remote` removes the writer, so a `Failed` remote also leaves the tree
    and the switcher. What discriminates `Disconnected` is the row LABEL, case 3,
    and case 5's dial count.
  * case 5's dial count is guarded TWICE over, and no single deletion turns it
    red. Letting `begin_connect_remote` accept `Disconnected` leaves this file
    green, because `reach_conn` returns its label before the dial is consulted.
    Deleting `reach_conn`'s `Disconnected` arm ALSO leaves it green, because the
    catch-all it falls to refuses to dial as well. It goes red only when a guard is
    made to dial rather than merely removed (proven by replacing the arm with one
    that calls `begin_connect_remote`). The registry guard has its own red-proof in
    the Rust test `begin_connect_remote_refuses_a_disconnected_remote`.
  * case 7's "disconnected it" is the one that guards the command chain: making
    `input.rs` return `InputAction::None` for `RemuxCommand::RemoteDisconnect`
    turns it red while "the palette lists the command" stays green, because the
    listing comes from the action registry and not from anything running.
  * the panic check runs from `finally`, because a panic is the likeliest thing to
    abort a case, and at the end of the body it could never report the panic that
    stopped the run.

Run from the repo root:
    python3 tests/pty/session_manager_disconnect.py [-v]
"""
import json, os, shutil, socket, struct, subprocess, sys, time, traceback
import pexpect

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
# Read out of `src/protocol.rs`, never restated: a hard-coded copy goes stale in
# SILENCE, because the server logs a skew and proceeds.
from pty_harness import (  # noqa: E402
    PROTOCOL_VERSION, BOX_TL, BOX_TR, BOX_V,
    sm_open, sm_tree, sm_selected, sm_goto, sm_goto_text, sm_expand, sm_children,
)
import pyte  # noqa: E402

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUN = "/tmp/rmxdisc"
SOCK1 = f"{RUN}/run/remux.sock"      # the LOCAL server the client attaches to
SOCK2 = f"{RUN}/run2/remux.sock"     # the "remote" server reached via the shim
DIALS = f"{RUN}/dials"               # one line per `ssh` shim invocation
VERBOSE = "-v" in sys.argv

REMOTE_SESSION = "rbox"
MARK_REMOTE = "REMOTE_MARK_5150"

fails = []


def check(label, ok, detail=None):
    print(("  PASS  " if ok else "  FAIL  ") + label)
    if not ok:
        if detail is not None:
            print("        ", detail)
        fails.append(label)
    return ok


def log(*a):
    if VERBOSE:
        print("   .", *a)


# ---------------------------------------------------------------------------
# environment
# ---------------------------------------------------------------------------

def base_env(rundir, statedir, datadir, cfgdir):
    return {
        **os.environ,
        "PATH": f"{RUN}/bin:" + os.environ.get("PATH", ""),
        "XDG_RUNTIME_DIR": rundir,
        "XDG_STATE_HOME": statedir,
        "XDG_DATA_HOME": datadir,
        "XDG_CONFIG_HOME": cfgdir,
        "SHELL": "/bin/sh",
        "ENV": "/dev/null",
        "TERM": "xterm-256color",
        "PS1": "$ ",
        "REMUX_ALLOW_NESTED": "1",
    }


def env_local(cfgdir):
    return base_env(f"{RUN}/run", f"{RUN}/state", f"{RUN}/data", cfgdir)


def env_remote():
    return base_env(f"{RUN}/run2", f"{RUN}/state2", f"{RUN}/data2", f"{RUN}/cfg-none")


def write_config(cfgdir, body):
    os.makedirs(f"{cfgdir}/remux", exist_ok=True)
    with open(f"{cfgdir}/remux/config.toml", "w") as f:
        f.write(body)


def write_shim():
    """The `ssh` shim: record the invocation, then pump the wire protocol into the
    second server's socket via the real relay.

    The append is what every "did it redial?" assertion here reads. It happens
    BEFORE the exec, so a dial that fails to relay is still counted -- a redial
    that errored is still a redial.
    """
    shim = f"{RUN}/bin/ssh"
    with open(shim, "w") as f:
        f.write(
            "#!/bin/sh\n"
            f'printf "dial\\n" >> {DIALS}\n'
            f"export XDG_RUNTIME_DIR={RUN}/run2\n"
            f"export XDG_STATE_HOME={RUN}/state2\n"
            f"export XDG_DATA_HOME={RUN}/data2\n"
            f"exec {BIN} relay\n"
        )
    os.chmod(shim, 0o755)


def dials():
    """How many times the `ssh` shim has been invoked."""
    if not os.path.exists(DIALS):
        return 0
    with open(DIALS) as f:
        return sum(1 for line in f if line.strip())


def setup_dirs():
    shutil.rmtree(RUN, ignore_errors=True)
    for s in ("run", "run2", "state", "state2", "data", "data2", "bin",
              "cfg", "cfg-none"):
        os.makedirs(f"{RUN}/{s}", exist_ok=True)
    write_shim()
    write_config(f"{RUN}/cfg",
                 '[remotes.mini]\nssh = "whatever"\nremux_path = "remux"\n'
                 'auto_connect = true\n')
    write_config(f"{RUN}/cfg-none", "")


def start_server(sock, env):
    p = subprocess.Popen([BIN, "server"], env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        if os.path.exists(sock):
            time.sleep(0.3)
            return p
        time.sleep(0.05)
    p.kill()
    raise SystemExit(f"server socket {sock} never appeared")


# ---------------------------------------------------------------------------
# minimal wire client (seeds the remote server's session)
# ---------------------------------------------------------------------------

class Wire:
    def __init__(self, sock):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(sock)
        self.s.settimeout(2.0)
        self.buf = b""

    def send(self, obj):
        b = json.dumps(obj).encode()
        self.s.sendall(struct.pack(">I", len(b)) + b)

    def recv(self):
        while len(self.buf) < 4:
            self.buf += self.s.recv(65536)
        n = struct.unpack(">I", self.buf[:4])[0]
        while len(self.buf) < 4 + n:
            self.buf += self.s.recv(65536)
        body, self.buf = self.buf[4:4 + n], self.buf[4 + n:]
        return json.loads(body)

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


def seed_remote_session():
    """Create a session on the "remote" server, print a marker in its pane, then
    detach so the pane is NOT session-visible (a session-visible pane shows a
    placeholder in a view cell instead of streaming)."""
    w = Wire(SOCK2)
    w.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "t"})
    w.recv()
    w.send({"CreateSession": {"name": REMOTE_SESSION, "folder": None}})
    w.send({"Attach": {"session_name": REMOTE_SESSION}})
    w.send({"Resize": {"cols": 100, "rows": 30}})
    time.sleep(0.5)
    # The marker is ASSEMBLED by the shell inside the pane rather than typed, so
    # the echo of the command line can never be mistaken for the pane's output.
    w.send({"Input": {"data": list("clear; printf 'REMOTE_MARK_%s\\n' 5150\n".encode())}})
    time.sleep(0.8)
    w.send("Detach")
    time.sleep(0.3)
    w.close()


# ---------------------------------------------------------------------------
# PTY client
# ---------------------------------------------------------------------------

class Client:
    """Just enough of `pty_harness.Tui` for the `sm_*` helpers, plus the isolated
    two-server environment the fake remote needs (which `Tui.start` does not
    build)."""

    def __init__(self, name, cfgdir, cols=120, rows=40):
        self.name = name
        self.cols = cols
        self.rows = rows
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.child = pexpect.spawn(BIN, [], env=env_local(cfgdir),
                                   dimensions=(rows, cols), encoding=None)
        self.pump(2.0)

    def pump(self, t=0.5):
        end = time.time() + t
        while time.time() < end:
            try:
                data = self.child.read_nonblocking(65536, 0.1)
                if data:
                    self.stream.feed(data)
            except Exception:
                pass

    def send(self, data, t=0.4):
        if isinstance(data, str):
            data = data.encode()
        self.child.send(data)
        self.pump(t)

    def prefix(self, keys, t=0.4):
        self.child.send(b"\x01")
        time.sleep(0.15)
        self.send(keys, t)

    def rows_text(self):
        return self.screen.display

    def has(self, needle):
        return any(needle in r for r in self.rows_text())

    def screen_text(self):
        return "\n".join(self.rows_text())

    def alive(self):
        return self.child.isalive()

    def dump(self, label=""):
        print(f"----- {self.name} {label} -----")
        for i, r in enumerate(self.rows_text()):
            if r.rstrip():
                print(f"{i:2} |{r.rstrip()}")
        print("-" * 46)

    def kill(self):
        try:
            self.child.terminate(force=True)
        except Exception:
            pass


def status_bar(c):
    return c.rows_text()[-1]


def popup_lines(c, title):
    """The interior lines of the overlay whose top border carries `title`.

    Restricted to the popup's own columns so a word that happens to be in a pane
    behind it cannot satisfy (or defeat) an assertion about the overlay.
    """
    rows = c.rows_text()
    ty, x0, x1 = popup_box(c, title)
    out = []
    for y in range(ty + 1, len(rows)):
        if rows[y][x0] != BOX_V and rows[y][x0] not in "├╰":
            break
        out.append(rows[y][x0 + 1:x1])
    return out


def popup_box(c, title):
    """`(title_y, left_x, right_x)` of the overlay whose top border says `title`."""
    rows = c.rows_text()
    for i, r in enumerate(rows):
        if title in r and BOX_TL in r:
            return i, r.index(BOX_TL), r.rindex(BOX_TR)
    raise AssertionError(f"{title!r} overlay is not on screen")


def popup_selected(c, title):
    """The highlighted line of an arbitrary overlay, read by BACKGROUND.

    The session-manager helpers in `pty_harness` key off the "Session Manager"
    title, so the quick switcher needs its own reader. Which row is highlighted is
    derived the same way theirs is -- the row whose background differs from its
    siblings' -- rather than from a hard-coded selection colour.
    """
    rows = c.rows_text()
    ty, x0, x1 = popup_box(c, title)
    body = []
    for y in range(ty + 1, len(rows)):
        if rows[y][x0] != BOX_V:
            if rows[y][x0] in "├╰":
                continue
            break
        bgs = [c.screen.buffer[y][x].bg for x in range(x0 + 1, x1)]
        body.append((y, rows[y][x0 + 1:x1], max(set(bgs), key=bgs.count)))
    assert body, f"{title!r} overlay has no body rows"
    bgs = [bg for _, _, bg in body]
    normal = max(set(bgs), key=bgs.count)
    hits = [(y, text) for y, text, bg in body if bg != normal]
    assert len(hits) == 1, f"expected one highlighted row in {title!r}, got {hits}"
    return hits[0][1]


def popup_goto(c, title, needle):
    """Press `j` in an arbitrary overlay until its highlighted line matches."""
    for _ in range(40):
        line = popup_selected(c, title)
        if needle in line:
            return line
        c.send("j", 0.15)
    raise AssertionError(
        f"never reached {needle!r} in {title!r}; stopped on "
        f"{popup_selected(c, title)!r}"
    )


def client_log():
    p = f"{RUN}/state/remux/client.log"
    return open(p, errors="ignore").read() if os.path.exists(p) else ""


def check_no_panic():
    """A panic reaches these logs only because `install_panic_logger()` routes
    `std::panic` through the `log` crate, and its message opens with the literal
    `panicked at`. Proven to go red by panicking on purpose once -- see the
    harness notes in CLAUDE.md."""
    for state in (f"{RUN}/state", f"{RUN}/state2"):
        for which in ("server", "client", "relay"):
            p = f"{state}/remux/{which}.log"
            if not os.path.exists(p):
                continue
            body = open(p, errors="ignore").read()
            if "panicked at" in body:
                hit = next(l for l in body.splitlines() if "panicked at" in l)
                check(f"no panic in {which}.log ({state})", False, hit)
                return
    check("no `panicked at` in any log", True)


# ---------------------------------------------------------------------------
# session-manager navigation
# ---------------------------------------------------------------------------

def sm_close(c):
    c.send(b"\x1b", 0.5)


def is_remote_server_row(row):
    """The remote's own SERVER row, not something else that says `mini`.

    Once a view exists, its cell rows are titled by their source (`mini: sh`), so
    a plain text match finds a cell first and every `d`/Enter would land on the
    wrong node. The server row is the one at depth 0. Neither end of the text is
    safe to anchor on either: a triangle precedes the alias and the state suffix
    that follows it is the thing under test.
    """
    return row.depth == 0 and "mini" in row.text


def remote_row(c):
    """The remote's server row, wherever it currently is."""
    for r in sm_tree(c):
        if is_remote_server_row(r):
            return r
    raise AssertionError(f"no `mini` server row in the tree: {sm_tree(c)}")


def sm_goto_remote(c):
    """Put the session-manager highlight on the remote's server row."""
    return sm_goto(c, is_remote_server_row, "the `mini` server row")


def local_session_name(c):
    """The local server's first session row, read off the tree rather than
    assumed: the name the client picks for its startup session is not this
    harness's to decide."""
    local = sm_goto_text(c, "local")
    kids = sm_children(c, local)
    assert kids, f"the local server has no sessions: {sm_tree(c)}"
    # `  ▼ * main (1)` -> `main`: drop the expansion triangle, the
    # current-session star, and the trailing tab count.
    return kids[0].text.strip().lstrip("▼▶● *").split(" (")[0].strip()


# ---------------------------------------------------------------------------
# cases
# ---------------------------------------------------------------------------

def main():
    setup_dirs()
    servers = []
    clients = []
    try:
        servers.append(start_server(SOCK2, env_remote()))
        seed_remote_session()
        servers.append(start_server(SOCK1, env_local(f"{RUN}/cfg-none")))

        c = Client("A", f"{RUN}/cfg")
        clients.append(c)
        # `auto_connect` dials once during startup, before raw mode.
        check("0 the auto-connect dialled the remote exactly once", dials() == 1,
              f"dials={dials()}")

        # -- (1) `d` arms the confirmation; `n` cancels -----------------------
        sm_open(c)
        local_name = local_session_name(c)
        log("local session:", local_name)
        kids = sm_expand(c, "mini")
        check("1 the connected remote's subtree is listed",
              any(REMOTE_SESSION in r.text for r in kids), kids)

        row = sm_goto_remote(c)
        check("1 the remote's row reads as connected (no state suffix)",
              "(" not in row.text, row.text)
        c.send("d", 0.4)
        if VERBOSE:
            c.dump("(1) after d")
        prompt = c.screen_text()
        check("1 `d` arms the disconnect confirmation naming the remote",
              "Disconnect mini? (y/n)" in prompt,
              [l for l in c.rows_text() if l.strip()][-6:])
        check("1 the prompt is not the DELETE prompt",
              "Delete mini" not in prompt and "Delete session" not in prompt)

        c.send("n", 0.5)
        check("1 `n` cancels: the prompt is gone",
              "Disconnect mini? (y/n)" not in c.screen_text())
        check("1 `n` cancels: the remote is still connected",
              "(disconnected)" not in remote_row(c).text, remote_row(c).text)
        check("1 `n` cancels: the subtree is still there",
              any(REMOTE_SESSION in r.text for r in sm_tree(c)), sm_tree(c))
        check("1 `n` cancels: nothing was redialled", dials() == 1, f"dials={dials()}")

        # -- (2) `d` `y` disconnects -----------------------------------------
        sm_goto_remote(c)
        c.send("d", 0.4)
        assert "Disconnect mini? (y/n)" in c.screen_text(), "the prompt did not re-arm"
        c.send("y", 1.5)
        if VERBOSE:
            c.dump("(2) after d y")
        row = remote_row(c)
        check("2 the remote's row reads `(disconnected)`",
              "(disconnected)" in row.text, row.text)
        check("2 the remote's subtree is gone",
              not any(REMOTE_SESSION in r.text for r in sm_tree(c)), sm_tree(c))
        check("2 the local subtree survives it",
              any(local_name in r.text for r in sm_tree(c)), sm_tree(c))
        check("2 the client is still alive", c.alive())

        # The switcher rebuilds from `connected_ids()` on open, so this is the
        # user's actual ask. The local session is the positive control: an
        # overlay that painted nothing would pass "rbox is absent" on its own.
        sm_close(c)
        c.send(b"\x1bs", 1.2)
        sw = popup_lines(c, "Switch Session")
        log("switcher:", [l.rstrip() for l in sw if l.strip()])
        check("2 the switcher no longer lists the remote's sessions",
              not any(REMOTE_SESSION in l for l in sw), sw)
        check("2 the switcher still lists the local session",
              any(local_name in l for l in sw), sw)
        c.send(b"\x1b", 0.5)

        # -- (3) the ssh EOF arrives afterwards ------------------------------
        # Killing the child made its pipe report EOF, which reaches the client as
        # an `Incoming::Closed` a moment later. That path must not overwrite the
        # user's choice with `Failed("connection lost")`.
        c.pump(2.5)
        sm_open(c)
        row = remote_row(c)
        check("3 the row is STILL `(disconnected)` after the EOF lands",
              "(disconnected)" in row.text, row.text)
        check("3 the EOF did not turn it into a failure",
              "failed" not in row.text and "connection lost" not in c.screen_text(),
              row.text)
        # TWO of them, not one: `disconnect_remote_now` runs the cleanup itself
        # and logs the first, so a mere presence check is green whether or not the
        # EOF ever arrives. The second line IS the EOF being received and declined.
        left_alone = client_log().count("state left as is")
        check("3 the EOF reached the client and left the state alone",
              left_alone == 2,
              f"'state left as is' appears {left_alone}x, wanted 2")
        check("3 nothing was redialled while it sat disconnected",
              dials() == 1, f"dials={dials()}")

        # -- (4) Enter reconnects --------------------------------------------
        sm_goto_remote(c)
        c.send("\r", 3.0)
        row = remote_row(c)
        check("4 Enter on the row reconnects it", "(disconnected)" not in row.text,
              row.text)
        check("4 the dial count went 1 -> 2", dials() == 2, f"dials={dials()}")
        kids = sm_expand(c, "mini")
        check("4 the subtree came back",
              any(REMOTE_SESSION in r.text for r in kids), kids)

        # -- (5) the Views hazard --------------------------------------------
        # A view cell aliasing a remote pane is re-subscribed on every layout and
        # focus pass, which is where a lazy redial would come from.
        pane = None
        tab = sm_expand(c, REMOTE_SESSION)[0]
        sm_goto_text(c, tab.text.strip())
        c.send("l", 0.6)
        panes = sm_children(c, sm_selected(c))
        assert panes, f"the remote tab has no panes: {sm_tree(c)}"
        pane = panes[0]
        sm_goto_text(c, pane.text.strip())
        c.send(" ", 0.4)
        c.send("v", 0.2)
        c.send("a", 0.6)
        assert c.has("Add Pane to View"), "the view picker never opened"
        c.send("\r", 2.5)
        check("5 the view cell streams the remote pane before the disconnect",
              c.has(MARK_REMOTE), c.screen_text()[-400:])
        c.prefix(b"q", 1.2)                       # leave the view

        sm_open(c)
        if VERBOSE:
            c.dump("(5) manager reopened after leaving the view")
        sm_goto_remote(c)
        if VERBOSE:
            log("(5) selected:", sm_selected(c))
        c.send("d", 0.4)
        if VERBOSE:
            c.dump("(5) after d")
        assert "Disconnect mini? (y/n)" in c.screen_text(), "the prompt did not arm"
        c.send("y", 1.5)
        sm_close(c)
        before = dials()
        c.send(b"\x1bs", 1.2)
        sw = popup_lines(c, "Switch Session")
        assert any("View" in l for l in sw), f"the switcher lists no view: {sw}"
        popup_goto(c, "Switch Session", "View 1")
        c.send("\r", 2.5)
        c.pump(2.5)                               # several repaint/subscribe passes
        if VERBOSE:
            c.dump("(5) in the view, remote disconnected")
        text = c.screen_text()
        check("5 the cell says the connection is gone", "disconnected" in text,
              text[-400:])
        check("5 the cell is not sitting on a transient label",
              "connecting to mini" not in text and "waiting" not in text, text[-400:])
        check("5 entering the view did NOT redial the remote", dials() == before,
              f"dials went {before} -> {dials()}")
        check("5 the client is still alive", c.alive())
        c.prefix(b"q", 1.0)

        # -- (6) the remote is the FOREGROUND --------------------------------
        sm_open(c)
        sm_goto_remote(c)
        c.send("\r", 3.0)                         # reconnect
        check("6 the remote reconnected for the foreground case",
              "(disconnected)" not in remote_row(c).text, remote_row(c).text)
        sm_expand(c, "mini")
        sm_goto_text(c, REMOTE_SESSION)
        c.send("\r", 2.5)                         # attach -> remote is foreground
        c.pump(1.0)
        fg = REMOTE_SESSION in status_bar(c)
        check("6 the remote session is the foreground", fg,
              repr(status_bar(c).strip()[:70]))

        sm_open(c)
        sm_goto_remote(c)
        c.send("d", 0.4)
        assert "Disconnect mini? (y/n)" in c.screen_text(), "the prompt did not arm"
        c.send("y", 2.5)
        c.pump(1.5)
        if VERBOSE:
            c.dump("(6) after disconnecting the foreground remote")
        check("6 the client survived disconnecting its own foreground", c.alive())
        # The STATUS BAR only, and both halves of it. Screen-wide would be
        # satisfied by the manager's own `main` tree row, which is on screen
        # because the manager is open, and the remote's name leaving the bar is
        # the discriminating half: before this it read `rbox`.
        check("6 the status bar names the local session it fell back to",
              local_name in status_bar(c), repr(status_bar(c).strip()[:70]))
        check("6 the status bar no longer names the remote's session",
              REMOTE_SESSION not in status_bar(c), repr(status_bar(c).strip()[:70]))
        # The manager is open -- the user pressed `y` in it -- so `remote_row`
        # raising is the right outcome if it somehow is not.
        check("6 the foreground remote's row reads `(disconnected)` too",
              "(disconnected)" in remote_row(c).text, remote_row(c).text)

        # -- (7) the `RemoteDisconnect` command ------------------------------
        # The palette reaches the same body through `InputAction`, and only a run
        # proves the chain (command -> parse -> intercept -> handler) is wired:
        # compiling proves the variant exists, not that anything calls it.
        sm_goto_remote(c)
        c.send("\r", 3.0)                         # reconnect
        assert "(disconnected)" not in remote_row(c).text, "case 7 needs it connected"
        sm_close(c)
        dialed = dials()
        c.prefix(b":", 0.8)
        assert c.has("Command Palette"), "the command palette never opened"
        c.send("RemoteDisconnect mini", 0.5)
        check("7 the palette lists the command",
              any("RemoteDisconnect" in l for l in popup_lines(c, "Command Palette")),
              popup_lines(c, "Command Palette"))
        c.send("\r", 2.0)
        c.pump(1.0)
        sm_open(c)
        check("7 `RemoteDisconnect mini` disconnected it",
              "(disconnected)" in remote_row(c).text, remote_row(c).text)
        check("7 the command did not redial anything", dials() == dialed,
              f"dials went {dialed} -> {dials()}")
        check("7 the client is still alive", c.alive())

        # -- (8) the stale-`Closed` race -------------------------------------
        # `d`, `y`, Enter with NOTHING between them: the reconnect installs a new
        # transport while the ssh pipe of the one just killed is still reporting
        # EOF. The `Closed` that arrives after it carries the superseded
        # generation, so it must be discarded rather than tear down the live
        # connection. Without the generation tag this ends `(failed: connection
        # lost)`.
        #
        # The hazardous interleaving is a COIN FLIP, not something this harness can
        # force: after `y` the client selects between the pending Enter on stdin and
        # the EOF on its own channel, and only the Enter-first order produces a
        # stale `Closed`. So the race is run repeatedly and the loop asserts that the
        # order under test was actually reached -- a run where it never is FAILS
        # rather than passing on the harmless ordering, which is the whole trap this
        # file exists to avoid.
        sm_close(c)
        sm_open(c)
        raced = False
        for attempt in range(8):
            sm_goto_remote(c)
            if "(disconnected)" in remote_row(c).text:
                c.send("\r", 3.0)
            assert "(disconnected)" not in remote_row(c).text, (
                f"attempt {attempt}: needs it connected, "
                f"got {remote_row(c).text!r}")
            dialed = dials()
            before = client_log().count("superseded transport")
            # No pump between the three: pexpect writes them back to back, which is
            # the point. Anything that waits here lets the EOF be handled before the
            # reconnect and tests nothing.
            c.child.send(b"d")
            c.child.send(b"y")
            c.child.send(b"\r")
            c.pump(4.0)
            row = remote_row(c)
            ok = "(disconnected)" not in row.text and "failed" not in row.text
            check(f"8.{attempt} the reconnect survived the old transport's EOF",
                  ok, row.text)
            check(f"8.{attempt} the remote really did reconnect (it dialled again)",
                  dials() == dialed + 1, f"dials went {dialed} -> {dials()}")
            if client_log().count("superseded transport") > before:
                raced = True
                log(f"the stale-Closed ordering was reached on attempt {attempt}")
                break
        check("8 the stale-`Closed` ordering was actually exercised", raced,
              "8 attempts and the EOF was always handled before the reconnect; "
              "this case proved nothing about the generation gate")
        kids = sm_expand(c, "mini")
        check("8 the subtree is present after the race",
              any(REMOTE_SESSION in r.text for r in kids), kids)
        check("8 the client is still alive", c.alive())

    except Exception as e:
        # A case that aborts must not take the panic check with it: a panic is the
        # likeliest thing to abort one, and it aborts on a missing overlay several
        # assertions before any panic check at the end of the body would run.
        traceback.print_exc()
        fails.append(f"harness aborted: {e}")
    finally:
        # Before the kills, so the logs are read while they are complete.
        check_no_panic()
        for cl in clients:
            cl.kill()
        for p in servers:
            p.kill()

    if fails:
        print("RESULT: FAIL")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("RESULT: PASS (a remote disconnected from the session manager leaves the "
          "tree and the switcher, survives its own ssh EOF, is never redialled by a "
          "view cell, and reconnects on Enter)")


if __name__ == "__main__":
    main()
