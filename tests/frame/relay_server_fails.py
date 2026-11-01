#!/usr/bin/env python3
"""A relay whose server cannot start says why, inside the dialler's deadline.

The relay starts the far machine's server when none is running and waits for
its socket. A server that cannot start used to leave the relay waiting out its
whole 5 s, and a dialler with a 3 s `dial_ms` gave up first, so the sender saw
only "dial timed out after 3s" and the cause stayed in the far machine's
relay.log. Two ways B's server cannot start, each run both ways:

  ro    B's runtime dir is 0500, the live case: the server cannot bind there
  long  B's socket path is past sun_path's 108 bytes

  1  `remux relay` alone: it fails within 2 s, exits non-zero, and its stderr
     says the server exited before its socket appeared
  2  through a dial (fake ssh, dial_ms 3000): the send fails route_unavailable,
     and the error carries the relay's cause rather than only the timeout
  3  no panic in A's log

SKIPs as root, whom a 0500 directory does not stop.

Run: python3 tests/frame/relay_server_fails.py   (RMX_RUNDIR overrides the run dir)
"""
import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, PROTOCOL_VERSION, Client, Server  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-relayfail")
CAUSE = "exited before its socket appeared"

failures = []
read_only = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def b_env(kind):
    """B's login environment, with a runtime dir B's server cannot bind in."""
    b = f"{RUNDIR}/b-{kind}"
    run = f"{b}/run" if kind == "ro" else f"{b}/run/" + "r" * 110
    for d in (run, f"{b}/state", f"{b}/data", f"{b}/config", f"{b}/home"):
        os.makedirs(d, exist_ok=True)
    os.chmod(run, 0o500 if kind == "ro" else 0o700)
    if kind == "ro":
        read_only.append(run)
    env = {k: v for k, v in os.environ.items() if k not in ("REMUX_SESSION", "REMUX_PANE")}
    env.update(XDG_RUNTIME_DIR=run, XDG_STATE_HOME=f"{b}/state", XDG_DATA_HOME=f"{b}/data",
               XDG_CONFIG_HOME=f"{b}/config", HOME=f"{b}/home", SHELL="/bin/sh")
    return env


def relay_alone(kind, env):
    t0 = time.time()
    p = subprocess.run([BIN, "relay"], env=env, stdin=subprocess.DEVNULL, capture_output=True,
                       timeout=30)
    took = time.time() - t0
    err = p.stderr.decode(errors="replace").strip()
    check(p.returncode != 0 and CAUSE in err and took < 2.0,
          f"1 {kind}: the relay fails fast and says why (rc={p.returncode}, {took:.1f}s, err={err!r})")


def through_a_dial(kind, env):
    shimdir = f"{RUNDIR}/shim-{kind}"
    os.makedirs(shimdir, exist_ok=True)
    with open(f"{shimdir}/ssh", "w") as f:
        f.write("#!/bin/sh\n")
        for k in ("XDG_RUNTIME_DIR", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "HOME"):
            f.write(f"export {k}='{env[k]}'\n")
        f.write(f"exec '{BIN}' relay\n")
    os.chmod(f"{shimdir}/ssh", 0o755)
    path = os.environ["PATH"]
    os.environ["PATH"] = f"{shimdir}:{path}"
    srv = Server(f"{RUNDIR}/a-{kind}")
    try:
        srv.start(config='[messaging.links]\ndial_ms = 3000\n\n'
                         '[remotes.mini]\nssh = "whatever"\nremux_path = "remux"\n')
        # A send is attributed to the pane it runs in, so it runs in one.
        c = Client(srv.sock)
        c.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "t"})
        c.drain(0.5)
        c.send({"CreateSession": {"name": "main", "folder": None}})
        c.drain(0.5)
        out = f"{RUNDIR}/send-{kind}"
        c.send({"CliSpawn": {"session": "main", "placement": "SplitBelow", "argv": [
            "sh", "-c", f"'{BIN}' msg send mini:%1 --deliver mailbox hello 2>'{out}.err'; "
                        f"echo $? > '{out}.rc.tmp'; mv '{out}.rc.tmp' '{out}.rc'"]}})
        end = time.time() + 30
        while time.time() < end and not os.path.exists(f"{out}.rc"):
            time.sleep(0.1)
        rc = open(f"{out}.rc").read().strip() if os.path.exists(f"{out}.rc") else None
        err = open(f"{out}.err").read().strip() if os.path.exists(f"{out}.err") else ""
        check(rc == "1" and "route_unavailable" in err and CAUSE in err,
              f"2 {kind}: the send names the relay's cause (rc={rc}, err={err!r})")
        check("panicked" not in srv.log(), f"3 {kind}: no panic in A's log")
    finally:
        srv.stop()
        os.environ["PATH"] = path


def main():
    for kind in ("ro", "long"):
        env = b_env(kind)
        relay_alone(kind, env)
        through_a_dial(kind, env)


if __name__ == "__main__":
    if os.geteuid() == 0:
        print("SKIP  root ignores the 0500 runtime dir the live case relies on")
        sys.exit(0)
    shutil.rmtree(RUNDIR, ignore_errors=True)
    os.makedirs(RUNDIR)
    try:
        main()
    finally:
        for d in read_only:
            os.chmod(d, 0o700)
        shutil.rmtree(RUNDIR, ignore_errors=True)
    print()
    print(f"{len(failures)} FAILED" if failures else "ALL PASS")
    sys.exit(1 if failures else 0)
