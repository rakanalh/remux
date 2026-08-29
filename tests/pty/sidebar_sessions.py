#!/usr/bin/env python3
"""The session-tree sidebar panel: does it list every server, and does Enter go there?

Spec assertion 11. Everything here runs with a `sessions` panel CONFIGURED --
with no `[[sidebar]]` the panel rects are empty and none of this code is
reachable, so a sidebar-less run would be structurally blind. The one exception
is the explicit regression case at the end, which asserts the opposite: with no
sidebar the client must not put a single extra byte on the wire.

The cross-machine half uses the fake-`ssh` recipe from `CLAUDE.md` -- a second
isolated server plus a shim on `PATH` that execs `remux relay` at it -- so no
real SSH is involved.

What it covers:

  1  the panel lists the LOCAL tree (server row, sessions, tabs) unasked
  2  a connected REMOTE appears as a second subtree, expanded, beside it
  3  Enter on a SESSION row lands in that session
  4  Enter on a TAB row lands in that tab
  5  Enter on a PANE row lands in that pane -- checked by typing into the pane
     the jump focused and seeing WHERE the text appears, not by trusting a label
  6  Enter on a REMOTE pane row attaches to the remote
  7  a structural change made by somebody else repaints the panel with no
     keystroke at all (the `SubscribeSessionTree` push)
  8  a connection that drops takes its subtree with it
  9  with no sidebar configured the client never subscribes
 10  a push that removes a row ABOVE the selection does not retarget it --
     Enter still jumps to the session the user picked

Run from the repo root:
    python3 tests/pty/sidebar_sessions.py [-v]
"""
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import time

import pexpect
import pyte

BIN = os.path.abspath("target/debug/remux")
RUN = "/tmp/rmx-sbs"
SOCK1 = f"{RUN}/run/remux.sock"
SOCK2 = f"{RUN}/run2/remux.sock"
VERBOSE = "-v" in sys.argv

COLS, ROWS = 100, 30
SIDEBAR_W = 26

PANEL = """
  [[sidebar.panel]]
  plugin = "sessions"
  weight = 1
"""

# `Alt-2` focuses the panel outright: the directional route into a sidebar is
# Task 7's business and is tested there, and an explicit binding keeps this
# harness honest about what it is measuring.
CFG_SIDEBAR = f"""
[keybindings.command]
"Alt-2" = "SidebarFocusLeft"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true
{PANEL}"""

CFG_SIDEBAR_REMOTE = (
    CFG_SIDEBAR
    + """
[remotes.mini]
ssh = "whatever"
remux_path = "remux"
auto_connect = true
"""
)

# The regression baseline: a valid config with no `[[sidebar]]` at all.
CFG_NONE = """
[appearance]
border_style = "zellij_style"
"""

FAILURES = []


def log(*a):
    if VERBOSE:
        print(*a)


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


def env_local(cfgdir=f"{RUN}/cfg"):
    return base_env(f"{RUN}/run", f"{RUN}/state", f"{RUN}/data", cfgdir)


def env_remote():
    return base_env(f"{RUN}/run2", f"{RUN}/state2", f"{RUN}/data2", f"{RUN}/cfg-none")


def write_config(cfgdir, body):
    os.makedirs(f"{cfgdir}/remux", exist_ok=True)
    with open(f"{cfgdir}/remux/config.toml", "w") as f:
        f.write(body)


def write_shim():
    shim = f"{RUN}/bin/ssh"
    with open(shim, "w") as f:
        f.write(
            "#!/bin/sh\n"
            f"export XDG_RUNTIME_DIR={RUN}/run2\n"
            f"export XDG_STATE_HOME={RUN}/state2\n"
            f"export XDG_DATA_HOME={RUN}/data2\n"
            f"export XDG_CONFIG_HOME={RUN}/cfg-none\n"
            f"exec {BIN} relay\n"
        )
    os.chmod(shim, 0o755)


def setup_dirs():
    shutil.rmtree(RUN, ignore_errors=True)
    for s in ("run", "run2", "state", "state2", "data", "data2", "bin",
              "cfg", "cfg-remote", "cfg-plain", "cfg-none"):
        os.makedirs(f"{RUN}/{s}", exist_ok=True)
    write_shim()
    write_config(f"{RUN}/cfg", CFG_SIDEBAR)
    write_config(f"{RUN}/cfg-remote", CFG_SIDEBAR_REMOTE)
    write_config(f"{RUN}/cfg-plain", CFG_NONE)
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


def stop_servers():
    for env in (env_local(), env_remote()):
        try:
            subprocess.run([BIN, "stop"], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=10)
        except Exception:
            pass


# ---------------------------------------------------------------------------
# a minimal wire client, used only to SEED structure the panel then renders
# ---------------------------------------------------------------------------

class Wire:
    def __init__(self, sock):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(sock)
        self.s.settimeout(3.0)
        self.buf = b""
        self.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "harness"})
        self.recv()

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

    def type(self, text):
        self.send({"Input": {"data": list(text.encode())}})
        time.sleep(0.35)

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


def protocol_version():
    src = open("src/protocol.rs").read()
    for line in src.splitlines():
        if "PROTOCOL_VERSION" in line and "=" in line and "pub const" in line:
            return int(line.split("=")[1].strip().rstrip(";"))
    raise SystemExit("could not read PROTOCOL_VERSION")


PROTOCOL_VERSION = protocol_version()


# ---------------------------------------------------------------------------
# PTY client
# ---------------------------------------------------------------------------

def spawn(env, cols=COLS, rows=ROWS):
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, [], env=env, dimensions=(rows, cols), encoding=None)

    def pump(t=0.7):
        end = time.time() + t
        while time.time() < end:
            try:
                chunk = child.read_nonblocking(65536, 0.1)
            except Exception:
                continue
            stream.feed(chunk)

    return child, screen, pump


# The sidebar is framed in the session's border style, and the frame is drawn
# INSIDE the bar, so the panel is the bar minus one cell on every side.
FRAME = 1


def panel_rows(screen):
    """The panel's INTERIOR rows, one string per PANEL row.

    Panel-relative, not screen-relative: row 0 is the plugin's own header, which
    is what the tree navigation below counts `j` presses from. Before the
    sidebar was framed the two coincided.
    """
    return [
        r[FRAME : SIDEBAR_W - FRAME].rstrip()
        for r in screen.display[FRAME : len(screen.display) - FRAME]
    ]


def content_rows(screen):
    """Everything to the RIGHT of the sidebar."""
    return [r[SIDEBAR_W:] for r in screen.display]


def panel_row_of(screen, needle):
    """PANEL row of the first panel row containing `needle`."""
    for y, r in enumerate(panel_rows(screen)):
        if needle in r:
            return y
    return None


def select_row(child, screen, pump, needle):
    """Move the panel selection onto the row containing `needle`.

    Row `y` of a full-height left sidebar is tree index `y - 1` (row 0 is the
    header), and `g` puts the selection on tree index 0, so the move is
    unambiguous without reading the highlight back.
    """
    y = panel_row_of(screen, needle)
    assert y is not None, f"no panel row {needle!r} in {panel_rows(screen)}"
    child.send(b"g")
    pump(0.3)
    for _ in range(y - 1):
        child.send(b"j")
        pump(0.12)
    return y


def check(name, cond, detail=""):
    if cond:
        print(f"  PASS  {name}")
    else:
        print(f"  FAIL  {name}\n        {detail}")
        FAILURES.append(name)


def check_no_panic(*states):
    for state in states:
        for name in ("client.log", "server.log"):
            path = f"{state}/remux/{name}"
            if os.path.exists(path):
                body = open(path, errors="replace").read()
                assert "panicked" not in body, f"{path} panicked:\n{body[-2000:]}"


def server_log(state=f"{RUN}/state"):
    path = f"{state}/remux/server.log"
    return open(path, errors="replace").read() if os.path.exists(path) else ""


def teardown(child):
    try:
        child.close(force=True)
    except Exception:
        pass


# ---------------------------------------------------------------------------
# scenarios
# ---------------------------------------------------------------------------

def seed_local():
    """alpha: tab 0 with two panes (A_ONE | A_TWO), tab 1 (A_TAB1).
       beta:  one pane (B_ONE)."""
    w = Wire(SOCK1)
    w.send({"CreateSession": {"name": "alpha", "folder": None}})
    w.send({"Attach": {"session_name": "alpha"}})
    w.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    w.type("echo A_ONE\n")
    w.send({"Command": "PaneSplitVertical"})
    time.sleep(0.4)
    w.type("echo A_TWO\n")
    w.send({"Command": "TabNew"})
    time.sleep(0.4)
    w.type("echo A_TAB1\n")
    w.send({"Command": {"SessionSwitchTab": {"session": "alpha", "tab_index": 0}}})
    time.sleep(0.3)

    w2 = Wire(SOCK1)
    w2.send({"CreateSession": {"name": "beta", "folder": None}})
    w2.send({"Attach": {"session_name": "beta"}})
    w2.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    w2.type("echo B_ONE\n")
    return w, w2


def seed_remote():
    w = Wire(SOCK2)
    w.send({"CreateSession": {"name": "mini1", "folder": None}})
    w.send({"Attach": {"session_name": "mini1"}})
    w.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    w.type("echo R_ONE\n")
    return w


def scenario_local():
    """1, 3, 4, 5, 7: the local tree, the three jump levels, and the live push."""
    print("local tree, jump levels, live push")
    start_server(SOCK1, env_local())
    seeds = seed_local()
    child, screen, pump = spawn(env_local())
    pump(2.0)

    rows = panel_rows(screen)
    log("panel:", rows)
    check("1 the panel headers itself", any(r.startswith("Sessions") for r in rows), rows)
    check("1 the local server row is listed", any("local" in r for r in rows), rows)
    check("1 both sessions are listed",
          any("alpha" in r for r in rows) and any("beta" in r for r in rows), rows)
    # alpha was seeded with two tabs; the session auto-expands on first load,
    # so both of its tab rows must be there, indented under it.
    alpha_y = panel_row_of(screen, "alpha")
    check("1 alpha's two tabs are listed under it (the session auto-expanded)",
          alpha_y is not None
          and rows[alpha_y + 1].startswith("    ")
          and rows[alpha_y + 2].startswith("    "), rows)
    check("1 the server row is marked expanded", rows[1].startswith("\u25bc "), rows)

    # -- 7: a structural change nobody in this terminal asked for -------------
    w3 = Wire(SOCK1)
    w3.send({"CreateSession": {"name": "pushed", "folder": None}})
    pump(1.5)
    check("7 a session created elsewhere appears with no keystroke",
          any("pushed" in r for r in panel_rows(screen)), panel_rows(screen))
    w3.close()

    # -- 3: Enter on a SESSION row -------------------------------------------
    child.send(b"\x1b2")           # Alt-2: focus the panel
    pump(0.5)
    select_row(child, screen, pump, "beta")
    child.send(b"\r")
    pump(1.5)
    body = "\n".join(content_rows(screen))
    check("3 Enter on a session row lands in that session",
          "B_ONE" in body and "A_ONE" not in body, repr(body[-400:]))

    # -- 4: Enter on a TAB row -----------------------------------------------
    child.send(b"\x1b2")
    pump(0.5)
    # alpha's second tab. Tab rows carry the tab's own name; the seeded tabs are
    # the shell's default names, so target the row by position: the tab rows sit
    # directly under the `alpha` session row.
    y_alpha = panel_row_of(screen, "alpha")
    assert y_alpha is not None, panel_rows(screen)
    child.send(b"g")
    pump(0.3)
    for _ in range(y_alpha - 1):
        child.send(b"j")
        pump(0.1)
    child.send(b"j")               # tab 0
    pump(0.2)
    child.send(b"j")               # tab 1
    pump(0.2)
    child.send(b"\r")
    pump(1.5)
    body = "\n".join(content_rows(screen))
    check("4 Enter on a tab row lands in that tab",
          "A_TAB1" in body and "A_ONE" not in body, repr(body[-400:]))

    # -- 5: Enter on a PANE row ----------------------------------------------
    # Go back to alpha's tab 0 (two panes side by side), select its SECOND pane
    # and prove the jump focused it by typing and seeing where the text lands.
    child.send(b"\x1b2")
    pump(0.5)
    y_alpha = panel_row_of(screen, "alpha")
    child.send(b"g")
    pump(0.3)
    for _ in range(y_alpha - 1):
        child.send(b"j")
        pump(0.1)
    child.send(b"j")               # tab 0
    pump(0.2)
    child.send(b"l")               # expand it -> its panes
    pump(0.5)
    # The FIRST pane -- deliberately not the one tab 0 already has focused (the
    # split made the right-hand pane current). A session- or tab-level jump
    # would land on the right; only a pane-level one lands on the left, so this
    # is what makes the assertion discriminate.
    child.send(b"j")               # pane 0 (the left-hand split)
    pump(0.2)
    child.send(b"\r")
    pump(1.5)
    child.send(b"echo PANEMARK\r")
    pump(1.5)
    mark_cols = [r.find("PANEMARK") for r in content_rows(screen) if "PANEMARK" in r]
    log("PANEMARK columns:", mark_cols, "content width:", COLS - SIDEBAR_W)
    check("5 Enter on a pane row focuses THAT pane, not its tab's current one",
          bool(mark_cols) and max(mark_cols) < (COLS - SIDEBAR_W) // 2,
          f"PANEMARK at content columns {mark_cols}; expected the left-hand split")

    check_no_panic(f"{RUN}/state")
    teardown(child)
    for w in seeds:
        w.close()


def scenario_remote():
    """2, 6, 8: a remote subtree, a jump onto it, and losing the connection."""
    print("remote subtree, remote jump, connection lost")
    start_server(SOCK1, env_local())
    start_server(SOCK2, env_remote())
    seeds = seed_local()
    rseed = seed_remote()

    child, screen, pump = spawn(base_env(f"{RUN}/run", f"{RUN}/state",
                                         f"{RUN}/data", f"{RUN}/cfg-remote"))
    pump(3.0)
    rows = panel_rows(screen)
    log("panel:", rows)
    check("2 the local subtree is listed", any("alpha" in r for r in rows), rows)
    check("2 the remote server row is listed", any("mini" in r for r in rows), rows)
    check("2 the remote's sessions are listed too (its node is expanded)",
          any("mini1" in r for r in rows), rows)

    # -- 6: Enter on a REMOTE pane row ---------------------------------------
    child.send(b"\x1b2")
    pump(0.5)
    y = panel_row_of(screen, "mini1")
    assert y is not None, rows
    child.send(b"g")
    pump(0.3)
    for _ in range(y - 1):
        child.send(b"j")
        pump(0.1)
    child.send(b"j")               # the tab under mini1
    pump(0.2)
    child.send(b"l")               # expand it
    pump(0.5)
    child.send(b"j")               # its pane
    pump(0.2)
    child.send(b"\r")
    pump(2.0)
    body = "\n".join(content_rows(screen))
    check("6 Enter on a remote pane row attaches to the remote",
          "R_ONE" in body, repr(body[-400:]))

    # -- 8: the remote goes away ---------------------------------------------
    # Kill the RELAY, not the far server: that is what a dropped SSH looks like
    # from here, and it is the only thing that closes the client's end. (The
    # relay outlives the server it was talking to -- a separate quirk, and not
    # this task's to fix.) Matched on the absolute path of the binary under
    # test so a real `remux relay` elsewhere on the machine is never touched.
    subprocess.run(["pkill", "-x", "-f", f"{BIN} relay"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    pump(3.0)
    rows = panel_rows(screen)
    log("panel after the remote died:", rows)
    check("8 a dropped connection takes its subtree with it",
          not any("mini1" in r for r in rows), rows)
    check("8 the local subtree survives it", any("alpha" in r for r in rows), rows)
    check("8 the client is still alive", child.isalive())

    check_no_panic(f"{RUN}/state", f"{RUN}/state2")
    teardown(child)
    for w in seeds:
        w.close()
    rseed.close()


def scenario_no_sidebar():
    """9: with no sidebar configured, nothing changes -- including on the wire."""
    print("regression: no sidebar configured")
    start_server(SOCK1, env_local())
    seeds = seed_local()
    child, screen, pump = spawn(base_env(f"{RUN}/run", f"{RUN}/state",
                                         f"{RUN}/data", f"{RUN}/cfg-plain"))
    pump(2.0)
    child.send(b"echo NOSIDEBAR\r")
    pump(1.0)
    body = "\n".join(screen.display)
    check("9 the client works normally", "NOSIDEBAR" in body, repr(body[-300:]))
    check("9 no session-tree subscription is sent",
          "SubscribeSessionTree" not in server_log(),
          "the client subscribed with no sessions panel configured")
    check("9 no panel is painted",
          not any("Sessions" in r for r in screen.display),
          "a panel painted with no sidebar configured")
    check_no_panic(f"{RUN}/state")
    teardown(child)
    for w in seeds:
        w.close()


def scenario_selection_identity():
    """10: a push that removes a row ABOVE the selection must not retarget it.

    The panel rebuilds on every push and Enter is its primary action, so an
    index-preserved selection is a wrong-JUMP bug here, not a cosmetic one.

    The fixture is built so the two behaviours land on DIFFERENT sessions. The
    server lists sessions alphabetically -- alpha, beta, delta, gamma -- so with
    `beta` (2 rows) deleted from above a selected `delta`, the old index lands
    on `gamma`. Selecting the LAST session instead would not discriminate: the
    clamp would land inside that same session's own tab row.
    """
    print("selection survives a row disappearing above it")
    start_server(SOCK1, env_local())
    seeds = seed_four()
    child, screen, pump = spawn(env_local())
    pump(2.0)

    rows = panel_rows(screen)
    log("panel:", rows)
    for want in ("alpha", "beta", "gamma", "delta"):
        assert any(want in r for r in rows), f"{want} missing: {rows}"

    child.send(b"\x1b2")           # focus the panel
    pump(0.5)
    select_row(child, screen, pump, "delta")

    # Somebody else deletes `beta` -- two rows above the selection.
    w = Wire(SOCK1)
    w.send({"KillSession": {"name": "beta"}})
    pump(2.0)
    rows = panel_rows(screen)
    log("panel after the delete:", rows)
    check("10 the deleted session is gone from the panel",
          not any("beta" in r for r in rows), rows)

    child.send(b"\r")
    pump(2.0)
    body = "\n".join(content_rows(screen))
    check("10 Enter still jumps to the session that was selected",
          "D_ONE" in body and "G_ONE" not in body,
          f"expected D_ONE (delta), not G_ONE (gamma): {body[-400:]!r}")

    check_no_panic(f"{RUN}/state")
    teardown(child)
    w.close()
    for x in seeds:
        x.close()


def seed_four():
    """alpha (2 tabs, the session the client attaches to and nobody deletes),
    then beta / gamma / delta, one pane each with a unique marker."""
    out = []
    w = Wire(SOCK1)
    w.send({"CreateSession": {"name": "alpha", "folder": None}})
    w.send({"Attach": {"session_name": "alpha"}})
    w.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    w.type("echo A_ONE\n")
    w.send({"Command": "TabNew"})
    time.sleep(0.4)
    w.type("echo A_TAB1\n")
    out.append(w)
    for name, mark in (("beta", "B_ONE"), ("gamma", "G_ONE"), ("delta", "D_ONE")):
        c = Wire(SOCK1)
        c.send({"CreateSession": {"name": name, "folder": None}})
        c.send({"Attach": {"session_name": name}})
        c.send({"Resize": {"cols": 74, "rows": 29}})
        time.sleep(0.4)
        c.type(f"echo {mark}\n")
        out.append(c)
    return out


def main():
    if not os.path.exists(BIN):
        raise SystemExit(f"{BIN} not found; run `cargo build` first")
    for scenario in (scenario_local, scenario_remote, scenario_no_sidebar,
                     scenario_selection_identity):
        setup_dirs()
        try:
            scenario()
        finally:
            stop_servers()
            time.sleep(0.3)
    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
