"""bug4 (PTY): entering a View must DETACH the host client's foreground session so
a cell aliasing a pane from THAT session no longer self-counts as session-visible.
The cell must therefore stream the pane's real CONTENT, not the "Active in
session" placeholder.

This is the corrected-behavior counterpart to partb_placeholder_input.py (which
encoded the pre-fix behavior where a single client's own attachment produced a
false placeholder). Same single-client compose flow; inverted assertions.

Two panes A and B in the active tab, composed into a 2-cell view WITHOUT any
second viewer. Because the sole client detaches on view entry:
  1. NEITHER cell shows an "Active in" placeholder.
  2. Both cells stream their pane content (the A/B markers are visible).
  3. The client stays alive and the logs have no panic.
  4. Closing the view (`w q`) re-attaches and hands the screen back cleanly.
"""
import sys
from pty_harness import Tui

MARK_A = "AAAA_bug4_marker"
MARK_B = "BBBB_bug4_marker"


def count_active(t):
    return sum(r.count("Active in") for r in t.rows_text())


def make_self_view(t):
    # Two panes in the CURRENT (active) tab. Pre-fix these would be
    # session-visible via the client's own attachment; the fix detaches on view
    # entry so they stream content instead.
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.6)                 # split vertical -> pane B, focused
    t.send(f"printf '{MARK_B}\\n'\r", 0.5)
    # Compose a 2-cell view over both (do NOT background them).
    t.prefix(b"xm", 0.7)
    # The manager opens with its search bar focused; Tab hands focus to the tree.
    t.send(b"\t", 0.3)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.4)  # expand Tab 1
    t.send("j", 0.2); t.send(" ", 0.3)                    # mark pane 1
    t.send("j", 0.2); t.send(" ", 0.3)                    # mark pane 2
    t.send("v", 0.2); t.send("a", 0.5)                    # AddToView
    t.send("\r", 0.9)                                     # create + enter view


def main():
    t = Tui("/tmp/rmxbug4pty/self", cols=120, rows=40).start()
    make_self_view(t)
    fails = []

    # Let the streamed PaneContent arrive and paint.
    t.pump(0.6)

    # 1. No "Active in" placeholder anywhere.
    n = count_active(t)
    print("'Active in' placeholders:", n, "(want 0)")
    if n != 0:
        fails.append(f"expected 0 'Active in' placeholders, got {n}")
        t.dump("unexpected placeholder")

    # 2. Both cells stream their pane content.
    has_a, has_b = t.has(MARK_A), t.has(MARK_B)
    print("content visible: A =", has_a, " B =", has_b, "(want both True)")
    if not (has_a and has_b):
        fails.append(f"view did not stream pane content (A={has_a} B={has_b})")
        t.dump("missing content")

    # 3. Client must be alive.
    if not t.alive():
        fails.append("client died while displaying the self-session view")

    # 4. Close the view -> re-attach + hand the screen back to the real panes.
    t.prefix(b"wq", 0.9)
    t.pump(0.6)
    if not t.has(MARK_A) and not t.has(MARK_B):
        fails.append("after closing the view the foreground session did not repaint "
                     "(re-attach on view exit failed -> blank screen)")
        t.dump("after close")

    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    t.kill()

    print("alive:", alive, "panic:", panic, "fails:", fails)
    if fails or not alive or panic:
        print("FAIL")
        sys.exit(1)
    print("PASS: self-session view streams content (no false placeholder); "
          "close re-attaches cleanly")


if __name__ == "__main__":
    main()
