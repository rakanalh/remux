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
  9  with no sidebar configured the client never subscribes -- to the session
     tree OR to the agent list
 10  a push that removes a row ABOVE the selection does not retarget it --
     Enter still jumps to the session the user picked
 11  each of folder, session, tab and pane wears its own icon; a server wears none
 12  right-clicking a folder offers Rename and a Delete that names the sessions
     it takes with it; both land, read back from the server
 13  a click on a tab's marker shows and hides its panes across tree pushes,
     never jumps, and a click on its name still does
 14  right-clicking a pane offers Switch and Close, and each works
 15  right-clicking a tab while another tab is expanded acts on the clicked row
 16  a folder's menu on a NON-foreground connection lands on that server, with a
     same-named local folder left alone
 17  marker clicks line up on a right-edge sidebar as well
 18  `d`: a tab closes at once, a folder or session asks first and names what
     closes, Cancel is the default, a pane or server row does nothing
 19  `?` lists the shortcuts, `?` and Esc dismiss it, also in a narrow panel
 20  `m` on a session moves it between folders and back out
 21  `m` on a tab moves it with live panes; moving the last one removes the
     session and leaves the client on a live one
 22  `t m` in the session manager does the same through the same menu
 23  `d` and `m` on a remote's rows land on that server
 24  `m` says so when there is nowhere to go
 25  `d` on a session's only tab asks, naming the session it ends
 26  killing the session the client is on lands it on a survivor, by right click,
     by `d` and by a folder; with nothing left the client ends
 28  `icons` picks each row's glyph, leaves the marker where it is, follows a
     config reload, and an unknown value keeps the panel on the default
 27  a menu left open when its remote drops: Confirm does not end the client

Run from the repo root:
    python3 tests/pty/sidebar_sessions.py [-v] [scenario_<name> ...]

Set REMUX_SBS_RUN to a short path to run beside another copy of this harness.
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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import sm_open, sm_tree

BIN = os.path.abspath("target/debug/remux")
RUN = os.environ.get("REMUX_SBS_RUN", "/tmp/rmx-sbs")
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
# Desktop notifications subscribe to the agent list with no panel at all, so
# they are off here for 9 to show that the panels alone gate the subscription.
CFG_NONE = """
[appearance]
border_style = "zellij_style"

[notifications]
enabled = false
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
        "HOME": f"{RUN}/home",
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
              "cfg", "cfg-remote", "cfg-plain", "cfg-none", "home"):
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

# The panel's header row plus the blank row under it: tree index 0 is panel
# row 2.
HEADER_ROWS = 2


def cell_rows(screen):
    """Every screen row with one character per CELL, so an index is a column.

    `screen.display` drops the second cell of a wide glyph, which shifts
    everything to its right one column left per icon: a slice at the sidebar's
    edge then takes in the frame and the pane beside it.
    """
    return [
        "".join((screen.buffer[y][x].data or " ") for x in range(screen.columns))
        for y in range(screen.lines)
    ]


def panel_rows(screen):
    """The panel's INTERIOR rows, one string per PANEL row.

    Panel-relative, not screen-relative: row 0 is the plugin's own header, which
    is what the tree navigation below counts `j` presses from. Before the
    sidebar was framed the two coincided.
    """
    return [
        r[FRAME : SIDEBAR_W - FRAME].rstrip()
        for r in cell_rows(screen)[FRAME : screen.lines - FRAME]
    ]


def indent(row):
    return len(row) - len(row.lstrip())


def content_rows(screen):
    """Everything to the RIGHT of the sidebar."""
    return [r[SIDEBAR_W:] for r in cell_rows(screen)]


def panel_row_of(screen, needle):
    """PANEL row of the first panel row containing `needle`."""
    for y, r in enumerate(panel_rows(screen)):
        if needle in r:
            return y
    return None


def select_row(child, screen, pump, needle):
    """Move the panel selection onto the row containing `needle`.

    Row `y` of a full-height left sidebar is tree index `y - HEADER_ROWS`, and
    `g` puts the selection on tree index 0, so the move is unambiguous without
    reading the highlight back.
    """
    y = panel_row_of(screen, needle)
    assert y is not None, f"no panel row {needle!r} in {panel_rows(screen)}"
    child.send(b"g")
    pump(0.3)
    for _ in range(y - HEADER_ROWS):
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
    check("1 the panel headers itself", any(r.strip() == "sessions" for r in rows), rows)
    check("1 the local server row is listed", any("local" in r for r in rows), rows)
    check("1 both sessions are listed",
          any("alpha" in r for r in rows) and any("beta" in r for r in rows), rows)
    # alpha was seeded with two tabs; the session auto-expands on first load,
    # so both of its tab rows must be there, indented under it.
    alpha_y = panel_row_of(screen, "alpha")
    check("1 alpha's two tabs are listed under it (the session auto-expanded)",
          alpha_y is not None
          and indent(rows[alpha_y + 1]) > indent(rows[alpha_y])
          and indent(rows[alpha_y + 2]) > indent(rows[alpha_y]), rows)
    check("1 the server row is marked expanded",
          rows[HEADER_ROWS].lstrip().startswith("\u25bc "), rows)

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
    for _ in range(y_alpha - HEADER_ROWS):
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
    for _ in range(y_alpha - HEADER_ROWS):
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
    for _ in range(y - HEADER_ROWS):
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
    # The agents panel's push is gated on its own flag, and the same way. With
    # no sidebar at all and notifications off, neither subscription may go out.
    check("9 no agents subscription is sent either",
          "SubscribeAgents" not in server_log(),
          "the client subscribed to the agent list with no agents panel configured")
    check("9 no panel is painted",
          not any("sessions" in r for r in screen.display),
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


# ---------------------------------------------------------------------------
# the mouse on the tree: real SGR events through the real client
# ---------------------------------------------------------------------------

MARKERS = "\u25b6\u25bc"
NERD_ICONS = {
    "folder": "\uF07B",
    "session": "\uF120",
    "tab": "\uF24D",
    "pane": "\uF096",
}
ICONS = {
    "folder": "\U0001F4C1",
    "session": "\U0001F4BB",
    "tab": "\U0001F4D1",
    "pane": "\U0001F532",
}


def find_cell(screen, needle, lo=0, hi=None):
    """0-based `(col, row)` of `needle` between columns `lo` and `hi`, or None."""
    for y, row in enumerate(cell_rows(screen)):
        x = row[lo:hi].find(needle)
        if x >= 0:
            return x + lo, y
    return None


def must_find(screen, needle, lo=0, hi=None):
    pos = find_cell(screen, needle, lo, hi)
    assert pos is not None, f"{needle!r} is not on screen:\n" + "\n".join(
        r.rstrip() for r in cell_rows(screen)
    )
    return pos


def press(child, pump, x, y, button=0, wait=0.5):
    """A press and release at the 0-based cell, as a terminal reports it."""
    child.send(f"\x1b[<{button};{x + 1};{y + 1}M".encode())
    pump(0.1)
    child.send(f"\x1b[<{button};{x + 1};{y + 1}m".encode())
    pump(wait)


def marker_of(screen, name, lo=0, hi=SIDEBAR_W):
    """The cell of the marker on `name`'s row, read off the screen.

    Located by the glyph a person aims at, so the harness does not share the
    column arithmetic it is testing.
    """
    _, y = must_find(screen, name, lo, hi)
    row = cell_rows(screen)[y]
    hits = [x for x in range(lo, hi) if row[x] in MARKERS]
    assert len(hits) == 1, f"no single marker on {row.rstrip()!r}"
    return hits[0], y


def server_tree(sock):
    """`(folders, unfiled)` as the SERVER holds them, from a fresh connection."""
    w = Wire(sock)
    try:
        w.send("ListSessionTree")
        while True:
            msg = w.recv()
            if isinstance(msg, dict) and "SessionTree" in msg:
                t = msg["SessionTree"]
                return t["folders"], t["unfiled"]
    finally:
        w.close()


def folder_names(sock):
    return sorted(f["name"] for f in server_tree(sock)[0])


def all_sessions(sock):
    folders, unfiled = server_tree(sock)
    return unfiled + [s for f in folders for s in f["sessions"]]


def session_names(sock):
    return sorted(s["name"] for s in all_sessions(sock))


def session_entry(sock, name):
    return next(s for s in all_sessions(sock) if s["name"] == name)


def tab_names(sock, session):
    return [t["name"] for t in session_entry(sock, session)["tabs"]]


def pane_names(sock, session, tab):
    entry = session_entry(sock, session)
    return [p["name"] for t in entry["tabs"] if t["name"] == tab for p in t["panes"]]


def seed_tree(sock=None):
    """`home`: tabone (paneA | paneB), tabtwo (paneC), the session the client
    attaches to, unfiled. `work` holds wone and wtwo, `junk` holds jone, and
    `spare` is empty. Each pane prints a marker, to tell where a jump landed."""
    sock = sock or SOCK1
    w = Wire(sock)
    w.send({"CreateSession": {"name": "home", "folder": None}})
    w.send({"Attach": {"session_name": "home"}})
    w.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    w.send({"Command": {"TabRename": "tabone"}})
    w.send({"Command": {"PaneRename": "paneA"}})
    w.type("echo MARK_A\n")
    w.send({"Command": "PaneSplitVertical"})
    time.sleep(0.4)
    w.send({"Command": {"PaneRename": "paneB"}})
    w.type("echo MARK_B\n")
    w.send({"Command": "TabNew"})
    time.sleep(0.4)
    w.send({"Command": {"TabRename": "tabtwo"}})
    w.send({"Command": {"PaneRename": "paneC"}})
    w.type("echo MARK_C\n")
    w.send({"Command": {"SessionSwitchTab": {"session": "home", "tab_index": 0}}})
    time.sleep(0.3)
    for folder, sessions in (("work", ("wone", "wtwo")), ("junk", ("jone",)), ("spare", ())):
        w.send({"Command": {"FolderNew": folder}})
        time.sleep(0.1)
        for name in sessions:
            c = Wire(sock)
            c.send({"CreateSession": {"name": name, "folder": folder}})
            time.sleep(0.3)
            c.close()
    time.sleep(0.5)
    return w


def provoke_push(sock, pump, name):
    """Make the server push a tree nobody in this terminal asked for."""
    w = Wire(sock)
    w.send({"CreateSession": {"name": name, "folder": None}})
    pump(1.2)
    w.close()


def start_tree(cfgdir=None):
    return start_tree_at(cfgdir)


def start_tree_at(cfgdir):
    start_server(SOCK1, env_local(cfgdir or f"{RUN}/cfg"))
    seed = seed_tree()
    child, screen, pump = spawn(env_local(cfgdir or f"{RUN}/cfg"))
    pump(2.0)
    must_find(screen, "tabone")
    return seed, child, screen, pump


def scenario_icons():
    """11: each row type carries its own glyph, in the cell after the marker."""
    print("icons")
    seed, child, screen, pump = start_tree()
    x, y = marker_of(screen, "tabone")
    press(child, pump, x, y)

    def icon_of(name):
        mx, my = marker_of(screen, name) if name != "paneA" else (None, None)
        if mx is None:
            nx, my = must_find(screen, name, 0, SIDEBAR_W)
            row = cell_rows(screen)[my]
            return next((c for c in row[:nx] if ord(c) > 0x2FFF), None)
        return cell_rows(screen)[my][mx + 2]

    got = {
        "folder": icon_of("work"),
        "session": icon_of("home"),
        "tab": icon_of("tabone"),
        "pane": icon_of("paneA"),
    }
    log("icons:", got)
    for kind, want in ICONS.items():
        check(f"11 a {kind} row wears its icon", got[kind] == want,
              f"{got} in {panel_rows(screen)}")
    check("11 the four icons are distinct", len(set(got.values())) == 4, got)
    server = next(r for r in panel_rows(screen) if "local" in r)
    check("11 a server row wears no icon",
          not any(i in server for i in ICONS.values()), server)
    check("11 the panel's right frame is unbroken by the wide glyphs",
          all(r[SIDEBAR_W - 1] != " " for r in cell_rows(screen)[1:ROWS - 2]),
          [r[:SIDEBAR_W + 1] for r in cell_rows(screen)])
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def config_with_icons(value):
    return CFG_SIDEBAR.replace('plugin = "sessions"\n',
                               f'plugin = "sessions"\n  icons = {value}\n')


def glyphs_after_markers(screen):
    """{row kind: the cell after the marker and its space}, by cell."""
    rows = cell_rows(screen)
    got = {}
    for kind, name in (("folder", "work"), ("session", "home"), ("tab", "tabone")):
        x, y = marker_of(screen, name)
        got[kind] = rows[y][x + 2]
    tab_x, _ = marker_of(screen, "tabone")
    x, y = must_find(screen, "paneA", 0, SIDEBAR_W)
    # A pane has a blank marker, one indent right of its tab's.
    got["pane"] = rows[y][tab_x + 4]
    return got


def check_style(label, want, screen):
    got = glyphs_after_markers(screen)
    log(f"{label} icons:", got)
    for kind, glyph in want.items():
        check(f"28 {label}: a {kind} row wears {glyph!r}", got[kind] == glyph,
              f"{got} in {panel_rows(screen)}")
    others = [i for s in (ICONS, NERD_ICONS) if s != want for i in s.values()]
    body = "".join(panel_rows(screen))
    check(f"28 {label}: no other style's glyph is drawn",
          not any(i in body for i in others), panel_rows(screen))


def open_tab_by_marker(label, child, screen, pump):
    x, y = marker_of(screen, "tabone")
    press(child, pump, x, y)
    check(f"28 {label}: a click on the marker opens the tab",
          any("paneA" in r for r in panel_rows(screen)), panel_rows(screen))
    return x


def scenario_icon_styles():
    """28: `icons` per panel, read by cell."""
    print("icon styles")
    cfg = f"{RUN}/cfg-icons"
    write_config(cfg, config_with_icons('"nerd"'))
    seed, child, screen, pump = start_tree(cfg)
    nerd_x = open_tab_by_marker("nerd", child, screen, pump)
    check_style("nerd", NERD_ICONS, screen)
    x, y = must_find(screen, "home", 0, SIDEBAR_W)
    mx, _ = marker_of(screen, "home")
    check("28 nerd: a space keeps the glyph off the name",
          x > mx + 3 and cell_rows(screen)[y][mx + 3] == " ", cell_rows(screen)[y])

    write_config(cfg, config_with_icons('"none"'))
    pump(2.0)
    none_x = open_tab_by_marker("none", child, screen, pump)
    check("28 the marker stays in its column across the styles", none_x == nerd_x,
          (nerd_x, none_x))
    body = "".join(panel_rows(screen))
    check("28 none: no glyph from either style",
          not any(i in body for s in (ICONS, NERD_ICONS) for i in s.values()),
          panel_rows(screen))
    mx, y = marker_of(screen, "tabone")
    check("28 none: the name follows the marker with no gap",
          cell_rows(screen)[y][mx + 2:mx + 8] == "tabone", cell_rows(screen)[y])

    write_config(cfg, config_with_icons('"emoji"'))
    pump(2.0)
    emoji_x = open_tab_by_marker("emoji", child, screen, pump)
    check("28 emoji: the marker is in the same column", emoji_x == nerd_x, (nerd_x, emoji_x))
    check_style("emoji", ICONS, screen)

    write_config(cfg, config_with_icons('"sparkly"'))
    pump(2.0)
    open_tab_by_marker("unknown", child, screen, pump)
    check_style("an unknown value", ICONS, screen)
    check("28 an unknown value is warned about",
          "unknown `icons` value \"sparkly\"" in open(f"{RUN}/state/remux/client.log").read())
    check("28 the client survived", child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def scenario_folder_menu():
    """12: the folder menu, locally."""
    print("folder menu")
    seed, child, screen, pump = start_tree()

    x, y = must_find(screen, "work", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    check("12 a folder row opens a menu with Rename",
          find_cell(screen, "Rename") is not None, cell_rows(screen))
    check("12 Delete says it takes the folder's two sessions",
          find_cell(screen, "Delete folder and 2 sessions") is not None,
          cell_rows(screen))
    mx, my = must_find(screen, "Rename")
    press(child, pump, mx + 1, my)
    check("12 Rename opens a prompt titled for a folder",
          find_cell(screen, "Folder") is not None, cell_rows(screen))
    child.send(b"office\r")
    pump(1.2)
    check("12 Rename renames the folder on the server",
          "office" in folder_names(SOCK1) and "work" not in folder_names(SOCK1),
          folder_names(SOCK1))
    check("12 the sessions stay in the renamed folder",
          {"wone", "wtwo"} <= set(session_names(SOCK1)), session_names(SOCK1))
    check("12 the panel shows the new name",
          any("office" in r for r in panel_rows(screen)), panel_rows(screen))

    # Enter on an empty prompt must not leave a folder with no name.
    x, y = must_find(screen, "office", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    mx, my = must_find(screen, "Rename")
    press(child, pump, mx + 1, my)
    child.send(b"\r")
    pump(1.0)
    check("12 an empty name leaves the folder as it was",
          "office" in folder_names(SOCK1) and "" not in folder_names(SOCK1),
          folder_names(SOCK1))

    # Delete asks first, and a dismissed confirmation deletes nothing.
    x, y = must_find(screen, "junk", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    check("12 one session is not pluralised",
          find_cell(screen, "Delete folder and 1 session") is not None
          and find_cell(screen, "1 sessions") is None, cell_rows(screen))
    mx, my = must_find(screen, "Delete folder")
    press(child, pump, mx + 1, my)
    check("12 Delete asks for confirmation",
          find_cell(screen, "Confirm: Delete folder") is not None, cell_rows(screen))
    child.send(b"\x1b")
    pump(0.8)
    check("12 Esc on the confirmation deletes nothing",
          "junk" in folder_names(SOCK1) and "jone" in session_names(SOCK1),
          folder_names(SOCK1))

    press(child, pump, x, y, button=2)
    mx, my = must_find(screen, "Delete folder")
    press(child, pump, mx + 1, my)
    mx, my = must_find(screen, "Confirm: Delete folder")
    press(child, pump, mx + 1, my, wait=1.2)
    check("12 Delete removes the folder", "junk" not in folder_names(SOCK1),
          folder_names(SOCK1))
    check("12 Delete kills the folder's session, as its label says",
          "jone" not in session_names(SOCK1), session_names(SOCK1))
    check("12 sessions outside the folder survive",
          {"home", "wone", "wtwo"} <= set(session_names(SOCK1)), session_names(SOCK1))
    check("12 the panel drops the folder",
          not any("junk" in r for r in panel_rows(screen)), panel_rows(screen))

    # An empty folder has nothing to warn about.
    x, y = must_find(screen, "spare", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    check("12 an empty folder's Delete mentions no sessions",
          find_cell(screen, "Delete folder") is not None
          and find_cell(screen, "Delete folder and") is None, cell_rows(screen))
    child.send(b"\x1b")
    pump(0.5)
    check("12 the client survived", child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def scenario_marker_click():
    """13: the marker toggles, the name selects then jumps."""
    print("marker click")
    seed, child, screen, pump = start_tree()

    def shown(name):
        return any(name in r for r in panel_rows(screen))

    check("13 a tab starts collapsed", not shown("paneA"), panel_rows(screen))
    x, y = marker_of(screen, "tabone")
    press(child, pump, x, y)
    check("13 a click on the marker shows the tab's panes",
          shown("paneA") and shown("paneB"), panel_rows(screen))
    panes_y = [i for i, r in enumerate(panel_rows(screen)) if "paneA" in r or "paneB" in r]
    tab_y = panel_row_of(screen, "tabone")
    check("13 the panes sit directly beneath their tab",
          panes_y == [tab_y + 1, tab_y + 2], panel_rows(screen))

    provoke_push(SOCK1, pump, "push1")
    check("13 an opened tab stays open across a tree push",
          shown("push1") and shown("paneA"), panel_rows(screen))

    x, y = marker_of(screen, "tabone")
    press(child, pump, x + 1, y)
    check("13 the marker's second column hides the panes again",
          not shown("paneA"), panel_rows(screen))
    provoke_push(SOCK1, pump, "push2")
    check("13 a closed tab stays closed across a tree push",
          shown("push2") and not shown("paneA"), panel_rows(screen))

    # A double click toggles twice and goes nowhere, on a tab that is not active.
    x, y = marker_of(screen, "tabtwo")
    child.send(f"\x1b[<0;{x + 1};{y + 1}M\x1b[<0;{x + 1};{y + 1}m".encode())
    child.send(f"\x1b[<0;{x + 1};{y + 1}M\x1b[<0;{x + 1};{y + 1}m".encode())
    pump(1.0)
    body = "\n".join(content_rows(screen))
    check("13 a double click on a marker leaves the tab closed", not shown("paneC"),
          panel_rows(screen))
    check("13 a double click on a marker does not jump",
          "MARK_A" in body and "MARK_C" not in body, body[-300:])

    # The marker click selected the row, so Enter acts on it.
    press(child, pump, x, y)
    check("13 the marker opens an inactive tab", shown("paneC"), panel_rows(screen))
    body = "\n".join(content_rows(screen))
    check("13 a single marker click does not jump",
          "MARK_A" in body and "MARK_C" not in body, body[-300:])

    # A click past the marker is the name: select first, jump on the second.
    x, y = must_find(screen, "tabone", 0, SIDEBAR_W)
    press(child, pump, x, y)
    check("13 a first click on a name neither toggles nor jumps",
          not shown("paneA") and "MARK_A" in "\n".join(content_rows(screen)),
          panel_rows(screen))
    x, y = must_find(screen, "tabtwo", 0, SIDEBAR_W)
    press(child, pump, x, y)
    press(child, pump, x, y, wait=1.2)
    body = "\n".join(content_rows(screen))
    check("13 a second click on a tab's name jumps to it",
          "MARK_C" in body and "MARK_A" not in body, body[-300:])

    # The same marker on a server, a folder and a session.
    for name, child_row in (("work", "wone"), ("wone", None), ("local", "home")):
        x, y = marker_of(screen, name)
        before = panel_rows(screen)
        press(child, pump, x, y)
        after = panel_rows(screen)
        check(f"13 the marker on {name!r} toggles it", before != after, after)
        if child_row:
            check(f"13 closing {name!r} hides {child_row!r}", not shown(child_row), after)
            provoke_push(SOCK1, pump, f"push-{name}")
            check(f"13 a tree push leaves {name!r} closed", not shown(child_row),
                  panel_rows(screen))
        x, y = marker_of(screen, name)
        press(child, pump, x, y)

    check("13 the client survived", child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def scenario_pane_menu():
    """14: Switch and Close on an inner pane row."""
    print("pane menu")
    seed, child, screen, pump = start_tree()
    x, y = marker_of(screen, "tabone")
    press(child, pump, x, y)

    x, y = must_find(screen, "paneA", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    check("14 a pane row offers Switch", find_cell(screen, "Switch") is not None,
          cell_rows(screen))
    check("14 a pane row offers Close", find_cell(screen, "Close pane") is not None,
          cell_rows(screen))
    check("14 a pane row offers no Rename", find_cell(screen, "Rename") is None,
          cell_rows(screen))

    # The split left focus on paneB, so landing in paneA is a real move: the
    # marker assembled by the shell appears in that pane's half of the screen.
    mx, my = must_find(screen, "Switch")
    press(child, pump, mx + 1, my, wait=1.2)
    child.send(b"printf 'HERE:%s\\n' a\r")
    pump(1.0)
    pos = find_cell(screen, "HERE:a", SIDEBAR_W)
    mark = find_cell(screen, "MARK_A", SIDEBAR_W)
    other = find_cell(screen, "MARK_B", SIDEBAR_W)
    check("14 Switch focuses the clicked pane",
          pos is not None and mark is not None and other is not None
          and abs(pos[0] - mark[0]) < abs(pos[0] - other[0]),
          f"HERE {pos} MARK_A {mark} MARK_B {other}")

    x, y = must_find(screen, "paneB", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    mx, my = must_find(screen, "Close pane")
    press(child, pump, mx + 1, my)
    check("14 Close asks for confirmation",
          find_cell(screen, "Confirm: Close pane") is not None, cell_rows(screen))
    mx, my = must_find(screen, "Confirm: Close pane")
    press(child, pump, mx + 1, my, wait=1.2)
    check("14 Close closes the clicked pane on the server",
          pane_names(SOCK1, "home", "tabone") == ["paneA"],
          pane_names(SOCK1, "home", "tabone"))
    check("14 the panel drops the row",
          not any("paneB" in r for r in panel_rows(screen)), panel_rows(screen))
    check("14 the client survived", child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def scenario_tab_menu_expanded():
    """15: rows shift under an expanded tab; the menu must follow the click."""
    print("tab menu while expanded")
    seed, child, screen, pump = start_tree()
    x, y = marker_of(screen, "tabone")
    press(child, pump, x, y)

    x, y = must_find(screen, "tabtwo", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    mx, my = must_find(screen, "Switch")
    press(child, pump, mx + 1, my, wait=1.2)
    body = "\n".join(content_rows(screen))
    check("15 Switch lands on the tab that was clicked",
          "MARK_C" in body and "MARK_A" not in body, body[-300:])

    x, y = must_find(screen, "tabtwo", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    mx, my = must_find(screen, "Close tab")
    press(child, pump, mx + 1, my)
    mx, my = must_find(screen, "Confirm: Close tab")
    press(child, pump, mx + 1, my, wait=1.2)
    check("15 Close closes the tab that was clicked",
          tab_names(SOCK1, "home") == ["tabone"], tab_names(SOCK1, "home"))
    check("15 the expanded tab keeps its panes on show",
          any("paneA" in r for r in panel_rows(screen)), panel_rows(screen))
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def scenario_folder_remote():
    """16: the folder lives on the background connection. Both servers hold a
    folder of the same name, so a command that went to the foreground would act
    on the WRONG folder rather than on nothing."""
    print("folder menu on a remote")
    start_server(SOCK2, env_remote())
    r = Wire(SOCK2)
    r.send({"CreateSession": {"name": "rhome", "folder": None}})
    time.sleep(0.4)
    r.send({"Command": {"FolderNew": "shared"}})
    r.send({"Command": {"FolderNew": "rgone"}})
    time.sleep(0.2)
    for name, folder in (("rone", "shared"), ("rtwo", "rgone")):
        c = Wire(SOCK2)
        c.send({"CreateSession": {"name": name, "folder": folder}})
        time.sleep(0.3)
        c.close()

    start_server(SOCK1, env_local(f"{RUN}/cfg-remote"))
    seed = seed_tree()
    seed.send({"Command": {"FolderNew": "shared"}})
    seed.send({"Command": {"FolderNew": "rgone"}})
    time.sleep(0.3)
    child, screen, pump = spawn(env_local(f"{RUN}/cfg-remote"))
    pump(3.0)

    # Collapsed local folders first, so the remote's rows fit on screen.
    x, y = marker_of(screen, "local")
    press(child, pump, x, y)
    x, y = must_find(screen, "shared", 0, SIDEBAR_W)
    check("16 the remote's folder is the one on screen",
          y > must_find(screen, "mini", 0, SIDEBAR_W)[1], panel_rows(screen))
    press(child, pump, x, y, button=2)
    mx, my = must_find(screen, "Rename")
    press(child, pump, mx + 1, my)
    child.send(b"moved\r")
    pump(1.5)
    check("16 Rename lands on the remote",
          "moved" in folder_names(SOCK2) and "shared" not in folder_names(SOCK2),
          folder_names(SOCK2))
    check("16 the local folder of that name is untouched",
          "shared" in folder_names(SOCK1) and "moved" not in folder_names(SOCK1),
          folder_names(SOCK1))

    x, y = must_find(screen, "rgone", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    mx, my = must_find(screen, "Delete folder")
    press(child, pump, mx + 1, my)
    mx, my = must_find(screen, "Confirm: Delete folder")
    press(child, pump, mx + 1, my, wait=1.5)
    check("16 Delete lands on the remote",
          "rgone" not in folder_names(SOCK2) and "rtwo" not in session_names(SOCK2),
          (folder_names(SOCK2), session_names(SOCK2)))
    check("16 the local folder of that name is untouched",
          "rgone" in folder_names(SOCK1), folder_names(SOCK1))
    check("16 the client survived", child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()
    r.close()


def scenario_right_edge():
    """17: `on_mouse` gets panel-local columns wherever the panel sits."""
    print("marker click on a right-edge sidebar")
    cfg = f"{RUN}/cfg-right"
    write_config(cfg, CFG_SIDEBAR.replace('edge = "left"', 'edge = "right"'))
    start_server(SOCK1, env_local(cfg))
    seed = seed_tree()
    child, screen, pump = spawn(env_local(cfg))
    pump(2.0)
    lo = COLS - SIDEBAR_W
    x, y = marker_of(screen, "tabone", lo, COLS)
    press(child, pump, x, y)
    check("17 a marker click opens the tab", find_cell(screen, "paneA", lo) is not None,
          cell_rows(screen))
    x, y = marker_of(screen, "tabone", lo, COLS)
    press(child, pump, x, y)
    check("17 a second marker click closes it", find_cell(screen, "paneA", lo) is None,
          cell_rows(screen))
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def focus_panel(child, pump):
    child.send(b"\x1b2")
    pump(0.5)


def menu_rows(screen):
    """The open context menu's rows as `(col, row, text)`, off its square frame."""
    rows = cell_rows(screen)
    top = next((y for y, r in enumerate(rows) if "\u250c" in r), None)
    if top is None:
        return []
    left = rows[top].index("\u250c")
    right = rows[top].index("\u2510")
    out = []
    y = top + 1
    while y < len(rows) and rows[y][left] == "\u2502":
        out.append((left + 1, y, rows[y][left + 1:right].strip()))
        y += 1
    return out


def menu_labels(screen):
    return [text for _, _, text in menu_rows(screen)]


def menu_choose(child, pump, screen, label, wait=1.2):
    hit = next(((x, y) for x, y, text in menu_rows(screen) if text == label), None)
    assert hit is not None, f"no menu row {label!r} in {menu_labels(screen)}"
    press(child, pump, hit[0] + 1, hit[1], wait=wait)


def current_row(screen):
    """The panel's row marked as the client's session."""
    return next((r for r in panel_rows(screen) if "* " in r), "")


def tabs_of(sock):
    return {s["name"]: [t["name"] for t in s["tabs"]] for s in all_sessions(sock)}


def folders_of(sock):
    folders, unfiled = server_tree(sock)
    out = {f["name"]: sorted(s["name"] for s in f["sessions"]) for f in folders}
    out[None] = sorted(s["name"] for s in unfiled)
    return out


def scenario_d_key():
    """18: `d` on each row type, with Enter unharmed."""
    print("d key")
    seed, child, screen, pump = start_tree()
    focus_panel(child, pump)

    select_row(child, screen, pump, "tabtwo")
    child.send(b"d")
    pump(1.2)
    check("18 d on a tab shows no dialog", menu_rows(screen) == [], cell_rows(screen))
    check("18 d on a tab closes it", tab_names(SOCK1, "home") == ["tabone"],
          tab_names(SOCK1, "home"))

    select_row(child, screen, pump, "tabone")
    child.send(b"l")
    pump(0.5)
    before = folders_of(SOCK1), tabs_of(SOCK1), pane_names(SOCK1, "home", "tabone")
    for name in ("paneA", "local"):
        select_row(child, screen, pump, name)
        child.send(b"d")
        pump(0.8)
        check(f"18 d on {name!r} shows nothing", menu_rows(screen) == [], menu_rows(screen))
        check(f"18 d on {name!r} changes nothing",
              (folders_of(SOCK1), tabs_of(SOCK1), pane_names(SOCK1, "home", "tabone")) == before)

    before = folders_of(SOCK1), tabs_of(SOCK1)
    select_row(child, screen, pump, "work")
    child.send(b"d")
    pump(0.7)
    labels = menu_labels(screen)
    check("18 d on a folder warns, naming it and what goes",
          "Closes everything under 'work': 2 sessions" in labels, labels)
    check("18 the folder dialog has Confirm and Cancel",
          "Confirm: Delete folder and 2 sessions" in labels and "Cancel" in labels, labels)
    child.send(b"\x1b")
    pump(0.8)
    check("18 Esc on the folder dialog changes nothing",
          (folders_of(SOCK1), tabs_of(SOCK1)) == before and menu_rows(screen) == [])
    child.send(b"d")
    pump(0.7)
    child.send(b"\r")
    pump(0.8)
    check("18 Enter on the opened dialog is Cancel",
          (folders_of(SOCK1), tabs_of(SOCK1)) == before, folders_of(SOCK1))
    child.send(b"d")
    pump(0.7)
    menu_choose(child, pump, screen, "Confirm: Delete folder and 2 sessions")
    check("18 Confirm removes the folder and its sessions",
          "work" not in folders_of(SOCK1)
          and not {"wone", "wtwo"} & set(session_names(SOCK1)), folders_of(SOCK1))

    before = folders_of(SOCK1), tabs_of(SOCK1)
    select_row(child, screen, pump, "jone")
    child.send(b"d")
    pump(0.7)
    labels = menu_labels(screen)
    check("18 d on a session warns, naming it and what goes",
          "Closes everything under 'jone': 1 tab" in labels
          and "Confirm: Kill session" in labels, labels)
    child.send(b"\x1b")
    pump(0.8)
    check("18 Esc on the session dialog changes nothing",
          (folders_of(SOCK1), tabs_of(SOCK1)) == before)
    child.send(b"d")
    pump(0.7)
    menu_choose(child, pump, screen, "Confirm: Kill session")
    check("18 Confirm kills the session", "jone" not in session_names(SOCK1),
          session_names(SOCK1))
    check("18 the folder that held it stays", "junk" in folders_of(SOCK1), folders_of(SOCK1))

    select_row(child, screen, pump, "spare")
    child.send(b"d")
    pump(0.7)
    check("18 an empty folder's dialog says so", "'spare' is empty" in menu_labels(screen),
          menu_labels(screen))
    child.send(b"\x1b")
    pump(0.5)

    provoke_push(SOCK1, pump, "enterme")
    select_row(child, screen, pump, "enterme")
    child.send(b"\r")
    pump(1.5)
    check("18 Enter still switches to a session", "enterme" in current_row(screen),
          panel_rows(screen))
    check("18 the client survived", child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


SHORTCUT_TEXT = ("Next row", "Previous row", "First row", "Last row", "Switch to row",
                 "Expand or collapse", "Collapse", "Expand", "Delete folder, session or tab",
                 "Move session or tab", "Show or hide this list", "Leave the panel")


def scenario_shortcuts():
    """19: `?` reaches the panel, shows the list, and goes away."""
    print("? key")
    cfg = f"{RUN}/cfg-narrow"
    write_config(cfg, CFG_SIDEBAR.replace(f"size = {SIDEBAR_W}", "size = 14"))
    for label, cfgdir in (("wide", None), ("narrow", cfg)):
        seed, child, screen, pump = start_tree_at(cfgdir)
        focus_panel(child, pump)
        child.send(b"?")
        pump(0.7)
        rows = menu_labels(screen)
        for text in SHORTCUT_TEXT:
            check(f"19 {label}: {text!r} is listed", any(r.startswith(text) for r in rows), rows)
        for key in ("j/Down", "Enter", "Space", "h/Left", "Esc"):
            check(f"19 {label}: key {key!r} is shown", any(r.endswith(key) for r in rows), rows)
        child.send(b"?")
        pump(0.7)
        check(f"19 {label}: ? dismisses it", menu_rows(screen) == [], menu_rows(screen))
        child.send(b"?")
        pump(0.7)
        check(f"19 {label}: ? reopens it", menu_rows(screen) != [])
        child.send(b"\x1b")
        pump(0.7)
        check(f"19 {label}: Esc dismisses it", menu_rows(screen) == [], menu_rows(screen))
        child.send(b"?")
        pump(0.7)
        check(f"19 {label}: the panel kept the keyboard after Esc", menu_rows(screen) != [])
        child.send(b"\x1b")
        pump(0.4)
        check_no_panic(f"{RUN}/state")
        teardown(child)
        seed.close()
        stop_servers()
        time.sleep(0.3)
        setup_dirs()
        write_config(cfg, CFG_SIDEBAR.replace(f"size = {SIDEBAR_W}", "size = 14"))


def scenario_m_session():
    """20: into a folder, between folders, back out."""
    print("m on a session")
    seed, child, screen, pump = start_tree()
    focus_panel(child, pump)
    before = folders_of(SOCK1)

    select_row(child, screen, pump, "wone")
    child.send(b"m")
    pump(0.7)
    labels = menu_labels(screen)
    check("20 the picker names the session", "Move session 'wone' to:" in labels, labels)
    check("20 it offers the other folders and the way out",
          {"junk", "spare", "(no folder)"} <= set(labels), labels)
    check("20 it omits the session's current folder", "work" not in labels, labels)
    child.send(b"\x1b")
    pump(0.7)
    check("20 Esc changes nothing", folders_of(SOCK1) == before, folders_of(SOCK1))

    child.send(b"m")
    pump(0.7)
    menu_choose(child, pump, screen, "spare")
    check("20 it moves between folders",
          "wone" in folders_of(SOCK1)["spare"] and "wone" not in folders_of(SOCK1)["work"],
          folders_of(SOCK1))

    select_row(child, screen, pump, "wone")
    child.send(b"m")
    pump(0.7)
    menu_choose(child, pump, screen, "(no folder)")
    check("20 it moves out of its folder", "wone" in folders_of(SOCK1)[None], folders_of(SOCK1))

    select_row(child, screen, pump, "wone")
    child.send(b"m")
    pump(0.7)
    labels = menu_labels(screen)
    check("20 an unfiled session has no way out to offer", "(no folder)" not in labels, labels)
    menu_choose(child, pump, screen, "junk")
    check("20 an unfiled session moves into a folder", "wone" in folders_of(SOCK1)["junk"],
          folders_of(SOCK1))
    check("20 the client survived", child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def assert_moved_pane_runs(child, screen, pump, tag):
    child.send(f"clear; printf 'RAN:%s\\n' {tag}\r".encode())
    pump(1.0)
    return find_cell(screen, f"RAN:{tag}", SIDEBAR_W) is not None


def scenario_m_tab():
    """21: the background tab, then the last one."""
    print("m on a tab")
    seed, child, screen, pump = start_tree()
    focus_panel(child, pump)
    before = tabs_of(SOCK1)

    select_row(child, screen, pump, "tabtwo")
    child.send(b"m")
    pump(0.7)
    labels = menu_labels(screen)
    check("21 the picker names the tab", "Move tab 'tabtwo' to:" in labels, labels)
    check("21 it offers the other sessions", {"jone", "wone", "wtwo"} <= set(labels), labels)
    check("21 it omits the tab's own session", "home" not in labels, labels)
    child.send(b"\x1b")
    pump(0.7)
    check("21 Esc changes nothing", tabs_of(SOCK1) == before, tabs_of(SOCK1))

    child.send(b"m")
    pump(0.7)
    menu_choose(child, pump, screen, "wone")
    check("21 the tab moved", "tabtwo" in tabs_of(SOCK1)["wone"]
          and "tabtwo" not in tabs_of(SOCK1)["home"], tabs_of(SOCK1))
    check("21 a background tab leaves the client where it was",
          "home" in current_row(screen), panel_rows(screen))

    select_row(child, screen, pump, "tabone")
    child.send(b"m")
    pump(0.7)
    menu_choose(child, pump, screen, "jone", wait=2.0)
    check("21 the session that lost its last tab is gone",
          "home" not in session_names(SOCK1), session_names(SOCK1))
    check("21 the tab arrived with both panes",
          pane_names(SOCK1, "jone", "tabone") == ["paneA", "paneB"],
          pane_names(SOCK1, "jone", "tabone"))
    check("21 the client followed the tab", "jone" in current_row(screen), panel_rows(screen))
    check("21 the client is alive", child.isalive())
    body = "\n".join(content_rows(screen))
    check("21 the panes' own output came with them",
          "MARK_A" in body and "MARK_B" in body, body[-400:])
    child.send(b"\x1b")
    pump(0.5)
    check("21 a command in the moved pane runs there",
          assert_moved_pane_runs(child, screen, pump, "tab"), cell_rows(screen))
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def scenario_m_tab_follow():
    """21: a followed move counts as a switch, so Alt+o goes back."""
    print("m on the active tab, then Alt+o")
    seed, child, screen, pump = start_tree()
    focus_panel(child, pump)
    select_row(child, screen, pump, "tabone")
    child.send(b"m")
    pump(0.7)
    menu_choose(child, pump, screen, "wone", wait=2.0)
    check("21 the client followed the active tab", "wone" in current_row(screen),
          panel_rows(screen))
    child.send(b"\x1b")
    pump(0.4)
    child.send(b"\x1bo")
    pump(1.5)
    check("21 Alt+o returns to the session it came from", "home" in current_row(screen),
          panel_rows(screen))
    teardown(child)
    seed.close()


class Drive:
    """What `pty_harness`'s session-manager helpers read, on cell-true rows."""

    def __init__(self, child, screen, pump):
        self.child, self.screen, self.pump = child, screen, pump
        self.rows = screen.lines

    def rows_text(self):
        return cell_rows(self.screen)

    def send(self, data, t=0.4):
        self.child.send(data)
        self.pump(t)

    def prefix(self, keys, t=0.4):
        self.child.send(b"\x01")
        self.pump(0.15)
        self.child.send(keys)
        self.pump(t)


def manager_goto(d, needle):
    """`sm_goto_text`, which stops after one popup of rows, past a scrolled tree."""
    for _ in range(40):
        if any(needle in r.text and r.selected for r in sm_tree(d)):
            return
        d.send(b"j", 0.15)
    raise AssertionError(f"never reached {needle!r} in {sm_tree(d)}")


def scenario_tm_manager():
    """22: the session manager's chord, on the same menu."""
    print("t m in the session manager")
    seed, child, screen, pump = start_tree()
    d = Drive(child, screen, pump)
    sm_open(d)
    before = tabs_of(SOCK1)

    manager_goto(d, "wone")
    d.send(b"t", 0.2)
    d.send(b"m", 0.7)
    check("22 t m on a session row opens nothing", menu_rows(screen) == [], menu_rows(screen))

    manager_goto(d, "tabtwo")
    d.send(b"t", 0.4)
    check("22 the pending-chord hint lists tab move", find_cell(screen, "tab move") is not None,
          cell_rows(screen))
    d.send(b"m", 0.8)
    labels = menu_labels(screen)
    check("22 t m on a tab opens the picker", "Move tab 'tabtwo' to:" in labels, labels)
    check("22 it omits the tab's own session", "home" not in labels, labels)
    child.send(b"\x1b")
    pump(0.8)
    check("22 Esc on the picker changes nothing", tabs_of(SOCK1) == before, tabs_of(SOCK1))
    check("22 Esc leaves the manager open", any("tabtwo" in r.text for r in sm_tree(d)))

    d.send(b"t", 0.2)
    d.send(b"m", 0.8)
    menu_choose(child, pump, screen, "wone")
    check("22 the tab moved", "tabtwo" in tabs_of(SOCK1)["wone"], tabs_of(SOCK1))
    check("22 the manager shows it under its new session",
          [r.text for r in sm_tree(d)].index(
              next(r.text for r in sm_tree(d) if "tabtwo" in r.text))
          > [r.text for r in sm_tree(d)].index(
              next(r.text for r in sm_tree(d) if "wone" in r.text)), sm_tree(d))

    manager_goto(d, "tabone")
    d.send(b"t", 0.2)
    d.send(b"m", 0.8)
    menu_choose(child, pump, screen, "jone", wait=2.0)
    check("22 the emptied session is gone", "home" not in session_names(SOCK1),
          session_names(SOCK1))
    check("22 the tab arrived with its panes",
          pane_names(SOCK1, "jone", "tabone") == ["paneA", "paneB"],
          pane_names(SOCK1, "jone", "tabone"))
    check("22 the client is alive", child.isalive())
    child.send(b"\x1b")
    pump(0.8)
    check("22 a command in the moved pane runs there",
          assert_moved_pane_runs(child, screen, pump, "mgr"), cell_rows(screen))
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def scenario_keys_remote():
    """23: the foreground is local. Both servers hold a session and a folder of
    the same names where they can, so a wrong route acts instead of no-ops."""
    print("d and m on a remote's rows")
    start_server(SOCK2, env_remote())
    r = Wire(SOCK2)
    r.send({"CreateSession": {"name": "rhome", "folder": None}})
    r.send({"Attach": {"session_name": "rhome"}})
    r.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    r.send({"Command": {"TabRename": "rtab"}})
    r.send({"Command": "TabNew"})
    time.sleep(0.4)
    r.send({"Command": {"TabRename": "rkeep"}})
    r.send({"Command": {"FolderNew": "shared"}})
    time.sleep(0.2)
    for name, folder in (("rone", None), ("rtwo", None)):
        c = Wire(SOCK2)
        c.send({"CreateSession": {"name": name, "folder": folder}})
        time.sleep(0.3)
        c.close()

    start_server(SOCK1, env_local(f"{RUN}/cfg-remote"))
    seed = seed_tree()
    seed.send({"Command": {"FolderNew": "shared"}})
    time.sleep(0.3)
    child, screen, pump = spawn(env_local(f"{RUN}/cfg-remote"))
    pump(3.0)
    x, y = marker_of(screen, "local")
    press(child, pump, x, y)
    focus_panel(child, pump)
    local_before = folders_of(SOCK1), tabs_of(SOCK1)

    select_row(child, screen, pump, "rone")
    child.send(b"m")
    pump(0.7)
    menu_choose(child, pump, screen, "shared")
    check("23 m on a session lands on the remote", "rone" in folders_of(SOCK2)["shared"],
          folders_of(SOCK2))

    select_row(child, screen, pump, "rtab")
    child.send(b"m")
    pump(0.7)
    labels = menu_labels(screen)
    check("23 a remote tab is offered only that server's sessions",
          "rone" in labels and "home" not in labels and "wone" not in labels, labels)
    menu_choose(child, pump, screen, "rtwo")
    check("23 m on a tab lands on the remote", "rtab" in tabs_of(SOCK2)["rtwo"],
          tabs_of(SOCK2))

    select_row(child, screen, pump, "rtab")
    child.send(b"d")
    pump(1.2)
    check("23 d on a tab lands on the remote", "rtab" not in tabs_of(SOCK2)["rtwo"],
          tabs_of(SOCK2))

    select_row(child, screen, pump, "rone")
    child.send(b"d")
    pump(0.7)
    menu_choose(child, pump, screen, "Confirm: Kill session")
    check("23 d on a session lands on the remote", "rone" not in session_names(SOCK2),
          session_names(SOCK2))
    check("23 the local server is untouched",
          (folders_of(SOCK1), tabs_of(SOCK1)) == local_before, tabs_of(SOCK1))
    check("23 the client survived", child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()
    r.close()


def scenario_menu_after_remote_drop():
    """27: a menu's action names its connection, which can go while it is open."""
    print("a confirm menu outlives its remote")
    start_server(SOCK2, env_remote())
    r = Wire(SOCK2)
    r.send({"CreateSession": {"name": "rone", "folder": None}})
    time.sleep(0.4)
    start_server(SOCK1, env_local(f"{RUN}/cfg-remote"))
    seed = seed_tree()
    child, screen, pump = spawn(env_local(f"{RUN}/cfg-remote"))
    pump(3.0)
    focus_panel(child, pump)
    select_row(child, screen, pump, "rone")
    child.send(b"d")
    pump(0.7)
    check("27 setup: the confirm is open",
          "Confirm: Kill session" in menu_labels(screen), menu_labels(screen))

    # By position: the drop repaints the panel over the menu, which stays open.
    mx, my = must_find(screen, "Confirm: Kill session")

    subprocess.run(["pkill", "-x", "-f", f"{BIN} relay"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    pump(2.0)
    check("27 setup: the remote's rows are gone",
          not any("rone" in r for r in panel_rows(screen)), panel_rows(screen))
    press(child, pump, mx + 1, my, wait=2.0)
    check("27 the client survived", child.isalive())
    check("27 the failed send is in the log",
          "context menu: the action failed" in open(f"{RUN}/state/remux/client.log").read())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()
    r.close()


def scenario_nowhere():
    """24: one session, no folders."""
    print("m with nowhere to go")
    start_server(SOCK1, env_local())
    w = Wire(SOCK1)
    w.send({"CreateSession": {"name": "only", "folder": None}})
    w.send({"Attach": {"session_name": "only"}})
    time.sleep(0.4)
    w.send({"Command": {"TabRename": "lonetab"}})
    time.sleep(0.2)
    child, screen, pump = spawn(env_local())
    pump(2.0)
    focus_panel(child, pump)
    for row, text in (("only", "No folders to move to"),
                      ("lonetab", "No other session on this server")):
        select_row(child, screen, pump, row)
        child.send(b"m")
        pump(0.7)
        check(f"24 m on {row!r} says so", menu_labels(screen) == [text], menu_labels(screen))
        child.send(b"\r")
        pump(0.4)
        check(f"24 Enter does nothing on {row!r}'s note", menu_labels(screen) == [text],
              menu_labels(screen))
        child.send(b"\x1b")
        pump(0.5)
        check(f"24 Esc closes it on {row!r}", menu_rows(screen) == [])
    check("24 the client survived", child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    w.close()


def scenario_d_only_tab():
    """25: the key press would end the session, so it asks."""
    print("d on a session's only tab")
    seed, child, screen, pump = start_tree()
    focus_panel(child, pump)
    before = session_names(SOCK1)

    select_row(child, screen, pump, "jone")
    child.send(b"j")
    pump(0.3)
    child.send(b"d")
    pump(0.7)
    labels = menu_labels(screen)
    check("25 it names the session it will close",
          "Closes everything under 'jone': 1 tab" in labels
          and "Confirm: Kill session" in labels, labels)
    child.send(b"\x1b")
    pump(0.8)
    check("25 Esc changes nothing", session_names(SOCK1) == before, session_names(SOCK1))
    child.send(b"d")
    pump(0.7)
    menu_choose(child, pump, screen, "Confirm: Kill session")
    check("25 Confirm ends the session", "jone" not in session_names(SOCK1),
          session_names(SOCK1))
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def assert_on_survivor(child, screen, pump, label, gone):
    pump(0.8)
    check(f"26 {label}: the client is alive", child.isalive())
    row = current_row(screen)
    names = session_names(SOCK1)
    on = next((n for n in names if n in row), None)
    check(f"26 {label}: it is on a live session", on is not None, (row, names))
    check(f"26 {label}: the removed session is gone", not set(gone) & set(names), names)
    child.send(b"\x1b")
    pump(0.5)
    child.send(f"printf 'ALIVE:%s\\n' {label.split()[0]}\r".encode())
    pump(0.8)
    check(f"26 {label}: its keys reach a shell",
          find_cell(screen, f"ALIVE:{label.split()[0]}", SIDEBAR_W) is not None,
          cell_rows(screen))
    return on


def scenario_kill_attached():
    """26: every way of removing the session on screen."""
    print("removing the attached session")
    seed, child, screen, pump = start_tree()
    seed.send({"Attach": {"session_name": "wtwo"}})
    pump(0.3)
    for name in ("stay", "gone2"):
        provoke_push(SOCK1, pump, name)

    # Right-click Kill session.
    x, y = must_find(screen, "home", 0, SIDEBAR_W)
    press(child, pump, x, y, button=2)
    menu_choose(child, pump, screen, "Kill session", wait=0.5)
    menu_choose(child, pump, screen, "Confirm: Kill session", wait=1.5)
    assert_on_survivor(child, screen, pump, "rightclick kill", ["home"])

    # `d` on the session the client is on.
    focus_panel(child, pump)
    select_row(child, screen, pump, "gone2")
    child.send(b"\r")
    pump(1.2)
    check("26 setup: on gone2", "gone2" in current_row(screen), panel_rows(screen))
    focus_panel(child, pump)
    select_row(child, screen, pump, "gone2")
    child.send(b"d")
    pump(0.7)
    menu_choose(child, pump, screen, "Confirm: Kill session", wait=1.5)
    assert_on_survivor(child, screen, pump, "dkey kill", ["gone2"])

    # A folder holding the session the client is on, and another client's.
    focus_panel(child, pump)
    select_row(child, screen, pump, "wone")
    child.send(b"\r")
    pump(1.2)
    check("26 setup: on wone", "wone" in current_row(screen), panel_rows(screen))
    focus_panel(child, pump)
    select_row(child, screen, pump, "work")
    child.send(b"d")
    pump(0.7)
    menu_choose(child, pump, screen, "Confirm: Delete folder and 2 sessions", wait=1.5)
    assert_on_survivor(child, screen, pump, "folder delete", ["wone", "wtwo"])

    # Nothing left: the client ends, as it always has.
    for name in list(session_names(SOCK1)):
        focus_panel(child, pump)
        select_row(child, screen, pump, name)
        child.send(b"d")
        pump(0.7)
        menu_choose(child, pump, screen, "Confirm: Kill session", wait=1.5)
    pump(1.0)
    check("26 no sessions are left", session_names(SOCK1) == [], session_names(SOCK1))
    check("26 with nothing left the client ends", not child.isalive())
    check_no_panic(f"{RUN}/state")
    teardown(child)
    seed.close()


def run_scenario(scenario):
    """A missing row stops one scenario, not the ones after it."""
    try:
        scenario()
    except AssertionError as e:
        check(f"{scenario.__name__} ran to its end", False, str(e)[:1500])


def main():
    if not os.path.exists(BIN):
        raise SystemExit(f"{BIN} not found; run `cargo build` first")
    only = [a for a in sys.argv[1:] if a.startswith("scenario_")]
    for scenario in (scenario_local, scenario_remote, scenario_no_sidebar,
                     scenario_selection_identity, scenario_icons, scenario_icon_styles,
                     scenario_folder_menu, scenario_marker_click,
                     scenario_pane_menu, scenario_tab_menu_expanded,
                     scenario_folder_remote, scenario_right_edge,
                     scenario_d_key, scenario_shortcuts, scenario_m_session,
                     scenario_m_tab, scenario_m_tab_follow, scenario_tm_manager,
                     scenario_keys_remote, scenario_d_only_tab, scenario_kill_attached,
                     scenario_menu_after_remote_drop, scenario_nowhere):
        if only and scenario.__name__ not in only:
            continue
        setup_dirs()
        try:
            run_scenario(scenario)
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
