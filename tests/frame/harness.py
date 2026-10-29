"""Shared frame-level protocol harness helpers for Remux headless tests.

Speaks the length-prefixed JSON wire protocol directly against a throwaway
server with isolated XDG dirs and a short socket path.
"""
import atexit, json, os, shutil, signal, socket, struct, subprocess, sys, time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import home_guard  # noqa: E402,F401  (installs the real-HOME refusal)

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))


# The runtime dir this harness started under, read before a harness can point
# XDG_RUNTIME_DIR at its own: the user's real server listens there.
_REAL_RUNTIME = os.environ.get("XDG_RUNTIME_DIR", "")


def _protected():
    """Where the user's real servers listen, which no rundir may contain."""
    uid = os.getuid()
    dirs = (_REAL_RUNTIME, os.environ.get("XDG_RUNTIME_DIR", ""), f"/run/user/{uid}",
            f"{os.environ.get('TMPDIR') or '/tmp'}/remux-{uid}")
    return [os.path.normpath(d) for d in dirs if d]


def _checked_root(rundir):
    """`rundir` normalised, or AssertionError when reaping under it could reach
    a server that is not this harness's: an empty, relative or top-level dir,
    or one that contains a dir where the user's real server listens. Raised
    explicitly rather than by `assert`, which `python -O` strips."""
    root = os.path.normpath(rundir) if rundir else ""
    if not os.path.isabs(root) or root.count("/") < 2:
        raise AssertionError(f"refusing to reap under {rundir!r}: not a dedicated absolute run dir")
    for live in _protected():
        if _under(live, root):
            raise AssertionError(f"refusing to reap under {rundir!r}: it contains {live}, "
                                 "where a real server listens")
    return root


def _under(path, root):
    return path == root or path.startswith(root + "/")


def _matches(pid, root, kinds):
    """Whether `pid` is a `remux` of one of `kinds` whose XDG_RUNTIME_DIR (or,
    with none set, TMPDIR) is under `root`. Both read normalised, so a `..` in
    the process's environment cannot escape `root` lexically."""
    try:
        argv = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
        environ = open(f"/proc/{pid}/environ", "rb").read().split(b"\0")
    except OSError:
        return False
    if len(argv) < 2 or not argv[0].endswith(b"remux") or argv[1] not in kinds:
        return False
    env = dict(e.split(b"=", 1) for e in environ if b"=" in e)
    runtime = env.get(b"XDG_RUNTIME_DIR", b"").decode(errors="replace")
    tmpdir = env.get(b"TMPDIR", b"").decode(errors="replace")
    if runtime:
        return _under(os.path.normpath(runtime), root)
    return bool(tmpdir) and _under(os.path.normpath(tmpdir), root)


def remux_daemons(rundir, kinds=(b"server", b"relay")):
    """Pids of every `remux server` / `remux relay` whose XDG_RUNTIME_DIR (or,
    with none set, TMPDIR) is under `rundir`.

    Found by environment, not by Popen handle: a server a CLIENT auto-spawns
    through `ensure_server_running` daemonizes (setsid) and is no child of the
    harness, so killing the harness's own processes never reaches it.

    Without /proc (macOS) nothing can be found this way, and the list is
    empty: the harness then reaps only what it holds handles to, as before.
    """
    root = _checked_root(rundir)
    if not os.path.isdir("/proc"):
        return []
    return [int(pid) for pid in os.listdir("/proc")
            if pid.isdigit() and _matches(pid, root, kinds)]


def _kill(pid, root, kinds):
    """SIGKILL `pid` if it is still the process the scan found. Through a pidfd
    where there is one: the match is checked again AFTER the pidfd pins the
    process, so a pid reused between the scan and the kill is never signalled.
    Without pidfds the check and the kill are separate, as they were."""
    try:
        fd = os.pidfd_open(pid)
    except ProcessLookupError:
        return
    except (AttributeError, OSError):
        if _matches(pid, root, kinds):
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
        return
    try:
        if _matches(pid, root, kinds):
            signal.pidfd_send_signal(fd, signal.SIGKILL)
    except OSError:
        pass
    finally:
        os.close(fd)


def reap_rundir(rundir, kinds=(b"server", b"relay")):
    """SIGKILL every remux server/relay living under `rundir`, and wait for them."""
    root = _checked_root(rundir)
    pids = remux_daemons(root, kinds)
    for pid in pids:
        _kill(pid, root, kinds)
    end = time.time() + 3.0
    while time.time() < end and any(os.path.exists(f"/proc/{p}") and
                                    _alive(p) for p in pids):
        time.sleep(0.05)
    return pids


def kill_relays(rundir):
    """SIGKILL the `remux relay` processes under `rundir` -- a dropped SSH, as
    the client sees it. Only this harness's: a `pkill -f 'remux relay'` also
    takes down every other harness's remote running at the same time."""
    root = _checked_root(rundir)
    for pid in remux_daemons(root, kinds=(b"relay",)):
        _kill(pid, root, (b"relay",))


def _alive(pid):
    try:
        state = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()[0]
    except OSError:
        return False
    return state != "Z"


_GUARDED = []

# Every server process a `Server` started, by handle. The /proc scan above
# finds nothing on macOS, so these are what teardown can always stop there:
# a crash-and-restart leaves the earlier handle behind, and a run that dies
# mid-case leaves the current one. A handle is our own unreaped child, so its
# pid cannot have been reused.
_STARTED = []


def _reap_started():
    for proc in _STARTED:
        if proc.poll() is None:
            proc.kill()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass


def _reap_all(*_):
    _reap_started()
    for root in _GUARDED:
        reap_rundir(root)


def _on_signal(signum, frame):
    _reap_all()
    signal.signal(signum, signal.SIG_DFL)
    os.kill(os.getpid(), signum)


def guard_rundir(rundir):
    """Reap every remux server/relay under `rundir` now (a crashed earlier run's)
    and again when this harness exits -- normally, on an exception, or on
    SIGTERM/SIGINT/SIGHUP (the suite's `timeout` sends SIGTERM).

    Called from `start()`, it also reaps a concurrent run that shares this
    rundir, deliberately: two runs on one rundir corrupt each other anyway.

    Its signal handler replaces Python's default SIGINT handler, so a Ctrl-C
    skips `finally` blocks and the other atexit handlers, `Tui.kill`'s graceful
    `remux stop` among them: the reaper SIGKILLs what that would have stopped.
    """
    root = _checked_root(rundir)
    reap_rundir(root)
    if root in _GUARDED:
        return
    if not _GUARDED:
        atexit.register(_reap_all)
        for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            if signal.getsignal(sig) in (signal.SIG_DFL, signal.default_int_handler):
                signal.signal(sig, _on_signal)
    _GUARDED.append(root)


def _protocol_version():
    """Read `PROTOCOL_VERSION` out of the source rather than restating it here.

    A hard-coded copy went stale silently: the server is deliberately LENIENT
    about skew (it logs and proceeds, see CLAUDE.md), so a harness announcing an
    old version kept passing while claiming to speak a protocol that no longer
    existed. Reading it is what makes the number true.
    """
    src = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(
        os.path.abspath(__file__)))), "src", "protocol.rs")
    for line in open(src):
        if "pub const PROTOCOL_VERSION" in line:
            return int(line.split("=")[1].strip().rstrip(";"))
    raise SystemExit("could not read PROTOCOL_VERSION from src/protocol.rs")


PROTOCOL_VERSION = _protocol_version()


class Server:
    def __init__(self, rundir):
        self.rundir = rundir
        self.sock = f"{rundir}/run/remux.sock"
        self.proc = None

    def start(self, config=None):
        """Start the throwaway server. `config`, when given, is written to the
        isolated `$XDG_CONFIG_HOME/remux/config.toml` before launch."""
        guard_rundir(self.rundir)
        shutil.rmtree(self.rundir, ignore_errors=True)
        for s in ("run", "state", "data", "config", "home"):
            os.makedirs(f"{self.rundir}/{s}", exist_ok=True)
        if config is not None:
            os.makedirs(f"{self.rundir}/config/remux", exist_ok=True)
            with open(f"{self.rundir}/config/remux/config.toml", "w") as f:
                f.write(config)
        self.env = {
            **os.environ,
            # A test may have pointed HOME somewhere deliberate (see
            # browser_listing.py); only the real home is replaced.
            "HOME": home_guard.throwaway_home(f"{self.rundir}/home"),
            "XDG_RUNTIME_DIR": f"{self.rundir}/run",
            "XDG_STATE_HOME": f"{self.rundir}/state",
            "XDG_DATA_HOME": f"{self.rundir}/data",
            "XDG_CONFIG_HOME": f"{self.rundir}/config",
            # `REMUX_TEST_SHELL` swaps the pane shell, e.g. for `/bin/zsh` on a
            # Mac, where `/bin/sh` is a trampoline that execs another shell.
            "SHELL": os.environ.get("REMUX_TEST_SHELL", "/bin/sh"),
            "ENV": "/dev/null",
            "TERM": "xterm-256color",
        }
        self.proc = subprocess.Popen(
            [BIN, "server"], env=self.env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        _STARTED.append(self.proc)
        for _ in range(200):
            if os.path.exists(self.sock):
                time.sleep(0.3)
                return self
            time.sleep(0.05)
        self.kill()
        raise SystemExit("no socket")

    def kill(self):
        if self.proc:
            self.proc.kill()
            self.proc.wait()

    def stop(self):
        """Stop the server the way `remux restart` does (`remux stop`, a
        SIGTERM and a clean save), and wait for it to exit."""
        subprocess.run([BIN, "stop"], env=self.env, timeout=20,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.proc.wait(timeout=10)
        for _ in range(100):
            if not os.path.exists(self.sock):
                break
            time.sleep(0.05)

    def crash(self):
        """SIGKILL: no shutdown save runs, so what is on disk is whatever was
        written before."""
        self.kill()

    def start_again(self):
        """Start a stopped server on the SAME run dir, keeping its state and
        data. `start()` cannot do this: it wipes the run dir first."""
        self.proc = subprocess.Popen(
            [BIN, "server"], env=self.env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        _STARTED.append(self.proc)
        for _ in range(200):
            if os.path.exists(self.sock):
                time.sleep(0.3)
                return self
            time.sleep(0.05)
        self.kill()
        raise SystemExit("no socket after a restart")

    def restart(self):
        """`remux restart`: the only way to restart under a harness, because
        `start()` wipes the run dir. Every client connection dies with it."""
        self.stop()
        return self.start_again()

    def log(self):
        p = f"{self.rundir}/state/remux/server.log"
        return open(p).read() if os.path.exists(p) else ""


class Client:
    def __init__(self, sock):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(sock)
        self.s.settimeout(1.5)
        self.buf = b""

    def send(self, obj):
        b = json.dumps(obj).encode()
        self.s.sendall(struct.pack(">I", len(b)) + b)

    def _fill(self):
        """Read one chunk; EOF raises rather than spinning forever (the server
        drops the connection on an undecodable message, and a bare `recv` would
        then return b"" in a tight loop instead of surfacing the close)."""
        chunk = self.s.recv(65536)
        if not chunk:
            raise ConnectionError("server closed the connection")
        self.buf += chunk

    def recv(self):
        while len(self.buf) < 4:
            self._fill()
        n = struct.unpack(">I", self.buf[:4])[0]
        while len(self.buf) < 4 + n:
            self._fill()
        body, self.buf = self.buf[4:4 + n], self.buf[4 + n:]
        return json.loads(body)

    def drain(self, t=0.6):
        """Collect all messages arriving within t seconds.

        A closed connection is RAISED, not swallowed. Returning `[]` for it
        would make every "no messages arrived" assertion pass unconditionally
        once the server dropped the peer -- a zero-count check that cannot tell
        "nothing was sent" from "there is nobody to send to" is not a check.
        """
        out = []
        end = time.time() + t
        old = self.s.gettimeout()
        try:
            while time.time() < end:
                self.s.settimeout(0.1)
                try:
                    out.append(self.recv())
                except socket.timeout:
                    pass
        finally:
            self.s.settimeout(old)
        return out

    def hello(self):
        self.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "t"})
        return self.recv()

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


def name_of(msg):
    """ServerMessage variant name (dict key, or the whole string if unit)."""
    if isinstance(msg, str):
        return msg
    if isinstance(msg, dict):
        return next(iter(msg.keys()))
    return None


def only(msg, key):
    return msg[key] if isinstance(msg, dict) and key in msg else None
