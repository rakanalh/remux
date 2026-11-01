#!/usr/bin/env python3
"""3k: a reused pid is never attributed as the sender.

A socket's peer pid is the pid of whoever called `connect()`. If that process
exits and another process takes its pid, attributing by pid names the new
process, which can be in any pane. The server pins every connection to its
connector (a pidfd on Linux) and refuses a send whose connector is gone.

This forces the reuse rather than waiting for it:

1. The harness re-runs itself in a fresh user and pid namespace
   (`unshare -Urpf --mount-proc`), where it may write `ns_last_pid`.
2. A server comes up with pane A. P connects from outside any pane and
   completes the handshake, forks C (which inherits the socket) and exits;
   the harness reaps P.
3. `ns_last_pid` is set to P-1 and pane B is spawned, so B's shell gets P's
   pid. B's shell reports its own pid (`printf 'PID:%s' $$`), so the number is
   the shell's word, not a guess. Server threads also take pids, so this is
   retried; a run that never lands the pid is INCONCLUSIVE and exits 2.
4. C sends a message to A on P's socket. It must be refused
   `sender_unverified` ("exited" under a pidfd pin, "process tree changed"
   under a start-time pin), and A's mailbox must stay empty. Without
   the pin the server would read P's pid, find B's shell, and store the
   message from B.

Exit codes: 0 pass, 1 fail, 2 skipped or inconclusive (never a pass). On
macOS, which has no pid namespaces, it prints SKIP and exits 0, because no
run there can be anything else.

Run: python3 tests/frame/msg_identity_reuse.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msgreuse")
ATTEMPTS = 20

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def reexec_in_namespace():
    """Run this file again as pid 1 of a new user and pid namespace."""
    env = dict(os.environ, RMX_IN_PIDNS="1")
    try:
        done = subprocess.run(
            ["unshare", "-Urpf", "--mount-proc", sys.executable, os.path.abspath(__file__)],
            env=env,
        )
    except FileNotFoundError:
        print("SKIP: no unshare(1); user+pid namespaces are needed to force pid reuse")
        return 2
    if done.returncode not in (0, 1, 2):
        print(f"SKIP: no user+pid namespaces here (unshare exited {done.returncode})")
        return 2
    return done.returncode


def cli_spawn(srv, placement, argv, before_send=None):
    caller = Client(srv.sock)
    caller.hello()
    # Let the server finish this connection's own work (its identity line
    # runs on a blocking thread, which takes a pid) before the pid is set.
    caller.drain(0.3)
    if before_send is not None:
        before_send()
    caller.send({"CliSpawn": {"session": "main", "placement": placement, "argv": argv}})
    reply = None
    for msg in caller.drain(2.0):
        if name_of(msg) in ("CliSpawned", "Error"):
            reply = msg
            break
    caller.close()
    return reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None


def pane_text(cli, pane_id, want, timeout=3.0):
    cli.send({"SubscribePane": {"pane_id": pane_id, "cols": 80, "rows": 10,
                                "size_demand": False}})
    end = time.time() + timeout
    while time.time() < end:
        for msg in cli.drain(0.3):
            if name_of(msg) != "PaneContent" or msg["PaneContent"]["pane_id"] != pane_id:
                continue
            for row in msg["PaneContent"]["cells"]:
                text = "".join(c["c"] for c in row)
                for m in re.finditer(want, text):
                    return m
    return None


def wait_file(path, timeout=6.0):
    end = time.time() + timeout
    while time.time() < end:
        if os.path.exists(path):
            with open(path) as f:
                return json.loads(f.read())
        time.sleep(0.05)
    return None


def connect_then_orphan(srv, go_file, out_file, msg):
    """P connects and handshakes, forks C, exits. C waits for `go_file`, then
    sends `msg` on the socket it inherited. Returns P's pid, already reaped."""
    p = os.fork()
    if p == 0:
        try:
            c = Client(srv.sock)
            c.s.settimeout(10)
            c.hello()
            if os.fork() == 0:
                while not os.path.exists(go_file):
                    time.sleep(0.05)
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
    return p


def main():
    if sys.platform == "darwin":
        print("SKIP: macOS has no pid namespaces, so this cannot force a pid to be reused (darwin). "
              "The macOS pin is covered by unit tests in src/server/identity.rs only; "
              "there is no end-to-end pid-reuse test on darwin")
        return 0
    if os.environ.get("RMX_IN_PIDNS") != "1":
        return reexec_in_namespace()
    for var in ("REMUX_SESSION", "REMUX_PANE"):
        os.environ.pop(var, None)

    srv = Server(RUNDIR)
    srv.start()
    try:
        cli = Client(srv.sock)
        cli.hello()
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": 120, "rows": 40}})
        cli.drain(0.8)
        a = cli_spawn(srv, "SplitBelow", ["/bin/sh"])

        go_file, out_file = f"{RUNDIR}/go", f"{RUNDIR}/reply.json"
        msg = {"MsgSend": {"to": {"host": None, "target": {"Pane": a}},
                           "body": "MSG:reused", "deliver": "Mailbox"}}
        p = connect_then_orphan(srv, go_file, out_file, msg)
        print(f"connector P was pid {p}; it has exited and been reaped")

        b = None
        for attempt in range(1, ATTEMPTS + 1):
            def set_last_pid():
                with open("/proc/sys/kernel/ns_last_pid", "w") as f:
                    f.write(str(p - 1))
            pane = cli_spawn(srv, "SplitRight",
                             ["/bin/sh", "-c", "printf 'PID:%s\\n' $$; exec /bin/sh"],
                             before_send=set_last_pid)
            m = pane_text(cli, pane, r"PID:(\d+)")
            got = int(m.group(1)) if m else None
            print(f"attempt {attempt}: pane %{pane} shell pid {got}")
            if got == p:
                b = pane
                break
        if b is None:
            print(f"INCONCLUSIVE: no pane shell took pid {p} in {ATTEMPTS} attempts")
            return 2

        # A pidfd sees P reaped; a start-time pin sees a process with P's pid
        # that started later. Both refuse, with different words.
        pins = re.findall(rf"identity: client_id=\d+ peer=Some\({p}\) pin=(\w+) ", srv.log())
        pin = pins[0] if pins else None
        expected = {"pidfd": "exited before it was checked",
                    "starttime": "process tree changed"}.get(pin)
        if expected is None:
            print(f"INCONCLUSIVE: P's connection was pinned by {pin!r}, which this test "
                  f"does not know the refusal for")
            return 2
        open(go_file, "w").close()
        reply = wait_file(out_file)
        refused = name_of(reply) == "Error" and reply["Error"].get("code") == "sender_unverified"
        check(refused and expected in reply["Error"]["message"],
              f"3k the send on P's socket (pin={pin}) is refused with {expected!r}, although "
              f"pane %{b}'s shell now has P's pid (got {reply})")

        c = Client(srv.sock)
        c.hello()
        c.send({"MsgRead": {"pane": a, "peek": True, "all": True}})
        box = None
        for m in c.drain(1.5):
            if name_of(m) == "MsgList":
                box = m["MsgList"]["messages"]
        c.close()
        check(box == [], f"3k pane %{a}'s mailbox is empty, so nothing arrived from %{b} (got {box})")
        check("panicked at" not in srv.log(), "3k no panic in the server log")
    finally:
        srv.kill()

    print()
    if failures:
        print(f"{len(failures)} FAILED")
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
