"""Self-test for `home_guard`: it must refuse a remux launch that keeps the real
HOME and allow one with a throwaway HOME. Starts no real remux: the "remux" it
launches is a copy of /usr/bin/true.

Run: python3 tests/home_guard_selftest.py
"""
import os, pwd, shutil, subprocess, sys, tempfile

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
        dev = os.path.join(tmp, "remux-dev")
        shutil.copy("/usr/bin/true", dev)
        real_link = os.path.join(tmp, "rt")
        os.symlink(f"/run/user/{os.getuid()}", real_link)
        tmp_link = os.path.join(tmp, "td")
        os.symlink("/tmp", tmp_link)
        cases = {
            "Popen real HOME refused": (lambda: subprocess.Popen([fake], env={"HOME": real}), True),
            "Popen missing HOME refused": (lambda: subprocess.Popen([fake], env={}), True),
            "Popen inherited real HOME refused": (lambda: subprocess.Popen([fake]), True),
            "Popen scratch HOME allowed": (
                lambda: subprocess.Popen([fake], env={"HOME": scratch}).wait(), False),
            "stop without XDG_RUNTIME_DIR refused": (
                lambda: subprocess.Popen([fake, "stop"], env={"HOME": scratch}), True),
            "stop at the real socket dir refused": (
                lambda: subprocess.Popen([fake, "stop"], env={
                    "HOME": scratch, "XDG_RUNTIME_DIR": f"/run/user/{os.getuid()}"}), True),
            "restart with TMPDIR=/tmp refused": (
                lambda: subprocess.Popen([fake, "restart"], env={"HOME": scratch, "TMPDIR": "/tmp"}), True),
            "stop at a private socket dir allowed": (
                lambda: subprocess.Popen([fake, "stop"], env={
                    "HOME": scratch, "XDG_RUNTIME_DIR": f"{tmp}/run"}).wait(), False),
            "stop with a private TMPDIR allowed": (
                lambda: subprocess.Popen([fake, "stop"], env={"HOME": scratch, "TMPDIR": tmp}).wait(), False),
            # PATH holds only the fake `remux`, so a refusal that does not fire
            # runs `true` rather than a real remux.
            "stop at a symlink to the real socket dir refused": (
                lambda: subprocess.Popen([fake, "stop"], env={
                    "HOME": scratch, "PATH": tmp, "XDG_RUNTIME_DIR": real_link}), True),
            "stop with TMPDIR a symlink to /tmp refused": (
                lambda: subprocess.Popen([fake, "stop"], env={
                    "HOME": scratch, "PATH": tmp, "TMPDIR": tmp_link}), True),
            "stop in a shell string refused": (
                lambda: subprocess.Popen("remux stop", shell=True, env={"HOME": scratch, "PATH": tmp}), True),
            "stop in sh -c refused": (
                lambda: subprocess.Popen(["/bin/sh", "-c", "cd /; remux stop"],
                                         env={"HOME": scratch, "PATH": tmp}), True),
            "restart of a remux-dev binary refused": (
                lambda: subprocess.Popen([dev, "restart"], env={"HOME": scratch, "PATH": tmp}), True),
            "an ordinary shell string allowed": (
                lambda: subprocess.Popen("true", shell=True, env={"HOME": scratch, "PATH": tmp}).wait(), False),
        }
        # Decisions only: `_check_launch` is what Popen and pexpect run before
        # they launch anything, so calling it launches nothing.
        uid = os.getuid()
        private = {"HOME": scratch, "PATH": tmp, "XDG_RUNTIME_DIR": f"{tmp}/run"}
        no_runtime = {"HOME": scratch, "PATH": tmp}
        real_env = {"HOME": scratch, "PATH": tmp, "XDG_RUNTIME_DIR": f"/run/user/{uid}"}
        inside = os.path.join(tmp, "cwd")
        os.makedirs(inside)
        os.makedirs(os.path.join(tmp, "t"))
        os.symlink(f"/run/user/{uid}", os.path.join(inside, "link"))
        outside = tempfile.mkdtemp(prefix="hg-out-", dir="/tmp")
        rel_env = {"HOME": scratch, "PATH": tmp, "XDG_RUNTIME_DIR": "run", "TMPDIR": f"{tmp}/t"}

        def as_run(fn, root=tmp):
            saved = os.environ.get("RMX_RUNDIR")
            os.environ["RMX_RUNDIR"] = root
            try:
                return fn()
            finally:
                if saved is None:
                    del os.environ["RMX_RUNDIR"]
                else:
                    os.environ["RMX_RUNDIR"] = saved

        def as_root(fn):
            fake_rundirs = type(sys)("rundirs")
            fake_rundirs._root = tmp
            saved = sys.modules.get("rundirs")
            sys.modules["rundirs"] = fake_rundirs
            try:
                return fn()
            finally:
                if saved is None:
                    del sys.modules["rundirs"]
                else:
                    sys.modules["rundirs"] = saved
        with_real = {**private, "REAL": f"/run/user/{uid}", "REALHOME": real}
        decide = home_guard._check_launch
        renamed = os.path.join(tmp, "remux.bin")
        shutil.copy("/usr/bin/true", renamed)
        home_guard._BIN = os.path.realpath(renamed)
        cases.update({
            "a shell string aiming XDG_RUNTIME_DIR at the real dir refused": (
                lambda: decide(f"XDG_RUNTIME_DIR=/run/user/{uid} remux stop", private, shell=True), True),
            "a shell string giving a private XDG_RUNTIME_DIR allowed": (
                lambda: decide(f"XDG_RUNTIME_DIR={tmp}/run remux stop", no_runtime, shell=True), False),
            "env -i with the real XDG_RUNTIME_DIR refused": (
                lambda: decide(["env", "-i", f"HOME={scratch}", f"XDG_RUNTIME_DIR=/run/user/{uid}",
                                "remux", "stop"], private), True),
            "env -u XDG_RUNTIME_DIR refused": (
                lambda: decide(["env", "-u", "XDG_RUNTIME_DIR", "remux", "stop"], private), True),
            "bash -lc with remux stop refused": (
                lambda: decide(["bash", "-lc", "cd /; remux stop"], no_runtime), True),
            "timeout 5 remux restart refused": (
                lambda: decide(["timeout", "5", "remux", "restart"], no_runtime), True),
            "a pexpect string through nohup refused": (
                lambda: decide("nohup remux stop", no_runtime, split=True), True),
            "a non-shell string path with a space is one remux program": (
                lambda: decide(f"{tmp}/a b/remux", {"HOME": real}), True),
            "bash -lc with a private stop allowed": (
                lambda: decide(["bash", "-lc", "remux stop"], private), False),
            "an export of the real XDG_RUNTIME_DIR before the stop refused": (
                lambda: decide(f"export XDG_RUNTIME_DIR=/run/user/{uid}; remux stop", private, shell=True), True),
            # A script statement can only add a refusal: this one is refused,
            # because the stop is also checked as if the export had not run.
            "an export of a private XDG_RUNTIME_DIR before the stop refused": (
                lambda: decide(f"export XDG_RUNTIME_DIR={tmp}/run; remux stop", no_runtime, shell=True), True),
            "an export in a subshell, then a stop under the real env, refused": (
                lambda: decide(f"(export XDG_RUNTIME_DIR={tmp}/run); remux stop", real_env, shell=True), True),
            "an export in $( ), then a stop under the real env, refused": (
                lambda: decide(f"x=$(export XDG_RUNTIME_DIR={tmp}/run); remux stop", real_env, shell=True), True),
            "an export in a pipeline, then a stop under the real env, refused": (
                lambda: decide(f"export XDG_RUNTIME_DIR={tmp}/run | true; remux stop", real_env, shell=True), True),
            "an export in the background, then a stop under the real env, refused": (
                lambda: decide(f"export XDG_RUNTIME_DIR={tmp}/run & remux stop", real_env, shell=True), True),
            "an export in an if that does not run, then a stop under the real env, refused": (
                lambda: decide(f"if false; then export XDG_RUNTIME_DIR={tmp}/run; fi; remux stop", real_env,
                               shell=True), True),
            "an export of an unset $D, then a stop under a private env, refused": (
                lambda: decide('export XDG_RUNTIME_DIR="$D"; remux stop', private, shell=True), True),
            "an export of an unset $D, then a stop under the real env, refused": (
                lambda: decide('export XDG_RUNTIME_DIR="$D"; remux stop', real_env, shell=True), True),
            "a command prefix from an unset $D refused": (
                lambda: decide("XDG_RUNTIME_DIR=$D remux stop", real_env, shell=True), True),
            "a command prefix from a $D that is private allowed": (
                lambda: decide("XDG_RUNTIME_DIR=$D remux stop", {**no_runtime, "D": f"{tmp}/run"}, shell=True),
                False),
            "an export of a $D that is private, under a private env, allowed": (
                lambda: decide('export XDG_RUNTIME_DIR="${D}"; remux stop', {**private, "D": f"{tmp}/run2"},
                               shell=True), False),
            "XDG_RUNTIME_DIR=$REAL remux stop refused": (
                lambda: decide("XDG_RUNTIME_DIR=$REAL remux stop", with_real, shell=True), True),
            "XDG_RUNTIME_DIR=${REAL} remux stop refused": (
                lambda: decide("XDG_RUNTIME_DIR=${REAL} remux stop", with_real, shell=True), True),
            "env XDG_RUNTIME_DIR=$REAL remux stop refused": (
                lambda: decide("env XDG_RUNTIME_DIR=$REAL remux stop", with_real, shell=True), True),
            "export XDG_RUNTIME_DIR=$REAL; remux stop refused": (
                lambda: decide("export XDG_RUNTIME_DIR=$REAL; remux stop", with_real, shell=True), True),
            "eval XDG_RUNTIME_DIR=$REAL remux stop refused": (
                lambda: decide("eval XDG_RUNTIME_DIR=$REAL remux stop", with_real, shell=True), True),
            "sh -c with XDG_RUNTIME_DIR=$REAL refused": (
                lambda: decide(["sh", "-c", "XDG_RUNTIME_DIR=$REAL remux stop"], with_real), True),
            "HOME=$REALHOME remux stop refused": (
                lambda: decide("HOME=$REALHOME remux stop", with_real, shell=True), True),
            "a ${NAME:-default} form refused": (
                lambda: decide(f"XDG_RUNTIME_DIR=${{D:-{tmp}/run}} remux stop", private, shell=True), True),
            "a $( ) form refused": (
                lambda: decide("XDG_RUNTIME_DIR=$(pwd) remux stop", private, shell=True), True),
            "a backtick form refused": (
                lambda: decide("XDG_RUNTIME_DIR=`pwd` remux stop", private, shell=True), True),
            "a single-quoted $DEEP that climbs to the real dir refused": (
                lambda: decide(f"XDG_RUNTIME_DIR='$DEEP/../../../../../../run/user/{uid}' remux stop",
                               {**private, "DEEP": f"{tmp}/a/b/c/d/e/f"}, shell=True), True),
            "an escaped $PRIV refused": (
                lambda: decide("XDG_RUNTIME_DIR=\\$PRIV remux stop", {**private, "PRIV": f"{tmp}/run"},
                               shell=True), True),
            "a relative XDG_RUNTIME_DIR refused": (
                lambda: decide("XDG_RUNTIME_DIR=run remux stop", no_runtime, shell=True), True),
            "an absolute XDG_RUNTIME_DIR with a .. component refused": (
                lambda: decide(f"XDG_RUNTIME_DIR={tmp}/a/../run remux stop", no_runtime, shell=True), True),
            "an ANSI-C $'…' hiding .. behind \\x2f refused": (
                lambda: decide("XDG_RUNTIME_DIR=$'PRIV\\x2f..\\x2f..\\x2f..\\x2f..\\x2f..\\x2f..\\x2frun"
                               f"\\x2fuser\\x2f{uid}' remux stop", {**private, "PRIV": f"{tmp}/run"}, shell=True),
                True),
            "a $\"PRIV\"/x translated string refused": (
                lambda: decide('XDG_RUNTIME_DIR=$"PRIV"/x remux stop', {**private, "PRIV": f"{tmp}/run"},
                               shell=True), True),
            "HOME=~ with the real HOME in env refused": (
                lambda: decide("HOME=~ remux server", {"HOME": real, "PATH": tmp}, shell=True), True),
            "HOME=~<user> with a private env refused": (
                lambda: decide(f"HOME=~{pwd.getpwuid(uid).pw_name} remux server", private, shell=True), True),
            "a relative XDG_RUNTIME_DIR below the run, with a private TMPDIR, allowed": (
                lambda: as_run(lambda: decide([fake, "stop"], rel_env, cwd=inside)), False),
            "...also under the root rundirs made": (
                lambda: as_root(lambda: decide([fake, "stop"], rel_env, cwd=inside)), False),
            "...refused with the cwd outside the run": (
                lambda: as_run(lambda: decide([fake, "stop"], rel_env, cwd=outside)), True),
            "...refused with a .. component": (
                lambda: as_run(lambda: decide([fake, "stop"], {**rel_env, "XDG_RUNTIME_DIR": "../run"},
                                              cwd=inside)), True),
            "...refused with a . component": (
                lambda: as_run(lambda: decide([fake, "stop"], {**rel_env, "XDG_RUNTIME_DIR": "./run"},
                                              cwd=inside)), True),
            "...refused with TMPDIR=/tmp": (
                lambda: as_run(lambda: decide([fake, "stop"], {**rel_env, "TMPDIR": "/tmp"}, cwd=inside)),
                True),
            "...refused with a relative TMPDIR": (
                lambda: as_run(lambda: decide([fake, "stop"], {**rel_env, "TMPDIR": "t"}, cwd=inside)), True),
            "...refused with a TMPDIR outside the run": (
                lambda: as_run(lambda: decide([fake, "stop"], {**rel_env, "TMPDIR": outside}, cwd=inside)),
                True),
            "...refused with no run dir known": (
                lambda: decide([fake, "stop"], rel_env, cwd=inside), True),
            "...refused when the opt-out dir holds the real runtime dir": (
                lambda: as_run(lambda: decide([fake, "stop"], {**rel_env, "TMPDIR": f"/run/user/{uid}/t"},
                                              cwd=f"/run/user/{uid}"), root="/run/user"), True),
            "...refused when the value is a symlink to the real runtime dir": (
                lambda: as_run(lambda: decide([fake, "stop"], {**rel_env, "XDG_RUNTIME_DIR": "link"},
                                              cwd=inside)), True),
            "...refused when the value holds a $": (
                lambda: as_run(lambda: decide([fake, "stop"], {**rel_env, "XDG_RUNTIME_DIR": "$X"},
                                              cwd=inside)), True),
            "a TMPDIR with a .. component refused": (
                lambda: decide(f"TMPDIR={tmp}/a/.. remux stop", no_runtime, shell=True), True),
            "a double-quoted $PRIV that is private allowed": (
                lambda: decide('XDG_RUNTIME_DIR="$PRIV" remux stop', {**no_runtime, "PRIV": f"{tmp}/run"},
                               shell=True), False),
            "a substituted remux given stop refused": (
                lambda: decide("$(command -v remux) stop", no_runtime, shell=True), True),
            "a backtick remux given stop refused": (
                lambda: decide("`which remux` stop", no_runtime, shell=True), True),
            "a substituted remux given stop with a private dir allowed": (
                lambda: decide("$(command -v remux) stop", private, shell=True), False),
            "eval remux stop refused": (
                lambda: decide("eval remux stop", no_runtime, shell=True), True),
            "eval of a stop aimed at the real dir refused": (
                lambda: decide(f"eval XDG_RUNTIME_DIR=/run/user/{uid} remux stop", private, shell=True), True),
            "eval of a private stop allowed": (
                lambda: decide("eval remux stop", private, shell=True), False),
            "unset XDG_RUNTIME_DIR before the stop refused": (
                lambda: decide("unset XDG_RUNTIME_DIR; remux stop", private, shell=True), True),
            "export -n XDG_RUNTIME_DIR before the stop refused": (
                lambda: decide("export -n XDG_RUNTIME_DIR; remux stop", private, shell=True), True),
            "a plain assignment to an exported XDG_RUNTIME_DIR refused": (
                lambda: decide(f"XDG_RUNTIME_DIR=/run/user/{uid}; remux stop", private, shell=True), True),
            "a plain assignment to an unexported XDG_RUNTIME_DIR does not reach the stop": (
                lambda: decide(f"XDG_RUNTIME_DIR={tmp}/run; remux stop", no_runtime, shell=True), True),
            "an export inside sh -c refused": (
                lambda: decide(["sh", "-c", f"export XDG_RUNTIME_DIR=/run/user/{uid}; remux stop"], private), True),
            "a list with shell=True refused": (
                lambda: decide(["remux stop"], no_runtime, shell=True), True),
            "a REMUX_BIN named remux.bin refused": (
                lambda: decide([renamed, "stop"], no_runtime), True),
        })
        if pexpect:
            cases["spawn real HOME refused"] = (
                lambda: pexpect.spawn(fake, [], env={"HOME": real}), True)
            cases["spawn scratch HOME allowed"] = (
                lambda: pexpect.spawn(fake, [], env={"HOME": scratch}).close(), False)
            cases["spawn of a 'remux stop' string refused"] = (
                lambda: pexpect.spawn("remux stop", env={"HOME": scratch, "PATH": tmp}), True)
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
        shutil.rmtree(outside, ignore_errors=True)
    sys.exit(1 if fails else 0)


main()
