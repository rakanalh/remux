#!/usr/bin/env python3
"""bug4 regression guard (two-client): session-visibility must STILL fire when a
GENUINE other viewer is attached to a pane's home session, while the bug4 fix
(client detaches on view entry) removes only the false-positive-from-self.

Scenario:
  * conn A attaches "Remux" -> a real, full-size viewer of Remux's active tab.
  * conn B (attached to NOTHING, models a view host that has detached) subscribes
    to Remux's active-tab pane. Because A is really viewing it, the cell must show
    the "Active in session" placeholder => session_visible == True.
  * conn A then Detaches. conn B re-subscribes. Now nobody is viewing the pane
    full-size => session_visible == False => the cell streams content.

This proves the feature is not regressed: visibility tracks a real other viewer,
not the subscriber's own (now-detached) attachment.
"""
import json, os, shutil, socket, struct, subprocess, sys, time

BIN, RUNDIR = "target/debug/remux", "/tmp/rmxbug4tc"
SOCK = f"{RUNDIR}/run/remux.sock"


def start_server():
    shutil.rmtree(RUNDIR, ignore_errors=True)
    for s in ("run", "state", "data", "config"):
        os.makedirs(f"{RUNDIR}/{s}", exist_ok=True)
    env = {**os.environ, "XDG_RUNTIME_DIR": f"{RUNDIR}/run", "XDG_STATE_HOME": f"{RUNDIR}/state",
           "XDG_DATA_HOME": f"{RUNDIR}/data", "XDG_CONFIG_HOME": f"{RUNDIR}/config",
           "SHELL": "/bin/sh", "ENV": "/dev/null", "TERM": "xterm-256color"}
    p = subprocess.Popen([BIN, "server"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        if os.path.exists(SOCK):
            time.sleep(0.3)
            return p
        time.sleep(0.05)
    p.kill()
    raise SystemExit("no socket")


def send(sock, obj):
    b = json.dumps(obj).encode()
    sock.sendall(struct.pack(">I", len(b)) + b)


def recv(sock, buf):
    while len(buf[0]) < 4:
        buf[0] += sock.recv(65536)
    n = struct.unpack(">I", buf[0][:4])[0]
    while len(buf[0]) < 4 + n:
        buf[0] += sock.recv(65536)
    body, buf[0] = buf[0][4:4 + n], buf[0][4 + n:]
    return json.loads(body)


def flush(sock, buf, t=0.2):
    """Discard any queued frames so a later read can't return a stale one."""
    sock.settimeout(t)
    try:
        while True:
            recv(sock, buf)
    except Exception:
        pass
    sock.settimeout(1.0)


def drain_pane_content(sock, buf, timeout=1.5):
    end = time.time() + timeout
    sock.settimeout(0.3)
    while time.time() < end:
        try:
            msg = recv(sock, buf)
        except socket.timeout:
            continue
        except Exception:
            break
        if isinstance(msg, dict) and "PaneContent" in msg:
            return msg["PaneContent"]
    return None


def newconn():
    s = socket.socket(socket.AF_UNIX)
    s.connect(SOCK)
    s.settimeout(1.0)
    buf = [b""]
    send(s, {"protocol_version": 4, "remux_version": "t"})
    recv(s, buf)  # Welcome
    return s, buf


def main():
    p = start_server()
    try:
        # conn A: create + attach "Remux" -> a genuine full-size viewer.
        a, abuf = newconn()
        send(a, {"CreateSession": {"name": "Remux", "folder": None}})
        send(a, {"Attach": {"session_name": "Remux"}})
        send(a, {"Resize": {"cols": 100, "rows": 30}})
        time.sleep(0.4)
        flush(a, abuf)

        # Discover Remux's active-tab pane id.
        send(a, "ListSessionTree")
        tree = None
        end = time.time() + 1.5
        a.settimeout(0.3)
        while time.time() < end:
            try:
                msg = recv(a, abuf)
            except socket.timeout:
                continue
            except Exception:
                break
            if isinstance(msg, dict) and "SessionTree" in msg:
                tree = msg["SessionTree"]
                break
        assert tree is not None, "no SessionTree"
        pane_id = None
        for sess in tree.get("unfiled", []) + [e for f in tree.get("folders", []) for e in f.get("sessions", [])]:
            if sess["name"] == "Remux":
                pane_id = sess["tabs"][0]["panes"][0]["id"]
        assert pane_id is not None, f"no Remux pane in {tree}"
        a.settimeout(1.0)

        # conn B: attached to nothing; subscribes to Remux's pane while A views it.
        b, bbuf = newconn()
        send(b, {"SubscribePane": {"pane_id": pane_id, "cols": 40, "rows": 12, "size_demand": False}})
        pc_other = drain_pane_content(b, bbuf)
        assert pc_other is not None, "no PaneContent for B while A attached"
        vis_other = pc_other.get("session_visible", False)

        # A detaches -> no genuine full-size viewer remains.
        send(b, {"UnsubscribePane": {"pane_id": pane_id}})
        send(a, "Detach")
        time.sleep(0.4)
        flush(b, bbuf)  # avoid a stale (visible) PaneContent
        send(b, {"SubscribePane": {"pane_id": pane_id, "cols": 40, "rows": 12, "size_demand": False}})
        pc_gone = drain_pane_content(b, bbuf)
        assert pc_gone is not None, "no PaneContent for B after A detached"
        vis_gone = pc_gone.get("session_visible", False)

        print(f"B subscribes while OTHER viewer A attached: session_visible = {vis_other}  (want True)")
        print(f"B subscribes after A detached:              session_visible = {vis_gone}   (want False)")

        ok = (vis_other is True) and (vis_gone is False)
        print("PASS" if ok else "FAIL")
        sys.exit(0 if ok else 1)
    finally:
        p.kill()


if __name__ == "__main__":
    main()
