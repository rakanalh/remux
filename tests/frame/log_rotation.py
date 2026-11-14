#!/usr/bin/env python3
"""`server.log` and `client.log` rotate by size while the process runs.

`main.rs` pins the logger at Debug, so a log grows for as long as its process
lives. `REMUX_TEST_LOG_ROTATE_BYTES` lowers the limit so a few hundred protocol
messages fill it.

  1  a running server rotates server.log: `.1` to `.3` exist, `.4` does not
  2  every server log file is 0600 and at most the limit plus one record
  3  every file starts on a whole record, so no record was split
  4  logging continues after rotation: a session created afterwards is named
     in the live file and in no rotated one
  2b every rotated file was at least the limit: a writer that shifted the
     chain a second time for one full file would rotate a small one
  5  25 rounds of thirty concurrent `remux ls` processes share
     client.log: `.4` never appears, every file is 0600 and bounded, every
     line is whole, and no round leaves a rotated file below the limit
  6  a server.log over twice the limit at startup (a log from before
     rotation) is dropped, so the directory is bounded again

Run: python3 tests/frame/log_rotation.py   (RMX_RUNDIR overrides the run dir)
"""
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import BIN, Client, Server  # noqa: E402
from rundirs import run_dir  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR") or run_dir("rmx-logrot")
LIMIT = 20_000
CLIENT_LIMIT = 2_000
CLIENT_ROUNDS = 25
# A record can land after the size check of the write before it, and other
# processes can append between a check and the write it guards.
SLACK = 4_096
RECORD = re.compile(r"^(\[\d{4}-\d\d-\d\dT|remux log: )")

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def files_of(logdir, name):
    found = {}
    for entry in sorted(os.listdir(logdir)):
        if entry == name or entry.startswith(name + "."):
            st = os.stat(os.path.join(logdir, entry))
            found[entry] = (st.st_size, st.st_mode & 0o777)
    return found


def read(path):
    with open(path, errors="replace") as f:
        return f.read()


def whole_records(path):
    lines = read(path).splitlines()
    first_ok = not lines or RECORD.match(lines[0]) is not None
    return first_ok, lines


def main():
    os.environ["REMUX_TEST_LOG_ROTATE_BYTES"] = str(LIMIT)
    srv = Server(f"{RUNDIR}/srv")
    srv.start()
    logdir = f"{RUNDIR}/srv/state/remux"
    try:
        c = Client(srv.sock)
        c.hello()
        c.send({"CreateSession": {"name": "main", "folder": None}})
        c.send({"Attach": {"session_name": "main"}})
        c.send({"Resize": {"cols": 80, "rows": 24}})
        c.drain(0.5)
        for i in range(1500):
            c.send({"Input": {"data": list(b"x")}})
            if i % 100 == 0:
                c.drain(0.05)
        c.send({"CreateSession": {"name": "after-rotation-marker", "folder": None}})
        c.drain(1.0)

        files = files_of(logdir, "server.log")
        # -- 1 -----------------------------------------------------------------
        check(all(f"server.log.{n}" in files for n in (1, 2, 3)) and "server.log.4" not in files
              and set(files) <= {"server.log", "server.log.1", "server.log.2", "server.log.3"},
              f"1 server.log rotated while running, three kept ({sorted(files)})")
        # -- 2 -----------------------------------------------------------------
        check(bool(files) and all(mode == 0o600 and size <= LIMIT + SLACK
                                  for size, mode in files.values()),
              f"2 server log files 0600 and bounded "
              f"({ {k: (s, oct(m)) for k, (s, m) in files.items()} })")
        check(all(size >= LIMIT for k, (size, _) in files.items() if k != "server.log"),
              f"2b every rotated server log file was full when it rotated "
              f"({ {k: s for k, (s, _) in files.items()} })")
        # -- 3 -----------------------------------------------------------------
        starts = {k: whole_records(f"{logdir}/{k}")[0] for k in files}
        check(len(starts) > 1 and all(starts.values()),
              f"3 every server log file starts on a record ({starts})")
        # -- 4 -----------------------------------------------------------------
        live = read(f"{logdir}/server.log")
        rotated = [k for k in files if k != "server.log"]
        check(bool(rotated) and "after-rotation-marker" in live
              and not any("after-rotation-marker" in read(f"{logdir}/{k}") for k in rotated),
              "4 a session created after rotation is logged in the live server.log only")

        # -- 5 -----------------------------------------------------------------
        env = dict(srv.env, REMUX_TEST_LOG_ROTATE_BYTES=str(CLIENT_LIMIT))
        env.pop("REMUX_SESSION", None)
        env.pop("REMUX_PANE", None)
        errs = []
        # A second shift for one full file leaves a short rotated file that
        # later rotations push out of `.3`, so each round is inspected.
        short = []
        for _ in range(CLIENT_ROUNDS):
            procs = [subprocess.Popen([BIN, "ls"], env=env, stdin=subprocess.DEVNULL,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                     for _ in range(30)]
            for p in procs:
                _, err = p.communicate(timeout=30)
                errs.append((p.returncode, err.decode(errors="replace")))
            cfiles = files_of(logdir, "client.log")
            short += [(k, size) for k, (size, _) in cfiles.items()
                      if k != "client.log" and size < CLIENT_LIMIT]
        bad_lines = []
        for k in cfiles:
            first_ok, lines = whole_records(f"{logdir}/{k}")
            bad_lines += [(k, ln[:60]) for ln in lines if not RECORD.match(ln)]
        check(all(rc == 0 and "panicked" not in e for rc, e in errs),
              f"5a thirty concurrent clients exited cleanly ({[rc for rc, _ in errs]})")
        check("client.log.1" in cfiles and "client.log.4" not in cfiles
              and set(cfiles) <= {"client.log", "client.log.1", "client.log.2", "client.log.3"},
              f"5b client.log rotated under concurrent writers, three kept ({sorted(cfiles)})")
        check(all(mode == 0o600 and size <= CLIENT_LIMIT + SLACK
                  for size, mode in cfiles.values()),
              f"5c client log files 0600 and bounded "
              f"({ {k: (s, oct(m)) for k, (s, m) in cfiles.items()} })")
        check(not short,
              f"5e every rotated client log file was full when it rotated, so no writer "
              f"shifted the chain twice ({short[:4]})")
        check(len(cfiles) > 1 and not bad_lines,
              f"5d every client.log line is a whole record ({bad_lines[:3]})")

        # -- 6 -----------------------------------------------------------------
        srv.stop()
        with open(f"{logdir}/server.log", "w") as f:
            f.write(("y" * 99 + "\n") * 2_000)
        srv.start_again()
        c = Client(srv.sock)
        c.hello()
        c.drain(0.3)
        c.close()
        live = read(f"{logdir}/server.log")
        total = sum(s for s, _ in files_of(logdir, "server.log").values())
        check(os.path.getsize(f"{logdir}/server.log") <= LIMIT + SLACK
              and "dropped the previous log at 200000 bytes" in live
              and "remux starting as server" in live
              and total <= 4 * (LIMIT + SLACK),
              f"6 an oversized server.log at startup is dropped "
              f"(live {os.path.getsize(f'{logdir}/server.log')} bytes, all {total} bytes)")

        log = srv.log() + "".join(read(f"{logdir}/{k}") for k in files_of(logdir, "client.log"))
        check("panicked" not in log, "no panic in any log")
    finally:
        srv.kill()


if __name__ == "__main__":
    main()
    print()
    print(f"{len(failures)} FAILED" if failures else "ALL PASS")
    sys.exit(1 if failures else 0)
