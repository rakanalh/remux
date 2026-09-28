"""Focus-by-id must move the visible pane with it: `SessionSwitchPane` in Monocle.

Every jump to a pane (session manager, sessions sidebar, agents panel, agent
switcher) reaches the server as `SessionSwitchPane`. In a Monocle tab all panes
share ONE stack, and the stack's `active` index is what gets painted. If focus
moves by id without moving `active`, the tree says pane A is focused, the strip
and the body keep showing pane C, and every keystroke goes to A -- a pane the
user cannot see.

  * Monocle, three panes `aa`/`bb`/`cc` (`cc` showing): `SessionSwitchPane` to
    `aa`, then to `bb`. After each, assert
      (a) the strip's BOLD chip is the switched-to pane,
      (b) that pane's own `ID:` marker is the body on screen, and
      (c) a line typed afterwards (`printf 'RAN:%s' <tag>`) is on screen, and
          `SubscribePane` shows it in THAT pane and in no other.
  * Master (`default_layout = "master"`), three panes by `PaneNew`: the FIRST
    pane holds the master slot -- the widest box, in the centre column, as
    `MasterLayout` documents (`master_pane: None` means "use the first pane").

Every marker is ASSEMBLED by the shell (`printf 'ID:%s' one`, or a prompt set
as `PS1="ID:"'one$ '`), so a typed command line can never satisfy the search,
and every row scan uses `finditer`.

Run: python3 tests/frame/monocle_switch_pane.py
"""
import os
import re
import sys
import time
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxfs-mono"
COLS, ROWS = 100, 30

fails = []


def check(cond, msg):
    if cond:
        print(f"  PASS {msg}")
    else:
        print(f"  FAIL {msg}")
        fails.append(msg)


class Grid:
    """Reconstruct the composited grid (text + bold) from FullRender/RenderDiff."""

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]
        self.b = [[False] * cols for _ in range(rows)]

    def _put(self, y, x, cell):
        if 0 <= y < self.rows and 0 <= x < self.cols:
            if isinstance(cell, dict):
                self.g[y][x] = cell.get("c", " ")
                self.b[y][x] = bool(cell.get("bold", False))
            else:
                self.g[y][x] = " "
                self.b[y][x] = False

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    self._put(y, x, cell)
        elif n == "RenderDiff":
            for ch in body["changes"]:
                self._put(ch["y"], ch["x"], ch["cell"])

    def text(self):
        return ["".join(r) for r in self.g]

    def layout_name(self):
        bar = "".join(self.g[self.rows - 1])
        for cand in ("monocle", "master", "custom", "grid", "bsp"):
            if cand in bar:
                return cand
        return "?"

    def where(self, pattern):
        rx = re.compile(pattern)
        return [(y, m.start(), m.end()) for y, row in enumerate(self.text())
                for m in rx.finditer(row)]

    def chips(self):
        """(label, bold) for every `aa`/`bb`/`cc` chip on a strip row."""
        out = []
        for y, row in enumerate(self.text()):
            if "ID:" in row or "RAN:" in row:
                continue
            for m in re.finditer(r"\b(aa|bb|cc)\b", row):
                out.append((m.group(1), all(self.b[y][x] for x in range(m.start(), m.end()))))
        return out

    def boxes(self):
        """(x, y, w, h) of every complete rounded box."""
        found = []
        for y in range(self.rows - 1):
            for x in range(self.cols):
                if self.g[y][x] != "╭":
                    continue
                x2 = next((c for c in range(x + 1, self.cols) if self.g[y][c] == "╮"), None)
                if x2 is None:
                    continue
                y2 = next((r for r in range(y + 1, self.rows - 1)
                           if self.g[r][x] == "╰" and self.g[r][x2] == "╯"), None)
                if y2 is not None:
                    found.append((x, y, x2 - x + 1, y2 - y + 1))
        return found


def snapshot(cli):
    """A fresh full grid: re-`Resize` to the same size to force a `FullRender`."""
    g = Grid(COLS, ROWS)
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in cli.drain(0.8):
        g.apply(m)
    return g


def cmd(cli, c):
    cli.send({"Command": c})
    cli.drain(0.4)


def type_line(cli, line):
    cli.send({"Input": {"data": list((line + "\n").encode())}})
    cli.drain(0.7)


def open_session(cli, name):
    cli.send({"CreateSession": {"name": name, "folder": None}})
    cli.send({"Attach": {"session_name": name}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)


def tree_panes(cli, session):
    """[(id, name, is_focused)] of the session's first tab, in tree order."""
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
                return [(p["id"], p["name"], p["is_focused"]) for p in s["tabs"][0]["panes"]]
    return []


def pane_text(cli, pane_id):
    """The pane's own screen, from a `PaneContent` subscription."""
    cli.send({"SubscribePane": {"pane_id": pane_id, "cols": 60, "rows": 20}})
    text = ""
    for m in cli.drain(0.8):
        body = only(m, "PaneContent")
        if body and body["pane_id"] == pane_id:
            text = "\n".join("".join(c.get("c", " ") for c in row) for row in body["cells"])
    cli.send({"UnsubscribePane": {"pane_id": pane_id}})
    cli.drain(0.3)
    return text


def count(text, pattern):
    return sum(1 for _ in re.finditer(pattern, text))


def monocle_case(cli):
    print("monocle: SessionSwitchPane moves the strip, the body and the input")
    open_session(cli, "mono")
    cmd(cli, "PaneNew")
    cmd(cli, "PaneNew")
    cmd(cli, "LayoutNext")
    cmd(cli, "LayoutNext")
    # Named only once in Monocle, stepping back from the last pane: a Monocle
    # rebuild starts every stack name afresh. The marker is the PROMPT, not a
    # line of output, because a hidden stack pane is resized when it is shown
    # and the shell's redraw can wipe a one-off line; a prompt is redrawn.
    # The typed `"ID:"'one$ '` never reads `ID:one`, so only the prompt can.
    for label, marker in (("cc", "three"), ("bb", "two"), ("aa", "one")):
        if label != "cc":
            cmd(cli, "PaneStackPrev")
        cmd(cli, {"PaneRename": label})
        type_line(cli, f"PS1=\"ID:\"'{marker}$ '; clear")
    cmd(cli, "PaneStackNext")
    cmd(cli, "PaneStackNext")

    g = snapshot(cli)
    print(f"  setup: layout={g.layout_name()} chips={g.chips()}")
    check(g.layout_name() == "monocle", f"setup is Monocle ({g.layout_name()})")
    check(g.chips() == [("aa", False), ("bb", False), ("cc", True)],
          f"setup strip is aa bb [cc] ({g.chips()})")
    check(len(g.where(r"ID:three")) == 1, "setup shows pane three (cc)")

    panes = tree_panes(cli, "mono")
    ids = {name: pid for pid, name, _ in panes}
    print(f"  tree: {panes}")
    check(set(ids) == {"aa", "bb", "cc"}, f"the tree names the three panes ({panes})")
    if set(ids) != {"aa", "bb", "cc"}:
        return

    for label, marker, tag in (("aa", "one", "zz"), ("bb", "two", "yy")):
        cli.send({"Command": {"SessionSwitchPane": {
            "session": "mono", "tab_index": 0, "pane_id": ids[label]}}})
        cli.drain(0.6)
        g = snapshot(cli)
        focused = [n for _, n, f in tree_panes(cli, "mono") if f]
        bold = [n for n, b in g.chips() if b]
        print(f"  after switch to {label}: tree focus={focused} chips={g.chips()}")
        check(focused == [label], f"the tree marks {label} focused ({focused})")
        check(bold == [label], f"(a) the strip highlights {label} ({bold})")
        shown = [m for m in ("one", "two", "three") if g.where(rf"ID:{m}\b")]
        check(shown == [marker], f"(b) the body is pane {label}'s ID:{marker} ({shown})")

        type_line(cli, f"printf 'RAN:%s\\n' {tag}")
        time.sleep(0.3)
        g = snapshot(cli)
        on_screen = len(g.where(rf"RAN:{tag}\b"))
        check(on_screen == 1, f"(c) RAN:{tag} is on screen after typing ({on_screen})")
        hits = {n: count(pane_text(cli, ids[n]), rf"RAN:{tag}\b") for n in ("aa", "bb", "cc")}
        print(f"  RAN:{tag} per pane: {hits}")
        check(hits[label] == 1 and sum(hits.values()) == 1,
              f"(c) RAN:{tag} landed in {label} and nowhere else ({hits})")


MASTER_CONFIG = '[appearance]\ndefault_layout = "master"\n'


def master_case(cli):
    print("master: default_layout = master gives the FIRST pane the master slot")
    open_session(cli, "mast")
    type_line(cli, "clear; printf 'ID:%s\\n' one")
    cmd(cli, "PaneNew")
    type_line(cli, "clear; printf 'ID:%s\\n' two")

    g = snapshot(cli)
    boxes = sorted(g.boxes())
    print(f"  2 panes: layout={g.layout_name()} boxes={boxes}")
    check(g.layout_name() == "master", f"the tab is Master ({g.layout_name()})")
    widest = max(boxes, key=lambda b: b[2]) if boxes else None
    one = g.where(r"ID:one\b")
    check(len(boxes) == 2 and widest == boxes[0] and one and widest[0] <= one[0][1] < widest[0] + widest[2],
          f"2 panes: pane one is the wide LEFT master ({boxes}, one at {one})")

    cmd(cli, "PaneNew")
    type_line(cli, "clear; printf 'ID:%s\\n' three")
    g = snapshot(cli)
    boxes = sorted(g.boxes())
    print(f"  3 panes: boxes={boxes}")
    widest = max(boxes, key=lambda b: b[2]) if boxes else None
    one = g.where(r"ID:one\b")
    check(len(boxes) == 3 and widest == boxes[1] and one and widest[0] <= one[0][1] < widest[0] + widest[2],
          f"3 panes: pane one is the wide CENTRE master ({boxes}, one at {one})")

    # Five panes, the README screenshot's count: the side columns alternate
    # left/right (2 and 4 left, 3 and 5 right), so the TOP-LEFT pane is pane
    # two. The master is still pane one, in the centre.
    cmd(cli, "PaneNew")
    cmd(cli, "PaneNew")
    # The lowest id, NOT the tree's first entry: the session tree lists panes
    # in layout order, and in Master that starts with the top-left side pane.
    first = min(pid for pid, _, _ in tree_panes(cli, "mast"))
    cli.send({"Command": {"SessionSwitchPane": {
        "session": "mast", "tab_index": 0, "pane_id": first}}})
    cli.drain(0.5)
    type_line(cli, "clear; printf 'ID:%s\\n' one")
    g = snapshot(cli)
    boxes = sorted(g.boxes())
    print(f"  5 panes: boxes={boxes}")
    tall = [b for b in boxes if b[3] == ROWS - 1]
    one = g.where(r"ID:one\b")
    check(len(boxes) == 5 and len(tall) == 1 and one
          and tall[0][0] <= one[0][1] < tall[0][0] + tall[0][2]
          and max(boxes, key=lambda b: b[2]) == tall[0] and tall[0][0] > 0,
          f"5 panes: pane one is the full-height CENTRE master ({boxes}, one at {one})")


def run(config, case):
    srv = Server(RUNDIR).start(config=config)
    cli = Client(srv.sock)
    try:
        cli.hello()
        case(cli)
        log = srv.log()
        check("panicked" not in log, "no panic in the server log")
    finally:
        cli.close()
        srv.kill()


def main():
    os.environ["HOME"] = RUNDIR
    run(None, monocle_case)
    run(MASTER_CONFIG, master_case)
    print()
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print(f"  - {f}")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
