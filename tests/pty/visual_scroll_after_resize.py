"""Visual mode must reach the OLDEST line after a height change (real PTY).

The user's exact gesture, and the one the frame harness cannot test: `Prefix+v`
then `gg`. `gg` is `VisualState::jump_to_top`, whose bound is the CLIENT-side
`total_lines` from `ScrollbackInfo` -- so this drives the whole reporting chain,
from `Screen::total_lines()` on the server through the wire to the bound the
client actually scrolls against. If the two disagree, `gg` stops short of the
oldest line and the pane looks like it has no history, which is what was
reported: "visual mode scroll up does not work beyond the top of the pane's
first line, though there is definitely something above".

The height change is the trigger. A resize that changes only the row count used
to take `Screen::resize_clamp`, which keeps the TOP `rows` of the grid -- so
shrinking the height deleted the bottom rows outright, without them ever
reaching scrollback. Zooming a full-width pane and shrinking the terminal window
both do exactly that.

Asserted:
  1. `gg` reaches LINE_1 on an untouched pane (the control).
  2. It still reaches LINE_1 after a height-only resize round trip.
  3. It still reaches LINE_1 after an alt-screen round trip that has a
     height-only resize in the middle of it -- and no alt-screen output leaked
     into the history.

Assertions 2 and 3 FAIL before the fix.

Run from the repo root:  python3 tests/pty/visual_scroll_after_resize.py
"""
import re
import sys

from pty_harness import Tui

RUNDIR = "/tmp/rmxfix/vsar"
COLS, TALL, SHORT = 120, 40, 24
NLINES = 300


def line_nums(t, prefix="LINE"):
    nums = set()
    for r in t.rows_text():
        for m in re.finditer(rf"{prefix}_(\d+)", r):
            nums.add(int(m.group(1)))
    return nums


def sweep(t, results, label):
    """Enter Visual mode, `gg` to the top, then page back down collecting every
    marker seen. Returns (oldest LINE_ reached, all LINE_ seen, all ZOOM_ seen,
    all ALT_ seen)."""
    for attempt in range(3):
        t.prefix(b"v", 0.6)
        t.pump(0.4)
        if t.has("VISUAL"):
            break
        t.send("\x1b", 0.3)
    else:
        results.append((f"{label}: entered Visual mode", False,
                        "no VISUAL indicator after 3 attempts"))
        return None, set(), set(), set()

    t.pump(0.5)  # let RequestScrollbackInfo round-trip
    lines, zooms, alts = set(), set(), set()

    def grab():
        lines.update(line_nums(t, "LINE"))
        zooms.update(line_nums(t, "ZOOM"))
        alts.update(line_nums(t, "ALT"))

    grab()
    t.send("gg", 0.8)
    grab()
    oldest = min(lines) if lines else None
    # Page back down to the live tail, collecting everything in between.
    for _ in range(40):
        t.send("\x04", 0.12)  # Ctrl-d, half page down
        grab()
    # ONE Escape: it leaves Visual mode. A second one would reach the shell,
    # where readline swallows the next keystroke as part of an ESC sequence
    # (`ESC f` = forward-word) and eats the first letter of whatever is typed
    # next.
    t.send("\x1b", 0.6)
    return oldest, lines, zooms, alts


def shell(t, cmd, settle=1.5):
    """Type a command at a known-clean prompt."""
    t.send("\r", 0.4)
    t.send(cmd + "\r", settle)


def main():
    results = []
    t = Tui(RUNDIR, cols=COLS, rows=TALL).start()
    try:
        t.send("clear\r", 0.4)
        t.send(f"for i in $(seq 1 {NLINES}); do echo LINE_$i; done\r", 2.2)
        t.pump(1.0)

        # -- 1. Control: an untouched pane.
        oldest, _, _, _ = sweep(t, results, "1. untouched pane")
        results.append((
            "1. untouched pane: `gg` reaches the oldest line",
            oldest == 1,
            f"oldest LINE_ reached = {oldest} (want 1)",
        ))

        # -- 2. A height-only resize round trip (the zoom analogue: a pane that
        # already spans the full width changes only its row count when zoomed).
        # The ZOOM_ lines are printed while TALL, so they are sitting in the
        # lower rows of the GRID -- exactly the rows a keep-the-top clamp drops.
        shell(t, "for i in $(seq 1 30); do echo ZOOM_$i; done")
        t.resize(COLS, SHORT)
        t.resize(COLS, TALL)
        t.pump(0.6)
        oldest, _, zooms, _ = sweep(t, results, "2. after a height-only resize")
        missing = sorted(set(range(1, 31)) - zooms)
        results.append((
            "2a. a height-only resize deletes no output",
            not missing,
            f"{len(zooms & set(range(1, 31)))}/30 ZOOM_ lines survived; deleted: {missing[:12]}",
        ))
        results.append((
            "2b. `gg` still reaches the oldest line afterwards",
            oldest == 1,
            f"oldest LINE_ reached = {oldest} (want 1)",
        ))

        # -- 3. The same, but with a full-screen application up across it.
        shell(t, "printf '\\033[?1049h'", 0.9)
        shell(t, "for i in $(seq 1 60); do echo ALT_$i; done")
        t.resize(COLS, SHORT)
        t.resize(COLS, TALL)
        t.send("\x03", 0.5)
        shell(t, "printf '\\033[?1049l'", 1.0)
        t.pump(0.6)
        oldest, _, _, alts = sweep(t, results, "3. after an alt-screen round trip")
        results.append((
            "3a. `gg` reaches the oldest line after an alt round trip + resize",
            oldest == 1,
            f"oldest LINE_ reached = {oldest} (want 1)",
        ))
        results.append((
            "3b. no alt-screen output leaked into the primary history",
            not alts,
            f"ALT_ marks in the history: {sorted(alts)[:8]}",
        ))

        alive = t.alive()
        panic = ("panic" in t.log("client").lower()
                 or "panic" in t.log("server").lower())
        results.append(("4. client still alive, no panic", alive and not panic,
                        f"alive={alive} panic={panic}"))
        if not all(p for _, p, _ in results):
            t.dump("final")
    finally:
        t.kill()

    ok = True
    for name, passed, detail in results:
        print(f"{'PASS' if passed else 'FAIL'}: {name}" + (f"  [{detail}]" if detail else ""))
        ok = ok and passed
    print("PASS: visual scroll after resize" if ok else "FAIL: visual scroll after resize")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
