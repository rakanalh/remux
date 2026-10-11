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
1p. An agent that exits the moment it reads the probe: the body and the
    Enter are never written, and the submit is no_such_target or
    agent_did_not_read, whichever of the pane's close and the agent's exit
    the server sees first.
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

Task 3, the typing guard, through the queue and the submitter:

3a. Unattended (another pane focused): a cell's keystrokes into F hold a
    submit for the SHORT window, 500 ms after the last one.
3b. Attended (F focused): a client's keystrokes hold it for 3000 ms, and the
    wait is logged once.
3c. Typing past typing_guard_max_ms: Submit is human_typing, Auto is
    Mailbox/human_typing, nothing typed, both Queued.
3d. submit_when_attended = false: an attended pane is refused pane_attended
    (Auto: Mailbox); once unattended it is typed.
3e. Acked while the guard held it: Mailbox/already_read under both modes,
    since the target has it and an error would provoke a retry; nothing is
    typed.
3f. The sender's connection closes mid-guard: the message is still typed,
    and its reply is logged as detached.
3g. A second send on a connection with a send parked: bad_request.
3h. A send held 3 s behind typing logs at most 4 lines for its id.
3j. A permission prompt drawn after the agent read the text (block-on-rx):
    no Enter; the agent has the message, so Mailbox/agent_blocked_read under
    both modes, and Submitted.
3k. A person's key after the agent read the text, before the Enter: no
    Enter, the key stays, Mailbox/typed_over_read, Submitted.
3l. A key binding's SendKey holds a submit like a keystroke.
3m. One pane waiting for its agent to read does not hold another pane's
    submit.
3n. A send refused before its text is written does not wedge the pane: the
    next send to it is answered.
3i. The submit budget: six sends to an agent that reads nothing, with the
    Enter delay at its 5 s cap, are typed one after another; the sixth
    reaches the head of the queue after the server's hold and is answered
    submit_timeout without being typed, and every answer arrives inside the budget
    the CLI waits for.
3o. Claude Code's file-edit approval dialog on screen, which no configured
    pattern matches (the panel reads Idle): Submit is agent_blocked, Auto is
    Mailbox/agent_blocked, nothing is typed. Only the submit's own
    "looks like a prompt" check can refuse it.
3p. The same dialog drawn after the text: no Enter.
3q. The SHIPPED claude patterns, read out of src/config/agents.rs, classify
    that dialog NeedsInput.
3r. An agent that took the probe and reads nothing more, a person's Enter
    while the text is unread, then the agent exits to bash: bash never runs
    the body, because the key took the pane back and the text was flushed
    ahead of it.
3s. The same without the person's key (the control): bash runs nothing.
3t. An agent that reads nothing, a person's Enter 1.2 s into the send and
    the agent's exit at 1.4 s, before any deadline: bash does not run the
    body.
3u. The same with no key: once the agent exits, the body is not in bash's
    line buffer either, so the person's next Enter runs nothing.
3v. A person's Enter during the Enter delay, after the agent read the text:
    the agent has the message, so the answer is Mailbox/typed_over_read
    under both modes and the message stays Submitted.
3w. An agent that took the probe and hangs, a person's key, then the
    agent's exit: human_typing, and bash runs nothing.
3x. The probe (the paste opener) is written and read on its own, before the
    rest of the message.
3y. An agent that exits after a message was typed and Submitted: its pane
    closes. (Nothing of the message's may hold the terminal open.)
3aa. An agent that takes the probe and hangs, then exits 0.5 s later, with a
    multi-line body: the body is flushed before the exit, so bash runs no
    line of it, then or on the next Enter.
3ab. A person's key while a slow agent is part-way through a long body: no
    byte of the body follows the key, the paste is closed before it, and the
    log says how many bytes were withdrawn.
3ac. A person's key right after the agent read the probe, before the body:
    the key is never inside an open paste.
3ad. An agent that reads the whole body and exits before its Enter, with
    nothing left to flush: read_by_another_program, and a WARN.
3z. No panic.

Run: python3 tests/frame/msg_submit.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import re
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


def wait_closed(pane, timeout=6.0):
    """Wait until the server reports `pane`'s terminal gone: its program
    exited and nothing else holds the terminal open."""
    end = time.time() + timeout
    while time.time() < end:
        if f"PTY channel disconnected for pane_id={pane} " in srv.log():
            return True
        time.sleep(0.1)
    return False


def ctl_count(log, command):
    try:
        return sum(1 for ln in open(log) if ln.strip() == f"CTL {command}")
    except FileNotFoundError:
        return 0


def raw_chunks(log):
    """Every read the fakeagent made: (time_ns, bytes)."""
    try:
        lines = open(log).read().splitlines()
    except FileNotFoundError:
        return []
    return [(int(ln.split()[1]), bytes.fromhex(ln.split()[2] if len(ln.split()) > 2 else ""))
            for ln in lines if ln.startswith("RX ")]


PROBES = (OPEN, b"\x1b[I" + OPEN)


def rx_chunks(log):
    """The reads, with the server's probe (a paste opener, written and read
    on its own before the rest of a message) joined to the read after it,
    timed by that later read. A probe with nothing after it yet is left out,
    so a wait for a message's text is not satisfied by its probe alone."""
    out = []
    pending = None
    for ns, data in raw_chunks(log):
        if pending is not None:
            out.append((ns, pending + data))
            pending = None
        elif data in PROBES:
            pending = data
        else:
            out.append((ns, data))
    return out


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
    return (f"# [remux message #{msg_id} from {sender}; "
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
    check(got_bodies == ["DUP:one"] and outcome(r) == ("Mailbox", "already_read") and rx_chunks(q_log) == [],
          f"1o the pane's own waiting read gets the message and nothing is typed "
          f"(reader {got_bodies}, reply {r}, chunks {rx_chunks(q_log)})")

    # --- 1p -----------------------------------------------------------------
    X, X_LOG, X_CTL = spawn_agent(paste=True)
    tell(X_CTL, X_LOG, "exit-on-rx")
    check(wait_idle(X), f"...the agent in %{X} settled Idle before the next send")
    r = type_send(A, send(X, "EXIT:now", "Submit"))
    time.sleep(0.5)
    chunks = [c for _, c in raw_chunks(X_LOG)]
    check(outcome(r) in (("Error", "no_such_target"), ("Error", "agent_did_not_read")) and chunks == [OPEN],
          f"1p an agent that exits on reading the probe: no text, no Enter; the pane closing or the agent gone "
          f"(reply {r}, chunks {chunks})")
    check(wait_closed(X), f"1p ...and its pane closes")

    # --- 1q -----------------------------------------------------------------
    # A body larger than the terminal's input queue and full of newlines, to
    # an agent that takes the probe and then stops reading: none of it, not
    # the tail the writer still held, reaches the agent when it reads again.
    big_lines = [f"fill{n:04d} " + "a" * 90 for n in range(100)] + ["touch /tmp/rmx-never"]
    big = "\n".join(big_lines)
    Q1, Q1_LOG, Q1_CTL = spawn_agent(paste=True)
    tell(Q1_CTL, Q1_LOG, "probe-only")
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

# === Task 3: the submit queue, the submitter and the typing guard ===========
# while_working = "queue": a keystroke's echo makes the fakeagent read as
# Working for working_ms, which is the unattended window itself, and these
# cases are about the guard, not the working policy (Task 2 covers that).
T3 = """
[messaging]
typing_guard_max_ms = 4000

[messaging.submit.fakeagent]
while_working = "queue"
"""
import threading  # noqa: E402


def focused_pane():
    """The focused pane of session main's active tab, from the session tree."""
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


def typer(stop, via, pane=None, every=0.2):
    """Type "k" into the pane every `every` seconds until `stop` is set, on a
    connection of its own. `via` "input" is a client's keystroke (attached to
    main, so it reaches main's focused pane); "sendkey" is a key binding's
    SendKey, which reaches the same pane; "cell" is a View cell's
    (InputToPane, to `pane`)."""
    c = Client(srv.sock)
    c.hello()
    if via in ("input", "sendkey"):
        c.send({"Attach": {"session_name": "main"}})
        c.send({"Resize": {"cols": 160, "rows": 50}})
        c.drain(0.3)
    while not stop.is_set():
        if via == "input":
            c.send({"Input": {"data": [0x6b]}})
        elif via == "sendkey":
            c.send({"Command": {"SendKey": [0x6b]}})
        else:
            c.send({"InputToPane": {"pane_id": pane, "data": [0x6b]}})
        c.drain(every)
    c.close()


def last_k(log):
    ks = [ns for ns, c in rx_chunks(log) if c == b"k"]
    return ks[-1] if ks else None


def chunk_at(log, body):
    for ns, c in rx_chunks(log):
        if body.encode() in c:
            return ns
    return None


def type_start(pane_id, msg, fire=False, background=False):
    """Like type_send, but return the reply file without waiting on it. With
    `background` the shell runs it as a background job, so the next command
    typed into the pane runs at once rather than after it exits."""
    requests[0] += 1
    n = requests[0]
    in_file, out_file = f"{RUNDIR}/in-{n}.json", f"{RUNDIR}/out-{n}.json"
    with open(in_file, "w") as f:
        json.dump(msg, f)
    flag = "--fire " if fire else ""
    job = " &" if background else ""
    line = f"python3 {WIRE_SEND} {flag}{srv.sock} {in_file} {out_file}{job}\n"
    cli.send({"InputToPane": {"pane_id": pane_id, "data": list(line.encode())}})
    return out_file


# --- U: F unattended (A spawned last, so A is focused) ---------------------
try:
    srv = Server(RUNDIR)
    srv.start(config=CONFIG + T3)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 160, "rows": 50}})
    cli.drain(0.8)
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    A = cli_spawn("SplitRight")
    time.sleep(0.8)
    check(focused_pane() == A, f"setup: A is focused, so %{F} is unattended (focused {focused_pane()})")
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")

    # 3a
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop, "cell", F))
    t.start()
    time.sleep(1.2)
    stop.set()
    t.join()
    r = type_send(A, send(F, "GUARD:unattended", "Submit"))
    k, at = last_k(F_LOG), chunk_at(F_LOG, "GUARD:unattended")
    gap = (at - k) / 1e6 if (k and at) else None
    check(submitted(r) and gap is not None and 450 <= gap < 2500,
          f"3a unattended: typed {gap} ms after the last cell keystroke (window 500 ms) (reply {r})")
    check("panicked at" not in srv.log(), "3z no panic (unattended)")
finally:
    srv.kill()

# --- T: F attended (F spawned last, so F is focused) ------------------------
try:
    srv = Server(RUNDIR)
    srv.start(config=CONFIG + T3)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 160, "rows": 50}})
    cli.drain(0.8)
    A = cli_spawn("SplitRight")
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    time.sleep(0.8)
    check(focused_pane() == F, f"setup: %{F} is focused, so it is attended (focused {focused_pane()})")
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")

    # 3b
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop, "input"))
    t.start()
    time.sleep(1.2)
    stop.set()
    t.join()
    r = type_send(A, send(F, "GUARD:attended", "Submit"))
    k, at = last_k(F_LOG), chunk_at(F_LOG, "GUARD:attended")
    gap = (at - k) / 1e6 if (k and at) else None
    check(submitted(r) and gap is not None and 2950 <= gap < 5000,
          f"3b attended: typed {gap} ms after the last keystroke (window 3000 ms) (reply {r})")
    sid = sent_id(r)
    waits = [ln for ln in srv.log().splitlines() if f"msg: id={sid} submit waiting: human_typing" in ln]
    check(len(waits) == 1, f"3b ...logged as waiting once (got {len(waits)})")
    # 3h: no per-pass logging
    lines = [ln for ln in srv.log().splitlines() if re.search(rf"\bmsg: id={sid}\b", ln)]
    check(len(lines) <= 4, f"3h a send held 3 s behind typing logs at most 4 lines (got {len(lines)}: {lines})")

    # 3c
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop, "input"))
    t.start()
    time.sleep(0.6)
    start = len(rx_chunks(F_LOG))
    t0 = time.time()
    r_sub = type_send(A, send(F, "GUARD:keptsub", "Submit"))
    took = time.time() - t0
    r_auto = type_send(A, send(F, "GUARD:keptauto", "Auto"))
    stop.set()
    t.join()
    typed_any = any(b"GUARD:kept" in c for _, c in rx_chunks(F_LOG)[start:])
    states = [m["state"] for m in (read_box(F) or []) if m["body"].startswith("GUARD:kept")]
    check(outcome(r_sub) == ("Error", "human_typing") and 3.5 <= took < 7.0,
          f"3c typing past the max: Submit is human_typing after {took:.1f}s (got {r_sub})")
    check(outcome(r_auto) == ("Mailbox", "human_typing") and not typed_any and states == ["Queued", "Queued"],
          f"3c ...Auto is Mailbox/human_typing, nothing typed, both Queued (auto {r_auto}, states {states})")

    # 3e
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    for mode in ("Submit", "Auto"):
        stop = threading.Event()
        t = threading.Thread(target=typer, args=(stop, "input"))
        t.start()
        time.sleep(0.6)
        stop.set()
        t.join()
        start = len(rx_chunks(F_LOG))
        out = type_start(A, send(F, f"ACKED:{mode}", mode))
        time.sleep(1.0)
        acked = ask({"MsgAck": {"pane": F, "all": True}})
        r = wait_file(out, timeout=10)
        typed_any = any(f"ACKED:{mode}".encode() in c for _, c in rx_chunks(F_LOG)[start:])
        want = ("Mailbox", "already_read")
        check(outcome(r) == want and not typed_any,
              f"3e {mode}: acked while the guard held it is {want[1]}, nothing typed (ack {acked}, reply {r})")

    # 3f
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop, "input"))
    t.start()
    time.sleep(0.6)
    stop.set()
    t.join()
    requests[0] += 1
    n = requests[0]
    in_file, out_file = f"{RUNDIR}/in-{n}.json", f"{RUNDIR}/out-{n}.json"
    with open(in_file, "w") as f:
        json.dump(send(F, "GUARD:detached", "Submit"), f)
    line = f"timeout 0.5 python3 {WIRE_SEND} {srv.sock} {in_file} {out_file}\n"
    cli.send({"InputToPane": {"pane_id": A, "data": list(line.encode())}})
    at = None
    end = time.time() + 10
    while time.time() < end and at is None:
        at = chunk_at(F_LOG, "GUARD:detached")
        time.sleep(0.2)
    # The detach is logged at the disconnect; "(reply detached)" only once
    # the Enter is written, after the text this loop saw arrive.
    end = time.time() + 5
    log = srv.log()
    while time.time() < end and "(reply detached)" not in log:
        time.sleep(0.2)
        log = srv.log()
    check(at is not None and "submit reply(ies) of client_id=" in log and "(reply detached)" in log,
          f"3f the sender gave up mid-guard: still typed, its reply detached (typed {at is not None}, "
          f"detach logged {'submit reply(ies) of client_id=' in log}, answer logged {'(reply detached)' in log})")

    # 3g
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop, "input"))
    t.start()
    time.sleep(0.6)
    out = type_start(A, [send(F, "FIRE:one", "Submit"), send(F, "FIRE:two", "Submit")], fire=True)
    time.sleep(0.5)
    stop.set()
    t.join()
    replies = wait_file(out, timeout=15) or []
    outcomes = sorted(outcome(x) for x in replies)
    check(outcomes == sorted([("Error", "bad_request"), ("Submitted", None)]),
          f"3g a second send on a connection with one parked is bad_request (got {replies})")
    stored = [m["body"] for m in (read_box(F, all=True) or [])]
    check("FIRE:one" in stored and "FIRE:two" not in stored,
          f"3g ...and the refused send was never stored (bodies {stored})")

    # 3l
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    stop = threading.Event()
    t = threading.Thread(target=typer, args=(stop, "sendkey"))
    t.start()
    time.sleep(1.2)
    stop.set()
    t.join()
    r = type_send(A, send(F, "GUARD:sendkey", "Submit"))
    k, at = last_k(F_LOG), chunk_at(F_LOG, "GUARD:sendkey")
    gap = (at - k) / 1e6 if (k and at) else None
    check(submitted(r) and gap is not None and 2950 <= gap < 5000,
          f"3l a key binding's SendKey holds a submit like a keystroke: typed {gap} ms after it (reply {r})")

    # 3k: the person's key lands between the text and the Enter.
    time.sleep(3.5)
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    start = len(rx_chunks(F_LOG))
    out = type_start(A, send(F, "HUMAN:between", "Submit"))
    end = time.time() + 15
    while time.time() < end and not any(c.startswith(OPEN) for _, c in rx_chunks(F_LOG)[start:]):
        time.sleep(0.01)
    cli.send({"Input": {"data": list(b"k")}})
    r = wait_file(out, timeout=10)
    time.sleep(0.5)
    seq = [c for _, c in rx_chunks(F_LOG)[start:]]
    states = [m["state"] for m in (read_box(F, all=True) or []) if m["body"] == "HUMAN:between"]
    check(outcome(r) == ("Mailbox", "typed_over_read") and not any(b"\r" in c for c in seq)
          and any(c.endswith(b"k") for c in seq) and states == ["Submitted"],
          f"3k a key after the agent read the text stops the Enter; the key stays, and the message is the "
          f"agent's: Mailbox/typed_over_read, Submitted "
          f"(reply {r}, chunks {[c[:12] for c in seq]}, states {states})")

    check("panicked at" not in srv.log(), "3z no panic (attended)")
finally:
    srv.kill()

# --- D: submit_when_attended = false -----------------------------------------
try:
    srv = Server(RUNDIR)
    srv.start(config=CONFIG + T3.replace("typing_guard_max_ms = 4000", "typing_guard_max_ms = 4000\nsubmit_when_attended = false"))
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 160, "rows": 50}})
    cli.drain(0.8)
    A = cli_spawn("SplitRight")
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    time.sleep(0.8)
    check(focused_pane() == F, f"setup: %{F} is focused (focused {focused_pane()})")
    r_sub = type_send(A, send(F, "ATT:sub", "Submit"))
    r_auto = type_send(A, send(F, "ATT:auto", "Auto"))
    check(outcome(r_sub) == ("Error", "pane_attended") and outcome(r_auto) == ("Mailbox", "pane_attended"),
          f"3d submit_when_attended=false: an attended pane is refused / left in the mailbox (sub {r_sub}, auto {r_auto})")
    X = cli_spawn("SplitRight")
    time.sleep(0.8)
    check(focused_pane() == X and wait_idle(F), f"setup: %{F} is no longer focused (focused {focused_pane()})")
    r = type_send(A, send(F, "ATT:unattended", "Submit"))
    check(submitted(r), f"3d ...and once it is not attended it is typed (got {r})")
    check("panicked at" not in srv.log(), "3z no panic (submit_when_attended)")
finally:
    srv.kill()

# --- E: the checks at the writes, and panes that do not wait on each other ----
try:
    srv = Server(RUNDIR)
    srv.start(config=CONFIG + T3)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 160, "rows": 50}})
    cli.drain(0.8)
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    D, D_LOG, D_CTL = spawn_agent(paste=True)
    got = f"{RUNDIR}/wrap-got.txt"
    W = cli_spawn("SplitBelow", argv=[WRAPPER, got])
    A = cli_spawn("SplitRight")
    time.sleep(0.8)
    check(focused_pane() == A, f"setup: A is focused, so the agents are unattended (focused {focused_pane()})")

    # 3j: the agent draws a permission prompt the moment the paste arrives,
    # so the prompt is there by the Enter's check and was not at the sample.
    for mode, want in (("Submit", ("Mailbox", "agent_blocked_read")), ("Auto", ("Mailbox", "agent_blocked_read"))):
        check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
        tell(F_CTL, F_LOG, "block-on-rx")
        check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
        start = len(rx_chunks(F_LOG))
        r = type_send(A, send(F, f"PROMPT:{mode}", mode))
        time.sleep(0.5)
        seq = [c for _, c in rx_chunks(F_LOG)[start:]]
        states = [m["state"] for m in (read_box(F, all=True) or []) if m["body"] == f"PROMPT:{mode}"]
        check(outcome(r) == want and any(c.startswith(OPEN) for c in seq) and b"\r" not in seq
              and states == ["Submitted"],
              f"3j {mode}: a prompt drawn after the agent read the text gets no Enter, and the text is the "
              f"agent's: {want}, Submitted "
              f"(reply {r}, chunks {[c[:12] for c in seq]}, states {states})")
        tell(F_CTL, F_LOG, "clear")

    # 3m: a pane whose agent does not read holds only its own queue.
    tell(D_CTL, D_LOG, "trickle")
    check(wait_idle(D) and wait_idle(F), f"...%{D} and %{F} settled Idle before the next send")
    slow = type_start(A, send(D, "DEAF:held", "Submit"), background=True)
    end = time.time() + 8
    while time.time() < end and "to=%%%d bytes" % D not in srv.log():
        time.sleep(0.05)
    time.sleep(0.4)
    t0 = time.time()
    r = type_send(A, send(F, "OTHER:pane", "Submit"))
    took = time.time() - t0
    held = not os.path.exists(slow)
    check(submitted(r) and took < 1.5 and held,
          f"3m a submit to %{F} is answered in {took:.2f}s while %{D}'s Enter still waits "
          f"(reply {r}, %{D} still unanswered {held})")
    check(outcome(wait_file(slow, timeout=10)) == ("Error", "agent_did_not_read"),
          f"3m ...and %{D}'s own send ends agent_did_not_read")

    # 3n: a refusal before the text is written ends the pane's typing, so the
    # next send to it is looked at at all.
    first = type_send(A, send(W, "WRAP:first", "Submit"))
    second = type_send(A, send(W, "WRAP:second", "Submit"))
    check(outcome(first) == ("Error", "agent_not_reading") and outcome(second) == ("Error", "agent_not_reading"),
          f"3n a send after a refused one is answered too (first {first}, second {second})")
    check("panicked at" not in srv.log(), "3z no panic (writes)")
finally:
    srv.kill()

# --- B: the submit budget ------------------------------------------------------
# Read from the Rust source, so this case follows the constants it tests: the
# hold is the guard's cap plus one tick, and the budget adds the Enter delay's
# cap and the two read-check bounds.
SRC = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "src")
DAEMON = open(f"{SRC}/server/daemon.rs").read()
CFG_RS = open(f"{SRC}/config/messaging.rs").read()


def rs_ms(src, pattern, scale=1):
    m = re.search(pattern, src)
    assert m, pattern
    return int(m.group(1).replace("_", "")) * scale


BUDGET_S = (rs_ms(CFG_RS, r"MAX_TYPING_GUARD_MS: u64 = ([\d_]+);")
            + rs_ms(DAEMON, r"SUBMIT_TICK: std::time::Duration = std::time::Duration::from_millis\(([\d_]+)\)")
            + rs_ms(DAEMON, r"PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_millis\(([\d_]+)\)")
            + rs_ms(CFG_RS, r"MAX_ENTER_DELAY_MS: u64 = ([\d_]+);")
            + rs_ms(DAEMON, r"CONSUME_DEADLINE: std::time::Duration = std::time::Duration::from_secs\(([\d_]+)\)", 1000)
            + rs_ms(DAEMON, r"CANCEL_ACK: std::time::Duration = std::time::Duration::from_millis\(([\d_]+)\)")) / 1000
try:
    srv = Server(RUNDIR)
    srv.start(config=CONFIG + """
[messaging.submit.fakeagent]
while_working = "queue"
enter_delay_ms = 5000
""")
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 160, "rows": 50}})
    cli.drain(0.8)
    A = cli_spawn("SplitRight")
    A2 = cli_spawn("SplitRight")
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    # Reads each message's probe whole and its text a byte at a time, slower
    # than the Enter delay and the read check, so every send runs the whole
    # read check out without ever stalling.
    tell(F_CTL, F_LOG, "trickle")
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")

    # 3i: six connections at once, each its own wire_send in the background,
    # three from each of two panes: one sender may hold only half a queue.
    outs, parts = [], {A: [], A2: []}
    for i in range(6):
        requests[0] += 1
        n = requests[0]
        in_file, out_file = f"{RUNDIR}/in-{n}.json", f"{RUNDIR}/out-{n}.json"
        with open(in_file, "w") as f:
            json.dump(send(F, f"SLOW:{i}", "Submit"), f)
        outs.append(out_file)
        parts[A if i % 2 else A2].append(f"python3 {WIRE_SEND} {srv.sock} {in_file} {out_file} &")
    t0 = time.time()
    for sender, jobs in parts.items():
        cli.send({"InputToPane": {"pane_id": sender, "data": list((" ".join(jobs) + "\n").encode())}})
    arrived = {}
    end = t0 + BUDGET_S + 15
    while time.time() < end and len(arrived) < len(outs):
        for out in outs:
            if out not in arrived and os.path.exists(out):
                arrived[out] = (time.time() - t0, json.load(open(out)))
        time.sleep(0.1)
    outcomes = sorted(outcome(r) for _, r in arrived.values())
    latest = max((t for t, _ in arrived.values()), default=None)
    states = [m["state"] for m in (read_box(F) or []) if m["body"].startswith("SLOW:")]
    check(outcomes == sorted([("Error", "agent_did_not_read")] * 5 + [("Error", "submit_timeout")]),
          f"3i the sixth send reaches the head past the hold and is answered submit_timeout (got {outcomes})")
    # Two seconds of slack for six pythons starting in the pane's shell. With
    # the hold off, the sixth is answered about 3.4 s past the budget.
    check(latest is not None and latest <= BUDGET_S + 2.0,
          f"3i ...every answer arrives inside the {BUDGET_S:.2f}s budget (last at {latest})")
    check(states == ["Queued"] * 6, f"3i ...and every message is still Queued in the mailbox (got {states})")
    check("panicked at" not in srv.log(), "3z no panic (budget)")
finally:
    srv.kill()



# --- S: prompts no pattern names, and text nobody read --------------------------
AGENTS_RS = open(f"{SRC}/config/agents.rs").read()
CLAUDE_PATTERNS = re.findall(r'p\(\s*"(claude-[\w-]+)",\s*"claude",\s*r"([^"]*)",?\s*\)', AGENTS_RS)
try:
    srv = Server(RUNDIR)
    srv.start(config=CONFIG + T3)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 200, "rows": 60}})
    cli.drain(0.8)
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    A = cli_spawn("SplitRight")
    time.sleep(0.8)
    check(focused_pane() == A, f"setup: A is focused, so the agent is unattended (focused {focused_pane()})")

    # 3o
    tell(F_CTL, F_LOG, "editblock")
    idle = wait_idle(F)
    start = len(rx_chunks(F_LOG))
    r_sub = type_send(A, send(F, "EDIT:sub", "Submit"))
    r_auto = type_send(A, send(F, "EDIT:auto", "Auto"))
    states = {m["body"]: m["state"] for m in (read_box(F, all=True) or []) if m["body"].startswith("EDIT:")}
    check(idle and outcome(r_sub) == ("Error", "agent_blocked") and outcome(r_auto) == ("Mailbox", "agent_blocked")
          and no_new_chunks(F_LOG, start) and states == {"EDIT:sub": "Queued", "EDIT:auto": "Queued"},
          f"3o an edit-approval dialog no pattern names (the panel reads Idle: {idle}) is not typed into "
          f"(submit {r_sub}, auto {r_auto}, states {states})")
    tell(F_CTL, F_LOG, "clear")

    # 3p
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    tell(F_CTL, F_LOG, "editblock-on-rx")
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    start = len(rx_chunks(F_LOG))
    r = type_send(A, send(F, "EDIT:after", "Submit"))
    time.sleep(0.5)
    seq = [c for _, c in rx_chunks(F_LOG)[start:]]
    check(outcome(r) == ("Mailbox", "agent_blocked_read") and any(c.startswith(OPEN) for c in seq)
          and b"\r" not in seq,
          f"3p an edit-approval dialog drawn after the text gets no Enter (reply {r}, chunks {[c[:12] for c in seq]})")
    tell(F_CTL, F_LOG, "clear")

    # 3x: the probe is its own read, ahead of the rest.
    check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
    start = len(raw_chunks(F_LOG))
    r = type_send(A, send(F, "PROBE:alone", "Submit"))
    raw = [c for _, c in raw_chunks(F_LOG)[start:]]
    check(submitted(r) and len(raw) >= 3 and raw[0] == OPEN and b"PROBE:alone" in raw[1] and raw[2] == b"\r",
          f"3x the probe is read alone, then the text, then the Enter (reply {r}, chunks {[c[:12] for c in raw]})")

    # 3v: a person's Enter after the agent read the text, inside the delay.
    for mode, want in (("Submit", ("Mailbox", "typed_over_read")), ("Auto", ("Mailbox", "typed_over_read"))):
        check(wait_idle(F), f"...the agent in %{F} settled Idle before the next send")
        start = len(rx_chunks(F_LOG))
        body = f"READ:{mode}"
        out = type_start(A, send(F, body, mode))
        end = time.time() + 15
        while time.time() < end and not any(body.encode() in c for _, c in rx_chunks(F_LOG)[start:]):
            time.sleep(0.01)
        cli.send({"InputToPane": {"pane_id": F, "data": list(b"\r")}})
        r = wait_file(out, timeout=10)
        time.sleep(0.5)
        seq = [c for _, c in rx_chunks(F_LOG)[start:]]
        states = [m["state"] for m in (read_box(F, all=True) or []) if m["body"] == body]
        check(outcome(r) == want and sum(c.count(b"\r") for c in seq) == 1 and states == ["Submitted"],
              f"3v {mode}: a person's Enter after the agent read the text: {want}, one Enter (theirs), "
              f"Submitted (reply {r}, chunks {[c[:12] for c in seq]}, states {states})")

    # 3y: rv-a's C1. An agent that exits after a Submitted send.
    Y, Y_LOG, Y_CTL = spawn_agent(paste=True)
    r = type_send(A, send(Y, "THEN:exit", "Submit"))
    time.sleep(0.5)
    tell(Y_CTL, Y_LOG, "quit", timeout=2.0)
    check(submitted(r) and wait_closed(Y),
          f"3y an agent that exits after a Submitted send: its pane closes (reply {r})")

    # 3ab: sec-b's P7. A key part-way through a long body read slowly.
    W7, W7_LOG, W7_CTL = spawn_agent(paste=True)
    tell(W7_CTL, W7_LOG, "slow:16:30")
    out = type_start(A, send(W7, "Z" * 6000, "Submit"))
    end = time.time() + 20
    while time.time() < end and sum(c.count(b"Z") for _, c in raw_chunks(W7_LOG)) < 300:
        time.sleep(0.01)
    cli.send({"InputToPane": {"pane_id": W7, "data": list(b"k")}})
    r = wait_file(out, timeout=15)
    time.sleep(1.5)
    stream = b"".join(c for _, c in raw_chunks(W7_LOG))
    k = stream.find(b"k")
    withdrawn = (re.findall(r"its text is withdrawn, (\d+) unread byte", srv.log()) or [None])[-1]
    check(outcome(r) == ("Error", "human_typing") and k > 0 and b"Z" not in stream[k:]
          and stream[:k].endswith(CLOSE) and withdrawn is not None and int(withdrawn) > 0,
          f"3ab a key part-way through a slowly read body: no body byte after it, the paste closed before it, "
          f"the count logged (reply {r}, {stream.count(b'Z')} Z read, before the key {stream[max(0, k - 8):k]!r}, "
          f"after it {stream[k:k + 8]!r}, logged {withdrawn} withdrawn)")

    # 3ac: sec-b's P9. A key between the probe's read and the body's write.
    W9, W9_LOG, W9_CTL = spawn_agent(paste=True)
    out = type_start(A, send(W9, "NINE", "Submit"))
    end = time.time() + 10
    while time.time() < end and not raw_chunks(W9_LOG):
        time.sleep(0.001)
    cli.send({"InputToPane": {"pane_id": W9, "data": list(b"k")}})
    r = wait_file(out, timeout=30)
    time.sleep(0.5)
    stream = b"".join(c for _, c in raw_chunks(W9_LOG))
    k = stream.find(b"k")
    check(k > 0 and stream[:k].count(OPEN) == stream[:k].count(CLOSE),
          f"3ac a key right after the probe was read is not inside an open paste "
          f"(reply {r}, stream {stream[:k + 1]!r})")

    check("panicked at" not in srv.log(), "3z no panic (prompts)")
finally:
    srv.kill()

# 3q: the shipped claude patterns, re-aimed at the fakeagent.
try:
    shipped = "".join(f"""
  [[agents.pattern]]
  name = "{name}"
  command = "fakeagent"
  regex = '{regex}'
""" for name, regex in CLAUDE_PATTERNS)
    srv = Server(RUNDIR)
    srv.start(config=CONFIG.split("  [[agents.pattern]]")[0] + shipped)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 200, "rows": 60}})
    cli.drain(0.8)
    F, F_LOG, F_CTL = spawn_agent(paste=True)
    tell(F_CTL, F_LOG, "editblock")
    check(len(CLAUDE_PATTERNS) >= 2 and wait_state(F, "NeedsInput"),
          f"3q the shipped claude patterns ({[n for n, _ in CLAUDE_PATTERNS]}) read the edit dialog as NeedsInput "
          f"(state {status_of(F)})")
finally:
    srv.kill()

# 3r / 3s: bash runs the agent; the agent reads nothing, then exits.
try:
    srv = Server(RUNDIR)
    srv.start(config=CONFIG + T3)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 200, "rows": 60}})
    cli.drain(0.8)
    A = cli_spawn("SplitRight")
    for case, human in (("3r", True), ("3s", False)):
        marker = f"{RUNDIR}/ran-{case}"
        log, ctl = f"{RUNDIR}/bash-agent-{case}.log", f"{RUNDIR}/bash-agent-{case}.ctl"
        B = cli_spawn("SplitBelow", argv=["bash", "--norc", "--noprofile", "-i"])
        time.sleep(1.0)
        cli.send({"InputToPane": {"pane_id": B, "data": list(f"{FAKEAGENT} {log} {ctl}\r".encode())}})
        end = time.time() + 8
        while time.time() < end and not os.path.exists(log):
            time.sleep(0.05)
        tell(ctl, log, "paste-on")
        check(wait_idle(B) and focused_pane() == B,
              f"setup {case}: bash runs the agent in %{B}, which is focused (focused {focused_pane()})")
        # Out of the typing guard's attended window, which the typed command armed.
        time.sleep(3.5)
        # 3r's agent takes the probe and reads the body slowly, so the body
        # is still being read when the person's Enter arrives; 3s's reads
        # nothing at all.
        tell(ctl, log, "trickle" if human else "deaf")
        out = type_start(A, send(B, f"touch {marker}", "Submit"))
        if human:
            end = time.time() + 15
            while time.time() < end and "to=%%%d bytes" % B not in srv.log():
                time.sleep(0.05)
            time.sleep(1.2)
            cli.send({"Input": {"data": list(b"\r")}})
        r = wait_file(out, timeout=15)
        tell(ctl, log, "quit")
        time.sleep(2.0)
        want = ("Error", "human_typing") if human else ("Error", "agent_did_not_read")
        check(outcome(r) == want and not os.path.exists(marker),
              f"{case} {'a person pressed Enter after the text, then ' if human else ''}the agent that read "
              f"nothing exits: bash does not run the body (reply {r}, ran {os.path.exists(marker)})")
        if human:
            check("its text is withdrawn" in srv.log(),
                  f"{case} ...and the withdrawal is logged")

    def bash_agent(case, mode):
        """bash in a new pane B running the fakeagent, settled and in `mode`."""
        log, ctl = f"{RUNDIR}/bash-agent-{case}.log", f"{RUNDIR}/bash-agent-{case}.ctl"
        B = cli_spawn("SplitBelow", argv=["bash", "--norc", "--noprofile", "-i"])
        time.sleep(1.0)
        cli.send({"InputToPane": {"pane_id": B, "data": list(f"{FAKEAGENT} {log} {ctl}\r".encode())}})
        end = time.time() + 8
        while time.time() < end and not os.path.exists(log):
            time.sleep(0.05)
        tell(ctl, log, "paste-on")
        check(wait_idle(B), f"setup {case}: bash runs the agent in %{B}")
        time.sleep(3.5)
        tell(ctl, log, mode)
        return B, log, ctl

    def stored(pane, timeout=15):
        """The time the server logged storing a send to `pane`."""
        end = time.time() + timeout
        while time.time() < end and "to=%%%d bytes" % pane not in srv.log():
            time.sleep(0.02)
        return time.time()

    def ran(marker, tag):
        try:
            return open(marker).read() == f"RAN:{tag}"
        except FileNotFoundError:
            return False

    # 3t / 3u: sec-a's P5 and P5c. The marker's content is assembled by the
    # shell, so text that merely reached bash's line does not count.
    for case, human in (("3t", True), ("3u", False)):
        marker = f"{RUNDIR}/ran-{case}"
        B, log, ctl = bash_agent(case, "deaf")
        out = type_start(A, send(B, f"printf 'RAN:%s' {case} > {marker}", "Submit"))
        t0 = stored(B)
        if human:
            time.sleep(max(0.0, t0 + 1.2 - time.time()))
            cli.send({"InputToPane": {"pane_id": B, "data": list(b"\r")}})
        time.sleep(max(0.0, t0 + 1.4 - time.time()))
        tell(ctl, log, "quit", timeout=2.0)
        r = wait_file(out, timeout=15)
        time.sleep(1.5)
        before = ran(marker, case)
        cli.send({"InputToPane": {"pane_id": B, "data": list(b"\r")}})
        time.sleep(1.5)
        check(name_of(r) == "Error" and not before and not ran(marker, case),
              f"{case} {'a person Enter at 1.2 s, ' if human else ''}the agent that reads nothing exits at 1.4 s: "
              f"bash runs nothing, then or on the next Enter (reply {r}, ran {before}, "
              f"ran after Enter {ran(marker, case)})")

    # 3w: the agent takes the probe and hangs; a person's key; it exits.
    marker = f"{RUNDIR}/ran-3w"
    B, log, ctl = bash_agent("3w", "trickle")
    out = type_start(A, send(B, f"printf 'RAN:%s' 3w > {marker}", "Submit"))
    t0 = stored(B)
    time.sleep(max(0.0, t0 + 1.2 - time.time()))
    cli.send({"InputToPane": {"pane_id": B, "data": list(b"\r")}})
    time.sleep(0.2)
    tell(ctl, log, "quit", timeout=2.0)
    r = wait_file(out, timeout=15)
    time.sleep(1.5)
    check(outcome(r) == ("Error", "human_typing") and not ran(marker, "3w"),
          f"3w a person's key while the text is unread, then the agent exits: human_typing, and bash runs "
          f"nothing (reply {r}, ran {ran(marker, '3w')})")

    # 3aa: sec-b's P6. The agent takes the probe, hangs, and exits 0.5 s
    # after: a multi-line body would run line by line in bash.
    marker = f"{RUNDIR}/ran-3aa"
    B, log, ctl = bash_agent("3aa", "deaf-on-rx")
    body = f"printf 'RAN:%s' 3aa > {marker}\nprintf 'RAN:%s' 3aa > {marker}"
    out = type_start(A, send(B, body, "Submit"))
    end = time.time() + 10
    while time.time() < end and not raw_chunks(log):
        time.sleep(0.005)
    time.sleep(0.5)
    tell(ctl, log, "quit", timeout=2.0)
    r = wait_file(out, timeout=15)
    time.sleep(1.5)
    before = ran(marker, "3aa")
    cli.send({"InputToPane": {"pane_id": B, "data": list(b"\r")}})
    time.sleep(1.5)
    check(name_of(r) == "Error" and not before and not ran(marker, "3aa"),
          f"3aa an agent that takes the probe, hangs and exits: no line of a multi-line body runs in bash, "
          f"then or on the next Enter (reply {r}, ran {before}, ran after Enter {ran(marker, '3aa')})")

    # 3ad: the agent reads the whole body and exits before its Enter. Nothing
    # is left to flush, and nothing can tell its read from the shell's.
    marker = f"{RUNDIR}/ran-3ad"
    B, log, ctl = bash_agent("3ad", "exit-after-body")
    r = type_send(A, send(B, f"printf 'RAN:%s' 3ad > {marker}", "Auto"))
    check(outcome(r) == ("Mailbox", "read_by_another_program")
          and "nothing was left to flush: another program may have read it" in srv.log(),
          f"3ad an agent that read the body and exited before its Enter: Mailbox/read_by_another_program, "
          f"and a WARN (reply {r})")
    check("panicked at" not in srv.log(), "3z no panic (bash)")
finally:
    srv.kill()


print()
if failures:
    print(f"{len(failures)} FAILED")
    sys.exit(1)
print("ALL PASS")
