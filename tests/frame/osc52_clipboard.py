"""OSC 52: an application in a pane copies -> the client is told to set the
system clipboard.

An app (claude, vim, a pager) copies by emitting `ESC ] 52 ; c ; <base64> BEL`
to its terminal. Remux IS that terminal, so the sequence has to leave the pane's
emulator and reach the client as a `CopyToClipboard` -- the same message a Remux
yank sends, which the client already turns back into OSC 52 for the OUTER
terminal.

Every case emits the sequence the way a real program does: the pane's shell runs
`printf '\\033]52;c;%s\\007' "$(... | base64)"`, so the ESC/BEL are produced by
the program on the PTY's output side rather than typed in.

Covered:
  1. local pane, small payload      -> CopyToClipboard with the exact text
  2. local pane, multi-KB payload   -> arrives intact (a 1 KiB-capped OSC buffer
                                       would deliver a mangled fragment)
  3. garbage base64                 -> no message, no panic
  4. read request (`;c;?`)          -> no message (never leak the clipboard)
  5. oversized payload              -> dropped
  6. background tab                 -> no message, with a foreground positive
                                       control so it cannot pass vacuously
  7. allow_app_clipboard = false    -> no message
  8. THROUGH A REAL RELAY           -> the reported case: the pane lives on a
                                       second server reached via `remux relay`
                                       (what `ssh <dest> remux relay` runs), and
                                       the write still arrives.

Run: python3 tests/frame/osc52_clipboard.py
"""
import json, os, struct, subprocess, sys, time
from harness import Server, Client, BIN, PROTOCOL_VERSION, name_of, only

RUNDIR = "/tmp/rmxosc"
MARKER = "REMUX_CLIP_7788"

# 400 lines, ~10 KB -- a realistic "copy a block of output" payload, and well
# past the 1 KiB the OSC parser buffers by default.
BIG = "".join(f"line {i:04} of copied text\n" for i in range(400))

failures = []


def check(ok, label):
    print(f"  {'PASS' if ok else 'FAIL'}  {label}")
    if not ok:
        failures.append(label)


def copy_cmd(literal):
    """Shell that makes the pane copy `literal` via OSC 52, as a program would.

    `base64` wraps at 76 columns, so the newlines are stripped -- a payload with
    whitespace in it is not valid base64 and would (correctly) be rejected.
    """
    quoted = literal.replace("'", "'\\''")
    return (
        "printf '\\033]52;c;%s\\007' "
        f"\"$(printf '%s' '{quoted}' | base64 | tr -d '\\n')\"\n"
    )


def raw_cmd(payload):
    """Shell that emits `OSC 52 ; c ; <payload> BEL` with the payload verbatim."""
    return f"printf '\\033]52;c;%s\\007' '{payload}'\n"


def run(c, shell_line):
    c.send({"Input": {"data": list(shell_line.encode())}})


def clips(msgs):
    return [only(m, "CopyToClipboard")["data"] for m in msgs if name_of(m) == "CopyToClipboard"]


# ---------------------------------------------------------------------------
# Local server
# ---------------------------------------------------------------------------

def attach(sock, name="main", create=True):
    c = Client(sock)
    c.hello()
    if create:
        c.send({"CreateSession": {"name": name, "folder": None}})
    c.send({"Attach": {"session_name": name}})
    c.send({"Resize": {"cols": 100, "rows": 30}})
    c.drain(1.0)
    return c


def run_local():
    print("\n[local pane, allow_app_clipboard default (on)]")
    srv = Server(f"{RUNDIR}/local").start()
    try:
        c = attach(srv.sock)

        run(c, copy_cmd(MARKER))
        got = clips(c.drain(1.5))
        check(got == [MARKER], f"small write arrives verbatim (got {got!r})")

        # A realistic multi-KB copy, written through a file so the shell does not
        # have to carry 10 KB on one command line.
        big_path = f"{srv.rundir}/big.txt"
        with open(big_path, "w") as f:
            f.write(BIG)
        run(c, f"printf '\\033]52;c;%s\\007' \"$(base64 < {big_path} | tr -d '\\n')\"\n")
        got = clips(c.drain(2.5))
        check(
            got == [BIG],
            f"{len(BIG)}-byte write arrives intact (got {len(got[0]) if got else 0} bytes)",
        )

        run(c, raw_cmd("!!!not-base64!!!"))
        check(clips(c.drain(1.2)) == [], "invalid base64 produces no clipboard message")

        # `?` asks the terminal to hand the clipboard BACK to the program.
        run(c, raw_cmd("?"))
        check(clips(c.drain(1.2)) == [], "OSC 52 read request produces no clipboard message")

        # Over the 256 KiB cap.
        run(c, "printf '\\033]52;c;%s\\007' \"$(head -c 300000 /dev/zero | tr '\\0' 'A')\"\n")
        check(clips(c.drain(3.0)) == [], "oversized payload is dropped")

        run(c, copy_cmd("STILL_ALIVE"))
        check(clips(c.drain(1.5)) == ["STILL_ALIVE"], "server still serving writes afterwards")
        c.close()
        check("panicked" not in srv.log(), "no panic in the server log")
    finally:
        srv.kill()


def run_focus_gate():
    """A pane only reaches the clipboard while the user is looking at it."""
    print("\n[focus gate: background tab vs foreground]")
    srv = Server(f"{RUNDIR}/focus").start()
    try:
        c = attach(srv.sock)

        # Positive control: the SAME delayed copy, left in the foreground.
        run(c, f"(sleep 2; {copy_cmd('FOREGROUND_OK').strip()}) &\n")
        got = clips(c.drain(4.0))
        check(got == ["FOREGROUND_OK"], f"delayed copy arrives while focused (got {got!r})")

        # Now arm the same thing and switch away before it fires.
        run(c, f"(sleep 2; {copy_cmd('BACKGROUND_LEAK').strip()}) &\n")
        c.drain(0.4)
        c.send({"Command": "TabNew"})
        got = clips(c.drain(4.0))
        check(got == [], f"background-tab pane cannot set the clipboard (got {got!r})")

        # ...and it does not arrive late when the tab comes back either: the
        # write was dropped, not queued.
        c.send({"Command": "TabPrev"})
        check(clips(c.drain(1.5)) == [], "the dropped write does not arrive on return")
        c.close()
        check("panicked" not in srv.log(), "no panic in the server log")
    finally:
        srv.kill()


def run_view_cell():
    """The other half of the routing rule: a View cell SHOWING the pane.

    A client displaying a view is detached, so it never matches the
    "attached and focused" branch -- it has to qualify by showing the pane in a
    cell. A watch-only subscription (a cell the view's layout has hidden) must
    not, for the same reason a background tab must not.
    """
    print("\n[View-cell routing]")
    srv = Server(f"{RUNDIR}/view").start()
    try:
        owner = attach(srv.sock)
        owner.send("ListSessionTree")
        pane_id = None
        for m in owner.drain(1.0):
            if name_of(m) == "SessionTree":
                tree = only(m, "SessionTree")
                blob = json.dumps(tree)
                # The session has exactly one pane; find its id in the tree.
                for folder in list(tree.get("folders", [])) + [{"sessions": tree.get("unfiled", [])}]:
                    for sess in folder.get("sessions", []):
                        for tab in sess.get("tabs", []):
                            for pane in tab.get("panes", []):
                                pane_id = pane["id"]
                assert pane_id is not None, blob
        check(pane_id is not None, "found the pane id")

        showing = Client(srv.sock)
        showing.hello()
        showing.send({"SubscribePane": {"pane_id": pane_id, "cols": 40, "rows": 10,
                                        "size_demand": True}})
        hidden = Client(srv.sock)
        hidden.hello()
        hidden.send({"SubscribePane": {"pane_id": pane_id, "cols": 40, "rows": 10,
                                       "size_demand": False}})
        showing.drain(0.8)
        hidden.drain(0.8)

        run(owner, copy_cmd("VIEW_CELL_COPY"))
        got_showing = clips(showing.drain(2.0))
        got_hidden = clips(hidden.drain(1.0))
        check(got_showing == ["VIEW_CELL_COPY"],
              f"a cell SHOWING the pane gets the write (got {got_showing!r})")
        check(got_hidden == [],
              f"a watch-only subscriber does not (got {got_hidden!r})")

        showing.close()
        hidden.close()
        owner.close()
        check("panicked" not in srv.log(), "no panic in the server log")
    finally:
        srv.kill()


def run_gate_off():
    print("\n[allow_app_clipboard = false]")
    srv = Server(f"{RUNDIR}/off").start(config="[general]\nallow_app_clipboard = false\n")
    try:
        c = attach(srv.sock)
        run(c, copy_cmd(MARKER))
        got = clips(c.drain(1.8))
        check(got == [], f"gate off suppresses the clipboard message (got {got!r})")
        # Nothing is left pending: a second write is dropped too.
        run(c, copy_cmd("AGAIN"))
        check(clips(c.drain(1.5)) == [], "gate off keeps suppressing (nothing left pending)")
        c.close()
        check("panicked" not in srv.log(), "no panic in the server log")
    finally:
        srv.kill()


# ---------------------------------------------------------------------------
# The reported case: a REMOTE session, reached through the real relay
# ---------------------------------------------------------------------------

class Relay:
    """Drive `remux relay` over its stdio like a socket (length-prefixed JSON).

    This is the exact transparent byte pump the client spawns as
    `ssh <dest> <remux_path> relay`, so a message that arrives here is a message
    that arrives at a local client attached to a remote session.
    """

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
            chunk = self.p.stdout.read1(65536)
            if not chunk:
                raise EOFError("relay stdout closed")
            self.buf += chunk

    def drain(self, t=1.5):
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


def run_remote():
    print("\n[remote pane, through a real `remux relay`]")
    srv = Server(f"{RUNDIR}/remote").start()
    try:
        r = Relay(srv.env)
        r.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "t"})
        r.drain(0.8)
        r.send({"CreateSession": {"name": "rmt", "folder": None}})
        r.send({"Attach": {"session_name": "rmt"}})
        r.send({"Resize": {"cols": 100, "rows": 30}})
        r.drain(1.2)

        r.send({"Input": {"data": list(copy_cmd(MARKER).encode())}})
        got = clips(r.drain(2.0))
        check(got == [MARKER], f"remote write arrives through the relay (got {got!r})")

        # The multi-KB case matters most here: this is the path the user hit.
        big_path = f"{srv.rundir}/big.txt"
        with open(big_path, "w") as f:
            f.write(BIG)
        r.send({"Input": {"data": list(
            f"printf '\\033]52;c;%s\\007' \"$(base64 < {big_path} | tr -d '\\n')\"\n".encode())}})
        got = clips(r.drain(2.5))
        check(
            got == [BIG],
            f"multi-KB remote write survives the relay (got {len(got[0]) if got else 0} bytes)",
        )
        # Reads stay refused over the relay too.
        r.send({"Input": {"data": list(raw_cmd("?").encode())}})
        check(clips(r.drain(1.5)) == [], "remote read request is still refused")

        r.kill()
        check("panicked" not in srv.log(), "no panic in the server log")
    finally:
        srv.kill()


run_local()
run_focus_gate()
run_view_cell()
run_gate_off()
run_remote()

print()
if failures:
    print(f"FAILED ({len(failures)}): " + "; ".join(failures))
    sys.exit(1)
print("ALL PASS")
