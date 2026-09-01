"""Bracketed paste (Ctrl+Shift+V) must reach the pane the user is looking at,
in a View as well as in a normal tab.

A client showing a View is DETACHED (`enter_view` sends `Detach`), so the server
has no foreground session for it. The paste path used to call
`send_foreground(Input { .. })` unconditionally, so in a View the pasted text
went nowhere. Keys already route by identity (`InputToPane` to the focused
cell); paste must do the same.

Two assertions, both needed:
  * NORMAL TAB (the regression gate): a paste still reaches the foreground pane.
  * VIEW: the same paste reaches the focused cell's aliased pane.

The pane is `/bin/sh` in canonical mode, so the pasted bytes are echoed back by
the line discipline -- the marker shows up on the pane's input line (wrapped in
the echoed `^[[200~` / `^[[201~` markers) without ever being executed.
"""
import sys
from pty_harness import Tui, sm_compose_view

A = "AAAA_marker_one"
B = "BBBB_marker_two"
NORMAL_MARK = "PASTEDNORM"
VIEW_MARK = "PASTEDVIEW"
RUNDIR = "/tmp/rmxfix/paste"


def paste(t, text, wait=0.7):
    """Send a real bracketed-paste sequence, as a terminal does on Ctrl+Shift+V."""
    t.send(b"\x1b[200~" + text.encode() + b"\x1b[201~", wait)


def make_view(t):
    """Two panes in a BACKGROUND tab, composed into a view (mirrors issue6)."""
    t.send("clear\r", 0.4)
    t.send(f"printf '{A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.6)
    t.send(f"printf '{B}\\n'\r", 0.5)
    # Background tab so A/B are not "session-visible" (else the cells show the
    # "● Active in session" placeholder instead of the live panes).
    t.send(b"\x1bt", 0.6)   # Alt+t: new empty tab
    sm_compose_view(t, panes=(0, 1), settle=0.9)


def main():
    t = Tui(RUNDIR, cols=120, rows=40).start()

    # --- 1. normal tab: the regression gate -------------------------------
    t.send("clear\r", 0.5)
    paste(t, NORMAL_MARK)
    normal_ok = t.has(NORMAL_MARK)
    print(f"normal tab: pasted text visible = {normal_ok}")
    if not normal_ok:
        t.dump("normal tab paste")
    t.send(b"\x15", 0.3)    # Ctrl-U: wipe the pasted line, never execute it

    # --- 2. view: the bug --------------------------------------------------
    make_view(t)
    if not t.has("View 1"):
        print("FAIL(setup): not in a view"); t.dump("setup"); t.kill(); sys.exit(1)
    if not (t.has(A) or t.has(B)):
        print("FAIL(setup): view cells show no pane content")
        t.dump("setup"); t.kill(); sys.exit(1)

    paste(t, VIEW_MARK)
    view_ok = t.has(VIEW_MARK)
    print(f"view: pasted text visible = {view_ok}")
    if not view_ok:
        t.dump("view paste")

    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    t.kill()

    print(f"normal_ok={normal_ok} view_ok={view_ok} alive={alive} panic={panic}")
    if normal_ok and view_ok and alive and not panic:
        print("PASS: paste reaches the focused pane in a normal tab AND in a view")
        sys.exit(0)
    print("FAIL: paste did not reach the pane the user is looking at")
    sys.exit(1)


if __name__ == "__main__":
    main()
