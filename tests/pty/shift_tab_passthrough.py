"""Does the client forward Shift-Tab (CSI Z / crossterm BackTab) to the PTY?

Runs `cat -v` in raw mode inside a pane, then sends Tab (control) and
Shift-Tab, and checks what the app actually received.
"""
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui

RUNDIR = "/tmp/rmx-stab"

t = Tui(RUNDIR).start()
t.send("stty raw -echo; cat -vT\r", t=1.0)

t.send(b"\t", t=0.6)
after_tab = "".join(t.rows_text())
t.send(b"\x1b[Z", t=0.6)          # legacy CSI Z
after_stab = "".join(t.rows_text())
t.send(b"\x1b[9;2u", t=0.6)       # kitty/CSI-u form of Shift+Tab
after_csiu = "".join(t.rows_text())

got_tab = "^I" in after_tab
got_stab = "^[[Z" in after_stab.replace(" ", "")
got_csiu = "^[[Z" in after_csiu.replace(" ", "")

t.dump("final")
print("Tab reached the app:      ", got_tab)
print("Shift-Tab (CSI Z) reached the app:  ", got_stab)
print("Shift-Tab (CSI-u) reached the app:  ", got_csiu)
print("alive:", t.alive())
print("panic in log:", "panicked" in t.log("client") + t.log("server"))
t.kill()
# Requirement, not current behaviour: both Tab and Shift-Tab must reach the app.
sys.exit(0 if (got_tab and got_stab) else 2)
