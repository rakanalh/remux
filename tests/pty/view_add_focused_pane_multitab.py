"""`Prefix+w a` must add the pane the user is ON, not the first tab's pane.

The reported bug: in a session with several tabs, `w a` added the FIRST tab's
focused pane. `PaneTreeEntry::is_focused` is per-tab -- every tab marks one --
and the client walked the tabs in order taking the first `is_focused` it met,
ignoring `TabTreeEntry::is_active`. Resolution now lives in `view_add_target`
(src/main.rs).

Why a real PTY: the resolution runs in the CLIENT, off a `SessionTree` reply to
a `ListSessionTree` the `w a` handler sends. The frame harness never runs that
code.

Setup, chosen so every wrong answer is distinguishable on screen:
  * Tab 1, its only pane prints RAN:alpha   <- what the bug picked
  * Tab 2, first pane prints RAN:gamma       <- first pane of the right tab
  * Tab 2, split -> second pane (focused) prints RAN:beta  <- the right answer
Each marker is ASSEMBLED by the shell (`printf 'RAN:%s\\n' beta`), so the typed
command line never contains the string searched for.

Then `w a` -> the picker -> Enter ("New view"). The new view must show
RAN:beta, and neither RAN:alpha nor RAN:gamma. Row scans use `finditer`, since
two cells can share a screen row.

Run from repo root:  python3 tests/pty/view_add_focused_pane_multitab.py
"""
import re
import sys
import time

from pty_harness import Tui

RUNDIR = "/tmp/rmx-vaddmt"


def wait_for(t, needle, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        if t.has(needle):
            return True
        t.pump(0.15)
    return False


def count(t, needle):
    """Occurrences of `needle` across every row -- all of them, not the first."""
    pat = re.compile(re.escape(needle))
    return sum(len(list(pat.finditer(r))) for r in t.rows_text())


def mark(t, tag):
    t.send(f"clear; printf 'RAN:%s\\n' {tag}\r", 0.4)
    if not wait_for(t, f"RAN:{tag}"):
        t.dump(f"mark {tag}")
        t.kill()
        print(f"FAIL(setup): the pane never printed RAN:{tag}")
        sys.exit(1)


def main():
    t = Tui(RUNDIR, cols=120, rows=40).start()

    mark(t, "alpha")                         # tab 1
    t.send(b"\x1bt", 0.8)                    # Alt+t: tab 2
    mark(t, "gamma")                         # tab 2, first pane
    t.prefix(b"pv", 0.8)                     # split -> second pane, focused
    mark(t, "beta")
    # Sanity: we are on tab 2 with both of its panes visible, not on tab 1.
    if count(t, "RAN:alpha") or not count(t, "RAN:gamma"):
        t.dump("setup")
        t.kill()
        print("FAIL(setup): not looking at tab 2 with its two panes")
        sys.exit(1)

    t.prefix(b"wa", 0.8)                     # add the focused pane to a view
    if not wait_for(t, "Add Pane to View", 4.0):
        t.dump("picker")
        t.kill()
        print("FAIL(setup): the view picker never opened")
        sys.exit(1)
    t.send("\r", 1.5)                        # "New view" -> create + enter
    in_view = wait_for(t, "View 1", 4.0)
    got_beta = wait_for(t, "RAN:beta", 5.0)
    t.pump(1.0)                              # let any wrong cell paint too
    alpha, gamma, beta = count(t, "RAN:alpha"), count(t, "RAN:gamma"), count(t, "RAN:beta")
    t.dump("view")

    alive = t.alive()
    logs = (t.log("client") + t.log("server")).lower()
    panic = "panicked at" in logs
    t.kill()

    print(f"in_view={in_view} beta={beta} alpha={alpha} gamma={gamma} "
          f"alive={alive} panic={panic}")
    ok = True
    if not in_view:
        print("FAIL: no view was created/entered")
        ok = False
    if not (got_beta and beta):
        print("FAIL: the focused pane (RAN:beta) is not in the view")
        ok = False
    if alpha:
        print("FAIL: tab 1's pane (RAN:alpha) was added -- the reported bug")
        ok = False
    if gamma:
        print("FAIL: tab 2's unfocused first pane (RAN:gamma) was added")
        ok = False
    if not alive:
        print("FAIL: client exited")
        ok = False
    if panic:
        print("FAIL: panic in a log")
        ok = False
    if ok:
        print("PASS: w a adds the active tab's focused pane")
        sys.exit(0)
    sys.exit(1)


if __name__ == "__main__":
    main()
