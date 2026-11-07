"""The last check before a harness signals a process that it found by scanning.

`harness._matches` decides which processes belong to a run. A scan reads every
process on the machine, so one wrong line there reaches every remux server,
including the developer's real one. This module is a second gate behind it. It
reads the process again with its own code, and it shares nothing with
`_matches`, so a fault in one does not disable the other.

A pid may be signalled only if all of these are true:

- no real pid file names it,
- its `XDG_RUNTIME_DIR` (`TMPDIR` when that is unset) is in a `rundirs` root,
  which is a directory `/tmp/rmxr.<8 chars>`,
- that path is under the root the caller is reaping, so another run is safe too.

A refusal prints to stderr and signals nothing. A run with `RMX_RUNDIR` or
another fixed path is outside every `rundirs` root, so the reaper leaves its
servers alone and says so. Without `/proc` (macOS) the environment cannot be
read, so everything is refused.

Import this before the harness overrides `XDG_RUNTIME_DIR`, because the real pid
file is found from the environment the harness started with.
"""
import os
import re
import sys

RUN_ROOT = re.compile(r"/tmp/rmxr\.[a-z0-9_]{8}(/|$)")


def _real_pid_files() -> list:
    uid = os.getuid()
    dirs = [f"/run/user/{uid}", f"{os.environ.get('TMPDIR') or '/tmp'}/remux-{uid}"]
    started_in = os.environ.get("XDG_RUNTIME_DIR")
    # A harness started from a pane of another harness has a run dir here, and
    # the pid file in it names a server that may be signalled.
    if started_in and not RUN_ROOT.match(os.path.normpath(started_in)):
        dirs.insert(0, started_in)
    return [f"{d}/remux.pid" for d in dirs]


PID_FILES = _real_pid_files()


def _real_pids() -> set:
    pids = set()
    for path in PID_FILES:
        try:
            with open(path) as f:
                pids.add(int(f.read().strip()))
        except (OSError, ValueError):
            pass
    return pids


def _runtime_dir(pid: int) -> str:
    """The process's runtime dir, normalised. An empty string if it has none or
    its environment cannot be read."""
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            entries = f.read().split(b"\0")
    except OSError:
        return ""
    found = {}
    for entry in entries:
        name, _, value = entry.partition(b"=")
        found[name] = value.decode(errors="replace")
    value = found.get(b"XDG_RUNTIME_DIR") or found.get(b"TMPDIR") or ""
    return os.path.normpath(value) if value else ""


def refusal(pid: int, root: str) -> str:
    """Why `pid` must not be signalled while reaping `root`, or an empty string."""
    if pid in _real_pids():
        return "a real pid file names it"
    runtime = _runtime_dir(pid)
    if not RUN_ROOT.match(runtime):
        return f"its runtime dir {runtime!r} is not in a run root"
    root = os.path.normpath(root)
    if runtime != root and not runtime.startswith(root + "/"):
        return f"its runtime dir {runtime!r} is not under {root!r}"
    return ""


def allows(pid: int, root: str) -> bool:
    why = refusal(pid, root)
    if why:
        print(f"REFUSED to signal pid {pid}: {why}", file=sys.stderr, flush=True)
    return not why
