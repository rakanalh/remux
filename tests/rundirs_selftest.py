"""Self-test for `rundirs`: each way a harness can end, and what a sweep must leave.

The exit routes run a real child Python, so they go through the fork, the wait
and the cleanup. A kept root of this test is removed afterwards.

The sweep cases set `rundirs.BASE` to a private directory. A sweep of the real
`/tmp` with a short age limit would delete another run's kept failure.

Run: python3 tests/rundirs_selftest.py
"""
import os, re, shutil, signal, subprocess, sys, tempfile, time

TESTS = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, TESTS)
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


def make_root(base, name, age_hours=100):
    root = f"{base}/{name}"
    os.makedirs(root)
    owner = f"{root}/.owner"
    open(owner, "w").close()
    old = time.time() - age_hours * 3600
    os.utime(owner, (old, old))
    return root


def sweep_rules():
    base = tempfile.mkdtemp(prefix="rmx-self-")
    held = None
    try:
        rundirs.BASE = base
        abandoned = make_root(base, "rmxr.abandon1")
        os.makedirs(f"{abandoned}/ro/inner")
        os.chmod(f"{abandoned}/ro", 0o500)
        young = make_root(base, "rmxr.younger1", age_hours=1)
        live = make_root(base, "rmxr.liverun1")
        held = subprocess.Popen(
            [sys.executable, "-c",
             "import fcntl, sys, time\n"
             f"f = open({live + '/.owner'!r})\n"
             "fcntl.flock(f, fcntl.LOCK_EX)\n"
             "print('locked', flush=True)\n"
             "time.sleep(60)\n"],
            stdout=subprocess.PIPE, text=True)
        held.stdout.readline()
        no_owner = f"{base}/rmxr.noowner1"
        os.makedirs(no_owner)
        open(f"{no_owner}/evidence", "w").close()
        other_name = make_root(base, "rmx-fixed")
        long_name = make_root(base, "rmxr.toolong123")
        target = make_root(base, "target")
        link = f"{base}/rmxr.symlink1"
        os.symlink(target, link)

        rundirs._sweep("")

        check(not os.path.exists(abandoned), "an old, unlocked root is removed, read-only dir included")
        check(os.path.exists(f"{young}/.owner"), "a root younger than the limit stays")
        check(os.path.exists(f"{live}/.owner"), "a root whose lock is held stays")
        check(os.path.exists(f"{no_owner}/evidence"), "a directory without .owner stays")
        check(os.path.exists(f"{other_name}/.owner"), "a fixed rmx- dir stays")
        check(os.path.exists(f"{long_name}/.owner"), "a name outside the format stays")
        check(os.path.islink(link) and os.path.exists(f"{target}/.owner"), "a symlink and its target stay")
    finally:
        if held:
            held.kill()
            held.wait()
        shutil.rmtree(base, ignore_errors=True)


def name_checks():
    for bad in ("/tmp/x", "../x", "a/../../x"):
        try:
            rundirs.run_dir(bad)
            check(False, f"run_dir refuses {bad!r}")
        except ValueError:
            check(True, f"run_dir refuses {bad!r}")


PRELUDE = f"""
import os, resource, shutil, signal, subprocess, sys, threading, time
sys.path.insert(0, {TESTS!r})
import rundirs
from rundirs import run_dir, run_root
"""

LINUX = sys.platform == "linux"


def child(body, **kw):
    return subprocess.run([sys.executable, "-c", PRELUDE + body], capture_output=True,
                          text=True, timeout=30, **kw)


def forget(p):
    """Remove the root that the run `p` printed, if it printed one."""
    m = re.search(r"run dir: (\S+)", p.stderr)
    if m:
        shutil.rmtree(m.group(1), ignore_errors=True)


def alive(pid):
    try:
        return open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()[0] != "Z"
    except OSError:
        return False


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
rundirs._reap = boom
""" + guarded.format(action="os._exit(0)"))
        check(p.returncode == 0 and "harness except ran" not in p.stdout,
              f"{raised} in the supervisor keeps the harness's status "
              f"({p.returncode}, {p.stdout!r})")
        check(raised.split("(")[0] in p.stderr, f"{raised} in the supervisor is printed")
        forget(p)

    root = tempfile.mkdtemp(prefix="rmx-self-")
    open(f"{root}/{rundirs.OWNER}", "w").close()
    open(f"{root}/vanishing", "w").close()
    real_unlink = os.unlink

    def vanishes(path, *a, **k):
        if path.endswith("/vanishing") and os.path.exists(path):
            real_unlink(path)
            raise FileNotFoundError(path)
        return real_unlink(path, *a, **k)

    os.unlink = vanishes
    try:
        rundirs._remove(root)
        check(not os.path.exists(root), "_remove skips a file that a dying pane shell deleted")
    except FileNotFoundError as e:
        check(False, f"_remove skips a file that a dying pane shell deleted ({e!r})")
    finally:
        os.unlink = real_unlink
        shutil.rmtree(root, ignore_errors=True)


def killed_supervisor():
    """The caller's pid is the supervisor. Killing it must not leave the harness
    running and holding the caller's pipe."""
    if not LINUX:
        print("SKIP killed supervisor: macOS has no PR_SET_PDEATHSIG")
        return
    body = """
resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
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
rundirs._reap = slow
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


def decoys(argv0, extra_env=None):
    """Run a harness that leaves a process claiming to be `argv0 server` under
    its root and another outside it. Return (inside pid, outside pid, status).

    `/bin/sh` runs the file `server` in its cwd with argv[0] set, so the process
    has the command line of a real server and needs no remux binary.
    """
    base = tempfile.mkdtemp(prefix="rmx-self-")
    with open(f"{base}/server", "w") as f:
        f.write("while :; do sleep 1; done\n")
    body = f"""
root = run_root()
def decoy(runtime):
    env = {{**os.environ, "XDG_RUNTIME_DIR": runtime}}
    return subprocess.Popen([{argv0!r}, "server"], executable="/bin/sh", cwd={base!r}, env=env,
                            start_new_session=True, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL).pid
print(decoy(root + "/rmx-self/run"), decoy({base + '/run'!r}), flush=True)
time.sleep(0.5)
os._exit(0)
"""
    env = {**os.environ, **(extra_env or {})}
    p = child(body, env=env)
    inside, outside = (int(x) for x in p.stdout.split())
    time.sleep(0.3)
    return inside, outside, p, base


def reap_scope():
    if not LINUX:
        print("SKIP reap scope: the scan reads /proc")
        return
    for name, argv0, env in (("a binary called remux", "/x/remux", {}),
                             ("a renamed binary that is $REMUX_BIN", "/x/remux-dev.bin",
                              {"REMUX_BIN": "/x/remux-dev.bin"})):
        inside, outside, p, base = decoys(argv0, extra_env=env)
        try:
            root = re.search(r"run dir: (\S+)", p.stderr).group(1)
            check(not alive(inside), f"{name}: the server under the root is reaped")
            check(alive(outside), f"{name}: a decoy outside the root survives")
            check(not os.path.exists(root) and p.returncode == 0, f"{name}: the root is then removed")
        finally:
            for pid in (inside, outside):
                try:
                    os.kill(pid, signal.SIGKILL)
                except OSError:
                    pass
            shutil.rmtree(base, ignore_errors=True)
            forget(p)


def keep_hours_limits():
    base = tempfile.mkdtemp(prefix="rmx-self-")
    rundirs.BASE = base
    try:
        for value in ("0", "-5", "nan", "inf", "-inf", "abc", ""):
            fresh = make_root(base, "rmxr.freshrt1", age_hours=0.01)
            os.environ["RMX_RUN_KEEP_HOURS"] = value
            rundirs._sweep("")
            check(os.path.exists(f"{fresh}/.owner"),
                  f"RMX_RUN_KEEP_HOURS={value!r} cannot remove a fresh unlocked root")
            shutil.rmtree(fresh, ignore_errors=True)
        old = make_root(base, "rmxr.oldroot1", age_hours=5)
        os.environ["RMX_RUN_KEEP_HOURS"] = "2"
        rundirs._sweep("")
        check(not os.path.exists(old), "a valid RMX_RUN_KEEP_HOURS is honoured")
    finally:
        os.environ.pop("RMX_RUN_KEEP_HOURS", None)
        shutil.rmtree(base, ignore_errors=True)


def lock_descriptor():
    if not LINUX:
        print("SKIP lock descriptor: counts /proc/self/fd")
        return
    base = tempfile.mkdtemp(prefix="rmx-self-")
    held = None
    try:
        rundirs.BASE = base
        live = make_root(base, "rmxr.liverun2")
        held = subprocess.Popen(
            [sys.executable, "-c",
             "import fcntl, time\n"
             f"f = open({live + '/.owner'!r})\n"
             "fcntl.flock(f, fcntl.LOCK_EX)\n"
             "print('locked', flush=True)\n"
             "time.sleep(60)\n"],
            stdout=subprocess.PIPE, text=True)
        held.stdout.readline()
        before = len(os.listdir("/proc/self/fd"))
        for _ in range(5):
            rundirs._sweep("")
        after = len(os.listdir("/proc/self/fd"))
        check(after == before, f"a failed lock closes its descriptor ({before} then {after})")
    finally:
        if held:
            held.kill()
            held.wait()
        shutil.rmtree(base, ignore_errors=True)


def gate_checks():
    """`kill_gate` must hold when `_matches` is wrong. `_matches` is forced True,
    so the reaper offers every process on the machine. `os.kill` and
    `pidfd_send_signal` are recording stubs for the whole case, so a fault here
    cannot signal anything."""
    if not LINUX:
        print("SKIP kill gate: the gate needs /proc")
        return
    import contextlib, io
    sys.path.insert(0, os.path.join(TESTS, "frame"))
    import harness
    import kill_gate

    root = "/tmp/rmxr.gatetest"
    base = tempfile.mkdtemp(prefix="rmx-self-")
    # `read` blocks in the shell itself. A child such as `sleep` would inherit the
    # decoy's environment and be a second process under the root.
    with open(f"{base}/server", "w") as f:
        f.write("read x\n")

    def decoy(runtime):
        env = {**os.environ, "XDG_RUNTIME_DIR": runtime, "HOME": base}
        return subprocess.Popen(["/x/remux", "server"], executable="/bin/sh", cwd=base, env=env,
                                start_new_session=True, stdin=subprocess.PIPE,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    procs = {"ours": decoy(f"{root}/x/run"), "listed": decoy(f"{root}/x/run2"),
             "other run": decoy("/tmp/rmxr.otherrun/x/run"), "lookalike": decoy(f"{root}x/run"),
             "elsewhere": decoy(f"{base}/run"), "dotdot": decoy(f"{root}/../../home/run")}
    pid = {name: p.pid for name, p in procs.items()}
    time.sleep(0.3)
    signalled = []

    def record_kill(target, sig):
        signalled.append(target)

    def record_pidfd(fd, sig, *rest):
        with open(f"/proc/self/fdinfo/{fd}") as f:
            signalled.append(int(re.search(r"^Pid:\s+(\d+)", f.read(), re.M).group(1)))

    real = os.kill, signal.pidfd_send_signal, harness._matches, list(kill_gate.PID_FILES)
    pid_file = f"{base}/remux.pid"
    open(pid_file, "w").write(str(pid["listed"]))
    err = io.StringIO()
    try:
        os.kill, signal.pidfd_send_signal = record_kill, record_pidfd
        assert harness.os.kill is record_kill and harness.signal.pidfd_send_signal is record_pidfd
        harness._matches = lambda *a: True
        kill_gate.PID_FILES.append(pid_file)
        with contextlib.redirect_stderr(err):
            offered = harness.reap_rundir(root)
    finally:
        os.kill, signal.pidfd_send_signal, harness._matches = real[:3]
        kill_gate.PID_FILES[:] = real[3]
    try:
        refused = set(int(x) for x in re.findall(r"REFUSED to signal pid (\d+)", err.getvalue()))
        real_pids = {int(open(f).read().strip()) for f in real[3] if os.path.exists(f)}
        check(signalled == [pid["ours"]], f"only the process under the root gets through ({signalled})")
        check(len(offered) > 10 and refused == set(offered) - {pid["ours"]},
              f"every other process offered is refused ({len(refused)} of {len(offered)})")
        for name in ("listed", "other run", "lookalike", "elsewhere", "dotdot"):
            check(pid[name] in refused, f"the gate refuses {name}")
        check(real_pids <= refused, f"the gate refuses the real server {sorted(real_pids)}")
        check(os.getpid() in refused, "the gate refuses the harness itself")
        check(all(p.poll() is None for p in procs.values()), "nothing was really signalled")
    finally:
        for p in procs.values():
            p.kill()
            p.wait()
        shutil.rmtree(base, ignore_errors=True)


def main():
    name_checks()
    exit_routes()
    sweep_rules()
    supervisor_failures()
    killed_supervisor()
    spawn_before_run_dir()
    signal_during_cleanup()
    reap_scope()
    keep_hours_limits()
    lock_descriptor()
    gate_checks()
    print("\nFAILED: " + "; ".join(fails) if fails else "\nALL PASS")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
