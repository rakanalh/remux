"""ssh's stderr never reaches the user's terminal, and a failed dial says why.

The report: mouse clicks stopped working mid-session while attached to a remote,
and restarting the client fixed it. The client used to spawn `ssh` with stderr
INHERITED, so for the life of the connection anything ssh or the far side wrote
there (a chatty `~/.bashrc`, a ProxyCommand, escape codes) went straight onto the
outer terminal, raw mode or not. `\\e[?1000l\\e[?1006l` on that stderr switches
the client's mouse reporting off for good.

Everything runs through the fake-remote recipe: a second isolated server, and an
`ssh` shim on PATH. The remotes are dialled from the SESSION MANAGER (Enter on an
offline row), i.e. after the client is in raw mode -- the case the old comment
said could not happen. `auto_connect` is off on purpose: it dials before raw mode,
where a DECRST is harmless because the client enables the mouse afterwards.

  `mini` (good shim): writes `LEAK` + the two DECRSTs, a normal line, then
     ~75 KB of filler to stderr, and only THEN execs the relay. The filler is
     larger than a pipe buffer, so a client that stops draining stderr wedges
     the shim before the relay ever runs and the connect times out.
  `bad` (failing shim): writes `ssh: Could not resolve hostname badhost: ...`
     and exits 255, which is what OpenSSH does for an unknown host.

Checks, and why each one cannot pass vacuously:
  1 the raw bytes the client wrote to the outer terminal, between pressing Enter
    and the remote showing as connected, hold neither the marker nor the DECRST
    pair. Read from the raw pty stream, never from pyte, and only inside that
    window: the client writes its own `?1000l` when it exits. The positive
    controls are checks 2 and 3 in the same run: the remote connected and the
    shim's line reached client.log, so the shim did run and did write.
  2 the remote connected (its row has no state suffix and its session is listed).
  3 client.log holds the line with the ESCs escaped (`\\x1b[?1000l`) and the
    line itself holds no raw ESC byte.
  4 the filler was capped: client.log carries the suppressed-lines note and does
    not hold every filler line.
  5 `bad`'s row reads `(failed: ...)` naming `Could not resolve hostname`, and so
    does the registry's failure line in client.log.
  6 `remux attach-remote` against `badhost` exits with an error naming it.
  7 `slow` writes one line and then holds stderr open from a background child
    (a ProxyCommand that outlives ssh): the dial times out, the row names that
    line, and the failure lands within the timeout plus the tail's bound.
  8 the two dials made BEFORE raw mode, `attach-remote` and `auto_connect`,
    echo the escaped lines, because a 2FA menu and the sshd banner reach ssh's
    stderr and not /dev/tty. The echo is escaped, so the DECRST pair is still
    absent from the outer terminal.

Run from the repo root (debug build):
    python3 tests/pty/ssh_stderr.py [-v]
"""
import json, os, shutil, socket, struct, subprocess, sys, time, traceback

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import (  # noqa: E402
    PROTOCOL_VERSION, sm_open, sm_tree, sm_selected, sm_goto,
)
from rundirs import relay_pids, run_dir  # noqa: E402
import pexpect  # noqa: E402
import pyte  # noqa: E402

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUN = run_dir("rmxsshe")
SOCK1 = f"{RUN}/run/remux.sock"
SOCK2 = f"{RUN}/run2/remux.sock"
VERBOSE = "-v" in sys.argv

DECRST = b"\x1b[?1000l\x1b[?1006l"
FILLER_LINES = 500
RESOLVE = "ssh: Could not resolve hostname badhost: Name or service not known"
SLOW = "proxy: still waiting for the jump host"

fails = []


def check(label, ok, detail=None):
    print(("  PASS  " if ok else "  FAIL  ") + label)
    if not ok:
        if detail is not None:
            print("        ", detail)
        fails.append(label)
    return ok


def base_env(rundir, statedir, datadir, cfgdir):
    return {
        **os.environ,
        "PATH": f"{RUN}/bin:" + os.environ.get("PATH", ""),
        "HOME": f"{RUN}/home",
        "XDG_RUNTIME_DIR": rundir,
        "XDG_STATE_HOME": statedir,
        "XDG_DATA_HOME": datadir,
        "XDG_CONFIG_HOME": cfgdir,
        "SHELL": "/bin/sh",
        "ENV": "/dev/null",
        "TERM": "xterm-256color",
        "PS1": "$ ",
        "REMUX_ALLOW_NESTED": "1",
    }


def env_local(cfgdir):
    return base_env(f"{RUN}/run", f"{RUN}/state", f"{RUN}/data", cfgdir)


def env_remote():
    return base_env(f"{RUN}/run2", f"{RUN}/state2", f"{RUN}/data2", f"{RUN}/cfg-none")


def write_config(cfgdir, body):
    os.makedirs(f"{cfgdir}/remux", exist_ok=True)
    with open(f"{cfgdir}/remux/config.toml", "w") as f:
        f.write(body)


def write_shim():
    shim = f"{RUN}/bin/ssh"
    with open(shim, "w") as f:
        f.write(
            "#!/bin/sh\n"
            'case "$*" in\n'
            f"  *badhost*) echo '{RESOLVE}' >&2; exit 255 ;;\n"
            f"  *slowhost*) echo '{SLOW}' >&2; sleep 20 & exec sleep 20 ;;\n"
            "esac\n"
            "printf 'LEAK\\033[?1000l\\033[?1006l\\n' >&2\n"
            "printf 'motd: welcome to the fake remote\\n' >&2\n"
            "i=0\n"
            f"while [ $i -lt {FILLER_LINES} ]; do\n"
            "  printf 'filler %04d xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
            "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' $i >&2\n"
            "  i=$((i+1))\n"
            "done\n"
            f"export XDG_RUNTIME_DIR={RUN}/run2\n"
            f"export XDG_STATE_HOME={RUN}/state2\n"
            f"export XDG_DATA_HOME={RUN}/data2\n"
            f"echo $$ >> {relay_pids()}\n" f"exec {BIN} relay\n"
        )
    os.chmod(shim, 0o755)


def setup_dirs():
    shutil.rmtree(RUN, ignore_errors=True)
    for s in ("run", "run2", "state", "state2", "data", "data2", "bin",
              "cfg", "cfg-auto", "cfg-none", "home"):
        os.makedirs(f"{RUN}/{s}", exist_ok=True)
    write_shim()
    write_config(f"{RUN}/cfg",
                 '[remotes.mini]\nssh = "whatever"\nremux_path = "remux"\n\n'
                 '[remotes.bad]\nssh = "badhost"\nremux_path = "remux"\n\n'
                 '[remotes.slow]\nssh = "slowhost"\nremux_path = "remux"\n')
    write_config(f"{RUN}/cfg-auto",
                 '[remotes.mini]\nssh = "whatever"\nremux_path = "remux"\n'
                 'auto_connect = true\n')
    write_config(f"{RUN}/cfg-none", "")


def start_server(sock, env):
    p = subprocess.Popen([BIN, "server"], env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        if os.path.exists(sock):
            time.sleep(0.3)
            return p
        time.sleep(0.05)
    p.kill()
    raise SystemExit(f"server socket {sock} never appeared")


def seed_remote_session():
    """A session on the far server, so a connected remote has a subtree to list."""
    s = socket.socket(socket.AF_UNIX)
    s.connect(SOCK2)
    s.settimeout(2.0)

    def send(obj):
        b = json.dumps(obj).encode()
        s.sendall(struct.pack(">I", len(b)) + b)

    send({"protocol_version": PROTOCOL_VERSION, "remux_version": "t"})
    s.recv(65536)
    send({"CreateSession": {"name": "rbox", "folder": None}})
    time.sleep(0.5)
    s.close()


class Client:
    def __init__(self, cfgdir, cols=160, rows=40):
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = bytearray()
        self.child = pexpect.spawn(BIN, [], env=env_local(cfgdir),
                                   dimensions=(rows, cols), encoding=None)
        self.pump(2.0)

    def pump(self, t=0.5):
        end = time.time() + t
        while time.time() < end:
            try:
                data = self.child.read_nonblocking(65536, 0.1)
                if data:
                    self.raw += data
                    self.stream.feed(data)
            except Exception:
                pass

    def send(self, data, t=0.4):
        if isinstance(data, str):
            data = data.encode()
        self.child.send(data)
        self.pump(t)

    def prefix(self, keys, t=0.4):
        self.child.send(b"\x01")
        time.sleep(0.15)
        self.send(keys, t)

    def rows_text(self):
        return self.screen.display

    def alive(self):
        return self.child.isalive()

    def dump(self, label=""):
        print(f"----- {label} -----")
        for i, r in enumerate(self.rows_text()):
            if r.rstrip():
                print(f"{i:2} |{r.rstrip()}")

    def kill(self):
        try:
            self.child.terminate(force=True)
        except Exception:
            pass


def client_log():
    p = f"{RUN}/state/remux/client.log"
    return open(p, "rb").read() if os.path.exists(p) else b""


def server_row(c, name):
    for r in sm_tree(c):
        if r.depth == 0 and name in r.text:
            return r
    raise AssertionError(f"no `{name}` server row: {sm_tree(c)}")


def wait_row(c, name, pred, timeout):
    deadline = time.time() + timeout
    row = server_row(c, name)
    while time.time() < deadline and not pred(row.text):
        c.pump(0.3)
        row = server_row(c, name)
    return row


def check_no_panic():
    for state in (f"{RUN}/state", f"{RUN}/state2"):
        for which in ("server", "client", "relay"):
            p = f"{state}/remux/{which}.log"
            if os.path.exists(p) and "panicked at" in open(p, errors="ignore").read():
                check(f"no panic in {which}.log ({state})", False)
                return
    check("no `panicked at` in any log", True)


def main():
    setup_dirs()
    servers, c = [], None
    try:
        servers.append(start_server(SOCK2, env_remote()))
        seed_remote_session()
        servers.append(start_server(SOCK1, env_local(f"{RUN}/cfg-none")))
        c = Client(f"{RUN}/cfg")

        # -- the good remote, dialled in raw mode ----------------------------
        sm_open(c)
        sm_goto(c, lambda r: r.depth == 0 and "mini" in r.text, "the mini row")
        start = len(c.raw)
        c.send("\r", 0.5)
        row = wait_row(c, "mini", lambda t: "(" not in t, 15.0)
        window = bytes(c.raw[start:])
        if VERBOSE:
            c.dump("after connecting mini")
        connected = "(" not in row.text
        check("2 the remote connected (no state suffix on its row)", connected, row.text)
        check("2 its session is listed",
              any("rbox" in r.text for r in sm_tree(c)), sm_tree(c))
        check("1 the marker never reached the outer terminal", b"LEAK" not in window)
        check("1 the DECRST pair never reached the outer terminal",
              DECRST not in window,
              f"found at offset {window.find(DECRST)} of the {len(window)}-byte window")
        check("1 the filler never reached the outer terminal", b"filler 0" not in window)

        log = client_log()
        leak = [l for l in log.splitlines() if b"LEAK" in l]
        check("3 client.log holds the escaped line",
              any(b"LEAK\\x1b[?1000l\\x1b[?1006l" in l for l in leak), leak[:3])
        check("3 that line holds no raw ESC", leak and not any(b"\x1b" in l for l in leak),
              leak[:3])
        check("3 client.log names the remote on it",
              any(b"mini" in l for l in leak), leak[:3])
        check("3 client.log holds the normal line",
              b"motd: welcome to the fake remote" in log)
        logged = sum(1 for l in log.splitlines() if b"filler " in l)
        check("4 the filler was capped, not logged in full",
              0 < logged < FILLER_LINES, f"{logged} filler lines logged")
        check("4 the cap is announced", b"(later stderr lines are suppressed)" in log)

        # -- the failing remote ---------------------------------------------
        sm_goto(c, lambda r: r.depth == 0 and "bad" in r.text, "the bad row")
        c.send("\r", 0.5)
        row = wait_row(c, "bad", lambda t: "failed" in t, 15.0)
        if VERBOSE:
            c.dump("after connecting bad")
        check("5 the bad remote's row reads failed", "(failed:" in row.text, row.text)
        check("5 the row names the ssh error",
              "Could not resolve hostname" in row.text, row.text)
        failed = [l for l in client_log().splitlines()
                  if b"remote 'bad' failed" in l]
        check("5 the registry's failure line names the ssh error",
              any(b"Could not resolve hostname badhost" in l for l in failed), failed)
        # -- the dial that times out ----------------------------------------
        sm_goto(c, lambda r: r.depth == 0 and "slow" in r.text, "the slow row")
        began = time.time()
        c.send("\r", 0.5)
        row = wait_row(c, "slow", lambda t: "failed" in t, 20.0)
        took = time.time() - began
        if VERBOSE:
            c.dump("after connecting slow")
        check("7 the slow remote's row reads failed", "(failed:" in row.text, row.text)
        check("7 the row names the line ssh wrote before it hung",
              f"ssh stderr: {SLOW}" in row.text, row.text)
        failed = [l for l in client_log().splitlines()
                  if b"remote 'slow' failed" in l]
        check("7 the failure line holds the tail and the timeout",
              any(f"ssh stderr: {SLOW}: connection timed out after 10s".encode() in l
                  for l in failed), failed)
        check("7 a stderr held open did not hold the failure up",
              took < 13.0, f"{took:.1f}s from Enter to failed")
        check("client alive", c.alive())

        # -- attach-remote dials BEFORE raw mode -----------------------------
        # Keyboard-interactive instructions (a 2FA menu) and the sshd banner
        # go to ssh's stderr, not /dev/tty, so this path echoes the escaped
        # lines until the handshake is done, and its error carries the tail.
        a = pexpect.spawn(BIN, ["attach-remote", "badhost", "rbox"],
                          env=env_local(f"{RUN}/cfg-none"), dimensions=(40, 160),
                          encoding=None)
        a.expect(pexpect.EOF, timeout=20)
        out = a.before or b""
        a.close()
        if VERBOSE:
            print(out.decode(errors="replace"))
        check("6 attach-remote fails non-zero", a.exitstatus not in (0, None),
              a.exitstatus)
        check("6 attach-remote's error names the ssh error",
              b"ssh stderr: " + RESOLVE.encode() in out, out[-400:])

        # -- the pre-raw dials echo, escaped ---------------------------------
        a = pexpect.spawn(BIN, ["attach-remote", "whatever", "rbox"],
                          env=env_local(f"{RUN}/cfg-none"), dimensions=(40, 160),
                          encoding=None)
        out = b""
        end = time.time() + 6.0
        while time.time() < end:
            try:
                out += a.read_nonblocking(65536, 0.2)
            except pexpect.TIMEOUT:
                pass
            except pexpect.EOF:
                break
        a.terminate(force=True)
        check("8 attach-remote echoed the normal line",
              b"motd: welcome to the fake remote" in out)
        check("8 attach-remote echoed the marker escaped",
              b"LEAK\\x1b[?1000l\\x1b[?1006l" in out)
        check("8 attach-remote never wrote the DECRST pair raw", DECRST not in out)

        auto = Client(f"{RUN}/cfg-auto")
        auto.pump(3.0)
        out = bytes(auto.raw)
        auto.kill()
        check("8 auto_connect echoed the marker escaped",
              b"LEAK\\x1b[?1000l\\x1b[?1006l" in out)
        check("8 auto_connect never wrote the DECRST pair raw", DECRST not in out)
    except Exception:
        traceback.print_exc()
        fails.append("exception")
    finally:
        if c:
            c.kill()
        check_no_panic()
        for p in servers:
            p.kill()
    print(f"\n{'FAIL' if fails else 'OK'}: {len(fails)} failing")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
