"""Self-test for `rundirs`: each way a harness can end, what may be stopped or
signalled, and the rule that nothing searches the machine for a process.

The exit routes run a real child Python, so they go through the fork, the wait
and the cleanup. A kept root of this test is removed afterwards.

The cases that exercise a signal replace `os.kill` with a recorder and `rundirs.BIN`
with a script that only logs, so a fault here cannot signal anything or reach a
real server. Decoys are shells with the command line of a remux process.

`guard_checks` scans the source of every Python file under tests/ for the rule
in `rundirs.py`'s docstring, and lists each exception with its reason.

Run: python3 tests/rundirs_selftest.py
"""
import ast, os, re, shlex, shutil, signal, subprocess, sys, tempfile, time

TESTS = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, TESTS)
import home_guard as hm  # noqa: E402
import rundirs  # noqa: E402

CHILD = f"""
import os, signal, sys
sys.path.insert(0, {TESTS!r})
from rundirs import run_dir
print("harness output")
path = run_dir("rmx-self/nested")
os.makedirs(path)
open(path + "/evidence", "w").write("kept")
"""

ROUTES = (
    ("falls off the end", "", 0, False),
    ("sys.exit(0)", "sys.exit(0)", 0, False),
    ("sys.exit(3)", "sys.exit(3)", 3, True),
    ("bare SystemExit with a message", "raise SystemExit('no socket')", 1, True),
    ("uncaught exception", "raise RuntimeError('boom')", 1, True),
    ("os._exit(2)", "os._exit(2)", 2, True),
    ("SIGTERM", "os.kill(os.getpid(), signal.SIGTERM)", -signal.SIGTERM, True),
    ("SIGKILL", "os.kill(os.getpid(), signal.SIGKILL)", -signal.SIGKILL, True),
)

PRELUDE = f"""
import os, resource, shutil, signal, subprocess, sys, threading, time
sys.path.insert(0, {TESTS!r})
import rundirs
from rundirs import run_dir, run_root
"""

LINUX = sys.platform == "linux"

fails = []


def check(ok, name):
    print("PASS" if ok else "FAIL", name)
    if not ok:
        fails.append(name)


def exit_routes():
    for name, last_line, want_code, want_kept in ROUTES:
        p = subprocess.run([sys.executable, "-c", CHILD + last_line],
                           capture_output=True, text=True, timeout=30)
        root = re.search(r"run dir: (\S+)", p.stderr).group(1)
        kept = os.path.exists(f"{root}/rmx-self/nested/evidence")
        printed = f"run dir kept: {root}" in p.stderr
        check(p.returncode == want_code, f"{name}: the exit status passes through ({p.returncode})")
        check(kept == want_kept and printed == want_kept and os.path.exists(root) == want_kept,
              f"{name}: kept={want_kept}, and the path is printed only then")
        check(p.stdout.count("harness output") == 1, f"{name}: buffered output is written once")
        shutil.rmtree(root, ignore_errors=True)


def name_checks():
    for bad in ("/tmp/x", "../x", "a/../../x"):
        try:
            rundirs.run_dir(bad)
            check(False, f"run_dir refuses {bad!r}")
        except ValueError:
            check(True, f"run_dir refuses {bad!r}")


def child(body, **kw):
    return subprocess.run([sys.executable, "-c", PRELUDE + body], capture_output=True,
                          text=True, timeout=30, **kw)


def forget(p):
    """Remove the root that the run `p` printed, if it printed one."""
    m = re.search(r"run dir: (\S+)", p.stderr)
    if m:
        shutil.rmtree(m.group(1), ignore_errors=True)


def supervisor_failures():
    """An exception after the fork must not return up the harness's own stack,
    where the harness's `except` block would run again in the supervisor."""
    guarded = """
try:
    root = run_root()
    {action}
except BaseException:
    print("harness except ran", flush=True)
    raise
"""
    p = child(guarded.format(action="shutil.rmtree(root); os._exit(0)"))
    check(p.returncode == 0 and "harness except ran" not in p.stdout,
          f"a harness that removed its own root and exited 0 stays 0 ({p.returncode}, {p.stdout!r})")
    forget(p)

    p = child(guarded.format(action="shutil.rmtree(root); os._exit(3)"))
    check(p.returncode == 3 and "harness except ran" not in p.stdout,
          f"a harness that removed its own root and exited 3 stays 3 ({p.returncode}, {p.stdout!r})")
    forget(p)

    for raised in ("RuntimeError('boom')", "SystemExit(9)", "KeyboardInterrupt()"):
        p = child(f"""
def boom(root):
    raise {raised}
rundirs.stop_servers = boom
""" + guarded.format(action="os._exit(0)"))
        check(p.returncode == 0 and "harness except ran" not in p.stdout,
              f"{raised} in the supervisor keeps the harness's status "
              f"({p.returncode}, {p.stdout!r})")
        check(raised.split("(")[0] in p.stderr, f"{raised} in the supervisor is printed")
        forget(p)

    root = tempfile.mkdtemp(prefix="rmx-self-")
    os.makedirs(f"{root}/vanishing")
    real_chmod = os.chmod

    def vanishes(path, *a, **k):
        shutil.rmtree(path, ignore_errors=True)
        raise FileNotFoundError(path)

    os.chmod = vanishes
    try:
        rundirs._remove(root)
        check(not os.path.exists(root), "_remove skips a dir that a dying pane shell deleted")
    except FileNotFoundError as e:
        check(False, f"_remove skips a dir that a dying pane shell deleted ({e!r})")
    finally:
        os.chmod = real_chmod
        shutil.rmtree(root, ignore_errors=True)


def killed_supervisor():
    """The caller's pid is the supervisor. Killing it must not leave the harness
    running and holding the caller's pipe."""
    if not LINUX:
        print("SKIP killed supervisor: macOS has no PR_SET_PDEATHSIG")
        return
    body = """
# 1, not 0: a core_pattern that pipes to a handler such as systemd-coredump
# ignores a limit of 0, and only 1 turns the dump off.
resource.setrlimit(resource.RLIMIT_CORE, (1, 1))
root = run_root()
print("ready " + root, flush=True)
time.sleep(8)
"""
    for name, stop in (("SIGKILL", lambda p: p.kill()),
                       ("SIGQUIT", lambda p: p.send_signal(signal.SIGQUIT))):
        p = subprocess.Popen([sys.executable, "-c", PRELUDE + body], stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, text=True)
        root = p.stdout.readline().split()[1]
        t0 = time.time()
        stop(p)
        p.stdout.read()
        held = time.time() - t0
        p.wait()
        check(held < 4, f"{name} of the supervisor ends the harness and frees the pipe ({held:.1f}s)")
        shutil.rmtree(root, ignore_errors=True)


def early_signal():
    """A signal that reaches the supervisor before its forwarding handlers exist
    must still be forwarded. The delay before `_wait_for` stands in for a
    supervisor that the scheduler runs late, which is how a loaded machine lost
    a SIGQUIT: the supervisor took the default action, and the harness ran on."""
    if not LINUX:
        print("SKIP early signal: the harness's death is read through PR_SET_PDEATHSIG timing")
        return
    mark = tempfile.mktemp(prefix="rmx-self-mark-")
    body = f"""
resource.setrlimit(resource.RLIMIT_CORE, (1, 1))
def note(signum, _frame):
    open({mark!r}, "w").write(signal.Signals(signum).name)
    os._exit(0)
for sig in (signal.SIGQUIT, signal.SIGTERM):
    signal.signal(sig, note)
real_wait = rundirs._wait_for
def late(pid, *a):
    time.sleep(1)
    return real_wait(pid, *a)
rundirs._wait_for = late
root = run_root()
print("ready " + root, flush=True)
time.sleep(8)
"""
    p = subprocess.Popen([sys.executable, "-c", PRELUDE + body], stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, text=True)
    root = p.stdout.readline().split()[1]
    p.send_signal(signal.SIGQUIT)
    p.stdout.read()
    p.wait()
    got = open(mark).read() if os.path.exists(mark) else "nothing"
    check(got == "SIGQUIT", f"a signal sent before the handlers exist reaches the harness ({got})")
    shutil.rmtree(root, ignore_errors=True)
    if os.path.exists(mark):
        os.unlink(mark)


def ps_lines():
    """`_split_ps_line` is how `runs` reads a command line without /proc."""
    for line, sub, want in (
            ("/x/remux kill main", "kill", ["/x/remux", "kill", "main"]),
            ("/x/remux server", "server", ["/x/remux", "server"]),
            ("/Users/a b/bin/remux server", "server", ["/Users/a b/bin/remux", "server"]),
            ("/x/remux relay --x", "relay", ["/x/remux", "relay", "--x"]),
            ("/usr/bin/vim server notes", "server", []),
            ("/x/remux serverx", "server", []),
            ("/x/remux kill server", "server", []),
            ("", "server", [])):
        got = rundirs._split_ps_line(line, sub)
        check(got == want, f"_split_ps_line({line!r}, {sub!r}) is {want} ({got})")


def spawn_before_run_dir():
    """A subprocess started first is the supervisor's child, so its exit status
    reads as 0 in the harness. A thread started first does not exist after the
    fork. Both must be refused rather than give a false pass."""
    for name, body in (
            ("a live subprocess", 'p = subprocess.Popen(["/bin/sh", "-c", "exit 7"])\nrun_dir("x")\nsys.exit(p.wait())'),
            ("a live thread", 'threading.Thread(target=time.sleep, args=(2,)).start()\nrun_dir("x")')):
        p = child(body)
        if not LINUX and "subprocess" in name:
            print(f"SKIP {name}: only /proc can list children")
            forget(p)
            continue
        check(p.returncode != 0 and "started before run_dir" in p.stderr,
              f"{name} before run_dir is refused ({p.returncode}, {p.stderr[-120:]!r})")
        check("run dir:" not in p.stderr, f"{name}: the refusal creates no root")
        forget(p)
    p = child('subprocess.run(["/bin/true"])\nrun_dir("x")')
    check(p.returncode == 0, f"a finished, reaped subprocess is fine ({p.returncode}, {p.stderr[-120:]!r})")
    forget(p)


def signal_during_cleanup():
    """After `waitpid` the harness's pid is free, so a signal must not be
    forwarded to it, and must not stop the cleanup."""
    mark = tempfile.mktemp(prefix="rmx-self-mark-")
    body = f"""
def slow(root):
    print("cleaning", flush=True)
    time.sleep(1.5)
def record(pid, signum):
    open({mark!r}, "a").write("forwarded")
rundirs.stop_servers = slow
rundirs._forward = record
run_dir("x")
os._exit(0)
"""
    p = subprocess.Popen([sys.executable, "-c", PRELUDE + body], stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True)
    p.stdout.readline()
    p.send_signal(signal.SIGTERM)
    _, err = p.communicate(timeout=10)
    check(not os.path.exists(mark), "SIGTERM during cleanup is not forwarded to the reaped pid")
    check(p.returncode == 0, f"SIGTERM during cleanup does not change the status ({p.returncode})")
    m = re.search(r"run dir: (\S+)", err)
    check(m is not None and not os.path.exists(m.group(1)), "SIGTERM during cleanup does not stop it")
    if os.path.exists(mark):
        os.unlink(mark)


class Decoys:
    """Shells with the command line of a remux process, a recorder in place of
    `os.kill`, and a logging script in place of the remux binary. `/bin/sh` runs
    the file named after the subcommand from its cwd with argv[0] set. `read`
    blocks in the shell, because a child would inherit the decoy's environment.
    The recorder raises ProcessLookupError after recording, as a signal does when
    its target exited after it was checked."""

    def __enter__(self):
        self.base = tempfile.mkdtemp(prefix="rmx-self-")
        for name in ("server", "relay"):
            with open(f"{self.base}/{name}", "w") as f:
                f.write("read x\n")
        self.stop_log = f"{self.base}/stop.log"
        fake = f"{self.base}/fake-remux"
        with open(fake, "w") as f:
            f.write(f'#!/bin/sh\necho "$1 $XDG_RUNTIME_DIR" >> {self.stop_log}\n')
        os.chmod(fake, 0o755)
        self.procs = []
        self.signalled = []
        self.real = os.kill, getattr(signal, "pidfd_send_signal", None), rundirs.BIN

        self.failure = ProcessLookupError

        def record(pid, sig):
            self.signalled.append(pid)
            raise self.failure(pid)

        def record_pidfd(fd, sig, *rest):
            with open(f"/proc/self/fdinfo/{fd}") as f:
                pid = int(re.search(r"^Pid:\s+(\d+)", f.read(), re.M).group(1))
            record(pid, sig)

        os.kill = record
        signal.pidfd_send_signal = record_pidfd
        rundirs.BIN = fake
        return self

    def __exit__(self, *_):
        os.kill, pidfd, rundirs.BIN = self.real
        if pidfd is not None:
            signal.pidfd_send_signal = pidfd
        for p in self.procs:
            p.kill()
            p.wait()
        shutil.rmtree(self.base, ignore_errors=True)

    def start(self, subcommand, runtime=None, argv0="/x/remux", tmpdir=None):
        env = {k: v for k, v in os.environ.items() if k not in ("XDG_RUNTIME_DIR", "TMPDIR")}
        env["HOME"] = self.base
        if runtime:
            env["XDG_RUNTIME_DIR"] = runtime
        if tmpdir:
            env["TMPDIR"] = tmpdir
        p = subprocess.Popen([argv0, subcommand], executable="/bin/sh", cwd=self.base, env=env,
                             start_new_session=True, stdin=subprocess.PIPE,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.procs.append(p)
        return p.pid

    def plant(self, path, pid):
        os.makedirs(path, exist_ok=True)
        open(f"{path}/remux.sock", "w").close()
        with open(f"{path}/remux.pid", "w") as f:
            f.write(f"{pid}\n")
        return path

    def run_dir(self, name, pid):
        return self.plant(f"{self.base}/{name}/run", pid)

    def stops(self):
        return open(self.stop_log).read().split("\n")[:-1] if os.path.exists(self.stop_log) else []


def stop_scope():
    """`remux stop` runs only for a server the run recorded, and only with that
    server's socket dir set explicitly."""
    if not LINUX:
        print("SKIP stop scope: the decoys need /proc to carry an environment")
        return
    uid = os.getuid()
    with Decoys() as d:
        ours_dir = f"{d.base}/ours/run"
        ours = d.start("server", ours_dir)
        foreign = d.start("server", "/run/user/0")
        relay = d.start("relay", f"{d.base}/relay-pid/run")
        # A fallback server's socket dir is exactly `$TMPDIR/remux-<uid>`. A server
        # with TMPDIR=/tmp and no XDG_RUNTIME_DIR is the user's own fallback server,
        # and planting its pid in an unrelated dir must not make it ours.
        tmp_fallback = d.start("server", tmpdir="/tmp")
        tmpdir = f"{d.base}/td"
        ours_fallback = d.start("server", tmpdir=tmpdir)
        time.sleep(0.3)
        d.run_dir("ours", ours)
        d.run_dir("foreign", foreign)
        d.run_dir("relay-pid", relay)
        d.run_dir("dead", 2 ** 22 + 7)
        d.run_dir("self", os.getpid())
        d.run_dir("tmp-fallback", tmp_fallback)
        fallback_dir = d.plant(f"{tmpdir}/remux-{uid}", ours_fallback)
        rundirs.stop_servers(d.base)
        stopped = sorted(line.split(" ", 1)[1] for line in d.stops())
        check(stopped == sorted([ours_dir, fallback_dir]),
              f"remux stop runs for the recorded servers only ({stopped})")
        check(sorted(d.signalled) == sorted([ours, ours_fallback]),
              f"only those are signalled when they outlive the stop, and a vanished one is "
              f"not an error ({d.signalled})")
        check(all(p.poll() is None for p in d.procs), "nothing was really signalled")

        before = d.stops()
        for bad in ("", None, "relative/run"):
            try:
                rundirs.stop_runtime(bad)
                check(False, f"stop_runtime refuses {bad!r}")
            except ValueError:
                check(True, f"stop_runtime refuses {bad!r} rather than fall back")
        try:
            rundirs.stop_runtime(f"/run/user/{uid}")
            check(False, "stop_runtime refuses the real socket dir")
        except AssertionError:
            check(True, "stop_runtime refuses the real socket dir")
        check(d.stops() == before, "a refused stop runs nothing")

        link = f"{d.base}/link"
        os.symlink(f"{d.base}/ours", link)
        try:
            rundirs.stop_servers(link)
            check(False, "stop_servers refuses a root that is a symlink")
        except AssertionError:
            check(True, "stop_servers refuses a root that is a symlink")
        rundirs._remove(link)
        check(os.path.islink(link) and os.path.exists(f"{ours_dir}/remux.pid"),
              "_remove leaves a symlinked root and its target alone")
    for bad in ("", "relative", "/tmp", f"/run/user/{uid}", "/run/user"):
        try:
            rundirs.stop_servers(bad)
            check(False, f"stop_servers refuses {bad!r}")
        except AssertionError:
            check(True, f"stop_servers refuses {bad!r}")


def runs_checks():
    """`runs` reads one pid's command line, with spaces in the binary's path."""
    if not LINUX:
        print("SKIP runs: the decoys need /proc")
        return
    with Decoys() as d:
        spaced = d.start("server", f"{d.base}/a/run", argv0=f"{d.base}/a b/remux")
        other = d.start("server", f"{d.base}/b/run", argv0="/usr/bin/editor")
        time.sleep(0.3)
        check(rundirs.runs(spaced, "server"), "a binary whose path holds a space is still recognised")
        check(not rundirs.runs(other, "server"), "a program that is not remux is not a server")
        check(not rundirs.runs(spaced, "relay"), "a server is not a relay")
        suffixed = d.start("server", f"{d.base}/c/run", argv0="/usr/bin/notremux")
        time.sleep(0.3)
        check(not rundirs.runs(suffixed, "server"), "a program whose name only ends in remux is not remux")


def relay_scope():
    """`kill_relays` signals only a recorded pid that is still a relay."""
    with Decoys() as d:
        relay = d.start("relay", f"{d.base}/r")
        reused = d.start("server", f"{d.base}/s")
        time.sleep(0.3)
        with open(f"{d.base}/relay.pids", "w") as f:
            f.write(f"{relay}\n{reused}\n{os.getpid()}\n{2 ** 22 + 7}\n")
        rundirs.kill_relays(d.base)
        check(d.signalled == [relay], f"only the live recorded relay is signalled ({d.signalled})")
        check(all(p.poll() is None for p in d.procs), "nothing was really signalled")
        os.unlink(f"{d.base}/relay.pids")
        d.signalled.clear()
        rundirs.kill_relays(d.base)
        check(d.signalled == [], "with nothing recorded, nothing is signalled")

        # With RMX_RUNDIR the run has opted out of a root, so neither call may
        # create one: that would fork the harness in the middle of its run.
        with open(f"{d.base}/relay.pids", "w") as f:
            f.write(f"{relay}\n")
        os.environ["RMX_RUNDIR"] = d.base
        try:
            path = rundirs.relay_pids()
            rundirs.kill_relays()
        finally:
            del os.environ["RMX_RUNDIR"]
        check(path == f"{d.base}/relay.pids" and rundirs._root is None,
              f"under RMX_RUNDIR relay_pids names that dir and creates no root ({path})")
        check(d.signalled == [relay], f"under RMX_RUNDIR kill_relays reads that dir ({d.signalled})")

        # A harness can record relays beside a second server, below the root.
        os.unlink(f"{d.base}/relay.pids")
        os.makedirs(f"{d.base}/second")
        with open(f"{d.base}/second/relay.pids", "w") as f:
            f.write(f"{relay}\n")
        d.signalled.clear()
        rundirs._clean(d.base, 1)
        check(d.signalled == [relay],
              f"the supervisor drops a relay recorded below the root ({d.signalled})")

        # A recorded pid that is now another run's relay, or a real one: its
        # argv matches, its socket dir is not under this run.
        elsewhere = d.start("relay", "/tmp/rmxr.otherrun/x/run")
        time.sleep(0.3)
        with open(f"{d.base}/relay.pids", "w") as f:
            f.write(f"{elsewhere}\n{relay}\n")
        d.signalled.clear()
        rundirs.kill_relays(d.base)
        check(d.signalled == [relay],
              f"a recorded relay outside the run is not signalled ({d.signalled}, outside {elsewhere})")
        os.unlink(f"{d.base}/relay.pids")
        os.makedirs(f"{d.base}/linked")
        with open(f"{d.base}/linked/target", "w") as f:
            f.write(f"{relay}\n")
        os.symlink(f"{d.base}/linked/target", f"{d.base}/relay.pids")
        d.signalled.clear()
        rundirs.kill_relays(d.base)
        check(d.signalled == [], f"a symlinked relay.pids is not read ({d.signalled})")

        # A signal that fails for any reason is reported and the rest go on.
        os.unlink(f"{d.base}/relay.pids")
        second = d.start("relay", f"{d.base}/r2")
        time.sleep(0.3)
        with open(f"{d.base}/relay.pids", "w") as f:
            f.write(f"{relay}\n{second}\n")
        d.signalled.clear()
        d.failure = PermissionError
        try:
            rundirs.kill_relays(d.base)
            check(d.signalled == [relay, second],
                  f"a failed signal does not stop the next ({d.signalled})")
        except OSError as e:
            check(False, f"a failed signal does not stop the next ({e!r})")
        d.failure = ProcessLookupError


def no_late_root():
    """A harness that opted out of `run_dir` without RMX_RUNDIR has no root.
    `relay_pids` and `kill_relays` must refuse rather than make one, which
    would fork the harness part way through its run."""
    for call in ("relay_pids()", "kill_relays()"):
        p = child(f"rundirs.{call}")
        check(p.returncode != 0 and "before run_dir" in p.stderr and "run dir:" not in p.stderr,
              f"{call} with no root refuses and makes none ({p.returncode}, {p.stderr[-100:]!r})")
        forget(p)
    p = child("root = run_root()\nos.environ['RMX_RUNDIR'] = '/tmp/rmx-self-optout'\n"
              "print(rundirs.relay_pids() == root + '/relay.pids', flush=True)")
    check(p.returncode == 0 and p.stdout.strip() == "True",
          f"a root that exists wins over an exported RMX_RUNDIR ({p.stdout.strip()!r})")
    forget(p)
    p = child("os.environ['RMX_RUNDIR'] = '/tmp/rmx-self-optout'\nprint(rundirs.relay_pids())")
    check(p.returncode == 0 and p.stdout.strip() == "/tmp/rmx-self-optout/relay.pids"
          and "run dir:" not in p.stderr, f"under RMX_RUNDIR relay_pids names that dir ({p.stdout.strip()})")


def stderr_closed():
    """A caller that stops reading stderr must not turn the supervisor's report
    into an exception on the harness's stack."""
    body = """
try:
    run_root()
    time.sleep(0.5)
    os._exit(3)
except BaseException:
    print("harness except ran", flush=True)
    raise
"""
    p = subprocess.Popen([sys.executable, "-c", PRELUDE + body], stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True)
    root = p.stderr.readline().split()[-1]
    p.stderr.close()
    out = p.stdout.read()
    p.wait()
    check(p.returncode == 3 and "harness except ran" not in out,
          f"a closed stderr keeps the harness's status and its except block in the harness "
          f"({p.returncode}, {out!r})")
    shutil.rmtree(root, ignore_errors=True)


# --------------------------------------------------------------------------
# The guard: what `rundirs.py`'s docstring promises, checked on the source of
# every Python file under tests/. It parses commands with `home_guard`'s parser,
# the one the runtime check uses, so the two cannot drift apart.

# Programs that signal a process, or find one to signal.
SIGNAL_WORDS = {"kill", "fuser", "pidof"}
# Programs that only read the process table.
COMMAND_WORDS = {"ps", "pgrep"}
# Never allowed anywhere: a process matched by name.
NEVER_WORDS = {"pkill", "killall"}
# Calls that signal directly, or run a shell that could.
SIGNAL_ATTRS = {"kill", "killpg", "pidfd_send_signal", "pthread_kill", "system", "popen"}
SIGNAL_MODULES = ("os", "signal")
CTYPES_LOADERS = {"CDLL", "PyDLL", "LoadLibrary"}
# Calls that start a program, and how each takes its command:
# "argv" a list, or a string that is one program path (or a shell string with shell=True);
# "split" a string that the call splits into words itself, plus an optional args list;
# "shell" a shell string; "path" a program path, with its arguments after it.
LAUNCHERS = {
    **{f"subprocess.{c}": ("argv", 0) for c in ("run", "Popen", "call", "check_call", "check_output")},
    "subprocess.getoutput": ("shell", 0), "subprocess.getstatusoutput": ("shell", 0),
    "os.system": ("shell", 0), "os.popen": ("shell", 0),
    **{f"os.exec{s}": ("path", 0) for s in ("l", "le", "lp", "lpe", "v", "ve", "vp", "vpe")},
    **{f"os.spawn{s}": ("path", 1) for s in ("l", "le", "lp", "lpe", "v", "ve", "vp", "vpe")},
    "os.posix_spawn": ("path", 0), "os.posix_spawnp": ("path", 0),
    "asyncio.create_subprocess_exec": ("path", 0), "asyncio.create_subprocess_shell": ("shell", 0),
    "pexpect.run": ("split", 0), "pexpect.spawn": ("split", 0), "pexpect.spawnu": ("split", 0),
    "pexpect.popen_spawn.PopenSpawn": ("split", 0), "pty.spawn": ("argv", 0),
}
# rundirs' private signal helpers, and the only function that may call each.
PRIVATE_SINKS = {"_forward": {"_wait_for"}, "_exit_as": {"_supervise"}, "_die_with": {"_supervise"}}
NEVER_TEXT = re.compile(r"\bpkill\b|\bkillall\b")
FIXED_RUN_DIR = "/tmp/rmx"
PROC = re.compile(r"/proc(/|\b)")
UNKNOWN = "\0"
SCAN_SKIP = {"rundirs_selftest.py": "holds the snippets that the guard is tested against"}


def _docstring_ids(tree):
    ids = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            for item in node.body:
                if isinstance(item, ast.Expr) and isinstance(item.value, ast.Constant) \
                        and isinstance(item.value.value, str):
                    ids.add(id(item.value))
    return ids


def _known(word):
    """A source word, or None when part of it is computed at run time, in Python
    (UNKNOWN) or by the shell (`home_guard`'s marker)."""
    return None if word is None or UNKNOWN in word or hm._MARK in word else word


def findings_for(name, text):
    """[(rule, scope, detail)] for the file `name` (relative to tests/). The
    scope is the qualified name of the enclosing definition, with `#n` on the
    n-th definition of the same name in one place, so a second `def main` or a
    method `X.main` is never the same scope as the first `main`."""
    try:
        tree = ast.parse(text)
    except SyntaxError as e:
        return [("parse", "<module>", str(e))]
    docs = _docstring_ids(tree)
    modules = {}        # local name -> module
    imported = {}       # local name -> "module.name"
    starred = set()     # modules imported with `*`
    handles = set()     # names bound to a ctypes library
    handle_attrs = set()  # attribute names bound to a ctypes library
    module_values = {}  # module-level name -> its value node
    found = []

    for node in tree.body:
        if isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Name):
                    module_values[t.id] = node.value
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for a in node.names:
                # `import a.b` binds `a` to the package; `import a.b as c` binds `c` to `a.b`.
                if a.asname:
                    modules[a.asname] = a.name
                else:
                    top = a.name.split(".")[0]
                    modules[top] = top
                if a.name.split(".")[0] == "psutil":
                    found.append(("never", "<module>", "import psutil"))
        elif isinstance(node, ast.ImportFrom) and node.module:
            if node.module.split(".")[0] == "psutil":
                found.append(("never", "<module>", "from psutil import"))
            for a in node.names:
                if a.name == "*":
                    starred.add(node.module)
                else:
                    imported[a.asname or a.name] = f"{node.module}.{a.name}"
        elif isinstance(node, ast.Assign) and isinstance(node.value, ast.Call):
            func = node.value.func
            loader = func.attr if isinstance(func, ast.Attribute) else getattr(func, "id", "")
            if loader in CTYPES_LOADERS:
                for t in node.targets:
                    if isinstance(t, ast.Name):
                        handles.add(t.id)
                    elif isinstance(t, ast.Attribute):
                        handle_attrs.add(t.attr)

    def qualified(func):
        """`module.name` of a called function, through import aliases."""
        if isinstance(func, ast.Attribute):
            if isinstance(func.value, ast.Name):
                mod = modules.get(func.value.id)
                if mod:
                    return f"{mod}.{func.attr}"
                target = imported.get(func.value.id)
                if target:
                    return f"{target}.{func.attr}"
            if isinstance(func.value, ast.Attribute) and isinstance(func.value.value, ast.Name):
                mod = modules.get(func.value.value.id)
                return f"{mod}.{func.value.attr}.{func.attr}" if mod else None
            return None
        if isinstance(func, ast.Name):
            if func.id in imported:
                return imported[func.id]
            for mod in starred:
                if f"{mod}.{func.id}" in LAUNCHERS:
                    return f"{mod}.{func.id}"
        return None

    def is_signal_ref(node):
        if isinstance(node, ast.Attribute) and node.attr in SIGNAL_ATTRS and isinstance(node.value, ast.Name) \
                and modules.get(node.value.id) in SIGNAL_MODULES:
            return True
        if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load):
            target = imported.get(node.id, "")
            if target.split(".")[0] in SIGNAL_MODULES and target.split(".")[-1] in SIGNAL_ATTRS:
                return True
            if node.id in SIGNAL_ATTRS and node.id not in imported and starred & set(SIGNAL_MODULES):
                return True
        if isinstance(node, ast.Attribute) and node.attr in ("kill", "killpg"):
            v = node.value
            if isinstance(v, ast.Name) and v.id in handles:
                return True
            if isinstance(v, ast.Attribute) and v.attr in handle_attrs:
                return True
            if isinstance(v, ast.Call):
                f = v.func
                return (f.attr if isinstance(f, ast.Attribute) else getattr(f, "id", "")) in CTYPES_LOADERS
        return False

    def as_text(node):
        """A literal string, with UNKNOWN for each computed part, or None."""
        if isinstance(node, ast.Name) and node.id in module_values:
            node = module_values[node.id]
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            return node.value
        if isinstance(node, ast.JoinedStr):
            return "".join(v.value if isinstance(v, ast.Constant) else f" {UNKNOWN} " for v in node.values)
        return None

    def as_words(node):
        """A literal list as words, None for a computed element, or None."""
        if isinstance(node, ast.Name) and node.id in module_values:
            node = module_values[node.id]
        if not isinstance(node, (ast.List, ast.Tuple)):
            return None
        words = []
        for e in node.elts:
            if isinstance(e, ast.Starred):
                words.append(None)
                break
            words.append(as_text(e))
        return words

    def env_scoped(call):
        """Whether `env=` names an environment built for the call, not the caller's."""
        def caller_environ(v):
            return isinstance(v, ast.Attribute) and v.attr == "environ" and isinstance(v.value, ast.Name) \
                and modules.get(v.value.id) == "os"

        def names_runtime(keys):
            return any(isinstance(k, ast.Constant) and k.value in ("XDG_RUNTIME_DIR", "TMPDIR") for k in keys)

        for k in call.keywords:
            if k.arg != "env":
                continue
            v = k.value
            if isinstance(v, ast.Constant) and v.value is None or caller_environ(v):
                return False
            if isinstance(v, ast.Call):
                f = v.func
                if isinstance(f, ast.Attribute) and f.attr == "copy" and caller_environ(f.value):
                    return False
                if isinstance(f, ast.Name) and f.id == "dict" and v.args and caller_environ(v.args[0]):
                    return any(kw.arg in ("XDG_RUNTIME_DIR", "TMPDIR") for kw in v.keywords)
            if isinstance(v, ast.Dict) and any(key is None and caller_environ(val)
                                               for key, val in zip(v.keys, v.values)):
                return names_runtime(v.keys)
            return True
        return False

    def judge(argv, call, scope):
        prog = _known(argv[0]) if argv else None
        base = os.path.basename(prog) if prog else None
        if base in NEVER_WORDS:
            found.append(("never", scope, f"runs {base}"))
        elif base in SIGNAL_WORDS:
            found.append(("signal", scope, f"runs {base}"))
        elif base in COMMAND_WORDS:
            found.append(("command", scope, f"runs {base}"))
        sub = _known(argv[1]) if len(argv) >= 2 else None
        if sub in ("stop", "restart") and (prog is None or hm._is_remux_program(prog)) \
                and not env_scoped(call):
            found.append(("stop", scope, f"remux {sub} without a scoped env="))

    def launch(call, scope, kind, index):
        args = list(call.args)
        keywords = {k.arg: k.value for k in call.keywords if k.arg}
        command = args[index] if len(args) > index else keywords.get("args" if kind == "argv" else "command")
        if command is None:
            return
        shell = kind == "shell" or any(k.arg == "shell" and getattr(k.value, "value", False) for k in call.keywords)
        text, words = as_text(command), as_words(command)
        if kind == "path":
            rest = args[index + 1:]
            if rest and isinstance(rest[0], (ast.List, ast.Tuple)):
                tail = (as_words(rest[0]) or [None])[1:]
            else:
                tail = [as_text(a) for a in rest]
            lines = [[text] + tail] if text is not None else []
        elif kind == "split":
            extra = args[index + 1] if len(args) > index + 1 else keywords.get("args")
            extra_words = as_words(extra) if extra is not None else None
            if text is not None:
                try:
                    lines = [shlex.split(text) + (extra_words or [])]
                except ValueError:
                    lines = [text.split() + (extra_words or [])]
            elif words is not None:
                lines = [words]
            else:
                lines = [[None] + (extra_words or [])] if extra_words else []
        elif shell and (text is not None or words):
            # shell=True runs a string, or the first item of a list, as the script.
            script = text if text is not None else words[0]
            if script is not None:
                for *_, argv in hm.script_commands(script):
                    judge(argv, call, scope)
            return
        elif text is not None:
            lines = [[text]]
        elif words is not None:
            lines = [words]
        else:
            return
        for line in lines:
            for *_, argv in hm.commands(line):
                judge(argv, call, scope)

    def visit(node, scope, seen):
        for item in ast.iter_child_nodes(node):
            inner, inner_seen = scope, seen
            if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                base = item.name if scope == "<module>" else f"{scope}.{item.name}"
                seen[base] = seen.get(base, 0) + 1
                inner = base if seen[base] == 1 else f"{base}#{seen[base]}"
                inner_seen = {}
            if isinstance(item, ast.Constant) and isinstance(item.value, str) and id(item) not in docs:
                s = item.value
                if NEVER_TEXT.search(s):
                    found.append(("never", scope, f"{s[:60]!r} names processes by pattern"))
                if s.startswith(FIXED_RUN_DIR):
                    found.append(("never", scope, f"{s[:60]!r} is a fixed run dir"))
                if PROC.search(s):
                    found.append(("proc", scope, f"{s[:60]!r}"))
            if is_signal_ref(item):
                found.append(("signal", scope, ast.unparse(item)[:60]))
            if isinstance(item, ast.Call):
                f = item.func
                if isinstance(f, ast.Name) and f.id == "getattr" and len(item.args) >= 2 \
                        and isinstance(item.args[1], ast.Constant) and item.args[1].value in SIGNAL_ATTRS:
                    found.append(("signal", scope, ast.unparse(item)[:60]))
                target = qualified(f)
                if target in LAUNCHERS:
                    launch(item, scope, *LAUNCHERS[target])
            sink = item.id if isinstance(item, ast.Name) else item.attr if isinstance(item, ast.Attribute) else None
            if sink in PRIVATE_SINKS and scope not in PRIVATE_SINKS[sink] and scope != sink:
                found.append(("sink", scope, f"{sink} used outside {sorted(PRIVATE_SINKS[sink])}"))
            visit(item, inner, inner_seen)

    visit(tree, "<module>", {})
    return found


def unexplained_in(rel, text):
    """(findings ALLOW does not explain, ALLOW keys that explained something).
    An entry explains exactly its count of findings in exactly its scope, so a
    second hit there is a finding of its own."""
    allowed = ALLOW.get(rel, {})
    counts = {}
    problems = []
    for rule, scope, detail in findings_for(rel, text):
        key = f"{rule}:{scope}"
        if rule != "never" and key in allowed:
            counts.setdefault(key, []).append(detail)
        else:
            problems.append(f"{rel}: {rule} in {scope}: {detail}")
    for key, details in counts.items():
        want = allowed[key][0]
        if len(details) != want:
            problems.append(f"{rel}: {key} has {len(details)} findings, the allowlist covers {want}: {details}")
    return problems, set(counts)


def tree_findings():
    """Every unexplained finding under tests/, and every ALLOW entry that
    explains nothing, so the list stays exact."""
    unexplained, used = [], set()
    for dirpath, dirnames, files in os.walk(TESTS):
        dirnames[:] = [d for d in dirnames if d != "__pycache__"]
        for file in sorted(files):
            if not file.endswith(".py"):
                continue
            rel = os.path.relpath(os.path.join(dirpath, file), TESTS)
            if rel in SCAN_SKIP:
                continue
            with open(os.path.join(dirpath, file)) as f:
                problems, keys = unexplained_in(rel, f.read())
            unexplained += problems
            used |= {(rel, k) for k in keys}
    for rel, entries in ALLOW.items():
        for key in entries:
            if (rel, key) not in used:
                unexplained.append(f"{rel}: allowlist entry {key!r} matches nothing")
    return unexplained


SNIPPETS = (
    ("from os import kill", "from os import kill\nkill(1, 9)\n"),
    ("import os as o", "import os as o\no.kill(1, 9)\n"),
    ("k = os.kill", "import os\nk = os.kill\nk(1, 9)\n"),
    ("getattr(os, 'kill')", "import os\ngetattr(os, 'kill')(1, 9)\n"),
    ("a helper calling _forward", "def reap(pid):\n    _forward(pid, 9)\n"),
    ("subprocess kill", "import subprocess\nsubprocess.run(['kill', '-9', '1'])\n"),
    ("/bin/kill", "import subprocess\nsubprocess.run(['/bin/kill', '1'])\n"),
    ("shell=True kill", "import subprocess\nsubprocess.run('true; kill -9 1', shell=True)\n"),
    ("os.system", "import os\nos.system('true')\n"),
    ("ctypes kill", "import ctypes\nctypes.CDLL(None).kill(1, 9)\n"),
    ("ctypes handle kill", "import ctypes\nlibc = ctypes.CDLL(None)\nlibc.kill(1, 9)\n"),
    ("[BIN, 'stop']", "import subprocess\nsubprocess.run([BIN, 'stop'])\n"),
    ("ps -axo", "import subprocess\nsubprocess.run(['ps', '-axo', 'pid='])\n"),
    ("os.walk('/proc')", "import os\nos.walk('/proc')\n"),
    ("Path('/proc').iterdir()", "from pathlib import Path\nPath('/proc').iterdir()\n"),
    ("os.listdir(P), P = '/proc'", "import os\nP = '/proc'\nos.listdir(P)\n"),
    ("fuser -k", "import subprocess\nsubprocess.run(['fuser', '-k', 'x'])\n"),
    ("os.killpg", "import os\nos.killpg(1, 9)\n"),
    ("from subprocess import run, pkill", "from subprocess import run\nrun(['pkill', 'x'])\n"),
    ("killall", "import subprocess\nsubprocess.Popen(['killall', 'remux'])\n"),
    ("pgrep", "import subprocess\nsubprocess.check_output(['pgrep', 'remux'])\n"),
    ("a fixed run dir", "RUNDIR = '/tmp/rmx-foo'\n"),
    ("a fixed f-string run dir", "import os\nRUNDIR = f'/tmp/rmx{os.getpid()}'\n"),
    ("kill inside sh -c", "import subprocess\nsubprocess.run(['sh', '-c', 'sleep 1; kill -9 1'])\n"),
    ("kill past /usr/bin/env", "import subprocess\nsubprocess.run(['/usr/bin/env', 'kill', '1'])\n"),
    ("kill past timeout N", "import subprocess\nsubprocess.run(['timeout', '5', 'kill', '1'])\n"),
    ("kill past nice", "import subprocess\nsubprocess.run(['nice', '-n', '5', 'kill', '1'])\n"),
    ("kill past setsid", "import subprocess\nsubprocess.run(['setsid', '-f', 'kill', '1'])\n"),
    ("kill past nohup", "import subprocess\nsubprocess.run(['nohup', 'kill', '1'])\n"),
    ("a module-level command", "import subprocess\nCMD = ['kill', '-9', '1']\nsubprocess.run(CMD)\n"),
    ("os.execv kill", "import os\nos.execv('/bin/kill', ['kill', '1'])\n"),
    ("os.spawnv kill", "import os\nos.spawnv(os.P_WAIT, '/bin/kill', ['kill', '1'])\n"),
    ("os.posix_spawn kill", "import os\nos.posix_spawn('/bin/kill', ['kill', '1'], {})\n"),
    ("asyncio exec kill", "import asyncio\nasyncio.create_subprocess_exec('kill', '1')\n"),
    ("asyncio shell kill", "import asyncio\nasyncio.create_subprocess_shell('kill -9 1')\n"),
    ("pexpect.run kill", "import pexpect\npexpect.run('kill -9 1')\n"),
    ("pexpect.spawn kill", "import pexpect\npexpect.spawn('kill -9 1')\n"),
    ("pty.spawn kill", "import pty\npty.spawn(['kill', '1'])\n"),
    ("import psutil", "import psutil\n"),
    ("remux stop in a shell string", "import subprocess\nsubprocess.run('remux stop', shell=True)\n"),
    ("remux stop in sh -c", "import subprocess\nsubprocess.run(['sh', '-c', 'remux stop'])\n"),
    ("remux stop in pexpect.spawn", "import pexpect\npexpect.spawn('remux stop')\n"),
    ("remux-dev restart", "import subprocess\nsubprocess.run(['/x/remux-dev', 'restart'])\n"),
    ("stop with env=os.environ", "import os, subprocess\nsubprocess.run([BIN, 'stop'], env=os.environ)\n"),
    ("kill inside bash -lc", "import subprocess\nsubprocess.run(['bash', '-lc', 'kill -9 1'])\n"),
    ("kill inside sh -ec", "import subprocess\nsubprocess.run(['sh', '-ec', 'true; kill 1'])\n"),
    ("kill past env -i", "import subprocess\nsubprocess.run(['env', '-i', 'kill', '1'])\n"),
    ("kill past exec in a shell string", "import subprocess\nsubprocess.run('exec kill 1', shell=True)\n"),
    ("kill past command in a shell string", "import subprocess\nsubprocess.run('command kill 1', shell=True)\n"),
    ("keyword args=", "import subprocess\nsubprocess.Popen(args=['kill', '1'])\n"),
    ("pexpect.spawn(BIN, ['stop'])", "import pexpect\npexpect.spawn(BIN, ['stop'])\n"),
    ("pexpect.spawnu kill", "import pexpect\npexpect.spawnu('kill -9 1')\n"),
    ("PopenSpawn kill", "from pexpect.popen_spawn import PopenSpawn\nPopenSpawn('kill -9 1')\n"),
    ("pexpect.popen_spawn.PopenSpawn kill",
     "import pexpect.popen_spawn\npexpect.popen_spawn.PopenSpawn('kill -9 1')\n"),
    ("from os import * then kill()", "from os import *\nkill(1, 9)\n"),
    ("ctypes kill on an attribute",
     "import ctypes\nclass X:\n    def __init__(self):\n        self.libc = ctypes.CDLL(None)\n"
     "    def f(self):\n        self.libc.kill(1, 9)\n"),
    ("a shell=True f-string with kill",
     "import subprocess\nsubprocess.run(f'cd {d}; kill -9 {pid}', shell=True)\n"),
    ("a shell=True f-string with remux stop",
     "import subprocess\nsubprocess.run(f'cd {d}; remux stop', shell=True)\n"),
    ("a shell=True f-string with {BIN} stop", "import subprocess\nsubprocess.run(f'{BIN} stop', shell=True)\n"),
    ("stop with env=dict(os.environ)",
     "import os, subprocess\nsubprocess.run([BIN, 'stop'], env=dict(os.environ))\n"),
    ("stop with env={**os.environ}",
     "import os, subprocess\nsubprocess.run([BIN, 'stop'], env={**os.environ})\n"),
    ("stop with env=os.environ.copy()",
     "import os, subprocess\nsubprocess.run([BIN, 'stop'], env=os.environ.copy())\n"),
    ("an f-string script in sh -c", "import subprocess\nsubprocess.run(['sh', '-c', f'kill {pid}'])\n"),
    ("a list with shell=True", "import subprocess\nsubprocess.Popen(['remux stop'], shell=True)\n"),
    ("kill after an export in a script", "import subprocess\nsubprocess.run('export X=1; kill 1', shell=True)\n"),
    ("remux stop through eval", "import subprocess\nsubprocess.run('eval remux stop', shell=True)\n"),
    ("kill after a case pattern inside $( )",
     "import subprocess\nsubprocess.run('echo $(case x in x) kill 1;; esac)', shell=True)\n"),
    ("a substituted program given stop", "import subprocess\nsubprocess.run('$(command -v remux) stop', shell=True)\n"),
    ("a backtick program given stop", "import subprocess\nsubprocess.run('`which remux` stop', shell=True)\n"),
    ("kill through eval", "import subprocess\nsubprocess.run(['sh', '-c', 'eval kill 1'])\n"),
)

# A hit of each rule, for the probes below. Each imports what it uses under its
# own name, so it is caught whatever the file imports.
HITS = {
    "signal": "import os as _evil_os\n_evil_os.kill(1, 9)\n",
    "proc": "import os as _evil_os\n_evil_os.listdir('/proc')\n",
    "command": "import subprocess as _evil_sp\n_evil_sp.run(['ps', '-ax'])\n",
    "stop": "import subprocess as _evil_sp\n_evil_sp.run([BIN, 'stop'])\n",
    "sink": "_forward(1, 9)\n",
}


def _indent(text, n):
    return "".join(" " * n + line + "\n" for line in text.splitlines())


def _scope_children(node):
    """The definitions directly inside `node`, in source order, through any
    statement that is not itself a definition."""
    out = []
    for child in ast.iter_child_nodes(node):
        if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            out.append(child)
        else:
            out += _scope_children(child)
    return out


def _inject(text, scope, hit):
    """`text` with `hit` appended to the body of the definition `scope` names."""
    tree = ast.parse(text)
    node = tree
    for part in scope.split("."):
        name, _, n = part.partition("#")
        node = [c for c in _scope_children(node) if c.name == name][int(n or 1) - 1]
    node.body.extend(ast.parse(hit).body)
    return ast.unparse(tree)


def probes(rel, key):
    """Ways new code could hide behind the allowlist entry `key` in `rel`:
    another definition with its name, a method with its name, and one more hit
    inside the definition it names (or at module level)."""
    with open(os.path.join(TESTS, rel)) as f:
        text = f.read()
    rule, _, scope = key.partition(":")
    hit = HITS[rule]
    if scope == "<module>":
        return {"another module-level hit": text + "\n" + hit,
                "the same hit in a class body": text + "\n\nclass _EvilC:\n" + _indent(hit, 4)}
    name = scope.rsplit(".", 1)[-1].partition("#")[0]
    return {f"another def {name}": text + f"\n\ndef {name}():\n" + _indent(hit, 4),
            f"a method {name}": text + f"\n\nclass _EvilC:\n    def {name}(self):\n" + _indent(hit, 8),
            f"one more hit inside {scope}": _inject(text, scope, hit)}


EVIL = {
    "signal": "\n\ndef _evil():\n    import subprocess\n    subprocess.run(['kill', '-9', '1'])\n",
    "signal-call": "\n\ndef _evil():\n    import os\n    os.kill(1, 9)\n",
    "proc": "\n\ndef _evil():\n    import os\n    os.listdir('/proc')\n",
    "command": "\n\ndef _evil():\n    import subprocess\n    subprocess.run(['ps', '-ax'])\n",
    "stop": "\n\ndef _evil():\n    import subprocess\n    subprocess.run([BIN, 'stop'])\n",
}


def guard_checks():
    for label, text in SNIPPETS:
        check(findings_for("pty/snippet.py", text), f"the guard catches {label}")
    check(findings_for("rundirs.py", "def reap(pid):\n    _forward(pid, 9)\n"),
          "the guard catches a new caller of _forward inside rundirs.py too")
    clean = ("import os, subprocess\n"
             "def f(p):\n    p.kill()\n    subprocess.run([BIN, 'stop'], env=e)\n"
             "    subprocess.run([BIN, 'stop'], env={**os.environ, 'XDG_RUNTIME_DIR': d})\n"
             "    subprocess.run(['ls'])\n    subprocess.run(['sh', '-c', 'echo hi'])\n"
             "    subprocess.run(['bash', '-lc', 'echo hi'])\n"
             "    subprocess.run('export X=1; unset Y; Z=2; echo hi', shell=True)\n")
    check(not findings_for("pty/clean.py", clean),
          "the guard accepts a handle's kill, an env-scoped stop and ordinary commands")
    check(not findings_for("pty/doc.py", '"""Uses pkill and /proc and /tmp/rmx-x."""\n# pkill /proc\n'),
          "the guard skips docstrings and comments")
    # New code of each kind, appended to every file with exceptions, is red.
    escaped = []
    for rel in sorted(ALLOW):
        with open(os.path.join(TESTS, rel)) as f:
            text = f.read()
        for rule, evil in EVIL.items():
            problems, _ = unexplained_in(rel, text + evil)
            if not any(" in _evil: " in p for p in problems):
                escaped.append(f"{rel} {rule}")
    check(not escaped, f"no allowlist entry covers new code in its file ({escaped})")
    # Code that reuses an allowlisted name, or adds to an allowlisted definition,
    # is red too. Every probe is parsed, never run.
    escaped = []
    total = 0
    for rel in sorted(ALLOW):
        for key in ALLOW[rel]:
            for label, text in probes(rel, key).items():
                total += 1
                problems, _ = unexplained_in(rel, text)
                if not problems:
                    escaped.append(f"{rel} {key}: {label}")
    check(not escaped, f"no allowlist entry covers a reused name or a second hit "
                       f"({len(escaped)} of {total} escaped: {escaped[:6]})")
    problems = tree_findings()
    check(not problems, "every file under tests/ keeps the rule, with each exception listed:\n  "
          + "\n  ".join(problems))


# Each exception as file -> {"rule:scope": (count, reason)}. The scope is one
# definition, by qualified name, and the count is exactly how many findings of
# that rule it holds, so a second hit there, a second definition with the same
# name, or a method of that name is not covered.
ALLOW = {
    "rundirs.py": {
        "signal:_die_with": (1, "the harness signals itself when its supervisor is already gone"),
        "signal:_forward": (1, "the supervisor's own child, before it is reaped"),
        "signal:_exit_as": (1, "the supervisor signals itself to report the harness's death"),
        "signal:kill_wedged_server": (1, "a pid the run's server recorded, verified first"),
        "signal:_kill_relay": (2, "a pid the run's shim recorded, verified as a relay under the run, "
                                  "through a pidfd where there is one and os.kill where there is not"),
        "proc:_refuse_started_work": (1, "this process's own children"),
        "proc:_argv": (2, "the command line of one recorded pid, and whether /proc exists"),
        "proc:_environ": (2, "the environment of one recorded pid, and whether /proc exists"),
        "command:_argv": (1, "`ps` for one recorded pid where /proc is absent"),
        "command:_environ": (1, "`ps -E` for one recorded pid where /proc is absent"),
    },
    "frame/harness.py": {"signal:_on_signal": (1, "the harness signals itself")},
    "frame/agents_panel.py": {
        "command:pids_matching": (1, "`pgrep` lists stand-ins by their BINDIR path, read for the checks"),
        "command:ps_field": (1, "`ps` reads one field of one pid"),
        "command:started_by_this_run": (1, "`ps -E` reads one pid's environment where /proc is absent"),
        "proc:proc_cmdline": (1, "one pid's cmdline"),
        "proc:proc_pgid": (1, "one pid's stat"),
        "proc:proc_state": (1, "one pid's stat"),
        "proc:started_by_this_run": (2, "one pid's environ, and whether /proc exists"),
        "signal:main": (1, "only a stand-in that also carries this run's XDG_RUNTIME_DIR"),
    },
    "frame/disconnect_keeps_panes.py": {
        "signal:alive": (1, "signal 0, which only asks whether a pid exists"),
    },
    "frame/harness_guards_selftest.py": {"proc:<module>": (1, "a shell lists its own fds")},
    "frame/msg_identity_reuse.py": {
        "proc:main.set_last_pid": (1, "writes ns_last_pid so the kernel reuses a pid"),
    },
    "frame/msg_remote.py": {
        "command:carries_this_runs_shim_env": (1, "`ps -E` reads one shim's environment where /proc is absent"),
        "proc:<module>": (1, "whether /proc/self exists"),
        "proc:alive": (1, "whether one pid exists"),
        "proc:carries_this_runs_shim_env": (2, "one recorded shim's environ, and whether /proc exists"),
        "proc:check_proc": (1, "message text"),
        "proc:under_sshd": (2, "one pid's stat and status, walking up from this process"),
        "signal:signal_relay": (1, "a shim this run recorded, verified by RMX_SHIM_PIDS in its environment"),
    },
    "frame/msg_retry.py": {
        "proc:<module>": (1, "message text"),
        "proc:cpu_seconds": (1, "the run's own server's stat, for its CPU time"),
    },
    "frame/pane_fd_hygiene.py": {
        "proc:fd_table": (1, "one pane shell's fd table"),
        "proc:main": (2, "whether /proc/self/fd exists, and a message naming it"),
    },
    "frame/pane_fd_isolation.py": {"proc:ask_for_fds": (1, "a pane's shell lists its own fds")},
    "frame/ssh_shim.py": {"proc:<module>": (1, "the shim lists its own fds")},
    "pty/agent_resume_real_e2e.py": {
        "command:processes_with_home": (1, "the macOS form of the HOME scan"),
        "command:Run.resumed_argv": (1, "`ps` reads one pid's arguments"),
        "proc:processes_with_home": (3, "a SCAN for processes whose HOME is this run's throwaway home; "
                                        "opt-in real-agent run"),
        "proc:Run.resumed_argv": (2, "one pid's cmdline, and whether /proc exists"),
        "signal:end_processes": (1, "those processes, after it refuses any HOME outside this run"),
    },
    "pty/agent_session_hooks.py": {
        "command:processes_with_home": (1, "the macOS form of the HOME scan"),
        "proc:processes_with_home": (3, "a SCAN for processes whose HOME is this run's throwaway home; "
                                        "real-binary modes"),
        "signal:end_processes_with_home": (2, "those processes, SIGTERM then SIGKILL, after it refuses "
                                              "any HOME outside this run"),
    },
    "pty/fakeclaude.py": {
        "proc:main.descendants": (1, "the children of a process this stub started"),
        "signal:main.run_bounded": (1, "that process and its descendants, at the hook's timeout"),
    },
    "pty/server_daemonized.py": {
        "command:darwin_proc_stat": (1, "`ps` for one pid where /proc is absent"),
        "proc:proc_stat": (1, "one pid's stat"),
        "signal:main": (1, "SIGHUP to the process group of the client this harness spawned"),
        "signal:stop_server": (2, "the server whose pid file `server_pid` verified, SIGTERM then SIGKILL"),
    },
    "pty/skew_keeps_old_server.py": {
        "command:darwin_remux_pids": (1, "the macOS form of the scan below"),
        "proc:remux_pids": (3, "a SCAN for servers whose environment names this run's socket dir, read "
                               "for the checks only; it signals nothing"),
    },
}


def main():
    name_checks()
    exit_routes()
    supervisor_failures()
    stderr_closed()
    killed_supervisor()
    early_signal()
    spawn_before_run_dir()
    signal_during_cleanup()
    stop_scope()
    runs_checks()
    ps_lines()
    relay_scope()
    no_late_root()
    guard_checks()
    print("\nFAILED: " + "; ".join(fails) if fails else "\nALL PASS")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
