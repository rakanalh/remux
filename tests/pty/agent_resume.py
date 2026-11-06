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
          argv file holds `--resume=<id>` as one line: the value is joined to
          its flag, so no id can ever be read as an option.
duplicate the stub runs in two panes with the same recorded session: both
          reports are stored, and the restart resumes it once.
typed_at_prompt  `remux agent session claude --hook < payload` typed AT THE
          PROMPT: its parent is the pane's shell, so it is refused
          `not_from_agent` on stderr, exits 0, stores nothing, and the restart
          runs no stub.
stub_exited  the stub reports and is then interrupted; the shell takes the
          foreground back, the record is forgotten, and the restart runs no
          stub.
bad_id    the stub's payload carries an id outside the charset: one stderr line
          naming bad_reference, nothing stored, no stub after the restart.
pane_unset  the stub runs with `$REMUX_PANE` unset: the hook exits 0 with no
          output and never connects (no new connection in server.log).
setsid    the report runs detached into a session of its own (`setsid -f`),
          so the server cannot place it: `not_in_pane` on stderr, exit 0.
slow_rc   `/bin/sh` with `ENV` naming an rc that sleeps 2 s before the first
          prompt: the typed resume waits out the rc and runs the stub with
          `--resume` and the id. bash reads an rc without job control, so the
          rc's `sleep` never takes the foreground and NO forget lands before
          the stub runs.
slow_rc_zsh  the same under zsh, whose `.zshrc` runs with job control on:
          its `sleep` takes the foreground and gives it back, which usually
          forgets the record BEFORE the typed line runs (the run prints which),
          and state.json holds NO record once the stub has run. That is the race SPEC 1.7 step 5 accepts
          (the resumed agent's own hook re-stores it), pinned as understood.
          It also proves the typed line survives zsh's line editor starting.
never_reported  the typed resume runs something that is not the agent and stays
          in the foreground (`claude` on PATH replaced by a script that execs
          `sleep`): nothing reports and the foreground never becomes claude, so
          the restart after that does not type it again, and says why (a
          resume that did not take is not retried for ever)
foreground_confirmed  the resumed stub stays in the foreground and never
          reports again (codex runs no SessionStart for `codex resume`): the
          foreground being claude confirms it, logged once with no id, and the
          second restart types it again
other_agent  as never_reported, with the foreground process named `codex`, a
          configured agent but not this record's: it is not confirmed
foreground_argv0  the same with `claude` a link to a binary named `2.1.251`:
          on macOS only argv[0] names it claude, and it is still confirmed
re_reported  the resumed stub reports `resume` with its session: the restart
          after that types it again
disabled  with `resume_agents = false` the hook's report is refused
          `disabled` (one stderr line, exit 0, nothing on stdout), state.json
          holds no record, and the restart runs no stub.
extra_args  `[general.resume_agent_args]` names claude, codex and an unknown
          agent: the stub's argv is claude's word and then `--resume=<id>`,
          with nothing from the other two entries. fast pins the unconfigured
          line, so the two together cover both directions.
bad_extra_args  claude's entry holds a control character and codex's a
          backslash: both are dropped with a warning naming the agent and not
          the word, claude resumes with plain `--resume=<id>`, and the record
          survives a second restart. `cluade` is warned about as no agent.

The hook is installed by `remux integrate claude-resume`, with
`CLAUDE_CONFIG_DIR` unset so that it lands in `$HOME/.claude/settings.json`,
where the stub (like `fakeclaude.py`) reads it. HOME is the throwaway tree.

Every mode but fast, duplicate and the slow_rc pair ends in "no argv file
after the restart". The argv FILE is read, never the screen. The pane's PATH
is the stubs and the system directories only, so no real agent on the
developer's PATH can run in place of a stub.

`AGENT_RESUME_PROBE=plant` stores a record from outside every pane once
slow_rc_zsh's stub has run, which must turn its no-record check red.

Run: python3 tests/pty/agent_resume.py [MODE ...]   (default: every mode)   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from pty_harness import BIN, Tui  # noqa: E402
from rundirs import run_dir  # noqa: E402
import standin  # noqa: E402
from portable import SETSID_F  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR") or run_dir("rmx-agres")
FIXTURES = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                        "fixtures", "agent_session")
ZDOTDIR = f"{RUNDIR}-zdot"
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

ARGV_DIR, LOG, RESUME = {argv_dir!r}, {log!r}, {resume!r}
PAYLOAD = os.environ.get("STUB_PAYLOAD", {payload!r})
MARKS = os.path.dirname(ARGV_DIR)


def run_hooks(payload_path):
    settings = os.path.join(os.environ.get("HOME", ""), ".claude", "settings.json")
    with open(settings) as f:
        hooks = json.load(f).get("hooks", {{}}).get("SessionStart", [])
    with open(payload_path, "rb") as f:
        payload = f.read()
    for entry in hooks:
        for hook in entry.get("hooks", []):
            done = subprocess.run(["sh", "-c", hook["command"]], input=payload,
                                  capture_output=True, timeout=20)
            with open(LOG, "a") as f:
                f.write(json.dumps({{"rc": done.returncode,
                                     "stdout": done.stdout.decode("utf-8", "replace"),
                                     "stderr": done.stderr.decode("utf-8", "replace")}}) + "\n")


def stay():
    while True:
        time.sleep(1)


# With arguments it is a resumed session: it records them, and then, as the
# run dir's marks say, runs its SessionStart hook with a `resume` payload and
# stays (`resumed-reports`), stays without reporting (`resumed-stays`), or exits.
if len(sys.argv) > 1:
    with open(os.path.join(ARGV_DIR, "claude.%d" % os.getpid()), "w") as f:
        f.write("".join(a + "\n" for a in sys.argv[1:]))
    if os.path.exists(os.path.join(MARKS, "resumed-reports")):
        run_hooks(RESUME)
        stay()
    if os.path.exists(os.path.join(MARKS, "resumed-stays")):
        stay()
    sys.exit(0)

run_hooks(PAYLOAD)
sys.stdout.write("STUB-%s\r\n" % "READY")
sys.stdout.flush()
stay()
'''

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


class Rig:
    def __init__(self, config=CONFIG, extra_env=None):
        self.bin = f"{RUNDIR}/home/bin"
        self.argv_dir = f"{RUNDIR}/home/argv"
        self.hook_log = f"{RUNDIR}/home/hooks.jsonl"
        # System directories only after the stubs: the developer's own PATH
        # can hold a REAL agent (`~/.local/bin/claude`), which a shell that
        # rebuilds PATH would otherwise find first.
        self.t = Tui(RUNDIR, cols=120, rows=36, config=config,
                     extra_env={"PATH": f"{self.bin}:/usr/local/bin:/usr/bin:/bin",
                                "STUB_BIN": self.bin, **(extra_env or {})})
        # Its own REMUX_PANE would make the outside-a-pane gate pass on the
        # developer's own pane.
        for var in ("REMUX_PANE", "REMUX_SESSION", "CLAUDE_CONFIG_DIR", "CURSOR_VERSION"):
            os.environ.pop(var, None)
        # Outside the run dir, which `start()` wipes, and in place before the
        # first zsh starts: without a .zshrc zsh runs its new-user wizard.
        os.makedirs(ZDOTDIR, exist_ok=True)
        shutil.copy(os.path.join(FIXTURES, "slow_rc.zsh"), f"{ZDOTDIR}/.zshrc")
        self.t.start()
        self.install()

    def install(self):
        """The stub and the settings file, written after `start()`, which wipes
        the run dir. The pane's shell looks `claude` up when it is typed."""
        for d in (self.bin, self.argv_dir, f"{RUNDIR}/home/.claude"):
            os.makedirs(d, exist_ok=True)
        # A login shell's /etc/profile can put system dirs ahead of the
        # inherited PATH (macOS path_helper, with Homebrew's real agents); the
        # throwaway HOME's profile, read after it, puts the stubs first again
        # in every shell the restore spawns.
        for profile in (".profile", ".zprofile"):
            with open(f"{RUNDIR}/home/{profile}", "w") as f:
                f.write(f'PATH="{self.bin}:$PATH"; export PATH\n')
        path = f"{self.bin}/claude"
        source = STUB.format(argv_dir=self.argv_dir, log=self.hook_log,
                             payload=os.path.join(FIXTURES, "claude_startup.json"),
                             resume=os.path.join(FIXTURES, "claude_resume.json"))
        script = path + ".py"

        def shebang():
            with open(path, "w") as f:
                f.write(f"#!{sys.executable}\n{source}")
            os.chmod(path, 0o755)

        standin.install_wrapper(path, [sys.executable, script], {script: source}, shebang)
        # The hook goes in the way a user puts it there. CLAUDE_CONFIG_DIR is
        # unset, so it lands in the throwaway HOME's .claude, where the stub
        # reads it.
        done = subprocess.run([BIN, "integrate", "claude-resume", "--allow-dev-path"],
                              env=self.t.env, capture_output=True, timeout=20)
        assert done.returncode == 0, done.stderr

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

    def typed(self, command, timeout=10.0):
        """Type `command` at the prompt with its stderr and exit code sent to
        files, and return (rc, stderr) once it has finished."""
        self.runs = getattr(self, "runs", 0) + 1
        rc, err = f"{RUNDIR}/home/rc.{self.runs}", f"{RUNDIR}/home/err.{self.runs}"
        self.t.send(f"{command} 2> {err}; echo $? > {rc}\r", 0.5)
        self.wait(lambda: os.path.exists(rc) and open(rc).read().endswith("\n"), timeout)
        if not os.path.exists(rc):
            return None, None
        with open(rc) as f, open(err) as g:
            return int(f.read().strip()), g.read()

    def connections(self):
        return len(set(re.findall(r"identity: client_id=(\d+) peer=", self.t.log("server"))))


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
    check(runs == [[f"--resume={SESSION_ID}", ""]],
          f"fast: the stub got --resume=<id> as one argv word, and ran once (got {runs})")
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


def payload(name):
    return os.path.join(FIXTURES, name)


def prompt(rig):
    check(rig.wait(lambda: rig.t.has("$ ")), "the pane shows a prompt")


def no_record(rig):
    return not (rig.state() or {}).get("agent_sessions")


def no_stub_after_restart(rig, mode):
    runs = restart_and_collect(rig)
    check(runs == [], f"{mode}: the restart ran no stub (got {runs})")


def typed_at_prompt(rig):
    prompt(rig)
    rc, err = rig.typed(f"'{BIN}' agent session claude --hook < {payload('claude_startup.json')}")
    check(rc == 0, f"typed_at_prompt: exit 0 (got {rc})")
    check(err is not None and len(err.splitlines()) == 1 and "not_from_agent" in err,
          f"typed_at_prompt: one stderr line naming not_from_agent (got {err!r})")
    check(no_record(rig), "typed_at_prompt: state.json holds no record")
    no_stub_after_restart(rig, "typed_at_prompt")


def stub_exited(rig):
    runs = start_agent(rig)
    check(len(runs) == 1 and runs[0]["rc"] == 0 and runs[0]["stderr"] == "",
          f"stub_exited: the report was accepted (got {runs})")
    check(rig.wait(lambda: not no_record(rig)), "stub_exited: state.json held the record")
    rig.t.send("\x03", 0.5)
    check(rig.wait(lambda: no_record(rig), timeout=3.0),
          "stub_exited: the record is forgotten once the stub has exited")
    no_stub_after_restart(rig, "stub_exited")


def bad_id(rig):
    prompt(rig)
    rig.t.send(f"STUB_PAYLOAD={payload('claude_bad_id.json')} claude\r", 0.5)
    check(rig.wait(lambda: rig.hook_runs()), "bad_id: the stub ran its hook")
    runs = rig.hook_runs()
    lines = runs[0]["stderr"].splitlines() if runs else []
    check(bool(runs) and runs[0]["rc"] == 0 and runs[0]["stdout"] == "",
          f"bad_id: the hook exited 0 with nothing on stdout (got {runs})")
    check(len(lines) == 1 and "bad_reference" in lines[0],
          f"bad_id: one stderr line naming bad_reference (got {lines})")
    check(no_record(rig), "bad_id: state.json holds no record")
    no_stub_after_restart(rig, "bad_id")


def pane_unset(rig):
    prompt(rig)
    time.sleep(0.5)
    before = rig.connections()
    rig.t.send("env -u REMUX_PANE claude\r", 0.5)
    check(rig.wait(lambda: rig.hook_runs() and ready_marker(rig.t)), "pane_unset: the stub ran its hook")
    runs = rig.hook_runs()
    check(runs == [{"rc": 0, "stdout": "", "stderr": ""}],
          f"pane_unset: exit 0 and no output at all (got {runs})")
    time.sleep(0.5)
    check(rig.connections() == before,
          f"pane_unset: no connection reached the server ({before} before, {rig.connections()} after)")
    check(no_record(rig), "pane_unset: state.json holds no record")
    no_stub_after_restart(rig, "pane_unset")


def setsid(rig):
    prompt(rig)
    rc_file, err_file = f"{RUNDIR}/home/setsid.rc", f"{RUNDIR}/home/setsid.err"
    inner = (f"sleep 0.3; '{BIN}' agent session claude --hook < {payload('claude_startup.json')} "
             f"2> {err_file}; echo $? > {rc_file}")
    rig.t.send(" ".join(shlex.quote(a) for a in SETSID_F) + f" /bin/sh -c {shlex.quote(inner)}\r", 0.5)
    check(rig.wait(lambda: os.path.exists(rc_file) and open(rc_file).read().endswith("\n")),
          "setsid: the detached report finished")
    rc = open(rc_file).read().strip() if os.path.exists(rc_file) else None
    err = open(err_file).read() if os.path.exists(err_file) else ""
    check(rc == "0", f"setsid: exit 0 (got {rc})")
    check(len(err.splitlines()) == 1 and "not_in_pane" in err,
          f"setsid: one stderr line naming not_in_pane (got {err!r})")
    check(no_record(rig), "setsid: state.json holds no record")
    no_stub_after_restart(rig, "setsid")


def log_times(log, needle):
    """The wall-clock time of every server.log line containing `needle`."""
    import datetime
    out = []
    for line in log.splitlines():
        if needle in line and line.startswith("["):
            stamp = line[1:].split(" ", 1)[0]
            out.append(datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp())
    return out


def slow_rc_case(rig, mode, races):
    t = rig.t
    check(rig.wait(lambda: t.has("$ "), timeout=8.0), f"{mode}: the pane shows a prompt after the rc")
    t.send("claude\r", 0.5)
    ran = rig.wait(lambda: rig.hook_runs() and ready_marker(t))
    check(ran, f"{mode}: the stub ran its hook")
    if not ran:
        t.dump(mode)
    check(rig.wait(lambda: not no_record(rig)), f"{mode}: state.json holds the record")
    pane = next(iter((rig.state() or {}).get("agent_sessions", {})), None)
    done = subprocess.run([BIN, "restart"], env=t.env, capture_output=True, timeout=30)
    check(done.returncode == 0, f"{mode}: remux restart exits 0")
    restarted = time.time()
    rig.wait(lambda: rig.argv_runs(), timeout=12.0)
    names = sorted(os.listdir(rig.argv_dir))
    ran_at = os.path.getmtime(f"{rig.argv_dir}/{names[0]}") if names else None
    runs = rig.argv_runs()
    check(runs == [[f"--resume={SESSION_ID}", ""]],
          f"{mode}: the typed line waited out the rc and ran the stub once (got {runs})")
    check(ran_at is not None and ran_at - restarted >= 1.5,
          f"{mode}: the stub ran only after the rc's 2 s sleep "
          f"({None if ran_at is None else round(ran_at - restarted, 2)} s after the restart)")
    time.sleep(1.0)
    early = [at for at in log_times(t.log("server"), f"pane %{pane} forgot")
             if ran_at is not None and restarted <= at <= ran_at]
    if races:
        # Whether the forget lands BEFORE the stub depends on a foreground
        # sample catching the shell between the rc's sleep and the typed line,
        # which it usually does and on a slower machine does not: zsh starts
        # the stub at once, the tracker sees one job hand over to another, and
        # the forget comes when the stub exits instead. Either way the record
        # is forgotten, which is what is pinned; the order is reported.
        forgets = [at for at in log_times(t.log("server"), f"pane %{pane} forgot") if at >= restarted]
        print(f"note: {mode}: {len(early)} of {len(forgets)} forget(s) landed before the stub ran")
        check(len(forgets) >= 1, f"{mode}: the record was forgotten after the restart "
                                 f"({len(forgets)} forget(s))")
        if os.environ.get("AGENT_RESUME_PROBE") == "plant" and pane is not None:
            plant(rig, int(pane))
        check(rig.wait(lambda: no_record(rig), timeout=3.0),
              f"{mode}: state.json holds no record once the stub has run "
              f"(got {(rig.state() or {}).get('agent_sessions')})")
    else:
        check(early == [], f"{mode}: no forget landed before the stub ran (got {len(early)})")


def slow_rc(rig):
    slow_rc_case(rig, "slow_rc", races=False)


def slow_rc_zsh(rig):
    slow_rc_case(rig, "slow_rc_zsh", races=True)


def plant(rig, pane):
    """Store a record from outside every pane, for the slow_rc probe."""
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                    "frame"))
    from harness import Client
    c = Client(f"{RUNDIR}/run/remux.sock")
    c.hello()
    c.send({"AgentSession": {"agent": "claude", "reference": {"Id": SESSION_ID}, "pane": pane}})
    print(f"probe: planted -> {c.drain(1.0)[-1:]}")
    c.close()


def restart_twice(rig, mark):
    """Start the stub, restart, let the resumed stub behave as `mark` says,
    restart again; the stub's runs after each restart."""
    start_agent(rig)
    check(rig.wait(lambda: not no_record(rig)), f"{mark}: state.json holds the record")
    open(f"{RUNDIR}/home/{mark}", "w").close()
    first = restart_and_collect(rig, wait=8.0)
    time.sleep(1.5)
    second = restart_and_collect(rig, wait=4.0)
    return first, second


def impostor_resumed(rig, mode, runs_as):
    """The typed line runs something that is NOT the agent and stays in the
    foreground: `claude` on PATH is now a script that records its argv and
    execs `runs_as` (a sleep under that name). Nothing reports, the foreground
    is never claude, and the shell never takes it back, so only the pending
    rule can drop it."""
    start_agent(rig)
    check(rig.wait(lambda: not no_record(rig)), f"{mode}: state.json holds the record")
    sleeper = shutil.which("sleep")
    if runs_as != "sleep":
        # A wrapper binary under that name, leading a sleep: macOS kills a
        # copy of a signed system binary such as /bin/sleep at exec.
        named = f"{rig.bin}/{runs_as}"

        def unavailable():
            raise SystemExit("no cc to build the wrapper")
        standin.install_wrapper(named, [sleeper, "600"], {}, unavailable)
        sleeper = named
    impostor = f"{rig.bin}/claude"
    os.remove(impostor)
    with open(impostor, "w") as f:
        args = "" if runs_as != "sleep" else " 600"
        f.write(f'#!/bin/sh\nprintf "%s\\n" "$@" > "{rig.argv_dir}/claude.$$"\nexec "{sleeper}"{args}\n')
    os.chmod(impostor, 0o755)
    first = restart_and_collect(rig, wait=8.0)
    time.sleep(3.0)
    second = restart_and_collect(rig, wait=4.0)
    check(first == [[f"--resume={SESSION_ID}", ""]], f"{mode}: the first restart typed it (got {first})")
    check(second == first,
          f"{mode}: a session whose resume never put claude in the foreground is not typed again "
          f"(got {len(second)} run(s))")
    log = rig.t.log("server")
    check(re.search(r"record dropped on restore \(typed last time and never reported again\)", log)
          is not None, f"{mode}: the drop is logged")
    check("typed resume confirmed by foreground" not in log,
          f"{mode}: {runs_as} in the foreground never confirmed the claude resume")


def never_reported(rig):
    impostor_resumed(rig, "never_reported", "sleep")


def other_agent(rig):
    """Another configured agent's name in the foreground (`codex`, in the
    default [agents] list) does not confirm a claude record."""
    impostor_resumed(rig, "other_agent", "codex")


def foreground_confirmed(rig):
    """The resumed agent stays in the foreground and never reports again, as
    codex does after `codex resume`: the foreground alone confirms it, so the
    session survives a second restart."""
    first, second = restart_twice(rig, "resumed-stays")
    check(first == [[f"--resume={SESSION_ID}", ""]],
          f"foreground_confirmed: the first restart resumed it (got {first})")
    check(len(rig.hook_runs()) == 1, f"foreground_confirmed: the resumed stub never reported "
                                     f"(hook runs {len(rig.hook_runs())})")
    check(len(second) == 2 and second[1] == first[0],
          f"foreground_confirmed: the second restart typed it again (got {len(second)} run(s))")
    log = rig.t.log("server")
    confirmed = re.findall(r"agent session: pane %\d+ \"claude\" typed resume confirmed by foreground", log)
    check(len(confirmed) >= 1, f"foreground_confirmed: server.log says the foreground confirmed it "
                               f"({len(confirmed)} line(s))")
    check("record dropped on restore" not in log, "foreground_confirmed: nothing was dropped")
    check(SESSION_ID not in log, "foreground_confirmed: the confirmation line holds no id")


def foreground_argv0(rig):
    """`claude` is a link to a binary whose own name is `2.1.251`, as Claude
    Code's installer lays it out. macOS names a process after its executable,
    so there only argv[0] says claude; the resume must still be confirmed."""
    versions = f"{rig.bin}/versions"
    os.makedirs(versions, exist_ok=True)
    os.rename(f"{rig.bin}/claude", f"{versions}/2.1.251")
    os.symlink(f"{versions}/2.1.251", f"{rig.bin}/claude")
    first, second = restart_twice(rig, "resumed-stays")
    check(first == [[f"--resume={SESSION_ID}", ""]], f"foreground_argv0: the first restart resumed it (got {first})")
    check(len(second) == 2, f"foreground_argv0: the second restart typed it again (got {len(second)} run(s))")
    check(re.search(r"typed resume confirmed by foreground", rig.t.log("server")) is not None,
          "foreground_argv0: server.log says the foreground confirmed it")


def re_reported(rig):
    first, second = restart_twice(rig, "resumed-reports")
    check(first == [[f"--resume={SESSION_ID}", ""]], f"re_reported: the first restart resumed it (got {first})")
    resumed = [r for r in rig.hook_runs()[1:]]
    check(len(resumed) >= 1 and all(r["rc"] == 0 and r["stdout"] == "" and r["stderr"] == "" for r in resumed),
          f"re_reported: the resumed stub reported its session silently (got {resumed})")
    check(len(second) == 2, f"re_reported: a session its agent reported again is typed again (got {len(second)} run(s))")


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


def extra_args(rig):
    start_agent(rig)
    check(rig.wait(lambda: not no_record(rig)), "extra_args: state.json holds the record")
    runs = restart_and_collect(rig, wait=12.0)
    check(runs == [["--dangerously-skip-permissions", f"--resume={SESSION_ID}", ""]],
          f"extra_args: the configured word comes before --resume=<id> (got {runs})")
    log = rig.t.log("server")
    check(re.search(r'typed resume for pane %\d+ agent="claude" \(\d+ bytes\)', log) is not None,
          "extra_args: server.log names the typed resume")
    check(SESSION_ID not in log, "extra_args: server.log never holds the id")


def bad_extra_args(rig):
    first, second = restart_twice(rig, "resumed-stays")
    plain = [f"--resume={SESSION_ID}", ""]
    check(first == [plain], f"bad_extra_args: the first restart resumed it plainly (got {first})")
    check(second == [plain, plain],
          f"bad_extra_args: the second restart resumed it plainly (got {second})")
    held = lambda: any(r.get("value") == SESSION_ID
                       for r in (rig.state() or {}).get("agent_sessions", {}).values())
    check(rig.wait(held), f"bad_extra_args: state.json still holds the record "
                          f"(got {(rig.state() or {}).get('agent_sessions')})")
    log = rig.t.log("server")
    for agent in ("claude", "codex"):
        check(f'[general.resume_agent_args] "{agent}" dropped' in log,
              f"bad_extra_args: server.log says {agent}'s entry was dropped")
    check('[general.resume_agent_args] "cluade" names no agent' in log,
          "bad_extra_args: server.log says cluade names no agent")
    check("zzword" not in log, "bad_extra_args: server.log never holds a configured word")


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
    check(runs == [[f"--resume={SESSION_ID}", ""]], f"duplicate: the restart resumed it once (got {runs})")


MODES = {"fast": fast, "duplicate": duplicate, "never_reported": never_reported,
         "foreground_confirmed": foreground_confirmed, "foreground_argv0": foreground_argv0,
         "other_agent": other_agent,
         "re_reported": re_reported, "typed_at_prompt": typed_at_prompt,
         "stub_exited": stub_exited, "bad_id": bad_id, "pane_unset": pane_unset,
         "setsid": setsid,
         "slow_rc": (slow_rc, "", {"ENV": os.path.join(FIXTURES, "slow_rc.sh")}),
         "slow_rc_zsh": (slow_rc_zsh, "", {"SHELL": "/bin/zsh", "ZDOTDIR": ZDOTDIR}),
         "disabled": (disabled, "resume_agents = false\n"),
         "extra_args": (extra_args, '[general.resume_agent_args]\n'
                                    'claude = ["--dangerously-skip-permissions"]\n'
                                    'codex = ["--from-codex"]\n'
                                    'not-an-agent = ["--from-nobody"]\n'),
         "bad_extra_args": (bad_extra_args, '[general.resume_agent_args]\n'
                                            'claude = ["zzword\\u0003b"]\n'
                                            'codex = ["zzword\\\\b"]\n'
                                            'cluade = ["--zzword"]\n')}


def main():
    modes = sys.argv[1:] or list(MODES)
    newest = max(os.path.getmtime(os.path.join(d, f))
                 for d, _, fs in os.walk(os.path.join(os.path.dirname(BIN), "..", "..", "src"))
                 for f in fs)
    check(os.path.getmtime(BIN) >= newest, f"{BIN} is newer than every file under src/")
    for mode in modes:
        entry = MODES[mode] if isinstance(MODES[mode], tuple) else (MODES[mode],)
        case, extra, env = (entry + ("", None)[len(entry) - 1:])[:3]
        rig = Rig(CONFIG + extra, env)
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
