#!/usr/bin/env python3
"""The agents sidebar panel: does it show the right states, and does Enter go there?

A real PTY, because the panel is painted by the CLIENT around a server frame:
the frame harness sees the content rect alone and would pass on a panel that
never drew a thing.

Everything here runs with NO `[agents]` config at all -- the shipped defaults
have to make the panel work with zero setup, so that is what is tested.

What it covers:

  1  the panel lists every agent pane, across sessions, unasked
  2  a pane running something that is not a configured agent is not listed
  3  the three states paint three DIFFERENT marker colours, and the urgent one
     is the theme's bell colour
  4  Enter jumps across a SESSION boundary -- checked by what appears in the
     content area, not by trusting a label
  5  a refresh that removes an entry ABOVE the selection does not retarget it:
     Enter still goes where the user pointed (identity, not index). Set up so
     that index-preservation and identity-preservation give DIFFERENT answers --
     with the selection on the last row either rule lands in the same place, and
     the check would pass on the bug it exists to catch

Uses a stand-in `claude` script on `PATH`, so no real agent need be installed.

Run from the repo root:
    python3 tests/pty/sidebar_agents.py [-v]
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
RUN = "/tmp/rmx-sba"
SOCK = f"{RUN}/run/remux.sock"
VERBOSE = "-v" in sys.argv

COLS, ROWS = 100, 30
SIDEBAR_W = 26
FRAME = 1  # the sidebar's border is drawn INSIDE the bar

# No `[agents]` table: the shipped defaults are what is under test.
CFG = f"""
[keybindings.command]
"Alt-2" = "SidebarFocusLeft"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "agents"
  weight = 1
"""

# The stand-in agent. `comm` for a `#!`-script is the script's basename, so
# `/proc/<pgid>/comm` reads `claude` exactly as it would for the real thing.
#
#   claude TAG        prints a tag (so a jump can be seen in the content area)
#                     and then waits for commands on its stdin
#   claude spin       prints for ever, so it is permanently `Working`
#   "block" on stdin  prints an approval prompt, so it is `NeedsInput`
AGENT = """#!/bin/sh
printf 'agent ready %s\\n' "$*"
if [ "$1" = "spin" ]; then
  while :; do printf '.'; sleep 0.2; done
fi
while read line; do
  case "$line" in
    block) printf 'Do you want to proceed?\\n> 1. Yes\\n' ;;
    *) printf 'ok\\n' ;;
  esac
done
"""

FAILURES = []


def log(*a):
    if VERBOSE:
        print(*a)


def check(name, cond, detail=""):
    if cond:
        print(f"  PASS  {name}")
    else:
        print(f"  FAIL  {name}\n        {detail}")
        FAILURES.append(name)


# ---------------------------------------------------------------------------
# environment
# ---------------------------------------------------------------------------

def env_local():
    return {
        **os.environ,
        "PATH": f"{RUN}/bin:" + os.environ.get("PATH", ""),
        "XDG_RUNTIME_DIR": f"{RUN}/run",
        "XDG_STATE_HOME": f"{RUN}/state",
        "XDG_DATA_HOME": f"{RUN}/data",
        "XDG_CONFIG_HOME": f"{RUN}/cfg",
        "SHELL": "/bin/sh",
        "ENV": "/dev/null",
        "TERM": "xterm-256color",
        "PS1": "$ ",
        "REMUX_ALLOW_NESTED": "1",
    }


def setup_dirs():
    shutil.rmtree(RUN, ignore_errors=True)
    for s in ("run", "state", "data", "bin", "cfg"):
        os.makedirs(f"{RUN}/{s}", exist_ok=True)
    os.makedirs(f"{RUN}/cfg/remux", exist_ok=True)
    with open(f"{RUN}/cfg/remux/config.toml", "w") as f:
        f.write(CFG)
    for name in ("claude", "notanagent"):
        p = f"{RUN}/bin/{name}"
        with open(p, "w") as f:
            f.write(AGENT)
        os.chmod(p, 0o755)


def start_server():
    p = subprocess.Popen([BIN, "server"], env=env_local(),
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        if os.path.exists(SOCK):
            time.sleep(0.3)
            return p
        time.sleep(0.05)
    p.kill()
    raise SystemExit("server socket never appeared")


def stop_server():
    try:
        subprocess.run([BIN, "stop"], env=env_local(), stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=10)
    except Exception:
        pass


def check_no_panic():
    for name in ("client.log", "server.log"):
        path = f"{RUN}/state/remux/{name}"
        if os.path.exists(path):
            body = open(path, errors="replace").read()
            check(f"no panic in {name}", "panicked at" not in body, body[-1500:])


# ---------------------------------------------------------------------------
# a minimal wire client, used only to SEED panes the panel then lists
# ---------------------------------------------------------------------------

def protocol_version():
    for line in open("src/protocol.rs"):
        if "pub const PROTOCOL_VERSION" in line:
            return int(line.split("=")[1].strip().rstrip(";"))
    raise SystemExit("could not read PROTOCOL_VERSION")


PROTOCOL_VERSION = protocol_version()


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

    def type(self, text, settle=0.5):
        self.send({"Input": {"data": list(text.encode())}})
        time.sleep(settle)

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


# ---------------------------------------------------------------------------
# PTY client
# ---------------------------------------------------------------------------

def spawn():
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, [], env=env_local(), dimensions=(ROWS, COLS),
                          encoding=None)

    def pump(t=0.7):
        end = time.time() + t
        while time.time() < end:
            try:
                chunk = child.read_nonblocking(65536, 0.1)
            except Exception:
                continue
            stream.feed(chunk)

    return child, screen, pump


def panel_rows(screen):
    """The panel's INTERIOR rows, panel-relative (row 0 is the plugin header)."""
    return [
        r[FRAME:SIDEBAR_W - FRAME].rstrip()
        for r in screen.display[FRAME:len(screen.display) - FRAME]
    ]


def agent_rows(screen):
    """Just the agent rows: everything under the header, up to the first blank."""
    out = []
    for r in panel_rows(screen)[1:]:
        if not r.strip():
            break
        out.append(r)
    return out


def content_rows(screen):
    return [r[SIDEBAR_W:] for r in screen.display]


def marker_fg(screen, panel_y):
    """The colour of the state marker on panel row `panel_y`."""
    return str(screen.buffer[FRAME + panel_y][FRAME].fg)


def panel_row_of(screen, needle):
    for y, r in enumerate(panel_rows(screen)):
        if needle in r:
            return y
    return None


def select_row(child, pump, panel_y):
    """Put the selection on panel row `panel_y` (row 0 is the header)."""
    child.send(b"g")
    pump(0.3)
    for _ in range(panel_y - 1):
        child.send(b"j")
        pump(0.12)


def wait_until(pump, cond, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.3)
        if cond():
            return True
    return False


# ---------------------------------------------------------------------------
# the scenario
# ---------------------------------------------------------------------------

def seed():
    """alpha tab 0: a `notanagent`, then `claude spin` (-> Working),
       `claude ALPHATAG` + `block` (-> NeedsInput); alpha tab 1:
       `claude THIRDTAG` (-> Idle); beta: `claude BETATAG` (-> Idle).

    Four agent rows, so case 5 can remove one from ABOVE a selection that is not
    the last row -- which is what makes it able to tell identity-preservation
    apart from index-preservation."""
    a = Wire(SOCK)
    a.send({"CreateSession": {"name": "alpha", "folder": None}})
    a.send({"Attach": {"session_name": "alpha"}})
    a.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    a.type("notanagent NOTME\n")

    a.send({"Command": "PaneSplitVertical"})
    time.sleep(0.5)
    a.type("claude spin\n")

    a.send({"Command": "PaneSplitHorizontal"})
    time.sleep(0.5)
    a.type("claude ALPHATAG\n")
    a.type("block\n")

    # A new TAB, not another split: a fourth pane in this one would leave every
    # pane narrow, and this harness should not depend on how narrow.
    a.send({"Command": "TabNew"})
    time.sleep(0.5)
    a.type("claude THIRDTAG\n")

    # Back to tab 0, so the pane the panel's first rows describe is the one the
    # client actually shows -- case 4 reads the content area to decide where a
    # jump landed, and it has to start somewhere known.
    a.send({"Command": {"SessionSwitchTab": {"session": "alpha", "tab_index": 0}}})
    time.sleep(0.4)

    b = Wire(SOCK)
    b.send({"CreateSession": {"name": "beta", "folder": None}})
    b.send({"Attach": {"session_name": "beta"}})
    b.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    b.type("claude BETATAG\n")
    return a, b


def alpha_pane_ids(w):
    """alpha tab 0's pane ids, in layout order."""
    w.send("ListSessionTree")
    end = time.time() + 3.0
    while time.time() < end:
        msg = w.recv()
        if isinstance(msg, dict) and "SessionTree" in msg:
            body = msg["SessionTree"]
            for sess in list(body["unfiled"]) + [
                s for f in body["folders"] for s in f["sessions"]
            ]:
                if sess["name"] == "alpha":
                    return [p["id"] for p in sess["tabs"][0]["panes"]]
    raise SystemExit("no SessionTree for alpha")


def scenario():
    start_server()
    seeds = seed()
    child, screen, pump = spawn()
    pump(2.5)

    rows = panel_rows(screen)
    log("panel:", rows)
    check("0 the panel headers itself", any(r.startswith("Agents") for r in rows), rows)

    # -- 1: every agent pane, across sessions --------------------------------
    ok = wait_until(pump, lambda: len(agent_rows(screen)) == 4)
    listed = agent_rows(screen)
    check("1 all four agent panes are listed, across both sessions",
          ok and len(listed) == 4, listed)
    check("1 both sessions are named", any("alpha" in r for r in listed)
          and any("beta" in r for r in listed), listed)

    # -- 2: the unlisted command is absent -----------------------------------
    check("2 a pane running an unlisted command is not listed",
          all("notanagent" not in r for r in listed), listed)

    # -- 3: three states, three colours --------------------------------------
    # The default theme's `tab_bell_fg` is Indexed(9) and `tab_activity_fg` is
    # Indexed(11); pyte resolves those to these hexes.
    BELL, ACTIVITY = "ff0000", "ffff00"
    colours = [marker_fg(screen, y) for y in range(1, 1 + len(listed))]
    log("markers:", colours, listed)
    check("3 the three states paint three different colours",
          len(set(colours)) == 3, list(zip(colours, listed)))
    check("3 the blocked agent wears the theme's bell colour",
          BELL in colours, list(zip(colours, listed)))
    check("3 the working agent wears the theme's activity colour",
          ACTIVITY in colours, list(zip(colours, listed)))

    # -- 4: Enter jumps across a session boundary ----------------------------
    body_before = "\n".join(content_rows(screen))
    check("4 the client is attached to alpha to start with",
          "ALPHATAG" in body_before and "BETATAG" not in body_before,
          repr(body_before[-300:]))

    child.send(b"\x1b2")  # Alt-2: focus the panel
    pump(0.5)
    y_beta = panel_row_of(screen, "beta")
    assert y_beta is not None, panel_rows(screen)
    select_row(child, pump, y_beta)
    child.send(b"\r")
    pump(2.0)
    body = "\n".join(content_rows(screen))
    check("4 Enter on an agent row lands in ITS session",
          "BETATAG" in body, repr(body[-400:]))

    # -- 5: a refresh that removes a row above the selection ------------------
    #
    # The rows are ordered by pane id, so alpha's are [spin, ALPHATAG, THIRDTAG]
    # and beta's is last. The selection goes on THIRDTAG (index 2), and the row
    # ABOVE it (ALPHATAG, index 1) is then removed. After that:
    #
    #   identity-preservation -> THIRDTAG, now at index 1
    #   index-preservation    -> whatever slid into index 2, i.e. BETATAG
    #
    # Two different answers, which is the only arrangement that can catch the
    # bug. Selecting the LAST row instead would make both rules agree.
    child.send(b"\x1b2")
    pump(0.5)
    select_row(child, pump, 3)  # panel row 3 = agent index 2 = THIRDTAG
    pump(0.4)

    # Closed by id from another connection, not by typing: `spin` never reads
    # its stdin, and this pane is not the one alpha's focus is on.
    ids = alpha_pane_ids(seeds[0])
    victim = ids[2]  # tab 0 holds notanagent, spin, ALPHATAG
    log("alpha panes:", ids, "closing", victim)
    seeds[0].send({"Command": {"PaneCloseById": {"session": "alpha", "pane_id": victim}}})
    gone = wait_until(pump, lambda: len(agent_rows(screen)) == 3)
    check("5 the closed agent's row goes away", gone, agent_rows(screen))

    # Enter without touching the selection.
    child.send(b"\r")
    pump(2.0)
    body = "\n".join(content_rows(screen))
    check("5 Enter still goes where the user pointed, not to the new occupant "
          "of that index (THIRDTAG, not BETATAG)",
          "THIRDTAG" in body and "BETATAG" not in body, repr(body[-400:]))

    check("client still alive", child.isalive())
    try:
        child.close(force=True)
    except Exception:
        pass
    for w in seeds:
        w.close()


def main():
    if not os.path.exists(BIN):
        raise SystemExit(f"{BIN} not found; run `cargo build` first")
    setup_dirs()
    try:
        scenario()
    finally:
        check_no_panic()
        stop_server()
        time.sleep(0.3)
    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
