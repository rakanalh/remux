"""Part A (frame-level, via the REAL relay): does an UNFOCUSED remote View cell
blank on a focus change?

Reproduces the remote path end-to-end without SSH: a second isolated server owns
the pane, and we speak the wire protocol through `remux relay` -- the exact
transparent byte pump the client spawns as `ssh <dest> remux relay`. If the
immediate-PaneContent-on-SubscribePane were emitted only on the direct/local
socket path (and not carried through the relay on a remote re-subscribe), an
unfocused remote cell would have nothing to repaint from and go blank.

We simulate the client's focus-change behavior: on every focus change the client
re-subscribes ALL cells (flipping size_demand). We re-subscribe the pane as an
UNFOCUSED cell (size_demand=false) repeatedly and assert a fresh PaneContent --
carrying the pane's real content -- arrives through the relay every time.
"""
import json, os, struct, subprocess, sys, time
from harness import Server, BIN, name_of, only

RUNDIR = "/tmp/rmxais/parta"
MARKER = "REMOTE_MARKER_4417"


class Relay:
    """Drive `remux relay` over its stdio like a socket (length-prefixed JSON)."""

    def __init__(self, env):
        self.p = subprocess.Popen(
            [BIN, "relay"], env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
        self.buf = b""

    def send(self, obj):
        b = json.dumps(obj).encode()
        self.p.stdin.write(struct.pack(">I", len(b)) + b)
        self.p.stdin.flush()

    def _fill(self, n):
        while len(self.buf) < n:
            chunk = self.p.stdout.read1(65536) if hasattr(self.p.stdout, "read1") else self.p.stdout.read(65536)
            if not chunk:
                raise EOFError("relay stdout closed")
            self.buf += chunk

    def recv(self):
        self._fill(4)
        n = struct.unpack(">I", self.buf[:4])[0]
        self._fill(4 + n)
        body, self.buf = self.buf[4:4 + n], self.buf[4 + n:]
        return json.loads(body)

    def drain(self, t=0.8):
        out = []
        os.set_blocking(self.p.stdout.fileno(), False)
        end = time.time() + t
        while time.time() < end:
            try:
                self._fill(4)
                n = struct.unpack(">I", self.buf[:4])[0]
                self._fill(4 + n)
                body, self.buf = self.buf[4:4 + n], self.buf[4 + n:]
                out.append(json.loads(body))
            except (BlockingIOError, EOFError):
                time.sleep(0.05)
            except Exception:
                break
        os.set_blocking(self.p.stdout.fileno(), True)
        return out

    def kill(self):
        self.p.kill()
        self.p.wait()


def pane_text(pc):
    return "".join(c.get("c", "") for row in pc["cells"] for c in row)


def first_pane_id(c):
    c.send("ListSessionTree")
    for _ in range(60):
        m = c.recv()
        if name_of(m) == "SessionTree":
            st = m["SessionTree"]
            for grp in list(st.get("folders", [])) + list(st.get("unfiled", [])):
                sessions = [grp] if "tabs" in grp else grp.get("sessions", [])
                for sess in sessions:
                    for tab in sess["tabs"]:
                        for p in tab["panes"]:
                            return p["id"]
    return None


def main():
    from harness import Client
    srv = Server(RUNDIR).start()
    fails = []

    # -- Set up the pane on the "remote" server, then DETACH so it is NOT
    #    session-visible (an unfocused watch-only remote cell must show content). --
    setup = Client(srv.sock)
    setup.hello()
    setup.send({"CreateSession": {"name": "remote", "folder": None}})
    setup.send({"Attach": {"session_name": "remote"}})
    setup.send({"Resize": {"cols": 100, "rows": 30}})
    time.sleep(0.3)
    setup.send({"Input": {"data": list(f"printf '{MARKER}\\n'\n".encode())}})
    time.sleep(0.5)
    setup.drain(0.4)
    P = first_pane_id(setup)
    print("remote pane id:", P)
    if P is None:
        print("FAIL: no pane id")
        srv.kill(); sys.exit(1)
    setup.send({"Detach": {}} if False else "Detach")  # Detach is a unit variant
    time.sleep(0.3)

    # -- Now reach that pane THROUGH the relay, as a remote client would. --
    relay = Relay(srv.env)
    welcome = None
    relay.send({"protocol_version": 6, "remux_version": "t"})
    welcome = relay.recv()
    print("relay handshake:", name_of(welcome))

    def subscribe_unfocused(step):
        # Exactly what the client sends for an UNFOCUSED remote cell on a focus
        # change: re-subscribe, no size demand.
        relay.send({"SubscribePane": {"pane_id": P, "cols": 40, "rows": 20, "size_demand": False}})
        msgs = relay.drain(0.8)
        pcs = [only(m, "PaneContent") for m in msgs if name_of(m) == "PaneContent" and only(m, "PaneContent")["pane_id"] == P]
        has_content = any(MARKER in pane_text(pc) for pc in pcs)
        print(f"[{step}] PaneContent frames={len(pcs)} content_present={has_content}")
        if not pcs:
            fails.append(f"{step}: no PaneContent through the relay (remote cell would blank)")
        elif not has_content:
            fails.append(f"{step}: PaneContent arrived but WITHOUT the pane's content (blank)")

    # First subscribe (cell opens), then simulate several focus changes that each
    # re-subscribe the still-UNFOCUSED remote cell.
    subscribe_unfocused("open (unfocused)")
    subscribe_unfocused("focus change 1")
    subscribe_unfocused("focus change 2")
    subscribe_unfocused("focus change 3")

    if "panic" in srv.log().lower():
        fails.append("server panic in log")
    relay.kill()
    srv.kill()

    if fails:
        print("FAIL:")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("PASS: unfocused REMOTE cell receives a fresh content-bearing PaneContent "
          "through the relay on every (re)subscribe -- it does NOT blank. "
          "=> bug2 is not reproducible; the remote path matches the local path.")


if __name__ == "__main__":
    main()
