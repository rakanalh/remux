"""Session manager `va` on a TAB row adds every pane of that tab to the view.

Before, `take_marked_or_highlighted_panes` matched only `NodeType::Pane`, so a
tab row yielded nothing, `AddToView` returned `None`, and the picker never
opened.

Why a real PTY: the session manager and the view picker are client overlays,
and the view's cells are client-composited from `PaneContent`.

Setup:
  * Tab 1 holds three panes printing RAN:one, RAN:two, RAN:three (assembled by
    the shell, so the typed line never contains the marker).
  * Alt+t to a second tab, so those panes are not session-visible and the view
    cells stream them live.
  * Open the manager and move onto the `Tab 1` row by READING the highlight
    (`sm_goto`), never by counting `j`. The tab is left as the manager shows it
    -- not expanded by us -- and no pane is marked.
  * `v a` -> the picker -> Enter ("New view").
All three markers must show in the view. Row scans use `finditer`.

Run from repo root:  python3 tests/pty/view_add_tab_panes.py
"""
import re
import sys
import time

from pty_harness import Tui, sm_goto, sm_open, sm_title

RUNDIR = "/tmp/rmx-vaddtab"
TAGS = ("one", "two", "three")


def wait_for(t, needle, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        if t.has(needle):
            return True
        t.pump(0.15)
    return False


def count(t, needle):
    pat = re.compile(re.escape(needle))
    return sum(len(list(pat.finditer(r))) for r in t.rows_text())


def mark(t, tag):
    t.send(f"clear; printf 'RAN:%s\\n' {tag}\r", 0.4)
    if not wait_for(t, f"RAN:{tag}"):
        t.dump(f"mark {tag}")
        t.kill()
        print(f"FAIL(setup): the pane never printed RAN:{tag}")
        sys.exit(1)


def die(t, msg, label):
    t.dump(label)
    t.kill()
    print(f"FAIL(setup): {msg}")
    sys.exit(1)


def main():
    t = Tui(RUNDIR, cols=140, rows=40).start()

    mark(t, TAGS[0])
    t.prefix(b"pv", 0.8)
    mark(t, TAGS[1])
    t.prefix(b"pv", 0.8)
    mark(t, TAGS[2])
    t.send(b"\x1bt", 0.8)                    # Alt+t: tab 2
    t.send("clear\r", 0.4)
    if any(count(t, f"RAN:{tag}") for tag in TAGS):
        die(t, "still looking at tab 1", "tab 2")

    sm_open(t)
    row = sm_goto(
        t,
        lambda r: re.search(r"\bTab 1\b", r.text) is not None,
        "the Tab 1 row",
    )
    print(f"highlighted: {row!r}")
    if "marked)" in sm_title(t):
        die(t, f"unexpected marks: {sm_title(t).strip()!r}", "manager")

    t.send("v", 0.2)
    t.send("a", 0.8)
    picker = wait_for(t, "Add Pane to View", 3.0)
    if picker:
        t.send("\r", 1.5)                    # "New view" -> create + enter
    in_view = picker and wait_for(t, "View 1", 4.0)
    for tag in TAGS:
        wait_for(t, f"RAN:{tag}", 3.0)
    t.pump(1.0)
    seen = {tag: count(t, f"RAN:{tag}") for tag in TAGS}
    t.dump("view")

    alive = t.alive()
    logs = (t.log("client") + t.log("server")).lower()
    panic = "panicked at" in logs
    t.kill()

    print(f"picker={picker} in_view={in_view} seen={seen} alive={alive} panic={panic}")
    ok = True
    if not picker:
        print("FAIL: `va` on the tab row did not open the view picker (no panes)")
        ok = False
    if not in_view:
        print("FAIL: no view was created/entered")
        ok = False
    missing = [tag for tag, n in seen.items() if not n]
    if missing:
        print(f"FAIL: these panes are not in the view: {missing}")
        ok = False
    if not alive:
        print("FAIL: client exited")
        ok = False
    if panic:
        print("FAIL: panic in a log")
        ok = False
    if ok:
        print("PASS: va on a tab row adds every pane of that tab")
        sys.exit(0)
    sys.exit(1)


if __name__ == "__main__":
    main()
