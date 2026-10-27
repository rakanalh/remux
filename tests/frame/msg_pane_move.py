#!/usr/bin/env python3
"""Agent messaging across a pane move into another session.

`PaneMoveToTabTarget` with a `session` re-homes a pane, same id, same PTY.
Mail, names and attribution are keyed by the pane id and the process tree,
never by the session, so the move must carry all three with the pane:

  main: A | B (B focused, named bravo)      other: its own first pane

7a. Before the move, A's message to bravo is stored in B.
7b. B is moved into `other`: the tree has B there and A still in main, so
    every later case is about a pane that really changed session.
7c. B's mailbox still holds the message, Queued, and status still counts it.
7d. The name bravo still resolves to B: a new message lands there.
7e. B reading its own mailbox from inside is attributed to B, so it is a real
    read that marks both messages Read, not a peek.
7f. B's reply to A is from B, named bravo, labelled with the session B is in
    NOW.
7g. A wait parked on B before a move back into main is answered by the send
    that follows the move, not dropped as if B had closed.
7h. B execs the fakeagent and is moved into other again, and no client
    attends it there. A Submit --wait to bravo is typed into it whole, framed,
    and answered Submitted when its turn settles, never
    pane_closed_after_submit: the submit path finds B in whichever session
    holds it.
7i. A Submit --wait whose agent is still working when B is moved back into
    main, mid-wait, is answered the same way, after the move.
7z. No panic and no message body in the server log.

Run: python3 tests/frame/msg_pane_move.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402
import fakeagent  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msgmove")
# Outside the run dir: `Server.start` wipes the run dir.
BINDIR = RUNDIR + "-bin"
FAKEAGENT = f"{BINDIR}/fakeagent"

CONFIG = """
[agents]
commands = ["fakeagent"]
working_ms = 500

[messaging]
stall_ms = 2000
settle_ms = 500
"""

OPEN, CLOSE = b"\x1b[200~", b"\x1b[201~"
PROBES = (OPEN, b"\x1b[I" + OPEN)
WIRE_SEND = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wire_send.py")

for var in ("REMUX_SESSION", "REMUX_PANE"):
    os.environ.pop(var, None)

failures = []
requests = [0]


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def wait_file(path, timeout=6.0):
    end = time.time() + timeout
    while time.time() < end:
        if os.path.exists(path):
            with open(path) as f:
                return json.loads(f.read())
        time.sleep(0.05)
    return None


def type_send(pane_id, msg):
    """Run one wire request from inside `pane_id`'s interactive shell."""
    requests[0] += 1
    n = requests[0]
    in_file, out_file = f"{RUNDIR}/in-{n}.json", f"{RUNDIR}/out-{n}.json"
    with open(in_file, "w") as f:
        json.dump(msg, f)
    line = f"python3 {WIRE_SEND} {srv.sock} {in_file} {out_file}\n"
    cli.send({"InputToPane": {"pane_id": pane_id, "data": list(line.encode())}})
    return wait_file(out_file)


def ask(msg, timeout=3.0):
    """One request from the harness's own connection (never a pane)."""
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


def send_to(target, body):
    return {"MsgSend": {"to": {"host": None, "target": target}, "body": body,
                        "deliver": "Mailbox"}}


def peek(pane):
    r = ask({"MsgRead": {"pane": pane, "peek": True, "all": True}})
    return r["MsgList"]["messages"] if name_of(r) == "MsgList" else None


def status_of(pane):
    r = ask({"AgentStatus": {"target": {"host": None, "target": {"Pane": pane}}}})
    rows = r["AgentStatusList"]["panes"] if name_of(r) == "AgentStatusList" else []
    return rows[0] if len(rows) == 1 else r


def raw_chunks(log):
    """Every read the fakeagent made: (time_ns, bytes)."""
    try:
        lines = open(log).read().splitlines()
    except FileNotFoundError:
        return []
    return [(int(ln.split()[1]), bytes.fromhex(ln.split()[2] if len(ln.split()) > 2 else ""))
            for ln in lines if ln.startswith("RX ")]


def rx_chunks(log):
    """The reads, with the server's probe joined to the body it opens, up to
    the read that carries the paste's close (as msg_submit.py reads them)."""
    out = []
    pending = None
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


def framed(msg_id, pane, body):
    """A message typed wrapped, exactly, with a header and trailer mark that
    must agree (msg_submit.py's `framed`, its defaults)."""
    head = (re.escape(f"# [remux message #{msg_id} from %{pane}, mark ".encode())
            + rb"(?P<mark>[0-9a-f]{16})"
            + re.escape(f"; reply: remux msg send %{pane} --reply-to {msg_id}]".encode()))
    tail = re.escape(f"# [end of remux message #{msg_id}, mark ".encode()) + rb"(?P=mark)\]"
    return re.compile(re.escape(OPEN) + head + b"\n" + re.escape(body.encode()) + b"\n" + tail
                      + re.escape(CLOSE), re.S)


def typed_then_entered(log, pattern, timeout=8.0):
    """Whether a read matching `pattern` arrived, followed by a lone Enter."""
    end = time.time() + timeout
    while time.time() < end:
        chunks = rx_chunks(log)
        for i, (_, data) in enumerate(chunks):
            if pattern.fullmatch(data) and i + 1 < len(chunks) and chunks[i + 1][1] == b"\r":
                return True
        time.sleep(0.05)
    return False


def tell(ctl, log, command, timeout=5.0):
    """Give the fakeagent one command and wait for its acknowledgement."""
    def acks():
        try:
            return sum(1 for ln in open(log) if ln.strip() == f"CTL {command}")
        except FileNotFoundError:
            return 0
    before = acks()
    with open(ctl + ".tmp", "w") as f:
        f.write(command + "\n")
    os.replace(ctl + ".tmp", ctl)
    end = time.time() + timeout
    while time.time() < end and acks() == before:
        time.sleep(0.05)
    time.sleep(0.4)


def wait_idle_agent(pane, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        row = status_of(pane)
        if isinstance(row, dict) and row.get("command") == "fakeagent" and row.get("state") == "Idle":
            return True
        time.sleep(0.1)
    return False


def type_start(pane_id, msg):
    """Like type_send, but return the reply file without waiting on it."""
    requests[0] += 1
    n = requests[0]
    in_file, out_file = f"{RUNDIR}/in-{n}.json", f"{RUNDIR}/out-{n}.json"
    with open(in_file, "w") as f:
        json.dump(msg, f)
    line = f"python3 {WIRE_SEND} {srv.sock} {in_file} {out_file}\n"
    cli.send({"InputToPane": {"pane_id": pane_id, "data": list(line.encode())}})
    return out_file


def wait_send(target, body):
    msg = send_to(target, body)
    msg["MsgSend"]["deliver"] = "Submit"
    msg["MsgSend"]["wait"] = True
    return msg


def waited(reply):
    """(delivery or error code, reason, final_state, timed_out) of a reply."""
    if name_of(reply) == "Error":
        return ("Error", reply["Error"].get("code"), None, None)
    sent = (reply or {}).get("MsgSent") or {}
    return (sent.get("delivery"), sent.get("reason"), sent.get("final_state"), sent.get("timed_out"))


def move_focused(session):
    cli.send({"Command": {"PaneMoveToTabTarget": {"tab_id": None, "session": session}}})
    cli.drain(1.0)


def attach(session):
    cli.send({"Attach": {"session_name": session}})
    cli.drain(0.8)


def tree():
    """{pane_id: (session, is_focused)} from a fresh SessionTree."""
    c = Client(srv.sock)
    c.hello()
    c.send("ListSessionTree")
    try:
        for msg in c.drain(1.0):
            if name_of(msg) != "SessionTree":
                continue
            t = msg["SessionTree"]
            groups = list(t["unfiled"])
            for f in t["folders"]:
                groups += f["sessions"]
            return {p["id"]: (s["name"], p["is_focused"])
                    for s in groups for tab in s["tabs"] for p in tab["panes"]}
    finally:
        c.close()
    return {}


os.makedirs(BINDIR, exist_ok=True)
fakeagent.install(FAKEAGENT)

srv = Server(RUNDIR)
try:
    srv.start(config=CONFIG)

    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 120, "rows": 40}})
    cli.drain(0.8)
    other = Client(srv.sock)
    other.hello()
    other.send({"CreateSession": {"name": "other", "folder": None}})
    other.drain(0.8)
    other.close()

    [A] = [p for p, (s, _) in tree().items() if s == "main"]
    cli.send({"Command": "PaneSplitVertical"})
    cli.drain(0.8)
    B = next((p for p, (s, focused) in tree().items() if s == "main" and focused and p != A), None)
    check(B is not None, f"setup: main has A=%{A} and a focused B (tree {tree()})")
    time.sleep(0.5)

    r = type_send(B, {"AgentName": {"name": "bravo"}})
    check(r == {"AgentNamed": {"pane": B, "name": "bravo"}}, f"setup: B names itself bravo (got {r})")

    # --- 7a ---------------------------------------------------------------
    r = type_send(A, send_to({"Name": "bravo"}, "MOVE:before"))
    check(name_of(r) == "MsgSent" and r["MsgSent"]["to"]["pane_id"] == B,
          f"7a A's message to bravo is stored in %{B} (got {r})")

    # --- 7b ---------------------------------------------------------------
    cli.send({"Command": {"PaneMoveToTabTarget": {"tab_id": None, "session": "other"}}})
    cli.drain(1.0)
    t = tree()
    check(t.get(B, (None,))[0] == "other" and t.get(A, (None,))[0] == "main",
          f"7b B is in other and A is still in main (tree {t})")

    # --- 7c ---------------------------------------------------------------
    box = peek(B)
    check([(m["body"], m["state"]) for m in box or []] == [("MOVE:before", "Queued")],
          f"7c B's mailbox still holds MOVE:before, Queued (got {box})")
    row = status_of(B)
    check(isinstance(row, dict) and row.get("name") == "bravo" and row.get("unread") == 1,
          f"7c status still lists B as bravo with 1 unread (got {row})")

    # --- 7d ---------------------------------------------------------------
    r = type_send(A, send_to({"Name": "bravo"}, "MOVE:after"))
    check(name_of(r) == "MsgSent" and r["MsgSent"]["to"]["pane_id"] == B,
          f"7d bravo still resolves to %{B} after the move (got {r})")

    # --- 7e ---------------------------------------------------------------
    r = type_send(B, {"MsgRead": {}})
    got = [m["body"] for m in r["MsgList"]["messages"]] if name_of(r) == "MsgList" else r
    check(got == ["MOVE:before", "MOVE:after"],
          f"7e B reads both messages from inside, after the move (got {r})")
    box = peek(B)
    check([m["state"] for m in box or []] == ["Read", "Read"],
          f"7e B's own read marked both Read, so it was attributed to B (got {box})")

    # --- 7f ---------------------------------------------------------------
    r = type_send(B, send_to({"Pane": A}, "MOVE:reply"))
    check(name_of(r) == "MsgSent", f"7f B's reply to A is stored (got {r})")
    froms = [m["from"].get("Pane") for m in peek(A) or [] if m["body"] == "MOVE:reply"]
    check([(f["pane_id"], f["session"], f["name"]) for f in froms if f]
          == [(B, "other", "bravo")],
          f"7f the reply is from %{B}, bravo, in session other (got {froms})")

    # --- 7g ---------------------------------------------------------------
    w = Client(srv.sock)
    w.hello()
    w.send({"MsgRead": {"pane": B, "peek": True, "wait_ms": 8000}})
    end = time.time() + 6.0
    while not re.search(rf"msg: wait on %{B} parked for client_id=\d+ ", srv.log()) \
            and time.time() < end:
        time.sleep(0.05)
    check(re.search(rf"msg: wait on %{B} parked for client_id=\d+ ", srv.log()) is not None,
          f"7g a wait on %{B} parked")
    cli.send({"Command": {"PaneMoveToTabTarget": {"tab_id": None, "session": "main"}}})
    cli.drain(1.0)
    t = tree()
    check(t.get(B, (None,))[0] == "main", f"7g B is moved back into main (tree {t})")
    r = type_send(A, send_to({"Name": "bravo"}, "MOVE:waited"))
    check(name_of(r) == "MsgSent", f"7g A's message to bravo is stored (got {r})")
    got = None
    end = time.time() + 6.0
    while got is None and time.time() < end:
        for m in w.drain(0.2):
            if name_of(m) != "ViewList":
                got = m
                break
    w.close()
    bodies = [m["body"] for m in got["MsgList"]["messages"]] if name_of(got) == "MsgList" else got
    check(bodies == ["MOVE:waited"],
          f"7g the wait parked before the move is answered by the send after it (got {got})")

    # --- 7h ---------------------------------------------------------------
    AGENT_LOG, AGENT_CTL = f"{RUNDIR}/agent.log", f"{RUNDIR}/agent.ctl"
    cli.send({"InputToPane": {"pane_id": B, "data": list(f"exec {FAKEAGENT} {AGENT_LOG} {AGENT_CTL}\n".encode())}})
    end = time.time() + 8
    while time.time() < end and not os.path.exists(AGENT_LOG):
        time.sleep(0.05)
    tell(AGENT_CTL, AGENT_LOG, "paste-on")
    check(wait_idle_agent(B), f"7h B runs the fakeagent, detected and Idle (got {status_of(B)})")
    t = tree()
    check(t.get(B) == ("main", True), f"7h ...and is the focused pane of main, so the move takes it (tree {t})")
    move_focused("other")
    attach("main")
    t = tree()
    check(t.get(B, (None,))[0] == "other", f"7h B is in other again, and the client is back in main (tree {t})")
    check(wait_idle_agent(B), "7h ...and the agent there is Idle")
    out = type_start(A, wait_send({"Name": "bravo"}, "GO:2500 after the move"))
    r = wait_file(out, 15.0)
    sent = (r or {}).get("MsgSent") or {}
    check(waited(r) == ("Submitted", None, "Idle", False),
          f"7h a Submit --wait to bravo in other is Submitted and settles Idle, "
          f"not pane_closed_after_submit (got {r})")
    check(sent.get("id") is not None and typed_then_entered(AGENT_LOG, framed(sent.get("id"), A, "GO:2500 after the move")),
          f"7h ...and the agent read it framed, whole, then its Enter (reads {rx_chunks(AGENT_LOG)[-3:]})")

    # --- 7i ---------------------------------------------------------------
    check(wait_idle_agent(B), "7i the agent is Idle before the next send")
    t0 = time.time()
    out = type_start(A, wait_send({"Name": "bravo"}, "GO:3000 moved mid-wait"))
    end = time.time() + 8
    while time.time() < end and not any(b"GO:3000 moved mid-wait" in d for _, d in rx_chunks(AGENT_LOG)):
        time.sleep(0.05)
    attach("other")
    move_focused("main")
    moved_at = time.time()
    t = tree()
    answered_early = os.path.exists(out)
    check(t.get(B, (None,))[0] == "main" and not answered_early,
          f"7i B was moved back into main while the wait was still open "
          f"({moved_at - t0:.1f}s after the send; tree {t})")
    r = wait_file(out, 15.0)
    check(waited(r) == ("Submitted", None, "Idle", False),
          f"7i the wait spanning the move is Submitted and settles Idle, not pane_closed_after_submit (got {r})")

    # --- 7z ---------------------------------------------------------------
    cli.close()
    time.sleep(0.3)
    log = srv.log()
    check("panicked at" not in log, "7z no panic in the server log")
    leaked = [b for b in ("MOVE:before", "MOVE:after", "MOVE:reply", "MOVE:waited", "after the move",
                          "moved mid-wait")
              if b in log]
    check(leaked == [], f"7z no message body is in the server log (found {leaked})")
finally:
    srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED")
    sys.exit(1)
print("ALL PASS")
