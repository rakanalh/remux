#!/usr/bin/env python3
"""The attribution line a typed message opens with is inert at every shell.

An agent that dies right after reading a message's probe can leave the
first line of the typed text at the prompt of the shell behind it, where
Enter runs it. That line is the attribution line, so whatever a session
name or a body holds must not run from it, in any shell, Enter or not.

The sender sits in a session whose name is shell syntax, `$(touch <file>)`.
The line is read back off a fakeagent's terminal, exactly as the server
typed it, then typed into each real shell this host has, with Enter:

sa1. The line is `message #N, reply with remux msg send --reply-to N, from
     <name> (<session>)`: N is the server's number for the message (its
     answer to the send), the name the one the server holds for the sender
     (`agent status`), and the session is shown with everything outside
     [A-Za-z0-9._-] escaped as \\xNN, spaces included. The reply clause
     comes before the session, and appears once.
sa2. Typed with Enter into `sh -i`, `bash -i`, `zsh -f -i` (where `#` is
     NOT a comment) and `zsh -f -i` with interactive_comments set, it runs
     nothing the session holds: the file is never created.
sa3. A body sent with Submit to an agent whose terminal cannot take a
     pasted block is never typed (the attribution and a body on one line is
     not inert at zsh): Submit is refused paste_unavailable, Auto stores it
     with that reason, the agent reads nothing, and the body is in its
     mailbox. Whatever WAS typed for it, Entered into each shell, runs
     nothing: on a build that typed the attribution and a body
     `x; printf 'RAN:%s' y` on one line, zsh runs the printf.
sa4. The same with `bracketed = "on"` forcing a wrap over that agent.
saz. No panic.

Run: python3 tests/frame/msg_attribution_shell.py   (RMX_RUNDIR overrides)
"""
import json
import os
import re
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402
import fakeagent  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-attrshell")
BINDIR = RUNDIR + "-bin"
FAKEAGENT = f"{BINDIR}/fakeagent"
WIRE_SEND = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wire_send.py")
OPEN, CLOSE = b"\x1b[200~", b"\x1b[201~"
MARK = f"{RUNDIR}-subst"
SESSION = f"$(touch {MARK})"
BODY = "x; printf 'RAN:%s' y > " + RUNDIR + "-ran"
CONFIG = """
[agents]
commands = ["fakeagent"]
working_ms = 500
"""

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


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


def cli_spawn(session, argv=None):
    body = {"session": session, "placement": "SplitBelow"}
    if argv is not None:
        body["argv"] = argv
    reply = ask({"CliSpawn": body})
    return reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None


def status_of(pane):
    reply = ask({"AgentStatus": {"target": {"host": None, "target": {"Pane": pane}}}})
    rows = reply["AgentStatusList"]["panes"] if name_of(reply) == "AgentStatusList" else []
    return rows[0] if rows else {}


requests = [0]


def type_send(pane, to, body, deliver):
    requests[0] += 1
    n = requests[0]
    in_file, out_file = f"{RUNDIR}/in-{n}.json", f"{RUNDIR}/out-{n}.json"
    with open(in_file, "w") as f:
        json.dump({"MsgSend": {"to": {"host": None, "target": {"Pane": to}}, "body": body,
                               "deliver": deliver}}, f)
    line = f"python3 {WIRE_SEND} {srv.sock} {in_file} {out_file}\n"
    cli.send({"InputToPane": {"pane_id": pane, "data": list(line.encode())}})
    end = time.time() + 15
    while time.time() < end:
        if os.path.exists(out_file):
            with open(out_file) as f:
                return json.load(f)
        time.sleep(0.05)
    return None


def raw(log):
    try:
        lines = open(log).read().splitlines()
    except FileNotFoundError:
        return b""
    return b"".join(bytes.fromhex(ln.split()[2]) for ln in lines if ln.startswith("RX ") and len(ln.split()) > 2)


def spawn_agent(paste):
    n = len(os.listdir(RUNDIR))
    log, ctl = f"{RUNDIR}/agent-{n}.log", f"{RUNDIR}/agent-{n}.ctl"
    pane = cli_spawn("main", [FAKEAGENT, log, ctl])
    end = time.time() + 8
    while time.time() < end and not os.path.exists(log):
        time.sleep(0.05)
    if paste:
        fakeagent.tell(ctl, log, "paste-on")
    end = time.time() + 6
    while time.time() < end and status_of(pane).get("state") != "Idle":
        time.sleep(0.1)
    return pane, log


def strict(session):
    return "".join(c if re.fullmatch(r"[A-Za-z0-9._-]", c) else
                   (f"\\x{ord(c):02x}" if ord(c) < 0x80 else f"\\u{{{ord(c):x}}}") for c in session)


def outcome(reply):
    if name_of(reply) == "Error":
        return ("Error", reply["Error"].get("code"))
    sent = (reply or {}).get("MsgSent") or {}
    return (sent.get("delivery"), sent.get("reason"))


def start_server(extra=""):
    global srv, cli
    srv = Server(RUNDIR)
    srv.start(config=CONFIG + extra)
    cli = Client(srv.sock)
    cli.hello()
    for name in ("main", SESSION):
        cli.send({"CreateSession": {"name": name, "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 200, "rows": 60}})
    cli.drain(0.8)


os.makedirs(BINDIR, exist_ok=True)
fakeagent.install(FAKEAGENT)
for leftover in (MARK, RUNDIR + "-ran"):
    if os.path.exists(leftover):
        os.remove(leftover)

try:
    start_server()
    S = cli_spawn(SESSION)
    F, F_LOG = spawn_agent(paste=True)
    time.sleep(0.5)

    # --- sa1 ----------------------------------------------------------------
    r = type_send(S, F, BODY, "Submit")
    sid = ((r or {}).get("MsgSent") or {}).get("id")
    end = time.time() + 8
    while time.time() < end and CLOSE not in raw(F_LOG):
        time.sleep(0.05)
    typed = raw(F_LOG)
    inner = typed.split(OPEN, 1)[1].split(CLOSE, 1)[0] if OPEN in typed and CLOSE in typed else b""
    line = inner.split(b"\n", 1)[0].decode("utf-8", "replace")
    who = status_of(S)
    want = (f"message #{sid}, reply with remux msg send --reply-to {sid}, "
            f"from {who.get('name')} ({strict(who.get('session') or '')})")
    check(sid is not None and who.get("session") == SESSION and line == want
          and line.count("--reply-to") == 1 and inner.endswith(BODY.encode()),
          f"sa1 the typed line is the server's, its session strictly escaped (got {line!r}, want {want!r})")

    # --- sa2 ----------------------------------------------------------------
    shells = [("sh", ["sh", "-i"], ""), ("bash", ["bash", "--norc", "--noprofile", "-i"], ""),
              ("zsh", ["zsh", "-f", "-i"], ""),
              ("zsh+interactive_comments", ["zsh", "-f", "-i"], "setopt interactive_comments\r")]
    for label, argv, setup in shells:
        if not shutil.which(argv[0]):
            print(f"      skipped, {argv[0]} is not installed")
            continue
        if os.path.exists(MARK):
            os.remove(MARK)
        sh = cli_spawn("main", argv)
        time.sleep(1.0)
        if setup:
            cli.send({"InputToPane": {"pane_id": sh, "data": list(setup.encode())}})
            time.sleep(0.3)
        cli.send({"InputToPane": {"pane_id": sh, "data": list(line.encode() + b"\r")}})
        time.sleep(1.5)
        check(line and not os.path.exists(MARK),
              f"sa2 {label}: the attribution line, Entered, runs nothing its session holds")

    # --- sa3 ----------------------------------------------------------------
    P, P_LOG = spawn_agent(paste=False)
    before = raw(P_LOG)
    r_sub = type_send(S, P, BODY, "Submit")
    r_auto = type_send(S, P, BODY + " again", "Auto")
    time.sleep(1.0)
    box = ask({"MsgRead": {"pane": P, "peek": True, "all": True}})
    bodies = [m["body"] for m in (box or {}).get("MsgList", {}).get("messages", [])]
    check(outcome(r_sub) == ("Error", "paste_unavailable") and outcome(r_auto) == ("Mailbox", "paste_unavailable")
          and raw(P_LOG) == before and bodies == [BODY, BODY + " again"],
          f"sa3 an agent that cannot take a pasted block is typed nothing; both are in its mailbox "
          f"(submit {r_sub}, auto {r_auto}, read {raw(P_LOG)[len(before):]!r}, mailbox {bodies})")
    # Whatever the server did type for that agent (nothing, now; the
    # attribution and the body on one line, before), Entered into each shell
    # as a shell left holding it would run it.
    leftover = raw(P_LOG)[len(before):].replace(OPEN, b"").replace(CLOSE, b"").split(b"\r")[0]
    for label, argv, setup in shells:
        if not shutil.which(argv[0]):
            continue
        sh = cli_spawn("main", argv)
        time.sleep(1.0)
        if setup:
            cli.send({"InputToPane": {"pane_id": sh, "data": list(setup.encode())}})
            time.sleep(0.3)
        cli.send({"InputToPane": {"pane_id": sh, "data": list(leftover + b"\r")}})
        time.sleep(1.5)
        check(not os.path.exists(RUNDIR + "-ran") and not os.path.exists(MARK),
              f"sa3 {label}: what was typed for that agent, Entered, runs nothing (typed {leftover!r})")
        for left in (RUNDIR + "-ran", MARK):
            if os.path.exists(left):
                os.remove(left)
    check("panicked at" not in srv.log(), "saz no panic")
finally:
    srv.kill()

# --- sa4: a forced wrap over a program with paste off ------------------------
try:
    start_server('\n[messaging.submit.fakeagent]\nbracketed = "on"\n')
    S = cli_spawn(SESSION)
    P, P_LOG = spawn_agent(paste=False)
    before = raw(P_LOG)
    r = type_send(S, P, BODY, "Submit")
    time.sleep(1.0)
    check(outcome(r) == ("Error", "paste_unavailable") and raw(P_LOG) == before,
          f"sa4 bracketed = on over paste off is refused too, nothing typed (reply {r})")
    check("panicked at" not in srv.log(), "saz no panic (forced wrap)")
finally:
    srv.kill()

for leftover in (MARK, RUNDIR + "-ran"):
    if os.path.exists(leftover):
        os.remove(leftover)
print()
print("ALL PASS" if not failures else f"{len(failures)} FAILED")
sys.exit(1 if failures else 0)
