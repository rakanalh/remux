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
  m2  a session start then two agent_starts, the same file -> ONE call (the
      start's, Unchanged): agent_start fires every turn, and an unchanged
      session spawns nothing
  m3  agent_start, the file changed under the agent -> session_held
  m4  session_switch with an allowlisted reason -> Replaced
  m5  pi outside its TUI / omp with no UI -> no call, no connection
  m6  omp with OMPCODE=1 (a nested omp) -> no call, no connection
  m7  no absolute session file -> --id, Stored as an id (a pane of its own)

opencode (the stub, installed as `opencode`, runs node with
`opencode_driver.mjs`, which imports the INSTALLED plugins with the API surface
the T18 spike recorded; the driver records each CLI call):
  w1  server plugin under `opencode run`: a root session, then a child of it
      (parentID), then the child updated -> ONE call, for the ROOT, Stored
  w2  server plugin under `opencode serve` -> no call
  w3  server plugin inside the full TUI (Bun.main is the TUI worker) -> no call
  w4  TUI plugin: the route goes from home to a CHILD session -> one call,
      --id <root> --source select, Stored; the same route polled for a second
      makes no second call
  w5  TUI plugin: the route moves to another root session -> Replaced
  w6  server plugin under `opencode --mini` -> reports, as `run` does
then the same two files driven as OpenCode 2.x drives them (`api: v2`):
  w7  the server plugin has a `setup`, and calling it (v2's shared background
      service, or 1.18's TUI worker, which calls it beside `server`) makes no
      call and no connection
  w8  TUI `setup`: the route goes from home to a CHILD session -> one call,
      --id <root> --source select, Stored; polled again, no second call
  w9  TUI `setup`: the child is on screen before the store has loaded it ->
      still one call, for the ROOT, never the child first
  w10 TUI `setup` in a TUI started with `--server <url>` or `--server=<url>`
      (a session on another machine) -> no call, no connection
  w11 TUI `setup` with `--standalone` -> reports
  w12 TUI `setup`: the route moves to another root -> Replaced

The *_real modes (OPTIONAL, never a failure, not in the default run) start the
REAL agent: claude_real, codex_real (and codex_real_nested, the same start
inside an outer codex thread, which must make no report), pi_real (needs
REMUX_PI_CLI, the path of a pi cli.js from a throwaway npm prefix), omp_real
and opencode_real. Each prints SMOKE lines: the report that reached the
server, or the pane's screen when none did. Each hashes the real config it
must not touch before and after (Python hashlib) and FAILS on a change, or on
a check that hashed no file under a path that exists.

omp_real: the REAL omp
from `~/.bun/bin/omp`, started as a job at a shell pane's prompt under the
throwaway HOME with the extension installed, for long enough to start a
session. Prints `SMOKE` lines: whether a report reached the server, or why it
was skipped. A sha256 of every file under the real `~/.omp` is taken before
and after and must not change; a change is the one thing reported as FAIL.

opencode_real (OPTIONAL, never a failure, not in the default run): the REAL
`~/.opencode/bin/opencode --mini`, started as a job at a shell pane's prompt
under `unshare -rn` (no network, so no model call) and the throwaway HOME and
XDG dirs, with the integration installed. `--mini` creates a session at
startup, so the installed server plugin should report it, once OpenCode's
first-start bootstrap is done; offline, that bootstrap can stop first, and the
smoke then says so rather than claiming anything about the plugins. Prints `SMOKE`
lines; a change under the real `~/.config/opencode`, `~/.local/state/opencode`
or `~/.local/share/opencode` is the one thing reported as FAIL.

Run: python3 tests/pty/agent_session_hooks.py [claude codex pi omp opencode omp_real opencode_real ...]
"""
import json
import os
import re
import shlex
import shutil
import signal
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
        # What the real agents hand an extension: pi 0.73's ExtensionContext
        # has `hasUI` and no `mode`; omp 18.3 has both.
        ui = {"hasUI": True} if agent == "pi" else {"hasUI": True, "mode": "tui"}
        no_ui = {"hasUI": False} if agent == "pi" else {"hasUI": False, "mode": "print"}
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
        drive(main, f"m2 {agent} agent_start, same file, makes no call", {**ui, "file": first},
              [{"type": "session_start", "reason": start}, {"type": "agent_start"},
               {"type": "agent_start"}], "Unchanged")
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


def opencode():
    os.makedirs(BINDIR, exist_ok=True)
    fakeclaude.install(f"{BINDIR}/opencode")
    rig = Rig("opencode")
    try:
        # The server's environment sets XDG_CONFIG_HOME, which OpenCode's dir
        # follows when OPENCODE_CONFIG_DIR is unset.
        config = f"{rig.env['XDG_CONFIG_HOME']}/opencode"
        os.makedirs(config)
        rc, err = rig.integrate("opencode-resume")
        check(rc == 0, f"setup: opencode-resume installs into the throwaway HOME (rc={rc}, err={err!r})")
        server = f"{config}/plugins/remux-agent-session.js"
        tui = f"{config}/plugins/remux-tui-session/tui.js"
        record = f"{rig.srv.rundir}/calls.jsonl"
        count = [0]

        def stub(name):
            fake = FakeClaude.__new__(FakeClaude)
            fake.ctl, fake.log = f"{rig.srv.rundir}/{name}.ctl", f"{rig.srv.rundir}/{name}.log"
            fake.pane = rig.spawn([f"{BINDIR}/opencode", fake.ctl, fake.log, FIXTURES, BIN])
            end = time.time() + 8
            while time.time() < end and not os.path.exists(fake.log):
                time.sleep(0.05)
            fake.tell(f"env:NODE={NODE}")
            return fake

        def drive(fake, plugin, spec):
            count[0] += 1
            path = f"{rig.srv.rundir}/oc-spec-{count[0]}.json"
            with open(path, "w") as f:
                json.dump({**spec, "record": record}, f)
            before_calls = len(open(record).readlines()) if os.path.exists(record) else 0
            before_conn, before = rig.connections(), len(rig.verdicts(fake.pane))
            fake.tell(f"node={plugin}|{path}|opencode_driver.mjs")
            ran = [e for e in fake.entries() if e.get("event") == "node"][-1:]
            time.sleep(0.3)
            calls = [json.loads(c) for c in (open(record).readlines()[before_calls:]
                                             if os.path.exists(record) else [])]
            return (ran[0] if ran else None, calls, rig.verdicts(fake.pane)[before:],
                    rig.connections() - before_conn)

        def called(ran):
            lines = [ln for ln in (ran or {}).get("stdout", "").splitlines() if ln.startswith("{")]
            return json.loads(lines[0]).get("called") if lines else None

        def created(sid, parent=None, kind="session.created"):
            info = {"id": sid, **({"parentID": parent} if parent else {})}
            return {"type": kind, "properties": {"sessionID": sid, "info": info}}

        root, child, other = "ses_root0001aaaa", "ses_child002bbbb", "ses_other003cccc"
        run_pane = stub("oc-run")
        ran, calls, got, _ = drive(run_pane, server, {"argv": ["run", "hello"], "events": [
            created(root), created(child, root), created(child, root, "session.updated")]})
        check(ran and ran["rc"] == 0, f"w1 the driver ran the server plugin (got {ran})")
        check([c["args"] for c in calls] == [["agent", "session", "opencode", "--id", root]]
              and all(c["code"] == 0 and c["stdout"] == "" for c in calls),
              f"w1 one call, for the root, exit 0, nothing on stdout (got {calls})")
        check(got == ["Stored"], f"w1 the server stored it (got {got})")

        serve_pane = stub("oc-serve")
        _, calls, got, conns = drive(serve_pane, server, {"argv": ["serve", "--port", "1"],
                                                          "events": [created(root)]})
        check(calls == [] and got == [] and conns == 0, f"w2 serve makes no call (got {calls})")
        _, calls, got, conns = drive(serve_pane, server, {
            "argv": [], "bun_main": "/$bunfs/root/src/cli/tui/worker.js", "events": [created(root)]})
        check(calls == [] and got == [] and conns == 0, f"w3 the TUI's own process makes no call (got {calls})")

        tui_pane = stub("oc-tui")
        _, calls, got, _ = drive(tui_pane, tui, {
            "parents": {child: root},
            "routes": [{"after_ms": 300, "route": {"name": "session", "params": {"sessionID": child}}}],
            "run_ms": 1600})
        check([c["args"] for c in calls] == [["agent", "session", "opencode", "--id", root, "--source", "select"]]
              and all(c["stdout"] == "" for c in calls),
              f"w4 the TUI plugin reports the selected child's ROOT once, with select (got {calls})")
        check(got == ["Stored"], f"w4 the server stored it (got {got})")
        _, calls, got, _ = drive(tui_pane, tui, {
            "routes": [{"after_ms": 300, "route": {"name": "session", "params": {"sessionID": other}}}],
            "run_ms": 1200})
        check([c["args"][4] for c in calls] == [other] and got == ["Replaced"],
              f"w5 a different session selected is Replaced (calls {calls}, got {got})")

        mini_pane = stub("oc-mini")
        _, calls, got, _ = drive(mini_pane, server, {"argv": ["--mini"], "events": [created(other)]})
        check([c["args"][4] for c in calls] == [other] and got == ["Stored"],
              f"w6 --mini reports from the server plugin (calls {calls}, got {got})")

        v2_server = stub("oc2-server")
        for argv in (["serve", "--service"], []):
            ran, calls, got, conns = drive(v2_server, server, {"api": "v2", "argv": argv})
            check(ran and ran["rc"] == 0 and called(ran) == "setup" and calls == [] and got == []
                  and conns == 0,
                  f"w7 the server plugin's setup (argv {argv}) runs and makes no call "
                  f"(hook {called(ran)}, rc {ran and ran['rc']}, calls {calls}, conns {conns})")

        def session_route(sid):
            return {"type": "session", "sessionID": sid}

        v2_tui = stub("oc2-tui")
        ran, calls, got, _ = drive(v2_tui, tui, {
            "api": "v2", "sessions": {child: {"parentID": root}, root: {}},
            "routes": [{"after_ms": 300, "route": session_route(child)}], "run_ms": 1600})
        check(called(ran) == "setup"
              and [c["args"] for c in calls] == [["agent", "session", "opencode", "--id", root, "--source", "select"]]
              and all(c["stdout"] == "" for c in calls),
              f"w8 v2 setup reports the selected child's ROOT once, with select (hook {called(ran)}, calls {calls})")
        check(got == ["Stored"], f"w8 the server stored it (got {got})")

        late = stub("oc2-late")
        ran, calls, got, _ = drive(late, tui, {
            "api": "v2", "sessions": {child: {"parentID": root, "loaded_after_ms": 900},
                                      root: {"loaded_after_ms": 900}},
            "routes": [{"after_ms": 100, "route": session_route(child)}], "run_ms": 2200})
        check(called(ran) == "setup" and [c["args"][4] for c in calls] == [root] and got == ["Stored"],
              f"w9 a child on screen before the store loads it is reported once, as its root "
              f"(hook {called(ran)}, calls {[c['args'][4] for c in calls]}, got {got})")

        for flag in (["--server", "http://10.0.0.9:4096"], ["--server=http://10.0.0.9:4096"]):
            remote = stub("oc2-remote" + str(len(flag)))
            ran, calls, got, conns = drive(remote, tui, {
                "api": "v2", "argv": flag, "sessions": {root: {}},
                "routes": [{"after_ms": 200, "route": session_route(root)}], "run_ms": 1200})
            check(called(ran) == "setup" and calls == [] and got == [] and conns == 0,
                  f"w10 a TUI attached with {flag[0].split('=')[0]} makes no call "
                  f"(hook {called(ran)}, calls {calls}, conns {conns})")

        standalone = stub("oc2-standalone")
        ran, calls, got, _ = drive(standalone, tui, {
            "api": "v2", "argv": ["--standalone"], "sessions": {root: {}},
            "routes": [{"after_ms": 200, "route": session_route(root)}], "run_ms": 1200})
        check(called(ran) == "setup" and [c["args"][4] for c in calls] == [root] and got == ["Stored"],
              f"w11 --standalone reports (hook {called(ran)}, calls {calls}, got {got})")

        ran, calls, got, _ = drive(v2_tui, tui, {
            "api": "v2", "sessions": {other: {}},
            "routes": [{"after_ms": 300, "route": session_route(other)}], "run_ms": 1200})
        check([c["args"][4] for c in calls] == [other] and got == ["Replaced"],
              f"w12 v2: a different session selected is Replaced (calls {calls}, got {got})")
        check("panicked at" not in rig.srv.log(), "opencode: no panic in server.log")
    finally:
        rig.close()


# The developer's PATH as this harness was started with it, before any Rig
# makes PATH hermetic: the real-binary smokes find their binary through it,
# and nothing else does.
DEV_PATH = os.environ.get("PATH", "")
REAL_HOME = os.path.expanduser("~")


def dev_which(name):
    return shutil.which(name, path=DEV_PATH)


def launcher(path):
    """The argv that runs `path` with no PATH lookup: a `#!/usr/bin/env X`
    script gets X resolved from the developer's PATH, since the pane's PATH
    holds no interpreter beyond the system's."""
    path = os.path.realpath(path)
    try:
        with open(path, "rb") as f:
            first = f.readline(200).decode("utf-8", "replace").strip()
    except OSError:
        return None
    if first.startswith("#!"):
        words = first[2:].split()
        if words and os.path.basename(words[0]) == "env" and len(words) > 1:
            interpreter = dev_which(words[1])
            return [interpreter, path] if interpreter else None
        return [*words, path]
    return [path]


class RealDirs:
    """A sha256 of every file under the real dirs (and files) a smoke must not
    touch, by Python's hashlib, so it needs no `sha256sum` (macOS has none).

    A path that exists but yields no file is a failure of the check itself, not
    a pass: two empty digests compare equal whatever happened."""

    def __init__(self, label, paths):
        # Against the REAL home, captured at import: a Rig points HOME at a
        # throwaway one, and `~` expanded after that would hash the wrong tree.
        self.label = label
        self.paths = [REAL_HOME + p[1:] if p.startswith("~") else p for p in paths]
        self.before = self.digest()

    def digest(self):
        import hashlib
        out = {}
        for root in self.paths:
            if os.path.isfile(root):
                files = [root]
            else:
                files = [os.path.join(d, f) for d, _, fs in os.walk(root) for f in fs]
            for path in files:
                try:
                    with open(path, "rb") as f:
                        out[path] = hashlib.sha256(f.read()).hexdigest()
                except OSError:
                    out[path] = "unreadable"
        return out

    def existing(self):
        return [p for p in self.paths if os.path.exists(p)]

    def verify(self):
        after = self.digest()
        present = self.existing()
        if not present:
            check(True, f"{self.label}: no real config exists here, and the smoke created none "
                        f"({', '.join(self.paths)})")
            return
        check(len(self.before) > 0, f"{self.label}: the real-config digest hashed "
                                    f"{len(self.before)} file(s) under {present}")
        changed = sorted(p for p in set(self.before) | set(after) if self.before.get(p) != after.get(p))
        check(changed == [], f"{self.label}: the real config is byte for byte as it was "
                             f"({len(after)} files; changed: {changed[:5]})")


def screen_of(rig, pane):
    c = Client(rig.srv.sock)
    c.hello()
    c.send({"SubscribePane": {"pane_id": pane, "cols": 110, "rows": 30, "size_demand": False}})
    rows = []
    try:
        for msg in c.drain(1.5):
            if name_of(msg) == "PaneContent" and msg["PaneContent"]["pane_id"] == pane:
                rows = ["".join(cell["c"] for cell in row).rstrip() for row in msg["PaneContent"]["cells"]]
    finally:
        c.close()
    return [r for r in rows if r.strip()]


def real_smoke(name, agent, argv, prepare, real_paths, wait_s=30, env_line=""):
    """Start the REAL `argv` as a job at a shell pane's prompt, with remux's
    integration installed by `prepare(rig)` under the throwaway HOME, and say
    whether its report reached the server. Never a failure, except a change to
    the real config (RealDirs) or a check that hashed nothing."""
    if not argv or not all(argv) or not all(os.path.exists(a) for a in argv if a.startswith("/")):
        print(f"SMOKE {name}: skipped, the real {agent} is not installed here")
        return
    real = RealDirs(name, real_paths)
    rig = Rig(name)
    try:
        why = prepare(rig)
        if why:
            print(f"SMOKE {name}: skipped, {why}")
            return
        c = Client(rig.srv.sock)
        c.hello()
        c.send({"CliSpawn": {"session": "main", "placement": "SplitBelow"}})
        pane = None
        for m in c.drain(2.0):
            if name_of(m) == "CliSpawned":
                pane = m["CliSpawned"]["pane_id"]
        c.close()
        time.sleep(0.5)
        line = env_line + " ".join(shlex.quote(a) for a in argv) + "\n"
        rig.cli.send({"InputToPane": {"pane_id": pane, "data": list(line.encode())}})
        end = time.time() + wait_s
        found = None
        while time.time() < end and found is None:
            found = re.search(rf'agent session: pane %{pane} "{agent}" (\w+) (\d+) bytes source=(\S+) -> (\w+)',
                              rig.srv.log())
            time.sleep(0.5)
        if found:
            print(f"SMOKE {name}: the real {agent} reported its session: kind={found.group(1)} "
                  f"{found.group(2)} bytes source={found.group(3)} -> {found.group(4)}")
        else:
            refused = re.findall(rf"agent session: pane %{pane} .*", rig.srv.log())
            screen = screen_of(rig, pane)
            print(f"SMOKE {name}: no report from the real {agent} within {wait_s} s "
                  f"(server lines for the pane: {refused[:2]}); the pane shows:")
            for row in screen[-12:]:
                print(f"SMOKE {name}:   | {row[:100]}")
        return rig, pane, found
    finally:
        rig.close()
        end_processes_with_home(name, rig.home)
        real.verify()


def processes_with_home(home):
    """Every process whose environment has HOME=`home`: what a real agent
    left running under a throwaway HOME (Codex detaches an app-server daemon
    that outlives its pane)."""
    found = []
    if os.path.isdir("/proc"):
        for pid in filter(str.isdigit, os.listdir("/proc")):
            try:
                with open(f"/proc/{pid}/environ", "rb") as f:
                    env = f.read().split(b"\0")
            except OSError:
                continue
            if f"HOME={home}".encode() in env:
                found.append(int(pid))
    else:
        out = subprocess.run(["ps", "-axE", "-o", "pid=,command="], capture_output=True,
                             timeout=20).stdout.decode("utf-8", "replace")
        for line in out.splitlines():
            pid, _, rest = line.strip().partition(" ")
            # The harness's own HOME is the throwaway while a Rig is up, so
            # this `ps` inherits it and lists itself.
            if rest.startswith("ps -axE"):
                continue
            if pid.isdigit() and f" HOME={home} " in f" {rest} ":
                found.append(int(pid))
    return [p for p in found if p != os.getpid()]


def end_processes_with_home(name, home):
    left = processes_with_home(home)
    for pid in left:
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass
    time.sleep(0.5)
    for pid in processes_with_home(home):
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    check(processes_with_home(home) == [],
          f"{name}: nothing the real agent started is left running ({len(left)} ended)")


def needs_dir(rig, path, harness):
    os.makedirs(path, exist_ok=True)
    rc, err = rig.integrate(harness)
    return None if rc == 0 else f"the install failed ({err.strip()})"


def claude_real():
    binary = f"{REAL_HOME}/.local/bin/claude"
    real_smoke("claude_real", "claude", launcher(binary) if os.path.exists(binary) else None,
               lambda rig: needs_dir(rig, f"{rig.home}/.claude", "claude-resume"),
               ["~/.claude/settings.json", "~/.claude/settings.local.json", "~/.claude/CLAUDE.md"])


def codex_real():
    binary = dev_which("codex")
    argv = launcher(binary) if binary else None
    real_smoke("codex_real", "codex", argv,
               lambda rig: needs_dir(rig, f"{rig.home}/.codex", "codex-resume"),
               ["~/.codex/config.toml", "~/.codex/hooks.json"])
    # The nested-run filter against the real codex: the same start, inside an
    # outer codex's thread. A report there would mean the filter failed.
    real_smoke("codex_real_nested", "codex", argv,
               lambda rig: needs_dir(rig, f"{rig.home}/.codex", "codex-resume"),
               ["~/.codex/config.toml", "~/.codex/hooks.json"],
               env_line="CODEX_THREAD_ID=an-outer-session-0000 ")


def pi_real():
    cli = os.environ.get("REMUX_PI_CLI")
    node = dev_which("node")
    argv = [node, os.path.realpath(cli)] if cli and node else None
    real_smoke("pi_real", "pi", argv,
               lambda rig: needs_dir(rig, f"{rig.home}/.pi/agent", "pi-resume"),
               ["~/.pi"])


def omp_real():
    binary = dev_which("omp") or f"{REAL_HOME}/.bun/bin/omp"
    real_smoke("omp_real", "omp", launcher(binary) if os.path.exists(binary) else None,
               lambda rig: needs_dir(rig, f"{rig.home}/.omp/agent", "omp-resume"),
               ["~/.omp"], wait_s=25)


def opencode_real():
    """`--mini` creates a session at startup, so the server plugin reports it
    once OpenCode's first-start bootstrap (which downloads @opencode-ai/plugin
    into the throwaway config dir) is done. Under `unshare -rn` (Linux, no
    network) that bootstrap cannot finish; where unshare is absent (macOS) it
    runs with the network, as a first start does."""
    # REMUX_OPENCODE_BIN picks a version installed in a throwaway npm prefix:
    # the integration targets OpenCode 1.18, and a machine's own may be newer.
    binary = (os.environ.get("REMUX_OPENCODE_BIN") or dev_which("opencode")
              or f"{REAL_HOME}/.opencode/bin/opencode")
    # REMUX_SMOKE_NETWORK=1 lets the bootstrap reach the network, as on a
    # machine with no unshare; by default Linux runs it offline.
    unshare = None if os.environ.get("REMUX_SMOKE_NETWORK") == "1" else shutil.which("unshare")
    base = launcher(binary) if os.path.exists(binary) else None
    if not base:
        argv = None
    elif unshare:
        argv = [unshare, "-rn", *base, "--mini"]
    else:
        argv = [*base, "--mini"]

    def prepare(rig):
        return needs_dir(rig, f"{rig.env['XDG_CONFIG_HOME']}/opencode", "opencode-resume")
    real_smoke("opencode_real", "opencode", argv, prepare,
               ["~/.config/opencode", "~/.local/state/opencode", "~/.local/share/opencode"],
               wait_s=60)


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
         "opencode": opencode, "omp_real": omp_real, "opencode_real": opencode_real,
         "claude_real": claude_real, "codex_real": codex_real, "pi_real": pi_real}
DEFAULT = ["claude", "codex", "pi", "omp", "opencode"]


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
