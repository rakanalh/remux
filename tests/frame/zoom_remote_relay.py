"""Zoom toggling on a REMOTE session, measured through a real `remux relay`.

Companion to `zoom_monocle_wedge.py`. That test pins the *state* bug ("zoomed
in and cannot zoom out"); this one answers the question the report first raised
-- whether sustained zoom toggling over the SSH relay wedges anything, floods
the client, or drops the connection.

An isolated server plays the remote machine and the real `remux relay`
subprocess is spawned against it -- exactly what `ssh <dest> remux relay` runs
-- so the wire protocol travels the same pipe pair SSH would carry. Then it
hammers `PaneToggleZoom` and measures:

  * bytes and renders the server pushes per toggle (the amplification),
  * round-trip latency over the RELAY path DURING sustained toggling,
  * round-trip latency on a DIRECT socket to the same server at the same time
    -- the control that separates "the server is wedged" from "the pipe is
    backed up",
  * whether the relay process or the connection dies.

Modes:
    idle    one pane running an idle /bin/sh
    app     two panes, each running a full-screen alt-screen app that repaints
            every cell on SIGWINCH. Two panes on purpose: zoom must actually
            change the pane geometry for the kernel to signal the foreground
            process group, so this is the mode where a real TUI repaints on
            both edges of every toggle.
    view    the zoomed pane is also aliased by a View cell on a second client.
            Note what this can and cannot show: `resize_session_panes` folds a
            cell's size demand in ONLY when no client is attached to the
            session, and `PaneToggleZoom` is a session command that a detached
            client cannot issue -- so "a View cell resizes the pane the zoom is
            resizing" is unreachable by construction. What this mode does check
            is that a subscribed cell riding along through 60 toggles adds no
            resize feedback, no extra renders and no stall.
    stress  three clients toggling zoom concurrently while a pane is closed
            mid-zoom, with round-trips sampled throughout (the deadlock probe)

Usage: python3 tests/frame/zoom_remote_relay.py [mode] [--slow BYTES_PER_SEC]
"""
import json, os, select, shutil, socket, struct, subprocess, sys, threading, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, PROTOCOL_VERSION, Server, Client, name_of  # noqa: E402

RUNDIR = "/tmp/rmxzr"
COLS, ROWS = 200, 60
TOGGLES = 60

# A full-screen app: enters the alt screen and repaints every cell on SIGWINCH,
# which is what zoom does to a real TUI on both edges of a toggle.
APP_SRC = r"""
import fcntl, signal, struct, sys, termios, time
n = [0]
def repaint(*_a):
    n[0] += 1
    d = fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8)
    r, c = struct.unpack("hhhh", d)[:2]
    ch = chr(65 + (n[0] % 26))
    out = ["\033[H"]
    for y in range(r):
        out.append("\033[3%dm" % (y % 8))
        out.append(ch * c)
        if y < r - 1:
            out.append("\r\n")
    sys.stdout.write("".join(out))
    sys.stdout.flush()
signal.signal(signal.SIGWINCH, repaint)
sys.stdout.write("\033[?1049h")
sys.stdout.flush()
repaint()
while True:
    time.sleep(3600)
"""


class RelayLink:
    """A wire-protocol client speaking over a real `remux relay` subprocess.

    `rate` (bytes/sec) throttles the read side to model a bandwidth-limited SSH
    channel; `None` reads as fast as the pipe delivers.
    """

    def __init__(self, env, rate=None):
        self.proc = subprocess.Popen(
            [BIN, "relay"], env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        self.buf = bytearray()
        self.bytes_in = 0
        self.eof = False
        self.rate = rate
        self.lock = threading.Lock()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        fd = self.proc.stdout.fileno()
        tokens, last = 0.0, time.time()
        while True:
            if self.rate is not None:
                now = time.time()
                tokens = min(tokens + (now - last) * self.rate, self.rate * 0.05)
                last = now
                if tokens < 1024:
                    time.sleep(0.005)
                    continue
                want = int(min(tokens, 65536))
            else:
                want = 65536
            if not select.select([fd], [], [], 0.2)[0]:
                continue
            data = os.read(fd, want)
            if not data:
                self.eof = True
                return
            if self.rate is not None:
                tokens -= len(data)
            with self.lock:
                self.buf += data
                self.bytes_in += len(data)

    def send(self, obj):
        b = json.dumps(obj).encode()
        self.proc.stdin.write(struct.pack(">I", len(b)) + b)
        self.proc.stdin.flush()

    def try_msg(self):
        """Decode one buffered message, or None when a whole one isn't there."""
        with self.lock:
            if len(self.buf) < 4:
                return None
            n = struct.unpack(">I", self.buf[:4])[0]
            if len(self.buf) < 4 + n:
                return None
            body = bytes(self.buf[4:4 + n])
            del self.buf[:4 + n]
        return json.loads(body)

    def drain(self, seconds, tally=None):
        end = time.time() + seconds
        while time.time() < end:
            m = self.try_msg()
            if m is None:
                time.sleep(0.002)
                continue
            if tally is not None:
                tally[name_of(m)] = tally.get(name_of(m), 0) + 1

    def await_kind(self, kind, timeout, tally=None):
        """Drain until `kind` arrives; returns elapsed seconds, or None."""
        t0 = time.time()
        while time.time() - t0 < timeout:
            m = self.try_msg()
            if m is None:
                if self.eof:
                    return None
                time.sleep(0.002)
                continue
            k = name_of(m)
            if tally is not None:
                tally[k] = tally.get(k, 0) + 1
            if k == kind:
                return time.time() - t0
        return None

    def alive(self):
        return self.proc.poll() is None and not self.eof

    def kill(self):
        try:
            self.proc.kill()
            self.proc.wait(timeout=2)
        except Exception:
            pass


def fmt(v):
    return "TIMEOUT" if v is None else f"{v * 1000:.1f}ms"


def direct_rtt(sock_path):
    """Round-trip a ListSessionTree straight to the server's own socket."""
    c = Client(sock_path)
    c.hello()
    t0 = time.time()
    c.send("ListSessionTree")
    dt = None
    while time.time() - t0 < 5:
        try:
            m = c.recv()
        except Exception:
            break
        if name_of(m) == "SessionTree":
            dt = time.time() - t0
            break
    c.close()
    return dt


def main_pane_ids(client):
    client.send("ListSessionTree")
    tree = None
    for m in client.drain(0.8):
        if name_of(m) == "SessionTree":
            tree = m["SessionTree"]
    if tree is None:
        return []
    for sess in tree["unfiled"] + [s for f in tree["folders"] for s in f["sessions"]]:
        if sess["name"] == "main":
            return [p["id"] for p in sess["tabs"][0]["panes"]]
    return []


def start_app(link, script):
    link.send({"Input": {"data": list((f"python3 {script}\n").encode())}})
    time.sleep(1.2)


def setup(rate=None):
    srv = Server(RUNDIR).start()
    script = f"{RUNDIR}/app.py"
    with open(script, "w") as f:
        f.write(APP_SRC)
    link = RelayLink(srv.env, rate=rate)
    link.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "t"})
    time.sleep(0.4)
    link.drain(0.3)
    link.send({"CreateSession": {"name": "main", "folder": None}})
    link.send({"Attach": {"session_name": "main"}})
    link.send({"Resize": {"cols": COLS, "rows": ROWS}})
    time.sleep(0.9)
    link.drain(0.6)
    return srv, link, script


def run(mode="app", rate=None):
    srv, link, script = setup(rate)
    viewer = None

    if mode in ("app", "view"):
        start_app(link, script)
        link.send({"Command": "PaneSplitVertical"})
        time.sleep(0.8)
        start_app(link, script)
    if mode == "stress":
        link.send({"Command": "PaneSplitVertical"})
        time.sleep(0.8)
        link.send({"Command": "PaneSplitHorizontal"})
        time.sleep(0.8)
    if mode == "view":
        # Subscribe while detached (so the cells latch on), then re-attach: the
        # toggling client has to be attached for `PaneToggleZoom` to reach the
        # session at all, which is also why the demand-fold branch can never
        # coincide with a zoom (see the module docstring).
        viewer = Client(srv.sock)
        viewer.hello()
        ids = main_pane_ids(viewer)
        link.send("Detach")
        time.sleep(0.4)
        for pid in ids:
            viewer.send({"SubscribePane": {"pane_id": pid, "cols": 80, "rows": 24}})
        time.sleep(0.5)
        link.send({"Attach": {"session_name": "main"}})
        link.send({"Resize": {"cols": COLS, "rows": ROWS}})
        time.sleep(0.6)

    link.drain(0.8)
    with link.lock:
        link.bytes_in = 0
    tally = {}
    baseline = direct_rtt(srv.sock)

    # -- extra clients for the stress mode ----------------------------------
    stop = threading.Event()
    helpers = []
    if mode == "stress":
        def hammer():
            c = Client(srv.sock)
            c.hello()
            c.send({"Attach": {"session_name": "main"}})
            c.send({"Resize": {"cols": COLS, "rows": ROWS}})
            while not stop.is_set():
                try:
                    c.send({"Command": "PaneToggleZoom"})
                    c.drain(0.05)
                except Exception:
                    break
            c.close()
        for _ in range(2):
            t = threading.Thread(target=hammer, daemon=True)
            t.start()
            helpers.append(t)

    # -- sustained toggling --------------------------------------------------
    relay_rtts, direct_rtts = [], []
    t0 = time.time()
    for i in range(TOGGLES):
        link.send({"Command": "PaneToggleZoom"})
        link.drain(0.12, tally)
        if mode == "stress" and i == TOGGLES // 2:
            # Close a pane while the tab is mid-zoom, from a third connection.
            killer = Client(srv.sock)
            killer.hello()
            killer.send({"Attach": {"session_name": "main"}})
            killer.send({"Command": "PaneClose"})
            killer.drain(0.4)
            killer.close()
        if i % 10 == 9:
            d = direct_rtt(srv.sock)
            link.send("ListSessionTree")
            r = link.await_kind("SessionTree", 10, tally)
            direct_rtts.append(d)
            relay_rtts.append(r)
    elapsed = time.time() - t0
    stop.set()
    link.drain(1.5, tally)

    full, diff = tally.get("FullRender", 0), tally.get("RenderDiff", 0)
    alive = link.alive()
    err = b""
    if not alive:
        try:
            err = link.proc.stderr.read() or b""
        except Exception:
            pass

    print(f"--- mode={mode} rate={rate} toggles={TOGGLES} in {elapsed:.1f}s ---")
    print(f"bytes over relay       : {link.bytes_in:,}  "
          f"({link.bytes_in / TOGGLES:,.0f} / toggle)")
    print(f"FullRender / RenderDiff: {full} / {diff}  "
          f"({(full + diff) / TOGGLES:.1f} renders / toggle)")
    print(f"direct-socket RTT      : baseline {fmt(baseline)}  "
          f"during {[fmt(x) for x in direct_rtts]}")
    print(f"relay-path RTT         : during {[fmt(x) for x in relay_rtts]}")
    print(f"relay alive            : {alive}")
    if err:
        print(f"relay stderr           : {err[:400]!r}")
    panics = [l for l in srv.log().splitlines() if "panic" in l.lower()]
    print(f"server log panics      : {len(panics)}")
    for l in panics[:5]:
        print("   ", l)

    if viewer:
        viewer.close()
    link.kill()
    srv.kill()

    worst = max([r for r in relay_rtts if r is not None] or [0])
    worst_direct = max([d for d in direct_rtts if d is not None] or [0])
    print(f"worst RTT during toggling: relay {fmt(worst)}  direct {fmt(worst_direct)}")
    ok = (alive and not panics
          and all(r is not None for r in relay_rtts)
          and all(d is not None for d in direct_rtts)
          and worst < 1.0 and worst_direct < 1.0)
    return ok


if __name__ == "__main__":
    args = list(sys.argv[1:])
    rate = None
    if "--slow" in args:
        i = args.index("--slow")
        rate = int(args[i + 1])
        del args[i:i + 2]
    mode = args[0] if args else "app"
    shutil.rmtree(RUNDIR, ignore_errors=True)
    ok = run(mode, rate)
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
