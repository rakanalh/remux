#!/usr/bin/env python3
"""A name carrying an escape sequence never reaches the outer terminal.

Session, tab and view names are whatever a client sent, and the client prints
server cells verbatim. A tab renamed to `ESC ]0;PWN BEL` over the wire, shown
in the status bar the server composites, would retitle the user's OUTER
terminal, and an `OSC 52` in its place would write their clipboard. The client
therefore drops control characters where it prints.

  1  the bytes the client wrote to its terminal never contain the injected
     `ESC ]0;PWN BEL`, nor its OSC 52 twin
  2  the tab name still shows, without the controls, so the check is not
     passing on a status bar that never drew it

Run from the repo root:  python3 tests/pty/name_controls.py
"""
import json
import os
import socket
import struct
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import PROTOCOL_VERSION, Tui  # noqa: E402

RUN = f"/tmp/rmx-nc-{os.getpid()}"
FAILS = []


def check(name, cond, detail=""):
    print(("  PASS  " if cond else "  FAIL  ") + name + ("" if cond else f"\n        {detail}"))
    if not cond:
        FAILS.append(name)


def wire(sock_path, *msgs):
    s = socket.socket(socket.AF_UNIX)
    s.connect(sock_path)
    for m in ({"protocol_version": PROTOCOL_VERSION, "remux_version": "t"},) + msgs:
        b = json.dumps(m).encode()
        s.sendall(struct.pack(">I", len(b)) + b)
    # Let the server act before the connection goes away with its messages.
    s.settimeout(2.0)
    try:
        s.recv(65536)
    except socket.timeout:
        pass
    time.sleep(0.5)
    s.close()


def main():
    t = Tui(RUN, cols=120, rows=30).start()
    try:
        t.pump(1.0)
        before = len(t.raw)
        title = "\x1b]0;PWN\x07"
        clip = "\x1b]52;c;SGVsbG8=\x07"
        wire(f"{RUN}/run/remux.sock",
             {"Command": {"TabRenameByIndex": {"session": "main", "tab_index": 0,
                                               "name": f"T{title}X{clip}Y"}}})
        t.pump(1.5)
        # Force a full repaint so the renamed tab is drawn from scratch.
        t.resize(119, 30)
        t.resize(120, 30)
        written = bytes(t.raw[before:])
        check("1 the injected OSC 0 never reaches the terminal",
              title.encode() not in written, written[-200:])
        check("1 nor the injected OSC 52",
              b"\x1b]52;c;SGVsbG8=" not in written, written[-200:])
        check("2 the tab name is still drawn, controls removed",
              any("]0;PWN" in r for r in t.rows_text()), t.rows_text()[-1])
        check("the client is alive", t.alive())
    finally:
        t.kill()
    print(f"\n{'FAILED' if FAILS else 'OK'}: {len(FAILS)} failure(s)")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()
