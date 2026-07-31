"""Reusable real-PTY harness for the Remux client TUI.

Drives the actual client binary through a pseudo-terminal (pexpect) and reads
the rendered screen with pyte. Spins up an isolated throwaway server (the client
auto-spawns it) with its own XDG dirs and a short socket path.
"""
import os, shutil, time, pexpect, pyte

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
PREFIX = b"\x01"  # Ctrl-a


class Tui:
    def __init__(self, rundir, cols=120, rows=40, config=None):
        self.rundir = rundir
        self.cols = cols
        self.rows = rows
        self.child = None
        # Optional `config.toml` body. Written into the isolated
        # XDG_CONFIG_HOME, which the client AND the server it auto-spawns both
        # read -- so one file themes both sides of a parity comparison.
        self.config = config

    def start(self):
        shutil.rmtree(self.rundir, ignore_errors=True)
        for s in ("run", "state", "data", "config"):
            os.makedirs(f"{self.rundir}/{s}", exist_ok=True)
        if self.config is not None:
            os.makedirs(f"{self.rundir}/config/remux", exist_ok=True)
            with open(f"{self.rundir}/config/remux/config.toml", "w") as f:
                f.write(self.config)
        env = {
            **os.environ,
            "XDG_RUNTIME_DIR": f"{self.rundir}/run",
            "XDG_STATE_HOME": f"{self.rundir}/state",
            "XDG_DATA_HOME": f"{self.rundir}/data",
            "XDG_CONFIG_HOME": f"{self.rundir}/config",
            "SHELL": "/bin/sh",
            "ENV": "/dev/null",
            "TERM": "xterm-256color",
            "PS1": "$ ",
            "REMUX_ALLOW_NESTED": "1",
        }
        self.screen = pyte.Screen(self.cols, self.rows)
        self.stream = pyte.ByteStream(self.screen)
        # Raw copy of everything the client wrote. pyte drops the sequences it
        # does not model, and OSC 52 (the clipboard write a yank performs) is one
        # of them -- so a test that wants to assert on the YANKED text has to
        # read it out of the byte stream itself. See `yanks()`.
        self.raw = bytearray()
        self.child = pexpect.spawn(
            BIN, [], env=env, dimensions=(self.rows, self.cols), encoding=None,
        )
        self.pump(1.2)
        return self

    def pump(self, t=0.5):
        end = time.time() + t
        while time.time() < end:
            try:
                data = self.child.read_nonblocking(65536, 0.1)
                if data:
                    self.raw.extend(data)
                    self.stream.feed(data)
            except Exception:
                pass

    def rows_text(self):
        return self.screen.display

    def dump(self, label=""):
        print(f"----- screen {label} -----")
        for i, r in enumerate(self.rows_text()):
            print(f"{i:2} |{r.rstrip()}")
        print("-------------------------")

    def send(self, data, t=0.4):
        if isinstance(data, str):
            data = data.encode()
        self.child.send(data)
        self.pump(t)

    def prefix(self, keys, t=0.4):
        """Send Ctrl-a then the given key bytes."""
        self.child.send(PREFIX)
        time.sleep(0.15)
        self.send(keys, t)

    def alive(self):
        return self.child.isalive()

    def kill(self):
        try:
            self.child.terminate(force=True)
        except Exception:
            pass

    def log(self, which="client"):
        p = f"{self.rundir}/state/remux/{which}.log"
        return open(p).read() if os.path.exists(p) else ""

    def find_row(self, needle):
        for i, r in enumerate(self.rows_text()):
            if needle in r:
                return i
        return -1

    def has(self, needle):
        return any(needle in r for r in self.rows_text())

    def yanks(self):
        """Every clipboard write the client made, decoded, oldest first.

        `copy_to_clipboard` emits OSC 52 (`ESC ] 52 ; c ; <base64> BEL`), so this
        is what a real terminal would have put on the clipboard.
        """
        import base64, re
        out = []
        for m in re.finditer(rb"\x1b\]52;[^;]*;([A-Za-z0-9+/=]*)(?:\x07|\x1b\\)", bytes(self.raw)):
            try:
                out.append(base64.b64decode(m.group(1)).decode("utf-8", "replace"))
            except Exception:
                pass
        return out
