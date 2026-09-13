"""The session manager's Views group: `vr`, `vx`, Enter and `vd`.

Why a real PTY: the manager, the switcher and a View are all client-painted --
the cells are composited from `PaneContent` -- so only a PTY sees the result.

Setup:
  * Tab 1 holds two panes printing RAN:one and RAN:two. The markers are
    ASSEMBLED by the shell (`printf 'RAN:%s\\n' one`), so the typed line never
    contains the string searched for.
  * Alt+t to a second tab printing TAB:two, so the view cells stream live and
    "back to the session" has a marker of its own.
  * Compose View 1 from both panes (`va`), then `w q` out of it.

Both halves of the "is a view on screen" question are exercised: `vr` and
`vx` run with NO view displayed; Enter puts the view on screen; `vd` then runs
from a manager opened OVER the displayed view, which is the case where the
deleted-view resync has to leave to the session underneath an open overlay.

Every row is found by READING the highlight (`sm_goto`), never by counting `j`.
Row scans use `finditer`. Run from repo root:
    python3 tests/pty/session_manager_views.py
"""
import re
import sys
import time

from pty_harness import (
    PROTOCOL_VERSION,
    Tui,
    sm_children,
    sm_compose_view,
    sm_goto,
    sm_open,
    sm_tree,
)

RUNDIR = "/tmp/rmx-smviews"
FAILS = []


def fail(msg):
    print(f"  FAIL: {msg}")
    FAILS.append(msg)


def count(t, needle):
    pat = re.compile(re.escape(needle))
    return sum(len(list(pat.finditer(r))) for r in t.rows_text())


def wait_until(t, pred, timeout=6.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            if pred():
                return True
        except AssertionError:
            pass  # the popup is mid-repaint
        t.pump(0.15)
    try:
        return bool(pred())
    except AssertionError:
        return False


def is_group(row):
    return re.search(r"[▼▶] Views\s*$", row.text) is not None


def is_view(name):
    pat = re.compile(r"[▼▶] " + re.escape(name) + r"\s*$")
    return lambda row: pat.search(row.text) is not None


def tree_rows(t):
    try:
        return sm_tree(t)
    except AssertionError:
        return []


def view_rows(t, name):
    return [r for r in tree_rows(t) if is_view(name)(r)]


def cells_of(t, name):
    rows = view_rows(t, name)
    return sm_children(t, rows[0]) if rows else []


def mark(t, tag, prefix="RAN"):
    t.send(f"clear; printf '{prefix}:%s\\n' {tag}\r", 0.4)
    if not wait_until(t, lambda: count(t, f"{prefix}:{tag}") > 0):
        t.dump(f"mark {tag}")
        t.kill()
        print(f"FAIL(setup): the pane never printed {prefix}:{tag}")
        sys.exit(1)


def close_manager(t):
    t.send(b"\x1b", 0.5)
    if not wait_until(t, lambda: not t.has("Session Manager"), 3.0):
        fail("Esc did not close the session manager")


CLIENT = None


def main():
    global CLIENT
    print(f"protocol {PROTOCOL_VERSION}")
    t = CLIENT = Tui(RUNDIR, cols=140, rows=40).start()

    # --- setup: two marked panes, a second tab, View 1 over both panes -------
    mark(t, "one")
    t.prefix(b"pv", 0.8)
    mark(t, "two")
    t.send(b"\x1bt", 0.8)                      # Alt+t: tab 2
    mark(t, "two", prefix="TAB")
    if count(t, "RAN:one") or count(t, "RAN:two"):
        t.dump("tab 2")
        t.kill()
        print("FAIL(setup): still looking at tab 1")
        sys.exit(1)

    sm_compose_view(t, tab="Tab 1", panes=(0, 1), settle=1.5)
    composed = wait_until(
        t, lambda: count(t, "RAN:one") and count(t, "RAN:two"), 5.0)
    print(f"[setup] View 1 shows both panes: {bool(composed)}")
    if not composed:
        t.dump("composed")
        t.kill()
        print("FAIL(setup): the composed view did not show both panes")
        sys.exit(1)
    t.prefix(b"wq", 1.0)                       # leave the view, keep it
    if not wait_until(t, lambda: count(t, "TAB:two") and not count(t, "RAN:one")):
        t.dump("after w q")
        t.kill()
        print("FAIL(setup): `w q` did not return to the session")
        sys.exit(1)

    # --- the group is in the manager -----------------------------------------
    sm_open(t)
    groups = [r for r in tree_rows(t) if is_group(r)]
    cells = cells_of(t, "View 1")
    print(f"[tree] groups={groups} View 1 cells={cells}")
    if len(groups) != 1:
        fail(f"expected one Views group row, got {groups}")
    if len(cells) != 2:
        fail(f"expected View 1 to list two cells, got {cells}")

    # --- vr: rename from a CELL row (acts on its view) ------------------------
    if cells:
        sm_goto(t, lambda r: r.y == cells[1].y, "View 1's second cell")
        t.send("v", 0.2)
        t.send("r", 0.5)
        prompt = wait_until(t, lambda: t.has("Rename view 'View 1'"), 3.0)
        print(f"[vr] prompt names the view: {prompt}")
        if not prompt:
            t.dump("vr prompt")
            fail("`vr` on a cell row did not open a rename prompt for View 1")
        t.send("renamed", 0.3)
        t.send("\r", 0.8)
    renamed = wait_until(
        t, lambda: view_rows(t, "renamed") and not view_rows(t, "View 1"), 4.0)
    print(f"[vr] manager shows the new name: {bool(renamed)}")
    if not renamed:
        t.dump("after vr")
        fail("the manager did not show View 1 renamed to 'renamed'")
    close_manager(t)

    t.send(b"\x1bs", 0.8)                      # Alt+s: the switcher
    in_switcher = wait_until(t, lambda: t.has(" Views ") and t.has("renamed"), 3.0)
    print(f"[vr] switcher's Views section shows it: {bool(in_switcher)}")
    if not in_switcher:
        t.dump("switcher")
        fail("the switcher's Views section does not show 'renamed'")
    t.send(b"\x1b", 0.5)

    # --- vx: remove the FIRST cell (RAN:one; cells are in mark order) --------
    sm_open(t)
    cells = cells_of(t, "renamed")
    if len(cells) == 2:
        sm_goto(t, lambda r: r.y == cells[0].y, "renamed's first cell")
        t.send("v", 0.2)
        t.send("x", 0.8)
    removed = wait_until(t, lambda: len(cells_of(t, "renamed")) == 1, 4.0)
    print(f"[vx] the view lists one cell: {bool(removed)} -> {cells_of(t, 'renamed')}")
    if not removed:
        t.dump("after vx")
        fail("`vx` did not remove the cell from the view")

    # --- Enter on the remaining cell: the view is painted --------------------
    remaining = cells_of(t, "renamed")
    if remaining:
        sm_goto(t, lambda r: r.y == remaining[-1].y, "renamed's remaining cell")
        t.send("\r", 1.5)
    entered = wait_until(
        t,
        lambda: not t.has("Session Manager") and count(t, "RAN:two") > 0,
        5.0,
    )
    t.pump(0.8)
    one, two, tab = count(t, "RAN:one"), count(t, "RAN:two"), count(t, "TAB:two")
    print(f"[enter] view on screen: {bool(entered)} RAN:one={one} RAN:two={two} "
          f"TAB:two={tab} alive={t.alive()}")
    if not entered:
        t.dump("after Enter")
        fail("Enter on the cell row did not paint the view")
    if one:
        t.dump("removed cell still shown")
        fail("the removed cell's RAN:one is still in the view")
    if tab:
        fail("the session's TAB:two is still on screen, not the view")
    if not t.alive():
        fail("client exited after Enter")

    # --- vd + y from a manager opened OVER the displayed view ----------------
    sm_open(t)
    rows = view_rows(t, "renamed")
    if rows:
        sm_goto(t, is_view("renamed"), "the renamed view row")
        t.send("v", 0.2)
        t.send("d", 0.5)
        confirm = wait_until(t, lambda: t.has("Delete view 'renamed'? (y/n)"), 3.0)
        print(f"[vd] confirmation names the view: {confirm}")
        if not confirm:
            t.dump("vd prompt")
            fail("`vd` did not ask to confirm deleting 'renamed'")
        t.send("y", 1.0)
    else:
        fail(f"no 'renamed' view row to delete: {tree_rows(t)}")
    group_gone = wait_until(
        t,
        lambda: t.has("Session Manager") and not any(is_group(r) for r in sm_tree(t)),
        5.0,
    )
    print(f"[vd] the Views group is gone from the open manager: {bool(group_gone)}")
    if not group_gone:
        t.dump("after vd")
        fail("the Views group is still in the manager after `vd` + `y`")
    close_manager(t)
    back = wait_until(t, lambda: count(t, "TAB:two") and not count(t, "RAN:two"), 5.0)
    print(f"[vd] back on the session: {bool(back)}")
    if not back:
        t.dump("after vd close")
        fail("the screen did not return to the session after deleting the view")

    t.send(b"\x1bs", 0.8)
    still_listed = wait_until(t, lambda: t.has("renamed"), 1.5)
    t.send(b"\x1b", 0.5)
    if still_listed:
        fail("the switcher still lists the deleted view")

    alive = t.alive()
    logs = t.log("client") + t.log("server")
    panic = "panicked at" in logs
    t.kill()
    print(f"alive={alive} panic={panic}")
    if not alive:
        fail("client exited")
    if panic:
        fail("panic in a log")

    if FAILS:
        print(f"RESULT: FAIL -> {FAILS}")
        sys.exit(1)
    print("RESULT: PASS (session manager Views group: vr, vx, Enter, vd)")


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # a step raised (e.g. the client died mid-run)
        # Still report liveness and the panic grep: a crash is exactly when
        # the log is the evidence.
        t = CLIENT
        alive = t is not None and t.alive()
        logs = (t.log("client") + t.log("server")) if t is not None else ""
        panic = "panicked at" in logs
        if t is not None:
            t.kill()
        print(f"  FAIL: scenario aborted: {e!r}")
        print(f"alive={alive} panic={panic}")
        print("RESULT: FAIL -> scenario aborted")
        sys.exit(1)
