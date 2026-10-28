"""Self-test for `home_guard`: it must refuse a remux launch that keeps the real
HOME and allow one with a throwaway HOME. Starts no real remux: the "remux" it
launches is a copy of /usr/bin/true.

Run: python3 tests/home_guard_selftest.py
"""
import os, shutil, subprocess, sys, tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import home_guard  # noqa: E402,F401

try:
    import pexpect
except ImportError:
    pexpect = None


def refused(launch):
    try:
        launch()
    except RuntimeError as e:
        return "refusing to start remux" in str(e)
    return False


def main():
    tmp = tempfile.mkdtemp(prefix="hg-", dir="/tmp")
    fails = []
    try:
        fake = os.path.join(tmp, "remux")
        shutil.copy("/usr/bin/true", fake)
        real = os.path.expanduser("~")
        scratch = os.path.join(tmp, "home")
        os.makedirs(scratch)
        cases = {
            "Popen real HOME refused": (lambda: subprocess.Popen([fake], env={"HOME": real}), True),
            "Popen missing HOME refused": (lambda: subprocess.Popen([fake], env={}), True),
            "Popen inherited real HOME refused": (lambda: subprocess.Popen([fake]), True),
            "Popen scratch HOME allowed": (
                lambda: subprocess.Popen([fake], env={"HOME": scratch}).wait(), False),
        }
        if pexpect:
            cases["spawn real HOME refused"] = (
                lambda: pexpect.spawn(fake, [], env={"HOME": real}), True)
            cases["spawn scratch HOME allowed"] = (
                lambda: pexpect.spawn(fake, [], env={"HOME": scratch}).close(), False)
        for name, (launch, want_refused) in cases.items():
            got = refused(launch)
            print(("PASS" if got == want_refused else "FAIL"), name)
            if got != want_refused:
                fails.append(name)
        if refused(lambda: subprocess.run(["/usr/bin/true"], env={"HOME": real})):
            print("FAIL non-remux command was guarded")
            fails.append("non-remux")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(1 if fails else 0)


main()
