#!/usr/bin/env python3
"""Self-test for two harness guards that otherwise only fire on a bad day.

1. `fakeagent.install` refuses a BINDIR another user could write into: an
   other-writable directory and a symlink are refused, a private one is
   accepted, and a group-writable one of ours (a umask of 002) is accepted
   and closed to 0700, with any link planted in it before that removed
   rather than written through. (A directory owned by another uid needs a
   second user, so it is not exercised here.)
2. `relay_fds.verdict`, which case 1r of msg_remote.py uses, flags a REAL
   terminal descriptor: a shell is started with a pty on its stdin and lists
   its own descriptors the way the ssh shim does, so the line under test is
   whatever this platform's `ls -l /proc` or `lsof` actually prints for one.

Run: python3 tests/frame/harness_guards_selftest.py
"""
import os
import pty
import shutil
import stat
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fakeagent  # noqa: E402
import relay_fds  # noqa: E402

FAILURES = []

# The same commands the shim runs in relay mode.
LISTING = 'ls -l "/proc/$$/fd" 2>/dev/null || lsof -n -P -p "$$"'


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


def refused(path):
    try:
        fakeagent.install(os.path.join(path, "fakeagent"))
    except SystemExit as e:
        return str(e)
    return None


def listing(stdin):
    done = subprocess.run(["/bin/sh", "-c", LISTING], stdin=stdin, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL, text=True, timeout=30)
    return done.stdout.splitlines()


root = tempfile.mkdtemp(prefix="rmx-guards-")
try:
    shared = os.path.join(root, "shared")
    os.mkdir(shared)
    os.chmod(shared, 0o777)
    why = refused(shared)
    check(why is not None and "writable by others" in why, f"1 a 0777 BINDIR is refused ({why})")

    # Ours and private, but inside a directory anyone could rename it out of.
    inner = os.path.join(shared, "inner")
    os.mkdir(inner, 0o700)
    why = refused(inner)
    check(why is not None and "ancestor" in why, f"1 a private BINDIR inside a 0777 one is refused ({why})")

    # What `os.makedirs` makes under a umask of 002.
    grouped = os.path.join(root, "grouped")
    os.mkdir(grouped)
    os.chmod(grouped, 0o775)
    why = refused(grouped)
    mode = stat.S_IMODE(os.lstat(grouped).st_mode)
    check(why is None and mode == 0o700, f"1 a 0775 BINDIR of ours is accepted and closed to 0700 ({why}, {mode:o})")

    # A link planted in it before it was closed is removed, not written through.
    planted = os.path.join(root, "planted")
    os.mkdir(planted)
    os.chmod(planted, 0o775)
    decoy = os.path.join(root, "decoy")
    with open(decoy, "w") as f:
        f.write("decoy\n")
    for suffix in ("", ".py", ".c"):
        os.symlink(decoy, os.path.join(planted, "fakeagent" + suffix))
    why = refused(planted)
    installed = [os.path.join(planted, "fakeagent" + suffix) for suffix in ("", ".py")]
    check(why is None and open(decoy).read() == "decoy\n"
          and all(os.path.isfile(p) and not os.path.islink(p) for p in installed),
          f"1 links planted at the program, script and source are removed, the decoy untouched ({why})")

    private = os.path.join(root, "private")
    os.mkdir(private, 0o700)
    link = os.path.join(root, "link")
    os.symlink(private, link)
    why = refused(link)
    check(why is not None and "not a real directory" in why, f"1 a symlinked BINDIR is refused ({why})")

    why = refused(private)
    check(why is None and os.access(os.path.join(private, "fakeagent"), os.X_OK),
          f"1 a private BINDIR is accepted and gets an executable fakeagent ({why})")

    clean = listing(subprocess.DEVNULL)
    captured, extra, leaked = relay_fds.verdict(clean)
    check(captured and not leaked, f"2 a shell with no terminal lists 0, 1, 2 and no terminal (leaked {leaked})")

    master, slave = pty.openpty()
    try:
        held = listing(slave)
    finally:
        os.close(slave)
        os.close(master)
    captured, extra, leaked = relay_fds.verdict(held)
    tty_lines = [ln for ln in held if "/dev/" in ln and ("pts" in ln or "tty" in ln)]
    check(captured and leaked, f"2 a shell holding a pty on stdin is flagged (leaked {leaked}, lines {tty_lines})")
finally:
    shutil.rmtree(root, ignore_errors=True)

print()
print("ALL PASS" if not FAILURES else f"{len(FAILURES)} FAILED")
sys.exit(1 if FAILURES else 0)
