#!/usr/bin/env python3
"""`remux split` / `remux new-tab`, and the pane identity they run on.

**This is the only test that can prove the environment variables are real.** The
whole point of `REMUX_SESSION`/`REMUX_PANE` is that `Pty::spawn` exports them
into a pane, so every command here is typed INTO an actual pane and reads the
variables the server put there -- nothing is injected by the harness. A
frame-level harness cannot see this at all: it never runs anything inside a
pane, so it would only ever be asserting against its own environment.

What is covered:

  1. A pane's shell really has `REMUX_SESSION` and `REMUX_PANE`, naming the
     session it is in and its own pane id.
  2. `remux split` typed in that pane splits it, focus lands on the new pane,
     and **the requested command actually runs** -- "a second pane appeared"
     passes just as well on a split running a plain login shell.
  3. Arguments after `--` survive the whole trip: CLI -> `CliSpawn` ->
     `create_pane_in_tab` -> `Pty::spawn`'s argv.
  4. `remux new-tab` makes a tab, not a split.
  5. `remux split` **outside** a pane refuses, names `$REMUX_SESSION`, and exits
     non-zero -- it must never guess a session, because guessing is how a script
     splits a window nobody was looking at.
  6. An **aux pane**'s `REMUX_PANE` names the pane it was spawned FOR, not
     itself. This is ruling 2, and it is the whole feature for a `files` user:
     it is what makes an opener hook land the editor beside the user's work
     rather than trying to subdivide a sidebar.

**A note on every marker below.** A shell ECHOES the line it is given, so a
marker that appears literally in the typed command is on screen whether or not
anything ran -- an assertion that passes on the echo of its own command is not
an assertion. So the markers here are always ASSEMBLED by the thing under test:
`printf 'RAN:%s' alpha` is typed, `RAN:alpha` is searched for, and the two
strings never coincide. This was not a hypothetical: the first version of this
file searched for a literal and stayed green with the feature entirely removed.

Run: python3 tests/pty/cli_split.py
"""
import os
import re
import shutil
import subprocess
import sys
import time

import pexpect
import pyte

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUNDIR = "/tmp/rmx-cls"
COLS, ROWS = 110, 34
SIDEBAR_W = 34
RUNNER = f"{RUNDIR}/runner.sh"

FAILURES = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


def make_env(config=None):
    shutil.rmtree(RUNDIR, ignore_errors=True)
    for sub in ("run", "state", "data", "config"):
        os.makedirs(f"{RUNDIR}/{sub}", exist_ok=True)
    if config is not None:
        os.makedirs(f"{RUNDIR}/config/remux", exist_ok=True)
        with open(f"{RUNDIR}/config/remux/config.toml", "w") as fh:
            fh.write(config)
    # The pane command under test. Its ARGUMENT is what proves argv survived the
    # trip, and it prints a string that does not appear in the command line that
    # invoked it. The trailing shell keeps the pane alive and typeable.
    with open(RUNNER, "w") as fh:
        fh.write('#!/bin/sh\nprintf "RAN:%s\\n" "$1"\nexec /bin/sh\n')
    os.chmod(RUNNER, 0o755)
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
    # The developer running this harness may well be sitting inside remux, in
    # which case their own REMUX_SESSION would leak in and make the refusal test
    # (5) assert against the wrong world -- it would find a session, split it,
    # and pass for entirely the wrong reason. Strip both.
    env.pop("REMUX_SESSION", None)
    env.pop("REMUX_PANE", None)
    return env


def spawn(env, cols=COLS, rows=ROWS):
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, [], env=env, dimensions=(rows, cols), encoding=None)

    def pump(t=0.8):
        end = time.time() + t
        while time.time() < end:
            try:
                chunk = child.read_nonblocking(65536, 0.1)
            except Exception:
                continue
            stream.feed(chunk)

    pump(1.5)
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


def has(screen, want):
    return any(want in r for r in screen.display)


def wait_for(pump, screen, want, timeout=10.0):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.4)
        if has(screen, want):
            return True
    return False


def dump(screen, label=""):
    print(f"----- screen {label} -----")
    for i, r in enumerate(screen.display):
        if r.strip():
            print(f"{i:2} |{r.rstrip()}")
    print("-------------------------")


def type_line(child, pump, line, t=0.6):
    child.send(line.encode() + b"\r")
    pump(t)


# Ask the focused pane who it is. The FORMAT string is typed and the ANSWER is
# assembled by the shell, so `ID:[main][3]` can only come from a pane that
# really has the variables.
IDENT_CMD = """printf 'ID:[%s][%s]\\n' "$REMUX_SESSION" "$REMUX_PANE" """
IDENT_RE = re.compile(r"ID:\[([^\]]*)\]\[([^\]]*)\]")


def read_ident(pump, screen, timeout=10.0):
    """The LAST `ID:[session][pane]` on screen, as a (session, pane) pair."""
    end = time.time() + timeout
    while time.time() < end:
        pump(0.4)
        found = None
        for row in screen.display:
            for m in IDENT_RE.finditer(row):
                found = (m.group(1), m.group(2))
        if found is not None:
            return found
    return None


# ---------------------------------------------------------------------------
# Phase 1 -- the variables, `remux split`, `remux new-tab`, and the refusal
# ---------------------------------------------------------------------------
env = make_env()
child, screen, pump = spawn(env)
try:
    # --- 1: the pane's own environment ------------------------------------
    type_line(child, pump, IDENT_CMD)
    ident = read_ident(pump, screen)
    check(ident is not None, "a pane's shell answers the identity query")
    if ident is None:
        dump(screen, "identity")
    check(ident is not None and ident[0] == "main",
          f"REMUX_SESSION names the pane's session (got {ident})")
    check(ident is not None and ident[1].isdigit(),
          f"REMUX_PANE is a pane id, not empty (got {ident})")
    orig_pane = ident[1] if ident else None

    # --- 2 + 3: split, with a command and its arguments -------------------
    # Absolute path: the pane's shell has no reason to have the build directory
    # on its PATH. `alpha` is the argument whose arrival is under test.
    type_line(child, pump, f"{BIN} split -- {RUNNER} alpha", t=1.2)
    check(wait_for(pump, screen, "RAN:alpha"),
          "`remux split -- <cmd> <arg>` runs the command WITH its argument")
    if not has(screen, "RAN:alpha"):
        dump(screen, "after split")

    # Focus follows the split, as it does for an interactive one. Asking the
    # focused pane who it is answers both questions at once: a DIFFERENT pane id
    # means the split exists and has focus, and a non-empty one means the pane
    # `create_pane_in_tab` made carries the identity too.
    type_line(child, pump, IDENT_CMD)
    split_ident = read_ident(pump, screen)
    check(split_ident is not None and split_ident[0] == "main",
          f"the pane `remux split` created carries REMUX_SESSION (got {split_ident})")
    check(split_ident is not None and orig_pane is not None
          and split_ident[1] != orig_pane,
          f"focus is on the NEW pane, which has its own id "
          f"(original={orig_pane}, focused={split_ident[1] if split_ident else None})")

    # --- 4: new-tab ---------------------------------------------------------
    type_line(child, pump, f"{BIN} new-tab -- {RUNNER} bravo", t=1.2)
    check(wait_for(pump, screen, "RAN:bravo"),
          "`remux new-tab` runs its command in the new tab's pane")
    # A new tab REPLACES what is on screen, so the split's output being gone is
    # the evidence that this was a tab and not a third split.
    check(not has(screen, "RAN:alpha"),
          "new-tab created a TAB, not another split (the old tab's output is gone)")
    if has(screen, "RAN:alpha"):
        dump(screen, "after new-tab")

    # --- 5: the refusal outside a pane -------------------------------------
    outside = subprocess.run([BIN, "split"], env=env, capture_output=True, timeout=20)
    check(outside.returncode != 0,
          f"`remux split` outside a pane exits non-zero (got {outside.returncode})")
    stderr = outside.stderr.decode(errors="replace")
    check("REMUX_SESSION" in stderr,
          f"the refusal names $REMUX_SESSION (stderr={stderr!r})")

    check(child.isalive(), "the client is still alive after all of it")
    log = open(f"{RUNDIR}/state/remux/server.log").read()
    check("panicked at" not in log, "no panic in the server log")
    clog_path = f"{RUNDIR}/state/remux/client.log"
    clog = open(clog_path).read() if os.path.exists(clog_path) else ""
    check("panicked at" not in clog, "no panic in the client log")
finally:
    teardown(child, env)


# ---------------------------------------------------------------------------
# Phase 2 -- ruling 2: an aux pane names the pane it was spawned FOR
# ---------------------------------------------------------------------------
STANDIN = f"{RUNDIR}/standin.sh"
CONFIG = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "files"
  command = "{STANDIN}"
"""

env2 = make_env(CONFIG)
# Written after make_env, which wipes RUNDIR. The stand-in prints the identity
# it was given and then sits still, exactly like the `files` stand-in elsewhere.
# It assembles `AUX:[...]` itself; nothing types that string.
with open(STANDIN, "w") as fh:
    fh.write('#!/bin/sh\n'
             'printf "AUX:[%s][%s]\\n" "$REMUX_SESSION" "$REMUX_PANE"\n'
             'while IFS= read -r line; do echo "K:$line"; done\n')
os.chmod(STANDIN, 0o755)

child2, screen2, pump2 = spawn(env2)
try:
    def panel_line(prefix):
        for row in screen2.display:
            cell = row[:SIDEBAR_W].strip("│╭╮╰╯─├┤ ")
            if cell.startswith(prefix):
                return cell
        return None

    end = time.time() + 10
    line = None
    while time.time() < end and line is None:
        pump2(0.4)
        line = panel_line("AUX:[")
    check(line is not None, "the files panel's aux pane printed its identity")
    if line is None:
        dump(screen2, "aux panel")

    check(line is not None and line.startswith("AUX:[main]["),
          f"an aux pane's REMUX_SESSION is the requesting client's session (got {line!r})")

    # The session's real pane is the FIRST pane the server ever minted, so it is
    # id 1; the aux pane is minted afterwards and cannot be. Asserting the exact
    # value is what makes this a test of ruling 2 rather than of "some number is
    # present" -- pointing REMUX_PANE at the aux pane itself would print a 2 here
    # and an `nnn` opener would then try to subdivide the sidebar.
    check(line == "AUX:[main][1]",
          f"an aux pane's REMUX_PANE names the pane it was spawned FOR, not itself "
          f"(got {line!r}, want 'AUX:[main][1]')")

    # And the whole thing still works in the configuration a `files` user is
    # actually in: a sidebar up, splitting from the real pane beside it.
    child2.send(b"\x1bl")  # Alt+l: into the pane, if focus happens to be in the sidebar
    pump2(0.5)
    child2.send(f"{BIN} split -- {RUNNER} charlie".encode() + b"\r")
    check(wait_for(pump2, screen2, "RAN:charlie", timeout=12),
          "`remux split` works from a pane while a sidebar is up")
    if not has(screen2, "RAN:charlie"):
        dump(screen2, "sidebar split")

    check(child2.isalive(), "the client survived the sidebar phase")
    log2 = open(f"{RUNDIR}/state/remux/server.log").read()
    check("panicked at" not in log2, "no panic in the server log (phase 2)")
finally:
    teardown(child2, env2)

print()
if FAILURES:
    print(f"{len(FAILURES)} FAILED:")
    for f in FAILURES:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
