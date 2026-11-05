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
R5. Human typing is never retried: an `auto` send while a person keeps
    typing past the guard's cap is answered `human_typing`, and it is NOT
    typed once the person stops, since it would be submitted with their
    draft. It stays unread in the mailbox.
R6. `--deliver submit` is never retried: refused `agent_blocked`, it is not
    typed once the prompt goes.
R7. A send behind a held retry is answered at once, as it would be at the
    head: with a retry held on `agent_working`, a second `auto` send is
    answered `agent_working` without waiting for the server's hold, and the
    two are typed in order once the work ends.
R8. A retry held on a working agent that then meets a person typing ends:
    it is not typed once the person stops, and the log says why.
R8b. A retry held on a prompt ends when a person's keys go on after the
    prompt has gone: they are a draft. Nothing is typed and it stays unread.
    A fresh `auto` send behind it is answered `human_typing` at its own cap,
    and is not retried.
R8c. Keys on a second prompt are its answer, not a draft: a retry anchored
    after one prompt loses the anchor when another comes up, and is typed
    once that one has gone.
R9. No panic, and no message body in the log.
R10. A retry held on a prompt takes the keys before the prompt went as its
    answer: they do not end it, and it is typed only once the guard's window
    after the last of them has run out. Its own server, with a window several
    retry ticks long, so that a retry that ignores the guard cannot pass by
    landing late in a short one.
R11. A retry that meets a working agent while a person's last key is inside
    `working_ms` but past the guard's window is put back as it was, and the
    submitter waits for its tick: the server stays near idle. Its own server,
    with `working_ms` above the window.

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

CONFIG_LONG_GUARD = """
[agents]
commands = ["fakeagent"]
working_ms = 500
  [[agents.pattern]]
  name = "fake-proceed"
  command = "fakeagent"
  regex = "Do you want to proceed"

[messaging]
typing_guard_ms = 3000
typing_guard_max_ms = 6000
"""

CONFIG_LONG_WORKING = """
[agents]
commands = ["fakeagent"]
working_ms = 3000
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


def typer(stop, every=0.2, keys=None):
    """A person typing "k" into main's focused pane until `stop` is set. The
    time of each key is appended to `keys`."""
    c = Client(srv.sock)
    c.hello()
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": 160, "rows": 50}})
    c.drain(0.3)
    while not stop.is_set():
        c.send({"Input": {"data": [0x6b]}})
        if keys is not None:
            keys.append(time.time_ns())
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


def boot(rundir, config):
    """A server with a shell pane and a focused fakeagent pane, attached."""
    global srv, cli, A, F, F_LOG, F_CTL
    srv = Server(rundir)
    srv.start(config=config)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 160, "rows": 50}})
    cli.drain(0.8)
    A = cli_spawn("SplitRight")
    F_LOG, F_CTL = f"{rundir}/agent.log", f"{rundir}/agent.ctl"
    F = cli_spawn("SplitBelow", argv=[FAKEAGENT, F_LOG, F_CTL])
    end = time.time() + 8
    while time.time() < end and not os.path.exists(F_LOG):
        time.sleep(0.05)
    tell("paste-on")
    time.sleep(0.8)


def keys_on_prompt(seconds):
    """A person typing on the prompt for `seconds`: the keys' times."""
    keys, stop = [], threading.Event()
    t = threading.Thread(target=typer, args=(stop,), kwargs={"keys": keys})
    t.start()
    time.sleep(seconds)
    stop.set()
    t.join()
    return keys


def cpu_seconds(pid):
    """Seconds of CPU `pid` has used, or None where /proc has no such file."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            fields = f.read().rsplit(")", 1)[1].split()
    except OSError:
        return None
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


os.makedirs(BINDIR, exist_ok=True)
fakeagent.install(FAKEAGENT)

srv = None
try:
    boot(RUNDIR, CONFIG)

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
    work_end_ns = time.time_ns() + 3_000_000_000
    tell("work:3000")
    check(wait_state(F, "Working"), "R4 the agent reads Working")
    start = len(rx_chunks(F_LOG))
    r, _ = type_send(A, send(F, "WORK:four", "Auto"))
    check(outcome(r) == ("Mailbox", "agent_working"), f"R4 stored agent_working ({r})")
    check(count_typed(F_LOG, "WORK:four", start) == 0, "R4 ...nothing typed while it works")
    at = wait_typed(F_LOG, "WORK:four", start, timeout=10.0)
    check(at is not None and rx_chunks(F_LOG)[at][0] > work_end_ns,
          f"R4 once the work ends the message is typed, after it ends ({rx_chunks(F_LOG)[start:]})")
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
    time.sleep(4.0)
    check(count_typed(F_LOG, "HUMAN:five", start) == 0,
          f"R5 ...and nothing typed once the person stops ({rx_chunks(F_LOG)[start:]})")
    check(box_states(F).get(rid) == "Queued" and f"msg: id={rid} retry" not in srv.log(),
          f"R5 ...it stays unread in the mailbox, with no retry ({box_states(F)})")

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
    tell("work:4000")
    check(wait_state(F, "Working"), "R8 the agent reads Working")
    start = len(rx_chunks(F_LOG))
    r, _ = type_send(A, send(F, "DRAFT:eight", "Auto"))
    rid = sent_id(r)
    check(outcome(r) == ("Mailbox", "agent_working"), f"R8 stored agent_working ({r})")
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop,))
    t.start()
    time.sleep(6.0)
    stop.set()
    t.join()
    time.sleep(3.0)
    check(count_typed(F_LOG, "DRAFT:eight", start) == 0,
          f"R8 a working retry that meets a person typing is never typed ({rx_chunks(F_LOG)[start:]})")
    check(f"msg: id={rid} retry ended (human_typing)" in srv.log() and box_states(F).get(rid) == "Queued",
          f"R8 ...its retry ends human_typing and it stays unread ({box_states(F)})")

    # --- R8b ----------------------------------------------------------------
    check(wait_state(F, "Idle"), "R8b the agent is Idle again")
    tell("block")
    check(wait_state(F, "NeedsInput"), "R8b the prompt reads NeedsInput")
    start = len(rx_chunks(F_LOG))
    r1, _ = type_send(A, send(F, "ANSWER:first", "Auto"))
    rid = sent_id(r1)
    check(outcome(r1) == ("Mailbox", "agent_blocked"), f"R8b the first is stored agent_blocked ({r1})")
    time.sleep(1.5)
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop,))
    t.start()
    time.sleep(0.6)
    tell("clear")
    r2, took = type_send(A, send(F, "ANSWER:second", "Auto"))
    still_typing = t.is_alive()
    time.sleep(1.0)
    stop.set()
    t.join()
    check(outcome(r2) == ("Mailbox", "human_typing") and took < 5.0 and still_typing,
          f"R8b a send behind the retry is answered human_typing at its own cap ({took:.1f}s, {r2})")
    time.sleep(4.0)
    check(count_typed(F_LOG, "ANSWER:first", start) == 0,
          f"R8b keys that go on after the prompt has gone are a draft: nothing is typed ({rx_chunks(F_LOG)[start:]})")
    check(f"msg: id={rid} retry ended (human_typing)" in srv.log() and box_states(F).get(rid) == "Queued",
          f"R8b ...its retry ends human_typing and it stays unread ({box_states(F)})")
    check(count_typed(F_LOG, "ANSWER:second", start) == 0,
          "R8b ...and the human_typing send behind it is not retried")

    # --- R8c ----------------------------------------------------------------
    ask({"MsgAck": {"pane": F, "all": True}})
    check(wait_state(F, "Idle"), "R8c the agent is Idle again")
    tell("block")
    check(wait_state(F, "NeedsInput"), "R8c the first prompt reads NeedsInput")
    start = len(rx_chunks(F_LOG))
    r, _ = type_send(A, send(F, "AGAIN:first", "Auto"))
    rid = sent_id(r)
    check(outcome(r) == ("Mailbox", "agent_blocked"), f"R8c stored agent_blocked ({r})")
    tell("clear")
    tell("work:1500")
    check(wait_state(F, "Working"), "R8c the first prompt is gone and the agent works")
    time.sleep(1.2)
    held = f"msg: id={rid} retry pending (agent_working)" in srv.log()
    tell("block")
    check(wait_state(F, "NeedsInput"), "R8c a second prompt reads NeedsInput")
    check(held, "R8c the retry moved to agent_working, so a pass saw the first prompt gone")
    keys_on_prompt(0.6)
    time.sleep(1.5)
    tell("clear")
    at = wait_typed(F_LOG, "AGAIN:first", start)
    check(at is not None, f"R8c keys on the second prompt are its answer: the retry is typed ({rx_chunks(F_LOG)[start:]})")
    log = srv.log()
    check(f"msg: id={rid} retry ended" not in log, "R8c ...and it did not end")
    check(log.count(f"msg: id={rid} retry pending (agent_blocked)") == 2
          and f"msg: id={rid} submit waiting" not in log,
          "R8c moving back onto a prompt is logged as a retry, once more")

    # --- R9 -----------------------------------------------------------------
    log = srv.log()
    check("panicked at" not in log, "R9 no panic in the log")
    leaked = [b for b in ("RETRY:one", "ORDER:first", "MBOX:two", "HUMAN:five") if b in log]
    check(not leaked, f"R9 no message body in the log ({leaked})")
    srv.kill()

    # --- R10 ----------------------------------------------------------------
    boot(RUNDIR + "-guard", CONFIG_LONG_GUARD)
    check(wait_state(F, "Idle"), "R10 the agent settles Idle")
    tell("block")
    check(wait_state(F, "NeedsInput"), "R10 the prompt reads NeedsInput")
    start = len(rx_chunks(F_LOG))
    r, _ = type_send(A, send(F, "GUARD:ten", "Auto"))
    rid = sent_id(r)
    check(outcome(r) == ("Mailbox", "agent_blocked"), f"R10 stored agent_blocked ({r})")
    keys = keys_on_prompt(0.6)
    tell("clear")
    at = wait_typed(F_LOG, "GUARD:ten", start)
    check(at is not None, f"R10 keys that stop before the prompt goes are its answer: it is typed ({rx_chunks(F_LOG)[start:]})")
    if at is not None:
        waited = (rx_chunks(F_LOG)[at][0] - keys[-1]) / 1e9
        check(waited >= 3.0, f"R10 ...only once the guard's window after the last key ran out ({waited:.2f}s)")
    time.sleep(1.0)
    check(count_typed(F_LOG, "GUARD:ten", start) == 1, "R10 ...exactly once")
    check(f"msg: id={rid} retry ended" not in srv.log(), "R10 ...and its retry did not end")
    check("panicked at" not in srv.log(), "R10 no panic in the log")
    srv.kill()

    # --- R11 ----------------------------------------------------------------
    boot(RUNDIR + "-spin", CONFIG_LONG_WORKING)
    check(wait_state(F, "Idle", timeout=10.0), "R11 the agent settles Idle")
    tell("block")
    check(wait_state(F, "NeedsInput"), "R11 the prompt reads NeedsInput")
    start = len(rx_chunks(F_LOG))
    r, _ = type_send(A, send(F, "SPIN:eleven", "Auto"))
    rid = sent_id(r)
    check(outcome(r) == ("Mailbox", "agent_blocked"), f"R11 stored agent_blocked ({r})")
    keys = keys_on_prompt(0.4)
    tell("clear")
    tell("work:4000")
    time.sleep(max(0.0, keys[-1] / 1e9 + 1.3 - time.time()))
    before, began = cpu_seconds(srv.proc.pid), time.time()
    time.sleep(max(0.0, keys[-1] / 1e9 + 2.8 - time.time()))
    after, ended = cpu_seconds(srv.proc.pid), time.time()
    if before is None or after is None:
        print("SKIP  R11 no /proc here to read the server's CPU from")
    else:
        share = (after - before) / (ended - began)
        check(share < 0.3, f"R11 a retry put back as it was waits for its tick ({share:.0%} of a core)")
    check(f"msg: id={rid} retry pending (agent_working)" in srv.log(),
          "R11 ...the window measured is the retry meeting the working agent")
    at = wait_typed(F_LOG, "SPIN:eleven", start, timeout=12.0)
    check(at is not None, f"R11 ...and it is typed once the work ends ({rx_chunks(F_LOG)[start:]})")
    check("panicked at" not in srv.log(), "R11 no panic in the log")
finally:
    if srv is not None:
        srv.kill()

print()
print(f"{len(failures)} failure(s)" if failures else "all passed")
sys.exit(1 if failures else 0)
