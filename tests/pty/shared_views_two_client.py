"""Phase 2 acceptance (real PTY, TWO clients, ONE shared server).

The user's exact scenario: two `remux` clients attached to the SAME local
server. Client A composes a shared View; client B must LIST it in its switcher,
ENTER it and SEE the cell content, and then MIRROR — live, with no manual
refresh — a cell added / focus / layout / zoom change made in the OTHER client.

Run from repo root:  PYTHONPATH=tests/pty python3 tests/pty/shared_views_two_client.py [-v]
"""
import os, shutil, subprocess, sys, time
import pexpect, pyte

BIN = os.path.abspath("target/debug/remux")
RUN = "/tmp/rmx2c"
SOCK = f"{RUN}/run/remux.sock"
VERBOSE = "-v" in sys.argv

MARK_A = "AAAA_alpha"
MARK_B = "BBBB_bravo"


def env_for():
    return {
        **os.environ,
        "XDG_RUNTIME_DIR": f"{RUN}/run",
        "XDG_STATE_HOME": f"{RUN}/state",
        "XDG_DATA_HOME": f"{RUN}/data",
        "XDG_CONFIG_HOME": f"{RUN}/config",
        "SHELL": "/bin/sh",
        "ENV": "/dev/null",
        "TERM": "xterm-256color",
        "PS1": "$ ",
        "REMUX_ALLOW_NESTED": "1",
    }


def start_server():
    shutil.rmtree(RUN, ignore_errors=True)
    for s in ("run", "state", "data", "config"):
        os.makedirs(f"{RUN}/{s}", exist_ok=True)
    p = subprocess.Popen(
        [BIN, "server"], env=env_for(),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    for _ in range(200):
        if os.path.exists(SOCK):
            time.sleep(0.3)
            return p
        time.sleep(0.05)
    p.kill()
    raise SystemExit("server socket never appeared")


class Client:
    def __init__(self, name, cols=110, rows=36):
        self.name, self.cols, self.rows = name, cols, rows
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.child = pexpect.spawn(
            BIN, [], env=env_for(), dimensions=(rows, cols), encoding=None,
        )
        self.pump(1.2)

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

    def find(self, needle):
        for i, r in enumerate(self.rows_text()):
            if needle in r:
                return i
        return -1

    def dump(self, label=""):
        print(f"----- {self.name} screen {label} -----")
        for i, r in enumerate(self.rows_text()):
            if r.rstrip():
                print(f"{i:2} |{r.rstrip()}")
        print("-" * 40)

    def alive(self):
        return self.child.isalive()

    def kill(self):
        try:
            self.child.terminate(force=True)
        except Exception:
            pass


def log_has_panic():
    for which in ("server", "client"):
        p = f"{RUN}/state/remux/{which}.log"
        if os.path.exists(p):
            if "panic" in open(p, errors="ignore").read().lower():
                return True
    return False


def sm_compose_mark_first(a):
    """Open the session manager, expand the session's Tab 1, mark ONLY the
    first pane, and create+enter a new view over it."""
    a.prefix(b"xm", 0.8)
    # The manager opens with its search bar focused; Tab hands focus to the tree.
    a.send(b"\t", 0.3)
    if VERBOSE:
        a.dump("SM opened")
    # Expand down to the panes. Mirrors issue1_view_blank: j to session, j to
    # Tab 1, l to expand, j onto the first pane, space to mark.
    a.send("j", 0.2)   # Local -> session (or session if already there)
    a.send("j", 0.2)
    a.send("l", 0.3)
    a.send("j", 0.2)
    if VERBOSE:
        a.dump("SM before mark")
    a.send(" ", 0.3)   # mark first pane
    if VERBOSE:
        a.dump("SM marked first pane")
    a.send("v", 0.2)
    a.send("a", 0.5)   # AddToView -> picker
    if VERBOSE:
        a.dump("picker (new view)")
    a.send("\r", 0.9)  # confirm -> new view (create + enter)


def main():
    server = start_server()
    a = Client("A")
    fails = []

    def fail(msg):
        print("  FAIL:", msg)
        fails.append(msg)

    try:
        # --- A: two panes with distinct markers, backgrounded, compose view ---
        a.send("clear\r", 0.3)
        a.send(f"printf '{MARK_A}\\n'\r", 0.5)
        a.prefix(b"pv", 0.7)            # split -> pane 2 (focused)
        a.send(f"printf '{MARK_B}\\n'\r", 0.5)
        a.send(b"\x1bt", 0.7)          # Alt+t: new empty Tab 2 (backgrounds 1&2)
        a.send(b"\x1bh", 0.3)          # (harmless) ensure normal mode focus
        sm_compose_mark_first(a)
        a.pump(0.6)
        if VERBOSE:
            a.dump("A in view")
        print("[A] shows MARK_A in view:", a.has(MARK_A))

        # --- B connects to the SAME server ---
        b = Client("B")
        b.pump(0.8)
        try:
            # (a) B's switcher LISTS the view by name.
            b.send(b"\x1bs", 0.8)      # Alt+s: switcher
            if VERBOSE:
                b.dump("B switcher")
            listed = b.has("View 1")
            print("[a] B switcher lists 'View 1':", listed)
            if not listed:
                fail("(a) B switcher did not list the shared view")

            # (b) B enters the view (select index 0 = the view) and SEES content.
            # Views are index 0..; navigate to the top then Enter.
            b.send("k", 0.3)           # move highlight (wraps toward the view)
            if VERBOSE:
                b.dump("B switcher after k")
            b.send("\r", 1.0)          # activate selected
            b.pump(0.8)
            if VERBOSE:
                b.dump("B after entering view")
            saw = b.has(MARK_A)
            print("[b] B sees cell content (MARK_A):", saw)
            if not saw:
                fail("(b) B did not see the cell content after entering")

            # (c) A adds a SECOND pane to the view; B repaints WITHOUT input.
            a.prefix(b"xm", 0.8)
            # The manager opens with its search bar focused; Tab hands focus to the tree.
            a.send(b"\t", 0.3)
            a.send("j", 0.2); a.send("j", 0.2); a.send("l", 0.3)
            a.send("j", 0.2); a.send("j", 0.2)   # move to the SECOND pane
            if VERBOSE:
                a.dump("A SM second pane")
            a.send(" ", 0.3)                       # mark it
            a.send("v", 0.2); a.send("a", 0.5)     # AddToView
            if VERBOSE:
                a.dump("A picker for existing view")
            # Pick the EXISTING view (not "new"): navigate to it then Enter.
            a.send("k", 0.3)
            a.send("\r", 1.0)
            # Give B time to receive the ViewList broadcast + PaneContent.
            b.pump(1.2)
            if VERBOSE:
                b.dump("B after A added second pane")
            repainted = b.has(MARK_B)
            print("[c] B repaints with the new cell (MARK_B), no B input:", repainted)
            if not repainted:
                fail("(c) B did not repaint with the added cell")

            # (d) A toggles zoom; B MIRRORS (only the focused cell remains).
            a.prefix(b"f", 0.8)        # Prefix+f zoom
            b.pump(1.0)
            if VERBOSE:
                b.dump("B after A zoom")
            b_a, b_b = b.has(MARK_A), b.has(MARK_B)
            # Zoom shows exactly one cell: exactly one marker visible on B.
            mirrored_zoom = b_a != b_b
            print(f"[d.zoom] B mirrors zoom (one cell): A_vis={b_a} B_vis={b_b}")
            if not mirrored_zoom:
                fail("(d) B did not mirror the zoom (should show one cell)")

            # A un-zooms; B shows both again.
            a.prefix(b"f", 0.8)
            b.pump(1.0)
            both = b.has(MARK_A) and b.has(MARK_B)
            print("[d.unzoom] B mirrors un-zoom (both cells):", both)
            if not both:
                fail("(d) B did not mirror the un-zoom (should show both)")

            # (d) FOCUS mirror: A moves focus to the OTHER cell, then zooms. If
            # focus mirrored, B now zooms the SECOND cell -> B shows MARK_B alone
            # (not MARK_A). Pure text-presence witness; no attribute reads.
            a.prefix(b"pl", 0.7)       # Prefix p l: focus right -> MARK_B cell
            b.pump(0.8)
            a.prefix(b"f", 0.8)        # zoom the (now) focused cell
            b.pump(1.0)
            if VERBOSE:
                b.dump("B after A focus-right + zoom")
            fa, fb = b.has(MARK_A), b.has(MARK_B)
            focus_mirrored = fb and not fa
            print(f"[d.focus] B mirrors focus move (zooms MARK_B): A_vis={fa} B_vis={fb}")
            if not focus_mirrored:
                fail("(d) B did not mirror the focus move")
            a.prefix(b"f", 0.8)        # un-zoom for the layout step
            b.pump(0.8)

            # (d) LAYOUT mirror: capture B's right-aligned status-bar layout token,
            # cycle the layout in A, and assert B's token CHANGED (don't hardcode
            # the successor -- LayoutMode::next() order is an impl detail).
            def b_layout_token():
                last = b.screen.display[-1].split()
                return last[-1] if last else ""
            before_tok = b_layout_token()
            a.send(b"\x1b ", 0.9)      # Alt+Space: cycle layout (shared)
            b.pump(1.0)
            after_tok = b_layout_token()
            layout_mirrored = before_tok != after_tok and after_tok != ""
            print(f"[d.layout] B mirrors layout cycle: {before_tok!r} -> {after_tok!r}")
            if not layout_mirrored:
                fail("(d) B did not mirror the layout cycle")

            alive = a.alive() and b.alive()
            panic = log_has_panic()
            print("both alive:", alive, "panic:", panic)
            if not alive:
                fail("a client exited")
            if panic:
                fail("panic in a log")
        finally:
            b.kill()
    finally:
        a.kill()
        server.kill()

    if fails:
        print("RESULT: FAIL ->", fails)
        sys.exit(1)
    print("RESULT: PASS (two-client shared views: list, enter, live add + mirror)")


if __name__ == "__main__":
    main()
