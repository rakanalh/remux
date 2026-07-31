"""tmux border style + zoom on a stacked pane: the PTY must fill the painted area.

`active_tab_content_sizes` (which drives the PTY/screen size) used to compute the
zoom rect from the zoom substitution but ask the REAL layout tree whether the
pane is in a multi-pane stack. Under tmux style a multi-pane stack reserves its
top row for the tab bar, so the PTY was sized `rect.height - 1` -- while the
render path asks the *zoom-substituted* tree, sees a single-pane stack, draws no
tab bar and blits the FULL rect. One row of painted area with no screen behind
it: a dead bottom row.

Measured two ways, with `border_style = "tmux_style"` in the isolated config:

  1. `stty size` inside the pane reports the PTY's real row count; while zoomed
     it must equal the painted content height (`rows - 1` for the status bar).
  2. After filling the screen with output, the LAST painted row of the pane must
     carry content -- a short screen leaves it blank in the freshly composited
     frame.

The unzoomed stacked pane is measured first as the control: there the tab bar is
real, so one row less is CORRECT.

Run: python3 tests/frame/tmux_zoom_stack_rows.py
"""
import re
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxtzs"
COLS, ROWS = 100, 30
CONTENT_ROWS = ROWS - 1  # the status bar owns the bottom row

CONFIG = """
[appearance]
border_style = "tmux_style"
"""

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

    def _put(self, y, x, cell):
        if 0 <= y < self.rows and 0 <= x < self.cols:
            self.g[y][x] = cell.get("c", " ") if isinstance(cell, dict) else " "

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

    def row(self, y):
        return "".join(self.g[y]).rstrip()

    def rows_text(self):
        return [self.row(y) for y in range(self.rows)]


def snapshot(cli):
    g = Grid(COLS, ROWS)
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in cli.drain(0.7):
        g.apply(m)
    return g


def type_line(cli, text):
    cli.send({"Input": {"data": list(text.encode()) + [0x0A]}})
    cli.drain(0.8)


def pty_rows(cli):
    """`stty size` -> the focused pane's real PTY row count."""
    type_line(cli, "stty size")
    g = snapshot(cli)
    sizes = [r for r in g.rows_text() if re.fullmatch(r"\d+ \d+", r.strip())]
    if not sizes:
        return None, g
    return int(sizes[-1].split()[0]), g


def main():
    srv = Server(RUNDIR).start(CONFIG)
    cli = Client(srv.sock)
    try:
        cli.hello()
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
        cli.drain(0.8)

        g = snapshot(cli)
        check("╭" not in "".join(g.rows_text()),
              "tmux_style config took effect (no zellij boxes)")

        # Two panes in one stack; the focused pane is the new one.
        cli.send({"Command": "PaneStackAdd"})
        cli.drain(0.8)

        # -- control: unzoomed stack really does own a tab-bar row ----------
        n, _ = pty_rows(cli)
        print(f"  unzoomed stacked pane: stty rows={n} (painted {CONTENT_ROWS})")
        check(n == CONTENT_ROWS - 1,
              f"unzoomed stacked pane loses exactly the tab-bar row ({n})")

        # -- zoomed: no tab bar is drawn, so no row may be withheld ---------
        cli.send({"Command": "PaneToggleZoom"})
        cli.drain(0.8)
        n, g = pty_rows(cli)
        print(f"  zoomed stacked pane:   stty rows={n} (painted {CONTENT_ROWS})")
        check("Z" in g.row(ROWS - 1), "the status bar shows the zoom flag")
        check(n == CONTENT_ROWS,
              f"zoomed pane's PTY fills the painted area: {n} == {CONTENT_ROWS}")

        # -- and the painted bottom row is really alive ---------------------
        type_line(cli, "i=1; while [ $i -le 40 ]; do echo LINE$i; i=$((i+1)); done")
        g = snapshot(cli)
        last = g.row(CONTENT_ROWS - 1)
        print(f"  bottom painted row (y={CONTENT_ROWS - 1}): {last!r}")
        check("LINE40" in "".join(g.rows_text()), "the fill output reached the screen")
        check(last.strip() != "", "the last painted row of the zoomed pane is not dead")

        log = srv.log()
        check("panicked" not in log, "no panic in server log")
    finally:
        cli.close()
        srv.kill()

    print()
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
