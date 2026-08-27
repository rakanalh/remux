#!/usr/bin/env python3
"""The server is told the CONTENT size, never the terminal size.

Design-doc assertions 8 and 12: a 30-column left sidebar on a 100x30 terminal
makes the server composite 70x30, and a terminal too narrow for the configured
sidebar still yields a non-zero content rect (the sidebar is clamped, then
force-hidden, rather than starving the server).

The client is driven under a real PTY -- only the client knows about sidebars,
so the negotiated size has to be observed on the SERVER side. `handle_resize`
logs `server: client_id=N resize cols=C rows=R` at debug, and the client always
logs at debug, so the server log is the observation point.

Run: python3 tests/frame/sidebar_resize.py
"""
import os
import re
import shutil
import subprocess
import sys
import time

import pexpect

BIN = os.path.abspath("target/debug/remux")
RUNDIR = "/tmp/rmx-sbr"

RESIZE_RE = re.compile(r"resize cols=(\d+) rows=(\d+)")


def make_env(config: str) -> dict:
    shutil.rmtree(RUNDIR, ignore_errors=True)
    for sub in ("run", "state", "data", "config"):
        os.makedirs(f"{RUNDIR}/{sub}", exist_ok=True)
    os.makedirs(f"{RUNDIR}/config/remux", exist_ok=True)
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


def negotiated_size(config: str, cols: int, rows: int):
    """Spawn a client at `cols`x`rows` and return the first size the server saw."""
    env = make_env(config)
    child = pexpect.spawn(BIN, [], env=env, dimensions=(rows, cols), encoding=None)
    deadline = time.time() + 6
    while time.time() < deadline:
        try:
            child.read_nonblocking(65536, 0.1)
        except Exception:
            pass
    log = f"{RUNDIR}/state/remux/server.log"
    body = open(log, errors="replace").read() if os.path.exists(log) else ""
    try:
        child.close(force=True)
    except Exception:
        pass
    subprocess.run(
        [BIN, "stop"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=10,
    )
    assert "panicked" not in body, f"server panicked:\n{body[-2000:]}"
    hits = RESIZE_RE.findall(body)
    assert hits, f"server never logged a resize:\n{body[-2000:]}"
    return int(hits[0][0]), int(hits[0][1])


def test_left_sidebar_shrinks_the_server_area():
    cfg = """
[[sidebar]]
edge = "left"
size = 30

  [[sidebar.panel]]
  plugin = "placeholder"
"""
    got = negotiated_size(cfg, 100, 30)
    assert got == (70, 30), f"expected the server to composite 70x30, got {got}"
    print("PASS test_left_sidebar_shrinks_the_server_area")


def test_no_sidebar_is_the_whole_terminal():
    got = negotiated_size("", 100, 30)
    assert got == (100, 30), f"baseline changed: {got}"
    print("PASS test_no_sidebar_is_the_whole_terminal")


def test_terminal_too_small_still_yields_a_usable_rect():
    # A 30-column sidebar on a 25-column terminal, plus a 20-row sidebar on an
    # 8-row one: neither fits. The geometry clamps each to whatever leaves
    # MIN_CONTENT_COLS (20) / MIN_CONTENT_ROWS (5) for the server, so the
    # server is still handed a usable rect rather than a starved or zero one.
    cfg = """
[[sidebar]]
edge = "left"
size = 30

  [[sidebar.panel]]
  plugin = "placeholder"

[[sidebar]]
edge = "bottom"
size = 20

  [[sidebar.panel]]
  plugin = "placeholder"
"""
    got = negotiated_size(cfg, 25, 8)
    cols, rows = got
    assert cols >= 20 and rows >= 5, f"content rect starved the server: {got}"
    assert cols <= 25 and rows <= 8, f"content rect exceeds the terminal: {got}"
    print(f"PASS test_terminal_too_small_still_yields_a_usable_rect (got {got})")


if __name__ == "__main__":
    if not os.path.exists(BIN):
        sys.exit(f"build first: {BIN} missing")
    test_no_sidebar_is_the_whole_terminal()
    test_left_sidebar_shrinks_the_server_area()
    test_terminal_too_small_still_yields_a_usable_rect()
    print("ALL PASS")
