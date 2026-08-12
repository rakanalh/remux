"""Exploratory step 2: expand tab, mark panes, create view."""
import sys, time
from pty_harness import Tui

t = Tui("/tmp/rmxfix/exp2", cols=120, rows=40).start()
t.send("clear\r", 0.4)
t.send("printf 'AAAA_marker_one\\n'\r", 0.5)
t.prefix(b"pv", 0.6)
t.send("printf 'BBBB_marker_two\\n'\r", 0.5)
# Background tab so A/B are not session-visible (cells show content, not the
# "Active in session" placeholder).
t.send(b"\x1bt", 0.6)   # Alt+t: new empty tab
t.prefix(b"xm", 0.7)
# The manager opens with its search bar focused; Tab hands focus to the tree.
t.send(b"\t", 0.3)
# Navigate to Tab 1 row and expand it. Selected starts on 'local' (row 0).
# j down to 'main', j to 'Tab 1', then 'l' to expand.
t.send("j", 0.2)   # -> main
t.send("j", 0.2)   # -> Tab 1
t.send("l", 0.4)   # expand Tab 1 -> reveals panes
t.dump("expanded tab")

# Now navigate onto the first pane and mark, then next pane and mark.
t.send("j", 0.2)   # -> first pane
t.send(" ", 0.3)   # mark
t.dump("mark 1")
t.send("j", 0.2)   # -> second pane
t.send(" ", 0.3)   # mark
t.dump("mark 2")

# 'v' then 'a' => AddToView -> view picker.
t.send("v", 0.2)
t.send("a", 0.5)
t.dump("view picker")

# Confirm selection (Enter) -> creates new view with both panes.
t.send("\r", 0.8)
t.dump("in view")
print("alive:", t.alive())
print("LOG panic:", "panic" in t.log("client").lower() or "panic" in t.log("server").lower())
t.kill()
