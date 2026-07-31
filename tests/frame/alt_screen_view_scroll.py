#!/usr/bin/env python3
"""A view cell on the alt screen has nothing to scroll (frame harness).

The view path reads the SAME `Screen` as a session pane, so the alt-screen
scrollback bug showed up there too: the wheel over a cell whose pane was running
a full-screen application that does not grab the mouse (neovim without
`set mouse=a`, `less`) walked the cell into the PRIMARY screen's history.

Two cells are watched: one on the alternate screen (must not scroll into history
at all) and one plain shell (must keep scrolling exactly as before, so the guard
is not a blanket "views stop scrolling").

Assertions 5 and 6 FAIL before the fix: the full-screen app's redraws were pushed
into the pane's scrollback, so on leaving the alt screen the shell's own history
came back with 100 lines of the app's output wedged into the middle of it.

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
        results.append((
            "1. the alt-screen cell shows the application's own output",
            bool(watch.marks(alt, "ALT")) and not watch.marks(alt, "LINE"),
            f"ALT_ present={bool(watch.marks(alt, 'ALT'))}, "
            f"LINE_ present={bool(watch.marks(alt, 'LINE'))}",
        ))

        # -- 2/3. Wheel over the alt-screen cell: deep enough to walk past the
        # app's own redraws and into the primary screen's history.
        for _ in range(10):
            b.send({"ScrollPane": {"pane_id": alt, "up": True, "lines": 30,
                                   "x": 5, "y": 5}})
            time.sleep(0.1)
        time.sleep(0.4)
        pump(b, watch, 0.6)
        leaked = watch.marks(alt, "LINE")
        results.append((
            "2. the wheel does not walk an alt-screen cell into primary history",
            not leaked,
            f"LINE_ marks in the alt-screen cell: {sorted(leaked)[:8]}",
        ))
        results.append((
            "3. the alt-screen cell still shows the application, not history",
            bool(watch.marks(alt, "ALT")),
            f"ALT_ marks after the wheel: {bool(watch.marks(alt, 'ALT'))}",
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
        # Ctrl-C first: the wheel above was forwarded to the full-screen app as
        # arrow keys (the 8a85ca9 policy), and they land on the shell's input
        # line.
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
