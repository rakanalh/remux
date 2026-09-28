#!/usr/bin/env python3
"""Frame-level test: a process name can never carry a control character.

A program can set its own `comm` (Linux `prctl(PR_SET_NAME)`), and on macOS
its `argv[0]`, to anything. A pane is named after it, the name is drawn into
the border and sent in the session tree, and the client prints cells
verbatim, so an escape sequence in a name would reach the user's OUTER
terminal and bypass remux's emulator and its OSC 52 policy.

  1. a program naming itself `ESC ]0;PWN BEL` leaves no C0 or C1 control
     character in any frame cell or in any tree name
  2. its name still shows, without the controls (`]0;PWN`), so the check is
     not passing on a pane that was never named after it
  3. a `PaneRename` carrying controls is stored and shown without them

Linux only: macOS has no `prctl(PR_SET_NAME)`.

Run: python3 tests/frame/process_name_controls.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of, only  # noqa: E402

RUNDIR = f"/tmp/rmx-pnc-{os.getpid()}"
COLS, ROWS = 100, 30
FAILS = []


def check(cond, msg):
    print(("PASS " if cond else "FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


def is_control(ch):
    o = ord(ch)
    return o < 0x20 or 0x7F <= o < 0xA0


def frame_and_tree(c):
    """Every cell char of a fresh full frame, and every tree pane name."""
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cells = []
    for m in c.drain(1.0):
        if name_of(m) == "FullRender":
            cells = [cell.get("c", " ") for row in only(m, "FullRender")["cells"] for cell in row]
        elif name_of(m) == "RenderDiff":
            cells += [ch["cell"].get("c", " ") for ch in only(m, "RenderDiff")["changes"]]
    c.send("ListSessionTree")
    names = []
    for m in c.drain(1.0):
        if name_of(m) == "SessionTree":
            for s in only(m, "SessionTree")["unfiled"]:
                for t in s["tabs"]:
                    names += [p["name"] for p in t["panes"]]
    return cells, names


def main():
    if sys.platform != "linux":
        print("SKIP: needs prctl(PR_SET_NAME)")
        return
    srv = Server(RUNDIR).start()
    try:
        c = Client(srv.sock)
        c.hello()
        c.send({"CreateSession": {"name": "main", "folder": None}})
        c.send({"Attach": {"session_name": "main"}})
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        c.drain(0.8)
        script = f"{RUNDIR}/rename.py"
        with open(script, "w") as f:
            f.write(
                "import ctypes, time\n"
                "ctypes.CDLL(None).prctl(15, bytes([27]) + b']0;PWN' + bytes([7]), 0, 0, 0)\n"
                "time.sleep(8)\n"
            )
        c.send({"Input": {"data": list(f"python3 {script}\r".encode())}})
        time.sleep(2.8)
        cells, names = frame_and_tree(c)
        bad_cells = [repr(ch) for ch in cells if is_control(ch)]
        bad_names = [n for n in names if any(is_control(ch) for ch in n)]
        check(not bad_cells, f"1 no control character in any frame cell ({bad_cells[:5]})")
        check(not bad_names, f"1 no control character in any tree name ({bad_names})")
        check(names == ["]0;PWN"], f"2 the pane is named after the program, controls removed ({names})")

        time.sleep(6)
        c.send({"Command": {"PaneRename": "a\x1b]52;c;SGVsbG8=\x07b"}})
        time.sleep(0.5)
        cells, names = frame_and_tree(c)
        check(not any(is_control(ch) for ch in cells) and names == ["a]52;c;SGVsbG8=b"],
              f"3 a PaneRename is stored and shown without controls ({names})")
        c.close()
        check("panicked" not in srv.log(), "no panic in the server log")
    finally:
        srv.kill()
    print(f"\n{'FAILED' if FAILS else 'OK'}: {len(FAILS)} failure(s)")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()
