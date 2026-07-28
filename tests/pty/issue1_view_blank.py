"""Issue #1 validation (PTY): a 2-cell view with static content in both cells.
Focus cell A, then B, then back to A. At EVERY step BOTH cells must keep showing
their content (no blank), and the client must stay alive.
"""
import sys
from pty_harness import Tui

A = "AAAA_marker_one"
B = "BBBB_marker_two"


def make_view(t):
    t.send("clear\r", 0.4)
    t.send(f"printf '{A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.6)
    t.send(f"printf '{B}\\n'\r", 0.5)
    t.prefix(b"xm", 0.7)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.4)  # expand Tab 1
    t.send("j", 0.2); t.send(" ", 0.3)                    # mark pane 1
    t.send("j", 0.2); t.send(" ", 0.3)                    # mark pane 2
    t.send("v", 0.2); t.send("a", 0.5)                    # AddToView
    t.send("\r", 0.9)                                     # create + enter view


def both_visible(t):
    return t.has(A), t.has(B)


def main():
    t = Tui("/tmp/rmxfix/i1v", cols=120, rows=40).start()
    make_view(t)

    fails = []

    def check(step):
        t.pump(0.3)
        a, b = both_visible(t)
        print(f"[{step}] A_visible={a} B_visible={b} alive={t.alive()}")
        if not (a and b):
            fails.append(step)
            t.dump(step)

    check("initial (focus A)")
    t.prefix(b"pl", 0.6)   # focus right -> cell B
    check("focus B")
    t.prefix(b"ph", 0.6)   # focus left -> cell A
    check("back to A")
    t.prefix(b"pl", 0.6)   # focus B again
    check("focus B again")

    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    t.kill()

    print("alive:", alive, "panic:", panic, "fails:", fails)
    if fails or not alive or panic:
        print("FAIL")
        sys.exit(1)
    print("PASS: both cells kept content across every focus change")


if __name__ == "__main__":
    main()
