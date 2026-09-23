#!/usr/bin/env python3
"""The agent switcher (Alt+a): list every agent pane, jump to one.

A real PTY, because the switcher is a CLIENT overlay: the server only supplies
the agent list it pushes to a subscriber.

What it covers:

  1  with NO sidebar configured, Alt+a lists every agent pane, each with its
     state marker in the agents panel's state colour, and the pane the user is
     in on the current-pane background (the selection sits elsewhere)
  2  a printable key neither narrows the list nor reaches the pane: there is no
     type-to-filter, and the key is swallowed
  3  Esc closes it, and with no panel wanting the list the client unsubscribes
  4  j/j/Enter jumps to the chosen agent's pane, proven by typing into it: the
     stand-in agent answers `GOT:<line>`, a string the typed text never
     contains, so an echo cannot fake it
  5  with an agents SIDEBAR configured, closing the switcher does not tear down
     the panel's subscription: a later state change still reaches the panel
  6  a REMOTE agent (fake ssh -> a second server) is listed host-prefixed and
     jumped to

Uses a stand-in `claude` script on `PATH`, so no real agent need be installed.

Run from the repo root:
    python3 tests/pty/agent_switcher.py [-v]
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

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUN = "/tmp/rmx-asw"
SOCK = f"{RUN}/run/remux.sock"
SOCK2 = f"{RUN}/run2/remux.sock"
VERBOSE = "-v" in sys.argv

COLS, ROWS = 100, 30
SIDEBAR_W = 30

ALT_A = b"\x1ba"
ESC = b"\x1b"


def theme_hex(role):
    """`role`'s Rgb default in `src/config/theme.rs`, as the hex pyte reports."""
    needle = f"{role}: ThemeColor::Rgb("
    for line in open("src/config/theme.rs"):
        if needle in line:
            rgb = line.split(needle)[1].split(")")[0]
            return "".join(f"{int(v):02x}" for v in rgb.split(","))
    raise SystemExit(f"no Rgb default for {role}")


# `tab_bell_fg` / `tab_activity_fg` default to Indexed(9) / Indexed(11), which
# pyte resolves to these; `Idle` is `status_bar_fg`, an Rgb literal.
NEEDS_INPUT, WORKING = "ff0000", "ffff00"
IDLE = theme_hex("status_bar_fg")
CURRENT_BG = theme_hex("sidebar_current_bg")
SELECTED_BG = theme_hex("whichkey_fg")

AGENTS_SIDEBAR = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "agents"
  weight = 1
"""

REMOTE = """
[remotes.mini]
ssh = "whatever"
remux_path = "remux"
auto_connect = true
"""

# The stand-in agent. `comm` for a `#!`-script is the script's basename, so
# `/proc/<pgid>/comm` reads `claude` exactly as it would for the real thing.
#
#   claude TAG        prints a tag, then answers each stdin line with GOT:<line>
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
    *) printf 'GOT:%s\\n' "$line" ;;
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


def env_local(cfg):
    return base_env(f"{RUN}/run", f"{RUN}/state", f"{RUN}/data", f"{RUN}/{cfg}")


def env_remote():
    return base_env(f"{RUN}/run2", f"{RUN}/state2", f"{RUN}/data2", f"{RUN}/cfg-none")


def write_config(name, body):
    os.makedirs(f"{RUN}/{name}/remux", exist_ok=True)
    with open(f"{RUN}/{name}/remux/config.toml", "w") as f:
        f.write(body)


def setup_dirs():
    shutil.rmtree(RUN, ignore_errors=True)
    for s in ("run", "run2", "state", "state2", "data", "data2", "bin"):
        os.makedirs(f"{RUN}/{s}", exist_ok=True)
    write_config("cfg-none", "")
    write_config("cfg-sidebar", AGENTS_SIDEBAR)
    write_config("cfg-remote", REMOTE)
    p = f"{RUN}/bin/claude"
    with open(p, "w") as f:
        f.write(AGENT)
    os.chmod(p, 0o755)
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
    for env in (env_local("cfg-none"), env_remote()):
        try:
            subprocess.run([BIN, "stop"], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=10)
        except Exception:
            pass
    subprocess.run(["pkill", "-x", "-f", f"{BIN} relay"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


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


def seed_local():
    """alpha tab 0: `claude spin` (Working, pane 1), then `claude ALPHATAG` +
    `block` (NeedsInput, pane 2, left focused). beta: `claude BETATAG` (Idle,
    pane 3). Rows are listed by pane id, so the order is spin, ALPHATAG, beta."""
    a = Wire(SOCK)
    a.send({"CreateSession": {"name": "alpha", "folder": None}})
    a.send({"Attach": {"session_name": "alpha"}})
    a.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    a.type("claude spin\n")
    a.send({"Command": "PaneSplitVertical"})
    time.sleep(0.5)
    a.type("claude ALPHATAG\n")
    a.type("block\n")
    b = Wire(SOCK)
    b.send({"CreateSession": {"name": "beta", "folder": None}})
    b.send({"Attach": {"session_name": "beta"}})
    b.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    b.type("claude BETATAG\n")
    return a, b


def seed_remote():
    w = Wire(SOCK2)
    w.send({"CreateSession": {"name": "far", "folder": None}})
    w.send({"Attach": {"session_name": "far"}})
    w.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    w.type("claude FARTAG\n")
    return w


# ---------------------------------------------------------------------------
# PTY client
# ---------------------------------------------------------------------------

def spawn(env):
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, [], env=env, dimensions=(ROWS, COLS), encoding=None)

    def pump(t=0.7):
        end = time.time() + t
        while time.time() < end:
            try:
                chunk = child.read_nonblocking(65536, 0.1)
            except Exception:
                continue
            stream.feed(chunk)

    return child, screen, pump


def wait_until(pump, cond, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.3)
        if cond():
            return True
    return False


def popup_x(screen):
    """The popup's left border column, from its titled top line, or None."""
    for row in screen.display:
        t = row.find("Switch Agent")
        if t >= 0:
            return row.rfind("\u256d", 0, t)
    return None


def switcher_rows(screen):
    """The switcher's agent rows as `(y, x_of_marker, label)`, top to bottom.

    Located by the popup's own border column, never by the first match on a
    row: with an agents sidebar shown, the panel's rows carry the very same
    `● claude …` text further left on the same screen rows."""
    left = popup_x(screen)
    if left is None or left < 0:
        return []
    x = left + 2  # the border, then the leading space
    out = []
    for y, row in enumerate(screen.display):
        if row[x:x + 9] == "\u25cf claude ":
            out.append((y, x, row[x + 2:].split("\u2502")[0].strip()))
    return out


LAYOUT_BG = "8a8a8a"  # `layout_indicator_bg`, Indexed(245), through pyte's palette


def check_paint(label, screen, opened, owned):
    """The overlay and the status bar agree about the screen.

    An overlay is torn down by replaying the front buffer, and with a sidebar
    shown the CLIENT draws the status bar across the last row. Either could
    clobber the other: the popup must sit wholly above the bar, the bar must
    survive while the popup is up, and a closed popup must leave nothing
    behind.

    With no sidebar the bar is the SERVER's, on its frame's last row. The seed
    clients here are smaller than this terminal and the frame is sized to the
    smallest client, so that row is not the terminal's last: the bar is found
    as the lowest row carrying a mode chip, and must end in the layout
    indicator. With a sidebar it must be the last row, full width."""
    rows = screen.display
    chips = [y for y, r in enumerate(rows) if r.startswith(" [")]
    bar_y = chips[-1] if chips else None
    if bar_y is None:
        check(f"{label}: the status bar is on screen", False, rows)
        return
    bar = rows[bar_y]
    end = len(bar.rstrip())
    check(f"{label}: the status bar is intact, ending in the layout indicator",
          end > 0 and str(screen.buffer[bar_y][end].bg) == LAYOUT_BG, repr(bar))
    if owned:
        check(f"{label}: the client's bar spans the last row",
              bar_y == ROWS - 1 and end == COLS - 1, (bar_y, repr(bar)))
    left = popup_x(screen)
    if opened:
        bottom = [y for y, r in enumerate(rows)
                  if left is not None and left >= 0 and r[left:left + 1] == "\u2570"]
        check(f"{label}: the popup is drawn whole, above the status bar",
              left is not None and left >= 0 and bool(bottom) and max(bottom) < bar_y,
              rows)
    else:
        check(f"{label}: the closed popup left nothing behind",
              not any("Switch Agent" in r or "\u25cf claude" in r[SIDEBAR_W:] for r in rows),
              rows)


def switcher_open(screen):
    return any("Switch Agent" in r for r in screen.display)


def cell(screen, y, x):
    return screen.buffer[y][x]


def client_log():
    p = f"{RUN}/state/remux/client.log"
    return open(p, errors="replace").read() if os.path.exists(p) else ""


def check_no_panic(label):
    for state in ("state", "state2"):
        for name in ("client.log", "server.log"):
            path = f"{RUN}/{state}/remux/{name}"
            if os.path.exists(path):
                body = open(path, errors="replace").read()
                check(f"{label}: no panic in {state}/{name}",
                      "panicked at" not in body, body[-1500:])


def teardown(child, wires):
    try:
        child.close(force=True)
    except Exception:
        pass
    for w in wires:
        w.close()
    stop_servers()
    time.sleep(0.5)


# ---------------------------------------------------------------------------
# scenarios
# ---------------------------------------------------------------------------

def scenario_no_sidebar():
    print("no sidebar: list, colours, no filter, close, jump")
    setup_dirs()
    start_server(SOCK, env_local("cfg-none"))
    wires = seed_local()
    child, screen, pump = spawn(env_local("cfg-none"))
    pump(2.5)

    # -- 1 ----------------------------------------------------------------------
    child.send(ALT_A)
    ok = wait_until(pump, lambda: len(switcher_rows(screen)) == 3)
    rows = switcher_rows(screen)
    log("switcher:", rows)
    check("1 Alt+a opens the switcher", switcher_open(screen), screen.display)
    check("1 every agent pane is listed, with the panel's labels",
          ok and [r[2] for r in rows] == ["claude alpha/0", "claude alpha/0", "claude beta/0"],
          rows)
    if len(rows) == 3:
        markers = [str(cell(screen, y, x).fg) for y, x, _ in rows]
        check("1 each marker is in its state colour (Working, NeedsInput, Idle)",
              markers == [WORKING, NEEDS_INPUT, IDLE], markers)
        (y0, x0, _), (y1, x1, _), _ = rows
        check("1 the first row is the selection",
              str(cell(screen, y0, x0 + 3).bg) == SELECTED_BG,
              str(cell(screen, y0, x0 + 3).bg))
        # ALPHATAG (pane 2) is where the seed left alpha's focus.
        ok = wait_until(pump, lambda: str(cell(screen, y1, x1 + 3).bg) == CURRENT_BG,
                        timeout=4.0)
        check("1 the pane the user is in has the current-pane background",
              ok, str(cell(screen, y1, x1 + 3).bg))
        check("1 the current row's marker keeps its state colour",
              str(cell(screen, y1, x1).fg) == NEEDS_INPUT, str(cell(screen, y1, x1).fg))
    check_paint("1 no sidebar, open", screen, True, False)
    check("1 subscribed with no panel configured",
          "subscribing to the agent list" in client_log(), client_log()[-800:])

    # -- 2 ----------------------------------------------------------------------
    before = [r[2] for r in switcher_rows(screen)]
    child.send(b"X")
    pump(0.8)
    check("2 a letter does not narrow the list",
          [r[2] for r in switcher_rows(screen)] == before and switcher_open(screen),
          (before, switcher_rows(screen)))

    # -- 3 ----------------------------------------------------------------------
    child.send(ESC)
    ok = wait_until(pump, lambda: not switcher_open(screen), timeout=3.0)
    check("3 Esc closes the switcher", ok, screen.display)
    check_paint("3 no sidebar, closed", screen, False, False)
    check("2 the letter never reached the pane", not any("X" in r for r in screen.display),
          [r for r in screen.display if "X" in r])
    ok = wait_until(pump, lambda: "unsubscribing from the agent list" in client_log(),
                    timeout=3.0)
    check("3 closing it unsubscribes when no panel wants the list", ok,
          client_log()[-800:])

    # -- 4 ----------------------------------------------------------------------
    child.send(ALT_A)
    wait_until(pump, lambda: len(switcher_rows(screen)) == 3)
    child.send(b"j")
    pump(0.3)
    child.send(b"j")
    pump(0.3)
    child.send(b"\r")
    pump(2.0)
    check("4 Enter closes the switcher", not switcher_open(screen))
    child.send(b"zulu\r")
    ok = wait_until(pump, lambda: any("GOT:zulu" in r for r in screen.display), timeout=4.0)
    body = "\n".join(screen.display)
    check("4 keys now reach the chosen agent's pane", ok, body[-600:])
    check("4 and that pane is beta's", "agent ready BETATAG" in body
          and "ALPHATAG" not in body, body[-600:])

    check("client alive", child.isalive())
    teardown(child, wires)
    check_no_panic("no sidebar")


def panel_marker_fg(screen, needle):
    for y, row in enumerate(screen.display):
        if needle in row[:SIDEBAR_W]:
            return str(screen.buffer[y][1].fg)
    return None


def scenario_with_panel():
    print("agents sidebar: closing the switcher keeps the panel's subscription")
    setup_dirs()
    start_server(SOCK, env_local("cfg-sidebar"))
    wires = seed_local()
    child, screen, pump = spawn(env_local("cfg-sidebar"))
    pump(2.5)
    check("5 the panel lists beta, idle",
          wait_until(pump, lambda: panel_marker_fg(screen, "beta/0") == IDLE),
          panel_marker_fg(screen, "beta/0"))
    child.send(ALT_A)
    check("5 the switcher opens beside the panel",
          wait_until(pump, lambda: len(switcher_rows(screen)) == 3), switcher_rows(screen))
    check_paint("5 sidebar, open", screen, True, True)
    check("5 the panel is still painted under an open switcher",
          panel_marker_fg(screen, "beta/0") == IDLE, panel_marker_fg(screen, "beta/0"))
    child.send(ESC)
    wait_until(pump, lambda: not switcher_open(screen), timeout=3.0)
    pump(1.0)
    check_paint("5 sidebar, closed", screen, False, True)
    check("5 the panel survives the popup's teardown",
          panel_marker_fg(screen, "beta/0") == IDLE, panel_marker_fg(screen, "beta/0"))
    check("5 closing it did not unsubscribe",
          "unsubscribing from the agent list" not in client_log(), client_log()[-800:])
    # beta goes from Idle to NeedsInput AFTER the switcher closed. The panel only
    # hears of that through the push the switcher must not have torn down.
    wires[1].type("block\n")
    ok = wait_until(pump, lambda: panel_marker_fg(screen, "beta/0") == NEEDS_INPUT)
    check("5 the panel still receives state changes", ok, panel_marker_fg(screen, "beta/0"))
    check("client alive", child.isalive())
    teardown(child, wires)
    check_no_panic("panel")


def scenario_remote():
    print("remote: a remote agent is listed and jumped to")
    setup_dirs()
    start_server(SOCK, env_local("cfg-remote"))
    start_server(SOCK2, env_remote())
    wires = list(seed_local()) + [seed_remote()]
    child, screen, pump = spawn(env_local("cfg-remote"))
    pump(3.5)
    child.send(ALT_A)
    ok = wait_until(pump, lambda: len(switcher_rows(screen)) == 4)
    rows = switcher_rows(screen)
    log("switcher:", rows)
    check("6 the remote agent is listed after the local ones, host-prefixed",
          ok and rows[-1][2] == "claude mini:far/0", rows)
    child.send(b"G")
    pump(0.3)
    child.send(b"\r")
    pump(2.5)
    child.send(b"yankee\r")
    ok = wait_until(pump, lambda: any("GOT:yankee" in r for r in screen.display), timeout=5.0)
    body = "\n".join(screen.display)
    check("6 keys now reach the remote agent's pane", ok and "agent ready FARTAG" in body,
          body[-600:])
    check("client alive", child.isalive())
    teardown(child, wires)
    check_no_panic("remote")


def main():
    if not os.path.exists(BIN):
        raise SystemExit(f"{BIN} not found; run `cargo build` first")
    try:
        scenario_no_sidebar()
        scenario_with_panel()
        scenario_remote()
    finally:
        stop_servers()
    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
