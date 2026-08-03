#!/usr/bin/env python3
"""A view cell on the alt screen has nothing to scroll (frame harness).

The view path reads the SAME `Screen` as a session pane, so the alt-screen
scrollback bug showed up there too: the wheel over a cell whose pane was running
a full-screen application that does not grab the mouse (neovim without
`set mouse=a`, `less`) walked the cell into the PRIMARY screen's history.

Two cells are watched: one on the alternate screen (must not scroll into history
at all) and one plain shell (must keep scrolling exactly as before, so the guard
is not a blanket "views stop scrolling").

The alt-screen cell's stand-in application must actually behave like one. A
non-tracking alt-screen wheel is forwarded to the app as arrow keys
(`alt_scroll_arrows`, the 8a85ca9 routing policy), so leaving the pane sitting at
a shell prompt hands 300 up-arrows to readline: it walks the machine's real
`~/.bash_history` and redraws recalled lines of varying width, scrolling the alt
grid by an amount that depends on what is in that file. That made "the app's
screen is still there" a coin flip on an input no test controls. The stand-in
therefore hands its screen to a reader that neither echoes nor line-edits
(`stty -echo -icanon; cat`), which is what a full-screen application does with a
key it has no binding for: nothing. The cell's content is then asserted
BYTE-IDENTICAL across the gesture.

The wheel is applied twice, because a view cell reaches the alt screen by two
different routes and only one of them is remux's own scrolling:
  - in NORMAL mode `mouse_route` returns `AltArrows`, so the app gets the keys
    and remux must not repaint the cell at all;
  - in VISUAL mode the user has claimed the mouse, so the wheel takes
    `MouseRoute::Remux` and is clamped by `Screen::max_scroll_offset()` -- the
    value 4918c65 makes zero on the alternate screen. This leg is what fails if
    that guard is reverted. It is a real state: `ModeChanged` is sent to the
    FOREGROUND connection, which is the one hosting a local cell's pane.

Note for whoever touches the wheel next: the `ScrollPane` handler destructures
`MouseRoute::Remux { .. }` and ignores the `scrollback` flag, so the VISUAL leg
detects a reverted guard only because `max_scroll_offset() == 0` is the single
thing stopping the scroll. If that call site is ever made to honour
`scrollback: false`, this leg goes green with the guard reverted and quietly
stops being load-bearing -- give it a different fault to chase then.

Assertions 2, 3, 5 and 6 FAIL before the fix: the full-screen app's redraws were
pushed into the pane's scrollback, so the wheel walked the cell out of the
application and into unrelated text, and on leaving the alt screen the shell's
own history came back with 100 lines of the app's output wedged into the middle
of it.

Run from the repo root:  python3 tests/frame/alt_screen_view_scroll.py
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx_asv"
COLS, ROWS = 100, 30
CELL_COLS, CELL_ROWS = 40, 14


class PaneWatch:
    """Latest `PaneContent` per pane, as text."""

    def __init__(self):
        self.text = {}

    def apply(self, msg):
        if name_of(msg) != "PaneContent":
            return
        b = msg["PaneContent"]
        self.text[b["pane_id"]] = "\n".join(
            "".join(cell.get("c", " ") for cell in row) for row in b["cells"]
        )

    def marks(self, pid, prefix="LINE"):
        return {int(m) for m in re.findall(rf"{prefix}_(\d+)", self.text.get(pid, ""))}


def pump(c, watch, t=0.5):
    msgs = c.drain(t)
    for m in msgs:
        watch.apply(m)
    return len(msgs)


def main():
    srv = Server(RUNDIR).start()
    results = []
    try:
        a = Client(srv.sock)
        a.hello()
        a.send({"CreateSession": {"name": "main", "folder": None}})
        a.send({"Attach": {"session_name": "main"}})
        a.send({"Resize": {"cols": COLS, "rows": ROWS}})
        time.sleep(0.3)
        a.send({"Command": "PaneSplitVertical"})
        time.sleep(0.5)
        a.drain(0.5)
        a.send("ListSessionTree")
        tree = None
        for m in a.drain(0.8):
            if name_of(m) == "SessionTree":
                tree = m["SessionTree"]
        panes = []
        for sess in tree["unfiled"] + [s for f in tree["folders"] for s in f["sessions"]]:
            if sess["name"] == "main":
                panes = [p["id"] for p in sess["tabs"][0]["panes"]]
        assert len(panes) == 2, f"expected 2 panes, got {panes}"
        alt, plain = panes[0], panes[1]
        print("panes:", {"alt": alt, "plain": plain})

        # A view client: subscribed, never attached.
        b = Client(srv.sock)
        b.hello()
        watch = PaneWatch()
        for pid in panes:
            b.send({"SubscribePane": {"pane_id": pid, "cols": CELL_COLS,
                                      "rows": CELL_ROWS, "size_demand": True}})
        time.sleep(0.5)
        pump(b, watch, 0.6)

        # Primary history in both cells, then one of them goes full-screen (no
        # mouse tracking -- the case 8a85ca9 could not cover) and redraws a lot.
        for pid in panes:
            b.send({"InputToPane": {"pane_id": pid,
                                    "data": list(b"for i in $(seq 1 150); do echo LINE_$i; done\n")}})
        time.sleep(1.5)
        pump(b, watch, 0.8)
        live_plain = watch.marks(plain)
        b.send({"InputToPane": {"pane_id": alt,
                                "data": list(b"printf '\\033[?1049h'\n")}})
        time.sleep(0.5)
        pump(b, watch, 0.4)
        b.send({"InputToPane": {"pane_id": alt,
                                "data": list(b"for i in $(seq 1 100); do echo ALT_$i; done\n")}})
        time.sleep(1.2)
        pump(b, watch, 0.8)
        # From here the pane owns its screen the way a real application does:
        # keys are consumed, nothing is echoed, nothing is redrawn. Without this
        # the forwarded arrows land on readline and IT rewrites the screen, so
        # the assertions below would be measuring bash, not remux.
        b.send({"InputToPane": {"pane_id": alt,
                                "data": list(b"stty -echo -icanon min 1 time 0; cat > /dev/null\n")}})
        time.sleep(1.0)
        pump(b, watch, 0.8)
        results.append((
            "1. the alt-screen cell shows the application's own output",
            bool(watch.marks(alt, "ALT")) and not watch.marks(alt, "LINE"),
            f"ALT_ present={bool(watch.marks(alt, 'ALT'))}, "
            f"LINE_ present={bool(watch.marks(alt, 'LINE'))}",
        ))
        painted = watch.text[alt]

        # -- 2/3. Wheel over the alt-screen cell, in both modes: deep enough to
        # walk past the app's own redraws and into the primary screen's history
        # if anything let it. NORMAL forwards the gesture to the app; VISUAL is
        # the one that reaches remux's own scrolling, clamped by
        # `max_scroll_offset()`.
        def wheel_alt():
            for _ in range(10):
                b.send({"ScrollPane": {"pane_id": alt, "up": True, "lines": 30,
                                       "x": 5, "y": 5}})
                time.sleep(0.1)
            time.sleep(0.4)
            pump(b, watch, 0.6)

        wheel_alt()
        normal_leaked, normal_text = watch.marks(alt, "LINE"), watch.text[alt]
        b.send({"ModeChanged": {"mode": "VISUAL"}})
        time.sleep(0.3)
        pump(b, watch, 0.3)
        wheel_alt()
        visual_leaked, visual_text = watch.marks(alt, "LINE"), watch.text[alt]
        # Park the cell back at the live view while remux still owns the wheel.
        # A no-op when the guard holds (the offset never left 0), but without it
        # a broken build leaves the cell pinned at the top of a history it
        # should not have, and assertions 5/6 below would scroll from there and
        # never reach the region they are looking at.
        b.send({"ScrollPane": {"pane_id": alt, "up": False, "lines": 1000,
                               "x": 5, "y": 5}})
        time.sleep(0.4)
        pump(b, watch, 0.4)
        b.send({"ModeChanged": {"mode": "NORMAL"}})
        time.sleep(0.3)
        pump(b, watch, 0.3)
        results.append((
            "2. the wheel does not walk an alt-screen cell into primary history",
            not normal_leaked and not visual_leaked,
            f"LINE_ marks in the alt-screen cell: normal={sorted(normal_leaked)[:8]}, "
            f"visual={sorted(visual_leaked)[:8]}",
        ))
        results.append((
            "3. the alt-screen cell still shows the application, not history",
            normal_text == painted and visual_text == painted,
            f"cell unchanged by the wheel: normal={normal_text == painted}, "
            f"visual={visual_text == painted}",
        ))

        # -- 4. The plain cell still scrolls its own scrollback.
        b.send({"ScrollPane": {"pane_id": plain, "up": True, "lines": 10,
                               "x": 5, "y": 5}})
        time.sleep(0.4)
        pump(b, watch, 0.6)
        scrolled_plain = watch.marks(plain)
        results.append((
            "4. the wheel still scrolls a plain cell",
            bool(scrolled_plain) and bool(live_plain)
            and min(scrolled_plain) < min(live_plain),
            f"top LINE {min(live_plain) if live_plain else None} -> "
            f"{min(scrolled_plain) if scrolled_plain else None}",
        ))

        # -- 5/6. Leaving the alt screen hands the pane's own history back, and
        # that history must be the shell's -- not the shell's with 100 lines of
        # the full-screen app's redraw wedged into the middle of it, which is
        # what the missing guard produced.
        # Ctrl-C first, to end the reader standing in for the application and
        # get the shell's prompt back.
        b.send({"InputToPane": {"pane_id": alt, "data": [0x03]}})
        time.sleep(0.3)
        pump(b, watch, 0.3)
        b.send({"InputToPane": {"pane_id": alt,
                                "data": list(b"printf '\\033[?1049l'\n")}})
        time.sleep(0.7)
        pump(b, watch, 0.6)
        seen_line, seen_alt = set(), set()
        for _ in range(8):
            b.send({"ScrollPane": {"pane_id": alt, "up": True, "lines": 10,
                                   "x": 5, "y": 5}})
            time.sleep(0.25)
            pump(b, watch, 0.4)
            seen_line |= watch.marks(alt, "LINE")
            seen_alt |= watch.marks(alt, "ALT")
        results.append((
            "5. after leaving the alt screen the cell scrolls its real history",
            bool(seen_line) and min(seen_line) < 100,
            f"LINE_ range seen while scrolling back: "
            f"{min(seen_line) if seen_line else None}..{max(seen_line) if seen_line else None}",
        ))
        results.append((
            "6. the app's alt-screen output never entered the pane's history",
            not seen_alt,
            f"ALT_ marks wedged into the restored history: {sorted(seen_alt)[:8]}",
        ))

        results.append(("7. no panic in the server log",
                        "panic" not in srv.log().lower(), ""))
    finally:
        srv.kill()

    ok = True
    for name, passed, detail in results:
        print(f"{'PASS' if passed else 'FAIL'}: {name}" + (f"  [{detail}]" if detail else ""))
        ok = ok and passed
    print("PASS: alt-screen view cell scroll" if ok else "FAIL: alt-screen view cell scroll")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
