"""Every pane in a stack keeps its capped tab chip ("pill") when names are long.

A capped strip used to size every chip to the LONGEST name and then drop each
chip that ran past the right edge, so with names from a `{command}: {title}`
template:

  * 4 panes in Monocle at 120 columns showed 3 pills, and
  * a 2-pane stack in the left half of a Custom layout showed 1.

The dropped chip was the last one, which right after `PaneNew` or a
`PaneStackInto*` is the FOCUSED pane. The chips now shrink until all fit.

Names are set through OSC 2 titles with `pane_title = "{title}"`, and the
client sends `TabStyle rounded`. Both border styles are checked. Pills are
counted by their cap glyphs on the strip row, which nothing else on that row
draws (the typed `printf` lines are pane content on later rows, or in another
pane's content, and contain no cap glyph). A wide control at 160 columns checks
that a strip with room keeps every name whole.

Run: python3 tests/frame/monocle_pills.py
"""
import os
import shutil
import sys
import time
from harness import Server, Client, name_of, only

RUNDIR = f"/tmp/rmxmpl-{os.getpid()}"
ROWS = 30
LEFT_CAP, RIGHT_CAP, ELLIPSIS = "", "", "…"
TITLES = [
    "zsh: ~/Work/Personal/Remux",
    "claude: fix monocle pills",
    "nvim: compositor.rs",
    "sh: last pane",
]
CONFIG = '[appearance]\nborder_style = "{style}"\npane_title = "{{title}}"\n'

fails = []


def check(cond, msg):
    if cond:
        print(f"  PASS {msg}")
    else:
        print(f"  FAIL {msg}")
        fails.append(msg)


def snapshot(cli, cols):
    g = [[" "] * cols for _ in range(ROWS)]
    cli.send({"Resize": {"cols": cols, "rows": ROWS}})
    for m in cli.drain(0.8):
        n = name_of(m)
        body = only(m, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    if y < ROWS and x < cols:
                        g[y][x] = cell["c"]
        elif n == "RenderDiff":
            for ch in body["changes"]:
                if ch["y"] < ROWS and ch["x"] < cols:
                    g[ch["y"]][ch["x"]] = ch["cell"]["c"]
    return ["".join(r) for r in g]


def cmd(cli, c):
    cli.send({"Command": c})
    cli.drain(0.3)


def set_title(cli, title):
    line = f"printf '\\033]2;%s\\007' '{title}'\n"
    cli.send({"Input": {"data": list(line.encode())}})
    cli.drain(0.3)


def session(style, cols, panes):
    """A server and client with `panes` titled panes in a BSP tab."""
    srv = Server(RUNDIR).start(CONFIG.format(style=style))
    try:
        cli = Client(srv.sock)
        cli.hello()
        cli.send({"TabStyle": {"style": "rounded"}})
        cli.drain(0.3)
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": cols, "rows": ROWS}})
        cli.drain(0.8)
        set_title(cli, TITLES[0])
        for title in TITLES[1:panes]:
            cmd(cli, "PaneNew")
            set_title(cli, title)
        # Titles are used only once they settle (src/server/title.rs).
        time.sleep(1.5)
    except BaseException:
        srv.kill()
        raise
    return srv, cli


def pills(row):
    return row.count(LEFT_CAP), row.count(RIGHT_CAP)


def monocle_case(style, cols, expect_cut):
    print(f"--- {style}: 4 panes in Monocle at {cols} columns")
    srv, cli = session(style, cols, 4)
    try:
        cmd(cli, "LayoutNext")
        cmd(cli, "LayoutNext")
        g = snapshot(cli, cols)
        print(f"  strip:  {g[0]!r}")
        print(f"  status: {g[-1][-20:]!r}")
        check("monocle" in g[-1], "the tab is in Monocle")
        check(pills(g[0]) == (4, 4), f"4 pills on the strip ({pills(g[0])})")
        check("sh: last pane" in g[0],
              "the focused (last) pane's chip is drawn")
        if expect_cut:
            check(ELLIPSIS in g[0], "squeezed names are marked as cut")
        else:
            check(ELLIPSIS not in g[0] and all(t in g[0] for t in TITLES),
                  "a strip with room keeps every name whole")
    finally:
        cli.close()
        srv.kill()
        check("panicked" not in srv.log(), "no panic in server.log")


def custom_stack_case(style, cols):
    print(f"--- {style}: a 2-pane stack in a Custom layout at {cols} columns")
    srv, cli = session(style, cols, 3)
    try:
        # BSP puts pane 2 right of pane 1 and pane 3 below pane 2; stacking the
        # focused pane 3 into its left neighbour leaves a 2-pane stack on the
        # left half and pane 2 alone on the right.
        cmd(cli, "PaneStackIntoLeft")
        g = snapshot(cli, cols)
        left = g[0][: cols // 2]
        print(f"  strip:  {g[0]!r}")
        print(f"  status: {g[-1][-20:]!r}")
        check("custom" in g[-1], "the tab is in Custom")
        check(pills(left) == (2, 2), f"2 pills on the stack's strip ({pills(left)})")
        check(pills(g[0]) == (2, 2), f"no other pill on the row ({pills(g[0])})")
    finally:
        cli.close()
        srv.kill()
        check("panicked" not in srv.log(), "no panic in server.log")


def main():
    os.environ["HOME"] = RUNDIR
    for style in ("zellij_style", "tmux_style"):
        monocle_case(style, 120, expect_cut=True)
        custom_stack_case(style, 120)
    monocle_case("zellij_style", 160, expect_cut=False)
    shutil.rmtree(RUNDIR, ignore_errors=True)
    print()
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print(f"  - {f}")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
