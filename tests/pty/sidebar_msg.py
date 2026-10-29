#!/usr/bin/env python3
"""The agents panel shows each agent's messaging name and its unread mail.

A real PTY, because the panel is painted by the CLIENT. The stand-in `claude`
names ITSELF (its own process runs `remux agent name alpha`), so the name is
set by the agent, not typed into a shell before it.

  3a  the row's detail line, under its name, reads "claude @alpha", with no
      badge
  3b  two messages sent to alpha: within 3 s the row ends "*2", in the
      theme's bell colour, though no pane printed anything (AGENTS_DIRTY on
      mail, not on output). The sender is one python process held on a file
      from spawn to the end, because a shell typing `remux msg send` shows
      output and changes its foreground process, and either one marks the
      list dirty by itself.
  3c  the agent reads its mail silently: the badge is gone within 1.5 s
  3d  the name cleared: the suffix is gone; a 32-character name with unread
      mail: the label is cut and the badge still holds the right edge
  3e  Alt+a: the switcher's row for the agent carries " @alpha"
  3z  no panic; the client is alive

Run from the repo root:  python3 tests/pty/sidebar_msg.py   (RMX_RUNDIR overrides)
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
RUN = os.environ.get("RMX_RUNDIR", "/tmp/rmx-sbmsg")
WIRE_SEND = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame", "wire_send.py")
SOCK = f"{RUN}/run/remux.sock"

COLS, ROWS = 100, 30
SIDEBAR_W = 26
FRAME = 1
LONG_NAME = "abcdefghijklmnopqrstuvwxyz012345"

CFG = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "agents"
  weight = 1
"""

# The stand-in: `comm` for a direct `#!` script is its basename, `claude`.
AGENT = f"""#!/bin/sh
'{BIN}' agent name alpha > /dev/null 2>&1
while read line; do
  case "$line" in
    read) '{BIN}' msg read --format json > /dev/null 2>&1 ;;
    clear) '{BIN}' agent name --clear > /dev/null 2>&1 ;;
    long) '{BIN}' agent name {LONG_NAME} > /dev/null 2>&1 ;;
    name) '{BIN}' agent name alpha > /dev/null 2>&1 ;;
  esac
done
"""

FAILURES = []


def check(name, cond, detail=""):
    print(("PASS  " if cond else "FAIL  ") + name + (f"  [{detail}]" if detail and not cond else ""))
    if not cond:
        FAILURES.append(name)


def env():
    e = {k: v for k, v in os.environ.items() if k not in ("REMUX_SESSION", "REMUX_PANE")}
    e.update(PATH=f"{RUN}/bin:" + os.environ.get("PATH", ""), XDG_RUNTIME_DIR=f"{RUN}/run",
             XDG_STATE_HOME=f"{RUN}/state", XDG_DATA_HOME=f"{RUN}/data", XDG_CONFIG_HOME=f"{RUN}/cfg",
             HOME=f"{RUN}/home", SHELL="/bin/sh", ENV="/dev/null", TERM="xterm-256color", PS1="$ ",
             REMUX_ALLOW_NESTED="1")
    return e


def theme_hex(role):
    needle = f"{role}: ThemeColor::Rgb("
    for line in open("src/config/theme.rs"):
        if needle in line:
            rgb = line.split(needle)[1].split(")")[0]
            return "".join(f"{int(v):02x}" for v in rgb.split(","))
    raise SystemExit(f"no Rgb default for {role}")


# The default `tab_bell_fg` is Indexed(9), which pyte resolves to this hex.
BELL = "ff0000"


def protocol_version():
    for line in open("src/protocol.rs"):
        if "pub const PROTOCOL_VERSION" in line:
            return int(line.split("=")[1].strip().rstrip(";"))
    raise SystemExit("no PROTOCOL_VERSION")


class Wire:
    def __init__(self):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(SOCK)
        self.s.settimeout(3.0)
        self.buf = b""
        self.send({"protocol_version": protocol_version(), "remux_version": "harness"})
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

    def spawn(self, argv=None, placement="SplitBelow"):
        body = {"session": "main", "placement": placement}
        if argv:
            body["argv"] = argv
        self.send({"CliSpawn": body})
        end = time.time() + 5
        while time.time() < end:
            m = self.recv()
            if isinstance(m, dict) and "CliSpawned" in m:
                return m["CliSpawned"]["pane_id"]
        return None

    def to_pane(self, pane, text):
        self.send({"InputToPane": {"pane_id": pane, "data": list(text.encode())}})


def agent_row(screen):
    """The stand-in's detail line, the second line of its row: the command,
    the messaging name and the unread badge are all drawn there."""
    for y in range(FRAME + 1, ROWS - FRAME - 1):
        if screen.display[y][FRAME:SIDEBAR_W - FRAME].lstrip()[:1] in ("\u25cf", "\u25cb"):
            return y + 1, screen.display[y + 1][FRAME:SIDEBAR_W - FRAME].rstrip()
    return None, ""


def wait_row(pump, screen, cond, timeout=1.5):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.15)
        y, text = agent_row(screen)
        if cond(text):
            return y, text
    return agent_row(screen)


def main():
    shutil.rmtree(RUN, ignore_errors=True)
    for d in ("run", "state", "data", "bin", "cfg/remux", "home"):
        os.makedirs(f"{RUN}/{d}", exist_ok=True)
    os.chmod(f"{RUN}/run", 0o700)
    with open(f"{RUN}/cfg/remux/config.toml", "w") as f:
        f.write(CFG)
    with open(f"{RUN}/bin/claude", "w") as f:
        f.write(AGENT)
    os.chmod(f"{RUN}/bin/claude", 0o755)

    server = subprocess.Popen([BIN, "server"], env=env(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        if os.path.exists(SOCK):
            break
        time.sleep(0.05)
    time.sleep(0.3)
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, [], env=env(), dimensions=(ROWS, COLS), encoding=None)

    def pump(t=0.7):
        end = time.time() + t
        while time.time() < end:
            try:
                stream.feed(child.read_nonblocking(65536, 0.1))
            except Exception:
                pass

    try:
        pump(1.5)
        w = Wire()
        a = w.spawn()
        b = w.spawn([f"{RUN}/bin/claude"], placement="SplitRight")
        go, in_file, out_file = (f"{RUN}/mail-{n}" for n in ("go", "in.json", "out.json"))
        with open(in_file, "w") as f:
            json.dump([{"MsgSend": {"to": {"host": None, "target": {"Name": "alpha"}},
                                    "body": f"M:{n}", "deliver": "Mailbox"}} for n in ("one", "two")], f)
        w.spawn(["python3", WIRE_SEND, "--held", go, SOCK, in_file, out_file])
        pump(2.0)

        # --- 3a -----------------------------------------------------------
        y, text = wait_row(pump, screen, lambda t: "@alpha" in t, timeout=5)
        check("3a the detail line names the agent after its command, with no badge",
              "@alpha" in text and "*" not in text, text)

        # --- 3b -----------------------------------------------------------
        pump(2.0)
        with open(go, "w"):
            pass
        y, text = wait_row(pump, screen, lambda t: t.endswith("*2"), timeout=3)
        badge_x = SIDEBAR_W - FRAME - 2
        colour = str(screen.buffer[y][badge_x].fg) if y is not None else None
        check("3b two messages: the row ends *2 in the bell colour, unprompted by output",
              text.endswith("*2") and colour == BELL, f"{text!r} {colour}")
        sent = json.load(open(out_file)) if os.path.exists(out_file) else None
        check("3b both sends were accepted",
              sent is not None and [next(iter(m)) for m in sent] == ["MsgSent", "MsgSent"], sent)

        # --- 3c -----------------------------------------------------------
        w.to_pane(b, "read\n")
        y, text = wait_row(pump, screen, lambda t: "*" not in t and "@alpha" in t, timeout=3)
        check("3c the agent reads its mail: the badge is gone", "*" not in text and "@alpha" in text, text)

        # --- 3d -----------------------------------------------------------
        w.to_pane(b, "clear\n")
        y, text = wait_row(pump, screen, lambda t: "@" not in t and "claude" in t, timeout=3)
        check("3d a cleared name: the suffix is gone", "@" not in text and "claude" in text, text)
        w.to_pane(b, "long\n")
        wait_row(pump, screen, lambda t: "@abc" in t, timeout=3)
        w.to_pane(a, f"""'{BIN}' msg send {LONG_NAME} --deliver mailbox "$(printf 'M:%s' three)"\n""")
        y, text = wait_row(pump, screen, lambda t: t.endswith("*1"), timeout=3)
        full = screen.display[y][FRAME:SIDEBAR_W - FRAME] if y is not None else ""
        check("3d a long name is cut, and the badge holds the right edge",
              text.endswith("*1") and full.endswith(" *1") and LONG_NAME not in full and "@abc" in full,
              repr(full))
        w.to_pane(b, "read\n")
        w.to_pane(b, "name\n")
        wait_row(pump, screen, lambda t: "@alpha" in t and "*" not in t, timeout=3)

        # --- 3e -----------------------------------------------------------
        child.send(b"\x1ba")
        pump(1.0)
        content = [r[SIDEBAR_W:] for r in screen.display]
        check("3e the Alt+a switcher's row carries @alpha", any("@alpha" in r for r in content),
              "\n".join(r for r in content if r.strip())[:400])
        child.send(b"\x1b")
        pump(0.5)
        check("3z the client is alive", child.isalive())
    finally:
        try:
            child.close(force=True)
        except Exception:
            pass
        subprocess.run([BIN, "stop"], env=env(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
        server.wait(timeout=10)
    for name in ("server.log", "client.log"):
        path = f"{RUN}/state/remux/{name}"
        body = open(path, errors="replace").read() if os.path.exists(path) else ""
        check(f"3z no panic in {name}", "panicked at" not in body)

    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
