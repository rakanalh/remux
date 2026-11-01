"""A stand-in remux server that answers the handshake and then misbehaves.

The CLI's soft-failure contract is about servers that do NOT behave: one that
never replies, or one that speaks another protocol. A real server can be made
to do neither on demand, so this one binds the socket itself, answers the
`Hello` with a `Welcome` carrying whatever `protocol_version` it was given,
then either writes `reply` once or holds the connection open in silence until
it is closed.

    srv = FakeServer(f"{rundir}/run/remux.sock")          # never replies
    srv = FakeServer(path, protocol_version=11)           # protocol skew
    srv.start(); ...; srv.close()
"""
import json
import os
import socket
import struct
import threading

from pty_harness import PROTOCOL_VERSION


def _frame(obj):
    body = json.dumps(obj).encode()
    return struct.pack(">I", len(body)) + body


def _read_frame(conn):
    buf = b""
    while len(buf) < 4:
        chunk = conn.recv(4 - len(buf))
        if not chunk:
            return None
        buf += chunk
    (n,) = struct.unpack(">I", buf)
    body = b""
    while len(body) < n:
        chunk = conn.recv(n - len(body))
        if not chunk:
            return None
        body += chunk
    return json.loads(body)


class FakeServer:
    def __init__(self, sock_path, protocol_version=PROTOCOL_VERSION, reply=None):
        self.sock_path = sock_path
        self.protocol_version = protocol_version
        self.reply = reply
        self.hellos = []
        self._stop = threading.Event()
        self._conns = []
        self._listener = None
        self._thread = None

    def start(self):
        os.makedirs(os.path.dirname(self.sock_path), exist_ok=True)
        try:
            os.unlink(self.sock_path)
        except FileNotFoundError:
            pass
        self._listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._listener.bind(self.sock_path)
        self._listener.listen(8)
        self._listener.settimeout(0.2)
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()
        return self

    def _serve(self):
        while not self._stop.is_set():
            try:
                conn, _ = self._listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            self._conns.append(conn)
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    def _handle(self, conn):
        try:
            hello = _read_frame(conn)
            if hello is None:
                return
            self.hellos.append(hello)
            conn.sendall(_frame({"protocol_version": self.protocol_version,
                                 "remux_version": "fake"}))
            if self.reply is not None:
                conn.sendall(_frame(self.reply))
            # Hold the connection open, reading and discarding, until the peer
            # goes or the fake is closed.
            conn.settimeout(0.2)
            while not self._stop.is_set():
                try:
                    if not conn.recv(65536):
                        return
                except socket.timeout:
                    continue
        except OSError:
            return

    def close(self):
        self._stop.set()
        for c in self._conns:
            try:
                c.close()
            except OSError:
                pass
        if self._listener is not None:
            self._listener.close()
        try:
            os.unlink(self.sock_path)
        except FileNotFoundError:
            pass
