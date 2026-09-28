#!/usr/bin/env python3
"""Two client overlays: which-key sizing, and the agent switcher's cursor.

A real PTY, because both are CLIENT overlays that a frame harness never sees.

  W  which-key, at every `which_key_position`: the two longest Alt labels,
     "Alt-s switch session" and "Alt-Space next layout", are drawn in full and
     followed by a gap, not cut short or run into the next column. A narrow
     terminal drops to one column and still fits them; a terminal too narrow
     for any label truncates with an ellipsis instead of drawing nothing.
  A  agent switcher: the popup opens while only a REMOTE agent is listed, then
     local agents arrive. The local rows sort ahead of the remote one, and the
     cursor must stay on row 0 rather than follow the remote row to the end.
     Once the user has moved it, a later list keeps it on the same agent.

Run from the repo root:
    python3 tests/pty/overlay_fixes.py [which|agents] [-v]
"""
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import time

import pexpect
import pyte

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUN = "/tmp/rmxov-a1afb"
SOCK = f"{RUN}/run/remux.sock"
SOCK2 = f"{RUN}/run2/remux.sock"
VERBOSE = "-v" in sys.argv

ALT_A = b"\x1ba"
PREFIX = b"\x01"

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


def theme_hex(role):
    """`role`'s Rgb default in `src/config/theme.rs`, as the hex pyte reports."""
    needle = f"{role}: ThemeColor::Rgb("
    for line in open("src/config/theme.rs"):
        if needle in line:
            rgb = line.split(needle)[1].split(")")[0]
            return "".join(f"{int(v):02x}" for v in rgb.split(","))
    raise SystemExit(f"no Rgb default for {role}")


SELECTED_BG = theme_hex("whichkey_fg")


def protocol_version():
    for line in open("src/protocol.rs"):
        if "pub const PROTOCOL_VERSION" in line:
            return int(line.split("=")[1].strip().rstrip(";"))
    raise SystemExit("could not read PROTOCOL_VERSION")


PROTOCOL_VERSION = protocol_version()

# `/proc/<pgid>/comm` of a `#!` script is the script's basename, so the server
# detects this as `claude` exactly as it would the real agent.
AGENT = """#!/bin/sh
printf 'agent ready %s\\n' "$*"
while read line; do printf 'GOT:%s\\n' "$line"; done
"""


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


def env_local():
    return base_env(f"{RUN}/run", f"{RUN}/state", f"{RUN}/data", f"{RUN}/cfg")


def env_remote():
    return base_env(f"{RUN}/run2", f"{RUN}/state2", f"{RUN}/data2", f"{RUN}/cfg-none")


def write_config(name, body):
    os.makedirs(f"{RUN}/{name}/remux", exist_ok=True)
    with open(f"{RUN}/{name}/remux/config.toml", "w") as f:
        f.write(body)


def setup_dirs(config):
    shutil.rmtree(RUN, ignore_errors=True)
    for s in ("run", "run2", "state", "state2", "data", "data2", "bin"):
        os.makedirs(f"{RUN}/{s}", mode=0o700, exist_ok=True)
    write_config("cfg", config)
    write_config("cfg-none", "")
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
    for env in (env_local(), env_remote()):
        try:
            subprocess.run([BIN, "stop"], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=10)
        except Exception:
            pass
    subprocess.run(["pkill", "-f", f"{RUN}/"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(["pkill", "-x", "-f", f"{BIN} relay"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


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


def spawn(env, cols, rows):
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


def wait_until(pump, cond, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.3)
        if cond():
            return True
    return False


def check_no_panic(label):
    for state in ("state", "state2"):
        for name in ("client.log", "server.log"):
            path = f"{RUN}/{state}/remux/{name}"
            if os.path.exists(path):
                body = open(path, errors="replace").read()
                check(f"{label}: no panic in {state}/{name}",
                      "panicked at" not in body, body[-1500:])


def teardown(child, wires=()):
    try:
        child.close(force=True)
    except Exception:
        pass
    for w in wires:
        w.close()
    stop_servers()
    time.sleep(0.5)


# ---------------------------------------------------------------------------
# W: which-key
# ---------------------------------------------------------------------------

# A label counts as drawn in full only when a space, a border or the row's end
# follows it. A bare substring check passes on "next layoutAlt-a", where the
# label runs straight into the next column.
FULL = {
    "switch session": re.compile(r"Alt-s switch session(?=[ │]|$)"),
    "next layout": re.compile(r"Alt-Space next layout(?=[ │]|$)"),
}


def whichkey_up(screen):
    return any("command palette" in r for r in screen.display)


def labels_in_full(screen):
    return {name: any(rx.search(r) for r in screen.display) for name, rx in FULL.items()}


def whichkey_case(label, position, cols, rows):
    extra = f'which_key_position = "{position}"\n' if position else ""
    setup_dirs(f"[appearance]\n{extra}")
    start_server(SOCK, env_local())
    child, screen, pump = spawn(env_local(), cols, rows)
    pump(2.0)
    child.send(PREFIX)
    up = wait_until(pump, lambda: whichkey_up(screen), timeout=6.0)
    pump(0.5)
    log("\n".join(screen.display))
    found = labels_in_full(screen)
    check(f"W {label}: the popup opens", up, screen.display)
    for name, ok in found.items():
        check(f"W {label}: \"{name}\" is drawn in full", ok,
              [r for r in screen.display if name.split()[0] in r])
    check(f"W {label}: client alive", child.isalive())
    teardown(child)
    check_no_panic(f"W {label}")


def whichkey_tiny():
    """Too narrow for any Alt label: truncated with an ellipsis, not blank."""
    label = "tiny, 20 cols"
    setup_dirs("[appearance]\n")
    start_server(SOCK, env_local())
    child, screen, pump = spawn(env_local(), 20, 60)
    pump(2.0)
    child.send(PREFIX)
    up = wait_until(pump, lambda: any("…" in r for r in screen.display), timeout=6.0)
    pump(0.5)
    log("\n".join(screen.display))
    check(f"W {label}: the popup is drawn, with an ellipsis on a cut label",
          up and any(re.search(r"Alt-s swi[^│]*…", r) for r in screen.display),
          screen.display)
    check(f"W {label}: client alive", child.isalive())
    teardown(child)
    check_no_panic(f"W {label}")


def scenario_whichkey():
    print("which-key: labels drawn in full")
    for position in (None, "anchored", "centered", "full_width"):
        whichkey_case(position or "default (anchored)", position, 120, 40)
    whichkey_case("anchored, 40 cols (one column)", "anchored", 40, 60)
    whichkey_case("centered, 40 cols (one column)", "centered", 40, 60)
    whichkey_tiny()


# ---------------------------------------------------------------------------
# A: agent switcher
# ---------------------------------------------------------------------------

REMOTE = """
[remotes.mini]
ssh = "whatever"
remux_path = "remux"
auto_connect = true
"""

COLS, ROWS = 100, 30


def popup_x(screen):
    for row in screen.display:
        t = row.find("Switch Agent")
        if t >= 0:
            return row.rfind("╭", 0, t)
    return None


def switcher_rows(screen):
    """`(label, selected)` for each agent row, located by the popup's border."""
    left = popup_x(screen)
    if left is None or left < 0:
        return []
    x = left + 2
    out = []
    for y, row in enumerate(screen.display):
        if row[x:x + 9] == "● claude ":
            label = row[x + 2:].split("│")[0].strip()
            out.append((label, str(screen.buffer[y][x + 3].bg) == SELECTED_BG))
    return out


def selected_label(screen):
    sel = [label for label, s in switcher_rows(screen) if s]
    return sel[0] if len(sel) == 1 else sel


def local_agent(w, tag):
    w.send({"Command": "PaneSplitVertical"})
    time.sleep(0.6)
    w.type(f"claude {tag}\n", settle=0.8)


def scenario_agents():
    print("agent switcher: a late local list does not drag the cursor")
    setup_dirs(REMOTE)
    start_server(SOCK, env_local())
    start_server(SOCK2, env_remote())
    far = Wire(SOCK2)
    far.send({"CreateSession": {"name": "far", "folder": None}})
    far.send({"Attach": {"session_name": "far"}})
    far.send({"Resize": {"cols": 80, "rows": 29}})
    time.sleep(0.4)
    far.type("claude FARTAG\n")
    near = Wire(SOCK)
    near.send({"CreateSession": {"name": "alpha", "folder": None}})
    near.send({"Attach": {"session_name": "alpha"}})
    near.send({"Resize": {"cols": 80, "rows": 29}})
    time.sleep(0.4)
    wires = [far, near]

    child, screen, pump = spawn(env_local(), COLS, ROWS)
    pump(3.0)
    child.send(ALT_A)
    ok = wait_until(pump, lambda: [r[0] for r in switcher_rows(screen)] == ["claude mini:far/0"])
    check("A the switcher opens listing only the remote agent", ok, screen.display)

    local_agent(near, "ONE")
    local_agent(near, "TWO")
    ok = wait_until(pump, lambda: len(switcher_rows(screen)) == 3, timeout=10.0)
    rows = switcher_rows(screen)
    log("after the local list:", rows)
    check("A the local agents arrive ahead of the remote one",
          ok and rows[-1][0] == "claude mini:far/0", rows)
    check("A the cursor is on row 0, not dragged to the remote row at the end",
          ok and rows[0][1] and not rows[-1][1], rows)

    child.send(b"G")
    wait_until(pump, lambda: selected_label(screen) == "claude mini:far/0", timeout=3.0)
    local_agent(near, "THREE")
    ok = wait_until(pump, lambda: len(switcher_rows(screen)) == 4, timeout=10.0)
    rows = switcher_rows(screen)
    log("after the user moved and another list arrived:", rows)
    check("A once the user moved it, the cursor stays on the agent they chose",
          ok and selected_label(screen) == "claude mini:far/0" and rows[-1][1], rows)

    check("A client alive", child.isalive())
    teardown(child, wires)
    check_no_panic("A")


def main():
    if not os.path.exists(BIN):
        raise SystemExit(f"{BIN} not found; run `cargo build` first")
    modes = [m for m in sys.argv[1:] if not m.startswith("-")] or ["which", "agents"]
    try:
        if "which" in modes:
            scenario_whichkey()
        if "agents" in modes:
            scenario_agents()
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
