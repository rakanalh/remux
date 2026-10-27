#!/usr/bin/env python3
"""The fake `ssh` for the remote-messaging harness (`msg_remote.py`).

`SHIM` is written to `$SHIMDIR/ssh`, which is on server A's PATH, so A's
daemon runs it where it would run ssh. It records how it was called, then
plays the mode the harness chose in `$RMX_SHIM_MODE`:

  relay    exec B's relay under B's four XDG variables, as sshd's login
           environment on B would give it, so relay.log is B's
  prompt   ssh asking for a passphrase under BatchMode: one stderr line, 255
  hang     a dial that never answers (exec, so the recorded pid IS the sleep)
  skew     a Welcome with protocol_version 16, then exit (this file, run as a
           program on stdin/stdout)
  skewnasty  the same, with a newline, a forged log line and ESC sequences
           in its build stamp
  oldpeer  a correct handshake, then exit on the first frame after it
  eatsend  a correct handshake and peer link, then exit on the first send,
           unanswered: the send was written and its outcome is unknown
  mute     the same, but stay open and never answer the send
  echo     a far server that answers every forwarded send with two answers
           naming no send in flight (req 999, then no req at all), never
           with its own
  proxy    B's real relay, behind a proxy the harness steers with files:
           while $RMX_SHIM_SWALLOW exists, nothing from A reaches B and the
           proxy answers A's pings itself (B hears nothing, A stays Up);
           when $RMX_SHIM_HOLD exists, the next ping from A is held 1.5 s.
           The proxy exits when B's relay does, as ssh would
  garbage  ssh failing with 10 KiB of ESC-laden stderr, then 255
  garbage-forever  the same with 1 MiB, more than any pipe holds: an ssh
           whose stderr nobody drains blocks for ever
  stalepong  a far server that answers the dial's barrier ping and every
           forwarded send, but every later ping with the wrong nonce
  hostile  a far server that pushes messages back: one MsgInbound before
           its Pong (req 100), then, once the dialler forwards a send (which
           it answers), each frame in $RMX_SHIM_PUSH with "$PANE" and
           "$MINTED" replaced by that send's origin pane and id; every
           MsgInboundAck it gets is appended to $RMX_SHIM_ACKS, one per line

Before the relay, the shim lists its own open descriptors to `$RMX_SHIM_FDS`:
the relay inherits exactly these, and decision 71 says they are 0, 1 and 2.

Run as a program (`python3 ssh_shim.py skew|skewnasty|oldpeer|eatsend|mute|hostile|echo|stalepong|garbage|garbage-forever|proxy`) it
is the far side of the link for the modes that need no real server.
"""
import json
import os
import struct
import sys

# A build stamp that forges a log line, overwrites one with CR and colours
# the terminal that shows the log, if any of it reaches a log raw.
NASTY_STAMP = "shim-nasty\nINFO forgedline here\rCR\x1b[31mRED\x1b]0;title\x07" + "z" * 300

SHIM = r"""#!/bin/sh
# The fake ssh: records how it was called, then plays the mode the harness chose.
echo "$$" >> "$RMX_SHIM_PIDS"; printf '%s\n' "$*" >> "$RMX_SHIM_ARGV"
mode=$(cat "$RMX_SHIM_MODE" 2>/dev/null || echo relay)
case "$mode" in
  relay)   ls -l "/proc/$$/fd" > "$RMX_SHIM_FDS" 2>/dev/null || lsof -p "$$" > "$RMX_SHIM_FDS" 2>/dev/null
           export XDG_RUNTIME_DIR="$RMX_B_RUN" XDG_STATE_HOME="$RMX_B_STATE" XDG_DATA_HOME="$RMX_B_DATA" XDG_CONFIG_HOME="$RMX_B_CONFIG"
           exec "$RMX_BIN" relay ;;
  prompt)  echo "Enter passphrase for key '/home/x/.ssh/id_ed25519':" >&2; exit 255 ;;
  hang)    exec sleep 60 ;;
  skew)    exec "$RMX_PY" "$RMX_SHIM_PY" skew ;;
  skewnasty) exec "$RMX_PY" "$RMX_SHIM_PY" skewnasty ;;
  oldpeer) exec "$RMX_PY" "$RMX_SHIM_PY" oldpeer ;;
  eatsend) exec "$RMX_PY" "$RMX_SHIM_PY" eatsend ;;
  mute)    exec "$RMX_PY" "$RMX_SHIM_PY" mute ;;
  hostile) exec "$RMX_PY" "$RMX_SHIM_PY" hostile ;;
  echo)    exec "$RMX_PY" "$RMX_SHIM_PY" echo ;;
  stalepong) exec "$RMX_PY" "$RMX_SHIM_PY" stalepong ;;
  proxy)   exec "$RMX_PY" "$RMX_SHIM_PY" proxy ;;
  garbage) exec "$RMX_PY" "$RMX_SHIM_PY" garbage ;;
  garbage-forever) exec "$RMX_PY" "$RMX_SHIM_PY" garbage-forever ;;
esac
"""


def _read_frame(stream):
    head = stream.read(4)
    if len(head) < 4:
        return None
    (n,) = struct.unpack(">I", head)
    return json.loads(stream.read(n))


def _write_frame(stream, obj):
    body = json.dumps(obj).encode()
    stream.write(struct.pack(">I", len(body)) + body)
    stream.flush()


def main(mode):
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from harness import PROTOCOL_VERSION  # noqa: E402

    stdin, stdout = sys.stdin.buffer, sys.stdout.buffer
    if mode == "proxy":
        return _proxy(stdin, stdout)
    if mode in ("garbage", "garbage-forever"):
        size = 10 * 1024 if mode == "garbage" else 1024 * 1024
        line = b"Permission \x1b[31mdenied\x1b[0m (publickey)" + b"\x1b" * 400 + b"\n"
        rest = (b"\x1b[2J\x07garbage " * (size // 16 + 1))[:size]
        sys.stderr.buffer.write(line + rest)
        sys.stderr.buffer.flush()
        return 255
    if _read_frame(stdin) is None:
        return 0
    version = 16 if mode in ("skew", "skewnasty") else PROTOCOL_VERSION
    stamp = NASTY_STAMP if mode == "skewnasty" else f"shim-{mode}"
    _write_frame(stdout, {"protocol_version": version, "remux_version": stamp})
    if mode == "oldpeer":
        _read_frame(stdin)
    if mode == "hostile":
        return _hostile(stdin, stdout)
    if mode == "stalepong":
        while True:
            frame = _read_frame(stdin)
            if frame is None:
                return 0
            if "Ping" in frame:
                nonce = frame["Ping"]["nonce"]
                _write_frame(stdout, {"Pong": {"nonce": nonce if nonce == 1 else 0}})
            elif "MsgSend" in frame:
                _write_frame(stdout, {"MsgSent": {
                    "id": 1, "to": {"host": None, "pane_id": 1, "name": None},
                    "delivery": "Mailbox", "reason": None, "timed_out": False,
                    "final_state": None, "req": frame["MsgSend"].get("req")}})
    if mode == "echo":
        while True:
            frame = _read_frame(stdin)
            if frame is None:
                return 0
            if "Ping" in frame:
                _write_frame(stdout, {"Pong": {"nonce": frame["Ping"]["nonce"]}})
            elif "MsgSend" in frame:
                for req in (999, None):
                    _write_frame(stdout, {"MsgSent": {
                        "id": 1, "to": {"host": None, "pane_id": 1, "name": None},
                        "delivery": "Mailbox", "reason": None, "timed_out": False,
                        "final_state": None, "req": req}})
    if mode in ("eatsend", "mute"):
        # The far server answers a PeerLink only to refuse it, and the ping
        # after it with a Pong, which is what tells the dialler the link is up.
        while True:
            frame = _read_frame(stdin)
            if frame is None:
                return 0
            if "Ping" in frame:
                _write_frame(stdout, {"Pong": {"nonce": frame["Ping"]["nonce"]}})
            elif "MsgSend" in frame and mode == "eatsend":
                return 0
    return 0


def _filled(frame, pane, minted):
    """`frame` with "$PANE" and "$MINTED" replaced, wherever they are."""
    text = json.dumps(frame).replace('"$PANE"', str(pane)).replace('"$MINTED"', str(minted))
    return json.loads(text)


def _hostile(stdin, stdout):
    early = {"MsgInbound": {"to": {"Pane": 1}, "body": "early", "deliver": "Mailbox",
                            "reply_to": None, "req": 100,
                            "origin": {"host": "x", "pane_id": 1, "name": None, "session": "s", "id": 1}}}
    pushed = False
    while True:
        frame = _read_frame(stdin)
        if frame is None:
            return 0
        if "Ping" in frame:
            _write_frame(stdout, early)
            _write_frame(stdout, {"Pong": {"nonce": frame["Ping"]["nonce"]}})
        elif "MsgSend" in frame and not pushed:
            send = frame["MsgSend"]
            _write_frame(stdout, {"MsgSent": {
                "id": 1, "to": {"host": None, "pane_id": 1, "name": None},
                "delivery": "Mailbox", "reason": None, "timed_out": False,
                "final_state": None, "req": send.get("req")}})
            origin = send["origin"]
            with open(os.environ["RMX_SHIM_PUSH"]) as f:
                frames = json.load(f)
            for push in frames:
                _write_frame(stdout, _filled(push, origin["pane_id"], origin["id"]))
            pushed = True
        elif "MsgInboundAck" in frame:
            with open(os.environ["RMX_SHIM_ACKS"], "a") as f:
                f.write(json.dumps(frame["MsgInboundAck"]) + "\n")


def _proxy(stdin, stdout):
    import subprocess
    import threading
    import time

    env = dict(os.environ, XDG_RUNTIME_DIR=os.environ["RMX_B_RUN"], XDG_STATE_HOME=os.environ["RMX_B_STATE"],
               XDG_DATA_HOME=os.environ["RMX_B_DATA"], XDG_CONFIG_HOME=os.environ["RMX_B_CONFIG"])
    relay = subprocess.Popen([os.environ["RMX_BIN"], "relay"], env=env,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    lock = threading.Lock()

    def to_a(frame):
        with lock:
            _write_frame(stdout, frame)

    def from_b():
        while True:
            frame = _read_frame(relay.stdout)
            if frame is None:
                os._exit(0)
            to_a(frame)

    threading.Thread(target=from_b, daemon=True).start()
    swallow, hold = os.environ["RMX_SHIM_SWALLOW"], os.environ["RMX_SHIM_HOLD"]
    while True:
        frame = _read_frame(stdin)
        if frame is None:
            relay.stdin.close()
            relay.wait()
            return 0
        if os.path.exists(swallow):
            if "Ping" in frame:
                to_a({"Pong": {"nonce": frame["Ping"]["nonce"]}})
            continue
        if "Ping" in frame and os.path.exists(hold):
            os.remove(hold)
            time.sleep(1.5)
        try:
            _write_frame(relay.stdin, frame)
        except BrokenPipeError:
            os._exit(0)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
