#!/usr/bin/env python3
"""A pane added to a View must be resized to the CELL it is given -- every
visible cell, not just the focused one.

The bug this guards (user report): a session had two panes split horizontally
with the bottom one ~25% of the height, running neovim. Adding both to a view
put the small pane in the SECOND (unfocused) cell, and neovim stayed tiny: the
pane was never resized to the cell it now occupies. Cause: `subscribe_view_cells`
sent `size_demand: true` for the FOCUSED cell only (the old "Model B" rule), so
every other cell recorded `None` on the server, `subscriber_min_demand_locked`
ignored it and `recompute_pane_size` left the pane at its home-session
allotment. A reflowing shell rewraps and looks tolerable; a full-screen app
renders at its PTY size and looks tiny inside a big cell.

This has to be a real-PTY test: the decision lives in the CLIENT
(`subscribe_view_cells` in src/main.rs), so a frame-level harness -- which
hand-writes its own `SubscribePane` messages -- cannot exercise it at all.

The pane's ACTUAL size is read two ways, neither of which depends on how the
cell happens to look:
  * an OBSERVER: a second plain protocol socket onto the same throwaway server,
    subscribed to both panes with `size_demand: false`. A `None` demand is a
    no-op in the min-across fold, so the observer cannot perturb what it
    measures; it just reads `PaneContent { cols, rows }`.
  * the bottom pane itself runs `trap 'stty size' WINCH`, so its PTY size is
    printed into the pane on every resize and shows up in the cell.

Phases:
  1. grid    -- both cells visible: BOTH panes are sized to their cell.
  2. monocle -- the hidden cell imposes NO demand: its pane keeps its size
                instead of being clamped by a cell it is not visible in.
  3. zoom    -- same for the non-zoomed cells while a view is zoomed.
  4. terminal resize -- every cell rect changed, so every pane follows.
  5. reclaim -- closing the view re-attaches the session, which must give the
                pane its home allotment back (~25%).
  6. alt screen (second client, historyless full-screen app stand-in): the pane
     is sized to the cell, so neovim would fill it.

Run from the repo root:  python3 tests/pty/view_cell_pane_resize.py
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
from pty_harness import Tui, sm_compose_view   # noqa: E402
from harness import Client, name_of  # noqa: E402

COLS, ROWS = 120, 40
MARK_TOP = "TOP_marker"
MARK_BOT = "BOT_marker"


class Observer:
    """A plain protocol client that watches pane sizes without demanding one."""

    def __init__(self, rundir):
        self.c = Client(f"{rundir}/run/remux.sock")
        self.c.hello()
        self.last = {}

    def panes(self):
        """[(pane_id, is_focused)] of the one session's one tab, in order."""
        self.c.send("ListSessionTree")
        for m in self.c.drain(1.0):
            self._absorb(m)
            if name_of(m) == "SessionTree":
                body = m["SessionTree"]
                sessions = body["unfiled"] + [s for f in body["folders"] for s in f["sessions"]]
                return [(p["id"], p["is_focused"])
                        for s in sessions for tab in s["tabs"] for p in tab["panes"]]
        return []

    def watch(self, pane_id):
        self.c.send({"SubscribePane": {"pane_id": pane_id, "cols": 80, "rows": 24,
                                       "size_demand": False}})

    def _absorb(self, m):
        if name_of(m) == "PaneContent":
            pc = m["PaneContent"]
            self.last[pc["pane_id"]] = (pc["cols"], pc["rows"])

    def sizes(self, t=1.2):
        """Latest known (cols, rows) per pane, refreshed with anything pending."""
        for m in self.c.drain(t):
            self._absorb(m)
        return dict(self.last)

    def close(self):
        self.c.close()


def two_stacked_panes(t):
    """Top pane ~75% / bottom pane ~25%, the user's uneven horizontal split."""
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_TOP}\\n'\r", 0.5)
    t.prefix(b"ps", 0.8)                       # split horizontal -> stacked
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_BOT}\\n'\r", 0.6)
    # Focus is on the new BOTTOM pane: shrink it (the resize group is sticky, so
    # each `k` shrinks it further) down to roughly a quarter of the height.
    t.prefix(b"pR", 0.5)
    for _ in range(4):
        t.send("k", 0.6)
    t.send("\x1b", 0.3)                        # leave the sticky resize group


def compose_view(t):
    """Mark both panes in the session manager and alias them into a new view."""
    sm_compose_view(t, panes=(0, 1), settle=1.5)           # top + bottom -> new view
    t.pump(1.0)


def require_in_view(t):
    """Hard gate: without a live view every size assertion below is vacuous."""
    reasons = []
    if t.has("Session Manager"):
        reasons.append("session manager overlay still up")
    if t.has("Add Pane to View"):
        reasons.append("view picker overlay still up")
    if "View 1" not in t.rows_text()[-1]:
        reasons.append(f"not a view status bar: {t.rows_text()[-1].rstrip()!r}")
    if reasons:
        print("ABORT: never entered the view:")
        for r in reasons:
            print(f"  - {r}")
        t.dump("not in a view")
        t.kill()
        sys.exit(1)


def cycle_to(t, name, obs, fails):
    """Cycle the view's layout (Prefix w Space) until the status bar names it.

    Returns the pane sizes observed in the layout immediately BEFORE the one we
    land on, or `None` if `name` was never reached. Cycling walks through the
    intermediate layouts, each of which sizes the cells it shows -- so "the pane
    a hidden cell aliases was left alone" means "unchanged since the last layout
    that showed it", not "unchanged since the start".
    """
    before = obs.sizes(0.4)
    for _ in range(6):
        if name in t.rows_text()[-1].lower():
            return before
        before = obs.sizes(0.4)
        t.prefix(b"w ", 0.8)
    if name in t.rows_text()[-1].lower():
        return before
    fails.append(f"could not reach the {name} layout; status bar is "
                 f"{t.rows_text()[-1].rstrip()!r}")
    return None


def main_scenario(fails):
    """Phases 1-5: the user's exact scenario, plus monocle/zoom/terminal
    resize/reclaim."""
    t = Tui("/tmp/rmxvcr", cols=COLS, rows=ROWS).start()
    obs = None
    try:
        two_stacked_panes(t)
        obs = Observer("/tmp/rmxvcr")
        panes = obs.panes()
        if len(panes) != 2:
            print(f"ABORT: expected 2 panes, got {panes}")
            t.dump("no two panes")
            t.kill()
            sys.exit(1)
        top, bot = panes[0][0], panes[1][0]
        obs.watch(top)
        obs.watch(bot)
        home = obs.sizes()
        print(f"session allotment : top={home.get(top)} bottom={home.get(bot)}")
        if not home.get(bot) or home[bot][1] > home[top][1] // 2:
            fails.append(f"the split is not uneven enough to be meaningful: {home}")
        # The bottom pane reports its PTY size on every SIGWINCH, so the cell
        # shows the pane's real size as a human would see it.
        t.send("trap 'stty size' WINCH; while :; do sleep 1; done\r", 0.8)

        # --- phase 1: grid -- every visible cell sizes its pane --------------
        compose_view(t)
        require_in_view(t)
        grid = obs.sizes()
        print(f"view (grid)       : top={grid.get(top)} bottom={grid.get(bot)}")
        if grid.get(top) is None or grid.get(bot) is None:
            fails.append(f"no PaneContent for one of the panes in the view: {grid}")
        elif grid[bot] != grid[top]:
            fails.append(
                f"the UNFOCUSED cell's pane was not sized to its cell: "
                f"bottom={grid[bot]} but the focused cell's pane is {grid[top]} "
                f"(a 2-cell grid gives both cells the same interior). It is still "
                f"at its session allotment {home.get(bot)} -- a full-screen app "
                f"would render tiny inside the cell.")
        elif grid[bot][1] <= home[bot][1]:
            fails.append(f"the bottom pane did not grow into its cell: "
                         f"{home[bot]} -> {grid[bot]}")
        # ... and the pane itself agrees, in the cell, in the terminal.
        want = f"{grid.get(bot, (0, 0))[1]} {grid.get(bot, (0, 0))[0]}"
        if not t.has(want):
            fails.append(f"the bottom pane never reported its new PTY size "
                         f"{want!r} (stty size) inside the cell")

        # --- phase 2: monocle -- a HIDDEN cell must impose no demand ---------
        before = cycle_to(t, "monocle", obs, fails)
        if before is not None:
            mono = obs.sizes()
            print(f"view (monocle)    : top={mono.get(top)} bottom={mono.get(bot)} "
                  f"(previous layout: bottom={before.get(bot)})")
            if mono.get(top) == grid.get(top):
                fails.append("monocle did not resize the visible cell's pane "
                             f"(still {mono.get(top)})")
            if mono.get(bot) != before.get(bot):
                fails.append(f"a cell HIDDEN by monocle clamped its pane: "
                             f"{before.get(bot)} -> {mono.get(bot)}")
            if mono.get(bot) == mono.get(top):
                fails.append(f"the monocle-hidden cell's pane was sized to the "
                             f"monocle area {mono.get(bot)} -- a cell that is not "
                             "drawn must impose no size")
            cycle_to(t, "grid", obs, fails)
            back = obs.sizes()
            print(f"view (grid again) : top={back.get(top)} bottom={back.get(bot)}")
            if back.get(top) != grid.get(top) or back.get(bot) != grid.get(bot):
                fails.append(f"returning to grid did not restore the cell sizes: "
                             f"top={back.get(top)} bottom={back.get(bot)}")

        # --- phase 3: zoom -- the non-zoomed cells impose no demand ----------
        t.prefix(b"f", 1.0)
        zoom = obs.sizes()
        print(f"view (zoomed)     : top={zoom.get(top)} bottom={zoom.get(bot)}")
        if zoom.get(top) == grid.get(top):
            fails.append(f"zoom did not grow the focused cell's pane "
                         f"(still {zoom.get(top)})")
        if zoom.get(bot) != grid.get(bot):
            fails.append(f"a cell hidden by ZOOM clamped its pane: "
                         f"{grid.get(bot)} -> {zoom.get(bot)}")
        t.prefix(b"f", 1.0)                      # unzoom
        unzoom = obs.sizes()
        print(f"view (unzoomed)   : top={unzoom.get(top)} bottom={unzoom.get(bot)}")
        if unzoom.get(top) != grid.get(top):
            fails.append(f"unzoom did not restore the cell size: {unzoom.get(top)}")

        # --- phase 4: the terminal itself is resized -------------------------
        # Every cell rect changed, so every cell must re-demand. Restored to the
        # original size afterwards: the home allotment the reclaim phase asserts
        # is derived from the attached client's terminal size.
        t.resize(100, 30)
        small = obs.sizes()
        print(f"view (100x30)     : top={small.get(top)} bottom={small.get(bot)}")
        if small.get(bot) != small.get(top):
            fails.append(f"after a terminal resize the cells' panes disagree: "
                         f"top={small.get(top)} bottom={small.get(bot)}")
        elif small.get(bot) == grid.get(bot):
            fails.append(f"the cells' panes did not follow the terminal resize "
                         f"(still {small.get(bot)} at 100x30)")
        t.resize(COLS, ROWS)
        wide = obs.sizes()
        print(f"view (120x40)     : top={wide.get(top)} bottom={wide.get(bot)}")
        if wide.get(top) != grid.get(top) or wide.get(bot) != grid.get(bot):
            fails.append(f"resizing back did not restore the cell sizes: "
                         f"top={wide.get(top)} bottom={wide.get(bot)}")

        # --- phase 5: reclaim -- the session takes its panes back ------------
        t.prefix(b"wq", 1.5)                     # close the view, re-attach
        t.pump(1.0)
        home2 = obs.sizes()
        print(f"after leaving     : top={home2.get(top)} bottom={home2.get(bot)}")
        if home2.get(top) != home.get(top) or home2.get(bot) != home.get(bot):
            fails.append(f"the panes were not reclaimed by their session: "
                         f"top={home2.get(top)} (want {home.get(top)}) "
                         f"bottom={home2.get(bot)} (want {home.get(bot)})")

        alive, logs = t.alive(), (t.log("client") + t.log("server")).lower()
        print(f"alive={alive} panic={'panic' in logs}")
        if not alive:
            fails.append("client died")
        if "panic" in logs:
            fails.append("panic in the logs")
        if fails:
            t.dump("final")
    finally:
        if obs:
            obs.close()
        t.kill()


def alt_screen_scenario(fails):
    """Phase 6: the small pane on the ALT screen (a full-screen app like the
    reported neovim) must still be sized to its cell."""
    t = Tui("/tmp/rmxvcra", cols=COLS, rows=ROWS).start()
    obs = None
    try:
        two_stacked_panes(t)
        obs = Observer("/tmp/rmxvcra")
        panes = obs.panes()
        if len(panes) != 2:
            print(f"ABORT(alt): expected 2 panes, got {panes}")
            t.kill()
            sys.exit(1)
        top, bot = panes[0][0], panes[1][0]
        obs.watch(top)
        obs.watch(bot)
        home = obs.sizes()
        print(f"alt: allotment    : top={home.get(top)} bottom={home.get(bot)}")
        # Enter the alt screen in the bottom pane: since "the alt screen has no
        # scrollback" it never reflows, exactly like a full-screen app.
        t.send("printf '\\033[?1049h'\r", 0.6)
        compose_view(t)
        require_in_view(t)
        got = obs.sizes()
        print(f"alt: view (grid)  : top={got.get(top)} bottom={got.get(bot)}")
        if got.get(bot) != got.get(top):
            fails.append(f"alt-screen pane not sized to its cell: bottom="
                         f"{got.get(bot)} focused-cell pane={got.get(top)} "
                         f"(allotment was {home.get(bot)}) -- a full-screen app "
                         "would not fill the cell")
        alive, logs = t.alive(), (t.log("client") + t.log("server")).lower()
        print(f"alt: alive={alive} panic={'panic' in logs}")
        if not alive:
            fails.append("client died (alt screen)")
        if "panic" in logs:
            fails.append("panic in the logs (alt screen)")
        if fails:
            t.dump("final (alt)")
    finally:
        if obs:
            obs.close()
        t.kill()


def main():
    fails = []
    main_scenario(fails)
    alt_screen_scenario(fails)
    if fails:
        print("\nFAILURES:")
        for f in fails:
            print(f"  - {f}")
        print("RESULT: FAIL")
        sys.exit(1)
    print("\nRESULT: PASS -- every visible view cell sizes its pane to the cell, "
          "hidden (monocle/zoomed-out) cells impose no size, and the session "
          "reclaims its panes when the view is closed")


if __name__ == "__main__":
    main()
