#!/usr/bin/env python3
"""Agent session reports, the server half: `ClientMessage::AgentSession`.

The harness's own connection is in no pane, so every report it sends names
the pane with `pane: %N`, which is what that field is for. Each case uses a
pane of its own, created through `CliSpawn`, so no case sees another's record.

accept          a claude UUID is Stored on a pane holding nothing.
unchanged       the same report again is Unchanged.
held_other_id   a different claude id is refused session_held.
bad_agent       an agent nobody configured is refused bad_agent.
bad_reference   a 129-byte id, a path for claude, and a control char are each
                refused bad_reference.
not_in_pane     an outside caller that names no pane is refused not_in_pane.
cli_attributed  `remux agent session claude --id` run as a pane's own program,
                with a DIRECT argv so remux's parent is the server and not the
                pane's shell, is attributed to that pane by the server and
                Stored.
cli_stdout      the CLI prints nothing on stdout for any outcome, exits 0, and
                writes one stderr line for a refusal.
cli_outside     with `$REMUX_PANE` unset and no --pane the CLI exits 0, prints
                nothing and never connects.
cli_filters     a subagent payload, a non-SessionStart payload, and
                `CURSOR_VERSION` set each exit 0 without connecting, with
                `$REMUX_PANE` set so that the filter is what stops them; the
                plain SessionStart payload does connect.
persist         state.json holds the record right after the report, before any
                structural change; a report that hangs (a lock-order deadlock
                in the save) is a timeout red here.
restart_types   after `remux restart` the restored /bin/sh pane runs the stub
                `claude` on PATH with `--resume` and the id as two argv words,
                read from the file the stub writes, never from the screen.
redaction       server.log and client.log carry the summary line with the id's
                byte count and never the id.
no_panic        server.log has no `panicked at`.

Run: python3 tests/frame/agent_session.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import re
import socket
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-agentsess")
UUID = "7f3c2a9e-51d4-4b8e-9a61-0c2f5d8e4b17"
OTHER = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"
CLI_ID = "c11a77e1-9d0b-4c3a-8e2f-6b5a4d3c2b1a"
RESTORE_ID = "4e5a1f0c-7b2d-4c8e-a3f9-1d6b0e2c5a7f"
CONFIG = """
[general]
save_sessions = true
automatic_restore = true
"""
STUB = """#!/bin/sh
for a in "$@"; do printf '%s\\n' "$a"; done > "{argv_dir}/$(basename "$0").$$"
"""
FIXTURES = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                        "fixtures", "agent_session")

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


class Rig:
    def __init__(self, rundir, config=None):
        self.srv = Server(rundir)
        # The pane shells inherit the server's PATH, so the stub agents must
        # come first on it. The directory is filled after `start()`, which
        # wipes the run dir.
        self.bin = f"{rundir}/bin"
        self.argv_dir = f"{rundir}/argv"
        os.environ["PATH"] = f"{self.bin}:{os.environ['PATH']}"
        self.srv.start(config=config)
        for d in (self.bin, self.argv_dir):
            os.makedirs(d, exist_ok=True)
        for agent in ("claude", "codex", "opencode", "pi", "omp"):
            stub = f"{self.bin}/{agent}"
            with open(stub, "w") as f:
                f.write(STUB.format(argv_dir=self.argv_dir))
            os.chmod(stub, 0o755)
        self.attach(create=True)

    def attach(self, create=False):
        self.cli = Client(self.srv.sock)
        self.cli.hello()
        if create:
            self.cli.send({"CreateSession": {"name": "main", "folder": None}})
        self.cli.send({"Attach": {"session_name": "main"}})
        self.cli.send({"Resize": {"cols": 160, "rows": 50}})
        self.cli.drain(0.8)

    def restart(self):
        self.cli.close()
        self.srv.restart()
        self.attach()

    def state_json(self):
        try:
            with open(f"{self.srv.rundir}/data/remux/state.json") as f:
                return json.load(f)
        except (OSError, ValueError):
            return None

    def argv_files(self, agent):
        return sorted(n for n in os.listdir(self.argv_dir) if n.startswith(agent + "."))

    def spawn(self, argv=None):
        c = Client(self.srv.sock)
        c.hello()
        body = {"session": "main", "placement": "SplitRight"}
        if argv is not None:
            body["argv"] = argv
        c.send({"CliSpawn": body})
        try:
            for m in c.drain(2.0):
                if name_of(m) == "CliSpawned":
                    return m["CliSpawned"]["pane_id"]
                if name_of(m) == "Error":
                    return None
        finally:
            c.close()
        return None

    def ask(self, msg):
        """One request on a fresh connection; the first answer that is not a
        push. A closed connection is an answer too: an old server drops a
        message it cannot decode."""
        c = Client(self.srv.sock)
        c.hello()
        c.send(msg)
        end = time.time() + 3.0
        try:
            while time.time() < end:
                try:
                    m = c.recv()
                except socket.timeout:
                    continue
                if name_of(m) not in ("ViewList", "Event"):
                    return m
        except ConnectionError as e:
            return {"Closed": str(e)}
        finally:
            c.close()
        return None


def report(rig, pane, agent, reference, source=None):
    body = {"agent": agent, "reference": reference}
    if pane is not None:
        body["pane"] = pane
    if source is not None:
        body["source"] = source
    return rig.ask({"AgentSession": body})


def outcome(reply):
    if name_of(reply) == "AgentSessionStored":
        return reply["AgentSessionStored"]["outcome"]
    return None


def code(reply):
    if name_of(reply) == "Error":
        return reply["Error"].get("code")
    return None


def read(path):
    return open(path).read() if os.path.exists(path) else ""


def connections(rig):
    """How many connections the server has accepted, by the identity line it
    writes for each one at accept."""
    return len(set(re.findall(r"identity: client_id=(\d+) peer=", rig.srv.log())))


def cli(rig, args, env_extra=None, drop=(), stdin=None):
    env = {k: v for k, v in rig.srv.env.items() if k not in drop}
    env.update(env_extra or {})
    return subprocess.run([BIN, "agent", "session", *args], env=env, input=stdin,
                          capture_output=True, timeout=20)


def wait_log(rig, pattern, timeout=6.0):
    end = time.time() + timeout
    while time.time() < end:
        m = re.search(pattern, rig.srv.log())
        if m:
            return m
        time.sleep(0.1)
    return None


def cli_attributed(rig):
    before = rig.srv.log().count("AgentSession caller -> pane")
    pane = rig.spawn([BIN, "agent", "session", "claude", "--id", CLI_ID, "--source", "startup"])
    check(pane is not None, f"cli_attributed: the pane was spawned (got {pane})")
    found = wait_log(rig, rf"agent session: pane %{pane} \"claude\" id 36 bytes "
                          rf"source=\"startup\" -> Stored")
    check(found is not None, f"cli_attributed: the report from inside %{pane} was Stored")
    callers = re.findall(r"AgentSession caller -> pane %(\d+) by (\w+)", rig.srv.log())[before:]
    check(callers == [(str(pane), "Sid")] or callers == [(str(pane), "Walk")],
          f"cli_attributed: the server attributed the caller to %{pane} (got {callers})")


def cli_stdout(rig):
    pane = rig.spawn()
    first = cli(rig, ["claude", "--id", CLI_ID, "--pane", f"%{pane}"], drop=("REMUX_PANE",))
    again = cli(rig, ["claude", "--id", CLI_ID, "--pane", f"%{pane}"], drop=("REMUX_PANE",))
    held = cli(rig, ["claude", "--id", OTHER, "--pane", f"%{pane}"], drop=("REMUX_PANE",))
    for label, run in (("Stored", first), ("Unchanged", again), ("session_held", held)):
        check(run.returncode == 0, f"cli_stdout: {label} exits 0 (got {run.returncode})")
        check(run.stdout == b"", f"cli_stdout: {label} prints nothing on stdout (got {run.stdout!r})")
    check(first.stderr == b"" and again.stderr == b"",
          f"cli_stdout: a success writes no stderr (got {first.stderr!r}, {again.stderr!r})")
    lines = held.stderr.decode(errors="replace").splitlines()
    check(len(lines) == 1 and "session_held" in lines[0],
          f"cli_stdout: a refusal writes one stderr line naming it (got {lines})")


def cli_outside(rig):
    before = connections(rig)
    run = cli(rig, ["claude", "--id", CLI_ID], drop=("REMUX_PANE",))
    time.sleep(0.3)
    check(run.returncode == 0 and run.stdout == b"" and run.stderr == b"",
          f"cli_outside: exit 0 and no output (got {run.returncode}, {run.stdout!r}, {run.stderr!r})")
    check(connections(rig) == before, "cli_outside: no connection reached the server")


def cli_filters(rig):
    pane = rig.spawn()
    in_pane = {"REMUX_PANE": f"%{pane}"}
    for label, fixture, extra in (
            ("a subagent payload", "claude_subagent.json", {}),
            ("a UserPromptSubmit payload", "claude_prompt_submit.json", {}),
            ("CURSOR_VERSION set", "claude_startup.json", {"CURSOR_VERSION": "1.2.3"})):
        before = connections(rig)
        payload = open(os.path.join(FIXTURES, fixture), "rb").read()
        run = cli(rig, ["claude", "--hook"], env_extra={**in_pane, **extra},
                  drop=("CURSOR_VERSION",), stdin=payload)
        time.sleep(0.3)
        check(run.returncode == 0 and run.stdout == b"" and run.stderr == b"",
              f"cli_filters: {label} exits 0 silently "
              f"(got {run.returncode}, {run.stdout!r}, {run.stderr!r})")
        check(connections(rig) == before, f"cli_filters: {label} never connects")
    before = connections(rig)
    payload = open(os.path.join(FIXTURES, "claude_startup.json"), "rb").read()
    run = cli(rig, ["claude", "--hook"], env_extra=in_pane, drop=("CURSOR_VERSION",), stdin=payload)
    time.sleep(0.3)
    check(connections(rig) == before + 1,
          "cli_filters: the plain SessionStart payload does connect, so the filters are what "
          "stopped the others")
    check(run.returncode == 0 and run.stdout == b"" and b"not_in_pane" in run.stderr,
          f"cli_filters: from outside every pane it is refused not_in_pane on stderr "
          f"(got {run.returncode}, {run.stdout!r}, {run.stderr!r})")


def persist(rig):
    pane = rig.spawn()
    before = rig.state_json() or {}
    check(str(pane) not in before.get("agent_sessions", {}),
          "persist: the new pane holds no record before the report")
    started = time.time()
    r = report(rig, pane, "claude", {"Id": RESTORE_ID}, source="startup")
    check(outcome(r) == "Stored", f"persist: the report is Stored without hanging (got {r} "
                                  f"after {time.time() - started:.1f}s)")
    held = (rig.state_json() or {}).get("agent_sessions", {}).get(str(pane))
    check(held == {"agent": "claude", "kind": "Id", "value": RESTORE_ID},
          f"persist: state.json holds the record at once (got {held})")
    return pane


def restart_types(rig, pane):
    check(rig.argv_files("claude") == [], "restart_types: the stub has not run before the restart")
    rig.restart()

    def runs():
        """The stub's argv, one list per run that named this case's id. The
        earlier cases' panes hold records too, and are resumed alongside."""
        out = []
        for name in rig.argv_files("claude"):
            with open(f"{rig.argv_dir}/{name}") as f:
                argv = f.read().split("\n")
            if RESTORE_ID in argv:
                out.append(argv)
        return out

    end = time.time() + 8.0
    while time.time() < end and not runs():
        time.sleep(0.1)
    time.sleep(0.5)
    got = runs()
    check(len(got) == 1, f"restart_types: the restore ran the stub claude once for the id (got {got})")
    check(bool(got) and got[0] == ["--resume", RESTORE_ID, ""],
          f"restart_types: --resume and the id are two argv words and nothing else (got {got})")
    r = report(rig, pane, "claude", {"Id": RESTORE_ID}, source="resume")
    check(outcome(r) == "Unchanged",
          f"restart_types: the restored pane %{pane} holds the record again (got {r})")


def main():
    rig = Rig(RUNDIR, CONFIG)
    try:
        pane = rig.spawn()
        check(pane is not None, f"a pane to report into (got {pane})")

        r = report(rig, pane, "claude", {"Id": UUID}, source="startup")
        check(outcome(r) == "Stored", f"accept: a claude UUID is Stored (got {r})")
        check(name_of(r) == "AgentSessionStored" and r["AgentSessionStored"]["pane"] == pane,
              f"accept: the reply names the pane (got {r})")

        r = report(rig, pane, "claude", {"Id": UUID}, source="startup")
        check(outcome(r) == "Unchanged", f"unchanged: the same report is Unchanged (got {r})")

        r = report(rig, pane, "claude", {"Id": OTHER}, source="startup")
        check(code(r) == "session_held", f"held_other_id: a different id is session_held (got {r})")

        fresh = rig.spawn()
        r = report(rig, fresh, "vim", {"Id": UUID})
        check(code(r) == "bad_agent", f"bad_agent: an unknown agent is refused (got {r})")

        for label, ref in (("129-byte id", {"Id": "a" * 129}),
                           ("a path for claude", {"Path": "/home/u/.claude/x.jsonl"}),
                           ("a control char", {"Id": "abc\x07def"})):
            r = report(rig, fresh, "claude", ref)
            check(code(r) == "bad_reference", f"bad_reference: {label} is refused (got {r})")
        r = report(rig, fresh, "claude", {"Id": UUID})
        check(outcome(r) == "Stored",
              f"bad_reference: the refusals stored nothing, so a good report is Stored (got {r})")

        r = report(rig, None, "claude", {"Id": UUID})
        check(code(r) == "not_in_pane", f"not_in_pane: an outside caller naming no pane (got {r})")

        cli_attributed(rig)
        cli_stdout(rig)
        cli_outside(rig)
        cli_filters(rig)
        restored = persist(rig)
        restart_types(rig, restored)

        log = rig.srv.log()
        client_log = read(f"{rig.srv.rundir}/state/remux/client.log")
        check("AgentSession(agent=\"claude\", id, 36 bytes" in log,
              "redaction: server.log has the summary line with the byte count")
        check("AgentSession(agent=\"claude\", id, 36 bytes" in client_log,
              "redaction: client.log has the summary line with the byte count")
        for name, text in (("server.log", log), ("client.log", client_log)):
            check(all(v not in text for v in (UUID, OTHER, CLI_ID)),
                  f"redaction: {name} never holds an id")
        check("panicked at" not in log, "no_panic: server.log has no panic")
        check("panicked at" not in client_log, "no_panic: client.log has no panic")
    finally:
        rig.cli.close()
        rig.srv.kill()

    if failures:
        print(f"\n{len(failures)} FAILED")
        sys.exit(1)
    print("\nALL PASS")


if __name__ == "__main__":
    main()
