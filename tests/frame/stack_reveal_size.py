"""A pane revealed in its stack runs at the size it is painted at.

A stack paints only its active pane, and the PTY sizing used to follow the same
rects, so a hidden pane kept whatever size it had when it was last visible.
Revealing it (stack step, chip click, focus by id, a zoom following focus) moved
the pane on screen without a resize: the program inside kept wrapping at its old
width until an unrelated resize came along.

Every size here is read from INSIDE the pane, by `stty size`, never from the
frame. The reference is a pane that was painted at the stack's size all along.

  * Monocle: a pane that stays hidden measures itself from the background (the
    harness reads the file it writes) and is already at the stack's size;
  * Monocle from BSP (each pane narrow first): `PaneStackPrev` reveals a pane;
  * Monocle: a click on a strip chip reveals a pane;
  * Monocle: `SessionSwitchPane` reveals a pane (focus by id);
  * BSP zoom: `PaneFocusLeft` while zoomed carries the zoom to the other pane,
    which must now run at the full zoomed size.

`printf 'SZ:%s\\n' "$(stty size)"` is typed and `SZ:<rows> <cols>` searched for,
so the typed line (which reads `SZ:%s`) can never satisfy the search. Row scans
use `finditer`.

Run: python3 tests/frame/stack_reveal_size.py
"""
import os
import re
import sys
import time
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxfs-reveal"
COLS, ROWS = 100, 30

fails = []


def check(cond, msg):
    if cond:
        print(f"  PASS {msg}")
    else:
        print(f"  FAIL {msg}")
        fails.append(msg)


class Grid:
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    if y < self.rows and x < self.cols:
                        self.g[y][x] = cell.get("c", " ") if isinstance(cell, dict) else " "
        elif n == "RenderDiff":
            for ch in body["changes"]:
                y, x = ch["y"], ch["x"]
                if y < self.rows and x < self.cols:
                    c = ch["cell"]
                    self.g[y][x] = c.get("c", " ") if isinstance(c, dict) else " "

    def text(self):
        return ["".join(r) for r in self.g]

    def find(self, pattern):
        rx = re.compile(pattern)
        return [(y, m) for y, row in enumerate(self.text()) for m in rx.finditer(row)]


class TrackingClient(Client):
    """Applies every frame it drains to one grid.

    The grid is never refreshed with a re-`Resize`, as other harnesses do: a
    `Resize` runs `resize_session_panes`, which would itself give the revealed
    pane its right size and hide the bug under test."""

    def __init__(self, sock):
        super().__init__(sock)
        self.grid = Grid(COLS, ROWS)

    def drain(self, t=0.6):
        out = super().drain(t)
        for m in out:
            self.grid.apply(m)
        return out


def snapshot(cli):
    cli.drain(0.6)
    return cli.grid


def cmd(cli, c, t=0.4):
    cli.send({"Command": c})
    cli.drain(t)


def type_line(cli, line):
    cli.send({"Input": {"data": list((line + "\n").encode())}})
    cli.drain(0.7)


def open_session(cli, name):
    cli.send({"CreateSession": {"name": name, "folder": None}})
    cli.send({"Attach": {"session_name": name}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)


def stty_size(cli):
    """The focused pane's own `stty size`, as (rows, cols), or None."""
    type_line(cli, "clear; printf 'SZ:%s\\n' \"$(stty size)\"")
    time.sleep(0.3)
    hits = snapshot(cli).find(r"SZ:(\d+) (\d+)")
    if not hits:
        return None
    _, m = hits[-1]
    return int(m.group(1)), int(m.group(2))


def tree_ids(cli, session):
    cli.send("ListSessionTree")
    for m in cli.drain(0.6):
        body = only(m, "SessionTree")
        if body is None:
            continue
        sessions = list(body["unfiled"])
        for f in body["folders"]:
            sessions += f["sessions"]
        for s in sessions:
            if s["name"] == session:
                return sorted(p["id"] for p in s["tabs"][0]["panes"])
    return []


def monocle_from_bsp(cli, name):
    """Three BSP panes (each laid out narrow), then Monocle on the last one.
    Returns the size the always-visible last pane reports."""
    open_session(cli, name)
    cmd(cli, "PaneNew")
    cmd(cli, "PaneNew")
    cmd(cli, "LayoutNext")
    cmd(cli, "LayoutNext")
    return stty_size(cli)


def case_hidden_pane_is_sized(cli):
    print("monocle: a pane that stays HIDDEN already runs at the stack's size")
    open_session(cli, "hide")
    # The first pane measures itself later, from the background, while it sits
    # hidden in the Monocle stack; it is never revealed. The file is read by the
    # harness, so nothing about the painted frame is involved.
    out = f"{RUNDIR}/hidden-size.txt"
    type_line(cli, f"(sleep 4; stty size > {out}) &")
    cmd(cli, "PaneNew")
    cmd(cli, "PaneNew")
    cmd(cli, "LayoutNext")
    cmd(cli, "LayoutNext")
    ref = stty_size(cli)
    deadline = time.time() + 8
    while not os.path.exists(out) and time.time() < deadline:
        time.sleep(0.2)
    time.sleep(0.2)
    got = None
    if os.path.exists(out):
        parts = open(out).read().split()
        if len(parts) == 2:
            got = (int(parts[0]), int(parts[1]))
    print(f"  visible pane {ref}, hidden pane measured itself at {got}")
    check(ref is not None and got == ref, f"hidden pane is {ref} ({got})")


def case_stack_prev(cli):
    print("monocle: PaneStackPrev reveals a pane at the stack's size")
    ref = monocle_from_bsp(cli, "prev")
    print(f"  reference (visible all along): {ref}")
    check(ref is not None, "the reference pane reports a size")
    cmd(cli, "PaneStackPrev")
    got = stty_size(cli)
    print(f"  revealed by PaneStackPrev: {got}")
    check(ref is not None and got == ref, f"revealed pane is {ref} ({got})")


def case_chip_click(cli):
    print("monocle: a strip-chip click reveals a pane at the stack's size")
    ref = monocle_from_bsp(cli, "click")
    cmd(cli, {"PaneRename": "cc"})
    cmd(cli, "PaneStackPrev")
    cmd(cli, "PaneStackPrev")
    cmd(cli, {"PaneRename": "aa"})
    cmd(cli, "PaneStackNext")
    cmd(cli, "PaneStackNext")
    g = snapshot(cli)
    chip = [(y, m.start()) for y, m in g.find(r"\baa\b") if y == 0]
    check(len(chip) == 1, f"the aa chip is on the strip ({chip})")
    if len(chip) != 1:
        return
    y, x = chip[0]
    cli.send({"MouseClick": {"x": x, "y": y, "pane_id": None, "release": False}})
    cli.send({"MouseClick": {"x": x, "y": y, "pane_id": None, "release": True}})
    cli.drain(0.5)
    got = stty_size(cli)
    print(f"  reference {ref}, revealed by click: {got}")
    check(ref is not None and got == ref, f"clicked pane is {ref} ({got})")


def case_switch_pane(cli):
    print("monocle: SessionSwitchPane reveals a pane at the stack's size")
    ref = monocle_from_bsp(cli, "jump")
    first = tree_ids(cli, "jump")[0]
    cmd(cli, {"SessionSwitchPane": {"session": "jump", "tab_index": 0, "pane_id": first}}, 0.6)
    got = stty_size(cli)
    print(f"  reference {ref}, revealed by SessionSwitchPane: {got}")
    check(ref is not None and got == ref, f"jumped-to pane is {ref} ({got})")


def case_zoom_follows_focus(cli):
    print("bsp zoom: PaneFocusLeft carries the zoom to a pane that must grow")
    open_session(cli, "zoom")
    cmd(cli, "PaneNew")
    half = stty_size(cli)
    cmd(cli, "PaneToggleZoom", 0.6)
    ref = stty_size(cli)
    print(f"  split pane {half}, zoomed pane {ref}")
    check(ref is not None and half is not None and ref[1] > half[1],
          f"the zoom widens the focused pane ({half} -> {ref})")
    cmd(cli, "PaneFocusLeft")
    got = stty_size(cli)
    print(f"  pane the zoom moved to: {got}")
    check(ref is not None and got == ref, f"newly zoomed pane is {ref} ({got})")


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start()
    cli = TrackingClient(srv.sock)
    try:
        cli.hello()
        case_hidden_pane_is_sized(cli)
        case_stack_prev(cli)
        case_chip_click(cli)
        case_switch_pane(cli)
        case_zoom_follows_focus(cli)
        check("panicked" not in srv.log(), "no panic in the server log")
    finally:
        cli.close()
        srv.kill()
    print()
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print(f"  - {f}")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
