"""Phase 2 single-client lifecycle (real PTY), now server-round-tripped.

Exercises the view-management verbs through the shared-view intents on ONE
client: `w n` create+enter an empty view, `w q` leave-to-session, re-enter via
the switcher, SM `va` compose over real panes, `Prefix+f` zoom in AND out,
`Alt+Space` layout cycle, and `w x` eject a cell. Asserts on the actual screen
and that the client stays alive with no panic.

Run from repo root:  PYTHONPATH=tests/pty python3 tests/pty/shared_views_single_lifecycle.py [-v]
"""
import sys
from pty_harness import Tui

VERBOSE = "-v" in sys.argv
A = "AAAA_alpha"
B = "BBBB_bravo"


def main():
    t = Tui("/tmp/rmxlife/v", cols=110, rows=36).start()
    fails = []

    def fail(m):
        print("  FAIL:", m)
        fails.append(m)

    # --- `w n` create + enter an empty view, `w q` leave, switcher re-enter ---
    t.send("clear\r", 0.3)
    t.prefix(b"wn", 0.9)               # new empty view (create + enter)
    t.send("\r", 0.9)                  # confirm default name
    t.pump(0.6)
    if VERBOSE:
        t.dump("empty view")
    empty_ok = t.has("Add panes to this view")
    print("[w n] empty view entered:", empty_ok)
    if not empty_ok:
        fail("w n did not create+enter an empty view")

    t.prefix(b"wq", 0.9)               # leave to session
    t.pump(0.5)
    left_ok = not t.has("Add panes to this view")
    print("[w q] left the view back to a session:", left_ok)
    if not left_ok:
        fail("w q did not leave the view")

    t.send(b"\x1bs", 0.8)              # Alt+s switcher
    if VERBOSE:
        t.dump("switcher")
    listed = t.has("View 1")
    print("[switcher] lists the left-but-not-deleted view:", listed)
    if not listed:
        fail("switcher did not list the view after w q (should persist)")
    t.send("k", 0.3)                   # highlight the view (index 0)
    t.send("\r", 0.9)                  # re-enter
    t.pump(0.6)
    reentered = t.has("Add panes to this view")
    print("[switcher] re-entered the empty view:", reentered)
    if not reentered:
        fail("switcher re-enter did not show the view")

    # `w d` delete-for-everyone WHILE displaying it: exercises the resync's
    # deleted-view branch (leave-to-session + re-attach). After delete the view
    # is gone from the switcher entirely.
    t.prefix(b"wd", 1.0)
    t.pump(0.6)
    if VERBOSE:
        t.dump("after w d")
    left_to_session = not t.has("Add panes to this view")
    t.send(b"\x1bs", 0.7)
    still_listed = t.has("View 1")
    t.send(b"\x1b", 0.3)               # esc the switcher
    print(f"[w d] delete-while-displayed: left_to_session={left_to_session} "
          f"still_listed={still_listed}")
    if not left_to_session:
        fail("w d did not drop the displaying terminal back to a session")
    if still_listed:
        fail("w d did not delete the view for everyone (still in switcher)")

    # --- SM `va` compose over two real backgrounded panes ---
    t.send(f"printf '{A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.7)               # split -> pane 2
    t.send(f"printf '{B}\\n'\r", 0.5)
    t.send(b"\x1bt", 0.7)             # Alt+t: background panes 1 & 2
    t.prefix(b"xm", 0.8)
    # The manager opens with its search bar focused; Tab hands focus to the tree.
    t.send(b"\t", 0.3)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.3)
    t.send("j", 0.2); t.send(" ", 0.3)   # mark pane 1
    t.send("j", 0.2); t.send(" ", 0.3)   # mark pane 2
    t.send("v", 0.2); t.send("a", 0.5)   # AddToView
    t.send("\r", 1.0)                     # create + enter view with 2 cells
    t.pump(0.6)
    if VERBOSE:
        t.dump("2-cell view")
    both = t.has(A) and t.has(B)
    print("[va] composed 2-cell view, both visible:", both)
    if not both:
        fail("va compose did not show both cells")

    # --- Prefix+f zoom in AND out ---
    t.prefix(b"f", 0.8)
    t.pump(0.4)
    za, zb = t.has(A), t.has(B)
    zoom_ok = za != zb
    print(f"[zoom in] only one cell shown: A={za} B={zb}")
    if not zoom_ok:
        fail("zoom did not hide the unfocused cell")
    t.prefix(b"f", 0.8)
    t.pump(0.4)
    unzoom_ok = t.has(A) and t.has(B)
    print("[zoom out] both cells shown again:", unzoom_ok)
    if not unzoom_ok:
        fail("un-zoom did not restore both cells")

    # --- Alt+Space layout cycle ---
    t.send(b"\x1b ", 0.8)
    t.pump(0.4)
    print("[layout] alive after Alt+Space cycle:", t.alive())

    # --- `w x` eject the focused cell (real pane untouched) ---
    t.prefix(b"wx", 0.9)
    t.pump(0.6)
    if VERBOSE:
        t.dump("after eject")
    # One cell remains: exactly one marker still on screen.
    ea, eb = t.has(A), t.has(B)
    eject_ok = ea != eb
    print(f"[w x] one cell ejected, one remains: A={ea} B={eb}")
    if not eject_ok:
        fail("w x did not leave exactly one cell")

    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    t.kill()
    print("alive:", alive, "panic:", panic)
    if not alive:
        fail("client exited")
    if panic:
        fail("panic in a log")

    if fails:
        print("RESULT: FAIL ->", fails)
        sys.exit(1)
    print("RESULT: PASS (single-client view lifecycle over the wire)")


if __name__ == "__main__":
    main()
