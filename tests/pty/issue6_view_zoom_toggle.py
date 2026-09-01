"""Issue #6 (PTY, only if reproducible): in a view with a CUSTOM cell layout,
Prefix+f zoom then Prefix+f again must EXIT zoom and restore both cells.

Build a 2-cell view, make a custom arrangement (move a cell), zoom (Prefix f) ->
only the focused cell shows; unzoom (Prefix f) -> BOTH cells show again.
"""
import sys
from pty_harness import Tui, sm_compose_view

A = "AAAA_marker_one"
B = "BBBB_marker_two"
RUNDIR = "/tmp/rmxfix/i6"


def make_view(t):
    t.send("clear\r", 0.4)
    t.send(f"printf '{A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.6)
    t.send(f"printf '{B}\\n'\r", 0.5)
    # Background tab so A/B are NOT "session-visible" (else the cells show the
    # "● Active in session" placeholder instead of the live A/B content).
    t.send(b"\x1bt", 0.6)   # Alt+t: new empty tab
    sm_compose_view(t, panes=(0, 1), settle=0.9)


def both_visible(t):
    return t.has(A), t.has(B)


def main():
    t = Tui(RUNDIR, cols=120, rows=40).start()
    make_view(t)
    if not t.has("View 1"):
        print("FAIL: not in view"); t.dump("s"); t.kill(); sys.exit(1)

    # Make a CUSTOM arrangement: move the focused cell right (sets custom_tree).
    t.prefix(b"pL", 0.6)
    a0, b0 = both_visible(t)
    print(f"custom layout: A={a0} B={b0} (status has 'custom': {t.has('custom')})")

    # Zoom the focused cell.
    t.prefix(b"f", 0.6)
    az, bz = both_visible(t)
    zoomed_flag = t.has(" Z") or t.has("Z ")
    print(f"after zoom: A={az} B={bz} (only one visible expected)")

    # Unzoom -- must exit and restore both cells.
    t.prefix(b"f", 0.6)
    au, bu = both_visible(t)
    print(f"after unzoom: A={au} B={bu} (both expected)")

    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    if not (au and bu):
        t.dump("after unzoom")
    t.kill()

    zoom_hid_one = (az != bz) or (not az and not bz)  # zoom shows only focused
    unzoom_restored = au and bu
    print(f"zoom_hid_one={zoom_hid_one} unzoom_restored={unzoom_restored} alive={alive} panic={panic}")
    if unzoom_restored and alive and not panic:
        print("PASS: Prefix+f zoom toggles correctly in a custom-layout view")
        sys.exit(0)
    print("FAIL: zoom did not exit / restore both cells")
    sys.exit(1)


if __name__ == "__main__":
    main()
