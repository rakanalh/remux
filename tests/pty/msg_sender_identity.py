#!/usr/bin/env python3
"""Who sent it: the sender-identity proof, and the P0 definition of done.

A message's sender is the pane the SENDING PROCESS lives in, found from the
process (its session id, else a walk up its parents), never from anything the
caller says about itself. This harness types every send into a real pane of a
real client, so the process tree is the one a user or an agent has:

  9a  `remux agent name` in each pane; `agent ls --json` shows both names.
  9b  A send under a LYING $REMUX_PANE (naming the recipient) is from the
      real pane, with its name; the server logs the disagreement, and the
      identity line says `by Sid`.
  9c  A `nohup ... &` send is still from the pane.
  9d  A `setsid -w` send is attributed by the walk (`by Walk { depth: 2 }`:
      the CLI, then setsid, then the pane's shell).
  9e  A plain `setsid sh -c '...'` send typed at the shell is REFUSED
      sender_unverified: the pane shows it, the server logs the refusal and
      the peer pid it could not place, and the message never arrives.
  9f  A send inside `script` (a nested pty) is from the pane, and the identity
      line says its tty differs.
  9g  From the harness's own shell (no pane): a send is refused
      sender_unverified; a read of the pane's mailbox is a peek.
  SEC1 A double fork WITHOUT setsid, `( remux msg send ... & )`, is from A:
      the orphan is reparented, but it keeps the pane's session.
  SEC2 `env -u REMUX_PANE -u REMUX_SESSION remux msg send ...` is from A: the
      variables are hints, and removing them changes nothing.
  SEC3 Inside A, `REMUX_PANE=%<B> REMUX_SESSION=main remux msg read --pane
      %<B>` is a peek: the notice is printed and B's mail stays Queued.
  9h  `agent status alpha --json` is the recipient with its unread mail;
      `agent status %<A> --json` names lead. JSON, because the table is for
      people and a field can look like a column.
  9i  THE DEFINITION OF DONE: the client closes, `remux restart` restarts the
      server, a client re-attaches to the restored session, and in the
      restored recipient pane `remux msg read` shows `#1 from lead (%<A>)`
      above `MSG:one`, a peek then shows every message Read, and a second
      read prints nothing and exits 0.
  9j  After the restart, A acknowledges B's mailbox with --all: the log's ack
      line names B as the box and A as the caller.
  9z  No panic in either log; the client was alive before each teardown.

The plain-`setsid` case holds ONLY from an interactive shell with job
control: a command run from one is already a process-group leader, so
`setsid(1)` forks, its parent exits, and the child is reparented to init --
unverifiable by design (§6.5 rulings 3-4). Under a `CliSpawn`'d `sh -c` there
is no job control, the command is not a group leader, `setsid(1)` does not
fork, and the case passes for the wrong reason: a surplus pass. The cases
run `tests/portable.py`'s stand-ins rather than `setsid(1)`, which macOS does
not ship; they always fork, which is the interactive-shell shape.

Every marker is ASSEMBLED by the shell (`"$(printf 'MSG:%s' one)"` typed,
`MSG:one` searched for), so a marker is never on screen as the echo of the
command that should have produced it.

Run: python3 tests/pty/msg_sender_identity.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time

import pexpect
import pyte

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from portable import SETSID_F, SETSID_W  # noqa: E402

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msgid")
COLS, ROWS = 110, 34
SOCK = f"{RUNDIR}/run/remux.sock"
SERVER_LOG = f"{RUNDIR}/state/remux/server.log"
CLIENT_LOG = f"{RUNDIR}/state/remux/client.log"

FAILURES = []


def shell_words(argv):
    return " ".join(shlex.quote(a) for a in argv)


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


for tool in ("script", "nohup"):
    if shutil.which(tool) is None:
        sys.exit(f"msg_sender_identity: `{tool}` is not installed; the case needing it cannot run")


def make_env():
    shutil.rmtree(RUNDIR, ignore_errors=True)
    for sub in ("run", "state", "data", "config", "out"):
        os.makedirs(f"{RUNDIR}/{sub}", exist_ok=True)
    os.chmod(f"{RUNDIR}/run", 0o700)
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
        PS1="> ",
    )
    # The developer may be inside remux: an inherited REMUX_PANE would reach
    # the harness's own "outside any pane" calls as a hint.
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


def wait_for(pump, screen, want, timeout=10.0):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.4)
        if any(want in r for r in screen.display):
            return True
    return False


IDENT_RE = re.compile(r"ID-(\w+):\[([^\]]*)\]\[([^\]]*)\]")


def ident(child, pump, screen, tag, timeout=10.0):
    """The focused pane's (session, pane) as its shell reports them. The tag
    is assembled by printf, so an old answer on screen cannot be mistaken
    for this one."""
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


def run_in_pane(child, pump, cmdline, timeout=10.0):
    """Run `cmdline` in the focused pane's shell: (exit code, stdout, stderr)."""
    runs[0] += 1
    base = f"{RUNDIR}/out/r{runs[0]}"
    child.send(f"{cmdline} > {base}.out 2> {base}.err; echo $? > {base}.tmp; mv {base}.tmp {base}.code\r".encode())
    end = time.time() + timeout
    while time.time() < end:
        pump(0.2)
        if os.path.exists(f"{base}.code"):
            code = int(open(f"{base}.code").read().strip() or "-1")
            return code, open(f"{base}.out").read(), open(f"{base}.err").read()
    return None, "", ""


def outside(env, *args, timeout=20):
    """The CLI from the harness itself, outside every pane."""
    done = subprocess.run([BIN, *args], env=env, capture_output=True, timeout=timeout)
    return done.returncode, done.stdout.decode("utf-8", "replace"), done.stderr.decode("utf-8", "replace")


def box(env, pane):
    code, out, _ = outside(env, "msg", "read", "--pane", f"%{pane}", "--all", "--peek", "--format", "json")
    try:
        return json.loads(out)["messages"]
    except (ValueError, KeyError):
        return []


def arrived(env, pane, body, timeout=6.0):
    end = time.time() + timeout
    while time.time() < end:
        found = [m for m in box(env, pane) if m["body"] == body]
        if found:
            return found[0]
        time.sleep(0.2)
    return None


def server_log():
    try:
        return open(SERVER_LOG).read()
    except OSError:
        return ""


def last_send_identity(pane):
    lines = [ln for ln in server_log().splitlines() if f"MsgSend caller -> pane %{pane} by " in ln]
    return lines[-1] if lines else ""


def alive(child):
    return child.isalive()


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

    # --- 9a ---------------------------------------------------------------
    code, out, err = run_in_pane(child, pump, f"{BIN} agent name alpha")
    check(code == 0 and out.strip() == f'pane %{B} is now "alpha"', f"9a B names itself alpha (exit {code}, out {out!r}, err {err!r})")
    child.send(b"\x1bk")
    pump(0.6)
    got = ident(child, pump, screen, "a2")
    check(got is not None and got[1] == A, f"setup: Alt+k focuses A again (got {got})")
    code, out, err = run_in_pane(child, pump, f"{BIN} agent name lead")
    check(code == 0 and out.strip() == f'pane %{A} is now "lead"', f"9a A names itself lead (exit {code}, out {out!r}, err {err!r})")
    code, out, _ = outside(env, "agent", "ls", "--json")
    try:
        names = {str(p["pane_id"]): p["name"] for p in json.loads(out)["panes"]}
    except (ValueError, KeyError):
        names = {}
    check(names.get(A) == "lead" and names.get(B) == "alpha", f"9a agent ls --json: A is lead, B is alpha (got {names})")

    # --- 9b ---------------------------------------------------------------
    code, out, err = run_in_pane(child, pump, f"""REMUX_PANE=%{B} {BIN} msg send alpha "$(printf 'MSG:%s' one)" """)
    check(code == 0 and out.startswith(f"sent #1 to %{B} (alpha)"), f"9b the send under REMUX_PANE=%{B} is #1 (exit {code}, out {out!r}, err {err!r})")
    m = arrived(env, B, "MSG:one")
    frm = (m or {}).get("from", {}).get("Pane", {})
    check(m is not None and str(frm.get("pane_id")) == A and frm.get("name") == "lead",
          f"9b MSG:one is from %{A} (lead), NOT from the %{B} the caller claimed (got {frm})")
    check(f"pane_hint=%{B} disagrees with attribution %{A}" in server_log(), "9b the server logs the disagreement")
    line = last_send_identity(A)
    # macOS has no sid to attribute by; the walk finds the CLI's parent shell.
    how = "by Walk { depth: 1 }" if sys.platform == "darwin" else "by Sid"
    check(line.endswith(how), f"9b the identity line says {how} (got {line!r})")

    # --- 9c ---------------------------------------------------------------
    type_line(child, pump, f"""nohup {BIN} msg send %{B} "$(printf 'MSG:%s' two)" >/dev/null 2>&1 &""")
    m = arrived(env, B, "MSG:two")
    check(m is not None and str(m["from"]["Pane"]["pane_id"]) == A, f"9c a nohup'd background send is from %{A} (got {m and m['from']})")

    # --- 9d ---------------------------------------------------------------
    code, out, err = run_in_pane(child, pump, f"""{shell_words(SETSID_W)} {BIN} msg send %{B} "$(printf 'MSG:%s' three)" """)
    m = arrived(env, B, "MSG:three")
    check(code == 0 and m is not None and str(m["from"]["Pane"]["pane_id"]) == A,
          f"9d a setsid -w send is from %{A} (exit {code}, err {err!r})")
    line = last_send_identity(A)
    check(line.endswith("by Walk { depth: 2 }"), f"9d ...attributed by the walk, two links up (got {line!r})")

    # --- 9e ---------------------------------------------------------------
    type_line(child, pump, f"""{shell_words(SETSID_F)} /bin/sh -c 'sleep 0.2; {BIN} msg send %{B} "$(printf "MSG:%s" four)"'""")
    check(wait_for(pump, screen, "sender_unverified", timeout=8.0),
          "9e the daemonized send is refused: the pane shows sender_unverified")
    time.sleep(3.0)
    check(not any(m["body"] == "MSG:four" for m in box(env, B)), "9e ...and MSG:four never arrives")
    log = server_log()
    refused = re.findall(r"msg: sender unverified: client_id=(\d+)", log)
    peers = [re.search(rf"identity: client_id={cid} peer=Some\((\d+)\)", log) for cid in refused]
    check(refused and peers[-1] is not None,
          f"9e the server logs the refusal and the peer pid it could not place (refusals {refused})")

    # --- 9f ---------------------------------------------------------------
    if sys.platform == "darwin":
        nested = f"""script -q /dev/null /bin/sh -c '{BIN} msg send %{B} "$(printf "MSG:%s" five)"'"""
    else:
        nested = f"""script -qc '{BIN} msg send %{B} "$(printf "MSG:%s" five)"' /dev/null"""
    code, out, err = run_in_pane(child, pump, nested)
    m = arrived(env, B, "MSG:five")
    check(code == 0 and m is not None and str(m["from"]["Pane"]["pane_id"]) == A,
          f"9f a send inside script is from %{A} (exit {code}, err {err!r})")
    line = last_send_identity(A)
    check("by Walk" in line and line.endswith("its tty differs (nested pty?)"),
          f"9f ...by the walk, and its tty differs (got {line!r})")

    # --- 9g ---------------------------------------------------------------
    code, out, err = outside(env, "msg", "send", f"%{B}", "x")
    check(code == 1 and err.startswith("remux: sender_unverified: "), f"9g a send from outside every pane is refused (exit {code}, err {err!r})")
    first = [(m["id"], m["state"]) for m in box(env, B)]
    code, out, err = outside(env, "msg", "read", "--pane", f"%{B}", "--format", "json")
    second = [(m["id"], m["state"]) for m in box(env, B)]
    check(code == 0 and "(peek: not your mailbox)" in err and first and first == second
          and all(state == "Queued" for _, state in second),
          f"9g an outside read is a peek and leaves every message Queued ({first} -> {second})")

    # --- SEC1 -------------------------------------------------------------
    refused_before = len(re.findall(r"msg: sender unverified: client_id=\d+ not a descendant of any pane", server_log()))
    type_line(child, pump, f"""( {BIN} msg send %{B} "$(printf 'MSG:%s' sec1)" >/dev/null 2>&1 & )""")
    m = arrived(env, B, "MSG:sec1")
    if sys.platform == "darwin":
        # A known macOS limitation: there is no sid to attribute by, and the
        # reparented orphan's walk reaches init, so the send is refused.
        # Failing closed is the accepted outcome; Linux credits it by Sid.
        refused_after = len(re.findall(r"msg: sender unverified: client_id=\d+ not a descendant of any pane",
                                        server_log()))
        check(m is None and refused_after > refused_before,
              f"SEC1 a double-forked send is refused on macOS, not a descendant of any pane, and nothing is stored "
              f"(got {m and m['from']}, refusals {refused_before} -> {refused_after})")
    else:
        check(m is not None and str(m["from"]["Pane"]["pane_id"]) == A,
              f"SEC1 a double-forked send without setsid is from %{A} (got {m and m['from']})")

    # --- SEC2 -------------------------------------------------------------
    code, out, err = run_in_pane(child, pump, f"""env -u REMUX_PANE -u REMUX_SESSION {BIN} msg send %{B} "$(printf 'MSG:%s' sec2)" """)
    m = arrived(env, B, "MSG:sec2")
    check(code == 0 and m is not None and str(m["from"]["Pane"]["pane_id"]) == A,
          f"SEC2 a send with REMUX_PANE and REMUX_SESSION unset is from %{A} (exit {code}, err {err!r})")

    # --- SEC3 -------------------------------------------------------------
    before = [(m["id"], m["state"]) for m in box(env, B)]
    code, out, err = run_in_pane(child, pump, f"REMUX_PANE=%{B} REMUX_SESSION=main {BIN} msg read --pane %{B}")
    after = [(m["id"], m["state"]) for m in box(env, B)]
    check(code == 0 and "MSG:one" in out and "(peek: not your mailbox)" in err and before == after
          and all(state == "Queued" for _, state in after),
          f"SEC3 A posing as %{B} only peeks: notice printed, B's mail still Queued (err {err!r}, {before} -> {after})")

    # --- 9h ---------------------------------------------------------------
    def status_json(target):
        code, out, err = run_in_pane(child, pump, f"{BIN} agent status {target} --json")
        try:
            return code, json.loads(out)["panes"]
        except (ValueError, KeyError):
            return code, []
    code, panes = status_json("alpha")
    check(code == 0 and len(panes) == 1 and str(panes[0]["pane_id"]) == B and panes[0]["name"] == "alpha"
          and panes[0]["unread"] >= 1, f"9h agent status alpha is %{B} with its unread mail (exit {code}, got {panes})")
    code, panes = status_json(f"%{A}")
    check(code == 0 and len(panes) == 1 and panes[0]["name"] == "lead", f"9h agent status %{A} is lead (exit {code}, got {panes})")

    check(alive(child), "9z the client is alive before it is closed")
finally:
    try:
        child.close(force=True)
    except Exception:
        pass

# --- 9i: the definition of done ---------------------------------------------
child = None
try:
    if A is None or B is None:
        raise SystemExit("setup failed; 9i cannot run")
    starts_before = server_log().count("server listening on ")
    done = subprocess.run([BIN, "restart"], env=env, capture_output=True, timeout=30)
    check(done.returncode == 0, f"9i remux restart exits 0 (exit {done.returncode}, err {done.stderr[:200]!r})")
    # `attach` starts a server if it finds none, which would test a fresh,
    # empty one: wait for the restarted server to list the restored session.
    end, listed = time.time() + 10, ""
    while time.time() < end:
        code, listed, _ = outside(env, "ls", timeout=10)
        if code == 0 and "main" in listed:
            break
        time.sleep(0.2)
    check("main" in listed, f"9i the restarted server lists the restored session (got {listed!r})")
    check(server_log().count("server listening on ") == starts_before + 1,
          "9i a NEW server process is serving (one more 'server listening' line)")

    child, screen, pump = spawn(env, ["attach", "main"])
    got = ident(child, pump, screen, "r1")
    if got is not None and got[1] != B:
        child.send(b"\x1bj")
        pump(0.6)
        got = ident(child, pump, screen, "r2")
    check(got is not None and got[1] == B, f"9i the focused restored pane's REMUX_PANE is B (got {got}, B={B})")
    child.send(b"\x01f")
    pump(0.8)
    # Piped through head so the header stays on screen however many cases
    # before this one sent B mail: the first message is the one asserted.
    type_line(child, pump, f"clear; {BIN} msg read | head -n 8", t=1.5)
    header = f"#1 from lead (%{A}) in main at "
    ok = wait_for(pump, screen, header, timeout=8.0)
    rows = screen.display
    at = next((i for i, r in enumerate(rows) if header in r), None)
    # Compare from the header's column, and without the right border: the
    # zoomed pane is framed on both sides.
    col = rows[at].index(header) if at is not None else 0
    body_ok = at is not None and at + 1 < len(rows) and rows[at + 1][col:].rstrip(" │") == "    MSG:one"
    check(ok and body_ok, f"9i after the restart, B's own read shows '#1 from lead (%{A})' above MSG:one")
    if not (ok and body_ok):
        dump(screen, "9i read")
    states = [m["state"] for m in box(env, B)]
    check(states and all(state == "Read" for state in states), f"9i ...and it marked every message Read (got {states})")
    type_line(child, pump, f"""clear; {BIN} msg read; printf 'END:%s:%s\\n' second "$?" """, t=1.5)
    wait_for(pump, screen, "END:second:", timeout=8.0)
    check(any("END:second:0" in r for r in screen.display) and not any(" from lead (" in r for r in screen.display),
          "9i a second read prints nothing and exits 0: the first marked everything")

    # --- 9j ---------------------------------------------------------------
    child.send(b"\x01f")
    pump(0.8)
    child.send(b"\x1bk")
    pump(0.6)
    got = ident(child, pump, screen, "j1")
    check(got is not None and got[1] == A, f"9j Alt+k focuses the restored A (got {got})")
    code, out, err = run_in_pane(child, pump, f"""{BIN} msg send %{B} "$(printf 'MSG:%s' ackme)" """)
    code, out, err = run_in_pane(child, pump, f"{BIN} msg ack --pane %{B} --all")
    check(code == 0 and out.strip() == "acked 1", f"9j A acks B's one unread message (exit {code}, out {out!r}, err {err!r})")
    check(re.search(rf"msg: acked pane=%{B} n=1 by=%{A}\b", server_log()) is not None,
          "9j the log's ack line names B's box and A as the caller")
    check(alive(child), "9z the re-attached client is alive before it is closed")
finally:
    if child is not None:
        try:
            child.close(force=True)
        except Exception:
            pass
    subprocess.run([BIN, "stop"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)

for path in (SERVER_LOG, CLIENT_LOG):
    text = open(path).read() if os.path.exists(path) else ""
    check("panicked at" not in text, f"9z no panic in {os.path.basename(path)}")

print()
if FAILURES:
    print(f"{len(FAILURES)} FAILED")
    sys.exit(1)
print("ALL PASS")
