#!/usr/bin/env python3
"""Agent session restore, from INSIDE a pane: the only proof that the CLI,
`$REMUX_PANE` and the restore typing are real.

The harness types one word into the pane, `claude`, and nothing else. That runs
a stub agent named `claude` (`tests/standin.py`), which does what Claude Code
does at a session start: it reads the SessionStart hook commands out of
`$HOME/.claude/settings.json` and runs each through `sh -c` with a recorded
payload on stdin. The command there is the installed shape,
`'<bin>' agent session claude --hook || true`, so the report reaches the server
from a process an AGENT started, with the pane's own environment. A report typed
at the prompt is refused `not_from_agent`, so this is the only route that can
store one.

Then `remux restart`. The restored pane's shell is typed the resume line, which
runs the same stub WITH arguments; the stub writes them one per line to a file
under `argv/`. The assertion reads that file, never the screen: the typed line
is echoed by the shell whether or not anything ran.

fast      the report is stored (state.json names the pane), the hook printed
          nothing on stdout and exited 0, and after `remux restart` the stub's
          argv file holds `--resume` and the id as two separate lines.
duplicate the stub runs in two panes with the same recorded session: both
          reports are stored, and the restart resumes it once.
disabled  with `resume_agents = false` the hook's report is refused
          `disabled` (one stderr line, exit 0, nothing on stdout), state.json
          holds no record, and the restart runs no stub.

`fakeclaude.py` reads `$HOME/.claude/settings.json` too, so this harness leaves
`CLAUDE_CONFIG_DIR` unset and writes the settings file there by hand. HOME is
the throwaway tree.

Run: python3 tests/pty/agent_resume.py [fast|duplicate|disabled ...]   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from pty_harness import BIN, Tui  # noqa: E402
import standin  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-agres")
FIXTURES = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                        "fixtures", "agent_session")
SESSION_ID = "7f3c2a9e-51d4-4b8e-9a61-0c2f5d8e4b17"
CONFIG = """
[general]
save_sessions = true
automatic_restore = true
"""

# The stub agent. With arguments it is a resumed session: it records them and
# exits. Without, it is a fresh one: it runs the SessionStart hooks, records
# each run, and stays in the foreground the way an agent does.
STUB = r'''
import json, os, subprocess, sys, time

ARGV_DIR, LOG, PAYLOAD = {argv_dir!r}, {log!r}, {payload!r}

if len(sys.argv) > 1:
    with open(os.path.join(ARGV_DIR, "claude.%d" % os.getpid()), "w") as f:
        f.write("".join(a + "\n" for a in sys.argv[1:]))
    sys.exit(0)

settings = os.path.join(os.environ.get("HOME", ""), ".claude", "settings.json")
with open(settings) as f:
    hooks = json.load(f).get("hooks", {{}}).get("SessionStart", [])
with open(PAYLOAD, "rb") as f:
    payload = f.read()
for entry in hooks:
    for hook in entry.get("hooks", []):
        done = subprocess.run(["sh", "-c", hook["command"]], input=payload,
                              capture_output=True, timeout=20)
        with open(LOG, "a") as f:
            f.write(json.dumps({{"rc": done.returncode,
                                 "stdout": done.stdout.decode("utf-8", "replace"),
                                 "stderr": done.stderr.decode("utf-8", "replace")}}) + "\n")
sys.stdout.write("STUB-%s\r\n" % "READY")
sys.stdout.flush()
while True:
    time.sleep(1)
'''

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


class Rig:
    def __init__(self, config=CONFIG):
        self.bin = f"{RUNDIR}/home/bin"
        self.argv_dir = f"{RUNDIR}/home/argv"
        self.hook_log = f"{RUNDIR}/home/hooks.jsonl"
        self.t = Tui(RUNDIR, cols=120, rows=36, config=config,
                     extra_env={"PATH": f"{RUNDIR}/home/bin:{os.environ['PATH']}"})
        # Its own REMUX_PANE would make the outside-a-pane gate pass on the
        # developer's own pane.
        for var in ("REMUX_PANE", "REMUX_SESSION", "CLAUDE_CONFIG_DIR", "CURSOR_VERSION"):
            os.environ.pop(var, None)
        self.t.start()
        self.install()

    def install(self):
        """The stub and the settings file, written after `start()`, which wipes
        the run dir. The pane's shell looks `claude` up when it is typed."""
        for d in (self.bin, self.argv_dir, f"{RUNDIR}/home/.claude"):
            os.makedirs(d, exist_ok=True)
        path = f"{self.bin}/claude"
        source = STUB.format(argv_dir=self.argv_dir, log=self.hook_log,
                             payload=os.path.join(FIXTURES, "claude_startup.json"))
        script = path + ".py"

        def shebang():
            with open(path, "w") as f:
                f.write(f"#!{sys.executable}\n{source}")
            os.chmod(path, 0o755)

        standin.install_wrapper(path, [sys.executable, script], {script: source}, shebang)
        command = f"'{BIN}' agent session claude --hook || true"
        with open(f"{RUNDIR}/home/.claude/settings.json", "w") as f:
            json.dump({"hooks": {"SessionStart": [
                {"matcher": "startup", "hooks": [{"type": "command", "command": command}]}]}}, f)

    def state(self):
        try:
            with open(f"{RUNDIR}/data/remux/state.json") as f:
                return json.load(f)
        except (OSError, ValueError):
            return None

    def hook_runs(self):
        try:
            with open(self.hook_log) as f:
                return [json.loads(line) for line in f if line.strip()]
        except OSError:
            return []

    def argv_runs(self):
        out = []
        for name in sorted(os.listdir(self.argv_dir)):
            with open(f"{self.argv_dir}/{name}") as f:
                out.append(f.read().split("\n"))
        return out

    def wait(self, predicate, timeout=10.0):
        end = time.time() + timeout
        while time.time() < end:
            self.t.pump(0.2)
            if predicate():
                return True
        return predicate()

    def stopped(self):
        self.t.kill()


def ready_marker(t):
    return any(m for row in t.rows_text() for m in re.finditer(r"STUB-READY", row))


def fast(rig):
    t = rig.t
    check(rig.wait(lambda: t.has("$ ")), "fast: the pane shows a prompt")
    t.send("claude\r", 0.5)
    check(rig.wait(lambda: rig.hook_runs() and ready_marker(t)),
          "fast: the stub agent ran its SessionStart hook and is in the foreground")
    runs = rig.hook_runs()
    check(len(runs) == 1 and runs[0]["rc"] == 0 and runs[0]["stdout"] == "",
          f"fast: the hook exited 0 with nothing on stdout (got {runs})")
    check(all(r["stderr"] == "" for r in runs), f"fast: the hook wrote no stderr (got {runs})")

    def held():
        sessions = (rig.state() or {}).get("agent_sessions", {})
        return {p: r for p, r in sessions.items() if r.get("value") == SESSION_ID}

    check(rig.wait(lambda: held()), f"fast: state.json holds the record (got {held()})")
    pane = next(iter(held()), None)
    log = t.log("server")
    check(re.search(rf"AgentSession caller -> pane %{pane} by ", log) is not None,
          f"fast: the server attributed the report to the stub's pane %{pane}")

    check(rig.argv_runs() == [], "fast: the stub has not been resumed before the restart")
    done = subprocess.run([BIN, "restart"], env=t.env, capture_output=True, timeout=30)
    check(done.returncode == 0, f"fast: remux restart exits 0 (got {done.returncode}, {done.stderr[:200]!r})")
    check(rig.wait(lambda: rig.argv_runs(), timeout=12.0), "fast: the restore ran the stub with arguments")
    time.sleep(0.5)
    runs = rig.argv_runs()
    check(runs == [["--resume", SESSION_ID, ""]],
          f"fast: --resume and the id are two argv lines, and the stub ran once (got {runs})")
    log = t.log("server")
    check(re.search(rf"typed resume for pane %{pane} agent=\"claude\" \(\d+ bytes\)", log) is not None,
          "fast: server.log names the typed resume by pane, agent and byte count")
    check(SESSION_ID not in log, "fast: server.log never holds the id")
    check(SESSION_ID not in t.log("client"), "fast: client.log never holds the id")


def start_agent(rig):
    """Type `claude` at the prompt and wait for the stub's hook to have run."""
    t = rig.t
    check(rig.wait(lambda: t.has("$ ")), "the pane shows a prompt")
    t.send("claude\r", 0.5)
    check(rig.wait(lambda: rig.hook_runs() and ready_marker(t)),
          "the stub agent ran its SessionStart hook and is in the foreground")
    return rig.hook_runs()


def restart_and_collect(rig, wait=4.0):
    done = subprocess.run([BIN, "restart"], env=rig.t.env, capture_output=True, timeout=30)
    check(done.returncode == 0, f"remux restart exits 0 (got {done.returncode}, {done.stderr[:200]!r})")
    rig.wait(lambda: rig.argv_runs(), timeout=wait)
    time.sleep(0.5)
    return rig.argv_runs()


def disabled(rig):
    runs = start_agent(rig)
    check(len(runs) == 1 and runs[0]["rc"] == 0 and runs[0]["stdout"] == "",
          f"disabled: the hook exited 0 with nothing on stdout (got {runs})")
    lines = runs[0]["stderr"].splitlines() if runs else []
    check(len(lines) == 1 and "disabled" in lines[0],
          f"disabled: one stderr line naming the refusal (got {lines})")
    time.sleep(0.5)
    check(not (rig.state() or {}).get("agent_sessions"), "disabled: state.json holds no record")
    runs = restart_and_collect(rig)
    check(runs == [], f"disabled: the restart ran no stub (got {runs})")


def duplicate(rig):
    t = rig.t
    check(rig.wait(lambda: t.has("$ ")), "duplicate: the pane shows a prompt")
    t.send(f"'{BIN}' split\r", 1.5)
    t.send("claude\r", 0.5)
    check(rig.wait(lambda: len(rig.hook_runs()) == 1), "duplicate: the stub ran in the new pane")
    t.send("\x1bk", 0.6)
    t.send("claude\r", 0.5)
    check(rig.wait(lambda: len(rig.hook_runs()) == 2), "duplicate: and in the first pane")
    runs = rig.hook_runs()
    check(all(r["rc"] == 0 and r["stdout"] == "" and r["stderr"] == "" for r in runs),
          f"duplicate: both reports were accepted silently (got {runs})")
    check(rig.wait(lambda: len((rig.state() or {}).get("agent_sessions", {})) == 2),
          f"duplicate: state.json holds both panes (got {(rig.state() or {}).get('agent_sessions')})")
    runs = restart_and_collect(rig, wait=8.0)
    time.sleep(1.0)
    runs = rig.argv_runs()
    check(runs == [["--resume", SESSION_ID, ""]], f"duplicate: the restart resumed it once (got {runs})")


MODES = {"fast": fast, "duplicate": duplicate, "disabled": (disabled, "resume_agents = false\n")}


def main():
    modes = sys.argv[1:] or list(MODES)
    for mode in modes:
        case, extra = MODES[mode] if isinstance(MODES[mode], tuple) else (MODES[mode], "")
        rig = Rig(CONFIG + extra)
        try:
            case(rig)
        finally:
            rig.stopped()
        for which in ("server", "client"):
            check("panicked at" not in rig.t.log(which), f"{mode}: no panic in {which}.log")
    if failures:
        print(f"\n{len(failures)} FAILED")
        sys.exit(1)
    print("\nALL PASS")


if __name__ == "__main__":
    main()
