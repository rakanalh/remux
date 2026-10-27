#!/usr/bin/env python3
"""Agent messaging, server side: who a sender is, and the mailbox.

The server attributes every connection's PEER PID to a pane by process
ancestry and logs one `identity:` line per connection. The answer comes from
the process table, never from `$REMUX_PANE`, so these cases connect from a
real process running inside a real pane and read the server log:

1a. `remux ls` run by a pane's `sh -c` is a descendant of the pane's session
    leader, so it is attributed to that pane by its session id.
1b. The harness's own connection is not in any pane of THIS server, and says so.
1c. `remux ls` as the pane's own program is the session leader itself.
1d. `setsid -w remux ls` leaves the pane's session, so only the parent walk
    reaches the pane, one link up.
1e. `remux ls` under `script` runs on script's pty, not the pane's. The walk
    still reaches the pane and the differing tty is logged, never refused.
1f. A double fork that detaches fully (new session, parent gone) names no
    pane and is refused attribution.
1g. Every connection is pinned by a pidfd when the kernel offers
    SO_PEERPIDFD (6.5 and later), and by start time when it does not.

Sending and reading. The harness's own connection is never a pane, so every
send that needs a real sender runs `wire_send.py` INSIDE a pane: typed into an
interactive shell (`type_send`, the same pane every time, the session-id path)
or as a pane's own program (`spawn_sender`, the session leader itself).

3a. A send from outside any pane is refused `sender_unverified`.
3b. A send from pane A to %B is stored and answered `MsgSent`; a request to
    type it into the pane is answered `submit_unavailable`, not refused.
3c. B's mailbox holds it, from A in session main, Queued.
3d. A read of B's mailbox by anyone but B is a peek, whatever it asks.
3e. B reading its own mailbox marks it Read; a second read is empty; --all
    still lists it.
3f. `pane_hint` naming B does not make a message from A come from B, and the
    disagreement is logged.
3f'. A sender that is its pane's own program is attributed to that pane.
3g. An unknown pane, a remote host and a malformed name are refused with their
    own codes.
3h. A read from outside any pane must name a pane; an unknown one is refused.
3i. The server log has the send and its attribution, and never the body.
3j. A socket handed to another process after its connector exited is refused
    `sender_unverified` as "exited", never attributed to whoever holds it.
3m. A pane whose program exited but which is still in its (background) tab is
    not a message target.
3n. A popup is in no tab: it can neither receive nor send, and says so.
3o. A body carrying a control character (ESC[201~ ends a bracketed paste) is
    refused `bad_body`.
3p. A host carrying a newline is refused `bad_address` and cannot forge a
    line in the server log.

1z/3z. No panic in the server log.

Run: python3 tests/frame/msg_mailbox.py   (RMX_RUNDIR overrides the run dir)
"""
import os
import re
import json
import shutil
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msg")

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def rows_text(msg):
    pc = msg["PaneContent"]
    return ["".join(c["c"] for c in row).rstrip() for row in pc["cells"]]


def cli_spawn(placement, argv=None, session="main"):
    """One CliSpawn on a fresh unattached connection, as the real CLI sends it."""
    caller = Client(srv.sock)
    caller.hello()
    body = {"session": session, "placement": placement}
    if argv is not None:
        body["argv"] = argv
    caller.send({"CliSpawn": body})
    reply = None
    for msg in caller.drain(3.0):
        if name_of(msg) in ("CliSpawned", "Error"):
            reply = msg
            break
    caller.close()
    return reply


def pane_content(cli, pane_id, want, timeout=6.0):
    cli.send({"SubscribePane": {"pane_id": pane_id, "cols": 80, "rows": 20,
                                "size_demand": False}})
    end = time.time() + timeout
    seen = []
    while time.time() < end:
        for msg in cli.drain(0.3):
            if name_of(msg) != "PaneContent":
                continue
            if msg["PaneContent"]["pane_id"] != pane_id:
                continue
            seen = rows_text(msg)
            if any(want in r for r in seen):
                return True, seen
    return False, seen


def log_has(pattern, timeout=4.0):
    """Wait for a server.log line matching `pattern`, returning the match."""
    rx = re.compile(pattern)
    end = time.time() + timeout
    while True:
        for line in srv.log().splitlines():
            if rx.search(line):
                return line
        if time.time() >= end:
            return None
        time.sleep(0.2)


SO_PEERPIDFD = 77


def kernel_has_peer_pidfd():
    """Ask the kernel, not its version string: a backported or patched kernel
    can have the option at a version number that says it should not."""
    a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        fd = a.getsockopt(socket.SOL_SOCKET, SO_PEERPIDFD)
    except OSError:
        return False
    finally:
        a.close()
        b.close()
    os.close(fd)
    return True


def identity_lines():
    return [ln for ln in srv.log().splitlines() if "identity:" in ln]


# A developer running this from inside remux would otherwise hand the throwaway
# server's panes a pane identity that belongs to their real server.
for var in ("REMUX_SESSION", "REMUX_PANE"):
    os.environ.pop(var, None)

WIRE_SEND = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wire_send.py")

# Small caps, so that a case depending on one reaches it in a few sends.
CONFIG = """
[messaging]
max_body_bytes = 64
mailbox_max_messages = 3
rate_per_10s = 4
"""

requests = [0]


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


def spawn_sender(msg):
    """Run one wire request as a NEW pane's own program: (pane_id, reply)."""
    requests[0] += 1
    n = requests[0]
    in_file, out_file = f"{RUNDIR}/in-{n}.json", f"{RUNDIR}/out-{n}.json"
    with open(in_file, "w") as f:
        json.dump(msg, f)
    spawned = cli_spawn("SplitBelow", argv=["python3", WIRE_SEND, srv.sock, in_file, out_file])
    pane = spawned["CliSpawned"]["pane_id"] if name_of(spawned) == "CliSpawned" else None
    return pane, wait_file(out_file)


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


def read_box(pane, peek=True, all=False):
    return ask({"MsgRead": {"pane": pane, "peek": peek, "all": all}})


def send_to(target, body, **extra):
    msg = {"to": {"host": None, "target": target}, "body": body, "deliver": "Mailbox"}
    msg.update(extra)
    return {"MsgSend": msg}


def code_of(reply):
    return reply["Error"].get("code") if name_of(reply) == "Error" else None


def listed(reply):
    return reply["MsgList"]["messages"] if name_of(reply) == "MsgList" else None


def inherited_socket_send(msg):
    """P connects and finishes the handshake, forks C and exits; the harness
    reaps P; C then sends on the socket it inherited."""
    out_file = f"{RUNDIR}/inherited.json"
    p = os.fork()
    if p == 0:
        try:
            c = Client(srv.sock)
            c.s.settimeout(5)
            c.hello()
            if os.fork() == 0:
                time.sleep(1.0)
                c.send(msg)
                while True:
                    reply = c.recv()
                    if name_of(reply) != "ViewList":
                        break
                with open(out_file + ".tmp", "w") as f:
                    f.write(json.dumps(reply))
                os.rename(out_file + ".tmp", out_file)
            os._exit(0)
        except BaseException:
            os._exit(1)
    os.waitpid(p, 0)
    return wait_file(out_file)


def tree_panes():
    cli.send("ListSessionTree")
    for msg in cli.drain(1.0):
        if name_of(msg) != "SessionTree":
            continue
        tree = msg["SessionTree"]
        groups = list(tree["unfiled"])
        for f in tree["folders"]:
            groups += f["sessions"]
        return [p["id"] for sess in groups for tab in sess["tabs"] for p in tab["panes"]]
    return []


srv = Server(RUNDIR)
try:
    srv.start(config=CONFIG)

    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 120, "rows": 40}})
    cli.drain(0.8)

    # --- 1a: a descendant of the pane's session leader ---------------------
    reply = cli_spawn("SplitBelow", argv=[
        "/bin/sh", "-c", f"{BIN} ls >/dev/null; printf 'LS:%s\\n' done; exec /bin/sh"])
    check(name_of(reply) == "CliSpawned", f"1a pane A spawned (got {reply})")
    a = reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None
    ok, seen = pane_content(cli, a, "LS:done")
    check(ok, f"1a `remux ls` ran inside pane A to completion (saw {seen[-5:]})")
    line = log_has(rf"identity: client_id=\d+ peer=Some\(\d+\) pin=\w+ -> pane %{a} by Sid$")
    check(line is not None,
          f"1a the connection from inside pane A is attributed to %{a} by Sid "
          f"(identity lines: {identity_lines()})")

    # --- 1b: the harness itself is in no pane of this server ---------------
    line = log_has(rf"identity: client_id=\d+ peer=Some\({os.getpid()}\) pin=\w+ "
                   r"not a pane: not a descendant of any pane$")
    check(line is not None,
          f"1b the harness's own connection is not a pane "
          f"(identity lines: {identity_lines()})")

    # --- 1c: the pane's own program is the session leader ------------------
    reply = cli_spawn("SplitRight", argv=[BIN, "ls"])
    check(name_of(reply) == "CliSpawned", f"1c pane B spawned (got {reply})")
    b = reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None
    line = log_has(rf"identity: client_id=\d+ peer=Some\(\d+\) pin=\w+ -> pane %{b} by Sid$")
    check(line is not None,
          f"1c the pane's own `remux ls` is attributed to %{b} by Sid "
          f"(identity lines: {identity_lines()})")

    # --- 1d: a sender that left the pane's session ------------------------
    reply = cli_spawn("SplitBelow", argv=["setsid", "-w", BIN, "ls"])
    check(name_of(reply) == "CliSpawned", f"1d pane D spawned (got {reply})")
    d = reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None
    line = log_has(rf"identity: client_id=\d+ peer=Some\(\d+\) pin=\w+ -> pane %{d} "
                   r"by Walk \{ depth: 1 \}$")
    check(line is not None,
          f"1d `setsid -w remux ls` is attributed to %{d} by the walk at depth 1 "
          f"(identity lines: {identity_lines()})")

    # --- 1e: a sender on a nested pty --------------------------------------
    if shutil.which("script") is None:
        print("SKIP  1e `script` is not installed")
    else:
        reply = cli_spawn("SplitRight", argv=["script", "-qec", f"{BIN} ls", "/dev/null"])
        check(name_of(reply) == "CliSpawned", f"1e pane E spawned (got {reply})")
        e = reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None
        line = log_has(rf" INFO .*identity: client_id=\d+ peer=Some\(\d+\) pin=\w+ -> pane %{e} "
                       r"by Walk \{ depth: \d+ \}; its tty differs \(nested pty\?\)$")
        check(line is not None,
              f"1e `remux ls` under `script` is attributed to %{e} with its tty "
              f"reported as differing (identity lines: {identity_lines()})")

    # --- 1f: a fully detached double fork ----------------------------------
    # The subshell exits at once, so the `setsid` child is reparented away
    # from the pane, and `setsid` gives it a session of its own.
    reply = cli_spawn("SplitBelow", argv=[
        "/bin/sh", "-c",
        f"( setsid /bin/sh -c 'sleep 0.5; exec {BIN} ls' & ); "
        "printf 'DF:%s\\n' forked; exec /bin/sh"])
    check(name_of(reply) == "CliSpawned", f"1f pane F spawned (got {reply})")
    rx = re.compile(r"identity: client_id=\d+ peer=Some\((\d+)\) pin=\w+ "
                    r"not a pane: not a descendant of any pane$")
    detached = []
    end = time.time() + 5.0
    while not detached and time.time() < end:
        time.sleep(0.2)
        detached = [m.group(1) for ln in srv.log().splitlines()
                    for m in rx.finditer(ln) if m.group(1) != str(os.getpid())]
    check(len(detached) == 1,
          f"1f the detached `remux ls` is not attributed to any pane "
          f"(identity lines: {identity_lines()})")

    # --- 1g: the pin in use -------------------------------------------------
    want = "pidfd" if kernel_has_peer_pidfd() else "starttime"
    pins = set(re.findall(r"identity: client_id=\d+ peer=Some\(\d+\) pin=(\w+) ", srv.log()))
    check(pins == {want}, f"1g every connection is pinned by {want}, which this kernel offers (saw {pins})")

    # === 3: sending and reading ============================================
    A = cli_spawn("SplitBelow")["CliSpawned"]["pane_id"]
    B = cli_spawn("SplitRight")["CliSpawned"]["pane_id"]
    time.sleep(0.8)

    # --- 3a ---------------------------------------------------------------
    r = ask(send_to({"Pane": B}, "MSG:outside"))
    check(code_of(r) == "sender_unverified" and "not a descendant" in r["Error"]["message"],
          f"3a a send from outside any pane is refused sender_unverified (got {r})")

    # --- 3b ---------------------------------------------------------------
    r = type_send(A, {"MsgSend": {"to": {"host": None, "target": {"Pane": B}},
                                  "body": "MSG:one", "deliver": "Auto"}})
    sent = r.get("MsgSent") if r else None
    check(sent is not None and sent["id"] == 1 and sent["to"]["pane_id"] == B
          and sent["delivery"] == "Mailbox" and sent["reason"] == "submit_unavailable",
          f"3b A's send to %{B} is stored as id 1, submit_unavailable (got {r})")

    # --- 3c ---------------------------------------------------------------
    msgs = listed(read_box(B))
    first = msgs[0] if msgs else {}
    check(msgs is not None and len(msgs) == 1
          and first.get("from", {}).get("Pane", {}).get("pane_id") == A
          and first["from"]["Pane"].get("session") == "main"
          and first.get("body") == "MSG:one" and first.get("state") == "Queued",
          f"3c B's mailbox holds MSG:one from %{A} in main, Queued (got {msgs})")

    # --- 3d ---------------------------------------------------------------
    # Each read must itself return the message: a read that failed would leave
    # the mail Queued too, and the last check would pass on nothing.
    by_harness = listed(read_box(B, peek=False))
    check(by_harness is not None and [m["body"] for m in by_harness] == ["MSG:one"],
          f"3d the harness's non-peek read of B's mailbox returned the message (got {by_harness})")
    by_a = listed(type_send(A, {"MsgRead": {"pane": B, "peek": False}}))
    check(by_a is not None and [m["body"] for m in by_a] == ["MSG:one"],
          f"3d A's non-peek read of B's mailbox returned the message (got {by_a})")
    msgs = listed(read_box(B))
    check(msgs is not None and [m["state"] for m in msgs] == ["Queued"],
          f"3d ...and both reads left it Queued (got {msgs})")

    # --- 3e ---------------------------------------------------------------
    own = listed(type_send(B, {"MsgRead": {}}))
    check(own is not None and [m["body"] for m in own] == ["MSG:one"],
          f"3e B reads its own mailbox (got {own})")
    msgs = listed(read_box(B))
    check(msgs == [] , f"3e ...and it is no longer Queued (got {msgs})")
    again = listed(type_send(B, {"MsgRead": {}}))
    check(again == [], f"3e a second read is empty (got {again})")
    everything = listed(type_send(B, {"MsgRead": {"all": True}}))
    check(everything is not None and [(m["body"], m["state"]) for m in everything]
          == [("MSG:one", "Read")], f"3e --all still lists it, Read (got {everything})")

    # --- 3f ---------------------------------------------------------------
    r = type_send(A, {"MsgSend": {"to": {"host": None, "target": {"Pane": B}},
                                  "body": "MSG:two", "deliver": "Mailbox", "pane_hint": B}})
    msgs = listed(read_box(B, all=True)) or []
    two = [m for m in msgs if m["body"] == "MSG:two"]
    check(name_of(r) == "MsgSent" and len(two) == 1
          and two[0]["from"]["Pane"]["pane_id"] == A,
          f"3f pane_hint=%{B} did not change the sender from %{A} (reply {r}, box {msgs})")
    check(f"pane_hint=%{B} disagrees with attribution %{A}" in srv.log(),
          "3f the disagreeing pane_hint is logged")

    # --- 3f' --------------------------------------------------------------
    leader, r = spawn_sender(send_to({"Pane": B}, "MSG:leader"))
    msgs = listed(read_box(B, all=True)) or []
    by_leader = [m for m in msgs if m["body"] == "MSG:leader"]
    check(name_of(r) == "MsgSent" and len(by_leader) == 1
          and by_leader[0]["from"]["Pane"]["pane_id"] == leader,
          f"3f' a pane's own program is attributed to that pane %{leader} (reply {r}, box {msgs})")

    # --- 3g ---------------------------------------------------------------
    r = type_send(A, send_to({"Pane": 9999}, "MSG:nowhere"))
    check(code_of(r) == "no_such_target", f"3g %9999 is no_such_target (got {r})")
    r = type_send(A, {"MsgSend": {"to": {"host": "mini", "target": {"Pane": 1}},
                                  "body": "MSG:remote", "deliver": "Mailbox"}})
    check(code_of(r) == "route_unavailable", f"3g mini:%1 is route_unavailable (got {r})")
    r = type_send(A, send_to({"Name": "9zz"}, "MSG:badname"))
    check(code_of(r) == "bad_address", f"3g the name 9zz is bad_address (got {r})")

    # --- 3h ---------------------------------------------------------------
    r = ask({"MsgRead": {}})
    check(code_of(r) == "not_in_pane", f"3h a read from outside any pane must name one (got {r})")
    r = read_box(9999)
    check(code_of(r) == "no_such_target", f"3h a read of %9999 is no_such_target (got {r})")

    # --- 3j ---------------------------------------------------------------
    r = inherited_socket_send(send_to({"Pane": B}, "MSG:inherited"))
    check(code_of(r) == "sender_unverified" and "exited before it was checked" in r["Error"]["message"],
          f"3j an inherited socket whose connector exited is refused as exited (got {r})")

    # --- 3m ---------------------------------------------------------------
    # The program exits only once its tab is in the background, which is the
    # one place close_pane leaves an exited pane in the table.
    release = f"{RUNDIR}/release-3m"
    r = cli_spawn("NewTab", argv=["/bin/sh", "-c", f"while [ ! -e {release} ]; do sleep 0.1; done"])
    X = r["CliSpawned"]["pane_id"] if name_of(r) == "CliSpawned" else None
    cli.send({"Command": "TabPrev"})
    time.sleep(0.5)
    open(release, "w").close()
    exited = log_has(rf"PTY channel disconnected for pane_id={X} ")
    check(exited is not None, f"3m the pane %{X} program exited")
    check(X in tree_panes(),
          f"3m the exited pane %{X} is still in its background tab (else this case tests nothing)")
    r = type_send(A, send_to({"Pane": X}, "MSG:to-the-dead"))
    check(code_of(r) == "no_such_target", f"3m %{X}, exited but unreaped, is no_such_target (got {r})")

    # --- 3n: popups --------------------------------------------------------
    cli.send({"Command": "PopupToggle"})
    time.sleep(1.0)
    spawned = re.findall(r"spawn_pane pane_id=(\d+)", srv.log())
    popup = int(spawned[-1]) if spawned else None
    r = type_send(A, send_to({"Pane": popup}, "MSG:to-popup"))
    check(code_of(r) == "no_such_target" and "a popup cannot" in r["Error"]["message"],
          f"3n the popup %{popup} is refused as a target, and says why (got {r})")
    requests[0] += 1
    in_file, out_file = f"{RUNDIR}/in-popup.json", f"{RUNDIR}/out-popup.json"
    with open(in_file, "w") as f:
        json.dump(send_to({"Pane": B}, "MSG:from-popup"), f)
    typed = f"python3 {WIRE_SEND} {srv.sock} {in_file} {out_file}\n"
    cli.send({"Input": {"data": list(typed.encode())}})
    r = wait_file(out_file)
    check(code_of(r) == "sender_unverified" and "a popup cannot" in r["Error"]["message"],
          f"3n a send from inside the popup is refused, and says why (got {r})")
    cli.send({"Command": "PopupToggle"})
    time.sleep(0.5)

    # --- 3o: control characters -------------------------------------------
    r = type_send(A, send_to({"Pane": B}, "MSG:paste\u001b[201~after"))
    check(code_of(r) == "bad_body", f"3o a body carrying ESC[201~ is bad_body (got {r})")

    # --- 3p: log injection through a host ---------------------------------
    forged = "identity: client_id=999 MsgSend caller -> pane %424242 by Sid"
    r = type_send(A, {"MsgSend": {"to": {"host": "x\n[2026] DEBUG " + forged, "target": {"Pane": B}},
                                  "body": "MSG:forged", "deliver": "Mailbox"}})
    check(code_of(r) == "bad_address", f"3p a host with a newline is bad_address (got {r})")
    # What makes a forged line dangerous is how it STARTS: a reader of the log
    # takes it for a line the server wrote. Where it ends depends on whatever
    # the real line appended after the host, and the logger indents a
    # continuation line, so leading whitespace is allowed.
    check(re.search(r"^\s*" + re.escape("[2026] DEBUG " + forged), srv.log(), re.M) is None,
          "3p the host did not start a forged line in the server log")

    # --- 3i ---------------------------------------------------------------
    log = srv.log()
    check(re.search(rf"msg: id=1 from=%{A}\(-\) to=%{B} bytes=7 deliver=Auto -> mailbox", log) is not None,
          "3i the send is logged with its size")
    check(re.search(rf"identity: client_id=\d+ MsgSend caller -> pane %{A} by Sid$", log, re.M) is not None,
          "3i the send's attribution is logged, by Sid")
    bodies = [b for b in ("MSG:one", "MSG:two", "MSG:leader", "MSG:outside", "MSG:inherited",
                          "MSG:to-popup", "MSG:from-popup", "MSG:paste", "MSG:forged")
              if b in log]
    check(bodies == [], f"3i no message body is in the server log (found {bodies})")

    cli.close()
    time.sleep(0.3)
    check("panicked at" not in srv.log(), "1z/3z no panic in the server log")
finally:
    srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED")
    sys.exit(1)
print("ALL PASS")
