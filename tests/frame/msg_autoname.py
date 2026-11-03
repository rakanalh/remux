#!/usr/bin/env python3
"""Auto names: every agent is addressable with no `agent name` step.

A pane with no name is given one the first time `agent ls` / `agent status`
lists it, it sends, or it is sent to: a slug of its session (`build`,
`build-2`, ...), `agent` when the slug is empty or reserved. A slug's count
only grows, so a closed pane's auto name is never issued again, and it
survives a restart. `[agents] commands = ["sleep"]` makes a pane running
`sleep` count as an agent.

an1. Two unnamed agents in session `build` list as `build` and `build-2` on
     the first `agent ls`, with no `agent name` run.
an2. After `build-2` closes, a third agent gets `build-3`, never `build-2`.
an3. The names, and the count, survive `remux restart`: a fourth agent after
     it gets `build-4`.
an4. Session `Remux` names its agent `remux` (not reserved); `2024 work!`
     names `s-2024-work`; `user` (reserved) names `agent`.
an5. `agent name alpha` on `build-3` frees `build-3`: a send to it is
     no_such_target.
an6. A shell pane that sends is named at that send, and its mail is from
     that name.
an7. A pane that asks for a name an unused AUTO name holds takes it: the
     auto holder gives it up and is named afresh when next listed. A
     self-given name is never taken that way, and neither is an auto name in
     use (one a send went out under or found its target by): asking for
     either is name_taken.
an8. A pane with no name that is sent to by `%N` is named by that send.
anz. No panic.

Run: python3 tests/frame/msg_autoname.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-autoname")
CONFIG = """
[general]
save_sessions = true
automatic_restore = true

[agents]
commands = ["sleep"]
"""

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def session(name):
    c = Client(srv.sock)
    c.hello()
    c.send({"CreateSession": {"name": name, "folder": None}})
    c.drain(0.6)
    c.close()


def spawn(sess, argv=None):
    c = Client(srv.sock)
    c.hello()
    body = {"session": sess, "placement": "SplitRight"}
    if argv is not None:
        body["argv"] = argv
    c.send({"CliSpawn": body})
    reply = None
    for m in c.drain(2.0):
        if name_of(m) in ("CliSpawned", "Error"):
            reply = m
            break
    c.close()
    return reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None


def run_cli(*args, timeout=20):
    done = subprocess.run([BIN, *args], env=clean_env, capture_output=True, timeout=timeout)
    return done.returncode, done.stdout.decode("utf-8", "replace"), done.stderr.decode("utf-8", "replace")


runs = [0]


def type_cli(pane_id, cmdline, timeout=10.0):
    """Run `cmdline` in `pane_id`'s shell: (exit code, stdout, stderr)."""
    runs[0] += 1
    base = f"{RUNDIR}/cli-{runs[0]}"
    line = f"{cmdline} > {base}.out 2> {base}.err; echo $? > {base}.tmp; mv {base}.tmp {base}.code\n"
    cli.send({"InputToPane": {"pane_id": pane_id, "data": list(line.encode())}})
    end = time.time() + timeout
    while time.time() < end:
        if os.path.exists(f"{base}.code"):
            code = int(open(f"{base}.code").read().strip() or "-1")
            return code, open(f"{base}.out").read(), open(f"{base}.err").read()
        time.sleep(0.05)
    return None, "", ""


def names():
    """{pane_id: name} from `agent ls --json`, which is what assigns them."""
    code, out, _ = run_cli("agent", "ls", "--json")
    try:
        return {p["pane_id"]: p["name"] for p in json.loads(out)["panes"]}
    except (ValueError, KeyError):
        return {}


def wait_names(want, timeout=8.0):
    end = time.time() + timeout
    got = {}
    while time.time() < end:
        got = names()
        if all(got.get(p) == n for p, n in want.items()):
            return got
        time.sleep(0.2)
    return got


def attach():
    c = Client(srv.sock)
    c.hello()
    c.send({"Attach": {"session_name": "build"}})
    c.send({"Resize": {"cols": 200, "rows": 50}})
    c.drain(0.6)
    return c


for var in ("REMUX_SESSION", "REMUX_PANE"):
    os.environ.pop(var, None)

srv = Server(RUNDIR)
try:
    srv.start(config=CONFIG)
    clean_env = {k: v for k, v in srv.env.items() if k not in ("REMUX_SESSION", "REMUX_PANE")}
    session("build")
    cli = attach()
    P1 = spawn("build", ["sleep", "1000"])
    P2 = spawn("build", ["sleep", "4"])
    time.sleep(0.8)

    # --- an1 ----------------------------------------------------------------
    got = wait_names({P1: "build", P2: "build-2"})
    check(got.get(P1) == "build" and got.get(P2) == "build-2",
          f"an1 two unnamed agents in `build` list as build and build-2 (got {got})")

    # --- an2 ----------------------------------------------------------------
    end = time.time() + 10
    while time.time() < end and P2 in names():
        time.sleep(0.3)
    P3 = spawn("build", ["sleep", "1000"])
    time.sleep(0.8)
    got = wait_names({P3: "build-3"})
    check(P2 not in got and got.get(P3) == "build-3",
          f"an2 after build-2 closes the next agent is build-3, never build-2 again (got {got})")

    # --- an3 ----------------------------------------------------------------
    cli.close()
    srv.restart()
    cli = attach()
    time.sleep(0.5)
    got = names()
    check(got.get(P1) == "build" and got.get(P3) == "build-3",
          f"an3 the names survive a restart (got {got})")
    P4 = spawn("build", ["sleep", "1000"])
    time.sleep(0.8)
    got = wait_names({P4: "build-4"})
    check(got.get(P4) == "build-4", f"an3 ...and so does the count: the next is build-4 (got {got})")

    # --- an4 ----------------------------------------------------------------
    for sess, want in (("Remux", "remux"), ("2024 work!", "s-2024-work"), ("user", "agent")):
        session(sess)
        pane = spawn(sess, ["sleep", "1000"])
        time.sleep(0.8)
        got = wait_names({pane: want})
        check(got.get(pane) == want, f"an4 session {sess!r} names its agent {want!r} (got {got.get(pane)!r})")

    # --- an5 ----------------------------------------------------------------
    code, out, err = run_cli("agent", "name", "alpha", "--pane", f"%{P3}")
    got = names()
    check(code == 0 and got.get(P3) == "alpha" and "build-3" not in got.values(),
          f"an5 `agent name alpha` replaces build-3 (exit {code}, err {err!r}, got {got})")
    S = spawn("build")
    time.sleep(0.8)
    code, out, err = type_cli(S, f"{BIN} msg send build-3 x")
    check(code == 1 and "no_such_target" in err,
          f"an5 ...and a send to build-3 is no_such_target (exit {code}, err {err!r})")

    # --- an6 ----------------------------------------------------------------
    code, out, err = type_cli(S, f"{BIN} msg send build \"$(printf 'AN:%s' six)\"")
    s_name = names().get(S)
    _, box, _ = run_cli("msg", "read", "--pane", f"%{P1}", "--all", "--peek", "--format", "json")
    try:
        froms = [m["from"]["Pane"]["name"] for m in json.loads(box)["messages"] if m["body"] == "AN:six"]
    except (ValueError, KeyError):
        froms = []
    check(code == 0 and s_name is not None and s_name.startswith("build-") and froms == [s_name],
          f"an6 a shell pane that sends is named, and its mail is from that name "
          f"(exit {code}, err {err!r}, name {s_name!r}, from {froms})")

    # --- an7 ----------------------------------------------------------------
    # P4's auto name came from a listing and nothing has used it.
    p4_name = names().get(P4)
    code, out, err = type_cli(S, f"{BIN} agent name {p4_name}")
    got = names()
    check(code == 0 and p4_name is not None and got.get(S) == p4_name and got.get(P4) not in (None, p4_name),
          f"an7 asking for {p4_name}, an unused auto name, takes it; its old holder gets a fresh one "
          f"(exit {code}, err {err!r}, got {got})")
    # `build` (P1's) was used: an6 sent to it by name.
    code, out, err = run_cli("agent", "name", "build", "--pane", f"%{P4}")
    check(code == 1 and "name_taken" in err and names().get(P1) == "build",
          f"an7 ...an auto name a send has used is never taken (exit {code}, err {err!r})")
    code, out, err = run_cli("agent", "name", "alpha", "--pane", f"%{P4}")
    check(code == 1 and "name_taken" in err,
          f"an7 ...nor is a self-given name (exit {code}, err {err!r})")

    # --- an8 ----------------------------------------------------------------
    T = spawn("build")
    time.sleep(0.8)
    code, out, err = type_cli(S, f"{BIN} msg send %{T} hi")
    # Read from the saved mailbox file, not `agent ls`, which would name T
    # itself and pass on a send that named nothing.
    t_name = None
    end = time.time() + 6
    while time.time() < end and t_name is None:
        try:
            with open(f"{RUNDIR}/data/remux/mailbox.json") as f:
                t_name = dict((p, n) for p, n in json.load(f)["names"]).get(T)
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(0.2)
    check(code == 0 and t_name is not None and t_name.startswith("build-"),
          f"an8 a pane sent to by %N is named by the send (exit {code}, err {err!r}, name {t_name!r})")

    time.sleep(0.3)
    check("panicked at" not in open(f"{RUNDIR}/state/remux/server.log").read(), "anz no panic")
finally:
    srv.kill()

print()
print("ALL PASS" if not failures else f"{len(failures)} FAILED")
sys.exit(1 if failures else 0)
