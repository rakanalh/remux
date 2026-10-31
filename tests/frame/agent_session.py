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
redaction       server.log carries the summary line with the id's byte count
                and never the id.
no_panic        server.log has no `panicked at`.

Run: python3 tests/frame/agent_session.py   (RMX_RUNDIR overrides the run dir)
"""
import os
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-agentsess")
UUID = "7f3c2a9e-51d4-4b8e-9a61-0c2f5d8e4b17"
OTHER = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


class Rig:
    def __init__(self, rundir, config=None):
        self.srv = Server(rundir)
        self.srv.start(config=config)
        self.cli = Client(self.srv.sock)
        self.cli.hello()
        self.cli.send({"CreateSession": {"name": "main", "folder": None}})
        self.cli.send({"Attach": {"session_name": "main"}})
        self.cli.send({"Resize": {"cols": 160, "rows": 50}})
        self.cli.drain(0.8)

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


def main():
    rig = Rig(RUNDIR)
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

        log = rig.srv.log()
        check("AgentSession(agent=\"claude\", id, 36 bytes" in log,
              "redaction: server.log has the summary line with the byte count")
        check(UUID not in log and OTHER not in log, "redaction: server.log never holds an id")
        check("panicked at" not in log, "no_panic: server.log has no panic")
    finally:
        rig.cli.close()
        rig.srv.kill()

    if failures:
        print(f"\n{len(failures)} FAILED")
        sys.exit(1)
    print("\nALL PASS")


if __name__ == "__main__":
    main()
