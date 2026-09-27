#!/usr/bin/env python3
"""OSC titles on the client-painted surfaces: the agents panel, the agent
switcher and View cells.

A real PTY, because all three are composited by the CLIENT. A stand-in
`claude` (a `#!` script, whose `comm` is its basename) waits, then titles
itself the way Claude Code does, `✳ <task>`, so the settle window, the glyph
strip and the live push are all exercised.

  1  before the title, the panel row is today's `claude main/0`
  2  after it, the row is the title alone, with the `✳` stripped, and the pane
     border shows the same name
  3  the Alt+a switcher shows the same label
  4  a View cell over the agent pane is labelled with the title, followed by its
     location
  5  with `[appearance] pane_title = "{command}: {title}"` the panel row and the
     pane border both render the template

Run from the repo root:  python3 tests/pty/osc_titles.py
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui, sm_compose_view  # noqa: E402

RUN = "/tmp/rmx-osctp"
BINDIR = "/tmp/rmx-osctp-bin"
SIDEBAR_W = 34
ALT_A = b"\x1ba"
TITLE = "Fix the bug"

AGENT = """#!/bin/sh
printf 'agent %s\\n' "$(printf '%s' re ady)"
sleep 2
printf '\\033]0;\\342\\234\\263 %s%s\\007' 'Fix' ' the bug'
while read line; do :; done
"""

SIDEBAR = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "agents"
"""

FAILS = []


def check(name, cond, t=None):
    print(("  PASS  " if cond else "  FAIL  ") + name)
    if not cond:
        FAILS.append(name)
        if t is not None:
            t.dump(name)


def sidebar_text(t):
    return [r[:SIDEBAR_W] for r in t.rows_text()]


def content_text(t):
    return [r[SIDEBAR_W:] for r in t.rows_text()]


def in_sidebar(t, needle):
    return any(needle in r for r in sidebar_text(t))


def in_content(t, needle):
    return any(needle in r for r in content_text(t))


def write_agent():
    os.makedirs(BINDIR, exist_ok=True)
    p = f"{BINDIR}/claude"
    with open(p, "w") as f:
        f.write(AGENT)
    os.chmod(p, 0o755)


def start(config, rundir):
    env = {"PATH": f"{BINDIR}:" + os.environ.get("PATH", "")}
    return Tui(rundir, cols=130, rows=36, config=config, extra_env=env).start()


def wait_for(t, pred, timeout):
    import time
    end = time.time() + timeout
    while time.time() < end:
        t.pump(0.2)
        if pred():
            return True
    return False


def run_default():
    t = start(SIDEBAR, RUN)
    try:
        t.send("claude\r", 1.2)
        check("1 the untitled row is `claude main/0`",
              wait_for(t, lambda: in_sidebar(t, "claude main/0"), 2.0), t)
        check("2 the row becomes the title",
              wait_for(t, lambda: in_sidebar(t, TITLE), 5.0), t)
        check("2 the row is the title alone", not in_sidebar(t, "claude main/0"), t)
        check("2 the spinner glyph is stripped", not in_sidebar(t, "✳"), t)
        check("2 the pane border shows the title", in_content(t, TITLE), t)

        t.send(ALT_A, 0.8)
        check("3 the switcher shows the title",
              any("Switch Agent" in r for r in t.rows_text())
              and any(f"● {TITLE}" in r for r in content_text(t)), t)
        t.send(b"\x1b", 0.6)

        t.prefix(b"pv", 1.0)
        sm_compose_view(t, panes=(0, 1), settle=1.5)
        t.pump(1.0)
        check("4 a View cell is labelled with the title and its location",
              in_content(t, f"{TITLE} · main / Tab 1"), t)
        check("the client is alive", t.alive())
        log = t.log("server") + t.log("client")
        check("no panic in the logs", "panicked" not in log)
    finally:
        t.kill()


def run_template():
    t = start(SIDEBAR + '\n[appearance]\npane_title = "{command}: {title}"\n', RUN + "t")
    try:
        t.send("claude\r", 1.2)
        want = f"claude: {TITLE}"
        check("5 the template labels the agents row",
              wait_for(t, lambda: in_sidebar(t, want), 5.0), t)
        check("5 and the pane border", in_content(t, want), t)
        check("no panic in the logs",
              "panicked" not in t.log("server") + t.log("client"))
    finally:
        t.kill()


def main():
    write_agent()
    run_default()
    run_template()
    print(f"\n{'FAILED' if FAILS else 'OK'}: {len(FAILS)} failure(s)")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()
