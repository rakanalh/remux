"""`[appearance.theme] tab_style` draws end caps around a pane stack's tab chips.

`tab_style` is a CLIENT setting: the client sends it as `ClientMessage::TabStyle`
and the server draws that client's frames with it. The server's own config is
only the fallback for a client that sent none.

A two-pane stack (`alpha`, `beta`) is built with `PaneNew` +
`PaneStackIntoLeft`, and its tab strip is read off the reconstructed grid:

  * `rounded` + zellij borders: each chip is closed by U+E0B6 / U+E0B4, whose
    fg is the chip's own bg and whose bg is the border row's bg; the ` | `
    separator is gone;
  * `rounded` + tmux borders: the same on the 1-row tab bar, over its bg;
  * a click on the INACTIVE chip's RIGHT CAP (a column that only exists because
    of the caps) makes that tab active, which proves the hit ranges include
    the caps;
  * with no `tab_style` (the default, `plain`) the strip keeps ` | ` and has no
    cap glyphs;
  * a client that sends nothing is drawn with the server's `tab_style`, and one
    that sends `plain` overrides a `rounded` server;
  * re-sending a different style (what a config reload does) redraws;
  * two clients on ONE session with different styles each see their own: the
    style alone must keep them off a shared frame. Each also clicks the column
    that is the inactive chip's right cap in the ROUNDED layout, which only
    selects that tab for the client drawn rounded.

Chips are located by their label text and walked outwards over cells sharing
the label's bg, so no column is assumed from the Rust layout code.

Run: python3 tests/frame/tab_chip_style.py
"""
import os
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxtcs"
COLS, ROWS = 80, 24
LEFT_CAP, RIGHT_CAP = "", ""

fails = []


def check(cond, msg):
    if cond:
        print(f"  PASS {msg}")
    else:
        print(f"  FAIL {msg}")
        fails.append(msg)


class Grid:
    """The composited grid, keeping every cell's full attributes."""

    def __init__(self):
        blank = {"c": " ", "fg": "Default", "bg": "Default"}
        self.g = [[blank] * COLS for _ in range(ROWS)]

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    if y < ROWS and x < COLS:
                        self.g[y][x] = cell
        elif n == "RenderDiff":
            for ch in body["changes"]:
                if ch["y"] < ROWS and ch["x"] < COLS:
                    self.g[ch["y"]][ch["x"]] = ch["cell"]

    def text(self, y):
        return "".join(c["c"] for c in self.g[y])


def snapshot(cli):
    g = Grid()
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in cli.drain(0.8):
        g.apply(m)
    return g


def cmd(cli, c):
    cli.send({"Command": c})
    cli.drain(0.4)


def build_stack(cli):
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)
    cmd(cli, "PaneNew")
    cmd(cli, {"PaneRename": "beta"})
    cmd(cli, "PaneFocusLeft")
    cmd(cli, {"PaneRename": "alpha"})
    cmd(cli, "PaneFocusRight")
    cmd(cli, "PaneStackIntoLeft")


def chip(row, label):
    """`(left_cap_col, right_cap_col, chip_bg)` around the only `label` in row."""
    text = "".join(c["c"] for c in row)
    at = text.find(label)
    if at < 0 or text.find(label, at + 1) >= 0:
        return None
    bg = row[at]["bg"]
    l = at
    while l > 0 and row[l - 1]["bg"] == bg and row[l - 1]["c"] == " ":
        l -= 1
    r = at + len(label) - 1
    while r + 1 < len(row) and row[r + 1]["bg"] == bg and row[r + 1]["c"] == " ":
        r += 1
    return l - 1, r + 1, bg


def send_style(cli, style):
    cli.send({"TabStyle": {"style": style}})
    cli.drain(0.4)


def is_capped(text):
    return LEFT_CAP in text and RIGHT_CAP in text


def capped_phase(border_style, y):
    print(f"--- rounded (sent by the client), {border_style}")
    config = f'[appearance]\nborder_style = "{border_style}"\n'
    srv = Server(RUNDIR).start(config)
    cli = Client(srv.sock)
    try:
        cli.hello()
        send_style(cli, "rounded")
        build_stack(cli)
        g = snapshot(cli)
        row = g.g[y]
        print(f"  strip: {g.text(y)!r}")
        # The last column is the right corner (zellij) or the bar's own fill
        # (tmux); either way it wears the row's bg, which the caps must sit on.
        row_bg = row[COLS - 1]["bg"]
        chips = {}
        for label in ("alpha", "beta"):
            found = chip(row, label)
            check(found is not None, f"{label} is drawn once on the strip")
            if found is None:
                continue
            l, r, bg = found
            chips[label] = found
            check(row[l]["c"] == LEFT_CAP, f"{label} left cap at {l}: {row[l]['c']!r}")
            check(row[r]["c"] == RIGHT_CAP, f"{label} right cap at {r}: {row[r]['c']!r}")
            for col in (l, r):
                check(row[col]["fg"] == bg,
                      f"{label} cap {col} fg {row[col]['fg']} == chip bg {bg}")
                check(row[col]["bg"] == row_bg,
                      f"{label} cap {col} bg {row[col]['bg']} == row bg {row_bg}")
        check("|" not in g.text(y), "no ` | ` separator between capped chips")
        if len(chips) != 2:
            return
        check(chips["alpha"][2] != chips["beta"][2], "active and inactive chips differ")

        # The inactive chip wears tab_inactive_bg (Indexed 237 by default).
        inactive = [k for k, v in chips.items() if v[2] == {"Indexed": 237}]
        check(len(inactive) == 1, f"exactly one inactive chip ({inactive})")
        if len(inactive) != 1:
            return
        target = inactive[0]
        other = "beta" if target == "alpha" else "alpha"
        active_bg_before = chips[other][2]
        # The RIGHT cap: with plain chips that column is part of the separator
        # and hits nothing, so only a hit range that includes the caps passes.
        r = chips[target][1]
        cli.send({"MouseClick": {"x": r, "y": y, "pane_id": None, "release": False}})
        cli.send({"MouseClick": {"x": r, "y": y, "pane_id": None, "release": True}})
        cli.drain(0.5)
        after = snapshot(cli).g[y]
        t2, o2 = chip(after, target), chip(after, other)
        check(t2 is not None and t2[2] == active_bg_before,
              f"clicking {target}'s right cap made it active ({t2 and t2[2]})")
        check(o2 is not None and o2[2] == {"Indexed": 237},
              f"{other} is inactive after the click ({o2 and o2[2]})")
    finally:
        cli.close()
        srv.kill()
        log = srv.log()
        check("panicked" not in log, "no panic in server.log")


def plain_phase():
    print("--- default (plain), zellij_style")
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()
        build_stack(cli)
        g = snapshot(cli)
        top = g.text(0)
        print(f"  strip: {top!r}")
        check("alpha" in top and "beta" in top, "both labels drawn")
        check(" | " in top, "plain keeps the ` | ` separator")
        check(LEFT_CAP not in top and RIGHT_CAP not in top, "plain draws no caps")
    finally:
        cli.close()
        srv.kill()
        check("panicked" not in srv.log(), "no panic in server.log")


def fallback_phase():
    print("--- server rounded: silent client gets caps, `plain` client does not")
    srv = Server(RUNDIR).start('[appearance.theme]\ntab_style = "rounded"\n')
    cli = Client(srv.sock)
    try:
        cli.hello()
        build_stack(cli)
        top = snapshot(cli).text(0)
        print(f"  silent: {top!r}")
        check(is_capped(top), "a client that sent no style gets the server's")
        send_style(cli, "plain")
        top = snapshot(cli).text(0)
        print(f"  plain:  {top!r}")
        check(not is_capped(top) and " | " in top, "a client's `plain` overrides the server")
    finally:
        cli.close()
        srv.kill()
        check("panicked" not in srv.log(), "no panic in server.log")


def reload_phase():
    print("--- re-sending the style redraws (config reload)")
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()
        send_style(cli, "rounded")
        build_stack(cli)
        check(is_capped(snapshot(cli).text(0)), "rounded before the reload")
        cli.send({"TabStyle": {"style": "plain"}})
        g = Grid()
        for m in cli.drain(0.8):
            g.apply(m)
        top = g.text(0)
        print(f"  after:  {top!r}")
        check(not is_capped(top) and " | " in top,
              "the style change alone redraws the strip plain")
    finally:
        cli.close()
        srv.kill()
        check("panicked" not in srv.log(), "no panic in server.log")


def two_clients_phase():
    print("--- two clients, one session, different styles")
    srv = Server(RUNDIR).start()
    a, b = Client(srv.sock), Client(srv.sock)
    try:
        a.hello()
        send_style(a, "rounded")
        build_stack(a)
        b.hello()
        send_style(b, "plain")
        b.send({"Attach": {"session_name": "main"}})
        b.drain(0.5)
        ga, gb = Grid(), Grid()
        # One Resize reaches both: whichever client the server picks as the
        # shared frame's first, the other must still get its own.
        a.send({"Resize": {"cols": COLS, "rows": ROWS}})
        b.send({"Resize": {"cols": COLS, "rows": ROWS}})
        for m in a.drain(0.8):
            ga.apply(m)
        for m in b.drain(0.8):
            gb.apply(m)
        ta, tb = ga.text(0), gb.text(0)
        print(f"  rounded: {ta!r}")
        print(f"  plain:   {tb!r}")
        check(is_capped(ta) and "|" not in ta, "the rounded client sees caps")
        check(not is_capped(tb) and " | " in tb, "the plain client sees no caps")

        # Output in a pane drives the SHARED broadcast; both must keep their own.
        # The grids above are kept and no Resize is sent, so what is read is the
        # broadcast's diff on top of each client's previous frame. A Resize would
        # answer with a FullRender and pass whatever the broadcast did.
        a.send({"Input": {"data": list(b"echo tick\n")}})
        for m in a.drain(0.8):
            ga.apply(m)
        for m in b.drain(0.8):
            gb.apply(m)
        screen_a = "\n".join(ga.text(y) for y in range(ROWS))
        screen_b = "\n".join(gb.text(y) for y in range(ROWS))
        check("tick" in screen_a and "tick" in screen_b,
              "the broadcast reached both clients")
        check(is_capped(ga.text(0)), "rounded survives a broadcast")
        check(not is_capped(gb.text(0)) and " | " in gb.text(0),
              "plain survives a broadcast")

        found = chip(ga.g[0], "alpha"), chip(ga.g[0], "beta")
        inactive = [f for f in found if f and f[2] == {"Indexed": 237}]
        check(len(inactive) == 1, "one inactive chip on the rounded client")
        if len(inactive) != 1:
            return
        label = "alpha" if inactive[0] is found[0] else "beta"
        cap = inactive[0][1]
        # On the plain client the rounded right-cap column is not inside
        # `label`'s chip, so the same click must not select it there.
        pb = chip(gb.g[0], label)
        plain_hit = pb is not None and pb[0] < cap < pb[1]
        print(f"  {label} rounded cap col {cap}; plain chip span {pb and (pb[0], pb[1])}")
        check(not plain_hit, "the cap column is outside the plain chip (the case discriminates)")

        b.send({"MouseClick": {"x": cap, "y": 0, "pane_id": None, "release": False}})
        b.send({"MouseClick": {"x": cap, "y": 0, "pane_id": None, "release": True}})
        _, gb = snapshot_pair(a, b)
        after = chip(gb.g[0], label)
        check(after is not None and after[2] == {"Indexed": 237},
              f"the plain client's click there leaves {label} inactive")

        a.send({"MouseClick": {"x": cap, "y": 0, "pane_id": None, "release": False}})
        a.send({"MouseClick": {"x": cap, "y": 0, "pane_id": None, "release": True}})
        ga, _ = snapshot_pair(a, b)
        after = chip(ga.g[0], label)
        check(after is not None and after[2] != {"Indexed": 237},
              f"the rounded client's click on the cap makes {label} active")
    finally:
        a.close()
        b.close()
        srv.kill()
        check("panicked" not in srv.log(), "no panic in server.log")


def snapshot_pair(a, b):
    ga, gb = Grid(), Grid()
    a.send({"Resize": {"cols": COLS, "rows": ROWS}})
    b.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in a.drain(0.8):
        ga.apply(m)
    for m in b.drain(0.8):
        gb.apply(m)
    return ga, gb


def main():
    os.environ["HOME"] = RUNDIR
    capped_phase("zellij_style", 0)
    capped_phase("tmux_style", 0)
    plain_phase()
    fallback_phase()
    reload_phase()
    two_clients_phase()
    print()
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print(f"  - {f}")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
