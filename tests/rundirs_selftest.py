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


def main():
    name_checks()
    exit_routes()
    sweep_rules()
    print("\nFAILED: " + "; ".join(fails) if fails else "\nALL PASS")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
