"""Private run directories, so that any harness can run beside another run of itself.

THE RULE: this code signals only a process it started, or a pid that a run
recorded and that is verified just before the signal (`kill_wedged_server`,
`kill_relays`). It never searches the process table. A scan picks its targets by
a rule, and one wrong line in that rule reaches every remux server on the
machine, including the developer's real one.

`rundirs_selftest.py` checks the source of every Python file under tests/
except itself, which holds the snippets it is tested against (`SCAN_SKIP`). It
fails on `pkill` or `killall` and on a fixed `/tmp/rmx` run dir, anywhere. It
also fails on each of these unless an allowlist entry names the rule, the
definition it is in and how many times it occurs there, with a reason:
- a `/proc` path in a string;
- a signal call: `os.kill`, `killpg`, `pidfd_send_signal`, `pthread_kill`,
  `os.system` or `os.popen`, by name, alias, `from os import *`, `getattr`, or
  a ctypes library held in a name or an attribute;
- a program run that signals (`kill`, `fuser`, `pidof`) or reads the process
  table (`ps`, `pgrep`), through `subprocess`, `os.exec*`, `os.spawn*`,
  `os.posix_spawn*`, asyncio, `pexpect.run`, `spawn`, `spawnu`, `PopenSpawn` or
  `pty.spawn`, in a list, an `args=` keyword, a module-level variable, a shell
  string or f-string, past `env`, `timeout`, `nice`, `setsid`, `nohup`,
  `command`, `exec` and the other wrappers, and inside `sh -c` or `bash -lc`;
- a `remux stop` or `restart` whose `env=` is missing or is the caller's own
  environment (`os.environ`, a copy of it, or `{**os.environ}` with no socket
  dir set);
- a use of this module's private signal helpers outside their one caller.
It reads source text, so it does not see a command held in a local variable,
one built with `shlex.split` or at run time, or one run through `xargs` or
`find -exec`. At run time `home_guard` parses every launch with the same parser
and refuses a `remux stop` or `restart` whose environment could name the real
server's socket directory. A command's own assignments (`NAME=value cmd`,
`env NAME=value cmd`) count. An `export`, `export -n`, `unset` or plain
assignment earlier in a shell script can only add a refusal: the command is
checked with and without them and must pass both. `$NAME` and `${NAME}` are
expanded from the environment being checked; any other computed value (a
backtick, `$(`, a `$` the shell does not expand because it is in single quotes
or after a backslash, a name that environment does not hold, `${NAME:-default}`)
for `XDG_RUNTIME_DIR`, `TMPDIR` or `HOME` is refused, and so is an
`XDG_RUNTIME_DIR` or `TMPDIR` that is relative or has a `..` component. So an
`RMX_RUNDIR` or `REMUX_*_RUN` with a `..` component is refused when its server
is stopped. A `~` the shell would expand, and bash's `$'…'` and `$"…"`, count as
computed. A substituted program given `stop` or `restart` is checked as remux. The parser does not see `declare -x`,
`typeset -x`, `set -a` followed by a plain assignment, `.` or `source`, `read
NAME` followed by `export NAME`, or a function defined under one export and
called under another. Each of these is judged by the environment without it, so
it can let a stop the script points at the real server through, and it can
refuse a script that relies on it to point a stop at a private one. No harness
uses any of these shapes.

A harness asks for `run_dir("rmx-foo")` and gets `/tmp/rmxr.<random>/rmx-foo`.
The first call in a process creates that one root, and every later call shares
it, so a dir derived by appending to a run dir (`RUNDIR + "-bin"`) and a fake
remote's dirs also land inside the root. A fixed path shared a socket between
runs: one run's `remux stop` killed the other's server, and the false red looked
exactly like the bug under test.

To convert a harness, import the helper after the harness import, which puts
`tests/` on `sys.path`, and replace every fixed path. Use `RUNDIR + "-x"` or
another `run_dir` name for a sibling or a second server. `RMX_RUNDIR` stays an
exact opt-out:

    from rundirs import run_dir  # noqa: E402
    RUNDIR = os.environ.get("RMX_RUNDIR") or run_dir("rmx-foo")

A fake `ssh` shim records its relay, so that `kill_relays` can drop it:

    f"echo $$ >> {relay_pids()}\\n" f"exec {BIN} relay\\n"

The first call also forks. The original process becomes a supervisor that only
waits for the harness, which continues in the child. A supervisor is the only
way to read the real exit status: `atexit` cannot see it, and it does not run at
all when a harness dies from a signal. So call `run_dir` before starting any
subprocess or thread, and the first call refuses if one is running. A subprocess
started earlier belongs to the supervisor, so its exit status reads as 0 in the
harness, and a thread started earlier does not exist after the fork. Without
`/proc` (macOS) only the thread check can run.

When the harness exits, the supervisor stops what the run left. It drops the
relays that the run recorded, and it runs `remux stop` for each socket under the
root, with `XDG_RUNTIME_DIR` set to that socket's directory. Exit status 0 then
removes the root. Any other outcome keeps it and prints its path, because the
logs are the evidence. A kept root stays until someone deletes it or `/tmp` is
cleared. This covers a SIGKILL of the harness itself, such as from the OOM killer.

The pid a caller holds is the supervisor's. Signals it receives are forwarded to
the harness. If it dies without forwarding, as with `kill -9` of that pid or of
the process group, nothing cleans up: on Linux the harness then gets SIGTERM
through `PR_SET_PDEATHSIG` and stops what it can. macOS has no equivalent, so
there the harness runs to its end and holds the caller's pipes until it does.

`/tmp` and not `$TMPDIR`: a socket path must fit in 108 bytes, and the macOS
`$TMPDIR` alone uses about half of that.
"""
import ctypes
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

from home_guard import real_socket_dirs, socket_dir

BASE = "/tmp"
PREFIX = "rmxr."
BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
# `sun_path` holds 108 bytes. The margin is for suffixes that harnesses append
# to a run dir after this check, such as `-bin` or a second server's name.
MAX_SOCKET_PATH = 96
FORWARDED = (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGQUIT)
PR_SET_PDEATHSIG = 1

_root = None


def run_root() -> str:
    """This run's root directory. The first call creates it and starts the supervisor."""
    global _root
    if _root is None:
        _refuse_started_work()
        _root = tempfile.mkdtemp(prefix=PREFIX, dir=BASE)
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


def relay_pids(rundir: str = "") -> str:
    """The file where this run's fake `ssh` shims record their relays: in
    `rundir`, or by default in `RMX_RUNDIR` or the run's root."""
    return f"{rundir or _relay_dir()}/relay.pids"


def _relay_dir() -> str:
    """The root `run_dir` already made, else `RMX_RUNDIR`. The root comes first:
    a harness that called `run_dir` runs its relays there whatever the shell
    exports. A harness that opted out of `run_dir` some other way has no root,
    and making one now would fork it part way through its run, so it must pass
    its own dir instead."""
    if _root is not None:
        return _root
    opted_out = os.environ.get("RMX_RUNDIR")
    if opted_out:
        return opted_out
    raise RuntimeError("relay_pids or kill_relays was called before run_dir: pass the run dir")


def _refuse_started_work() -> None:
    """Refuse what the fork would break. See the module docstring."""
    if threading.active_count() > 1:
        raise RuntimeError("a thread was started before run_dir: call run_dir first")
    try:
        with open(f"/proc/self/task/{os.getpid()}/children") as f:
            children = f.read().split()
    except OSError:
        return
    if children:
        raise RuntimeError(f"a subprocess was started before run_dir (pids {children}): "
                           "call run_dir first")


def _say(message: str) -> None:
    """Write a line to stderr without raising. When the caller stops reading,
    `print` raises BrokenPipeError, and in the supervisor that exception would
    return up the harness's stack instead of reaching `_exit_as`."""
    try:
        os.write(2, f"{message}\n".encode(errors="replace"))
    except OSError:
        pass


def _supervise(root: str) -> None:
    """Return in the harness. In the supervisor, wait and exit as the harness did."""
    _say(f"run dir: {root}")
    # Without the flush, both processes would write the buffered output.
    sys.stdout.flush()
    supervisor = os.getpid()
    # Until `_wait_for` installs the forwarding handlers, a signal to the
    # supervisor would take its default action and never reach the harness. Held
    # blocked across the fork, it stays pending and is forwarded once they exist.
    unblocked = signal.pthread_sigmask(signal.SIG_BLOCK, FORWARDED)
    try:
        pid = os.fork()
    except BaseException:
        signal.pthread_sigmask(signal.SIG_SETMASK, unblocked)
        raise
    if pid == 0:
        signal.pthread_sigmask(signal.SIG_SETMASK, unblocked)
        _die_with(supervisor)
        return
    # This process holds the harness's stack. An exception that left this
    # function would run the harness's `except` blocks a second time here.
    code = 1
    try:
        code = _wait_for(pid, unblocked)
        _clean(root, code)
    except BaseException as e:
        _say(f"run dir supervisor failed: {e!r}")
    finally:
        _exit_as(code)


def _die_with(supervisor: int) -> None:
    if sys.platform != "linux":
        return
    ctypes.CDLL(None).prctl(PR_SET_PDEATHSIG, int(signal.SIGTERM))
    # The supervisor can die before the prctl takes effect.
    if os.getppid() != supervisor:
        os.kill(os.getpid(), signal.SIGTERM)


def _wait_for(pid: int, unblocked) -> int:
    for sig in FORWARDED:
        signal.signal(sig, lambda signum, _frame: _forward(pid, signum))
    signal.pthread_sigmask(signal.SIG_SETMASK, unblocked)
    code = os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1])
    # `pid` is free from here, so another process can already have it.
    for sig in FORWARDED:
        signal.signal(sig, signal.SIG_IGN)
    return code


def _clean(root: str, code: int) -> None:
    # A harness can record relays beside a second server rather than at the
    # root, so every `relay.pids` under the root is read.
    for dirpath, _, files in os.walk(root):
        if "relay.pids" in files:
            kill_relays(dirpath, within=root)
    stop_servers(root)
    if code == 0:
        _remove(root)
    else:
        _say(f"run dir kept: {root} (exit {code}), logs in {root}/*/state/remux/")


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


def _remove(root: str) -> None:
    if os.path.islink(root):
        _say(f"run dir is now a symlink, so it is not removed: {root}")
        return
    for attempt in range(5):
        for dirpath, dirnames, _ in os.walk(root):
            # A harness can leave a read-only directory, and `rmtree` cannot
            # empty one. `os.walk` does not follow symlinks here.
            for name in dirnames:
                path = os.path.join(dirpath, name)
                try:
                    if not os.path.islink(path):
                        os.chmod(path, 0o700)
                except FileNotFoundError:
                    pass
        shutil.rmtree(root, ignore_errors=True)
        if not os.path.lexists(root):
            return
        # A pane shell that is exiting can still create a file.
        time.sleep(0.2)
    _say(f"run dir not fully removed: {root}")


def _checked_root(rundir: str) -> str:
    """`rundir` normalised, or AssertionError when a walk under it could reach a
    socket that is not this harness's: an empty, relative or top-level dir, or
    one that contains a dir where the user's real server listens. Raised
    explicitly rather than by `assert`, which `python -O` strips."""
    root = os.path.normpath(rundir) if rundir else ""
    if not os.path.isabs(root) or root.count("/") < 2:
        raise AssertionError(f"refusing to stop servers under {rundir!r}: "
                             "not a dedicated absolute run dir")
    if os.path.islink(root):
        raise AssertionError(f"refusing to stop servers under {rundir!r}: it is a symlink")
    for live in real_socket_dirs() | {os.path.normpath(socket_dir(os.environ))}:
        if live == root or live.startswith(root + "/"):
            raise AssertionError(f"refusing to stop servers under {rundir!r}: it contains "
                                 f"{live}, where a real server listens")
    return root


def stop_servers(rundir: str) -> None:
    """Stop every server whose socket or pid file is under `rundir`. This walks
    the run's own directory, not the process table."""
    root = _checked_root(rundir)
    for dirpath, _, files in os.walk(root):
        if "remux.sock" in files or "remux.pid" in files:
            stop_runtime(dirpath)


def stop_runtime(runtime_dir: str) -> None:
    """`remux stop` for the server of one socket directory.

    Without XDG_RUNTIME_DIR, `remux stop` finds the user's real server, so a
    missing or relative value raises. The environment is built here and inherits
    nothing else that could point elsewhere.

    `remux stop` sends SIGTERM to the pid in `remux.pid` without checking it, and
    a crashed server leaves that file behind, so the pid is verified first.
    """
    if not runtime_dir or not os.path.isabs(runtime_dir):
        raise ValueError(f"stop_runtime needs an absolute socket dir, not {runtime_dir!r}")
    _checked_root(runtime_dir)
    if server_pid(runtime_dir) is None:
        return
    env = {"XDG_RUNTIME_DIR": runtime_dir, "HOME": runtime_dir,
           "XDG_STATE_HOME": f"{runtime_dir}/stop-state", "PATH": os.environ.get("PATH", "")}
    try:
        subprocess.run([BIN, "stop"], env=env, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=15)
    except (OSError, subprocess.TimeoutExpired) as e:
        _say(f"remux stop failed in {runtime_dir}: {e!r}")
    kill_wedged_server(runtime_dir)


def server_pid(runtime_dir: str):
    """The pid `runtime_dir/remux.pid` records, if that process is still this
    directory's server, else None. A stale pid file can name any process."""
    try:
        with open(f"{runtime_dir}/remux.pid") as f:
            pid = int(f.read().strip())
    except (OSError, ValueError):
        return None
    if runs(pid, "server") and _environment_names(pid, runtime_dir):
        return pid
    return None


def kill_wedged_server(runtime_dir: str) -> None:
    """SIGKILL the recorded server if it outlived `remux stop`'s SIGTERM."""
    # The server removes the socket and then the pid file, so allow the gap
    # between the two before treating a leftover file as a wedge.
    for _ in range(10):
        if server_pid(runtime_dir) is None:
            return
        time.sleep(0.1)
    pid = server_pid(runtime_dir)
    if pid is not None:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def kill_relays(root: str = "", within: str = "") -> None:
    """SIGKILL the relays recorded in `root/relay.pids`: a dropped SSH, as the
    client sees it. `root` defaults to `RMX_RUNDIR` or the run's root.

    A recorded pid can be long gone and reused by another run's relay or by a
    real `remux relay`, so one is signalled only while it is a remux relay whose
    socket dir resolves under `within` (default `root`). Where the platform has
    pidfds the process is pinned first and checked after, so the checks and the
    signal reach the same process."""
    root = root or _relay_dir()
    within = os.path.realpath(within or root)
    path = f"{root}/relay.pids"
    if os.path.islink(path):
        _say(f"not reading relay pids from a symlink: {path}")
        return
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd) as f:
            recorded = [int(line) for line in f.read().split()]
    except (OSError, ValueError):
        return
    for pid in recorded:
        _kill_relay(pid, within)


def _kill_relay(pid: int, within: str) -> None:
    try:
        fd = os.pidfd_open(pid)
    except ProcessLookupError:
        return
    except (AttributeError, OSError):
        if _relay_under(pid, within):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except OSError as e:
                _say(f"could not signal relay {pid}: {e!r}")
        return
    try:
        if _relay_under(pid, within):
            signal.pidfd_send_signal(fd, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError as e:
        _say(f"could not signal relay {pid}: {e!r}")
    finally:
        os.close(fd)


def _relay_under(pid: int, within: str) -> bool:
    """Whether `pid` is a remux relay whose socket dir resolves under `within`."""
    if not runs(pid, "relay"):
        return False
    env = _environ(pid)
    if env is None:
        return False
    resolved = os.path.realpath(socket_dir(env))
    return resolved == within or resolved.startswith(within + "/")


def runs(pid: int, subcommand: str) -> bool:
    """Whether `pid` is a live `remux <subcommand>`. This asks about one recorded
    pid. The binary is remux by name, or the one under test."""
    argv = _argv(pid, subcommand)
    if len(argv) < 2 or argv[1] != subcommand:
        return False
    return _is_remux(argv[0])


def _is_remux(program: str) -> bool:
    name = os.path.basename(program)
    return name == "remux" or name.startswith("remux-") or \
        os.path.realpath(program) == os.path.realpath(BIN)


def _argv(pid: int, subcommand: str) -> list:
    """`pid`'s command line. Without /proc (macOS) `ps` prints it as one line,
    and `_split_ps_line` finds the binary in it."""
    if os.path.isdir("/proc"):
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                return [w.decode(errors="replace") for w in f.read().split(b"\0")[:-1]]
        except OSError:
            return []
    try:
        out = subprocess.run(["/bin/ps", "-ww", "-o", "command=", "-p", str(pid)],
                             capture_output=True, text=True, timeout=10).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        return []
    return _split_ps_line(out, subcommand)


def _split_ps_line(line: str, subcommand: str) -> list:
    """`[binary, subcommand, *args]` from one `ps` command line, or [] if the line
    is not that subcommand of remux. A binary's path may hold spaces, so the
    split is at a ` <subcommand>` word whose prefix is remux by name or the
    binary under test, not at the first or the last space."""
    start = 0
    while True:
        i = line.find(f" {subcommand}", start)
        if i < 0:
            return []
        end = i + 1 + len(subcommand)
        if end == len(line) or line[end] == " ":
            head = line[:i]
            if _is_remux(head):
                return [head, subcommand] + line[end:].split()
        start = i + 1


def _environ(pid: int):
    """`pid`'s environment, or None when it cannot be read. Without /proc
    (macOS) `ps -E` appends it to the command line as KEY=VALUE words."""
    if os.path.isdir("/proc"):
        try:
            with open(f"/proc/{pid}/environ", "rb") as f:
                words = [w.decode(errors="replace") for w in f.read().split(b"\0")]
        except OSError:
            return None
    else:
        try:
            words = subprocess.run(["/bin/ps", "-E", "-ww", "-o", "command=", "-p", str(pid)],
                                   capture_output=True, text=True, timeout=10).stdout.split()
        except (OSError, subprocess.TimeoutExpired):
            return None
    return dict(w.split("=", 1) for w in words if "=" in w)


def _environment_names(pid: int, runtime_dir: str) -> bool:
    """Whether `pid` runs with `runtime_dir` as its socket directory."""
    env = _environ(pid)
    if env is None:
        return False
    return os.path.normpath(socket_dir(env)) == os.path.normpath(runtime_dir)
