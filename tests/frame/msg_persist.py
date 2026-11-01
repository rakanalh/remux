#!/usr/bin/env python3
"""Mailboxes and names across a restart: mailbox.json.

A server of its own, because restarting it kills every connection and would
disturb the other messaging cases. Every send runs INSIDE a pane (through
wire_send.py), since the harness's own connection is never a pane.

7a. A message and a name survive `remux restart`: the message keeps its id,
    sender and state, and the name still routes.
7b. A send after the restart gets a higher id than any before it.
7c. mailbox.json exists, is 0600, and holds the body (content at rest is the
    point of persisting); state.json does not, and keeps its shape.
7d. A pane closed before the restart leaves nothing behind; an entry for a
    pane that was not restored (added to the file while stopped) is pruned
    and logged.
7g. After a crash, no pane id this server issued is issued again. A pane
    created by `remux split` is not in state.json until something else saves
    it, so a crash leaves the pane counter behind it. A message such a pane
    sent survives in a restored mailbox, and a new pane given its id would
    receive every reply to it; a pane that never sent anything must not be
    reissued either.
7h. With `automatic_restore = false` (a third server) the sessions come back
    DORMANT: their mail is kept through a save while they are dormant, and is
    readable once the session is resurrected.
7e. With `[messaging] persist = false` (a second server), nothing is written
    and a restart starts empty.
7z. No panic.

Run: python3 tests/frame/msg_persist.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msgpersist")
WIRE_SEND = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wire_send.py")
CONFIG = """
[general]
save_sessions = true
automatic_restore = true
"""

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


class Rig:
    """A server plus the attached client that types into its panes."""

    def __init__(self, rundir, config):
        self.srv = Server(rundir)
        self.srv.start(config=config)
        self.requests = 0
        self.attach(create=True)

    def attach(self, create=False):
        self.cli = Client(self.srv.sock)
        self.cli.hello()
        if create:
            self.cli.send({"CreateSession": {"name": "main", "folder": None}})
        self.cli.send({"Attach": {"session_name": "main"}})
        self.cli.send({"Resize": {"cols": 160, "rows": 50}})
        self.cli.drain(0.8)

    def restart(self, crash=False, between=None):
        self.cli.close()
        if crash:
            self.srv.crash()
        else:
            self.srv.stop()
        if between:
            between()
        self.srv.start_again()
        self.attach()

    def spawn(self, argv=None, placement="SplitRight"):
        c = Client(self.srv.sock)
        c.hello()
        body = {"session": "main", "placement": placement}
        if argv is not None:
            body["argv"] = argv
        c.send({"CliSpawn": body})
        reply = None
        for m in c.drain(2.0):
            if name_of(m) in ("CliSpawned", "Error"):
                reply = m
                break
        c.close()
        return reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None

    def type_send(self, pane_id, msg):
        self.requests += 1
        n = self.requests
        in_file = f"{self.srv.rundir}/in-{n}.json"
        out_file = f"{self.srv.rundir}/out-{n}.json"
        with open(in_file, "w") as f:
            json.dump(msg, f)
        line = f"python3 {WIRE_SEND} {self.srv.sock} {in_file} {out_file}\n"
        self.cli.send({"InputToPane": {"pane_id": pane_id, "data": list(line.encode())}})
        end = time.time() + 8.0
        while time.time() < end:
            if os.path.exists(out_file):
                with open(out_file) as f:
                    return json.loads(f.read())
            time.sleep(0.05)
        return None

    def ask(self, msg):
        c = Client(self.srv.sock)
        c.hello()
        c.send(msg)
        try:
            for m in c.drain(2.0):
                if name_of(m) != "ViewList":
                    return m
        finally:
            c.close()
        return None

    def box(self, pane):
        r = self.ask({"MsgRead": {"pane": pane, "peek": True, "all": True}})
        return r["MsgList"]["messages"] if name_of(r) == "MsgList" else None

    def wait_for(self, predicate, timeout=6.0):
        end = time.time() + timeout
        while time.time() < end:
            if predicate():
                return True
            time.sleep(0.05)
        return predicate()

    def mailbox_json(self):
        path = f"{self.srv.rundir}/data/remux/mailbox.json"
        return open(path).read() if os.path.exists(path) else None


def send_to(target, body):
    return {"MsgSend": {"to": {"host": None, "target": target}, "body": body, "deliver": "Mailbox"}}


def code_of(reply):
    return reply["Error"].get("code") if name_of(reply) == "Error" else None


for var in ("REMUX_SESSION", "REMUX_PANE"):
    os.environ.pop(var, None)

rig = Rig(RUNDIR, CONFIG)
try:
    A = rig.spawn()
    B = rig.spawn()
    time.sleep(0.5)

    # --- 7a / 7b / 7c -----------------------------------------------------
    r = rig.type_send(A, send_to({"Pane": B}, "MSG:keep"))
    k = r["MsgSent"]["id"] if name_of(r) == "MsgSent" else None
    named = rig.type_send(A, {"AgentName": {"name": "alpha"}})
    check(k is not None and name_of(named) == "AgentNamed", f"7a a message and a name before the restart ({r}, {named})")
    rig.restart()
    box = rig.box(B) or []
    kept = [m for m in box if m["body"] == "MSG:keep"]
    check(len(kept) == 1 and kept[0]["id"] == k and kept[0]["from"]["Pane"]["pane_id"] == A
          and kept[0]["state"] == "Queued",
          f"7a the message survives with id {k}, from %{A}, Queued (got {box})")
    r = rig.type_send(B, send_to({"Name": "alpha"}, "MSG:to-alpha"))
    check(name_of(r) == "MsgSent" and r["MsgSent"]["to"]["pane_id"] == A,
          f"7a the name alpha still routes to %{A} (got {r})")
    check(name_of(r) == "MsgSent" and k is not None and r["MsgSent"]["id"] > k,
          f"7b the next id is above {k} (got {r})")

    text = rig.mailbox_json() or ""
    path = f"{rig.srv.rundir}/data/remux/mailbox.json"
    mode = os.stat(path).st_mode & 0o777 if os.path.exists(path) else None
    check(mode == 0o600, f"7c mailbox.json is 0600 (got {oct(mode) if mode else None})")
    check("MSG:keep" in text, "7c mailbox.json holds the body")
    state = json.load(open(f"{rig.srv.rundir}/data/remux/state.json"))
    check(sorted(state) == ["pane_cwds", "state"] and "MSG:keep" not in json.dumps(state),
          f"7c state.json keeps its shape and holds no body (keys {sorted(state)})")

    # --- 7d ---------------------------------------------------------------
    release = f"{rig.srv.rundir}/release-7d"
    D = rig.spawn(["/bin/sh", "-c", f"while [ ! -e {release} ]; do sleep 0.1; done"])
    r = rig.type_send(A, send_to({"Pane": D}, "MSG:to-d"))
    check(name_of(r) == "MsgSent", f"7d a message waits in %{D} (got {r})")
    # On disk first, so that "gone from the file" below means it was removed.
    check(rig.wait_for(lambda: "MSG:to-d" in (rig.mailbox_json() or "")),
          f"7d the message to %{D} is written to mailbox.json")
    open(release, "w").close()
    check(rig.wait_for(lambda: f"msg: pane %{D} closed; dropped 1 message(s)" in rig.srv.log()),
          f"7d %{D} closes and drops its message")
    check(rig.wait_for(lambda: "MSG:to-d" not in (rig.mailbox_json() or "")),
          "7d the saver rewrites mailbox.json without it")

    def add_orphan():
        with open(path) as f:
            doc = json.load(f)
        template = dict(doc["messages"][0])
        template.update(id=999999, to=9999, body="MSG:orphan")
        doc["messages"].append(template)
        with open(path, "w") as f:
            json.dump(doc, f)

    before = rig.srv.log().count("mailbox: restored")
    rig.restart(between=add_orphan)
    restored = [ln for ln in rig.srv.log().splitlines() if "mailbox: restored" in ln][before:]
    check(len(restored) == 1 and "pruned 1 for closed panes" in restored[0],
          f"7d the entry for %9999, never restored, is pruned and logged (got {restored})")

    # --- 7g (security M1) -------------------------------------------------
    # %N is created by `remux split` (a CliSpawn), which does not save
    # state.json, so after the crash below the pane counter there is behind %N.
    N = rig.spawn()
    time.sleep(0.5)
    counter = json.load(open(f"{rig.srv.rundir}/data/remux/state.json"))["state"]["next_pane_id"]
    check(counter <= N, f"7g setup: state.json's pane counter ({counter}) predates %{N}")
    r = rig.type_send(N, send_to({"Pane": B}, "MSG:from-n"))
    check(name_of(r) == "MsgSent", f"7g %{N} leaves a message for %{B} (got {r})")
    check(rig.wait_for(lambda: "MSG:from-n" in (rig.mailbox_json() or "")),
          "7g the saver writes it to mailbox.json before the crash")
    # A pane that never sends or receives anything: only the pane counter
    # knows it was issued.
    M = rig.spawn()

    def high_water():
        text = rig.mailbox_json()
        return json.loads(text)["pane_high_water"] if text else 0

    check(rig.wait_for(lambda: high_water() >= M),
          f"7g creating %{M} alone moves the file's high water to it (got {high_water()})")
    rig.restart(crash=True)
    box = rig.box(B) or []
    check(any(m["body"] == "MSG:from-n" and m["from"]["Pane"]["pane_id"] == N for m in box),
          f"7g the message from %{N} survives the crash in %{B} (got {box})")
    top = max(N, M)
    new_ids = []
    while not new_ids or max(new_ids) <= top:
        pane = rig.spawn()
        if pane is None or len(new_ids) > top + 5:
            break
        new_ids.append(pane)
    check(N not in new_ids and M not in new_ids and new_ids and max(new_ids) > top,
          f"7g neither %{N} nor %{M} is given to a new pane (new panes {new_ids})")
    status = rig.ask({"AgentStatus": {}})
    listed = {e["pane_id"] for e in status["AgentStatusList"]["panes"]} if name_of(status) == "AgentStatusList" else set()
    check(not (listed & set(new_ids)), f"7g no new pane has mail or a name (listed {sorted(listed)})")

    time.sleep(0.3)
    check("panicked at" not in rig.srv.log(), "7z no panic in the server log")
    check(not any(b in rig.srv.log() for b in ("MSG:keep", "MSG:from-n", "MSG:to-d")),
          "7z no message body in the server log")
finally:
    rig.srv.kill()

# --- 7e: persist = false ---------------------------------------------------
off = Rig(RUNDIR + "-off", CONFIG + "\n[messaging]\npersist = false\n")
try:
    A = off.spawn()
    B = off.spawn()
    time.sleep(0.5)
    r = off.type_send(A, send_to({"Pane": B}, "MSG:not-kept"))
    check(name_of(r) == "MsgSent", f"7e a message is sent (got {r})")
    # An absence can only be shown over a bound: the saver writes
    # MAILBOX_SAVE_DEBOUNCE (1 s, daemon.rs) after a change, so 2.5 s is well
    # past the moment a saver would have written.
    time.sleep(2.5)
    check(off.mailbox_json() is None, "7e persist=false: no mailbox.json is written while running")
    off.restart()
    check(off.box(B) == [], f"7e persist=false: the mailbox is empty after the restart (got {off.box(B)})")
    check(off.mailbox_json() is None, "7e persist=false: none is written at the stop either")

    # A file left by an earlier run with persist on is not read either.
    def plant():
        planted = {"version": 1, "next_id": 50, "pane_high_water": B, "names": [[B, "planted"]],
                   "messages": [{"id": 49, "from": {"Pane": {"pane_id": A, "name": None, "session": "main"}},
                                 "to": B, "body": "MSG:planted", "created_ms": int(time.time() * 1000),
                                 "expires_ms": int(time.time() * 1000) + 3_600_000, "state": "Queued",
                                 "reply_to": None}]}
        os.makedirs(f"{off.srv.rundir}/data/remux", exist_ok=True)
        with open(f"{off.srv.rundir}/data/remux/mailbox.json", "w") as f:
            json.dump(planted, f)

    off.restart(between=plant)
    check(off.box(B) == [], f"7e persist=false: a mailbox.json on disk is not loaded (got {off.box(B)})")
finally:
    off.srv.kill()

# --- 7h: automatic_restore = false --------------------------------------------
dormant = Rig(RUNDIR + "-dormant", CONFIG.replace("automatic_restore = true", "automatic_restore = false"))
try:
    A = dormant.spawn()
    B = dormant.spawn()
    time.sleep(0.5)
    r = dormant.type_send(A, send_to({"Pane": B}, "MSG:dormant"))
    check(name_of(r) == "MsgSent", f"7h a message waits in %{B} (got {r})")
    dormant.cli.close()
    dormant.srv.stop()
    dormant.srv.start_again()
    saves = lambda: dormant.srv.log().count("mailbox: saved")
    before = saves()
    # A pane in another session is created while main is dormant, which
    # makes the saver write.
    other = Client(dormant.srv.sock)
    other.hello()
    other.send({"CreateSession": {"name": "other", "folder": None}})
    other.drain(0.5)
    check(dormant.wait_for(lambda: saves() > before), "7h a save happens while main is dormant")
    check("MSG:dormant" in (dormant.mailbox_json() or ""),
          "7h the dormant session's mail survives that save")
    other.send({"ResurrectSession": {"name": "main"}})
    other.drain(1.0)
    other.close()
    box = dormant.box(B) or []
    check([m["body"] for m in box] == ["MSG:dormant"],
          f"7h once main is resurrected, %{B} has its mail (got {box})")
finally:
    dormant.srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED")
    sys.exit(1)
print("ALL PASS")
