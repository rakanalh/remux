"""Does a pane's shell inherit any OTHER pane's PTY master?

Field evidence: `lsof` on a pane's shell on the user's machine showed it holding
every earlier pane's PTY master. `openpty` returns a master WITHOUT `FD_CLOEXEC`,
so every `fork`/`exec` after it hands that descriptor to the new pane's shell and
to everything the shell runs. Reproduced on Linux through `/proc/<pid>/fd`.

The consequence is not cosmetic. A pane's master is what reports the pane's death:
while a sibling shell still holds a copy, closing the pane leaves the descriptor
open, the slave keeps a peer, and the pane's own shell never gets its hangup.

The check is per PANE SHELL, and the shape of the failure is asymmetric, which is
what makes a partly-red run meaningful rather than confusing: a shell drops the
master of its OWN pane in the forked child, so pane 1 is clean even on a leaking
build. Pane 2 holds one stale master and pane 3 holds two. A run that is red on
only pane 3 means the count is wrong, not that the leak is half fixed.

`/dev/pts` devices are counted as DISTINCT DEVICES rather than as descriptors,
because `/bin/sh` is bash on some distributions and bash keeps its own fd 255 on
the pane's tty alongside stdin, stdout and stderr. Four descriptors, one device,
and the device count is the thing being asserted.

Run from the repo root:
    python3 tests/frame/pane_fd_hygiene.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client  # noqa: E402

RUNDIR = "/tmp/rmx-fdhyg"
PANES = 3


def shell_pids(log):
    """Every pane shell the server reported spawning, in spawn order."""
    out = []
    for line in log.splitlines():
        if "child process spawned with pid=" in line:
            out.append(int(line.split("pid=")[1].split()[0]))
    return out


def fd_table(pid):
    """`{fd: target}` for one process, skipping descriptors that close mid-read."""
    out = {}
    d = f"/proc/{pid}/fd"
    for name in os.listdir(d):
        try:
            out[name] = os.readlink(f"{d}/{name}")
        except OSError:
            pass
    return out


def main():
    if not os.path.isdir("/proc/self/fd"):
        print("SKIP: this check reads /proc/<pid>/fd, which this platform has no "
              "equivalent of")
        return 0

    fails = []
    s = Server(RUNDIR).start()
    try:
        c = Client(s.sock)
        c.hello()
        c.send({"CreateSession": {"name": "main", "folder": None}})
        c.send({"Attach": {"session_name": "main"}})
        c.send({"Resize": {"cols": 100, "rows": 30}})
        time.sleep(0.8)
        c.send({"Command": "PaneSplitVertical"})
        time.sleep(0.8)
        c.send({"Command": "PaneSplitHorizontal"})
        time.sleep(1.2)
        c.drain(0.5)

        pids = shell_pids(s.log())
        print(f"pane shells spawned: {pids}")
        if len(pids) != PANES:
            print(f"RESULT: FAIL ({len(pids)} shells in the log, expected {PANES}; "
                  f"the panes were never created, so nothing below was tested)")
            return 1

        own_tty = []
        for n, pid in enumerate(pids, start=1):
            try:
                table = fd_table(pid)
            except OSError as e:
                fails.append(f"pane {n} shell (pid {pid}) is gone: {e}")
                continue
            masters = sorted(fd for fd, t in table.items() if t == "/dev/ptmx")
            ptys = sorted({t for t in table.values() if t.startswith("/dev/pts/")})
            print(f"pane {n} shell (pid {pid}): ptmx={masters or 'none'} pts={ptys}")
            if masters:
                fails.append(
                    f"pane {n} shell holds {len(masters)} PTY master(s) on fd "
                    f"{', '.join(masters)}; a pane shell must hold none"
                )
            if len(ptys) != 1:
                fails.append(
                    f"pane {n} shell sees {len(ptys)} pts device(s) ({ptys}); it must "
                    f"see only its own"
                )
            own_tty.extend(ptys)

        if len(set(own_tty)) != len(own_tty):
            fails.append(
                f"two pane shells share a pts device ({own_tty}); the panes are not "
                f"separate terminals, so the counts above prove nothing"
            )

        log = s.log()
        if "panicked at" in log:
            fails.append("the server panicked; see the log")
    finally:
        s.kill()

    if fails:
        print("RESULT: FAIL")
        for f in fails:
            print("  -", f)
        return 1
    print(f"RESULT: PASS (each of {PANES} pane shells holds its own pts and no PTY "
          f"master)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
