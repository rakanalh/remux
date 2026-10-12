#!/usr/bin/env python3
"""Frame-level test: the server log must not contain what the user TYPED.

`server.log` is plaintext, unrotated and never pruned -- one user's reached
586 MB in about a day -- and `main.rs` pins the logger at `Debug` with no
`RUST_LOG`, so nothing a user can set turns any of this off. Whatever reaches
it stays on disk indefinitely.

`handle_client_message` logs a summary line for every inbound message. Its
`Input` arm is careful: it prints `Input(N bytes)`, the COUNT and never the
content. Two later message types were not given the same treatment and fell
through to the catch-all `other => log::debug!("... msg={other:?}")`, which
Debug-prints the whole struct:

  1. **`InputToPane { pane_id, data }`** -- how every keystroke reaches a VIEW
     cell and how the `files` panel forwards keys. A password, an API key or an
     SSH passphrase typed into a view landed in the log verbatim.
  2. **`CliSpawn { session, placement, argv, cwd }`** -- a command line the user
     typed, e.g. `remux split -- psql postgresql://user:pass@host/db`. Logged
     TWICE: once by the catch-all and once by `handle_cli_spawn`'s own `info!`.

Both are asserted here two ways round, because "the secret is absent" passes
perfectly on a server that logged NOTHING at all: the redacted form must also
be PRESENT, naming the message and the byte/argument count.

Run: python3 tests/frame/log_redaction.py
"""
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Client, Server, name_of  # noqa: E402
import fakeagent  # noqa: E402

RUNDIR = "/tmp/rmx-logredact"
FAILURES = []

# Distinctive enough that a match cannot be a coincidence, and shaped like the
# thing that actually matters. Neither is ever sent as a whole shell command, so
# neither can arrive in the log by some route other than the one under test.
TYPED_SECRET = b"correct-horse-battery-staple-KEYSTROKE"
ARGV_SECRET = "postgresql://sam:hunter2@db.internal/prod-ARGV"
# Yanked to the clipboard. Assembled by `printf` INSIDE the pane rather than
# typed, so the string on screen and the string in the typed command never
# coincide -- a marker that appears in the command line would be "on screen"
# whether or not the yank did anything.
CLIP_HEAD, CLIP_TAIL = "swordfish", "CLIPBOARD"
CLIP_SECRET = CLIP_HEAD + "-" + CLIP_TAIL
# A window title, which an agent fills with a summary of what the user asked
# it. Assembled by `printf` in the pane for the same reason as the clipboard.
TITLE_HEAD, TITLE_TAIL = "rotate", "prod-credentials-TITLE"
TITLE_SECRET = TITLE_HEAD + "-" + TITLE_TAIL
# A message body. It travels in a request FILE read by wire_send.py inside the
# pane, so it is in no typed bytes and on no command line: the only way into
# the log is the server writing it.
MSG_SECRET = "the-deploy-token-is-swordfish-BODY"
# A body given to `remux msg send` on its command line, which writes the
# CLIENT log. Distinct from MSG_SECRET so a hit names which half leaked.
CLI_MSG_SECRET = "the-vault-pin-is-8812-CLI-BODY"
# A body TYPED into an agent's pane by the server (`deliver: Submit`). The
# bytes the server writes into the pane are the body, framed, so logging them
# in any form (raw, escaped or as hex) is the leak this case looks for.
SUBMIT_SECRET = "the-signing-key-is-lobster-SUBMIT"
# A body the CLI sends with `--deliver submit --wait`, typed by the server:
# both the client's and the server's log could hold it.
CLI_SUBMIT_SECRET = "the-badge-code-is-3141-CLI-SUBMIT"
# Outside the run dir, which `Server.start` wipes.
FAKEAGENT = f"{RUNDIR}-bin/fakeagent"
CONFIG = """
[agents]
commands = ["fakeagent"]
"""
WIRE_SEND = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wire_send.py")


def check(cond, msg):
    if cond:
        print(f"  PASS  {msg}")
    else:
        print(f"  FAIL  {msg}")
        FAILURES.append(msg)


def first_pane_name(c):
    """The first pane's name, read off the session tree."""
    c.send("ListSessionTree")
    for msg in c.drain(1.0):
        if name_of(msg) == "SessionTree":
            for sess in msg["SessionTree"].get("unfiled") or []:
                for tab in sess.get("tabs") or []:
                    for pane in tab.get("panes") or []:
                        return pane["name"]
    return None


def first_pane_id(c):
    """A pane id, read off the session tree rather than assumed to be 1."""
    c.send("ListSessionTree")
    for msg in c.drain(1.5):
        if name_of(msg) != "SessionTree":
            continue
        tree = msg["SessionTree"]
        for bucket in ("unfiled",):
            for sess in tree.get(bucket) or []:
                for tab in sess.get("tabs") or []:
                    for pane in tab.get("panes") or []:
                        return pane["id"]
        for folder in tree.get("folders") or []:
            for sess in folder.get("sessions") or []:
                for tab in sess.get("tabs") or []:
                    for pane in tab.get("panes") or []:
                        return pane["id"]
    return None


class Grid:
    """The composited frame, rebuilt from `FullRender` + `RenderDiff`.

    Fed by EVERY drain in `run`, not just the one next to the check that needs
    it: the baseline `FullRender` arrives once, right after `Attach`, and an
    earlier `drain` that discarded it left the grid permanently empty -- which
    reported the marker "not on screen" while it was plainly there.
    """

    def __init__(self):
        self.rows = []

    def pump(self, c, t=0.4):
        msgs = c.drain(t)
        for m in msgs:
            n = name_of(m)
            if n == "FullRender":
                self.rows = [
                    "".join(cell["c"] for cell in row) for row in m["FullRender"]["cells"]
                ]
            elif n == "RenderDiff" and self.rows:
                for ch in m["RenderDiff"]["changes"]:
                    y, x = ch["y"], ch["x"]
                    if y < len(self.rows) and x < len(self.rows[y]):
                        row = self.rows[y]
                        self.rows[y] = row[:x] + ch["cell"]["c"] + row[x + 1 :]
        return msgs

    def last_row_with(self, text):
        """The LAST row carrying `text`.

        Last, not first: the shell echoes the command that produces the marker,
        so the echo row and the output row both match, and it is the program's
        own output that should be selected.
        """
        return max((y for y, r in enumerate(self.rows) if text in r), default=None)


def run(srv, result):
    c = Client(srv.sock)
    c.hello()
    c.send({"CreateSession": {"name": "main", "folder": None}})
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": 100, "rows": 30}})
    grid = Grid()
    grid.pump(c, 0.6)

    pane_id = first_pane_id(c)
    check(pane_id is not None, "a pane id was resolved off the session tree")
    result["pane_id"] = pane_id
    if pane_id is None:
        return

    # 4. A CLIPBOARD selection, FIRST -- before anything below moves the focus.
    #    `CliSpawn` creates a pane and a tab, and `Input` goes to whatever is
    #    focused, so running this last typed the marker into a pane that was no
    #    longer on screen and the row scan found nothing.
    #    `CopyToClipboard { data }` carries a selection
    #    the user made, which is content by the same rule -- and a selection is
    #    exactly where a pasted password ends up. Asserted rather than reasoned
    #    about, so that if a future `{:?}` is ever added to that path it goes
    #    red here instead of shipping.
    c.send({"Input": {"data": list(f"printf '%s-%s\\n' {CLIP_HEAD} {CLIP_TAIL}\n".encode())}})
    grid.pump(c, 1.2)
    marker_y = grid.last_row_with(CLIP_SECRET)
    result["marker_row"] = marker_y
    if marker_y is not None:
        # Click, then a non-final drag to set the anchor, then the final drag --
        # the gesture a real mouse makes. A lone final drag establishes no
        # anchor and yanks nothing.
        c.send({"MouseClick": {"x": 1, "y": marker_y}})
        grid.pump(c, 0.2)
        c.send({"MouseDrag": {"start_x": 1, "start_y": marker_y,
                              "end_x": 40, "end_y": marker_y, "is_final": False}})
        grid.pump(c, 0.2)
        c.send({"MouseDrag": {"start_x": 1, "start_y": marker_y,
                              "end_x": 40, "end_y": marker_y, "is_final": True}})
    # Kept, because it is the ONLY positive proof this case has: the clipboard
    # path logs nothing about the data and never did, so there is no redacted
    # form to look for. Without evidence that the server really did extract and
    # send the selection, "the marker is in neither log" is satisfied by a drag
    # that selected nothing at all.
    yanked = [
        m["CopyToClipboard"]["data"]
        for m in grid.pump(c, 0.8)
        if name_of(m) == "CopyToClipboard"
    ]
    result["yanked"] = any(CLIP_SECRET in d for d in yanked)

    # 6. A window title set by a program. It becomes the pane's name, so it is
    #    shown everywhere, and must still never be written to the log.
    c.send({"Input": {"data": list(
        f"sh -c 'printf \"\\033]2;%s-%s\\007\" {TITLE_HEAD} {TITLE_TAIL}; sleep 3'\n".encode())}})
    grid.pump(c, 2.0)
    result["title_named"] = first_pane_name(c) == TITLE_SECRET
    grid.pump(c, 2.0)

    # 7. A message body, sent by a process INSIDE the pane (the harness's own
    #    connection is never a pane, so it could not send at all). Before 1,
    #    whose typed bytes are left on the pane's command line.
    in_file, out_file = f"{RUNDIR}/msg-in.json", f"{RUNDIR}/msg-out.json"
    with open(in_file, "w") as f:
        json.dump({"MsgSend": {"to": {"host": None, "target": {"Pane": pane_id}},
                               "body": MSG_SECRET, "deliver": "Mailbox"}}, f)
    line = f"python3 {WIRE_SEND} {srv.sock} {in_file} {out_file}\n"
    c.send({"InputToPane": {"pane_id": pane_id, "data": list(line.encode())}})
    end = time.time() + 6.0
    while not os.path.exists(out_file) and time.time() < end:
        grid.pump(c, 0.1)
    if os.path.exists(out_file):
        with open(out_file) as f:
            result["msg_sent"] = name_of(json.loads(f.read()))
    reader = Client(srv.sock)
    reader.hello()
    reader.send({"MsgRead": {"pane": pane_id, "peek": True}})
    for m in reader.drain(1.0):
        if name_of(m) == "MsgList":
            result["msg_read"] = [x["body"] for x in m["MsgList"]["messages"]]
    reader.close()

    # 1. A keystroke into a view cell. No trailing newline: nothing is RUN, the
    #    bytes just reach the PTY -- which is exactly the shape of typing a
    #    passphrase into a cell and is one fewer way for the string to escape
    #    into somewhere this test is not looking.
    c.send({"InputToPane": {"pane_id": pane_id, "data": list(TYPED_SECRET)}})
    grid.pump(c, 0.6)

    # 2. A command line off `remux split`. A fresh unattached connection, as the
    #    real CLI uses.
    cli = Client(srv.sock)
    cli.hello()
    cli.send(
        {
            "CliSpawn": {
                "session": "main",
                "placement": "SplitBelow",
                "argv": ["true", ARGV_SECRET],
                "cwd": None,
            }
        }
    )
    cli.drain(1.2)
    # ...and the OTHER placement, because `NewTab` and the split paths reach the
    # server's pane-spawning code through DIFFERENT functions. Auditing one arm
    # and assuming the other is the same is how three of the five sites this
    # test found survived the first pass.
    cli.send(
        {
            "CliSpawn": {
                "session": "main",
                "placement": "NewTab",
                "argv": ["true", ARGV_SECRET],
                "cwd": None,
            }
        }
    )
    cli.drain(1.2)
    cli.close()

    # 5. `ListDirectory`, which the `files` panel repeats every 2s FOR EVER.
    #    Ten identical polls, then one for a different path.
    for _ in range(10):
        c.send({"ListDirectory": {"path": "/tmp"}})
        c.drain(0.1)
    c.send({"ListDirectory": {"path": "/usr"}})
    c.drain(0.6)
    c.close()


def run_cli(srv):
    """3. The CLIENT half: `remux split -- ...` writes its own `client.log`.

    Run as a real subprocess against the isolated server. `$REMUX_SESSION` is
    injected rather than inherited from a live pane -- proving the variable is
    REAL is `tests/pty/cli_split.py`'s job, and all this needs is the code path
    that logs the command line.
    """
    env = {**srv.env, "REMUX_SESSION": "main"}
    subprocess.run(
        [BIN, "split", "--below", "--", "true", ARGV_SECRET],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=20,
    )


def run_msg_cli(srv, result):
    """8. The CLIENT half of message redaction: `remux msg send` and `read`.

    The send runs as a pane's own program through `CliSpawn`, so the server
    attributes it to a real pane and it is refused for no other reason. The
    read is a peek from outside, whose JSON is the proof the body arrived.
    """
    pane_id = result.get("pane_id")
    if pane_id is None:
        return
    cli = Client(srv.sock)
    cli.hello()
    cli.send(
        {
            "CliSpawn": {
                "session": "main",
                "placement": "SplitBelow",
                "argv": [os.path.abspath(BIN), "msg", "send", f"%{pane_id}", CLI_MSG_SECRET],
                "cwd": None,
            }
        }
    )
    cli.drain(0.6)
    cli.close()
    end = time.time() + 8.0
    while time.time() < end:
        out = subprocess.run(
            [BIN, "msg", "read", "--pane", f"%{pane_id}", "--all", "--peek", "--format", "json"],
            env=srv.env,
            capture_output=True,
            timeout=20,
        )
        bodies = [m["body"] for m in json.loads(out.stdout or b"{}").get("messages", [])]
        if CLI_MSG_SECRET in bodies:
            result["cli_msg_read"] = True
            return
        time.sleep(0.2)


def leak_forms(secret):
    """Every form a body has leaked into a log in: as text, as hex, and as
    the Debug print of a byte list (`[99, 111, 114, ...]`, how InputToPane
    once leaked), searched without its bracket because in a framed write the
    body's bytes sit in the middle of the list."""
    data = secret.encode()
    return [secret, data.hex(), str(list(data[:6]))[1:-1]]


def run_submit(srv, result):
    """9. A body the server TYPES into a fakeagent pane. The sender is a fresh
    shell pane: the first pane's command line still holds case 1's bytes."""
    log_path, ctl = f"{RUNDIR}/agent.log", f"{RUNDIR}/agent.ctl"
    spawner = Client(srv.sock)
    spawner.hello()
    spawned = {}
    for argv in ([FAKEAGENT, log_path, ctl], None):
        body = {"session": "main", "placement": "SplitBelow"}
        if argv is not None:
            body["argv"] = argv
        spawner.send({"CliSpawn": body})
        for m in spawner.drain(3.0):
            if name_of(m) == "CliSpawned":
                spawned["agent" if argv else "sender"] = m["CliSpawned"]["pane_id"]
                break
    time.sleep(1.0)
    agent, sender = spawned.get("agent"), spawned.get("sender")
    if agent is None or sender is None:
        spawner.close()
        return
    result["submit_agent"] = agent
    in_file, out_file = f"{RUNDIR}/submit-in.json", f"{RUNDIR}/submit-out.json"
    with open(in_file, "w") as f:
        json.dump({"MsgSend": {"to": {"host": None, "target": {"Pane": agent}},
                               "body": SUBMIT_SECRET, "deliver": "Submit"}}, f)
    line = f"python3 {WIRE_SEND} {srv.sock} {in_file} {out_file}\n"
    spawner.send({"InputToPane": {"pane_id": sender, "data": list(line.encode())}})
    end = time.time() + 8.0
    while not os.path.exists(out_file) and time.time() < end:
        time.sleep(0.1)
    if os.path.exists(out_file):
        with open(out_file) as f:
            reply = json.loads(f.read())
        result["submit_reply"] = reply
    end = time.time() + 4.0
    while time.time() < end:
        if os.path.exists(log_path) and SUBMIT_SECRET.encode().hex() in open(log_path).read():
            result["submit_typed"] = True
            break
        time.sleep(0.1)
    spawner.close()


def run_cli_submit(srv, result):
    """10. `remux msg send --deliver submit --wait` as a pane's own program,
    into case 9's agent: the body is typed, and the client logs the send."""
    agent = result.get("submit_agent")
    if agent is None:
        return
    # Case 9's echo reads as work for a moment; a submit waits that out.
    time.sleep(2.0)
    spawner = Client(srv.sock)
    spawner.hello()
    spawner.send(
        {
            "CliSpawn": {
                "session": "main",
                "placement": "SplitBelow",
                "argv": [os.path.abspath(BIN), "msg", "send", f"%{agent}", "--deliver", "submit",
                         "--wait", "--timeout", "3", CLI_SUBMIT_SECRET],
                "cwd": None,
            }
        }
    )
    spawner.drain(0.6)
    spawner.close()
    log_path = f"{RUNDIR}/agent.log"
    end = time.time() + 12.0
    while time.time() < end:
        if os.path.exists(log_path) and CLI_SUBMIT_SECRET.encode().hex() in open(log_path).read():
            result["cli_submit_typed"] = True
            break
        time.sleep(0.1)
    # Long enough for the wait to time out and the CLI to log its answer.
    time.sleep(4.0)


def main():
    # Findings that come from the WIRE rather than from the log, carried out of
    # `run` so the checks can be made after the server is stopped.
    result = {}
    os.makedirs(os.path.dirname(FAKEAGENT), exist_ok=True)
    fakeagent.install(FAKEAGENT)
    srv = Server(RUNDIR).start(config=CONFIG)
    try:
        try:
            run(srv, result)
            run_cli(srv)
            run_msg_cli(srv, result)
            run_submit(srv, result)
            run_cli_submit(srv, result)
        except Exception as e:  # noqa: BLE001 -- the log is the point; reach it
            check(False, f"the run completed without an exception ({e!r})")
    finally:
        log = srv.log()
        client_log_path = f"{srv.rundir}/state/remux/client.log"
        client_log = (
            open(client_log_path).read() if os.path.exists(client_log_path) else ""
        )
        srv.kill()

    check("panicked at" not in log, "no panic in the server log")

    # -- the absence half ----------------------------------------------------
    typed = TYPED_SECRET.decode()
    # The BYTE LIST first, because that is the form the leak actually took:
    # `{other:?}` prints a `Vec<u8>` as `[99, 111, 114, ...]`, full fidelity and
    # trivially decoded, while the plain string never appears. A test that only
    # searched for the string PASSED against the leaking server -- which is the
    # branch's own rule about a surplus pass being evidence, in miniature.
    check(
        str(list(TYPED_SECRET)[:6])[:-1] not in log,
        "1 a keystroke sent with InputToPane is NOT in the log as a Debug byte list",
    )
    check(
        typed not in log,
        "1 nor, obviously, as the text itself",
    )
    check(
        ARGV_SECRET not in log,
        "2 an argument passed to CliSpawn is NOT in the server log",
    )

    # -- the presence half ---------------------------------------------------
    #
    # Without these, every check above is satisfied by a server that logged
    # nothing whatsoever -- the same trap as counting log lines to prove a
    # dedup. The line has to stay useful: it is how "why did nothing reach my
    # view cell" is answered at all.
    check(
        "msg=InputToPane" in log,
        "1 but the message IS still logged, by name",
    )
    check(
        f"{len(TYPED_SECRET)} bytes" in log,
        f"1 with its length ({len(TYPED_SECRET)} bytes), which is the diagnostic",
    )
    check(
        "CliSpawn" in log and '"true"' in log,
        "2 and CliSpawn still names the PROGRAM it was asked to run",
    )

    # 3. `client.log`, which is the same kind of file and gets the same rule.
    check(
        client_log != "",
        "3 the CLI wrote a client.log at all (otherwise 3 proves nothing)",
    )
    check(
        ARGV_SECRET not in client_log,
        "3 and `remux split`'s own log does NOT carry the argument either",
    )
    check(
        "cli-spawn" in client_log and '"true"' in client_log,
        "3 while still naming the program it asked for",
    )

    # 4. The clipboard. Both logs: the server extracts the selection, the client
    #    receives it and shells it out over OSC 52.
    check(
        CLIP_SECRET not in log and CLIP_SECRET not in client_log,
        "4 a yanked selection is in NEITHER log",
    )
    # The presence guard for 4 is different in kind: there is no redacted form
    # to look for, because this path logs NOTHING about the data and never did.
    # So what has to be shown is that the yank really happened -- otherwise the
    # check above passes on a drag that selected nothing at all.
    check(
        result.get("yanked") is True,
        f"4 and the server really DID send that selection (row={result.get('marker_row')})",
    )

    # 6. A window title.
    check(
        result.get("title_named") is True,
        "6 the title really did become the pane's name (otherwise 6 proves nothing)",
    )
    check(
        TITLE_SECRET not in log and TITLE_TAIL not in log,
        "6 a window title is NOT in the server log",
    )
    check(
        "is now named by its foreground job" in log,
        "6 while the change of who names the pane IS logged",
    )

    # 7. A message body.
    check(
        result.get("msg_sent") == "MsgSent" and result.get("msg_read") == [MSG_SECRET],
        f"7 the message really was sent and stored (sent={result.get('msg_sent')}, "
        f"read={result.get('msg_read')})",
    )
    check(MSG_SECRET not in log, "7 a message body is never in the server log")
    send_lines = [ln for ln in log.splitlines() if "msg=MsgSend(" in ln]
    check(
        any(f"{len(MSG_SECRET)} bytes" in ln for ln in send_lines) and "msg: id=" in log,
        "7 ...but the redacted line naming the id and byte count is present",
    )

    # 8. A message body through the CLI: absent from BOTH logs, while the
    #    client's own lines naming the verb, the target and the counts are
    #    present.
    check(
        result.get("cli_msg_read") is True,
        "8 the CLI's message really was sent and stored (otherwise 8 proves nothing)",
    )
    check(
        CLI_MSG_SECRET not in client_log and CLI_MSG_SECRET not in log,
        "8 a body given to `remux msg send` is in NEITHER log",
    )
    check(
        f'cmd: msg send to="%{result.get("pane_id")}"' in client_log
        and f"bytes={len(CLI_MSG_SECRET)}" in client_log,
        "8 ...but client.log names the send, its target and its byte count",
    )
    check(
        "cmd: msg read answered MsgList(" in client_log and "message(s))" in client_log,
        "8 ...and names the read's answer by its count",
    )

    # 9. A body typed into an agent's pane: in no form in the server log,
    #    while the line naming the submit, its pane and its size is present.
    reply = result.get("submit_reply") or {}
    check(
        (reply.get("MsgSent") or {}).get("delivery") == "Submitted"
        and result.get("submit_typed") is True,
        f"9 the body really was typed into the agent's pane (reply {reply})",
    )
    leaked = [form for form in leak_forms(SUBMIT_SECRET) if form in log]
    check(
        not leaked,
        f"9 a submitted body is in the server log as neither text, hex nor a byte list (found {leaked})",
    )
    sent_id = (reply.get("MsgSent") or {}).get("id")
    check(
        f"msg: id={sent_id} submitted to %{result.get('submit_agent')} (bracketed=" in log,
        "9 ...but the line naming the submit is present",
    )

    # 10. A body the CLI sends with --deliver submit --wait: typed, and in
    #     neither log in any form, while the client's line naming the send
    #     and its flags is present.
    check(
        result.get("cli_submit_typed") is True,
        "10 the CLI's submitted body really was typed into the agent's pane",
    )
    leaked = [form for form in leak_forms(CLI_SUBMIT_SECRET) if form in log or form in client_log]
    check(not leaked, f"10 a body sent by `msg send --deliver submit` is in neither log (found {leaked})")
    sends = [ln for ln in client_log.splitlines() if f'cmd: msg send to="%{result.get("submit_agent")}"' in ln]
    check(
        any("deliver=Submit" in ln and "wait=true" in ln and "timeout=Some(3000)" in ln for ln in sends)
        and "cmd: msg send answered MsgSent(" in client_log,
        f"10 ...but client.log names the send, its delivery and its wait flags (lines {sends})",
    )

    # 5. ListDirectory: logged on CHANGE, not on every 2s poll.
    lists = [ln for ln in log.splitlines() if "msg=ListDirectory" in ln]
    tmp = [ln for ln in lists if '"/tmp"' in ln]
    usr = [ln for ln in lists if '"/usr"' in ln]
    check(
        len(tmp) == 1,
        f"5 ten identical ListDirectory polls log ONE line (got {len(tmp)})",
    )
    check(
        len(usr) == 1,
        f"5 and a CHANGE of path still logs its own line (got {len(usr)})",
    )
    check(
        '"/tmp"' in "".join(lists),
        "5 with the path kept, which is the diagnostic",
    )

    print()
    if FAILURES:
        print(f"FAILED ({len(FAILURES)}): " + "; ".join(FAILURES))
        return 1
    print("log_redaction: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
