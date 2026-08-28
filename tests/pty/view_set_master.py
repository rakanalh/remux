"""`SetMaster` (Alt+m / `Prefix p m`) must work inside a View, exactly as it does
in a normal tab: switch the layout to `master` AND promote the focused cell into
the master slot.

`handle_view_command` intercepts every structural command while a view is active
and consumes the ones it has no view counterpart for (the client is DETACHED, so
forwarding them would hit the wrong session). `SetMaster` had no counterpart, so
it was silently swallowed.

Both halves are asserted, and the focused cell is deliberately the SECOND one:
promoting the first cell is indistinguishable from `MasterLayout`'s "no master
set -> use panes[0]" fallback, so focusing right first is what makes the test
discriminate. The same sequence is run in a normal TAB as the parity reference.
"""
import sys
from pty_harness import Tui

A = "AAAA_marker_one"
B = "BBBB_marker_two"
RUNDIR = "/tmp/rmxfix/setmaster"


def col_of(t, needle):
    """Leftmost column at which `needle` appears, or -1."""
    for r in t.rows_text():
        i = r.find(needle)
        if i >= 0:
            return i
    return -1


def status(t):
    return t.rows_text()[-1]


def make_panes(t):
    t.send("clear\r", 0.4)
    t.send(f"printf '{A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.6)
    t.send(f"printf '{B}\\n'\r", 0.5)


def make_view(t):
    """Compose the two panes of tab 1 into a view (mirrors issue6)."""
    # Background tab so A/B are not "session-visible" (else the cells show the
    # "● Active in session" placeholder instead of the live panes).
    t.send(b"\x1bt", 0.6)   # Alt+t: new empty tab
    t.prefix(b"xm", 0.7)
    t.send(b"\t", 0.3)      # manager opens on its search bar; Tab -> tree
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.4)
    t.send("j", 0.2); t.send(" ", 0.3)
    t.send("j", 0.2); t.send(" ", 0.3)
    t.send("v", 0.2); t.send("a", 0.5)
    t.send("\r", 0.9)


def check_set_master(t, label):
    """Focus the right-hand pane/cell, SetMaster, report what happened.

    Which marker starts on the right is read off the screen rather than assumed:
    an earlier SetMaster in the same session can have reordered the panes.
    """
    ca, cb = col_of(t, A), col_of(t, B)
    print(f"{label}: before  A@{ca} B@{cb} status={status(t).strip()!r}")
    if ca < 0 or cb < 0 or ca == cb:
        print(f"FAIL(setup): {label} does not show both panes side by side")
        t.dump(f"{label} setup")
        return False
    right, left = (A, B) if ca > cb else (B, A)

    t.send(b"\x1bl", 0.6)   # Alt+l: focus right
    t.send(b"\x1bm", 0.9)   # Alt+m: SetMaster

    cr, cl = col_of(t, right), col_of(t, left)
    st = status(t)
    print(f"{label}: after   focused(was right)@{cr} other@{cl} status={st.strip()!r}")

    is_master = "master" in st
    promoted = 0 <= cr < cl
    if not (is_master and promoted):
        t.dump(f"{label} after SetMaster")
    print(f"{label}: layout_is_master={is_master} focused_became_master={promoted}")
    return is_master and promoted


def main():
    t = Tui(RUNDIR, cols=120, rows=40).start()
    make_panes(t)

    # --- 1. normal tab: the parity reference ------------------------------
    tab_ok = check_set_master(t, "tab")
    # Back to a plain grid-ish split for the view leg: cycling layouts from
    # master lands on the next automatic mode, and the view is composed fresh
    # anyway (a view has its own layout, defaulting to grid).

    # --- 2. view: the bug --------------------------------------------------
    make_view(t)
    if not t.has("View 1"):
        print("FAIL(setup): not in a view"); t.dump("setup"); t.kill(); sys.exit(1)
    view_ok = check_set_master(t, "view")

    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    t.kill()

    print(f"tab_ok={tab_ok} view_ok={view_ok} alive={alive} panic={panic}")
    if tab_ok and view_ok and alive and not panic:
        print("PASS: SetMaster switches to master AND promotes the focused cell, in a view too")
        sys.exit(0)
    print("FAIL: SetMaster did not behave in a view as it does in a tab")
    sys.exit(1)


if __name__ == "__main__":
    main()
