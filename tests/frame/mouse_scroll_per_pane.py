"""Scrollback is per (client, PANE), not one offset per client.

Two user reports, one cause. `ClientConnection` held a single `scroll_offset`
and the compositor pinned it to whichever pane owned input, so:

  1. the wheel over a NON-focused pane scrolled the FOCUSED one -- the hit test
     resolved the right pane and `handle_scroll_delta` then ignored it,
     clamping and applying against `Session::input_target`;
  2. scrolling a pane, moving focus away and coming back lost the scroll --
     `PaneFocus*`/`TabNext`/`TabPrev`/`TabGoto` were classified
     `returns_to_live_tail`, which zeroed the one offset there was.

Both are asserted on CONTENT, not on the wire `scroll_offset` field: that field
is the FOCUSED pane's offset by definition, so it stays 0 while the wheel moves
a non-focused pane and could never see report 1. The two panes are filled with
DIFFERENT markers (`AAA:n` on the left of the split, `BBB:n` on the right),
each assembled by the shell (`printf` + the loop variable) so a shell echoing
the command line cannot produce it, and every row scan uses `finditer`: two
panes share a screen row, and `re.search` would stop at the first match and
report the other pane's text as this pane's.

Run: python3 tests/frame/mouse_scroll_per_pane.py [-v]
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of, only  # noqa: E402

# Keyed on the pid: two concurrent runs of this harness on one machine would
# otherwise share a socket and a state dir, and the corruption that follows
# looks exactly like the bug under test. Kept short, because a Unix socket path
# must stay under 108 characters.
RUNDIR_BASE = f"/tmp/rmx-pp{os.getpid()}"
COLS, ROWS = 100, 30
VERBOSE = "-v" in sys.argv
HIST = 300
# Wheel notches to send. `WHEEL_LINES` is 3, so this moves ~60 lines -- more
# than a pane is tall, which is what makes "an earlier line is now on screen"
# unambiguous rather than an off-by-one against the live tail.
NOTCHES = 20
KEY_SCROLL = 30


class Grid:
    """Reconstruct the composited grid from Full/Diff/Scroll renders.

    Copied from `scroll_max_wedge.py` rather than imported: a frame harness
    stays runnable on its own.
    """

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]
        self.viewport_top = None
        self.scroll_offset = None
        self.focused_pane_rect = None
        self.frames = 0

    def _ch(self, cell):
        return cell.get("c", " ") if isinstance(cell, dict) else " "

    def _fields(self, body):
        self.viewport_top = body.get("viewport_top", self.viewport_top)
        if "scroll_offset" in body:
            self.scroll_offset = body["scroll_offset"]
        self.focused_pane_rect = body.get("focused_pane_rect") or self.focused_pane_rect
        self.frames += 1

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    if y < self.rows and x < self.cols:
                        self.g[y][x] = self._ch(cell)
            self._fields(body)
        elif n == "RenderDiff":
            for ch in body["changes"]:
                y, x = ch["y"], ch["x"]
                if y < self.rows and x < self.cols:
                    self.g[y][x] = self._ch(ch["cell"])
            self._fields(body)
        elif n == "ScrollRender":
            px, py = body["pane_x"], body["pane_y"]
            pw, ph = body["pane_width"], body["pane_height"]
            delta, new_rows = body["delta"], body["new_rows"]
            if delta > 0:
                for r in range(py, py + ph - delta):
                    if r + delta < self.rows:
                        self.g[r][px:px + pw] = self.g[r + delta][px:px + pw]
                for i, row in enumerate(new_rows):
                    r = py + ph - delta + i
                    if 0 <= r < self.rows:
                        self.g[r][px:px + pw] = [self._ch(c) for c in row][:pw]
            elif delta < 0:
                d = -delta
                for r in range(py + ph - 1, py + d - 1, -1):
                    self.g[r][px:px + pw] = self.g[r - d][px:px + pw]
                for i, row in enumerate(new_rows):
                    r = py + i
                    if 0 <= r < self.rows:
                        self.g[r][px:px + pw] = [self._ch(c) for c in row][:pw]
            self._fields(body)

    def rows_text(self):
        return ["".join(r) for r in self.g]

    def text(self):
        return "\n".join(self.rows_text())

    def marks(self, prefix):
        """Every `PREFIX:<n>` on screen, as a set of ints.

        `finditer`, never `search`: the two panes share every screen row, so a
        first-match scan reads the LEFT pane's text and reports it for the
        right one.
        """
        pat = re.compile(prefix + r":(\d+)")
        return {int(m.group(1)) for row in self.rows_text() for m in pat.finditer(row)}

    def marks_in(self, prefix, rect):
        """The same, restricted to a pane rect (`focused_pane_rect` geometry)."""
        pat = re.compile(prefix + r":(\d+)")
        out = set()
        for y in range(rect["y"], min(rect["y"] + rect["height"], self.rows)):
            row = "".join(self.g[y][rect["x"]:rect["x"] + rect["width"]])
            out |= {int(m.group(1)) for m in pat.finditer(row)}
        return out


class Fixture:
    """One throwaway server, one client, two side-by-side panes with history.

    `left` and `right` are the two panes' content rects, each captured from
    `focused_pane_rect` while that pane was focused. The pane created by the
    split holds `BBB`, the original holds `AAA`.
    """

    def __init__(self, rundir):
        self.srv = Server(rundir).start()
        self.c = Client(self.srv.sock)
        self.c.hello()
        self.c.send({"CreateSession": {"name": "main", "folder": None}})
        self.c.send({"Attach": {"session_name": "main"}})
        self.c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        self.grid = Grid(COLS, ROWS)
        self.settle()

        self.c.send({"Command": "PaneSplitVertical"})
        self.settle()
        self.right = dict(self.grid.focused_pane_rect)
        self.fill("BBB")

        self.focus("PaneFocusLeft")
        self.left = dict(self.grid.focused_pane_rect)
        self.fill("AAA")
        assert self.left["x"] != self.right["x"], (
            f"the split did not produce two side-by-side panes: "
            f"{self.left} vs {self.right}")

    def pump(self, t=0.5):
        msgs = self.c.drain(t)
        for m in msgs:
            self.grid.apply(m)
        return msgs

    def settle(self, quiet=0.4, rounds=25):
        """Drain until a whole window arrives empty, so a later assertion is
        never reading the trickle of the previous step's shell output."""
        for _ in range(rounds):
            if not self.pump(quiet):
                return
        raise SystemExit("server never went quiet")

    def fill(self, mark):
        """Put >1 screen of history into the pane that currently takes input."""
        self.c.send({"Input": {"data": list(
            b"for i in $(seq 1 %d); do printf '%s:%%s\\n' $i; done\n"
            % (HIST, mark.encode()))}})
        self.settle()

    def focus(self, cmd):
        self.c.send({"Command": cmd})
        self.settle()

    def wheel(self, rect, notches=NOTCHES, up=True):
        """Wheel inside `rect`, aimed at its middle row and column."""
        x = rect["x"] + rect["width"] // 2
        y = rect["y"] + rect["height"] // 2
        for _ in range(notches):
            self.c.send({"MouseScroll": {"x": x, "y": y, "up": up}})
            time.sleep(0.05)
            self.pump(0.1)
        self.pump(0.5)

    def click(self, rect):
        """Click the middle of `rect`, then settle.

        A press and its release, because `handle_mouse_click` is reached twice
        for one physical click and only the pair matches what a client sends.
        """
        x = rect["x"] + rect["width"] // 2
        y = rect["y"] + rect["height"] // 2
        self.c.send({"MouseClick": {"x": x, "y": y, "pane_id": None,
                                    "release": False}})
        self.pump(0.4)
        self.c.send({"MouseClick": {"x": x, "y": y, "pane_id": None,
                                    "release": True}})
        self.pump(0.8)

    def close(self, failed=False):
        """Tear down, and report a server panic without hiding anything.

        Raising from a `finally` replaces the body's failure with this one, so
        the message naming the actual defect never reaches the report. When the
        body already failed the panic is PRINTED instead of raised: a panic is
        usually the cause of that failure and dropping it silently would cost
        the one line that explains it. On the pass path it still raises, which
        is what catches a panic that broke nothing visible.
        """
        log = self.srv.log()
        self.c.close()
        self.srv.kill()
        panic = [ln for ln in log.splitlines() if "panicked" in ln]
        if not panic:
            return
        if failed:
            print("  (server also panicked: " + panic[0] + ")")
        else:
            assert False, "server panicked:\n" + log


def show(f, label):
    if VERBOSE:
        print(f"--- {label}: offset={f.grid.scroll_offset} "
              f"fpr={f.grid.focused_pane_rect}\n{f.grid.text()}\n")


def case_wheel_scrolls_the_pane_under_the_cursor():
    """Report 1: the wheel over a non-focused pane must scroll THAT pane.

    Both panes hold history, so the failure is two-sided: the pane under the
    cursor does not move AND the focused pane does. A single-sided check
    ("AAA did not move") would also pass on a build where the wheel did
    nothing at all.
    """
    f = Fixture(f"{RUNDIR_BASE}/wheel")
    try:
        # Focus the RIGHT pane; the wheel goes over the LEFT one.
        f.focus("PaneFocusRight")
        assert f.grid.focused_pane_rect["x"] == f.right["x"], \
            "focus is not on the right pane -- the case would not test a non-focused wheel"
        f.settle()
        a_before = f.grid.marks_in("AAA", f.left)
        b_before = f.grid.marks_in("BBB", f.right)
        assert a_before, "the left pane shows no AAA history -- nothing to scroll"
        assert b_before, "the right pane shows no BBB history -- the control is vacuous"
        show(f, "before wheel")

        f.wheel(f.left)
        show(f, "after wheel")

        a_after = f.grid.marks_in("AAA", f.left)
        b_after = f.grid.marks_in("BBB", f.right)
        assert a_after and min(a_after) < min(a_before), (
            f"the wheel over the non-focused pane did not scroll it: "
            f"AAA min {min(a_before)} -> {min(a_after) if a_after else None}")
        assert b_after == b_before, (
            f"the wheel over the LEFT pane moved the focused RIGHT pane: "
            f"BBB {sorted(b_before)[:3]}.. -> {sorted(b_after)[:3]}..")
        assert f.grid.scroll_offset == 0, (
            f"the focused pane reports scroll_offset={f.grid.scroll_offset}; "
            "the wheel was over the other pane")
    except BaseException:
        f.close(failed=True)
        raise
    else:
        f.close()
    print("PASS wheel scrolls the pane under the cursor")


def case_scroll_survives_a_focus_round_trip():
    """Report 2: focus away from a scrolled pane and back, scroll intact."""
    f = Fixture(f"{RUNDIR_BASE}/focus")
    try:
        # Focus is on the LEFT pane (AAA) after the fixture's last `focus`.
        assert f.grid.focused_pane_rect["x"] == f.left["x"]
        a_live = f.grid.marks_in("AAA", f.left)

        f.c.send({"ScrollDelta": {"delta": KEY_SCROLL}})
        f.pump(0.8)
        assert f.grid.scroll_offset == KEY_SCROLL, (
            f"scroll_offset={f.grid.scroll_offset}, expected {KEY_SCROLL} -- "
            "the pane is not scrolled, so the rest of the case proves nothing")
        a_scrolled = f.grid.marks_in("AAA", f.left)
        assert a_scrolled and min(a_scrolled) < min(a_live), \
            "the keyboard scroll did not move the left pane"
        show(f, "scrolled")

        f.focus("PaneFocusRight")
        assert f.grid.focused_pane_rect["x"] == f.right["x"], (
            "focusing away from a scrolled pane produced no frame moving the "
            "focus rect -- the client is looking at a stale screen")
        assert f.grid.scroll_offset == 0, (
            f"the newly focused (live) pane reports scroll_offset="
            f"{f.grid.scroll_offset}")
        assert f.grid.marks_in("AAA", f.left) == a_scrolled, (
            "the left pane snapped back to the live tail when focus left it: "
            f"{sorted(a_scrolled)[:3]}.. -> "
            f"{sorted(f.grid.marks_in('AAA', f.left))[:3]}..")
        show(f, "focus away")

        f.focus("PaneFocusLeft")
        assert f.grid.focused_pane_rect["x"] == f.left["x"], (
            "focusing BACK onto the scrolled pane produced no frame moving the "
            "focus rect -- the border is stale")
        assert f.grid.scroll_offset == KEY_SCROLL, (
            f"scroll_offset={f.grid.scroll_offset} on return, expected "
            f"{KEY_SCROLL} -- the pane's scroll was lost")
        assert f.grid.marks_in("AAA", f.left) == a_scrolled, \
            "the left pane's content moved across the focus round trip"
        show(f, "focus back")
    except BaseException:
        f.close(failed=True)
        raise
    else:
        f.close()
    print("PASS a pane's scroll survives a focus round trip")


def case_typing_snaps_only_the_focused_pane():
    """Typing returns the pane it goes to -- and only that one -- to the tail."""
    f = Fixture(f"{RUNDIR_BASE}/typing")
    try:
        # Scroll the LEFT pane with the wheel while the RIGHT one is focused,
        # then type. The keystroke lands in the right pane, so the left pane's
        # scroll must survive it.
        f.focus("PaneFocusRight")
        f.settle()
        f.wheel(f.left)
        a_scrolled = f.grid.marks_in("AAA", f.left)
        a_live = max(a_scrolled)
        assert min(a_scrolled) < HIST - 30, \
            f"the left pane is not scrolled back far enough: {sorted(a_scrolled)[:3]}"

        f.c.send({"Input": {"data": list(b"\n")}})
        f.settle()
        assert f.grid.marks_in("AAA", f.left) == a_scrolled, (
            "typing into the RIGHT pane snapped the LEFT pane's scroll: "
            f"{sorted(a_scrolled)[:3]}.. -> "
            f"{sorted(f.grid.marks_in('AAA', f.left))[:3]}..")

        # Now type into the scrolled pane itself: that one must snap.
        f.focus("PaneFocusLeft")
        f.c.send({"Input": {"data": list(b"\n")}})
        f.settle()
        assert f.grid.scroll_offset == 0, (
            f"typing into the scrolled pane left scroll_offset="
            f"{f.grid.scroll_offset}")
        after = f.grid.marks_in("AAA", f.left)
        assert after and max(after) >= a_live, (
            f"typing into the scrolled pane did not return it to the tail: "
            f"max AAA {max(after) if after else None} < {a_live}")
    except BaseException:
        f.close(failed=True)
        raise
    else:
        f.close()
    print("PASS typing snaps the pane it goes to, and only that one")


def case_click_to_focus_keeps_a_panes_scroll():
    """Clicking is the OTHER way focus moves, and it does not go through
    `RemuxCommand` at all.

    `PaneFocus*` is refused the snap by `returns_to_live_tail`, which a click
    never consults: `handle_mouse_click`'s pane arm sets the focused pane
    directly. So a snap re-introduced on the click path would be invisible to
    every case above.

    Both directions are asserted. Clicking ONTO the scrolled pane and clicking
    AWAY from it are different states -- away leaves it non-focused, which is
    the per-client render path, and onto it makes it the focused pane, which is
    the path the shared broadcast skips.
    """
    f = Fixture(f"{RUNDIR_BASE}/click")
    try:
        f.focus("PaneFocusRight")
        f.settle()
        f.wheel(f.left)
        a_scrolled = f.grid.marks_in("AAA", f.left)
        assert a_scrolled and min(a_scrolled) < HIST - 30, \
            f"the left pane is not scrolled back: {sorted(a_scrolled)[:3]}"

        f.click(f.left)
        assert f.grid.focused_pane_rect["x"] == f.left["x"], (
            "the click did not move focus to the left pane, so the case would "
            "not test a click-driven focus change")
        assert f.grid.marks_in("AAA", f.left) == a_scrolled, (
            "clicking ONTO the scrolled pane snapped it to the live tail: "
            f"{sorted(a_scrolled)[:3]}.. -> "
            f"{sorted(f.grid.marks_in('AAA', f.left))[:3]}..")
        assert f.grid.scroll_offset != 0, (
            "the newly focused pane reports scroll_offset=0 while its rows are "
            "still the scrolled ones")

        f.click(f.right)
        assert f.grid.focused_pane_rect["x"] == f.right["x"], \
            "the second click did not move focus back to the right pane"
        assert f.grid.marks_in("AAA", f.left) == a_scrolled, (
            "clicking AWAY from the scrolled pane snapped it: "
            f"{sorted(a_scrolled)[:3]}.. -> "
            f"{sorted(f.grid.marks_in('AAA', f.left))[:3]}..")
    except BaseException:
        f.close(failed=True)
        raise
    else:
        f.close()
    print("PASS click-to-focus keeps a pane's scroll, both directions")


def case_output_beside_a_scrolled_pane_is_still_diffed():
    """A client reading history in a non-focused pane still gets DIFFS.

    Such a client cannot take the shared broadcast frame, because that frame
    paints every pane live and would destroy the position. Compositing it on
    its own is not a reason to stop diffing, though: reading a build log while
    typing in the pane beside it is a state a user SITS in, and the pane being
    typed into produces output continuously, so a whole frame per chunk is the
    steady cost rather than a transient one.

    Asserted on the message kind AND on both panes' content, because a
    RenderDiff that dropped the scrolled pane's rows would satisfy a kind check
    on its own.
    """
    f = Fixture(f"{RUNDIR_BASE}/diffed")
    try:
        f.focus("PaneFocusRight")
        f.settle()
        f.wheel(f.left)
        a_scrolled = f.grid.marks_in("AAA", f.left)
        assert a_scrolled and min(a_scrolled) < HIST - 30, \
            f"the left pane is not scrolled back: {sorted(a_scrolled)[:3]}"

        # Output in the FOCUSED pane, which is where a user typing sits. The
        # marker is assembled by the shell, so an echo of the command line
        # cannot produce it.
        #
        # SEPARATE commands, not one loop: the server coalesces a burst of PTY
        # output into a single broadcast, and one broadcast cannot show a diff.
        # The first render after a scroll is a whole frame by design (the scroll
        # set `needs_full_render`), so the case needs several rounds before the
        # steady state it is about is even reached.
        f.c.drain(0.3)
        kinds = []
        for n in range(1, 7):
            f.c.send({"Input": {"data": list(
                b"printf 'CCC:%%s\\n' %d\n" % n)}})
            for _ in range(8):
                msgs = f.pump(0.25)
                kinds += [name_of(m) for m in msgs if name_of(m) in
                          ("FullRender", "RenderDiff", "ScrollRender")]
                if not msgs:
                    break
        assert kinds, "output beside a scrolled pane produced no frames at all"
        assert "RenderDiff" in kinds, (
            "every frame was a whole one while a non-focused pane was scrolled; "
            f"the per-client path is not diffing (frames: {kinds[:8]})")
        assert f.grid.marks_in("AAA", f.left) == a_scrolled, (
            "the diffed frames moved the scrolled pane: "
            f"{sorted(a_scrolled)[:3]}.. -> "
            f"{sorted(f.grid.marks_in('AAA', f.left))[:3]}..")
        assert f.grid.marks("CCC"), \
            "the focused pane's new output never reached the screen"
    except BaseException:
        f.close(failed=True)
        raise
    else:
        f.close()
    print("PASS output beside a scrolled pane is still diffed")


def case_a_focus_change_never_sends_a_scroll_delta():
    """A `ScrollRender` must never carry a delta taken across two panes.

    `ScrollRender` tells the client to SHIFT the pixels it already has by a
    number of rows. The server remembers the last offset it rendered at; if it
    forgot which PANE that offset belonged to, returning to a pane scrolled by
    6 from a pane scrolled by 3 would compute a delta of 3 and shift the first
    pane's rows by the second pane's movement, leaving the wrong three rows of
    history on screen.

    Asserted on the MESSAGE KIND, because that is what the guard decides, and
    then on the content, because a `FullRender` carrying the wrong cells would
    satisfy the kind check on its own. The deltas are deliberately small
    (`<= 10`, under the pane height) so that every other condition for a
    `ScrollRender` is satisfied and the pane identity is the only thing left
    that can refuse it.
    """
    f = Fixture(f"{RUNDIR_BASE}/deltapane")
    try:
        # Scroll the LEFT pane (focused) by 6, then the RIGHT one by 3.
        assert f.grid.focused_pane_rect["x"] == f.left["x"]
        f.c.send({"ScrollDelta": {"delta": 6}})
        f.pump(0.8)
        assert f.grid.scroll_offset == 6, \
            f"left pane offset={f.grid.scroll_offset}, expected 6"
        a_at_6 = f.grid.marks_in("AAA", f.left)
        assert a_at_6, "the left pane shows no AAA history at offset 6"

        f.focus("PaneFocusRight")
        f.c.send({"ScrollDelta": {"delta": 3}})
        f.pump(0.8)
        assert f.grid.scroll_offset == 3, \
            f"right pane offset={f.grid.scroll_offset}, expected 3"

        # Back to the left pane. Its offset is 6 and the last one rendered was
        # 3, so a server that ignored the pane would see a delta of +3 and shift.
        f.c.drain(0.3)
        f.c.send({"Command": "PaneFocusLeft"})
        msgs = f.pump(1.2)
        kinds = [name_of(m) for m in msgs if name_of(m) in
                 ("FullRender", "RenderDiff", "ScrollRender")]
        assert kinds, "returning to the scrolled pane produced no frame at all"
        assert "ScrollRender" not in kinds, (
            "a ScrollRender crossed a change of focused pane: the delta it "
            f"carries belongs to the other pane's scroll (frames: {kinds})")
        assert f.grid.scroll_offset == 6, (
            f"scroll_offset={f.grid.scroll_offset} on return, expected 6")
        assert f.grid.marks_in("AAA", f.left) == a_at_6, (
            "the left pane's rows moved across the focus round trip: "
            f"{sorted(a_at_6)[:3]}.. -> "
            f"{sorted(f.grid.marks_in('AAA', f.left))[:3]}..")
    except BaseException:
        f.close(failed=True)
        raise
    else:
        f.close()
    print("PASS a focus change never sends a cross-pane scroll delta")


def main():
    failures = []
    for case in (case_wheel_scrolls_the_pane_under_the_cursor,
                 case_scroll_survives_a_focus_round_trip,
                 case_typing_snaps_only_the_focused_pane,
                 case_a_focus_change_never_sends_a_scroll_delta,
                 case_output_beside_a_scrolled_pane_is_still_diffed,
                 case_click_to_focus_keeps_a_panes_scroll):
        try:
            case()
        except AssertionError as e:
            failures.append(f"{case.__name__}: {e}")
            print(f"FAIL {case.__name__}: {e}")
    if failures:
        print(f"\n{len(failures)} failure(s)")
        return 1
    print("\nALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
