#!/usr/bin/env python3
"""Typing a message into an agent's pane, through the real client: the P1
definition of done.

Pane A is the client's first shell. Pane B is a split whose shell `exec`s
the fakeagent (`tests/frame/fakeagent.py`), so B's foreground IS the agent.
Every send runs in A's shell, the way an agent or a person runs it; every
key a person presses goes through the real client to the server's `Input`
path.

  D1  `remux msg send %B --deliver submit --wait --timeout 20 "GO:hello"`
      prints "typed into the pane" and "idle, answered after": the GO: hook
      makes the agent work for 2 s from the Enter. B read the message as one
      paste (the probe opener, then the envelope and GO:hello up to the
      paste's close), then the Enter as a separate read at least 290 ms
      later; and B's own screen shows the agent's work, painted by the real
      client.
  D2  `sleep 2; remux msg send %B --deliver submit "GO:again"` runs in A
      while a person moves to B and presses a, b, c, 300 ms apart. B reads
      the three keys first, and its screen shows the RX line for the last;
      the message's probe comes at least 3 s after the
      last key (B is attended, so the guard waits 3 s); and nothing comes
      between the paste's close and its Enter.
  D3  `remux msg read --pane %B --all --json` lists both messages Submitted.
  D4  `remux msg send %A --deliver submit x` from A, into A's own shell:
      exit 1, agent_not_ready.
  Dz  No panic in either log; the client is alive before teardown.

Every marker is ASSEMBLED by the shell (`"$(printf 'GO:%s' hello)"` typed,
`GO:hello` searched for), so the echo of a command is never mistaken for its
effect.

Run: python3 tests/pty/msg_submit_tui.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import re
import shutil
import subprocess
import sys
import time

import pexpect
import pyte

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
import fakeagent  # noqa: E402

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msgsubtui")
# Outside the run dir, which make_env wipes.
BINDIR = RUNDIR + "-bin"
FAKEAGENT = f"{BINDIR}/fakeagent"
COLS, ROWS = 110, 34
SERVER_LOG = f"{RUNDIR}/state/remux/server.log"
CLIENT_LOG = f"{RUNDIR}/state/remux/client.log"
AGENT_LOG, AGENT_CTL = f"{RUNDIR}/agent.log", f"{RUNDIR}/agent.ctl"
OPEN, CLOSE = b"\x1b[200~", b"\x1b[201~"

CONFIG = """
[agents]
commands = ["fakeagent"]
working_ms = 500
  [[agents.pattern]]
  name = "fake-proceed"
  command = "fakeagent"
  regex = "Do you want to proceed"

[messaging]
typing_guard_max_ms = 6000
"""

FAILURES = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


def make_env():
    shutil.rmtree(RUNDIR, ignore_errors=True)
    for sub in ("run", "state", "data", "config/remux", "out", "home"):
        os.makedirs(f"{RUNDIR}/{sub}", exist_ok=True)
    os.chmod(f"{RUNDIR}/run", 0o700)
    with open(f"{RUNDIR}/config/remux/config.toml", "w") as f:
        f.write(CONFIG)
    os.makedirs(BINDIR, exist_ok=True)
    fakeagent.install(FAKEAGENT)
    env = dict(os.environ)
    env.update(
        XDG_RUNTIME_DIR=f"{RUNDIR}/run",
        XDG_STATE_HOME=f"{RUNDIR}/state",
        XDG_DATA_HOME=f"{RUNDIR}/data",
        XDG_CONFIG_HOME=f"{RUNDIR}/config",
        SHELL="/bin/sh",
        ENV="/dev/null",
        TERM="xterm-256color",
        REMUX_ALLOW_NESTED="1",
        # On a Mac the XDG variables do not cover every path the server
        # resolves, and an empty state falls back to the platform path once,
        # so a HOME of its own keeps the user's real sessions out.
        HOME=f"{RUNDIR}/home",
        PS1="> ",
    )
    env.pop("REMUX_SESSION", None)
    env.pop("REMUX_PANE", None)
    return env


def spawn(env, args=()):
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, list(args), env=env, dimensions=(ROWS, COLS), encoding=None)

    def pump(t=0.8):
        end = time.time() + t
        while time.time() < end:
            try:
                chunk = child.read_nonblocking(65536, 0.1)
            except Exception:
                continue
            stream.feed(chunk)

    pump(1.5)
    return child, screen, pump


def dump(screen, label=""):
    print(f"----- screen {label} -----")
    for i, r in enumerate(screen.display):
        if r.strip():
            print(f"{i:2} |{r.rstrip()}")
    print("-------------------------")


def type_line(child, pump, line, t=0.6):
    child.send(line.encode() + b"\r")
    pump(t)


IDENT_RE = re.compile(r"ID-(\w+):\[([^\]]*)\]\[([^\]]*)\]")


def ident(child, pump, screen, tag, timeout=10.0):
    """The focused pane's (session, pane) as its shell reports them."""
    type_line(child, pump, f"""printf 'ID-%s:[%s][%s]\\n' {tag} "$REMUX_SESSION" "$REMUX_PANE" """)
    end = time.time() + timeout
    while time.time() < end:
        pump(0.4)
        for row in screen.display:
            for m in IDENT_RE.finditer(row):
                if m.group(1) == tag:
                    return m.group(2), m.group(3)
    return None


runs = [0]


def start_in_pane(child, cmdline):
    """Type `cmdline` into the focused pane's shell with its output going to
    files, without waiting for it: the base path of those files."""
    runs[0] += 1
    base = f"{RUNDIR}/out/r{runs[0]}"
    child.send(f"{cmdline} > {base}.out 2> {base}.err; echo $? > {base}.tmp; mv {base}.tmp {base}.code\r".encode())
    return base


def finished(pump, base, timeout):
    """(exit code, stdout, stderr) of a command started with start_in_pane."""
    end = time.time() + timeout
    while time.time() < end:
        pump(0.2)
        if os.path.exists(f"{base}.code"):
            code = int(open(f"{base}.code").read().strip() or "-1")
            return code, open(f"{base}.out").read(), open(f"{base}.err").read()
    return None, "", ""


def run_in_pane(child, pump, cmdline, timeout=10.0):
    return finished(pump, start_in_pane(child, cmdline), timeout)


def outside(env, *args, timeout=20):
    """The CLI from the harness itself, outside every pane."""
    done = subprocess.run([BIN, *args], env=env, capture_output=True, timeout=timeout)
    return done.returncode, done.stdout.decode("utf-8", "replace"), done.stderr.decode("utf-8", "replace")


def agent_state(env, pane):
    code, out, _ = outside(env, "agent", "status", f"%{pane}", "--json")
    try:
        return json.loads(out)["panes"][0]["state"]
    except (ValueError, KeyError, IndexError):
        return None


def wait_state(env, pane, want, timeout=10.0):
    end = time.time() + timeout
    while time.time() < end:
        if agent_state(env, pane) == want:
            return True
        time.sleep(0.2)
    return False


def reads():
    """Every read the fakeagent made: (time_ns, bytes)."""
    try:
        lines = open(AGENT_LOG).read().splitlines()
    except OSError:
        return []
    out = []
    for ln in lines:
        parts = ln.split()
        if ln.startswith("RX ") and len(parts) >= 2:
            out.append((int(parts[1]), bytes.fromhex(parts[2] if len(parts) > 2 else "")))
    return out


def message(marker):
    """The reads that carried the message holding `marker`: the index of its
    probe (a paste opener read on its own), the index of the read that
    closed its paste, and the bytes from the probe to the close. None while
    it has not all arrived."""
    got = reads()
    for i, (_, data) in enumerate(got):
        if data != OPEN:
            continue
        joined = b""
        for j in range(i, len(got)):
            joined += got[j][1]
            if CLOSE in got[j][1]:
                if marker.encode() in joined:
                    return i, j, joined
                break
    return None


def wait_message(marker, timeout):
    end = time.time() + timeout
    while time.time() < end:
        found = message(marker)
        if found is not None and len(reads()) > found[1] + 1:
            return found
        time.sleep(0.1)
    return message(marker)


def log_text(path):
    try:
        return open(path).read()
    except OSError:
        return ""


env = make_env()
child, screen, pump = spawn(env)
A = B = None
try:
    got = ident(child, pump, screen, "a")
    check(got is not None and got[0] == "main" and got[1].isdigit(), f"setup: pane A reports its identity (got {got})")
    if got is None:
        dump(screen, "A")
        raise SystemExit(1)
    A = got[1]
    type_line(child, pump, f"{BIN} split", t=1.5)
    got = ident(child, pump, screen, "b")
    check(got is not None and got[1].isdigit() and got[1] != A, f"setup: a split below is pane B (got {got})")
    if got is None:
        dump(screen, "B")
        raise SystemExit(1)
    B = got[1]
    type_line(child, pump, f"exec {FAKEAGENT} {AGENT_LOG} {AGENT_CTL}", t=1.0)
    end = time.time() + 8
    while time.time() < end and not os.path.exists(AGENT_LOG):
        pump(0.2)
    fakeagent.tell(AGENT_CTL, AGENT_LOG, "paste-on")
    child.send(b"\x1bk")
    pump(0.6)
    got = ident(child, pump, screen, "a2")
    check(got is not None and got[1] == A, f"setup: Alt+k focuses A again (got {got})")
    check(wait_state(env, B, "Idle"), f"setup: B's fakeagent is detected and idle (state {agent_state(env, B)})")

    # --- D1 ---------------------------------------------------------------
    code, out, err = run_in_pane(
        child, pump,
        f"""{BIN} msg send %{B} --deliver submit --wait --timeout 20 "$(printf 'GO:%s' hello)" """,
        timeout=30)
    check(code == 0 and "typed into the pane" in out and "idle, answered after" in out,
          f"D1 the send says it was typed and the turn ended idle (exit {code}, out {out!r}, err {err!r})")
    found = wait_message("GO:hello", 5)
    got = reads()
    if found is not None:
        probe, closed, joined = found
        enter = got[closed + 1] if closed + 1 < len(got) else None
        gap = (enter[0] - got[closed][0]) / 1e6 if enter else None
        framed = re.fullmatch(
            re.escape(OPEN) + rb"# \[remux message #(\d+) from [^\n]*, mark ([0-9a-f]{16}); reply: [^\n]*\]"
            rb"\nGO:hello\n# \[end of remux message #\1, mark \2\]" + re.escape(CLOSE), joined, re.S)
        check(framed and enter is not None and enter[1] == b"\r" and gap >= 290,
              f"D1 B read the message as one paste, then the Enter alone {gap and round(gap)} ms later "
              f"(paste {joined!r}, then {enter and enter[1]!r})")
    else:
        check(False, f"D1 B read the message (reads {[d[:16] for _, d in got]})")
    pump(0.5)
    check(any("working..." in r for r in screen.display),
          "D1 B's screen shows the agent's work, painted by the client")

    # --- D2 ---------------------------------------------------------------
    check(wait_state(env, B, "Idle", timeout=10), f"...B is idle before the next send (state {agent_state(env, B)})")
    keys_from = len(reads())
    base = start_in_pane(child, f"""sleep 2; {BIN} msg send %{B} --deliver submit "$(printf 'GO:%s' again)" """)
    pump(0.3)
    child.send(b"\x1bj")
    pump(0.3)
    for key in (b"a", b"b", b"c"):
        child.send(key)
        pump(0.3)
    check(any(re.search(r"RX \d+ 63\b", r) for r in screen.display),
          "D2 B's screen shows the fakeagent's read of the person's c, painted by the client")
    code, out, err = finished(pump, base, timeout=30)
    found = wait_message("GO:again", 5)
    got = reads()
    keys = [(i, ns) for i, (ns, data) in enumerate(got) if i >= keys_from and data in (b"a", b"b", b"c")]
    order = [got[i][1] for i, _ in keys]
    if found is not None and len(keys) == 3:
        probe, closed, _ = found
        c_ns = keys[-1][1]
        quiet = (got[probe][0] - c_ns) / 1e6
        enter = got[closed + 1][1] if closed + 1 < len(got) else None
        check(code == 0 and "typed into the pane" in out, f"D2 the send was typed (exit {code}, out {out!r}, err {err!r})")
        check(order == [b"a", b"b", b"c"] and keys[-1][0] < probe,
              f"D2 B read the person's a, b, c before the message (order {order})")
        # The guard counts from when the server got the key; these times are
        # when the agent read the key and the probe, and a key's read can
        # trail its arrival by more than the probe's does. So the bound is
        # 100 ms under the 3 s guard rather than on it.
        check(quiet >= 2900, f"D2 the message's probe came {round(quiet)} ms after the last key, past the 3 s guard")
        check(enter == b"\r", f"D2 nothing came between the paste's close and its Enter (next read {enter!r})")
    else:
        check(False, f"D2 B read the keys and the message (keys {order}, message {found is not None}, "
                     f"exit {code}, err {err!r})")
    child.send(b"\x1bk")
    pump(0.6)
    got_a = ident(child, pump, screen, "a3")
    check(got_a is not None and got_a[1] == A, f"...Alt+k focuses A again (got {got_a})")

    # --- D3 ---------------------------------------------------------------
    code, out, _ = outside(env, "msg", "read", "--pane", f"%{B}", "--all", "--peek", "--format", "json")
    try:
        states = {m["body"]: m["state"] for m in json.loads(out)["messages"]}
    except (ValueError, KeyError):
        states = {}
    check(states.get("GO:hello") == "Submitted" and states.get("GO:again") == "Submitted",
          f"D3 both messages are Submitted in B's mailbox (got {states})")

    # --- D4 ---------------------------------------------------------------
    code, out, err = run_in_pane(child, pump, f"{BIN} msg send %{A} --deliver submit x")
    check(code == 1 and "agent_not_ready" in err,
          f"D4 a submit into A's own shell is agent_not_ready, exit 1 (exit {code}, err {err!r})")
    check(child.isalive(), "Dz the client is alive before teardown")
finally:
    if child is not None:
        try:
            child.close(force=True)
        except Exception:
            pass
    subprocess.run([BIN, "stop"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)

for path in (SERVER_LOG, CLIENT_LOG):
    check("panicked at" not in log_text(path), f"Dz no panic in {os.path.basename(path)}")

print()
if FAILURES:
    print(f"{len(FAILURES)} FAILED")
    sys.exit(1)
print("ALL PASS")
