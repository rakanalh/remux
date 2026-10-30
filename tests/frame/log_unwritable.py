#!/usr/bin/env python3
"""The CLI keeps working when it cannot write its log; the server and the
relay refuse to run without one.

A sandbox (Claude Code's Bash tool, for one) can leave `$XDG_STATE_HOME`
read-only. Every remux process opens `<state>/remux/<role>.log` before it does
anything else, and used to panic there, so no `remux msg` or `remux agent`
command could run inside the sandbox at all. A client now runs without a file
log, and says nothing about it on stderr, which hooks and scripts read. The
server and the relay are long-lived and their logs are the record of
attribution and links, so they exit with the reason instead.

  1  the state dir is read-only, so `remux/` cannot be created: `integrate
     --status` exits 0 with its answer, nothing on stderr, and no panic
  2  `remux/` exists but client.log is read-only: the same
  3  a server outside the sandbox, the CLI inside it: `agent ls --json`
     reaches the server and answers, with nothing on stderr
  4  remux left the read-only client.log as it was, rather than replacing it
  5  `remux server` with the read-only state dir exits non-zero naming its
     log, and binds no socket
  6  `remux relay` the same

SKIPs as root, whom a read-only mode does not stop.

Run: python3 tests/frame/log_unwritable.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Server  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-logro")

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def cli_env(state, runtime):
    env = {k: v for k, v in os.environ.items() if k not in ("REMUX_SESSION", "REMUX_PANE")}
    env.update(HOME=f"{RUNDIR}/home", XDG_STATE_HOME=state, XDG_RUNTIME_DIR=runtime,
               XDG_DATA_HOME=f"{RUNDIR}/data", XDG_CONFIG_HOME=f"{RUNDIR}/config")
    return env


def run(env, *args):
    p = subprocess.run([BIN, *args], env=env, capture_output=True, timeout=20)
    return p.returncode, p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace")


def no_panic(rc, err):
    return rc != 101 and "panicked" not in err


def main():
    for d in ("home", "data", "config", "run-none"):
        os.makedirs(f"{RUNDIR}/{d}", exist_ok=True)
    os.chmod(f"{RUNDIR}/run-none", 0o700)

    # -- 1 ---------------------------------------------------------------------
    ro = f"{RUNDIR}/state-ro"
    os.makedirs(ro, exist_ok=True)
    os.chmod(ro, 0o500)
    rc, out, err = run(cli_env(ro, f"{RUNDIR}/run-none"), "integrate", "claude-messaging", "--status")
    check(rc == 0 and "Absent" in out and err == "" and no_panic(rc, err),
          f"1 a read-only state dir: --status answers (rc={rc}, out={out.strip()!r}, err={err.strip()[:200]!r})")

    # -- 2 ---------------------------------------------------------------------
    rofile = f"{RUNDIR}/state-rofile"
    os.makedirs(f"{rofile}/remux", exist_ok=True)
    open(f"{rofile}/remux/client.log", "w").close()
    os.chmod(f"{rofile}/remux/client.log", 0o400)
    before = os.stat(f"{rofile}/remux/client.log").st_ino
    rc, out, err = run(cli_env(rofile, f"{RUNDIR}/run-none"), "integrate", "claude-messaging", "--status")
    check(rc == 0 and "Absent" in out and err == "" and no_panic(rc, err),
          f"2 a read-only client.log: --status answers (rc={rc}, out={out.strip()!r}, err={err.strip()[:200]!r})")

    # -- 3 ---------------------------------------------------------------------
    srv = Server(f"{RUNDIR}/srv")
    try:
        srv.start()
        rc, out, err = run(cli_env(ro, srv.env["XDG_RUNTIME_DIR"]), "agent", "ls", "--json")
        try:
            panes = json.loads(out).get("panes")
        except ValueError:
            panes = None
        check(rc == 0 and isinstance(panes, list) and err == "" and no_panic(rc, err),
              f"3 the CLI in the sandbox reaches a server outside it (rc={rc}, out={out.strip()[:120]!r}, "
              f"err={err.strip()[:200]!r})")
    finally:
        srv.stop()

    # -- 4 ---------------------------------------------------------------------
    after = os.stat(f"{rofile}/remux/client.log")
    check(after.st_ino == before and after.st_size == 0,
          f"4 the read-only client.log was neither replaced nor written (inode {before} -> "
          f"{after.st_ino}, {after.st_size} bytes)")

    # -- 5, 6 ------------------------------------------------------------------
    for n, role in ((5, "server"), (6, "relay")):
        run_dir = f"{RUNDIR}/run-{role}"
        os.makedirs(run_dir, mode=0o700, exist_ok=True)
        p = subprocess.Popen([BIN, role], env=cli_env(ro, run_dir), stdin=subprocess.DEVNULL,
                             stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            _, err = p.communicate(timeout=5)
            rc = p.returncode
        except subprocess.TimeoutExpired:
            p.kill()
            _, err = p.communicate()
            rc = "still running after 5s"
        err = err.decode(errors="replace").strip()
        check(rc not in (0, 101, "still running after 5s") and "cannot open its log" in err
              and f"{ro}/remux/{role}.log" in err and not os.path.exists(f"{run_dir}/remux.sock"),
              f"{n} `remux {role}` refuses to run without its log (rc={rc}, err={err[:200]!r})")


if __name__ == "__main__":
    if os.geteuid() == 0:
        print("SKIP  root ignores the read-only modes these cases rely on")
        sys.exit(0)
    shutil.rmtree(RUNDIR, ignore_errors=True)
    os.makedirs(RUNDIR)
    try:
        main()
    finally:
        for d in (f"{RUNDIR}/state-ro", f"{RUNDIR}/state-rofile/remux/client.log"):
            if os.path.exists(d):
                os.chmod(d, 0o700)
        shutil.rmtree(RUNDIR, ignore_errors=True)
    print()
    print(f"{len(failures)} FAILED" if failures else "ALL PASS")
    sys.exit(1 if failures else 0)
