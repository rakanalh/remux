#!/usr/bin/env python3
"""A view cell that sizes its pane TELLS the application (frame harness).

Since `ad13a6a` every visible cell demands its own interior, so a view resizes
panes far more often than it used to -- on subscribe, on a cell resize/move, on
a layout change, on zoom, on a terminal resize. That is only safe if the
application inside the pane is told, because a full-screen application renders
at its PTY size and repaints on `SIGWINCH`: a resize the app never learns about
leaves neovim drawing at the wrong size forever.

`Pty::resize` does the TIOCSWINSZ *and* an explicit `killpg(SIGWINCH)`, and
either one alone delivers the signal (the tty driver raises SIGWINCH on a size
change by itself), so asserting "a signal arrived" cannot distinguish a working
resize from a broken one. Assertion 4 is what makes this non-vacuous: the
application must read the NEW size, which only the ioctl can give it.

The demand only binds when the pane is NOT session-visible (`recompute_pane_size`
drops a view cell's demand for a pane its own session is driving), so the pane
under test is parked on a BACKGROUND tab -- the client stays attached, which is
what gets a live shell into the pane, but the active tab is another one. Every
assertion checks the size REALLY changed before checking the reaction, so none
of them can pass vacuously.

  1. a sized subscription resizes the pane to the cell
  2. re-subscribing at a new size resizes it again
  3. the shell's `trap ... WINCH` fires on that view-driven resize
  4. the application reads the NEW size (`stty size` from inside the trap)
  5. a pane on the ALTERNATE screen survives a view-driven resize with its own
     screen intact, and is told to repaint
  6. no panic in the server log

Run from the repo root:  python3 tests/frame/view_cell_sigwinch.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx_vcw"


class PaneWatch:
    """Latest `PaneContent` per pane: its size, and its text."""

    def __init__(self):
        self.size = {}
        self.text = {}

    def apply(self, msg):
        if name_of(msg) != "PaneContent":
            return
        b = msg["PaneContent"]
        self.size[b["pane_id"]] = (b["cols"], b["rows"])
        self.text[b["pane_id"]] = "\n".join(
            "".join(cell.get("c", " ") for cell in row) for row in b["cells"]
        )


def pump(c, watch, t=0.6):
    for m in c.drain(t):
        watch.apply(m)


def subscribe(c, pane_id, cols, rows):
    c.send({"SubscribePane": {"pane_id": pane_id, "cols": cols, "rows": rows,
                              "size_demand": True}})


def type_line(c, pane_id, line):
    c.send({"InputToPane": {"pane_id": pane_id, "data": list(line.encode())}})


def main():
    srv = Server(RUNDIR).start()
    results = []
    try:
        a = Client(srv.sock)
        a.hello()
        a.send({"CreateSession": {"name": "main", "folder": None}})
        a.send({"Attach": {"session_name": "main"}})
        a.send({"Resize": {"cols": 100, "rows": 30}})
        time.sleep(0.6)
        # Park the pane on a background tab. Attached is what gives it a live
        # shell; *visible* is what would make `recompute_pane_size` drop the
        # cell's demand and leave the pane at its session allotment forever.
        a.send({"Command": "TabNew"})
        time.sleep(0.6)
        a.drain(0.5)
        a.send("ListSessionTree")
        tree = None
        for m in a.drain(0.8):
            if name_of(m) == "SessionTree":
                tree = m["SessionTree"]
        sessions = list(tree["unfiled"]) + [s for f in tree["folders"]
                                            for s in f["sessions"]]
        panes = [p["id"] for s in sessions if s["name"] == "main"
                 for p in s["tabs"][0]["panes"]]
        assert panes, f"no pane in the session: {tree}"
        pane = panes[0]

        b = Client(srv.sock)
        b.hello()
        watch = PaneWatch()
        subscribe(b, pane, 60, 20)
        time.sleep(0.6)
        pump(b, watch, 0.6)
        results.append((
            "1. a sized subscription resizes the pane to the cell",
            watch.size.get(pane) == (60, 20),
            f"pane size: {watch.size.get(pane)}",
        ))

        # A stand-in for "the application handles SIGWINCH": the shell reports
        # the signal and what the terminal now measures.
        # The sentinel is split across a shell string concatenation so it
        # appears only in the shell's OUTPUT, never in the terminal-echoed
        # command line -- otherwise every assertion below would match the
        # typed text and pass without the signal ever arriving.
        type_line(b, pane, "trap 'echo WIN\"\"CH_OK; stty size' WINCH\n")
        time.sleep(0.6)
        pump(b, watch, 0.5)
        assert "WINCH_OK" not in watch.text.get(pane, ""), \
            "the trap must not have fired before the resize under test"

        subscribe(b, pane, 40, 14)
        time.sleep(1.0)
        pump(b, watch, 0.8)
        after = watch.text.get(pane, "")
        results.append((
            "2. re-subscribing at a new size resizes the pane again",
            watch.size.get(pane) == (40, 14),
            f"pane size: {watch.size.get(pane)}",
        ))
        results.append((
            "3. the shell's WINCH trap fires on a view-driven resize",
            "WINCH_OK" in after,
            f"WINCH_OK present={'WINCH_OK' in after}",
        ))
        results.append((
            "4. the application reads the NEW size from inside the trap",
            "14 40" in after,
            f"`stty size` reported: {[ln.strip() for ln in after.splitlines() if ln.strip() and ln.strip()[0].isdigit()][:2]}",
        ))

        # -- 5. The same, for a pane on the alternate screen: the case a view
        # cell aliasing neovim actually hits. `resize()` routes to
        # `resize_clamp` there, so the app's own screen must still be readable
        # afterwards -- and the app must be told, since only a repaint can fill
        # the rows the clamp left blank.
        type_line(b, pane, "printf '\\033[?1049h'\n")
        time.sleep(0.5)
        pump(b, watch, 0.4)
        type_line(b, pane, "for i in $(seq 1 8); do echo ALT_$i; done\n")
        time.sleep(0.8)
        pump(b, watch, 0.6)
        assert "ALT_8" in watch.text.get(pane, ""), \
            f"the stand-in never painted the alt screen: {watch.text.get(pane)!r}"
        type_line(b, pane, "trap 'echo ALTWIN\"\"CH_OK' WINCH\n")
        time.sleep(0.5)
        pump(b, watch, 0.4)
        subscribe(b, pane, 30, 12)
        time.sleep(1.0)
        pump(b, watch, 0.8)
        alt_after = watch.text.get(pane, "")
        results.append((
            "5. an alt-screen pane keeps its screen across a view-driven resize "
            "and is told to repaint",
            watch.size.get(pane) == (30, 12) and "ALT_" in alt_after
            and "ALTWINCH_OK" in alt_after,
            f"size={watch.size.get(pane)}, ALT_ kept={'ALT_' in alt_after}, "
            f"ALTWINCH_OK={'ALTWINCH_OK' in alt_after}",
        ))

        results.append(("6. no panic in the server log",
                        "panic" not in srv.log().lower(), ""))
    finally:
        srv.kill()

    ok = True
    for name, passed, detail in results:
        print(f"{'PASS' if passed else 'FAIL'}: {name}" + (f"  [{detail}]" if detail else ""))
        ok = ok and passed
    print("PASS: view cell sizing tells the app" if ok
          else "FAIL: view cell sizing tells the app")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
