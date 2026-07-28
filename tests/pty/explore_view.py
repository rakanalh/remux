"""Exploratory: build a 2-cell view via the TUI and dump screens per step."""
import sys, time
from pty_harness import Tui

t = Tui("/tmp/rmxfix/exp", cols=120, rows=40).start()
t.dump("initial")

# Distinct static content in pane A.
t.send("clear\r", 0.4)
t.send("printf 'AAAA_marker_one\\n'\r", 0.5)
t.dump("after A content")

# Split vertical -> new pane B focused.
t.prefix(b"pv", 0.6)
t.send("printf 'BBBB_marker_two\\n'\r", 0.5)
t.dump("after split + B content")
# Background tab so A/B are not session-visible (cells show content, not the
# "Active in session" placeholder).
t.send(b"\x1bt", 0.6)   # Alt+t: new empty tab

# Open session manager (Ctrl-a x m).
t.prefix(b"xm", 0.7)
t.dump("session manager")

# Expand current session/tab if needed with 'l', then mark panes.
print("alive:", t.alive())
t.kill()
