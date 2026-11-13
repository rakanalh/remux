#!/usr/bin/env python3
"""Clicks from a client with a sidebar land where it draws, while a SMALLER
client shares its session.

With a sidebar shown the client draws the status bar itself, across the
terminal's last row, and withholds the server frame's own bar row. A smaller
client on the same session makes the server composite every frame at the
smallest attached size, so this client is sent frames SHORTER and NARROWER than
its content rect. Two things then have to agree with the frame, not with the
content rect:

  1  a click on the client-drawn bar is sent as the frame's LAST row, which is
     where the server's tab hit regions are. Mapped to the content rect's last
     row instead, it lands below the frame and switches nothing;
  2  a click inside the right pane, as the frame draws it, focuses that pane.
     The column used is left of the divider a content-sized layout would have,
     so a server hit test at this client's own size focuses the left pane.

Focus is read from where a marker typed after the click is printed, and the
marker is assembled by the shell (`printf 'F_%s' HIT`), so the echoed command
line cannot produce it.

Run from the repo root:
    python3 tests/pty/mouse_status_bar_small_peer.py [-v]
"""
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sidebar_sessions as h  # noqa: E402
from rundirs import run_dir  # noqa: E402

h.BIN = os.path.abspath(os.environ.get("REMUX_BIN", h.BIN))
h.RUN = run_dir("rmx-msbsp")
h.SOCK1 = f"{h.RUN}/run/remux.sock"
h.SOCK2 = f"{h.RUN}/run2/remux.sock"

PEER = (60, 18)
# Content x: right of the peer-sized divider (about 30) and left of the one a
# 74-column content rect would have (about 37).
PROBE_X = 34


def sgr_click(child, col, row):
    child.send(f"\x1b[<0;{col + 1};{row + 1}M".encode())
    child.send(f"\x1b[<0;{col + 1};{row + 1}m".encode())


def seed():
    """`main`: tab `alpha` (T_ALPHA), tab `beta` split in two, left focused.
    The seeding connection stays attached at PEER, which is the smaller client."""
    w = h.Wire(h.SOCK1)
    w.send({"CreateSession": {"name": "main", "folder": None}})
    w.send({"Attach": {"session_name": "main"}})
    w.send({"Resize": {"cols": PEER[0], "rows": PEER[1]}})
    w.type("\n")
    w.send({"Command": {"TabRename": "alpha"}})
    w.type("printf 'T_%s\\n' ALPHA\n")
    w.send({"Command": "TabNew"})
    w.type("\n")
    w.send({"Command": {"TabRename": "beta"}})
    w.send({"Command": "PaneSplitVertical"})
    w.type("\n")
    w.send({"Command": "PaneFocusLeft"})
    w.type("printf 'T_%s\\n' BETA\n")
    return w


def content_hits(screen, needle):
    """`(content_x, row)` of every `needle` right of the sidebar."""
    return [(m.start(), y) for y, row in enumerate(h.content_rows(screen))
            for m in re.finditer(re.escape(needle), row)]


def main():
    h.setup_dirs()
    h.start_server(h.SOCK1, h.env_local(f"{h.RUN}/cfg"))
    w = seed()
    child, screen, pump = h.spawn(h.env_local(f"{h.RUN}/cfg"))
    try:
        pump(3.0)
        rows = h.cell_rows(screen)
        h.log("screen:\n" + "\n".join(rows))
        bar = rows[h.ROWS - 1]
        h.check("the client draws the bar on the terminal's last row",
                "alpha" in bar and "beta" in bar, repr(bar))
        h.check("the frame is the peer's size: T_BETA is drawn",
                bool(content_hits(screen, "T_BETA")), "\n".join(h.content_rows(screen)))

        sgr_click(child, h.SIDEBAR_W + PROBE_X, 4)
        pump(0.6)
        child.send(b"printf 'F_%s\\n' HIT\r")
        pump(1.0)
        hits = content_hits(screen, "F_HIT")
        h.log("content:\n" + "\n".join(h.content_rows(screen)))
        h.check("2 a click in the right pane as drawn focuses it",
                bool(hits) and all(x > PEER[0] // 2 for x, _ in hits), f"F_HIT at {hits}")

        at = bar.find("alpha")
        sgr_click(child, at + 1, h.ROWS - 1)
        pump(1.0)
        h.log("content:\n" + "\n".join(h.content_rows(screen)))
        h.check("1 a click on alpha in the client-drawn bar switches to alpha",
                bool(content_hits(screen, "T_ALPHA")) and not content_hits(screen, "F_HIT"),
                "\n".join(h.content_rows(screen)))
        h.check("the client is alive", child.isalive())
        h.check_no_panic(f"{h.RUN}/state")
    finally:
        h.teardown(child)
        w.close()
        subprocess.run([h.BIN, "stop"], env=h.env_local(f"{h.RUN}/cfg"),
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
    print(f"\n{'FAILED' if h.FAILURES else 'OK'}: {len(h.FAILURES)} failure(s)")
    sys.exit(1 if h.FAILURES else 0)


if __name__ == "__main__":
    main()
