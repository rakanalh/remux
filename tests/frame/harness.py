"""Shared frame-level protocol harness helpers for Remux headless tests.

Speaks the length-prefixed JSON wire protocol directly against a throwaway
server with isolated XDG dirs and a short socket path.
"""
import json, os, shutil, socket, struct, subprocess, time

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))


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
        shutil.rmtree(self.rundir, ignore_errors=True)
        for s in ("run", "state", "data", "config"):
            os.makedirs(f"{self.rundir}/{s}", exist_ok=True)
        if config is not None:
            os.makedirs(f"{self.rundir}/config/remux", exist_ok=True)
            with open(f"{self.rundir}/config/remux/config.toml", "w") as f:
                f.write(config)
        self.env = {
            **os.environ,
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
