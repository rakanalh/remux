#!/usr/bin/env python3
"""4f: expired mail is dropped, never delivered, over the wire.

A server of its own, because `ttl_hours = 0.0005` (1.8 s) would expire the
mail of every other case in `msg_mailbox.py` while it was still being read.

Nothing here races the clock. The message never has to be seen before it
expires: the `MsgSent` answer is what proves it was stored. The harness then
waits until the server's clock has certainly passed the expiry (the reply's
arrival plus the TTL plus a margin; `created_ms` is no later than the reply),
and reads once.

4f. ttl_hours=0.0005: two messages read after their TTL are not listed, and
    the server log says both were swept.
7f. A message that expires while the server is stopped is pruned as expired
    when it starts, not delivered.

Run: python3 tests/frame/msg_ttl.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msgttl")
WIRE_SEND = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wire_send.py")
TTL_S = 0.0005 * 3600
CONFIG = """
[general]
save_sessions = true
automatic_restore = true

[messaging]
ttl_hours = 0.0005
"""

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def cli_spawn(placement, argv=None):
    caller = Client(srv.sock)
    caller.hello()
    body = {"session": "main", "placement": placement}
    if argv is not None:
        body["argv"] = argv
    caller.send({"CliSpawn": body})
    reply = None
    for msg in caller.drain(2.0):
        if name_of(msg) in ("CliSpawned", "Error"):
            reply = msg
            break
    caller.close()
    return reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None


def spawn_sender(msg):
    """Run one wire request, or a list on one connection, as a new pane's own
    program; return the reply or the list of replies."""
    in_file, out_file = f"{RUNDIR}/in.json", f"{RUNDIR}/out.json"
    with open(in_file, "w") as f:
        json.dump(msg, f)
    cli_spawn("SplitBelow", ["python3", WIRE_SEND, srv.sock, in_file, out_file])
    end = time.time() + 6.0
    while time.time() < end:
        if os.path.exists(out_file):
            with open(out_file) as f:
                return json.loads(f.read())
        time.sleep(0.05)
    return None


def read_box(pane):
    c = Client(srv.sock)
    c.hello()
    c.send({"MsgRead": {"pane": pane, "peek": True, "all": True}})
    try:
        for m in c.drain(1.5):
            if name_of(m) != "ViewList":
                return m
    finally:
        c.close()
    return None


for var in ("REMUX_SESSION", "REMUX_PANE"):
    os.environ.pop(var, None)

srv = Server(RUNDIR)
try:
    srv.start(config=CONFIG)
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 120, "rows": 40}})
    cli.drain(0.8)
    target = cli_spawn("SplitRight")

    # Two, so that a sweep dropping only the first expired message is caught.
    replies = spawn_sender([{"MsgSend": {"to": {"host": None, "target": {"Pane": target}},
                                         "body": body, "deliver": "Mailbox"}}
                            for body in ("TTL:gone", "TTL:gone-too")])
    answered_at = time.time()
    check([name_of(r) for r in replies or []] == ["MsgSent", "MsgSent"],
          f"4f two messages were stored for %{target} (got {replies})")

    time.sleep(max(0.0, answered_at + TTL_S + 0.5 - time.time()))
    r = read_box(target)
    listed = r["MsgList"]["messages"] if name_of(r) == "MsgList" else None
    check(listed == [], f"4f ttl_hours=0.0005: once expired it is not listed (got {r})")
    check("msg: swept 2 expired message(s)" in srv.log(),
          "4f ttl_hours=0.0005: the server log says both were swept")
    check("TTL:gone" not in srv.log(), "4f the body is not in the server log")

    cli.close()
    time.sleep(0.3)
    check("panicked at" not in srv.log(), "4f no panic in the server log")
finally:
    srv.kill()

# --- 7f: expired on disk ---------------------------------------------------
# A server of its own with a TTL longer than a stop takes (a stop sweeps
# first, so mail that expires during it is never written, which is right).
DISK_TTL_S = 0.002 * 3600
srv = Server(RUNDIR + "-disk")
try:
    srv.start(config=CONFIG.replace("ttl_hours = 0.0005", "ttl_hours = 0.002"))
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 120, "rows": 40}})
    cli.drain(0.8)
    target = cli_spawn("SplitRight")
    reply = spawn_sender({"MsgSend": {"to": {"host": None, "target": {"Pane": target}},
                                      "body": "TTL:on-disk", "deliver": "Mailbox"}})
    answered_at = time.time()
    check(name_of(reply) == "MsgSent", f"7f a message was stored for %{target} (got {reply})")
    cli.close()
    srv.stop()
    stopped_at = time.time()
    on_disk = open(f"{srv.rundir}/data/remux/mailbox.json").read()
    check("TTL:on-disk" in on_disk and stopped_at < answered_at + DISK_TTL_S,
          f"7f the stop wrote it to mailbox.json, unexpired ({stopped_at - answered_at:.1f}s after it was sent)")
    time.sleep(max(0.0, answered_at + DISK_TTL_S + 0.5 - time.time()))
    srv.start_again()
    r = read_box(target)
    check(name_of(r) == "MsgList" and r["MsgList"]["messages"] == [],
          f"7f after the restart it is not listed (got {r})")
    check("pruned 0 for closed panes, 1 expired" in srv.log(),
          "7f the start logs it pruned as expired, not as a closed pane's")
    check("panicked at" not in srv.log(), "7f no panic in the server log")
finally:
    srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED")
    sys.exit(1)
print("ALL PASS")
