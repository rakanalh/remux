#!/usr/bin/env python3
"""The INSTALLED agent-session hooks, run by stub agents with recorded
payloads. No real agent runs: every agent is a stub, launched by absolute
path, and the server's PATH is the system directories alone, so no real
agent on the developer's PATH can run in its place.

The hooks are installed by `remux integrate <agent>-resume` into a throwaway
HOME; the stub runs them through `sh -c` from its own pane, as the agent
does. The server's side of each run is read from `server.log`: the verdict
line for the stub's pane (`-> Stored`, `-> Unchanged`, `-> Replaced`), and
the per-connection identity line, whose count says whether a run connected
at all.

claude (the stub is `fakeclaude.py`, a compiled leader named `claude`):
  h1  startup        -> Stored; rc 0, nothing on stdout or stderr
  h2  resume, same id -> Unchanged
  h3  clear, a new id -> Replaced
  h4  compact, same   -> Unchanged
  h5  a subagent's payload (agent_id) -> no connection, rc 0, no output
  h6  CURSOR_VERSION in the hook env, startup -> no connection, no output
  h7  a hook killed by its agent at the timeout leaves the pane running the
      stub and the server answering

codex (the same stub, pointed at the installed `$CODEX_HOME/hooks.json`):
  k1  startup with no CODEX_THREAD_ID -> Stored
  k2  CODEX_THREAD_ID equal to the payload's session_id -> passes the filter
      (Unchanged, the same session)
  k3  CODEX_THREAD_ID naming ANOTHER session (a nested `codex exec`) -> no
      connection, no output
  k4  a payload with no transcript_path -> no connection, no output
  k5  in a pane whose claude session is held, codex's startup is refused
      session_held: one stderr line, rc 0, nothing on stdout

pi and omp (the same stub, installed as `pi` / `omp`, runs node with
`pi_omp_driver.mjs`, which imports the INSTALLED extension with a fake ctx;
the extension's execFile reaches the CLI inside the pane, and the driver
records each call's args, stdout and stderr):
  m1  session_start with an absolute session file -> --path, Stored as a path
  m2  agent_start, the same file -> Unchanged
  m3  agent_start, the file changed under the agent -> session_held
  m4  session_switch with an allowlisted reason -> Replaced
  m5  pi outside its TUI / omp with no UI -> no call, no connection
  m6  omp with OMPCODE=1 (a nested omp) -> no call, no connection
  m7  no absolute session file -> --id, Stored as an id (a pane of its own)

omp_real (OPTIONAL, never a failure, not in the default run): the REAL omp
from `~/.bun/bin/omp`, started as a job at a shell pane's prompt under the
throwaway HOME with the extension installed, for long enough to start a
session. Prints `SMOKE` lines: whether a report reached the server, or why it
was skipped. A sha256 of every file under the real `~/.omp` is taken before
and after and must not change; a change is the one thing reported as FAIL.

Run: python3 tests/pty/agent_session_hooks.py [claude codex pi omp omp_real ...]
"""
import json
import os
import re
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "frame"))
sys.path.insert(0, HERE)
from harness import BIN, Client, Server, name_of  # noqa: E402
import fakeclaude  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-agsess-hooks")
BINDIR = RUNDIR + "-bin"
FIXTURES = os.path.join(HERE, "..", "fixtures", "agent_session")
HERMETIC_PATH = "/usr/local/bin:/usr/bin:/bin"

FAILURES = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


class Rig:
    """A server, an attached client, and a throwaway HOME the hooks are
    installed into."""

    def __init__(self, name):
        self.home = f"{RUNDIR}-{name}-home"
        subprocess.run(["rm", "-rf", self.home])
        os.makedirs(self.home)
        os.environ["HOME"] = self.home
        os.environ["PATH"] = HERMETIC_PATH
        for var in ("REMUX_SESSION", "REMUX_PANE", "CURSOR_VERSION", "CODEX_THREAD_ID"):
            os.environ.pop(var, None)
        self.srv = Server(f"{RUNDIR}-{name}").start()
        self.env = {k: v for k, v in self.srv.env.items() if k not in ("REMUX_SESSION", "REMUX_PANE")}
        self.cli = Client(self.srv.sock)
        self.cli.hello()
        self.cli.send({"CreateSession": {"name": "main", "folder": None}})
        self.cli.send({"Attach": {"session_name": "main"}})
        self.cli.send({"Resize": {"cols": 160, "rows": 50}})
        self.cli.drain(0.8)

    def integrate(self, harness):
        done = subprocess.run([BIN, "integrate", harness, "--allow-dev-path"], env=self.env,
                              capture_output=True, timeout=20)
        return done.returncode, done.stderr.decode(errors="replace")

    def spawn(self, argv):
        c = Client(self.srv.sock)
        c.hello()
        c.send({"CliSpawn": {"session": "main", "placement": "SplitBelow", "argv": argv}})
        try:
            for m in c.drain(3.0):
                if name_of(m) == "CliSpawned":
                    return m["CliSpawned"]["pane_id"]
        finally:
            c.close()
        return None

    def connections(self):
        return len(set(re.findall(r"identity: client_id=(\d+) peer=", self.srv.log())))

    def verdicts(self, pane):
        return re.findall(rf"agent session: pane %{pane} \S+ \S+ \d+ bytes source=\S+ -> (\w+)",
                          self.srv.log())

    def answering(self):
        c = Client(self.srv.sock)
        try:
            return "protocol_version" in c.hello()
        finally:
            c.close()

    def close(self):
        self.cli.close()
        self.srv.kill()


class FakeClaude:
    def __init__(self, rig):
        self.ctl, self.log = f"{rig.srv.rundir}/claude.ctl", f"{rig.srv.rundir}/claude.log"
        self.pane = rig.spawn([f"{BINDIR}/claude", self.ctl, self.log, FIXTURES, BIN])
        end = time.time() + 8
        while time.time() < end and not os.path.exists(self.log):
            time.sleep(0.05)

    def entries(self):
        try:
            return [json.loads(ln) for ln in open(self.log).read().splitlines() if ln.strip()]
        except OSError:
            return []

    def tell(self, command, timeout=30.0):
        start = len(self.entries())
        with open(self.ctl + ".tmp", "w") as f:
            f.write(command + "\n")
        os.replace(self.ctl + ".tmp", self.ctl)
        end = time.time() + timeout
        while time.time() < end:
            if any(e.get("ctl") == command for e in self.entries()[start:]):
                break
            time.sleep(0.05)
        return [e for e in self.entries()[start:] if e.get("event") == "start"]


def runner(rig, fake):
    """Run the stub's SessionStart hooks once with a fixture and check the
    run, then the server's verdict for the stub's pane (or that it never
    connected)."""
    def run(label, fixture, want, connects=True, stderr=""):
        before_conn, before = rig.connections(), len(rig.verdicts(fake.pane))
        runs = fake.tell(f"start={FIXTURES}/{fixture}")
        time.sleep(0.3)
        check(len(runs) == 1 and runs[0]["rc"] == 0 and runs[0]["stdout"] == ""
              and (runs[0]["stderr"] == "" if not stderr else
                   len(runs[0]["stderr"].splitlines()) == 1 and stderr in runs[0]["stderr"]),
              f"{label}: the hook exits 0, nothing on stdout, stderr {stderr or 'empty'} (got {runs})")
        got = rig.verdicts(fake.pane)[before:]
        if connects:
            check(got == [want], f"{label}: the server answered {want} (got {got})")
        else:
            check(rig.connections() == before_conn and got == [],
                  f"{label}: the hook never connected ({rig.connections() - before_conn} new)")
    return run


def codex():
    os.makedirs(BINDIR, exist_ok=True)
    fakeclaude.install(f"{BINDIR}/claude")
    rig = Rig("codex")
    try:
        codex_home = f"{rig.home}/.codex"
        os.makedirs(codex_home)
        os.makedirs(f"{rig.home}/.claude")
        for harness in ("codex-resume", "claude-resume"):
            rc, err = rig.integrate(harness)
            check(rc == 0, f"setup: {harness} installs into the throwaway HOME (rc={rc}, err={err!r})")
        fake = FakeClaude(rig)
        fake.tell(f"env:HOOKS_FILE={codex_home}/hooks.json")
        run = runner(rig, fake)
        session = json.load(open(f"{FIXTURES}/codex_startup.json"))["session_id"]
        run("k1 startup", "codex_startup.json", "Stored")
        fake.tell(f"env:CODEX_THREAD_ID={session}")
        run("k2 CODEX_THREAD_ID is this session", "codex_startup.json", "Unchanged")
        run("k3 CODEX_THREAD_ID is another session", "codex_nested.json", None, connects=False)
        fake.tell("unenv:CODEX_THREAD_ID")
        run("k4 no transcript_path", "codex_no_transcript.json", None, connects=False)

        held = FakeClaude.__new__(FakeClaude)
        held.ctl, held.log = f"{rig.srv.rundir}/held.ctl", f"{rig.srv.rundir}/held.log"
        held.pane = rig.spawn([f"{BINDIR}/claude", held.ctl, held.log, FIXTURES, BIN])
        end = time.time() + 8
        while time.time() < end and not os.path.exists(held.log):
            time.sleep(0.05)
        run = runner(rig, held)
        run("k5 setup: claude startup", "claude_startup.json", "Stored")
        held.tell(f"env:HOOKS_FILE={codex_home}/hooks.json")
        run("k5 codex startup over claude", "codex_startup.json", "session_held",
            stderr="session_held")
        check("panicked at" not in rig.srv.log(), "codex: no panic in server.log")
    finally:
        rig.close()


NODE = shutil.which("node")


def extension_mode(agent):
    os.makedirs(BINDIR, exist_ok=True)
    fakeclaude.install(f"{BINDIR}/{agent}")
    rig = Rig(agent)
    try:
        agent_dir = f"{rig.home}/.pi/agent" if agent == "pi" else f"{rig.home}/.omp/agent"
        os.makedirs(agent_dir)
        rc, err = rig.integrate(f"{agent}-resume")
        check(rc == 0, f"setup: {agent}-resume installs into the throwaway HOME (rc={rc}, err={err!r})")
        ext = f"{agent_dir}/extensions/remux-agent-session.ts"
        ui = {"mode": "tui"} if agent == "pi" else {"hasUI": True}
        no_ui = {"mode": "print"} if agent == "pi" else {"hasUI": False}
        start = "new" if agent == "pi" else None
        switch = "resume"
        record = f"{rig.srv.rundir}/calls.jsonl"
        specs = [0]

        def stub(name):
            fake = FakeClaude.__new__(FakeClaude)
            fake.ctl, fake.log = f"{rig.srv.rundir}/{name}.ctl", f"{rig.srv.rundir}/{name}.log"
            fake.pane = rig.spawn([f"{BINDIR}/{agent}", fake.ctl, fake.log, FIXTURES, BIN])
            end = time.time() + 8
            while time.time() < end and not os.path.exists(fake.log):
                time.sleep(0.05)
            fake.tell(f"env:NODE={NODE}")
            return fake

        def drive(fake, label, ctx, events, want, env=None, kind=None):
            specs[0] += 1
            spec = f"{rig.srv.rundir}/spec-{specs[0]}.json"
            with open(spec, "w") as f:
                json.dump({"ctx": ctx, "events": events, "env": env or {}, "record": record}, f)
            calls_before = len(open(record).readlines()) if os.path.exists(record) else 0
            conn_before, before = rig.connections(), len(rig.verdicts(fake.pane))
            fake.tell(f"node={ext}|{spec}")
            ran = [e for e in fake.entries() if e.get("event") == "node"][-1:]
            time.sleep(0.3)
            calls = open(record).readlines()[calls_before:] if os.path.exists(record) else []
            calls = [json.loads(c) for c in calls]
            check(ran and ran[0]["rc"] == 0, f"{label}: the driver ran the extension (got {ran})")
            got = rig.verdicts(fake.pane)[before:]
            if want is None:
                check(calls == [] and rig.connections() == conn_before and got == [],
                      f"{label}: no call and no connection (calls {calls})")
                return
            check(len(calls) == 1 and calls[0]["code"] == 0 and calls[0]["stdout"] == "",
                  f"{label}: one CLI call, exit 0, nothing on stdout (got {calls})")
            check(got == [want], f"{label}: the server answered {want} (got {got})")
            if kind:
                check(f"--{kind}" in calls[0]["args"] if calls else False,
                      f"{label}: the extension passed --{kind} (args {calls[0]['args'] if calls else None})")

        main = stub(agent)
        first, second = "/home/u/.pi/sessions/one.jsonl", "/home/u/.pi/sessions/two.jsonl"
        drive(main, f"m1 {agent} session_start, absolute file", {**ui, "file": first, "id": "ignored"},
              [{"type": "session_start", "reason": start}], "Stored", kind="path")
        drive(main, f"m2 {agent} agent_start, same file", {**ui, "file": first},
              [{"type": "agent_start"}], "Unchanged")
        drive(main, f"m3 {agent} agent_start, a changed file", {**ui, "file": second},
              [{"type": "agent_start"}], "session_held")
        drive(main, f"m4 {agent} session_switch ({switch})", {**ui, "file": second},
              [{"type": "session_switch", "reason": switch}], "Replaced")
        drive(main, f"m5 {agent} without a UI", {**no_ui, "file": first},
              [{"type": "session_start", "reason": start}], None)
        if agent == "omp":
            drive(main, "m6 omp with OMPCODE=1", {**ui, "file": first},
                  [{"type": "session_start"}], None, env={"OMPCODE": "1"})
        other = stub(f"{agent}-id")
        drive(other, f"m7 {agent} no absolute file, an id", {**ui, "file": "relative.jsonl",
                                                              "id": "7f3c2a9e-id-fallback"},
              [{"type": "session_start", "reason": start}], "Stored", kind="id")
        log = rig.srv.log()
        check(re.search(rf'agent session: pane %{main.pane} "{agent}" path \d+ bytes', log) is not None,
              f"m1 {agent}: the server stored it as a path")
        check("panicked at" not in log, f"{agent}: no panic in server.log")
    finally:
        rig.close()


def omp_real():
    launcher = os.path.realpath(os.path.expanduser("~/.bun/bin/omp"))
    bun = shutil.which("bun") or os.path.expanduser("~/.bun/bin/bun")
    if not (os.path.exists(launcher) and os.path.exists(bun)):
        print("SMOKE omp_real: skipped, no omp or bun installed")
        return
    real = os.path.expanduser("~/.omp")

    def sha():
        out = subprocess.run(["sh", "-c", f"find {real} -type f -exec sha256sum {{}} + 2>/dev/null | sort"],
                             capture_output=True, timeout=60)
        return out.stdout
    before = sha()
    rig = Rig("omp-real")
    try:
        os.makedirs(f"{rig.home}/.omp/agent")
        rc, err = rig.integrate("omp-resume")
        if rc != 0:
            print(f"SMOKE omp_real: skipped, the install failed ({err.strip()})")
            return
        pane = None
        c = Client(rig.srv.sock)
        c.hello()
        c.send({"CliSpawn": {"session": "main", "placement": "SplitBelow"}})
        for m in c.drain(2.0):
            if name_of(m) == "CliSpawned":
                pane = m["CliSpawned"]["pane_id"]
        c.close()
        time.sleep(0.5)
        line = f"{bun} {launcher}\n"
        rig.cli.send({"InputToPane": {"pane_id": pane, "data": list(line.encode())}})
        end = time.time() + 25
        found = None
        while time.time() < end and found is None:
            found = re.search(rf'agent session: pane %{pane} "omp" (\w+) \d+ bytes source=(\S+) -> (\w+)',
                              rig.srv.log())
            time.sleep(0.5)
        if found:
            print(f"SMOKE omp_real: the real omp reported its session: kind={found.group(1)} "
                  f"source={found.group(2)} -> {found.group(3)}")
        else:
            refused = re.findall(rf"agent session: pane %{pane} .*", rig.srv.log())
            print(f"SMOKE omp_real: no report from the real omp within 25 s "
                  f"(lines for the pane: {refused[:3]})")
    finally:
        rig.close()
    check(sha() == before, "omp_real: the real ~/.omp is byte for byte as it was")


def claude():
    os.makedirs(BINDIR, exist_ok=True)
    fakeclaude.install(f"{BINDIR}/claude")
    rig = Rig("claude")
    try:
        os.makedirs(f"{rig.home}/.claude")
        rc, err = rig.integrate("claude-resume")
        check(rc == 0, f"setup: claude-resume installs into the throwaway HOME (rc={rc}, err={err!r})")
        fake = FakeClaude(rig)
        check(fake.pane is not None and fake.entries() is not None, f"setup: the stub runs in %{fake.pane}")

        run = runner(rig, fake)

        run("h1 startup", "claude_startup.json", "Stored")
        run("h2 resume, same id", "claude_resume.json", "Unchanged")
        run("h3 clear, a new id", "claude_clear.json", "Replaced")
        run("h4 compact, same id", "claude_compact.json", "Unchanged")
        run("h5 a subagent", "claude_subagent.json", None, connects=False)
        fake.tell("env:CURSOR_VERSION=1.2.3")
        run("h6 CURSOR_VERSION set", "claude_startup.json", None, connects=False)
        fake.tell("unenv:CURSOR_VERSION")

        fake.tell("env:HOOK_KILL_AFTER_MS=1")
        runs = fake.tell(f"start={FIXTURES}/claude_startup.json")
        fake.tell("unenv:HOOK_KILL_AFTER_MS")
        check(len(runs) == 1 and runs[0]["rc"] != 0 and runs[0]["stdout"] == "",
              f"h7 the hook was killed at its timeout, printing nothing (got {runs})")
        later = fake.tell(f"start={FIXTURES}/claude_compact.json")
        check(len(later) == 1 and later[0]["rc"] == 0,
              f"h7 the stub in %{fake.pane} still runs its hooks afterwards (got {later})")
        check(rig.answering(), "h7 the server still answers a new connection")
        check("panicked at" not in rig.srv.log(), "claude: no panic in server.log")
    finally:
        rig.close()


MODES = {"claude": claude, "codex": codex,
         "pi": lambda: extension_mode("pi"), "omp": lambda: extension_mode("omp"),
         "omp_real": omp_real}
DEFAULT = ["claude", "codex", "pi", "omp"]


def main():
    for mode in sys.argv[1:] or DEFAULT:
        MODES[mode]()
    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
