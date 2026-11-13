"""Neovim scrolled inside a remux pane shows every line once, with its own text.

The harness drives real Neovim in a pane through the real client binary, so
the bytes go Neovim -> pane PTY -> server emulator -> frame -> client ->
terminal, and reads the client's screen with pyte. Neovim redraws rows as it
scrolls, and ends a redrawn row with an erase (EL, or ECH `CSI n X`) that
clears what is left of the row's old text. The file has `set number` on and
every third line empty. A STALE row is a numbered row whose text is not that
file line cut to the window's width: an erase that went missing leaves the old
line's tail after the new text, or the old text on an empty line, so the row
reads as a copy of a neighbour.

Each scenario runs on its own server:

  single  one remux pane, Neovim split with `:vsplit`, the left window read
  split   a remux vertical split, a shell printing in the left pane and
          Neovim with `:vsplit` in the focused right pane, its left window read
  pane    a remux vertical split, Neovim full width in the left pane (focused
          back with Alt-h) and a shell printing in the right pane
  remote-split, remote-pane
          `split` and `pane` on a REMOTE server: a second isolated server
          reached through an `ssh` shim that execs `remux relay`, attached from
          the client's sessions sidebar. Neovim, its PTY, the emulator and the
          compositor run on the remote; the frames cross the relay and the
          client draws them beside its sidebar

What each scenario covers:

  1  Neovim starts in the pane and draws numbered rows
  2  j past the bottom scrolls the window (its first row is no longer line 1)
  3  after every j, k, Ctrl-d and Ctrl-u has been drawn, every numbered row in
     the window shows exactly that file line, and the numbers are consecutive
  4  the client stays alive and neither log holds a panic

A key counts as drawn once the Neovim pane has changed and then reads the same
twice in a row. A pane that never changes (a lost key) or never settles fails
loudly rather than being read mid-redraw.

On Linux, Neovim emits ECH only in a window narrower than the screen, so only
the `:vsplit` scenarios (`single`, `split`, `remote-split`) go red when the
emulator drops ECH. `pane` and `remote-pane` cannot go red on Linux, because a
full-width window there ends its lines with EL. Neovim on macOS also uses ECH
in a full-width window; the `ech_*` unit tests in `src/screen.rs` cover that
byte shape. The `pane` scenarios guard the remux split itself: the server
compositor, the frame diffs, the relay and the client renderer with a
neighbour pane that keeps printing.

Every process the harness starts gets its HOME and XDG directories, cache
included, inside the run dir. Skipped (exit 0, with a SKIP line) when `nvim` is
not on PATH.

Run from the repo root: python3 tests/pty/nvim_scroll_ech.py [-v] [scenario]
"""
import atexit
import os
import re
import shutil
import subprocess
import sys
import time

import sidebar_sessions as remote_h
from pty_harness import BIN, Tui
from rundirs import run_dir  # noqa: E402

SCENARIOS = ("single", "split", "pane", "remote-split", "remote-pane")
COLS, ROWS = 120, 40
POLL = 0.1
CHANGE_WAIT = 2.0
SETTLE_TIMEOUT = 5.0
VERBOSE = "-v" in sys.argv

fails = []


def check(cond, msg):
    if VERBOSE or not cond:
        print(("  PASS  " if cond else "  FAIL  ") + msg)
    if not cond:
        fails.append(msg)


def write_file(path):
    lines = []
    for i in range(1, 301):
        lines.append("" if i % 3 == 0 else f"line {i} " + "x" * (i * 7 % 41))
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")
    return [line.rstrip() for line in lines]


# A window row: the pane's left border, the right-aligned number, then the text
# up to Neovim's vertical separator or the pane's right border.
ROW = re.compile(r"^│\s*(\d+) (.*?)│")


def left_window(t, x0):
    """`(number, text, width)` for each numbered row of the window at `x0`,
    where `width` is how many columns the window gives a line's text."""
    out = []
    for r in t.cell_rows()[:-1]:
        m = ROW.match(r[x0:])
        if m:
            out.append((int(m.group(1)), m.group(2).rstrip(), len(m.group(2))))
    return out


def verify(t, lines, label, x0):
    rows = left_window(t, x0)
    # `nowrap` cuts a line at the window's edge, so the row must equal the
    # line cut to that width: no more, no less, and blank only for a blank line.
    bad = [
        (n, text)
        for n, text, width in rows
        if n - 1 < len(lines) and text != lines[n - 1][:width].rstrip()
    ]
    nums = [n for n, _, _ in rows]
    gaps = [(a, b) for a, b in zip(nums, nums[1:]) if b != a + 1]
    check(len(rows) >= 20, f"{label}: the left window shows numbered rows ({len(rows)})")
    check(not bad, f"{label}: every row shows its own file line; stale: {bad[:3]}")
    check(not gaps, f"{label}: the row numbers are consecutive; gaps: {gaps[:3]}")
    return nums


class RemoteTui:
    """The `Tui` calls `run` makes, against a session on a fake remote.

    The client's config has the sessions sidebar and `[remotes.mini]`, whose
    `ssh` is a shim that execs `remux relay` against the second server, so
    the session `mac1` lives on the remote and is attached from the sidebar.
    """

    def __init__(self, name):
        self.envs = ()
        self._cleanup_registered = False
        self.rundir = run_dir(name)
        self.child = None

    def start(self):
        h, run = remote_h, self.rundir
        # Registered first, so a failure anywhere below still stops the two
        # servers and the relay. `kill` is safe to call more than once.
        if not self._cleanup_registered:
            atexit.register(self.kill)
            self._cleanup_registered = True
        # `setup_dirs`, the shim it writes, `base_env`'s PATH and `spawn` read
        # the module's RUN and BIN when called, so those two are set here.
        h.RUN, h.BIN = run, BIN
        h.setup_dirs()
        cfg = f"{run}/cfg-remote"

        def env(n, cfgdir):
            e = h.base_env(f"{run}/run{n}", f"{run}/state{n}", f"{run}/data{n}", cfgdir)
            e["XDG_CACHE_HOME"] = f"{run}/cache{n}"
            return e

        self.envs = (env("", cfg), env("2", f"{run}/cfg-none"))
        socks = (f"{run}/run/remux.sock", f"{run}/run2/remux.sock")
        for sock, e, name in zip(socks, self.envs, ("alpha", "mac1")):
            h.start_server(sock, e)
            w = h.Wire(sock)
            w.send({"CreateSession": {"name": name, "folder": None}})
            time.sleep(0.3)
            w.close()
        self.child, self.screen, self.pump = h.spawn(self.envs[0], COLS, ROWS)
        self.pump(3.0)
        self.send(b"\x1b2", 0.5)
        h.select_row(self.child, self.screen, self.pump, "mac1")
        self.send(b"\r", 2.5)
        return self

    def cell_rows(self):
        return remote_h.cell_rows(self.screen)

    def send(self, data, t=0.4):
        self.child.send(data.encode() if isinstance(data, str) else data)
        self.pump(t)

    def prefix(self, keys, t=0.4):
        self.send(b"\x01", 0.15)
        self.send(keys, t)

    def dump(self, label=""):
        print(f"----- screen {label} -----")
        for i, r in enumerate(self.cell_rows()):
            print(f"{i:2} |{r.rstrip()}")

    def alive(self):
        return self.child.isalive()

    def kill(self):
        envs, self.envs = self.envs, ()
        if self.child is not None:
            remote_h.teardown(self.child)
            self.child = None
        if not envs:
            return
        remote_h.kill_relays(self.rundir)
        for env in envs:
            try:
                subprocess.run([remote_h.BIN, "stop"], env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, timeout=10)
            except Exception:
                pass

    def log(self, which="client"):
        out = ""
        for state in ("state", "state2"):
            p = f"{self.rundir}/{state}/remux/{which}.log"
            if os.path.exists(p):
                out += open(p, errors="replace").read()
        return out


def pane_x(t, which):
    """The column of a pane's left border, read off the top border row.

    A sidebar's frame is on that row too, so only columns past it count.
    """
    skip = remote_h.SIDEBAR_W if isinstance(t, RemoteTui) else 0
    starts = [i for i, ch in enumerate(t.cell_rows()[0]) if ch == "╭" and i >= skip]
    return starts[0] if which == "left" else starts[-1]


def pane_region(t, x0):
    """The Neovim pane's interior, from its left border to its right one."""
    rows = t.cell_rows()
    right = rows[0].find("╮", x0)
    return [r[x0:right + 1] for r in rows[1:-2]]


def press(t, key, x0, label):
    """Send `key`, then wait until the Neovim pane has been redrawn.

    Every key the harness sends moves the cursor, and the ruler on Neovim's
    status line inside the pane shows the move, so the pane must change within
    `CHANGE_WAIT` and then read the same twice in a row. A pane that never
    changes means the key was lost, and one still changing after
    `SETTLE_TIMEOUT` is not something to read: both are failures.
    """
    before = pane_region(t, x0)
    t.send(key, 0)
    deadline = time.time() + CHANGE_WAIT
    while pane_region(t, x0) == before:
        if time.time() > deadline:
            check(False, f"{label}: the Neovim pane changed within {CHANGE_WAIT}s")
            return False
        t.pump(POLL)
    last = pane_region(t, x0)
    deadline = time.time() + SETTLE_TIMEOUT
    while True:
        t.pump(POLL)
        now = pane_region(t, x0)
        if now == last:
            return True
        if time.time() > deadline:
            check(False, f"{label}: the Neovim pane settled within {SETTLE_TIMEOUT}s")
            return False
        last = now


def run(scenario):
    before = len(fails)
    if scenario.startswith("remote-"):
        t = RemoteTui(f"rmxnvech-{scenario}")
    else:
        rundir = run_dir(f"rmxnvech-{scenario}")
        t = Tui(rundir, cols=COLS, rows=ROWS, extra_env={"XDG_CACHE_HOME": f"{rundir}/cache"})
    label = f"{scenario}:"
    t.start()
    try:
        if scenario.startswith("remote-"):
            check("mac1" in t.cell_rows()[-1], f"{scenario}: 0 the client shows the remote session")
        path = f"{t.rundir}/home/f.txt"
        lines = write_file(path)
        vsplit = "+vsplit" if scenario in ("single", "split", "remote-split") else ""
        nvim = f"nvim --clean -n -i NONE '+set nu so=4 cul nowrap' {vsplit} {path}\r"
        if scenario == "single":
            side = "left"
        elif scenario in ("split", "remote-split"):
            # The new pane takes focus; the old one keeps printing beside it.
            t.send("while :; do date; sleep 0.2; done\r", 0.3)
            t.prefix(b"pv", 1.0)
            side = "right"
        else:
            t.prefix(b"pv", 1.0)
            t.send("while :; do date; sleep 0.2; done\r", 0.3)
            t.send("\x1bh", 0.6)
            side = "left"
        t.send(nvim, 2.5)
        x0 = pane_x(t, side)

        nums = verify(t, lines, f"{label} 1 start", x0)
        check(bool(nums) and nums[0] == 1, f"{label} 1 the window starts at line 1 ({nums[:1]})")

        for i in range(50):
            press(t, "j", x0, f"{label} 3 j{i + 1}")
            verify(t, lines, f"{label} 3 j{i + 1}", x0)
        nums = left_window(t, x0)
        check(bool(nums) and nums[0][0] > 1, f"{label} 2 j scrolled the window ({nums[:1]})")

        for i in range(50):
            press(t, "k", x0, f"{label} 3 k{i + 1}")
            verify(t, lines, f"{label} 3 k{i + 1}", x0)
        for i in range(3):
            press(t, "\x04", x0, f"{label} 3 ctrl-d{i + 1}")
            verify(t, lines, f"{label} 3 ctrl-d{i + 1}", x0)
        for i in range(3):
            press(t, "\x15", x0, f"{label} 3 ctrl-u{i + 1}")
            verify(t, lines, f"{label} 3 ctrl-u{i + 1}", x0)

        if len(fails) > before:
            t.dump(f"{scenario} at failure")
        check(t.alive(), f"{label} 4 the client is alive")
    finally:
        t.kill()
    for which in ("client", "server"):
        check("panicked at" not in t.log(which), f"{label} 4 no panic in {which}.log")
    print(f"  {scenario}: {len(fails) - before} failure(s)")


def main():
    if shutil.which("nvim") is None:
        print("SKIP  nvim is not on PATH")
        return 0
    chosen = [a for a in sys.argv[1:] if a in SCENARIOS] or list(SCENARIOS)
    for scenario in chosen:
        run(scenario)
    print(f"{'FAIL' if fails else 'PASS'}  nvim_scroll_ech: {len(fails)} failure(s)")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
