"""Private run directories, so that any harness can run beside another run of itself.

A harness asks for `run_dir("rmx-foo")` and gets `/tmp/rmxr.<random>/rmx-foo`.
The first call in a process creates that one root, and every later call shares
it, so a dir derived by appending to a run dir (`RUNDIR + "-bin"`) and a fake
remote's dirs also land inside the root. A fixed path shared a socket between
runs: one run's `remux stop` killed the other's server, and the false red looked
exactly like the bug under test.

The first call also forks. The original process becomes a supervisor that only
waits for the harness, which continues in the child. A supervisor is the only
way to read the real exit status: `atexit` cannot see it, and it does not run at
all when a harness dies from a signal. So call `run_dir` before starting any
subprocess or thread. A subprocess started earlier belongs to the supervisor,
and the harness cannot wait for it.

When the harness exits, the supervisor reaps the remux servers and relays under
the root. Exit status 0 removes the root. Any other outcome keeps it and prints
its path, because the logs are the evidence.

The root holds a file `.owner`, which names the run and stays under an exclusive
`flock` for as long as the supervisor or the harness lives. The kernel releases
the lock when both are gone, so no pid can be reused to fake an owner. Every run
sweeps the leftovers of earlier runs. A sweep removes a directory only if all of
these are true: it is a directory directly in `/tmp` with the `rmxr.` name
format, this user owns it, it holds `.owner`, nothing holds the lock, and it is
older than `RMX_RUN_KEEP_HOURS` (72 by default). So a sweep cannot touch a live
run, a fixed `/tmp/rmx-*` dir of an unconverted harness, or another user's dir.

`/tmp` and not `$TMPDIR`: a socket path must fit in 108 bytes, and the macOS
`$TMPDIR` alone uses about half of that.
"""
import fcntl
import os
import re
import shlex
import shutil
import signal
import stat
import sys
import tempfile
import time

BASE = "/tmp"
PREFIX = "rmxr."
ROOT_NAME = re.compile(r"rmxr\.[a-z0-9_]{8}")
OWNER = ".owner"
KEEP_HOURS = 72
# `sun_path` holds 108 bytes. The margin is for suffixes that harnesses append
# to a run dir after this check, such as `-bin` or a second server's name.
MAX_SOCKET_PATH = 96

_root = None


def run_root() -> str:
    """This run's root directory. The first call creates it and starts the supervisor."""
    global _root
    if _root is None:
        _root = _create_root()
        _supervise(_root)
    return _root


def run_dir(name: str) -> str:
    """The path of `name` under this run's root. `name` may hold subdirectories.
    The same name returns the same path for the whole run. The caller creates it."""
    if name.startswith("/") or ".." in name.split("/"):
        raise ValueError(f"run_dir takes a name under the run root, not {name!r}")
    path = f"{run_root()}/{name}"
    socket = f"{path}/run/remux.sock"
    if len(socket) > MAX_SOCKET_PATH:
        raise ValueError(f"{socket} is over {MAX_SOCKET_PATH} bytes: use a shorter name")
    return path


def _create_root() -> str:
    root = tempfile.mkdtemp(prefix=PREFIX, dir=BASE)
    # The descriptor is deliberately never closed. It is close-on-exec, so a
    # remux server does not inherit the lock, and a harness that outlives a
    # killed supervisor still holds it through the shared open file description.
    fd = os.open(f"{root}/{OWNER}", os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    fcntl.flock(fd, fcntl.LOCK_EX)
    os.write(fd, f"pid {os.getpid()}\nargv {shlex.join(sys.argv)}\n".encode())
    return root


def _supervise(root: str) -> None:
    """Return in the harness. In the supervisor, wait and exit as the harness did."""
    print(f"run dir: {root}", file=sys.stderr, flush=True)
    # Without the flush, both processes would write the buffered output.
    sys.stdout.flush()
    pid = os.fork()
    if pid == 0:
        return
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, lambda signum, _frame: _forward(pid, signum))
    try:
        _sweep(root)
    except Exception as e:
        print(f"run dir sweep failed: {e!r}", file=sys.stderr)
    code = os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1])
    _reap(root)
    if code == 0:
        _remove(root)
    else:
        # A sweep counts age from the failure, not from the start of the run.
        os.utime(f"{root}/{OWNER}")
        print(f"run dir kept: {root} (exit {code}), logs in {root}/*/state/remux/",
              file=sys.stderr, flush=True)
    _exit_as(code)


def _forward(pid: int, signum: int) -> None:
    try:
        os.kill(pid, signum)
    except ProcessLookupError:
        pass


def _exit_as(code: int) -> None:
    """Report the harness's outcome to the caller, including death from a signal."""
    if code < 0:
        if -code != signal.SIGKILL:
            signal.signal(-code, signal.SIG_DFL)
        os.kill(os.getpid(), -code)
    os._exit(code if code >= 0 else 128 - code)


def _reap(root: str) -> None:
    """Stop what a harness left under `root`. A killed harness leaves its server
    running, and a server left in a removed directory runs for ever. The match is
    on the process's environment, so only this root's processes are hit."""
    frame = os.path.join(os.path.dirname(os.path.abspath(__file__)), "frame")
    if frame not in sys.path:
        sys.path.insert(0, frame)
    try:
        from harness import reap_rundir
        reap_rundir(root)
    except Exception as e:
        print(f"could not reap under {root}: {e!r}", file=sys.stderr)


def _remove(root: str) -> None:
    """Remove `root`, with `.owner` last. A failure part way leaves a dir that a
    later sweep can still recognise and remove."""
    for attempt in range(5):
        for dirpath, dirnames, _ in os.walk(root):
            # A harness can leave a read-only directory, and `rmtree` cannot
            # empty one. `os.walk` does not follow symlinks here.
            for name in dirnames:
                path = os.path.join(dirpath, name)
                if not os.path.islink(path):
                    os.chmod(path, 0o700)
        with os.scandir(root) as entries:
            for entry in entries:
                if entry.name == OWNER:
                    continue
                if entry.is_dir(follow_symlinks=False):
                    shutil.rmtree(entry.path, ignore_errors=True)
                else:
                    os.unlink(entry.path)
        if os.listdir(root) == [OWNER]:
            os.unlink(f"{root}/{OWNER}")
            os.rmdir(root)
            return
        # A pane shell that is exiting can still create a file.
        time.sleep(0.2)
    print(f"run dir not fully removed: {root}", file=sys.stderr)


def _sweep(own_root: str) -> None:
    hours = float(os.environ.get("RMX_RUN_KEEP_HOURS", KEEP_HOURS))
    horizon = time.time() - hours * 3600
    with os.scandir(BASE) as entries:
        candidates = [e.path for e in entries if ROOT_NAME.fullmatch(e.name)]
    for root in candidates:
        if root != own_root and _abandoned(root, horizon):
            _reap(root)
            _remove(root)
            print(f"swept old run dir: {root}", file=sys.stderr)


def _abandoned(root: str, horizon: float) -> bool:
    """Whether every sweep condition in the module docstring holds for `root`.
    True leaves the lock held by a descriptor that the caller's exit closes."""
    try:
        info = os.lstat(root)
        owner = os.lstat(f"{root}/{OWNER}")
    except OSError:
        return False
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        return False
    if not stat.S_ISREG(owner.st_mode) or owner.st_mtime > horizon:
        return False
    try:
        fd = os.open(f"{root}/{OWNER}", os.O_RDONLY | os.O_NOFOLLOW)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        return False
    return True
