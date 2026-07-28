"""Part B (PTY): a View cell whose source pane is "session-visible" (shown in the
client's own active tab) renders the "● Active in session" placeholder, does NOT
route raw text input to the pane, but still obeys view-management shortcuts.

Single client, two panes A and B in the active tab (so BOTH are session-visible),
composed into a 2-cell view WITHOUT backgrounding them.

Checks:
  1. Both cells render the "Active in" placeholder (not the A/B content).
  2. Typing into the focused placeholder cell is NOT delivered to the pane: after
     closing the view, the injected shell command's output is absent.
  3. View-management still works on the placeholder cell: Prefix+f zoom collapses
     to a single visible cell (one "Active in"); Alt+l focus does not crash.
"""
import sys
from pty_harness import Tui

INJECT = "INJECTED_ZZZ_9271"


def count_active(t):
    return sum(r.count("Active in") for r in t.rows_text())


def make_visible_view(t):
    # Two panes in the CURRENT (active) tab -> both session-visible.
    t.send("clear\r", 0.4)
    t.send("printf 'AAAA_marker\\n'\r", 0.5)
    t.prefix(b"pv", 0.6)                 # split vertical -> pane B, focused
    t.send("printf 'BBBB_marker\\n'\r", 0.5)
    # Compose a 2-cell view over both (do NOT background them -> stay visible).
    t.prefix(b"xm", 0.7)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.4)  # expand Tab 1
    t.send("j", 0.2); t.send(" ", 0.3)                    # mark pane 1
    t.send("j", 0.2); t.send(" ", 0.3)                    # mark pane 2
    t.send("v", 0.2); t.send("a", 0.5)                    # AddToView
    t.send("\r", 0.9)                                     # create + enter view


def main():
    t = Tui("/tmp/rmxais/partb_pty", cols=120, rows=40).start()
    make_visible_view(t)
    fails = []

    # 1. Placeholder render: both cells show "Active in", NOT the A/B content.
    t.pump(0.4)
    n = count_active(t)
    print("placeholders visible:", n)
    if n < 2:
        fails.append(f"expected 2 'Active in' placeholders, got {n}")
        t.dump("placeholders")
    if t.has("AAAA_marker") or t.has("BBBB_marker"):
        fails.append("session-visible cell leaked the pane's content instead of the placeholder")

    # 2. Input suppression: type a shell command into the focused placeholder cell.
    t.send(f"echo {INJECT}\r", 0.6)

    # 3. View-management shortcut still acts on the view: Prefix+f zoom collapses
    #    to a single visible cell (one placeholder), even though the focused cell
    #    is a placeholder.
    t.prefix(b"f", 0.7)
    zoomed = count_active(t)
    print("placeholders after zoom:", zoomed)
    if zoomed != 1:
        fails.append(f"Prefix+f zoom did not act on the view (expected 1 placeholder, got {zoomed})")
        t.dump("after zoom")
    t.prefix(b"f", 0.7)                   # unzoom
    if count_active(t) < 2:
        fails.append("unzoom did not restore both placeholders")
    # Alt+l focus must not crash on a placeholder cell.
    t.send(b"\x1bl", 0.4)
    if not t.alive():
        fails.append("client died on Alt+l focus over a placeholder cell")

    # Close the view -> hand the screen back to the real foreground panes.
    t.prefix(b"wq", 0.8)
    t.pump(0.5)

    # The injected command must NOT have run in the pane (input was suppressed).
    if t.has(INJECT):
        fails.append("raw text input WAS delivered to the session-visible pane")
        t.dump("after close")

    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    t.kill()

    print("alive:", alive, "panic:", panic, "fails:", fails)
    if fails or not alive or panic:
        print("FAIL")
        sys.exit(1)
    print("PASS: placeholder renders, input suppressed, view-management shortcuts work")


if __name__ == "__main__":
    main()
