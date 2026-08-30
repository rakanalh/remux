#!/usr/bin/env python3
"""The `files` sidebar plugin: a real program in a panel, following the cwd.

The panel is CLIENT-composited from a `PaneContent` snapshot, so only a real
PTY sees the result -- a frame-level harness sees the content rect alone and
would pass on a panel that never drew a glyph. Every test here runs with a
`[[sidebar]]` configured; with none, nothing is laid out and every assertion
below would be vacuous.

The file manager is a DETERMINISTIC STAND-IN, not a real `yazi`: a script that
prints its cwd and echoes the keys it is sent. What is under test is the
plumbing -- spawn, stream, route, re-target, reap -- not whether yazi renders.

What is covered:
  * the panel paints the aux pane's own output, and its header names the
    directory (asserted on the CONTENT, not on the panel's existence)
  * the aux pane starts in the FOCUSED pane's directory
  * a keystroke typed into the focused panel reaches the program
  * moving focus to a pane in another directory re-targets the panel, and
    moving focus within one directory does NOT restart it
  * the aux pane never appears in the session tree
  * killing the client reaps the aux pane -- asked of the OS, not the server
  * a `files` panel with no `command` is refused with a warning and spawns
    nothing

Run: python3 tests/pty/sidebar_files.py
"""
import os
import shutil
import subprocess
import sys
import time

import pexpect
import pyte

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUNDIR = "/tmp/rmx-sbf"
COLS, ROWS = 100, 30
SIDEBAR_W = 34
PREFIX = b"\x01"

# Short enough that `FM-HERE:<dir>` fits the panel interior on one row.
DIR_START = f"{RUNDIR}/start"
DIR_A = f"{RUNDIR}/alpha"
DIR_B = f"{RUNDIR}/bravo"
STANDIN = f"{RUNDIR}/standin.sh"
PIDFILE = f"{RUNDIR}/standin.pids"

FAILURES = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


def cfg(command='command = "STANDIN"'):
    return f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "files"
  {command.replace("STANDIN", STANDIN)}
"""


def make_env(config: str) -> dict:
    shutil.rmtree(RUNDIR, ignore_errors=True)
    for sub in ("run", "state", "data", "config"):
        os.makedirs(f"{RUNDIR}/{sub}", exist_ok=True)
    os.makedirs(f"{RUNDIR}/config/remux", exist_ok=True)
    os.makedirs(DIR_START, exist_ok=True)
    os.makedirs(DIR_A, exist_ok=True)
    os.makedirs(DIR_B, exist_ok=True)
    # The stand-in records its own PID so liveness can be asked of the OS.
    # `kill(pid, 0)` succeeds for a zombie, so /proc's state field is read
    # instead: a reap that leaves a `<defunct>` behind is still a leak.
    with open(STANDIN, "w") as fh:
        fh.write(
            f'#!/bin/sh\necho "$$" >> "{PIDFILE}"\n'
            'echo "FM-HERE:$(pwd)"\n'
            'while IFS= read -r line; do echo "FM-KEY:$line"; done\n'
        )
    os.chmod(STANDIN, 0o755)
    with open(f"{RUNDIR}/config/remux/config.toml", "w") as fh:
        fh.write(config)
    env = dict(os.environ)
    env.update(
        XDG_RUNTIME_DIR=f"{RUNDIR}/run",
        XDG_STATE_HOME=f"{RUNDIR}/state",
        XDG_DATA_HOME=f"{RUNDIR}/data",
        XDG_CONFIG_HOME=f"{RUNDIR}/config",
        SHELL="/bin/sh",
        ENV="/dev/null",
        TERM="xterm-256color",
        REMUX_ALLOW_NESTED="1",
        PS1="> ",
    )
    return env


def spawn(env, cols=COLS, rows=ROWS, cwd=None):
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, [], env=env, dimensions=(rows, cols), encoding=None, cwd=cwd)

    def pump(t=0.8):
        end = time.time() + t
        while time.time() < end:
            try:
                chunk = child.read_nonblocking(65536, 0.1)
            except Exception:
                continue
            stream.feed(chunk)

    return child, screen, pump


def teardown(child, env):
    try:
        child.close(force=True)
    except Exception:
        pass
    try:
        subprocess.run([BIN, "stop"], env=env, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=10)
    except Exception:
        pass


def panel_rows(screen):
    """The sidebar's columns of every row, frame glyphs stripped."""
    out = []
    for row in screen.display:
        cell = row[:SIDEBAR_W]
        out.append(cell.strip("│╭╮╰╯─├┤ "))
    return out


def panel_has(screen, want):
    return any(want in r for r in panel_rows(screen))


def wait_panel(pump, screen, want, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.4)
        if panel_has(screen, want):
            return True
    return False


def live_standins():
    """PIDs of stand-ins that have not been fully reaped.

    A zombie counts. `kill(pid, 0)` succeeds for one -- a `<defunct>` child keeps
    its pid and its name until somebody waits for it -- so "not in the process
    table under its old command name" is the WEAK check that passes on exactly
    the leak under test. The state field is read instead, and the two outcomes
    are reported separately so a failure says which one happened.
    """
    if not os.path.exists(PIDFILE):
        return []
    leaked = []
    for line in open(PIDFILE):
        line = line.strip()
        if not line:
            continue
        pid = int(line)
        try:
            stat = open(f"/proc/{pid}/stat").read()
        except OSError:
            continue  # gone: reaped, which is the outcome we want
        # `rsplit`, not `split`: `comm` is the only parenthesised field and
        # everything after it is fixed, so splitting from the RIGHT is exact
        # even for a process whose name contains `") "`.
        state = stat.rsplit(") ", 1)[1][0]
        leaked.append(f"{pid}{'(zombie)' if state == 'Z' else ''}")
    return leaked


def logs():
    out = ""
    for name in ("client.log", "server.log"):
        p = f"{RUNDIR}/state/remux/{name}"
        if os.path.exists(p):
            out += open(p, errors="replace").read()
    return out


def aux_pane_ids():
    """Aux pane ids, read out of the client's own log of what it was handed."""
    import re
    return [int(m) for m in re.findall(r"AuxPaneSpawned pane_id=(\d+) -> panel", logs())]


def session_tree_pane_ids():
    """Pane ids the SERVER reports in its tree, via a second wire client."""
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
    from harness import Client, name_of  # noqa: E402
    c = Client(f"{RUNDIR}/run/remux.sock")
    c.hello()
    c.send("ListSessionTree")
    ids = []
    for msg in c.drain(1.5):
        if name_of(msg) != "SessionTree":
            continue
        t = msg["SessionTree"]
        groups = list(t["unfiled"]) + [s for f in t["folders"] for s in f["sessions"]]
        for s in groups:
            for tab in s["tabs"]:
                ids += [p["id"] for p in tab["panes"]]
    c.close()
    return ids


# ---------------------------------------------------------------------------
# Test 1: the panel hosts the program, follows the focused pane's directory,
#         takes keys, stays out of the tree, and is reaped with the client.
# ---------------------------------------------------------------------------
env = make_env(cfg())
child, screen, pump = spawn(env, cwd=DIR_START)
try:
    pump(2.0)
    # The panel spawns as soon as it is laid out, in whatever directory the
    # focused pane is in at the time -- here the one the client started in.
    check(wait_panel(pump, screen, f"FM-HERE:{DIR_START}"),
          f"the panel paints the program's own output, from the focused pane's"
          f" directory\n      panel={panel_rows(screen)[:6]}")
    header = next((r for r in panel_rows(screen)[:3] if r.startswith("/")), "")
    check(header == DIR_START,
          f"the panel header names the directory\n      header={header!r}")

    # Ruling 3: the directory is followed at FOCUS-CHANGE granularity, not per
    # `cd`. A bare `cd` with focus unmoved must NOT restart the program -- doing
    # so would need a polling cwd watcher, which this deliberately does not have.
    child.send(f"cd {DIR_A}\r".encode())
    pump(2.0)
    check(panel_has(screen, f"FM-HERE:{DIR_START}"),
          f"a bare `cd` does not re-target the panel (focus-change granularity)"
          f"\n      panel={panel_rows(screen)[:6]}")

    # --- keys reach the program -------------------------------------------
    child.send(b"\x1bh")   # Alt+h: focus the left sidebar
    pump(0.8)
    child.send(b"q\r")     # the stand-in reads LINES, so the Enter matters
    check(wait_panel(pump, screen, "FM-KEY:q"),
          f"a keystroke typed into the focused panel reaches the program"
          f"\n      panel={panel_rows(screen)[:8]}")
    child.send(b"\x1b")    # Escape: back to the content area
    pump(0.6)

    # --- three tabs: two in DIR_A, one in DIR_B ---------------------------
    # Tabs, not splits: `Alt+<n>` is a deterministic jump, so which pane ends up
    # focused does not depend on the split geometry.
    child.send(PREFIX + b"tn")
    pump(1.5)
    child.send(f"cd {DIR_A}\r".encode())
    pump(1.0)
    child.send(PREFIX + b"tn")
    pump(1.5)
    child.send(f"cd {DIR_B}\r".encode())
    pump(1.0)

    child.send(b"\x1b1")   # tab 1 -- DIR_A
    check(wait_panel(pump, screen, f"FM-HERE:{DIR_A}", timeout=8.0),
          f"a focus change re-targets the panel to that pane's directory"
          f"\n      panel={panel_rows(screen)[:6]}")
    header = next((r for r in panel_rows(screen)[:3] if r.startswith("/")), "")
    check(header == DIR_A, f"the header follows too\n      header={header!r}")

    # --- the aux pane is invisible to the session tree ---------------------
    aux_ids = aux_pane_ids()
    check(aux_ids, "the client log records the aux pane ids it was given")
    tree_ids = session_tree_pane_ids()
    check(len(tree_ids) == 3, f"the session tree shows the three real panes (got {tree_ids})")
    check(not (set(aux_ids) & set(tree_ids)),
          f"no aux pane appears in the session tree (aux={aux_ids} tree={tree_ids})")

    # --- a focus move WITHIN one directory must not restart the program ----
    same_dir_pid = live_standins()
    check(len(same_dir_pid) == 1, f"exactly one program is running (got {same_dir_pid})")
    child.send(b"\x1b2")   # tab 2 -- also DIR_A
    pump(2.5)
    check(live_standins() == same_dir_pid,
          f"a focus move within one directory does not restart the program"
          f"\n      before={same_dir_pid} after={live_standins()}")

    # --- and a move to the other directory DOES re-target ------------------
    child.send(b"\x1b3")   # tab 3 -- DIR_B
    check(wait_panel(pump, screen, f"FM-HERE:{DIR_B}", timeout=8.0),
          f"moving to another directory re-targets the panel"
          f"\n      panel={panel_rows(screen)[:6]}")
    check(live_standins() != same_dir_pid,
          "the re-target really replaced the program")
    check(len(live_standins()) == 1,
          f"a re-target kills the old program rather than stacking them"
          f" (live={live_standins()})")

    # --- reap on client exit ----------------------------------------------
    child.close(force=True)
    end = time.time() + 6
    while time.time() < end and live_standins():
        time.sleep(0.1)
    check(not live_standins(),
          f"killing the client reaps the program (leftover={live_standins()})")

    check("panicked at" not in logs(), "no panic in either log")
finally:
    teardown(child, env)
    for entry in live_standins():
        try:
            os.kill(int(entry.split("(")[0]), 9)
        except OSError:
            pass

# ---------------------------------------------------------------------------
# Test 2: a `files` panel with no command is refused, loudly, and spawns
#         nothing.
# ---------------------------------------------------------------------------
env = make_env(cfg(command="# no command"))
child, screen, pump = spawn(env)
try:
    pump(2.5)
    log = logs()
    check("the `files` plugin requires a `command`" in log,
          "a files panel with no command is refused with a warning")
    check(not live_standins(), "and spawns nothing")
    check(child.isalive(), "the client is still alive with the panel skipped")
    check("panicked at" not in log, "no panic in either log")
finally:
    teardown(child, env)

# ---------------------------------------------------------------------------
# Test 3: a command that cannot run leaves a RESTARTABLE panel, not a permanent
#         "starting…".
# ---------------------------------------------------------------------------
env = make_env(cfg(command='command = "/nonexistent/not-a-file"'))
child, screen, pump = spawn(env, cwd=DIR_START)
try:
    pump(3.0)
    rows = panel_rows(screen)
    # The exited label is centred in the panel's content area, so slicing the
    # first few rows would hide the very thing under test.
    said = [r for r in rows if r]
    check(any("exited" in r for r in rows),
          f"a command that cannot run leaves the panel in its exited state"
          f"\n      panel={said}")
    check(not any("starting" in r for r in rows),
          f"...and not waiting on `starting…` for ever\n      panel={said}")
    check(child.isalive(), "the client survives a spawn that fails")
    check("panicked at" not in logs(), "no panic in either log")
finally:
    teardown(child, env)

print()
if FAILURES:
    print(f"{len(FAILURES)} FAILED:")
    for f in FAILURES:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
