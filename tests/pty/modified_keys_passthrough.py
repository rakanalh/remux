"""Does the client forward modified special keys (Ctrl/Shift/Alt + arrows) to the PTY?

The client re-encodes what crossterm parsed off its own stdin, so this is a
round trip: feed the client the xterm sequence for Ctrl+Right, and the app
inside the pane must receive that same sequence -- not a bare cursor key with
the modifier flattened away.

Alt is checked in its folded form (`ESC [ 1 ; 3 A`), which is what xterm/tmux
send; the client used to emit a doubled-ESC prefix instead.
"""
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui

RUNDIR = "/tmp/rmx-modkeys"

# (label, bytes we feed the client, what `cat -vT` must print in the pane)
CASES = [
    ("Ctrl+Right", b"\x1b[1;5C", "^[[1;5C"),
    ("Shift+Down", b"\x1b[1;2B", "^[[1;2B"),
    ("Alt+Up",     b"\x1b[1;3A", "^[[1;3A"),
    ("Ctrl+PageUp", b"\x1b[5;5~", "^[[5;5~"),
    ("plain Right", b"\x1b[C",    "^[[C"),
]

t = Tui(RUNDIR).start()
t.send("stty raw -echo; cat -vT\r", t=1.0)

results = []
for label, sent, want in CASES:
    before = "".join(t.rows_text()).replace(" ", "")
    t.send(sent, t=0.6)
    after = "".join(t.rows_text()).replace(" ", "")
    # Compare against the pre-keystroke screen: `^[[C` is a substring of
    # `^[[1;5C`, so an earlier case could otherwise satisfy a later one.
    results.append((label, want, after.count(want) > before.count(want)))

t.dump("final")
for label, want, ok in results:
    print(f"{label:12} -> {want!r:12} reached the app: {ok}")
print("alive:", t.alive())
print("panic in log:", "panicked" in t.log("client") + t.log("server"))
t.kill()
sys.exit(0 if all(ok for _, _, ok in results) else 2)
