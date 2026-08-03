"""The zoom wedge: a zoomed tab cycled into Monocle can never zoom out.

`Tab::effective_layout` obeys `zoomed_pane` over `layout_mode`, so while a zoom
is set the tab paints exactly one pane no matter which layout is selected.
`PaneToggleZoom` used to return early in Monocle ("already fullscreen"), which
made that state a one-way door: cycle a zoomed tab into Monocle and the zoom
can never be released. The zoom is *session* state, so every attached client on
every machine is stuck on the one pane, and both `zoomed_pane` and
`layout_mode` persist, so a save/restore carries the trap forward.

Three parts:

  cycle    the route in: zoom, then cycle the layout. The cycle must release
           the zoom (so the arrangement the user asked for is visible, and so
           no zoom can ever be parked in Monocle).
  restore  a session persisted in Monocle *while zoomed* -- the state a user
           already has on disk. Toggling zoom must free it.
  probe    a round-trip measured throughout, so "stuck" is proven to be state
           and not a wedged server.

Usage: python3 tests/frame/zoom_monocle_wedge.py
"""
import json, os, shutil, signal, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = "/tmp/rmxzw"
COLS, ROWS = 90, 24
FAILURES = []


class Frame:
    """The reconstructed client grid, fed from FullRender / RenderDiff."""

    def __init__(self, cols=COLS, rows=ROWS):
        self.g = [[" "] * cols for _ in range(rows)]

    def apply(self, msgs):
        for m in msgs:
            k = name_of(m)
            if k == "FullRender":
                for y, row in enumerate(m["FullRender"]["cells"]):
                    for x, cell in enumerate(row):
                        self.g[y][x] = cell["c"]
            elif k == "RenderDiff":
                for ch in m["RenderDiff"]["changes"]:
                    self.g[ch["y"]][ch["x"]] = ch["cell"]["c"]

    def row(self, y):
        return "".join(self.g[y])

    def status(self):
        return self.row(len(self.g) - 1)

    def zoomed(self):
        return "Z" in self.status()

    def layout(self):
        return self.status().split()[-1] if self.status().strip() else ""

    def boxes(self):
        """How many pane frames are painted (one top-left corner each)."""
        return sum(r.count("╭") for r in ("".join(x) for x in self.g))

    def strip_entries(self):
        """How many panes the Monocle strip lists on the tab's top border."""
        return self.row(0).count(" sh ")


def check(cond, label, extra=""):
    mark = "ok  " if cond else "FAIL"
    if not cond:
        FAILURES.append(label)
    print(f"  [{mark}] {label}{(' -- ' + extra) if extra else ''}")


def rtt(sock_path):
    """Round-trip a ListSessionTree on a fresh connection, in ms."""
    c = Client(sock_path)
    c.hello()
    t0 = time.time()
    c.send("ListSessionTree")
    out = None
    while time.time() - t0 < 5:
        try:
            m = c.recv()
        except Exception:
            break
        if name_of(m) == "SessionTree":
            out = (time.time() - t0) * 1000
            break
    c.close()
    return out


def attach(srv, name="main", create=True):
    c = Client(srv.sock)
    c.hello()
    if create:
        c.send({"CreateSession": {"name": name, "folder": None}})
    c.send({"Attach": {"session_name": name}})
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})
    time.sleep(0.9)
    f = Frame()
    f.apply(c.drain(0.7))
    return c, f


def cmd(c, f, name, t=0.7):
    c.send({"Command": name})
    f.apply(c.drain(t))


def part_cycle(srv, rtts):
    print("cycle: a layout change releases the zoom")
    c, f = attach(srv)
    cmd(c, f, "PaneNew", 1.2)
    cmd(c, f, "PaneNew", 1.2)
    check(f.boxes() == 3, "three panes painted", f"boxes={f.boxes()}")

    cmd(c, f, "PaneToggleZoom")
    check(f.zoomed() and f.boxes() == 1, "zoom in shows one pane",
          f"Z={f.zoomed()} boxes={f.boxes()}")
    rtts.append(rtt(srv.sock))

    # The route into the trap: cycle the layout while zoomed. The cycle must
    # release the zoom -- otherwise it silently parks a live zoom in Monocle
    # (and, pre-fix, every cycle step painted the identical single pane, so the
    # user got no feedback that anything had happened at all).
    cmd(c, f, "LayoutNext")
    check(not f.zoomed(), "cycling the layout releases the zoom",
          f"layout={f.layout()} Z={f.zoomed()}")
    check(f.boxes() == 3, "the new arrangement is visible", f"boxes={f.boxes()}")

    # Walk the rest of the automatic cycle; the zoom must never come back and
    # Monocle must list every pane in its strip.
    seen = {}
    for _ in range(4):
        cmd(c, f, "LayoutNext")
        seen[f.layout()] = (f.zoomed(), f.strip_entries(), f.boxes())
    check(all(not z for z, _, _ in seen.values()),
          "no cycle step re-takes the zoom", str(seen))
    mono = seen.get("monocle")
    check(mono is not None and mono[1] == 3,
          "the Monocle strip lists all three panes", f"monocle={mono}")
    rtts.append(rtt(srv.sock))

    # Zoom is redundant in Monocle, so taking one there is still refused.
    while f.layout() != "monocle":
        cmd(c, f, "LayoutNext")
    cmd(c, f, "PaneToggleZoom")
    check(not f.zoomed() and f.strip_entries() == 3,
          "Monocle still refuses a redundant zoom",
          f"Z={f.zoomed()} strip={f.strip_entries()}")
    c.close()


def part_restore(rtts):
    """A session already saved in Monocle *while zoomed* must be escapable."""
    print("restore: a persisted monocle+zoomed session can zoom out")
    rundir = RUNDIR + "b"
    shutil.rmtree(rundir, ignore_errors=True)
    srv = Server(rundir).start()
    c, f = attach(srv)
    cmd(c, f, "PaneNew", 1.2)
    cmd(c, f, "PaneNew", 1.2)
    for _ in range(5):
        if f.layout() == "monocle":
            break
        cmd(c, f, "LayoutNext")
    check(f.layout() == "monocle" and f.strip_entries() == 3,
          "saved in Monocle with all three panes", f"layout={f.layout()}")
    c.close()
    # SIGTERM makes the server save and exit cleanly.
    srv.proc.send_signal(signal.SIGTERM)
    srv.proc.wait(timeout=10)

    path = f"{rundir}/data/remux/state.json"
    if not os.path.exists(path):
        check(False, "persisted state.json exists", path)
        return
    # The only edit: park a zoom on the saved Monocle tab. That is precisely the
    # state the old `PaneToggleZoom` could reach by cycling the layout, and both
    # fields round-trip through the state file, so it survives a restart.
    with open(path) as fh:
        st = json.load(fh)
    tab = st["state"]["sessions"]["main"]["tabs"][0]
    check(tab["layout_mode"] == {"Monocle": None}, "the tab persisted as Monocle",
          str(tab["layout_mode"]))
    tab["zoomed_pane"] = tab["pane_order"][0]
    with open(path, "w") as fh:
        json.dump(st, fh)

    srv.proc = None
    srv2 = Server(rundir)
    srv2.rundir = rundir
    # Restart in place, keeping the patched state.json.
    import subprocess
    srv2.env = srv.env
    srv2.proc = subprocess.Popen(
        [os.environ.get("REMUX_BIN", os.path.abspath("target/debug/remux")), "server"],
        env=srv.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    for _ in range(200):
        if os.path.exists(srv2.sock):
            time.sleep(0.4)
            break
        time.sleep(0.05)

    c, f = attach(srv2, create=False)
    check(f.zoomed() and f.boxes() == 1 and f.strip_entries() < 3,
          "the restored session comes back stuck: one pane, zoom set",
          f"Z={f.zoomed()} layout={f.layout()} boxes={f.boxes()} "
          f"top={f.row(0)[:40]!r}")
    rtts.append(rtt(srv2.sock))
    cmd(c, f, "PaneToggleZoom")
    check(not f.zoomed(), "zoom out is accepted in Monocle", f"Z={f.zoomed()}")
    check(f.strip_entries() == 3, "all panes are reachable again",
          f"strip={f.strip_entries()}")
    rtts.append(rtt(srv2.sock))
    log = srv2.log()
    check("panic" not in log.lower(), "no panic in the server log")
    c.close()
    srv2.kill()


def main():
    shutil.rmtree(RUNDIR, ignore_errors=True)
    rtts = []
    srv = Server(RUNDIR).start()
    try:
        part_cycle(srv, rtts)
        log = srv.log()
        check("panic" not in log.lower(), "no panic in the server log")
    finally:
        srv.kill()
    part_restore(rtts)
    good = [r for r in rtts if r is not None]
    print(f"  round-trips (ms): {[f'{r:.1f}' for r in good]}")
    check(len(good) == len(rtts) and max(good) < 200,
          "the server answered every probe promptly")
    print("FAIL: " + ", ".join(FAILURES) if FAILURES else "PASS")
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())
