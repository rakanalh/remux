#!/usr/bin/env python3
"""Typing a message into an agent's pane (`deliver: Submit` / `Auto`).

The target is the fakeagent (`fakeagent.py`): a raw-mode program that logs
every read off its PTY as `RX <time_ns> <hex>`, so the bytes the server typed
are asserted exactly, hex for hex, and the Enter's separate write is seen as
a separate read. Every sender is an interactive `sh` pane running
`wire_send.py`, because the harness's own connection is never a pane.

0.  The fakeagent pane is DETECTED as an agent (AgentStatus command
    "fakeagent") before anything else runs: every later case would fail
    agent_not_ready for the wrong reason otherwise.
Every typed message opens with an envelope line naming it and its sender
(`envelope()` below mirrors the server's), inside the wrap on a line of its
own, or before the body on the same line when the pane has paste off.

1a. With bracketed paste on, a Submit is answered Submitted, and the pane
    reads ESC[200~ + envelope + \\n + body + ESC[201~ as one chunk, then \\r
    as a LATER chunk, 290 ms to 2 s after it.
1b. The message is Submitted: listed by --all, not by a plain read, and not
    counted unread.
1c. With bracketed paste off, the body arrives unwrapped after the envelope
    on the same line, then \\r.
1d. The framing follows the pane's LIVE mode: after paste-off, bare.
1e. An inner newline stays inside the wrap.
1f. Auto into a plain shell: Mailbox, reason agent_not_ready, and the shell
    never sees the body.
1g. Submit into a plain shell: Error agent_not_ready, and the message is in
    the shell's mailbox, Queued.
1h. Mailbox into the fakeagent: Mailbox with no reason, and nothing typed.
1i. server.log names the submit and the not-ready fallback, and holds no
    body in any form it has leaked in: text, hex or a Debug byte list.
1j. After a restart the typed message is still Submitted.
1k. An agent showing a permission prompt is not typed into: Submit is
    agent_blocked, Auto is Mailbox/agent_blocked.
1l. With paste off, a body of more than one line is refused bad_body
    (Auto: Mailbox/bad_body): each newline would submit a line of it.
1m. A job named like an agent whose terminal is line-buffered (a `read`
    is what reads) is refused agent_not_reading, and the reader gets
    nothing.
1n. An agent that stops reading and exits: the text it never read is
    flushed, the Enter is never typed, the shell that takes over runs
    nothing, and the message is Queued in the mailbox (agent_did_not_read).
1o. A pane whose own waiting read takes the message is not typed into as
    well: the reader gets it and the submit is already_read.
1p. An agent that exits the moment it reads the text: no Enter, and the
    submit is no_such_target.
1q. A body larger than the terminal's input queue (10 KiB, many lines, the
    last one `touch <marker>`) into an agent that stops reading and exits:
    agent_did_not_read, and NOTHING of it reaches the shell that takes over:
    not the tail the writer still held when the queue was flushed, not the
    marker line. The writer is cancelled before the queue is flushed.
1r. The same 10 KiB into an agent that reads it slower than it arrives: the
    Enter is written only after the whole text has been read, and the send
    is Submitted.
1z. No panic.

Task 2, the per-agent rows ([messaging.submit.<command>]). The policy is read
by the server at startup, so each variant is a fresh server with its config:

2a. Default row: a blocked agent is refused (Submit) or left in the mailbox
    (Auto), nothing typed; once it is cleared, a Submit is typed.
2b. Default row (while_working = mailbox): a working agent's message stays
    in the mailbox under Submit AND Auto (a delivery choice, not a refusal),
    nothing typed, and the log names the setting.
2c. while_working = "queue": a working agent is typed into, while it works.
2d. while_working = "refuse": Submit is refused agent_working, Auto is
    Mailbox/agent_working; the log names the setting.
2e. bracketed = "off" with paste on: no wrap. bracketed = "on" with paste
    off: wrapped.
2f. enter = "\\n", enter_delay_ms = 50: the Enter is 0a, under 200 ms later.
2g. focus_in = true: the text starts with ESC[I, then the wrap.
2h. envelope = false: no envelope line (with 2e's off half).
2i. bracketed = "on" over a program with paste off: a multi-line body is
    refused bad_body, and the envelope shares the body's line (2e's on half),
    because the program never asked for a paste and each newline submits.
2z. No panic under any variant.

Run: python3 tests/frame/msg_submit.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402
import fakeagent  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msgsub")
# Outside the run dir: `Server.start` wipes the run dir.
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
"""

OPEN, CLOSE = b"\x1b[200~", b"\x1b[201~"

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


def read_box(pane, all=False):
    reply = ask({"MsgRead": {"pane": pane, "peek": True, "all": all}})
    return reply["MsgList"]["messages"] if name_of(reply) == "MsgList" else None


def status_of(pane):
    reply = ask({"AgentStatus": {"target": {"host": None, "target": {"Pane": pane}}}})
    return reply["AgentStatusList"]["panes"] if name_of(reply) == "AgentStatusList" else reply


def send(to, body, deliver):
    return {"MsgSend": {"to": {"host": None, "target": {"Pane": to}}, "body": body, "deliver": deliver}}


agents = [0]


def spawn_agent(paste):
    """A fakeagent pane: (pane_id, log path, control path)."""
    agents[0] += 1
    log, ctl = f"{RUNDIR}/agent-{agents[0]}.log", f"{RUNDIR}/agent-{agents[0]}.ctl"
    pane = cli_spawn("SplitBelow", argv=[FAKEAGENT, log, ctl])
    end = time.time() + 8
    while time.time() < end and not os.path.exists(log):
        time.sleep(0.05)
    if paste:
        tell(ctl, log, "paste-on")
    check(wait_idle(pane), f"...the agent in %{pane} settled Idle before the next send")
    return pane, log, ctl


def tell(ctl, log, command, timeout=5.0):
    """Give the fakeagent one command and wait for its acknowledgement, then
    long enough for the server to have parsed the output it caused."""
    before = ctl_count(log, command)
    with open(ctl + ".tmp", "w") as f:
        f.write(command + "\n")
    os.replace(ctl + ".tmp", ctl)
    end = time.time() + timeout
    while time.time() < end and ctl_count(log, command) == before:
        time.sleep(0.05)
    time.sleep(0.4)


def wait_idle(pane, timeout=5.0):
    """Wait until the server classifies `pane`'s agent Idle. The fakeagent's
    own output (a mode switch, a prompt) counts as work for `working_ms`, and
    while an agent works its row, not the case, decides what a submit does."""
    end = time.time() + timeout
    while time.time() < end:
        entries = status_of(pane)
        if isinstance(entries, list) and entries and entries[0]["state"] == "Idle":
            return True
        time.sleep(0.1)
    return False


def ctl_count(log, command):
    try:
        return sum(1 for ln in open(log) if ln.strip() == f"CTL {command}")
    except FileNotFoundError:
        return 0


def rx_chunks(log):
    try:
        lines = open(log).read().splitlines()
    except FileNotFoundError:
        return []
    return [(int(ln.split()[1]), bytes.fromhex(ln.split()[2] if len(ln.split()) > 2 else ""))
            for ln in lines if ln.startswith("RX ")]


def wait_rx(log, want, start=0, timeout=6.0):
    """The index of the first chunk from `start` on that equals `want`."""
    end = time.time() + timeout
    while time.time() < end:
        chunks = rx_chunks(log)
        for i in range(start, len(chunks)):
            if chunks[i][1] == want:
                return i
        time.sleep(0.05)
    return None


def enter_after(log, at, timeout=4.0):
    """The chunk after `at`, once it has arrived: (gap in ms, bytes)."""
    end = time.time() + timeout
    while time.time() < end:
        chunks = rx_chunks(log)
        if len(chunks) > at + 1:
            return (chunks[at + 1][0] - chunks[at][0]) / 1e6, chunks[at + 1][1]
        time.sleep(0.05)
    return None, None


def envelope(msg_id, pane, name=None):
    sender = f"{name} (%{pane})" if name else f"%{pane}"
    return (f"[remux message #{msg_id} from {sender}; "
            f"reply: remux msg send %{pane} --reply-to {msg_id}]").encode()


def leak_forms(secret):
    """Every form a body has leaked into a log in: as text, as hex, and as
    the Debug print of a byte list (`[99, 111, 114, ...]`, how InputToPane
    once leaked), searched without its bracket because in a framed write the
    body's bytes sit in the middle of the list."""
    data = secret.encode()
    return [secret, data.hex(), str(list(data[:6]))[1:-1]]


def sent_id(reply):
    return ((reply or {}).get("MsgSent") or {}).get("id")


def outcome(reply):
    """(delivery or error code, reason) of a send's reply."""
    if name_of(reply) == "Error":
        return ("Error", reply["Error"].get("code"))
    sent = (reply or {}).get("MsgSent") or {}
    return (sent.get("delivery"), sent.get("reason"))


def submitted(reply):
    sent = (reply or {}).get("MsgSent")
    return sent is not None and sent["delivery"] == "Submitted" and sent["reason"] is None


os.makedirs(BINDIR, exist_ok=True)
fakeagent.install(FAKEAGENT)
# Named like the agent, so the server takes it for one, but it reads a LINE:
# its terminal is line-buffered, which is what a shell, `read` or a password
# prompt behind an agent's name looks like. Whatever it reads it writes out.
WRAPPER = f"{BINDIR}/wrap/fakeagent"
os.makedirs(os.path.dirname(WRAPPER), exist_ok=True)
with open(WRAPPER, "w") as f:
    f.write("#!/bin/sh\nread x\nprintf '%s' \"$x\" > \"$1\"\nsleep 30\n")
os.chmod(WRAPPER, 0o755)

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
    S = cli_spawn("SplitRight")
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    F2, F2_LOG, _ = spawn_agent(paste=False)
    time.sleep(0.8)

    # --- 0 ------------------------------------------------------------------
    entries = status_of(F)
    check(isinstance(entries, list) and len(entries) == 1 and entries[0]["command"] == "fakeagent"
          and entries[0]["state"] is not None,
          f"0 the fakeagent pane %{F} is detected as an agent (got {entries})")

    # --- 1a -----------------------------------------------------------------
    r = type_send(A, send(F, "MSG:one", "Submit"))
    check(submitted(r), f"1a a Submit to %{F} is answered Submitted (got {r})")
    at = wait_rx(F_LOG, OPEN + envelope(sent_id(r), A) + b"\nMSG:one" + CLOSE)
    gap, enter = enter_after(F_LOG, at) if at is not None else (None, None)
    check(at is not None, f"1a %{F} read ESC[200~ envelope, MSG:one ESC[201~ as one chunk (got {rx_chunks(F_LOG)})")
    check(enter == b"\r" and gap is not None and 290 <= gap < 2000,
          f"1a ...then \\r as a separate read {gap} ms later (got {enter!r})")

    # --- 1b -----------------------------------------------------------------
    time.sleep(0.2)
    everything = read_box(F, all=True) or []
    plain = read_box(F) or []
    entries = status_of(F)
    check([(m["body"], m["state"]) for m in everything] == [("MSG:one", "Submitted")],
          f"1b the message is Submitted (got {everything})")
    check(plain == [] and isinstance(entries, list) and entries and entries[0]["unread"] == 0,
          f"1b ...hidden from a plain read and not unread (plain {plain}, status {entries})")

    # --- 1c -----------------------------------------------------------------
    r = type_send(A, send(F2, "MSG:two", "Submit"))
    at = wait_rx(F2_LOG, envelope(sent_id(r), A) + " \u2014 MSG:two".encode())
    gap, enter = enter_after(F2_LOG, at) if at is not None else (None, None)
    check(submitted(r) and at is not None and enter == b"\r",
          f"1c with paste off %{F2} reads the envelope, then the body on its line, then \\r (got {rx_chunks(F2_LOG)})")

    # --- 1d -----------------------------------------------------------------
    tell(F_CTL, F_LOG, "paste-off")
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    start = len(rx_chunks(F_LOG))
    r = type_send(A, send(F, "MSG:three", "Submit"))
    at = wait_rx(F_LOG, envelope(sent_id(r), A) + " \u2014 MSG:three".encode(), start)
    check(submitted(r) and at is not None,
          f"1d after paste-off the framing follows the live mode: bare (got {rx_chunks(F_LOG)[start:]})")

    # --- 1e -----------------------------------------------------------------
    tell(F_CTL, F_LOG, "paste-on")
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    start = len(rx_chunks(F_LOG))
    r = type_send(A, send(F, "MSG:four\nline two", "Submit"))
    at = wait_rx(F_LOG, OPEN + envelope(sent_id(r), A) + b"\nMSG:four\nline two" + CLOSE, start)
    check(submitted(r) and at is not None,
          f"1e an inner newline stays inside the wrap (got {rx_chunks(F_LOG)[start:]})")

    # --- 1f -----------------------------------------------------------------
    r = type_send(A, send(S, "MSG:five", "Auto"))
    sent = (r or {}).get("MsgSent") or {}
    check(sent.get("delivery") == "Mailbox" and sent.get("reason") == "agent_not_ready",
          f"1f Auto into the shell %{S} is Mailbox, agent_not_ready (got {r})")
    cli.send({"SubscribePane": {"pane_id": S, "cols": 80, "rows": 20, "size_demand": False}})
    seen = []
    end = time.time() + 1.5
    while time.time() < end:
        for m in cli.drain(0.3):
            if name_of(m) == "PaneContent" and m["PaneContent"]["pane_id"] == S:
                seen = ["".join(c["c"] for c in row) for row in m["PaneContent"]["cells"]]
    check(seen and not any("MSG:five" in row for row in seen),
          f"1f ...and the shell never sees the body (screen {[r.rstrip() for r in seen if r.strip()]})")

    # --- 1g -----------------------------------------------------------------
    r = type_send(A, send(S, "MSG:six", "Submit"))
    check(name_of(r) == "Error" and r["Error"].get("code") == "agent_not_ready",
          f"1g Submit into the shell is Error agent_not_ready (got {r})")
    box = read_box(S) or []
    check([(m["body"], m["state"]) for m in box if m["body"] == "MSG:six"] == [("MSG:six", "Queued")],
          f"1g ...and the message is in the shell's mailbox, Queued (got {box})")

    # --- 1h -----------------------------------------------------------------
    start = len(rx_chunks(F_LOG))
    r = type_send(A, send(F, "MSG:seven", "Mailbox"))
    sent = (r or {}).get("MsgSent") or {}
    time.sleep(1.5)
    check(sent.get("delivery") == "Mailbox" and sent.get("reason") is None and len(rx_chunks(F_LOG)) == start,
          f"1h Mailbox into %{F} is stored, no reason, nothing typed (got {r}, new chunks {rx_chunks(F_LOG)[start:]})")

    # --- 1i -----------------------------------------------------------------
    log = srv.log()
    check(f"msg: id=1 submitted to %{F} (bracketed=on, " in log, "1i server.log names the submit of id 1")
    check("submit -> mailbox (agent_not_ready)" in log and "submit refused (agent_not_ready)" in log,
          "1i ...and the not-ready fallback and refusal")
    leaked = [form for body in ("MSG:one", "MSG:four\nline two", "MSG:three") for form in leak_forms(body)
              if form in log]
    check(not leaked, f"1i ...and holds no body as text, hex or a byte list (found {leaked})")

    # --- 1k -----------------------------------------------------------------
    tell(F_CTL, F_LOG, "block")
    start = len(rx_chunks(F_LOG))
    r_sub = type_send(A, send(F, "MSG:blocked", "Submit"))
    r_auto = type_send(A, send(F, "MSG:blocked2", "Auto"))
    time.sleep(0.8)
    check(outcome(r_sub) == ("Error", "agent_blocked") and outcome(r_auto) == ("Mailbox", "agent_blocked")
          and len(rx_chunks(F_LOG)) == start,
          f"1k a blocked agent is not typed into (submit {r_sub}, auto {r_auto}, chunks {rx_chunks(F_LOG)[start:]})")
    tell(F_CTL, F_LOG, "clear")
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")

    # --- 1l -----------------------------------------------------------------
    start = len(rx_chunks(F2_LOG))
    r_sub = type_send(A, send(F2, "PLAIN:one\ntwo", "Submit"))
    r_auto = type_send(A, send(F2, "PLAIN:three\nfour", "Auto"))
    time.sleep(0.8)
    states = [m["state"] for m in (read_box(F2) or []) if m["body"].startswith("PLAIN:")]
    check(outcome(r_sub) == ("Error", "bad_body") and outcome(r_auto) == ("Mailbox", "bad_body")
          and len(rx_chunks(F2_LOG)) == start and states == ["Queued", "Queued"],
          f"1l with paste off a multi-line body is refused, nothing typed, both Queued "
          f"(submit {r_sub}, auto {r_auto}, states {states})")

    # --- 1m -----------------------------------------------------------------
    got = f"{RUNDIR}/wrapper-got.txt"
    W = cli_spawn("SplitBelow", argv=[WRAPPER, got])
    time.sleep(1.0)
    entries = status_of(W)
    r = type_send(A, send(W, "WRAP:line", "Submit"))
    time.sleep(1.5)
    states = [m["state"] for m in (read_box(W) or [])]
    check(isinstance(entries, list) and entries and entries[0]["command"] == "fakeagent",
          f"1m the line-buffered wrapper is detected as the agent (got {entries})")
    check(outcome(r) == ("Error", "agent_not_reading") and not os.path.exists(got) and states == ["Queued"],
          f"1m a line-buffered reader is refused agent_not_reading and gets nothing "
          f"(reply {r}, got.txt {open(got).read() if os.path.exists(got) else None!r}, states {states})")

    # --- 1n -----------------------------------------------------------------
    # A raw agent (its own pane program, no shell in the way) that stops
    # reading while a message is typed at it. The server must not write the
    # Enter, and must flush what it wrote so a later reader gets none of it.
    N, N_LOG, N_CTL = spawn_agent(paste=True)
    tell(N_CTL, N_LOG, "deaf")
    check(wait_idle(N), f"...the agent in %{N} settled Idle before the next send")
    r = type_send(A, send(N, "MSG:unread", "Submit"))
    check(outcome(r) == ("Error", "agent_did_not_read"),
          f"1n an agent that stops reading is agent_did_not_read, no Enter (got {r})")
    check(f"not read by %{N}; typed text discarded, left in the mailbox" in srv.log(),
          "1n ...and the log says the text was discarded")
    # Now let it read again: the flush cleared the queue, so it reads NOTHING
    # of the message (a leftover would be read and, after a real agent exits,
    # run by the shell).
    before = len(rx_chunks(N_LOG))
    tell(N_CTL, N_LOG, "resume")
    time.sleep(0.8)
    leftover = b"".join(c for _, c in rx_chunks(N_LOG)[before:])
    states = [m["state"] for m in (read_box(N) or [])]
    check(b"MSG:unread" not in leftover and b"\r" not in leftover and states == ["Queued"],
          f"1n ...the flushed queue delivers none of it when reading resumes, and it is Queued "
          f"(leftover {leftover!r}, states {states})")

    # --- 1o -----------------------------------------------------------------
    Q = cli_spawn("SplitBelow")
    time.sleep(0.8)
    requests[0] += 1
    w_in, w_out = f"{RUNDIR}/wait-in.json", f"{RUNDIR}/wait-out.json"
    with open(w_in, "w") as f:
        json.dump({"MsgRead": {"wait_ms": 20000}}, f)
    q_log, q_ctl = f"{RUNDIR}/dup.log", f"{RUNDIR}/dup.ctl"
    line = f"python3 {WIRE_SEND} {srv.sock} {w_in} {w_out} & {FAKEAGENT} {q_log} {q_ctl}\n"
    cli.send({"InputToPane": {"pane_id": Q, "data": list(line.encode())}})
    end = time.time() + 8
    while time.time() < end and not os.path.exists(q_log):
        time.sleep(0.05)
    tell(q_ctl, q_log, "paste-on")
    check(wait_idle(Q), f"...the agent in %{Q} settled Idle before the next send")
    time.sleep(0.5)
    r = type_send(A, send(Q, "DUP:one", "Submit"))
    waited = wait_file(w_out, timeout=5.0) or {}
    got_bodies = [m["body"] for m in (waited.get("MsgList") or {}).get("messages", [])]
    time.sleep(0.8)
    check(got_bodies == ["DUP:one"] and outcome(r) == ("Error", "already_read") and rx_chunks(q_log) == [],
          f"1o the pane's own waiting read gets the message and nothing is typed "
          f"(reader {got_bodies}, reply {r}, chunks {rx_chunks(q_log)})")

    # --- 1p -----------------------------------------------------------------
    X, X_LOG, X_CTL = spawn_agent(paste=True)
    tell(X_CTL, X_LOG, "exit-on-rx")
    check(wait_idle(X), f"...the agent in %{X} settled Idle before the next send")
    r = type_send(A, send(X, "EXIT:now", "Submit"))
    time.sleep(0.5)
    chunks = [c for _, c in rx_chunks(X_LOG)]
    check(outcome(r) == ("Error", "no_such_target") and len(chunks) == 1 and b"\r" not in chunks,
          f"1p an agent that exits on reading the text: no Enter, no_such_target (reply {r}, chunks {chunks})")

    # --- 1q -----------------------------------------------------------------
    # The same, with a body larger than the terminal's input queue and full of
    # newlines: none of it, not the tail the writer still held, reaches the
    # agent when it reads again.
    big_lines = [f"fill{n:04d} " + "a" * 90 for n in range(100)] + ["touch /tmp/rmx-never"]
    big = "\n".join(big_lines)
    Q1, Q1_LOG, Q1_CTL = spawn_agent(paste=True)
    tell(Q1_CTL, Q1_LOG, "deaf")
    check(wait_idle(Q1), f"...the agent in %{Q1} settled Idle before the next send")
    r = type_send(A, send(Q1, big, "Submit"))
    check(outcome(r) == ("Error", "agent_did_not_read"),
          f"1q a 10 KiB body into an agent that stops reading is agent_did_not_read (got {r})")
    before = len(rx_chunks(Q1_LOG))
    tell(Q1_CTL, Q1_LOG, "resume")
    time.sleep(1.0)
    leftover = b"".join(c for _, c in rx_chunks(Q1_LOG)[before:])
    states = [m["state"] for m in (read_box(Q1) or [])]
    check(b"fill00" not in leftover and b"touch" not in leftover and states == ["Queued"],
          f"1q ...none of the 10 KiB reaches the agent when it reads again "
          f"({len(leftover)} leftover bytes, states {states})")

    # --- 1r -----------------------------------------------------------------
    S2, S_LOG, S_CTL = spawn_agent(paste=True)
    tell(S_CTL, S_LOG, "slow:256:20")
    check(wait_idle(S2), f"...the agent in %{S2} settled Idle before the next send")
    slow_body = "\n".join(f"slow{n:04d} " + "b" * 90 for n in range(100))
    r = type_send(A, send(S2, slow_body, "Submit"))
    time.sleep(0.5)
    chunks = [c for _, c in rx_chunks(S_LOG)]
    whole = OPEN + envelope(sent_id(r), A) + b"\n" + slow_body.encode() + CLOSE
    check(submitted(r) and chunks and chunks[-1] == b"\r" and b"".join(chunks[:-1]) == whole
          and len(chunks) > 10,
          f"1r a slow reader: every byte read before the Enter, then Submitted "
          f"(reply {r}, {len(chunks)} reads, last {chunks[-1:] if chunks else None}, "
          f"text read {len(b''.join(chunks[:-1]))} of {len(whole)} bytes)")

    # --- 1j -----------------------------------------------------------------
    cli.close()
    srv.restart()
    everything = read_box(F, all=True) or []
    check(("MSG:one", "Submitted") in [(m["body"], m["state"]) for m in everything],
          f"1j after a restart id 1 is still Submitted (got {everything})")

    # --- 1z -----------------------------------------------------------------
    check("panicked at" not in srv.log(), "1z no panic in server.log")
finally:
    srv.kill()



# === Task 2: the per-agent rows ============================================
def start_variant(rows):
    """A fresh server whose config adds `rows`, with the sender pane A."""
    global srv, cli
    srv = Server(RUNDIR)
    srv.start(config=CONFIG + rows)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 160, "rows": 50}})
    cli.drain(0.8)
    pane = cli_spawn("SplitRight")
    time.sleep(0.8)
    return pane


def wait_state(pane, want, timeout=5.0):
    end = time.time() + timeout
    while time.time() < end:
        entries = status_of(pane)
        if isinstance(entries, list) and entries and entries[0]["state"] == want:
            return True
        time.sleep(0.1)
    return False


def no_new_chunks(log, start, wait=1.5):
    time.sleep(wait)
    return len(rx_chunks(log)) == start


FOCUS = b"\x1b[I"

# --- V1: the default row ------------------------------------------------------
try:
    A = start_variant("")
    F, F_LOG, F_CTL = spawn_agent(paste=True)

    # 2a
    tell(F_CTL, F_LOG, "block")
    start = len(rx_chunks(F_LOG))
    r_sub = type_send(A, send(F, "BLK:one", "Submit"))
    r_auto = type_send(A, send(F, "BLK:two", "Auto"))
    check(outcome(r_sub) == ("Error", "agent_blocked") and outcome(r_auto) == ("Mailbox", "agent_blocked")
          and no_new_chunks(F_LOG, start),
          f"2a a blocked agent: Submit refused, Auto in the mailbox, nothing typed (submit {r_sub}, auto {r_auto})")
    tell(F_CTL, F_LOG, "clear")
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    r = type_send(A, send(F, "BLK:three", "Submit"))
    check(submitted(r) and wait_rx(F_LOG, OPEN + envelope(sent_id(r), A) + b"\nBLK:three" + CLOSE, start) is not None,
          f"2a ...and once it is cleared a Submit is typed (got {r})")

    # 2b
    tell(F_CTL, F_LOG, "work:4000")
    working = wait_state(F, "Working")
    start = len(rx_chunks(F_LOG))
    r_sub = type_send(A, send(F, "WORK:one", "Submit"))
    r_auto = type_send(A, send(F, "WORK:two", "Auto"))
    check(working and outcome(r_sub) == ("Mailbox", "agent_working") and outcome(r_auto) == ("Mailbox", "agent_working")
          and no_new_chunks(F_LOG, start, 0.5),
          f"2b the default row leaves a working agent's message in the mailbox under both modes "
          f"(working {working}, submit {r_sub}, auto {r_auto})")
    check(f"msg: id={sent_id(r_sub)} submit -> mailbox (agent_working, while_working=mailbox)" in srv.log(),
          "2b ...and the log names the setting that decided")
    check("panicked at" not in srv.log(), "2z no panic under the default row")
finally:
    srv.kill()

# --- V2: queue, "\n", 50 ms, focus_in -----------------------------------------
try:
    A = start_variant("""
[messaging.submit.fakeagent]
while_working = "queue"
enter = "\\n"
enter_delay_ms = 50
focus_in = true
""")
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    tell(F_CTL, F_LOG, "work:4000")
    working = wait_state(F, "Working")
    r = type_send(A, send(F, "QUEUE:one", "Submit"))
    still = wait_state(F, "Working", timeout=0.5)
    at = wait_rx(F_LOG, FOCUS + OPEN + envelope(sent_id(r), A) + b"\nQUEUE:one" + CLOSE)
    gap, enter = enter_after(F_LOG, at) if at is not None else (None, None)
    check(working and submitted(r) and still,
          f"2c while_working=queue types into a working agent, while it works (working {working}, reply {r}, still {still})")
    check(at is not None, f"2g focus_in: the text starts with ESC[I, then the wrap (got {rx_chunks(F_LOG)})")
    check(enter == b"\n" and gap is not None and gap < 200,
          f"2f enter and enter_delay_ms: the Enter is 0a, {gap} ms later (got {enter!r})")
    check("panicked at" not in srv.log(), "2z no panic under the queue row")
finally:
    srv.kill()

# --- V3: refuse, bracketed off, envelope off ---------------------------------
try:
    A = start_variant("""
[messaging.submit.fakeagent]
while_working = "refuse"
bracketed = "off"
envelope = false
""")
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    r = type_send(A, send(F, "BR:off", "Submit"))
    at = wait_rx(F_LOG, b"BR:off")
    check(submitted(r) and at is not None,
          f"2e bracketed=off with paste on: no wrap; 2h envelope=false: no envelope (reply {r}, got {rx_chunks(F_LOG)})")
    tell(F_CTL, F_LOG, "work:4000")
    working = wait_state(F, "Working")
    start = len(rx_chunks(F_LOG))
    r_sub = type_send(A, send(F, "REF:one", "Submit"))
    r_auto = type_send(A, send(F, "REF:two", "Auto"))
    check(working and outcome(r_sub) == ("Error", "agent_working") and outcome(r_auto) == ("Mailbox", "agent_working")
          and no_new_chunks(F_LOG, start, 0.5),
          f"2d while_working=refuse: Submit refused, Auto in the mailbox (submit {r_sub}, auto {r_auto})")
    check(f"msg: id={sent_id(r_auto)} submit -> mailbox (agent_working, while_working=refuse)" in srv.log()
          and "submit refused (agent_working, while_working=refuse)" in srv.log(),
          "2d ...and the log names the setting that decided")
    check("panicked at" not in srv.log(), "2z no panic under the refuse row")
finally:
    srv.kill()

# --- V4: bracketed on ----------------------------------------------------------
try:
    A = start_variant("""
[messaging.submit.fakeagent]
bracketed = "on"
""")
    F, F_LOG, F_CTL = spawn_agent(paste=False)
    r = type_send(A, send(F, "BR:on", "Submit"))
    at = wait_rx(F_LOG, OPEN + envelope(sent_id(r), A) + " \u2014 BR:on".encode() + CLOSE)
    check(submitted(r) and at is not None,
          f"2e bracketed=on with paste off: wrapped, the envelope on the body's line (reply {r}, got {rx_chunks(F_LOG)})")

    # 2i
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    start = len(rx_chunks(F_LOG))
    r = type_send(A, send(F, "BR:one\ntwo", "Submit"))
    check(outcome(r) == ("Error", "bad_body") and no_new_chunks(F_LOG, start, 0.8),
          f"2i bracketed=on over paste off refuses a multi-line body: a forced wrap would submit it line by line (got {r})")
    check("panicked at" not in srv.log(), "2z no panic under the bracketed row")
finally:
    srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED")
    sys.exit(1)
print("ALL PASS")
