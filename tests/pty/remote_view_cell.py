"""Remote View cells: does a cell aliasing a REMOTE pane ever stream?

User report: "as soon as I add a REMOTE pane to a view I see 'waiting for
waiting…'". Everything here runs through the FAKE REMOTE recipe (a second
isolated server + an `ssh` shim that execs `remux relay`), so no real SSH is
involved. Five scenarios, each a different way a terminal can reach -- or fail to
reach -- the cell's source server:

  (1) the composing terminal, remote already CONNECTED: the cell must stream.
  (2) the same terminal after the REMOTE became its FOREGROUND (attached to a
      remote session, then re-entered the view): the cell must still stream --
      entering a view `Detach`es the foreground, so exercise that ordering.
  (3) another terminal that has NEVER connected that remote (shared views reach
      it now): it must lazily connect and stream, must say `connecting to …`
      while dialing, and must stay RESPONSIVE during the dial -- the shim is
      slowed to 5s so an event-loop-blocking dial would be caught.
  (4) a terminal with no such remote in its config at all: it can never stream,
      so it must show an honest `not connected: <name>` and never a `waiting…`
      that would never resolve.
  (5) a terminal whose lazy dial FAILS (the shim refuses the connection): the
      cell must settle on `not connected: <name>` rather than stay on the
      transient `connecting to …`.

`waiting for waiting…` must appear in none of them.

Run from the repo root:
    python3 tests/pty/remote_view_cell.py [-v]
"""
import json, os, shutil, socket, struct, subprocess, sys, time
import pexpect, pyte

BIN = os.path.abspath("target/debug/remux")
RUN = "/tmp/rmxrv"
SOCK1 = f"{RUN}/run/remux.sock"      # the LOCAL server both clients attach to
SOCK2 = f"{RUN}/run2/remux.sock"     # the "remote" server reached via the shim
VERBOSE = "-v" in sys.argv

MARK_LOCAL = "LOCAL_MARK_1122"
MARK_REMOTE = "REMOTE_MARK_7788"


# ---------------------------------------------------------------------------
# environments
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


def setup_dirs():
    shutil.rmtree(RUN, ignore_errors=True)
    for s in ("run", "run2", "state", "state2", "data", "data2", "bin",
              "cfgA", "cfgB", "cfgC", "cfg-none"):
        os.makedirs(f"{RUN}/{s}", exist_ok=True)
    write_shim()
    # A: remote configured AND auto-connected at startup (the composing terminal).
    write_config(f"{RUN}/cfgA", '[remotes.mini]\nssh = "whatever"\nremux_path = "remux"\nauto_connect = true\n')
    # B: same remote configured but NOT connected -- must lazily connect.
    write_config(f"{RUN}/cfgB", '[remotes.mini]\nssh = "whatever"\nremux_path = "remux"\n')
    # C: no remotes at all -- can never stream; must say so honestly.
    write_config(f"{RUN}/cfgC", "")
    write_config(f"{RUN}/cfg-none", "")


def write_shim(delay=0):
    """The `ssh` shim: ignore every argument and pump the wire protocol straight
    into the second server's socket via the real relay. `delay` seconds of sleep
    first makes the dial slow enough to observe the client during it."""
    shim = f"{RUN}/bin/ssh"
    with open(shim, "w") as f:
        f.write(
            "#!/bin/sh\n"
            f"export XDG_RUNTIME_DIR={RUN}/run2\n"
            f"export XDG_STATE_HOME={RUN}/state2\n"
            f"export XDG_DATA_HOME={RUN}/data2\n"
            + (f"sleep {delay}\n" if delay else "")
            + f"exec {BIN} relay\n"
        )
    os.chmod(shim, 0o755)


def write_failing_shim():
    """An `ssh` shim that fails the dial outright (as a down host / bad key would),
    so the dial-FAILURE path is exercised end to end."""
    shim = f"{RUN}/bin/ssh"
    with open(shim, "w") as f:
        f.write("#!/bin/sh\necho 'ssh: connect to host mini: No route to host' >&2\nexit 255\n")
    os.chmod(shim, 0o755)


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
# minimal wire client (used only to seed the remote server's pane)
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


def seed_remote_pane():
    """Create a session on the 'remote' server, print the marker in its pane,
    then detach so the pane is NOT session-visible."""
    w = Wire(SOCK2)
    w.send({"protocol_version": 4, "remux_version": "t"})
    w.recv()
    w.send({"CreateSession": {"name": "rbox", "folder": None}})
    w.send({"Attach": {"session_name": "rbox"}})
    w.send({"Resize": {"cols": 100, "rows": 30}})
    time.sleep(0.5)
    w.send({"Input": {"data": list(f"clear; printf '{MARK_REMOTE}\\n'\n".encode())}})
    time.sleep(0.8)
    w.send("Detach")
    time.sleep(0.3)
    w.close()


# ---------------------------------------------------------------------------
# PTY client
# ---------------------------------------------------------------------------

class Client:
    def __init__(self, name, cfgdir, cols=110, rows=36):
        self.name = name
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.child = pexpect.spawn(BIN, [], env=env_local(cfgdir),
                                   dimensions=(rows, cols), encoding=None)
        self.pump(1.5)

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

    def dump(self, label=""):
        print(f"----- {self.name} {label} -----")
        for i, r in enumerate(self.rows_text()):
            if r.rstrip():
                print(f"{i:2} |{r.rstrip()}")
        print("-" * 46)

    def alive(self):
        return self.child.isalive()

    def kill(self):
        try:
            self.child.terminate(force=True)
        except Exception:
            pass


def logs_have_panic():
    for state in (f"{RUN}/state", f"{RUN}/state2"):
        for which in ("server", "client", "relay"):
            p = f"{state}/remux/{which}.log"
            if os.path.exists(p) and "panic" in open(p, errors="ignore").read().lower():
                return True
    return False


def grep_client_log(needle):
    p = f"{RUN}/state/remux/client.log"
    if not os.path.exists(p):
        return []
    return [l.rstrip() for l in open(p, errors="ignore") if needle in l]


# ---------------------------------------------------------------------------
# session-manager navigation
# ---------------------------------------------------------------------------

def sm_open(c):
    c.prefix(b"xm", 1.0)
    # The manager opens with its search bar focused; Tab hands focus to the tree.
    c.send(b"\t", 0.3)
def sm_goto_row(c, needle, max_steps=25):
    """Move the overlay cursor down (`j`) until the highlighted row matches
    `needle`. Works for the session manager and the switcher alike."""
    for _ in range(max_steps):
        row = sm_selected_row(c)
        if row is not None and needle in row:
            return True
        c.send("j", 0.2)
    row = sm_selected_row(c)
    return row is not None and needle in row


def sm_selected_row(c):
    """The overlay row under the cursor: the row whose dominant background differs
    from the overlay's own (overlays paint the selected row with the theme's
    highlight color)."""
    scr = c.screen
    x0, x1 = 28, 78
    dom = []
    for y in range(scr.lines):
        row = scr.buffer[y]
        bgs = [row[x].bg for x in range(x0, x1)]
        dom.append(max(set(bgs), key=bgs.count))
    inside = [b for b in dom if b != "default"]
    if not inside:
        return None
    modal = max(set(inside), key=inside.count)
    for y, b in enumerate(dom):
        if b != "default" and b != modal:
            row = scr.buffer[y]
            return "".join(row[x].data for x in range(x0, x1)).rstrip()
    return None


def compose_view_over_remote_pane(c):
    """Session manager: expand the `mini` remote down to its first pane, mark it,
    and create + enter a NEW view over it."""
    sm_open(c)
    assert sm_goto_row(c, "mini"), "remote node 'mini' not in the tree"
    c.send("l", 0.9)          # expand the remote server node
    c.send("j", 0.3)          # -> its session
    c.send("l", 0.4)          # expand the session
    c.send("j", 0.3)          # -> Tab 1
    c.send("l", 0.4)          # expand the tab
    c.send("j", 0.3)          # -> the first pane
    sel = sm_selected_row(c)
    # Pane rows sit at the deepest indent (8 columns) and are labelled by the
    # pane's command (e.g. `sh*`), not by a "Pane" literal.
    assert sel and sel.startswith(" " * 8), f"expected a pane row, got {sel!r}"
    c.send(" ", 0.4)          # mark it
    c.send("v", 0.2)
    c.send("a", 0.6)          # AddToView -> picker
    c.send("\r", 1.2)         # confirm: new view (create + enter)


def enter_first_view(c, name="View 1", settle=1.5):
    """Switcher (Alt+s) -> walk to the named view in the Views section -> Enter."""
    c.send(b"\x1bs", 1.0)
    assert sm_goto_row(c, name), f"switcher did not list {name!r}"
    c.send("\r", 0.4)
    c.pump(settle)


def leave_view(c):
    """Prefix q: leave the displayed view back to the foreground session."""
    c.prefix(b"q", 1.0)


def attach_remote_session(c):
    """Session manager: attach to the REMOTE server's session, making the remote
    the client's FOREGROUND connection."""
    sm_open(c)
    assert sm_goto_row(c, "mini"), "remote node 'mini' not in the tree"
    c.send("l", 0.9)               # expand -> its sessions appear
    assert sm_goto_row(c, "rbox"), "remote session 'rbox' not listed"
    c.send("\r", 1.5)              # SwitchSession -> foreground handoff
    c.pump(1.0)


def status_bar(c):
    return c.rows_text()[-1]


# ---------------------------------------------------------------------------
# scenarios
# ---------------------------------------------------------------------------

def main():
    setup_dirs()
    s2 = start_server(SOCK2, env_remote())
    seed_remote_pane()
    s1 = start_server(SOCK1, env_local(f"{RUN}/cfg-none"))
    fails = []
    clients = []

    def fail(msg):
        print("   FAIL:", msg)
        fails.append(msg)

    def check_no_doubled(c, where):
        if c.has("waiting for waiting"):
            fail(f"{where}: nonsense doubled placeholder 'waiting for waiting…' on screen")

    try:
        # -- (1) the composing terminal, remote already CONNECTED ------------
        a = Client("A", f"{RUN}/cfgA")
        clients.append(a)
        a.send(f"clear; printf '{MARK_LOCAL}\\n'\r", 0.6)
        compose_view_over_remote_pane(a)
        a.pump(1.2)
        if VERBOSE:
            a.dump("(1) A in view, local foreground")
        ok = a.has(MARK_REMOTE)
        print(f"(1) composing terminal (remote connected): remote content = {ok}")
        if not ok:
            fail("(1) remote cell never streamed on the composing terminal")
        check_no_doubled(a, "(1)")

        # -- (2) same terminal, but the REMOTE is the FOREGROUND -------------
        # Leave the view, hand the foreground to the remote's own session, then
        # re-enter the view: the remote cell must still stream.
        leave_view(a)
        attach_remote_session(a)
        fg_ok = "rbox" in status_bar(a)
        print(f"(2) foreground handed to the remote session: {fg_ok} "
              f"(status: {status_bar(a).strip()[:60]!r})")
        if not fg_ok:
            fail("(2) could not make the remote the foreground; scenario untested")
        enter_first_view(a)
        a.pump(1.5)
        if VERBOSE:
            a.dump("(2) A in view, REMOTE foreground")
        ok2 = a.has(MARK_REMOTE)
        print(f"(2) view entered with the REMOTE as foreground: remote content = {ok2}")
        if not ok2:
            fail("(2) remote cell stopped streaming when the remote was the foreground")
        check_no_doubled(a, "(2)")

        # -- (3) another terminal that NEVER connected that remote ----------
        #    (the remote IS in its config: it must lazily connect and stream)
        # A deliberately SLOW dial: the lazy connect must not freeze the TUI, and
        # the cell must say what it is doing while the dial is in flight.
        write_shim(delay=5)
        b = Client("B", f"{RUN}/cfgB")
        clients.append(b)
        enter_first_view(b, settle=0.3)
        if VERBOSE:
            b.dump("(3) B mid-dial")
        mid = b.has("connecting to mini")
        print(f"(3a) interim label while dialing: {mid}")
        if not mid:
            fail("(3a) no interim `connecting to …` label during the lazy dial")
        # Responsiveness: the client must still answer input while the SSH dial
        # runs (it would not if the dial were awaited on the event loop).
        b.send(b"\x1bs", 0.8)
        responsive = b.has("Switch Session")
        print(f"(3b) client still answers input mid-dial: {responsive}")
        if not responsive:
            fail("(3b) client froze during the lazy SSH dial (overlay never opened)")
        b.send(b"\x1b", 0.4)      # close the switcher
        b.pump(7.0)               # let the slow dial land
        write_shim()              # restore the fast shim
        if VERBOSE:
            b.dump("(3) B in shared view")
        ok3 = b.has(MARK_REMOTE)
        print(f"(3) other terminal, remote configured but not connected: "
              f"remote content = {ok3}")
        if not ok3:
            fail("(3) a shared view's remote cell never streamed on a terminal that "
                 "had not connected the remote (eternal waiting…)")
        check_no_doubled(b, "(3)")

        # -- (4) a terminal that CANNOT reach the remote at all --------------
        #    (no [remotes.mini] in its config): it must say so honestly and
        #    never sit on `waiting…` forever.
        c = Client("C", f"{RUN}/cfgC")
        clients.append(c)
        enter_first_view(c)
        c.pump(3.0)
        if VERBOSE:
            c.dump("(4) C in shared view")
        text = "\n".join(c.rows_text())
        honest = ("not connected" in text) or ("disconnected" in text)
        still_waiting = "waiting" in text
        print(f"(4) terminal that cannot reach the remote: honest_label={honest} "
              f"still_waiting={still_waiting}")
        if not honest:
            fail("(4) unreachable remote cell shows no honest label")
        if still_waiting:
            fail("(4) unreachable remote cell still sits on a `waiting…` placeholder")
        check_no_doubled(c, "(4)")

        # -- (5) a terminal whose lazy dial FAILS ---------------------------
        #    The remote IS configured, so the cell dials -- and the dial fails.
        #    It must settle on the honest label, not sit on `connecting to …`.
        write_failing_shim()
        d = Client("D", f"{RUN}/cfgB")
        clients.append(d)
        enter_first_view(d)
        d.pump(4.0)
        if VERBOSE:
            d.dump("(5) D after a failed dial")
        text5 = "\n".join(d.rows_text())
        settled = "not connected: mini" in text5
        stuck = ("connecting to" in text5) or ("waiting" in text5)
        print(f"(5) failed lazy dial: honest_label={settled} stuck_label={stuck}")
        if not settled:
            fail("(5) a failed lazy dial does not settle on `not connected: <name>`")
        if stuck:
            fail("(5) a failed lazy dial leaves the cell on a transient/eternal label")
        check_no_doubled(d, "(5)")
        write_shim()

        alive = all(cl.alive() for cl in clients)
        panic = logs_have_panic()
        print(f"all clients alive: {alive}   panic in logs: {panic}")
        if not alive:
            fail("a client exited")
        if panic:
            fail("panic in a log")
        for l in grep_client_log("view:"):
            print("   LOG", l[-150:])
    finally:
        for cl in clients:
            cl.kill()
        s1.kill()
        s2.kill()

    if fails:
        print("RESULT: FAIL")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("RESULT: PASS (remote view cells stream on the composing terminal, with the "
          "remote as foreground, and on a terminal that lazily connects; an "
          "unreachable remote says so honestly)")


if __name__ == "__main__":
    main()
