#!/usr/bin/env python3
"""The `views` sidebar panel lists the local server's views and marks the one
this client has on screen.

A real PTY, because the CLIENT paints the panel. "Another client" is a raw
wire connection on the same socket, never a second `Tui`: `Tui.start()` wipes
its rundir, which would destroy the first client's server.

The mark is checked two ways on every transition: the row's `* ` prefix (text)
and the full-width `sidebar_current_bg` band behind both of its lines (colour).
A row that is also the panel's j/k selection shows the selection band instead,
so the band is only asserted on rows the selection is not on.

  1  no views: the panel reads "no views"
  2  another client creates, renames and deletes a view: the panel follows
     within 2 s with NO byte sent to the PTY (the ViewList feed and the quiet
     repaint)
  3  startup feed: the PTY client is killed, a view is created while no PTY
     client is running, and a fresh client shows it on its first paint
  4  composing a view in the session manager enters it: its row is marked and
     reads "2 cells · grid"; `w q` clears the mark and keeps the row. This does
     NOT isolate the enter-path feed: the cell adds after the create produce a
     ViewList that marks the row too. Case 5 is the one that isolates it.
  5  an empty Tab 2 is opened, so the session on screen shows neither marker
     and a different pane size. Enter on the composed row from the panel
     enters the view: both panes' markers in the content columns, `stty size`
     in a cell reports the cell geometry, the panel header is no longer
     focused, and the row is marked. No ViewList arrives for this transition,
     so only the enter-path feed can mark it.
  6  Enter on the row of the view already on screen: the header is FOCUSED
     before Enter (Alt+h, since SidebarFocus* is refused inside a view), then
     no new Detach reaches the server, the header is unfocused and a typed
     marker lands in the view's cell
  7  Enter on another view switches: the mark moves; a session-manager jump
     to the session leaves the view and clears the mark
  8  selection identity: three views, the MIDDLE one selected, the other
     client deletes the FIRST, and Enter still enters the middle one. A panel
     that kept the index instead would land on the third.
  9  the other client deletes the view on screen while another view stays:
     the client leaves it, the other row is still listed and nothing is marked

Not driven: Enter on a row whose view another client deleted before this
client's ViewList arrived. With the ViewList feed working the panel is never a
list behind for long enough to press a key, so the race cannot be produced on
demand. The "gone" outcome is unit-tested on `resolve_view_entry` in
`src/client/view.rs`, which both the panel and the session manager call.
 10  config reload: the panel removed, a view created, the panel restored:
     the rebuilt panel lists the views at once
 11  no panic in either log (proven able to fail by a deliberate panic in the
     plugin's render once), and the client is alive

The rundir is fixed (RMX_RUNDIR overrides): two concurrent runs corrupt each
other, and the false red looks like a real failure.

Run from the repo root:  python3 tests/pty/sidebar_views.py
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import (  # noqa: E402
    PROTOCOL_VERSION, Tui, sm_compose_view, sm_goto_text, sm_open,
)
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
from harness import Client  # noqa: E402

import pexpect  # noqa: E402
import pyte  # noqa: E402

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUN = os.environ.get("RMX_RUNDIR", "/tmp/rmx-sbviews")
SOCK = f"{RUN}/run/remux.sock"

COLS, ROWS = 120, 40
SIDEBAR_W = 30
FRAME = 1
# Content 90 columns -> two 45-wide grid cells -> 43 inside the borders; rows:
# 40 - 1 status row - 2 border rows. The same arithmetic as sidebar_view.py.
CELL_STTY = "37 43"

KEYS = """
[keybindings.command]
"Alt-2" = "SidebarFocusLeft"
"""

PANEL = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "views"
  weight = 1
"""

CFG = KEYS + PANEL

FAILURES = []


def check(name, cond, detail=""):
    print(("PASS  " if cond else "FAIL  ") + name + (f"  [{detail}]" if detail and not cond else ""))
    if not cond:
        FAILURES.append(name)


def theme_hex(role):
    needle = f"{role}: ThemeColor::Rgb("
    for line in open("src/config/theme.rs"):
        if needle in line:
            rgb = line.split(needle)[1].split(")")[0]
            return "".join(f"{int(v):02x}" for v in rgb.split(","))
    raise SystemExit(f"no Rgb default for {role}")


CURRENT_BG = theme_hex("sidebar_current_bg")
HEADER_ACTIVE_BG = theme_hex("sidebar_header_active_bg")


class Wire:
    """The other client: a raw connection that tracks the last ViewList."""

    def __init__(self):
        self.c = Client(SOCK)
        self.c.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "t"})
        self.c.recv()
        self.views = []

    def pump(self, t=0.4):
        try:
            msgs = self.c.drain(t)
        except (ConnectionError, OSError):
            return
        for m in msgs:
            if isinstance(m, dict) and "ViewList" in m:
                self.views = m["ViewList"]["views"]

    def id_of(self, name):
        self.pump(0.3)
        for v in self.views:
            if v.get("name") == name:
                return v["id"]
        return None

    def create(self, name):
        self.c.send({"ViewCreate": {"name": name}})
        end = time.time() + 3
        while time.time() < end:
            vid = self.id_of(name)
            if vid is not None:
                return vid
        raise SystemExit(f"the server never listed view {name!r}")

    def rename(self, vid, name):
        self.c.send({"ViewRename": {"id": vid, "name": name}})

    def delete(self, vid):
        self.c.send({"ViewDelete": {"id": vid}})


def panel(t):
    """The panel's columns of every screen row, inside the sidebar frame."""
    return [r[FRAME:SIDEBAR_W - FRAME] for r in t.rows_text()]


def find_row(t, name):
    """(y, marked) of the panel row naming `name`, or (None, False)."""
    for y, text in enumerate(panel(t)):
        body = text.strip()
        if body == name or body == f"* {name}":
            return y, body.startswith("* ")
    return None, False


def wait_for(t, cond, timeout=2.0):
    end = time.time() + timeout
    while time.time() < end:
        t.pump(0.15)
        if cond():
            return True
    return cond()


def banded(t, y):
    """Both lines of the row at `y` are sidebar_current_bg across the panel."""
    if y is None:
        return False
    return all(str(t.screen.buffer[line][x].bg) == CURRENT_BG
               for line in (y, y + 1) for x in range(FRAME, SIDEBAR_W - FRAME))


def any_band(t):
    return any(all(str(t.screen.buffer[y][x].bg) == CURRENT_BG for x in range(FRAME, SIDEBAR_W - FRAME))
               for y in range(ROWS))


def any_star(t):
    return any(text.strip().startswith("* ") for text in panel(t))


def header_focused(t):
    for y, text in enumerate(panel(t)):
        if text.strip() == "views":
            return str(t.screen.buffer[y][FRAME].bg) == HEADER_ACTIVE_BG
    return False


def content(t):
    return [r[SIDEBAR_W:] for r in t.rows_text()]


def has_marker(t, marker):
    return any(re.finditer(re.escape(marker), "\n".join(content(t))))


def status(t):
    return t.rows_text()[-1]


def detaches(t):
    return t.log("server").count("Detach")


def stty_reading(t):
    t.send("stty size\r", 1.8)
    found = None
    for row in content(t):
        for tok in row.split("│"):
            parts = tok.strip().split()
            if len(parts) == 2 and all(p.isdigit() for p in parts):
                found = tok.strip()
    return found


def respawn(t):
    """A fresh PTY client on the SAME rundir and server: `Tui.start` would
    wipe the rundir and with it the server this case needs to survive."""
    t.screen = pyte.Screen(t.cols, t.rows)
    t.stream = pyte.ByteStream(t.screen)
    t.child = pexpect.spawn(BIN, [], env=t.env, dimensions=(t.rows, t.cols), encoding=None)
    t.pump(2.0)


def write_config(t, body):
    with open(f"{t.rundir}/config/remux/config.toml", "w") as fh:
        fh.write(body)
    t.pump(2.5)


def select_row(t, name):
    """Step j until the selection band (sidebar_selection_bg) is on `name`'s
    row. The panel must already hold the keyboard."""
    sel_bg = theme_hex("sidebar_selection_bg")
    for _ in range(8):
        y, _ = find_row(t, name)
        if y is not None and str(t.screen.buffer[y][FRAME].bg) == sel_bg:
            return True
        t.send("j", 0.3)
    return False


def main():
    t = Tui(RUN, cols=COLS, rows=ROWS, config=CFG).start()
    try:
        t.pump(1.0)
        t.send("clear\r", 0.4)
        t.send("printf 'RAN:%s\\n' one\r", 0.5)
        t.prefix(b"pv", 0.8)
        t.send("clear\r", 0.4)
        t.send("printf 'RAN:%s\\n' two\r", 0.6)
        w = Wire()

        # --- 1 -----------------------------------------------------------
        check("1 no views: the panel says so", any("no views" in p for p in panel(t)),
              "\n".join(panel(t)[:6]))

        # --- 2 -----------------------------------------------------------
        vid = w.create("alpha")
        ok = wait_for(t, lambda: find_row(t, "alpha")[0] is not None)
        y, _ = find_row(t, "alpha")
        detail = panel(t)[y + 1].strip() if y is not None else ""
        check("2 another client's ViewCreate shows with no keystroke", ok and detail == "0 cells · grid",
              f"{detail!r} {panel(t)[:6]}")
        w.rename(vid, "alpha-two")
        check("2 a rename shows with no keystroke",
              wait_for(t, lambda: find_row(t, "alpha-two")[0] is not None and find_row(t, "alpha")[0] is None))
        w.delete(vid)
        check("2 a delete removes the row and 'no views' is back",
              wait_for(t, lambda: any("no views" in p for p in panel(t))), "\n".join(panel(t)[:6]))

        # --- 3 -----------------------------------------------------------
        t.child.terminate(force=True)
        time.sleep(0.5)
        w.create("pre")
        respawn(t)
        check("3 a view created with no PTY client running is on the first paint",
              find_row(t, "pre")[0] is not None, "\n".join(panel(t)[:8]))

        # --- 4 -----------------------------------------------------------
        sm_compose_view(t, panes=(0, 1), settle=2.0)
        t.pump(1.0)
        w.pump(0.5)
        names = [v["name"] for v in w.views]
        composed = next((n for n in names if n != "pre"), None)
        y, marked = find_row(t, composed) if composed else (None, False)
        check("4 the composed view is on screen", composed is not None and composed in status(t),
              f"{names} {status(t)!r}")
        check("4 compose-enter marks the composed row with * and the band",
              marked and banded(t, y), f"y={y} marked={marked} {panel(t)[:8]}")
        detail = panel(t)[y + 1].strip() if y is not None else ""
        check("4 the composed row reads 2 cells · grid", detail == "2 cells · grid", detail)
        py, pmarked = find_row(t, "pre")
        check("4 the other row is not marked", py is not None and not pmarked and not banded(t, py))
        t.prefix(b"wq", 1.2)
        y, marked = find_row(t, composed)
        check("4 w q clears the mark and keeps the row",
              y is not None and not any_star(t) and not any_band(t), "\n".join(panel(t)[:8]))
        t.prefix(b"tn", 1.8)
        check("5 precondition: the session on screen shows neither marker",
              not has_marker(t, "RAN:one") and not has_marker(t, "RAN:two") and "Tab 2" in status(t),
              repr(status(t)))

        # --- 5 -----------------------------------------------------------
        t.send("\x1b2", 0.5)
        check("5 Alt-2 focuses the panel", header_focused(t))
        check("5 the selection reaches the composed row", select_row(t, composed))
        t.send("\r", 2.0)
        check("5 Enter puts the view on screen", composed in status(t), repr(status(t)))
        check("5 both panes' markers are in the content columns",
              has_marker(t, "RAN:one") and has_marker(t, "RAN:two"))
        check("5 the panel header lost focus", not header_focused(t))
        y, marked = find_row(t, composed)
        check("5 the entered view's row is starred (no ViewList for this transition)",
              marked, "\n".join(panel(t)[:8]))
        reading = stty_reading(t)
        check("5 stty size in a cell reports the cell geometry", reading == CELL_STTY, str(reading))

        # --- 6 -----------------------------------------------------------
        for _ in range(3):
            if header_focused(t):
                break
            t.send("\x1bh", 0.6)
        check("6 Alt+h reaches the panel from the view (header focused before Enter)", header_focused(t))
        select_row(t, composed)
        before = detaches(t)
        t.send("\r", 1.2)
        check("6 Enter on the displayed view sends no Detach", detaches(t) == before,
              f"{before} -> {detaches(t)}")
        check("6 the header is unfocused again", not header_focused(t))
        t.send("printf 'RAN:%s\\n' three\r", 0.8)
        check("6 a typed marker lands in the view's cell", has_marker(t, "RAN:three"))
        y, marked = find_row(t, composed)
        check("6 the row is still starred", marked)

        # --- 7 -----------------------------------------------------------
        for _ in range(3):
            if header_focused(t):
                break
            t.send("\x1bh", 0.6)
        select_row(t, "pre")
        t.send("\r", 1.5)
        py, pmarked = find_row(t, "pre")
        cy, cmarked = find_row(t, composed)
        check("7 Enter on another view switches to it", "pre" in status(t), repr(status(t)))
        check("7 the mark moves to the new view and off the old one",
              pmarked and not cmarked, "\n".join(panel(t)[:8]))
        check("7 the old row, no longer selected or current, has no band", not banded(t, cy))
        sm_open(t)
        sm_goto_text(t, "main")
        t.send("\r", 1.5)
        check("7 a session-manager jump leaves the view", "pre" not in status(t), repr(status(t)))
        check("7 the jump clears the mark", not any_star(t) and not any_band(t), "\n".join(panel(t)[:8]))

        # --- 8 -----------------------------------------------------------
        w.create("tail")
        check("8 three views are listed",
              wait_for(t, lambda: all(find_row(t, n)[0] is not None for n in ("pre", composed, "tail"))),
              "\n".join(panel(t)[:10]))
        t.send("\x1b2", 0.5)
        check("8 the selection reaches the middle row", select_row(t, composed))
        w.delete(w.id_of("pre"))
        wait_for(t, lambda: find_row(t, "pre")[0] is None)
        t.send("\r", 2.0)
        check("8 after the first view is deleted, Enter still enters the middle one",
              composed in status(t) and "tail" not in status(t), repr(status(t)))
        check("8 the middle view's panes are on screen", has_marker(t, "RAN:one"))

        # --- 9 -----------------------------------------------------------
        w.delete(w.id_of(composed))
        wait_for(t, lambda: find_row(t, composed)[0] is None, timeout=3)
        t.pump(1.0)
        check("9 another client deleting the displayed view leaves it",
              composed not in status(t) and t.alive(), repr(status(t)))
        ty, tmarked = find_row(t, "tail")
        check("9 the other view is still listed", ty is not None, "\n".join(panel(t)[:6]))
        check("9 nothing is marked", not tmarked and not any_star(t) and not any_band(t),
              "\n".join(panel(t)[:6]))

        # --- 10 ----------------------------------------------------------
        write_config(t, KEYS)
        check("10 the panel is gone with the sidebar removed", not any("views" in p for p in panel(t)[:4]))
        w.create("late")
        t.pump(0.8)
        write_config(t, CFG)
        check("10 the rebuilt panel lists the views at once", find_row(t, "late")[0] is not None,
              "\n".join(panel(t)[:8]))

        check("11 the client is alive", t.alive())
    finally:
        try:
            w.c.close()
        except Exception:
            pass
        t.kill()
    for name in ("server.log", "client.log"):
        path = f"{RUN}/state/remux/{name}"
        body = open(path, errors="replace").read() if os.path.exists(path) else ""
        check(f"11 no panic in {name}", "panicked at" not in body)

    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
