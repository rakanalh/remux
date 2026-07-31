#!/usr/bin/env python3
"""A view cell whose SOURCE PANE dies must say so, and its neighbours must not care.

Finding 3: nothing told the client a pane had died. `disconnected` is only ever
set when the TRANSPORT fails, so a perfectly healthy connection to a server whose
*pane* died never tripped it -- the cell sat on `waiting for …`, or kept painting
its last snapshot as if it were live, forever, and every keystroke typed into it
vanished into `InputToPane`'s missing `else`.

The server now emits `SessionEvent::PaneExited` (a variant that existed in the
wire enum but had no producer anywhere in the tree) and the client turns it into
the terminal `ViewCell::exited` state, drawn as `pane closed`.

Only a real PTY can check this: the cells are client-composited from
`PaneContent`, so what the user actually sees exists nowhere else. This drives
the real client binary, composes a 2-cell view over two real panes, kills ONE
cell's source pane by typing `exit` into it (the same path a user takes), and
asserts:

  * the dead cell reads `pane closed`  -- not an eternal `waiting…`
  * its stale content is GONE          -- frozen content presented as live is
                                          the lie this state exists to prevent
  * the OTHER cell keeps STREAMING     -- proven with fresh output, not a
                                          leftover paint
  * typing into the dead cell is inert and the client stays alive, no panic

Run from the repo root:  python3 tests/pty/view_cell_pane_exited.py
"""
import sys
from pty_harness import Tui

MARK_A = "AAAA_survivor_marker"
MARK_B = "BBBB_doomed_marker"
LIVE = "CCCC_still_streaming"


def locate(t, needle):
    """(row, col) of `needle` on screen, or None."""
    for y, row in enumerate(t.rows_text()):
        x = row.find(needle)
        if x >= 0:
            return (y, x)
    return None


def make_two_panes(t):
    """A tab with 2 panes, each carrying a distinct marker."""
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.7)                                   # split vertical
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_B}\\n'\r", 0.6)


def compose_view(t):
    """Mark both panes in the session manager and alias them into a new view."""
    t.prefix(b"xm", 0.8)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.5)   # expand Tab 1
    t.send("j", 0.2); t.send(" ", 0.3)                     # mark pane 1
    t.send("j", 0.2); t.send(" ", 0.3)                     # mark pane 2
    t.send("v", 0.3); t.send("a", 0.7)                     # AddToView picker
    t.send("\r", 1.5)                                      # "New view" -> enter
    t.pump(0.8)                                            # let PaneContent land


def require_in_view(t):
    """Hard gate: abort unless a VIEW is really on screen (see view_border_parity).

    Without it the whole test can pass against the plain session: killing a pane
    there also removes its content from the screen, so every assertion below
    would be trivially satisfied while never exercising a view cell at all.
    """
    reasons = []
    if t.has("Session Manager"):
        reasons.append("session manager overlay still up")
    if t.has("Add Pane to View"):
        reasons.append("view picker overlay still up")
    status = t.rows_text()[-1]
    if "View 1" not in status:
        reasons.append(f"status bar is not a view status bar: {status.rstrip()!r}")
    if not t.has("/ Tab 1"):
        reasons.append("no view-cell title ('<session> / Tab 1') on any border")
    if t.has("╭ sh"):
        reasons.append("a normal pane border ('╭ sh') is still on screen")
    if reasons:
        print("ABORT: never entered the view -- every assertion below would be "
              "meaningless:")
        for r in reasons:
            print(f"  - {r}")
        t.dump("not in a view")
        t.kill()
        sys.exit(1)


def focus_keys(pos_a, pos_b):
    """(keys towards B's cell, keys towards A's cell) for this cell arrangement.

    Derived from where the two markers actually landed rather than assumed, so
    the test does not silently drive focus the wrong way if the default view
    layout changes. Sent twice: a directional move at the edge is a no-op, so
    two presses always land on the intended cell whichever one starts focused.
    """
    (ya, xa), (yb, xb) = pos_a, pos_b
    if abs(xb - xa) >= abs(yb - ya):
        return (b"\x1bl", b"\x1bh") if xb > xa else (b"\x1bh", b"\x1bl")
    return (b"\x1bj", b"\x1bk") if yb > ya else (b"\x1bk", b"\x1bj")


def main():
    t = Tui("/tmp/rmxvcpe", cols=120, rows=40).start()
    fails = []
    try:
        make_two_panes(t)
        if not (t.has(MARK_A) and t.has(MARK_B)):
            print("ABORT: the two panes were not created")
            t.dump("no two panes")
            t.kill()
            sys.exit(1)

        compose_view(t)
        require_in_view(t)

        pos_a, pos_b = locate(t, MARK_A), locate(t, MARK_B)
        if pos_a is None or pos_b is None:
            print(f"ABORT: the view is not streaming both panes "
                  f"(A={pos_a} B={pos_b})")
            t.dump("view not streaming")
            t.kill()
            sys.exit(1)
        print(f"view: cell A marker at {pos_a}, cell B marker at {pos_b}")
        if t.has("pane closed"):
            fails.append("a healthy cell already reads 'pane closed'")

        to_b, to_a = focus_keys(pos_a, pos_b)
        print(f"focus keys: towards B={to_b!r} towards A={to_a!r}")

        # --- kill cell B's source pane, the way a user would ------------------
        t.send(to_b, 0.4)
        t.send(to_b, 0.5)
        t.send("exit\r", 2.0)
        t.pump(1.5)

        if not t.has("pane closed"):
            fails.append("the dead cell does not read 'pane closed'")
        if t.has("waiting"):
            fails.append("the dead cell fell back to an eternal 'waiting…'")
        if t.has(MARK_B):
            fails.append(f"the dead cell still paints its stale content "
                         f"({MARK_B!r}) as if it were live")
        if not t.has(MARK_A):
            fails.append(f"the SURVIVING cell lost its content ({MARK_A!r}) -- "
                         "one pane's death took out its neighbour")

        # --- typing into a dead cell is inert ---------------------------------
        t.send("echo BOOM_should_go_nowhere\r", 0.8)
        t.pump(0.5)
        if not t.alive():
            fails.append("the client died when typed into after the pane exited")
        if t.has("BOOM_should_go_nowhere") and not t.has("pane closed"):
            fails.append("input into a dead cell was echoed somewhere unexpected")

        # --- the other cell is LIVE, not a frozen paint -----------------------
        t.send(to_a, 0.4)
        t.send(to_a, 0.5)
        t.send(f"printf '{LIVE}\\n'\r", 1.5)
        t.pump(1.0)
        if not t.has(LIVE):
            fails.append(f"the surviving cell stopped streaming: fresh output "
                         f"({LIVE!r}) never arrived")
        if not t.has("pane closed"):
            fails.append("the dead cell stopped saying 'pane closed' "
                         "(a re-subscribe resurrected it)")

        # --- liveness ---------------------------------------------------------
        alive = t.alive()
        logs = (t.log("client") + t.log("server")).lower()
        panic = "panic" in logs
        print(f"alive={alive} panic={panic}")
        if not alive:
            fails.append("client died")
        if panic:
            fails.append("panic in the logs")

        if fails:
            print("\nFAILURES:")
            for f in fails:
                print(f"  - {f}")
            t.dump("final")
            print("RESULT: FAIL")
            sys.exit(1)
        t.dump("final")
        print("\nRESULT: PASS -- a view cell whose source pane died reads "
              "'pane closed' (no stale content, no eternal waiting) while its "
              "neighbour keeps streaming, and the client survives input into it")
    finally:
        t.kill()


if __name__ == "__main__":
    main()
