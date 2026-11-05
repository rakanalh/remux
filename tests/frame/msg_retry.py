#!/usr/bin/env python3
"""An `auto` message that could not be typed is typed later, once its agent
can take it, exactly once, and only while it is still unread.

The target is the fakeagent (`fakeagent.py`), which logs every read it makes,
so what was typed is read from its log, never from the screen. The sender is
a shell pane running `wire_send.py`, so every send is attributed to a pane.

R0. The fakeagent pane is detected as an agent and is the focused pane.
R1. Blocked: an `auto` send to an agent showing the permission prompt is
    answered at once `stored in its mailbox (agent_blocked)`, nothing is
    typed while the prompt is up, and once the prompt is gone the message is
    typed, framed, exactly once. The mailbox then shows it Submitted, and
    the log says `retry pending (agent_blocked)` once.
R2. Mailbox and order: while blocked, a `--deliver mailbox` send and two
    `auto` sends. Once unblocked both `auto` messages are typed, in the order
    they were sent, each once, and the mailbox message is never typed.
R3. Read first: an `auto` message refused `agent_blocked`, then acked by the
    pane, is not typed once the prompt goes, and the log says the retry
    ended.
R4. Working: an `auto` send to an agent that is working, under a row that
    leaves a working agent's mail in the mailbox, is answered `agent_working`
    and typed once the work ends.
R5. Human typing: an `auto` send while a person keeps typing past the guard's
    cap is answered `human_typing` and typed once the person stops.
R6. `--deliver submit` is never retried: refused `agent_blocked`, it is not
    typed once the prompt goes.
R7. A send behind a held retry is answered at once, as it would be at the
    head: with a retry held on `agent_working`, a second `auto` send is
    answered `agent_working` without waiting for the server's hold, and the
    two are typed in order once the work ends.
R8. The same behind a retry held on a person typing: a second `auto` send
    is answered `human_typing` when its own wait runs out, not at the
    server's longer hold, and the two are typed in order once the person
    stops.
R9. No panic, and no message body in the log.

Run: python3 tests/frame/msg_retry.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import re
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402
import fakeagent  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msgretry")
BINDIR = RUNDIR + "-bin"
FAKEAGENT = f"{BINDIR}/fakeagent"
WIRE_SEND = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wire_send.py")

CONFIG = """
[agents]
commands = ["fakeagent"]
working_ms = 500
  [[agents.pattern]]
  name = "fake-proceed"
  command = "fakeagent"
  regex = "Do you want to proceed"

[messaging]
typing_guard_ms = 1000
typing_guard_max_ms = 2000
"""

OPEN, CLOSE = b"\x1b[200~", b"\x1b[201~"
PROBES = (OPEN, b"\x1b[I" + OPEN)

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


for var in ("REMUX_SESSION", "REMUX_PANE"):
    os.environ.pop(var, None)


def cli_spawn(placement, argv=None):
    caller = Client(srv.sock)
    caller.hello()
    body = {"session": "main", "placement": placement}
    if argv is not None:
        body["argv"] = argv
    caller.send({"CliSpawn": body})
    reply = None
    for msg in caller.drain(3.0):
        if name_of(msg) in ("CliSpawned", "Error"):
            reply = msg
            break
    caller.close()
    return reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None


def wait_file(path, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        if os.path.exists(path):
            with open(path) as f:
                return json.loads(f.read())
        time.sleep(0.05)
    return None


requests = [0]


def type_send(pane_id, msg):
    """Run one wire request from inside `pane_id`'s shell: (reply, seconds)."""
    requests[0] += 1
    n = requests[0]
    in_file, out_file = f"{RUNDIR}/in-{n}.json", f"{RUNDIR}/out-{n}.json"
    with open(in_file, "w") as f:
        json.dump(msg, f)
    line = f"python3 {WIRE_SEND} {srv.sock} {in_file} {out_file}\n"
    start = time.time()
    cli.send({"InputToPane": {"pane_id": pane_id, "data": list(line.encode())}})
    reply = wait_file(out_file)
    return reply, time.time() - start


def ask(msg, timeout=3.0):
    c = Client(srv.sock)
    c.hello()
    c.send(msg)
    end = time.time() + timeout
    try:
        while time.time() < end:
            for m in c.drain(0.2):
                if name_of(m) != "ViewList":
                    return m
    finally:
        c.close()
    return None


def box_states(pane):
    reply = ask({"MsgRead": {"pane": pane, "peek": True, "all": True}})
    msgs = reply["MsgList"]["messages"] if name_of(reply) == "MsgList" else []
    return {m["id"]: m["state"] for m in msgs}


def status_of(pane):
    reply = ask({"AgentStatus": {"target": {"host": None, "target": {"Pane": pane}}}})
    return reply["AgentStatusList"]["panes"] if name_of(reply) == "AgentStatusList" else reply


def send(to, body, deliver):
    return {"MsgSend": {"to": {"host": None, "target": {"Pane": to}}, "body": body,
                        "deliver": deliver}}


def wait_state(pane, want, timeout=6.0):
    end = time.time() + timeout
    while time.time() < end:
        entries = status_of(pane)
        if isinstance(entries, list) and entries and entries[0]["state"] == want:
            return True
        time.sleep(0.1)
    return False


def raw_chunks(log):
    try:
        lines = open(log).read().splitlines()
    except FileNotFoundError:
        return []
    return [(int(ln.split()[1]), bytes.fromhex(ln.split()[2] if len(ln.split()) > 2 else ""))
            for ln in lines if ln.startswith("RX ")]


def rx_chunks(log):
    """The reads, each typed message joined from its probe to its close."""
    out, pending = [], None
    for ns, data in raw_chunks(log):
        if pending is not None:
            pending += data
            if CLOSE in data:
                out.append((ns, pending))
                pending = None
        elif data in PROBES:
            pending = data
        else:
            out.append((ns, data))
    return out


def typed(body):
    """A typed message carrying `body` as its whole second line."""
    return re.compile(re.escape(OPEN) + rb"message #\d+, reply with [^\n]*\n"
                      + re.escape(body.encode()) + re.escape(CLOSE), re.S)


def count_typed(log, body, start=0):
    pattern = typed(body)
    return sum(1 for _, c in rx_chunks(log)[start:] if pattern.fullmatch(c))


def first_typed_at(log, body, start=0):
    pattern = typed(body)
    for i, (_, c) in enumerate(rx_chunks(log)[start:]):
        if pattern.fullmatch(c):
            return start + i
    return None


def wait_typed(log, body, start=0, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        at = first_typed_at(log, body, start)
        if at is not None:
            return at
        time.sleep(0.1)
    return None


def outcome(reply):
    if name_of(reply) == "Error":
        return ("Error", reply["Error"].get("code"))
    sent = (reply or {}).get("MsgSent") or {}
    return (sent.get("delivery"), sent.get("reason"))


def sent_id(reply):
    return ((reply or {}).get("MsgSent") or {}).get("id")


def tell(command):
    fakeagent.tell(F_CTL, F_LOG, command)


def typer(stop, every=0.2):
    """A person typing "k" into main's focused pane until `stop` is set."""
    c = Client(srv.sock)
    c.hello()
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": 160, "rows": 50}})
    c.drain(0.3)
    while not stop.is_set():
        c.send({"Input": {"data": [0x6b]}})
        c.drain(every)
    c.close()


def focused_pane():
    reply = ask({"ListSessionTree": None})
    found = []

    def walk(node):
        if isinstance(node, dict):
            if node.get("is_active") and isinstance(node.get("panes"), list):
                found.extend(p["id"] for p in node["panes"] if p.get("is_focused"))
            for value in node.values():
                walk(value)
        elif isinstance(node, list):
            for value in node:
                walk(value)
    walk(reply)
    return found[0] if found else None


os.makedirs(BINDIR, exist_ok=True)
fakeagent.install(FAKEAGENT)

srv = Server(RUNDIR)
try:
    srv.start(config=CONFIG)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 160, "rows": 50}})
    cli.drain(0.8)
    A = cli_spawn("SplitRight")
    F_LOG, F_CTL = f"{RUNDIR}/agent.log", f"{RUNDIR}/agent.ctl"
    F = cli_spawn("SplitBelow", argv=[FAKEAGENT, F_LOG, F_CTL])
    end = time.time() + 8
    while time.time() < end and not os.path.exists(F_LOG):
        time.sleep(0.05)
    tell("paste-on")
    time.sleep(0.8)

    # --- R0 -----------------------------------------------------------------
    entries = status_of(F)
    check(isinstance(entries, list) and len(entries) == 1
          and entries[0]["command"] == "fakeagent",
          f"R0 the fakeagent pane %{F} is detected as an agent (got {entries})")
    check(focused_pane() == F, f"R0 ...and is main's focused pane (got {focused_pane()})")
    check(wait_state(F, "Idle"), "R0 ...and settles Idle")

    # --- R1 -----------------------------------------------------------------
    tell("block")
    check(wait_state(F, "NeedsInput"), "R1 the prompt reads NeedsInput")
    start = len(rx_chunks(F_LOG))
    r, took = type_send(A, send(F, "RETRY:one", "Auto"))
    rid = sent_id(r)
    check(outcome(r) == ("Mailbox", "agent_blocked") and took < 4.0,
          f"R1 auto to a blocked agent is answered agent_blocked at once ({took:.1f}s, {r})")
    time.sleep(2.0)
    check(count_typed(F_LOG, "RETRY:one", start) == 0,
          f"R1 ...nothing is typed while the prompt is up ({rx_chunks(F_LOG)[start:]})")
    tell("clear")
    at = wait_typed(F_LOG, "RETRY:one", start)
    check(at is not None, f"R1 once the prompt is gone the message is typed ({rx_chunks(F_LOG)[start:]})")
    time.sleep(3.0)
    check(count_typed(F_LOG, "RETRY:one", start) == 1,
          f"R1 ...exactly once (got {count_typed(F_LOG, 'RETRY:one', start)})")
    check(box_states(F).get(rid) == "Submitted",
          f"R1 ...and the mailbox shows it Submitted ({box_states(F)})")
    log = srv.log()
    check(log.count(f"msg: id={rid} retry pending (agent_blocked)") == 1,
          f"R1 the retry is logged pending once ({log.count(f'msg: id={rid} retry pending')})")
    check(f"msg: id={rid} submitted to %{F}" in log, "R1 ...and then submitted")

    # --- R2 -----------------------------------------------------------------
    check(wait_state(F, "Idle"), "R2 the agent is Idle again")
    tell("block")
    check(wait_state(F, "NeedsInput"), "R2 the prompt reads NeedsInput")
    start = len(rx_chunks(F_LOG))
    r_mb, _ = type_send(A, send(F, "MBOX:two", "Mailbox"))
    r_a, _ = type_send(A, send(F, "ORDER:first", "Auto"))
    r_b, _ = type_send(A, send(F, "ORDER:second", "Auto"))
    check(outcome(r_mb) == ("Mailbox", None) and outcome(r_a) == ("Mailbox", "agent_blocked")
          and outcome(r_b) == ("Mailbox", "agent_blocked"),
          f"R2 the mailbox send is stored, both auto sends agent_blocked ({r_mb}, {r_a}, {r_b})")
    tell("clear")
    a_at = wait_typed(F_LOG, "ORDER:first", start)
    b_at = wait_typed(F_LOG, "ORDER:second", start, timeout=10.0)
    check(a_at is not None and b_at is not None and a_at < b_at,
          f"R2 both auto messages are typed, in the order they were sent ({a_at}, {b_at})")
    time.sleep(3.0)
    check(count_typed(F_LOG, "ORDER:first", start) == 1
          and count_typed(F_LOG, "ORDER:second", start) == 1,
          "R2 ...each exactly once")
    check(not any(b"MBOX:two" in c for _, c in rx_chunks(F_LOG)),
          "R2 the --deliver mailbox message is never typed, though the pane took the others")
    check(box_states(F).get(sent_id(r_mb)) == "Queued",
          f"R2 ...and stays unread in the mailbox ({box_states(F)})")
    ask({"MsgAck": {"pane": F, "all": True}})

    # --- R3 -----------------------------------------------------------------
    check(wait_state(F, "Idle"), "R3 the agent is Idle again")
    tell("block")
    check(wait_state(F, "NeedsInput"), "R3 the prompt reads NeedsInput")
    start = len(rx_chunks(F_LOG))
    r, _ = type_send(A, send(F, "READ:three", "Auto"))
    rid = sent_id(r)
    check(outcome(r) == ("Mailbox", "agent_blocked"), f"R3 stored agent_blocked ({r})")
    acked = ask({"MsgAck": {"pane": F, "all": True}})
    check(box_states(F).get(rid) == "Read", f"R3 ...then acked ({acked}, {box_states(F)})")
    tell("clear")
    check(wait_state(F, "Idle"), "R3 the agent is Idle once the prompt is gone")
    time.sleep(3.0)
    check(count_typed(F_LOG, "READ:three", start) == 0,
          f"R3 a message read before the agent could take it is never typed ({rx_chunks(F_LOG)[start:]})")
    check(f"msg: id={rid} retry ended (no longer unread)" in srv.log(),
          "R3 ...and its retry is logged as ended")

    # --- R4 -----------------------------------------------------------------
    check(wait_state(F, "Idle"), "R4 the agent is Idle again")
    tell("work:3000")
    check(wait_state(F, "Working"), "R4 the agent reads Working")
    start = len(rx_chunks(F_LOG))
    r, _ = type_send(A, send(F, "WORK:four", "Auto"))
    check(outcome(r) == ("Mailbox", "agent_working"), f"R4 stored agent_working ({r})")
    check(count_typed(F_LOG, "WORK:four", start) == 0, "R4 ...nothing typed while it works")
    at = wait_typed(F_LOG, "WORK:four", start, timeout=10.0)
    check(at is not None, f"R4 once the work ends the message is typed ({rx_chunks(F_LOG)[start:]})")
    time.sleep(2.0)
    check(count_typed(F_LOG, "WORK:four", start) == 1, "R4 ...exactly once")

    # --- R5 -----------------------------------------------------------------
    check(wait_state(F, "Idle"), "R5 the agent is Idle again")
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop,))
    t.start()
    time.sleep(0.6)
    start = len(rx_chunks(F_LOG))
    r, took = type_send(A, send(F, "HUMAN:five", "Auto"))
    rid = sent_id(r)
    still_typing = t.is_alive()
    time.sleep(1.0)
    typed_while = count_typed(F_LOG, "HUMAN:five", start)
    stop.set()
    t.join()
    check(outcome(r) == ("Mailbox", "human_typing") and still_typing,
          f"R5 auto while a person keeps typing is answered human_typing ({took:.1f}s, {r})")
    check(typed_while == 0, "R5 ...nothing typed while the person types")
    at = wait_typed(F_LOG, "HUMAN:five", start, timeout=10.0)
    check(at is not None, f"R5 once the person stops the message is typed ({rx_chunks(F_LOG)[start:]})")
    time.sleep(2.0)
    check(count_typed(F_LOG, "HUMAN:five", start) == 1, "R5 ...exactly once")

    # --- R6 -----------------------------------------------------------------
    check(wait_state(F, "Idle"), "R6 the agent is Idle again")
    tell("block")
    check(wait_state(F, "NeedsInput"), "R6 the prompt reads NeedsInput")
    start = len(rx_chunks(F_LOG))
    r, _ = type_send(A, send(F, "SUBMIT:six", "Submit"))
    check(outcome(r) == ("Error", "agent_blocked"), f"R6 submit to a blocked agent is refused ({r})")
    tell("clear")
    check(wait_state(F, "Idle"), "R6 the agent is Idle once the prompt is gone")
    time.sleep(3.0)
    check(count_typed(F_LOG, "SUBMIT:six", start) == 0, "R6 ...and a refused submit is never typed")

    # --- R7 -----------------------------------------------------------------
    check(wait_state(F, "Idle"), "R7 the agent is Idle again")
    tell("work:8000")
    check(wait_state(F, "Working"), "R7 the agent reads Working")
    start = len(rx_chunks(F_LOG))
    r1, _ = type_send(A, send(F, "BEHIND:first", "Auto"))
    time.sleep(2.5)
    held = f"msg: id={sent_id(r1)} retry pending (agent_working)" in srv.log()
    r2, took = type_send(A, send(F, "BEHIND:second", "Auto"))
    check(held and outcome(r1) == ("Mailbox", "agent_working"),
          f"R7 the first is held as a retry on agent_working ({held}, {r1})")
    check(outcome(r2) == ("Mailbox", "agent_working") and took < 3.0,
          f"R7 a send behind it is answered agent_working at once ({took:.1f}s, {r2})")
    a_at = wait_typed(F_LOG, "BEHIND:first", start, timeout=12.0)
    b_at = wait_typed(F_LOG, "BEHIND:second", start, timeout=12.0)
    check(a_at is not None and b_at is not None and a_at < b_at,
          f"R7 ...and both are typed in order once the work ends ({a_at}, {b_at})")

    # --- R8 -----------------------------------------------------------------
    check(wait_state(F, "Idle"), "R8 the agent is Idle again")
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop,))
    t.start()
    time.sleep(0.6)
    start = len(rx_chunks(F_LOG))
    r1, _ = type_send(A, send(F, "GUARD:first", "Auto"))
    time.sleep(1.5)
    r2, took = type_send(A, send(F, "GUARD:second", "Auto"))
    still_typing = t.is_alive()
    time.sleep(1.0)
    stop.set()
    t.join()
    check(outcome(r1) == ("Mailbox", "human_typing"), f"R8 the first is stored human_typing ({r1})")
    check(outcome(r2) == ("Mailbox", "human_typing") and took < 5.0 and still_typing,
          f"R8 a send behind its retry is answered human_typing at its own cap ({took:.1f}s, {r2})")
    a_at = wait_typed(F_LOG, "GUARD:first", start, timeout=12.0)
    b_at = wait_typed(F_LOG, "GUARD:second", start, timeout=12.0)
    check(a_at is not None and b_at is not None and a_at < b_at,
          f"R8 ...and both are typed in order once the person stops ({a_at}, {b_at})")

    # --- R9 -----------------------------------------------------------------
    log = srv.log()
    check("panicked at" not in log, "R9 no panic in the log")
    leaked = [b for b in ("RETRY:one", "ORDER:first", "MBOX:two", "HUMAN:five") if b in log]
    check(not leaked, f"R9 no message body in the log ({leaked})")
finally:
    srv.kill()

print()
print(f"{len(failures)} failure(s)" if failures else "all passed")
sys.exit(1 if failures else 0)
