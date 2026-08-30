#!/usr/bin/env python3
"""The `browser` sidebar panel: does it navigate, and does Enter really open an
editor on the file?

A real PTY, because the panel is painted by the CLIENT around a server frame:
the frame harness sees the content rect alone and would pass on a panel that
never drew a thing. (`tests/frame/browser_listing.py` covers the server half --
the listing and the editor resolution.)

What it covers:

  0  the panel headers itself with the directory the FOCUSED PANE is in
  1  entries render, directories are marked, hidden ones are absent
  2  `.` reveals the hidden entry and hides it again
  3  `j` moves the selection -- proved by where Enter LANDS, not by reading a
     highlight
  4  `l` descends, `h` goes back up, and `h` lands the selection on the
     directory it just left, so `h` then Enter is a round trip
  5  a directory that cannot be read shows the ERROR rather than an empty panel
  6  the one that matters: Enter on a FILE opens a split RUNNING THE EDITOR on
     that file. The pane count going up is checked too, but on its own it would
     pass on a split running a plain shell
  7  and the keyboard has left the sidebar -- typing reaches the editor, which
     an Enter that opened a pane nobody could type into would fail

The stand-in `$EDITOR` lives in the SERVER's environment and is deliberately
REMOVED from the client's: the editor is resolved server-side on purpose (it has
to exist where the file is), and a harness that exported it on both sides would
pass whichever side resolved it.

Run from the repo root:
    python3 tests/pty/sidebar_browser.py [-v]
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
RUN = "/tmp/rmx-sbb"
FIX = f"{RUN}/fixture"
SOCK = f"{RUN}/run/remux.sock"
VERBOSE = "-v" in sys.argv

COLS, ROWS = 100, 30
SIDEBAR_W = 30
FRAME = 1  # the sidebar's border is drawn INSIDE the bar

CFG = f"""
[keybindings.command]
"Alt-2" = "SidebarFocusLeft"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "browser"
  weight = 1
"""

# The stand-in editor. It prints a marker and the file's BASENAME on separate
# short lines (neither can be broken by the wrap of a narrow split), then ECHOES
# WHAT IT READS -- which is how case 7 can tell that the keyboard reached it.
#
# It BLOCKS rather than exiting: one that printed and exited would leave a dead
# pane before the assertion ran, and the failure would read as a flake.
EDITOR = """#!/bin/sh
printf 'EDITING\\n'
printf 'F=%s\\n' "$(basename "$1")"
while read line; do printf 'GOT[%s]\\n' "$line"; done
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

def base_env():
    return {
        **os.environ,
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


def env_server():
    """The server's environment -- and the ONLY side with an `$EDITOR`."""
    return {**base_env(), "EDITOR": f"{RUN}/bin/stand-in-editor"}


def env_client():
    """The client's environment, with `$EDITOR` explicitly REMOVED.

    Not merely "not set": the resolution is meant to happen on the server, so
    the client must be a place where an editor could not have been found.
    """
    e = base_env()
    e.pop("EDITOR", None)
    return e


def setup_dirs():
    shutil.rmtree(RUN, ignore_errors=True)
    for s in ("run", "state", "data", "bin", "cfg"):
        os.makedirs(f"{RUN}/{s}", exist_ok=True)
    os.makedirs(f"{RUN}/cfg/remux", exist_ok=True)
    with open(f"{RUN}/cfg/remux/config.toml", "w") as f:
        f.write(CFG)
    p = f"{RUN}/bin/stand-in-editor"
    with open(p, "w") as f:
        f.write(EDITOR)
    os.chmod(p, 0o755)

    # The fixture. Visible, in the server's sort order: alpha/ beta/ locked/
    # notes.txt -- with `.hidden` between `locked/` and `notes.txt` once shown.
    for d in ("alpha", "beta", "locked"):
        os.makedirs(f"{FIX}/{d}", exist_ok=True)
    with open(f"{FIX}/alpha/inside.txt", "w") as f:
        f.write("x\n")
    for f_ in ("notes.txt", ".hidden"):
        with open(f"{FIX}/{f_}", "w") as f:
            f.write("x\n")
    os.chmod(f"{FIX}/locked", 0o000)


def start_server():
    # `cwd=FIX` is what puts the client's first pane in the fixture directory,
    # which is what the panel then follows. A `cd` typed into the pane would not
    # do: the session tree is pushed when it is DIRTIED, and a plain `cd`
    # dirties nothing.
    p = subprocess.Popen([BIN, "server"], env=env_server(), cwd=FIX,
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
        subprocess.run([BIN, "stop"], env=env_server(), stdout=subprocess.DEVNULL,
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
# a minimal wire client, used only to COUNT panes
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

    def pane_count(self):
        self.send("ListSessionTree")
        end = time.time() + 4.0
        while time.time() < end:
            msg = self.recv()
            if not (isinstance(msg, dict) and "SessionTree" in msg):
                continue
            body = msg["SessionTree"]
            sessions = list(body["unfiled"]) + [
                s for f in body["folders"] for s in f["sessions"]
            ]
            return sum(len(t["panes"]) for s in sessions for t in s["tabs"])
        raise SystemExit("no SessionTree")

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
    child = pexpect.spawn(BIN, [], env=env_client(), dimensions=(ROWS, COLS),
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


def entry_rows(screen):
    """Just the list rows: everything under the header, up to the first blank."""
    out = []
    for r in panel_rows(screen)[1:]:
        if not r.strip():
            break
        out.append(r)
    return out


def header(screen):
    return panel_rows(screen)[0]


def content_rows(screen):
    return [r[SIDEBAR_W:] for r in screen.display]


def content(screen):
    return "\n".join(content_rows(screen))


def wait_until(pump, cond, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.3)
        if cond():
            return True
    return False


def keys(child, pump, *seq, settle=0.35):
    for k in seq:
        child.send(k if isinstance(k, bytes) else k.encode())
        pump(settle)


# ---------------------------------------------------------------------------
# the scenario
# ---------------------------------------------------------------------------

def scenario():
    start_server()
    wire = Wire(SOCK)
    child, screen, pump = spawn()
    pump(2.5)

    # -- 0/1: the panel is showing the focused pane's directory ---------------
    ok = wait_until(pump, lambda: header(screen).endswith("fixture"))
    check("0 the panel headers itself with the focused pane's directory",
          ok, (header(screen), panel_rows(screen)))

    rows = entry_rows(screen)
    log("panel:", rows)
    check("1 directories are listed and marked, files are not",
          rows[:4] == ["alpha/", "beta/", "locked/", "notes.txt"], rows)
    check("1 hidden entries are hidden by default",
          all(".hidden" not in r for r in rows), rows)

    child.send(b"\x1b2")  # Alt-2: focus the panel
    pump(0.5)

    # -- 2: the hidden toggle ------------------------------------------------
    keys(child, pump, ".")
    shown = entry_rows(screen)
    check("2 `.` reveals the hidden entries",
          shown == ["alpha/", "beta/", "locked/", ".hidden", "notes.txt"], shown)
    keys(child, pump, ".")
    check("2 and `.` again hides them",
          all(".hidden" not in r for r in entry_rows(screen)), entry_rows(screen))

    # -- 3: `j` moves the selection, proved by where Enter lands -------------
    keys(child, pump, "g", "j", "\r")
    ok = wait_until(pump, lambda: header(screen).endswith("/beta"))
    check("3 after `j`, Enter descends into the SECOND entry, not the first",
          ok, (header(screen), entry_rows(screen)))

    # -- 4: `h` goes up AND lands on the directory it left -------------------
    keys(child, pump, "h")
    ok = wait_until(pump, lambda: header(screen).endswith("fixture"))
    check("4 `h` goes back up", ok, header(screen))
    keys(child, pump, "\r")
    ok = wait_until(pump, lambda: header(screen).endswith("/beta"))
    check("4 and lands the selection on the directory it left, so `h` then "
          "Enter is a round trip", ok, (header(screen), entry_rows(screen)))
    keys(child, pump, "h")
    wait_until(pump, lambda: header(screen).endswith("fixture"))

    # `l` on a directory descends too, and shows what is inside it.
    keys(child, pump, "g", "\r")
    ok = wait_until(pump, lambda: entry_rows(screen) == ["inside.txt"])
    check("4 `l`/Enter descends and the new directory's contents appear",
          ok and header(screen).endswith("/alpha"),
          (header(screen), entry_rows(screen)))
    keys(child, pump, "h")
    wait_until(pump, lambda: header(screen).endswith("fixture"))

    # -- 5: an unreadable directory says WHY ---------------------------------
    if os.geteuid() == 0:
        print("  SKIP  5 (running as root: mode 000 is still readable)")
    else:
        keys(child, pump, "g", "j", "j", "\r")
        ok = wait_until(pump, lambda: any("permission denied" in r
                                          for r in panel_rows(screen)))
        check("5 an unreadable directory shows the error, not an empty panel",
              ok, (header(screen), panel_rows(screen)))
        keys(child, pump, "h")
        wait_until(pump, lambda: header(screen).endswith("fixture"))

    # -- 6: THE ONE THAT MATTERS --------------------------------------------
    before = wire.pane_count()
    keys(child, pump, "G")
    check("6 the last visible row is the file", entry_rows(screen)[-1] == "notes.txt",
          entry_rows(screen))
    keys(child, pump, "\r", settle=0.5)
    ok = wait_until(pump, lambda: "EDITING" in content(screen))
    after = wire.pane_count()
    body = content(screen)
    log(body)
    check("6 the split exists", after == before + 1, (before, after))
    check("6 and the new pane is RUNNING THE EDITOR, on the file the user chose",
          ok and "F=notes.txt" in body, repr(body[-600:]))

    # -- 7: and the keyboard went with it ------------------------------------
    keys(child, pump, "zz\r", settle=0.6)
    ok = wait_until(pump, lambda: "GOT[zz]" in content(screen))
    check("7 the keyboard left the sidebar: typing reaches the editor",
          ok, repr(content(screen)[-600:]))

    check("client still alive", child.isalive())
    try:
        child.close(force=True)
    except Exception:
        pass
    wire.close()


def main():
    if not os.path.exists(BIN):
        raise SystemExit(f"{BIN} not found; run `cargo build` first")
    setup_dirs()
    try:
        scenario()
    finally:
        check_no_panic()
        stop_server()
        # Restore the mode so a later `rm -rf` of the fixture can descend it.
        try:
            os.chmod(f"{FIX}/locked", 0o755)
        except Exception:
            pass
        time.sleep(0.3)
    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
