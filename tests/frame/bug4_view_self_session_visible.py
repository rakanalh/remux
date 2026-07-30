#!/usr/bin/env python3
"""bug4 repro: a View cell aliasing a pane whose home session THIS SAME client is
attached to is wrongly flagged session_visible=true, so the cell shows the
"Active in session" placeholder instead of streaming content.

The real client, on entering a view, never detaches its foreground session, so a
single connection is both (a) attached to "Remux" and (b) a view-cell subscriber
of Remux's own pane. `pane_session_visible` counts that connection => true.

Expected AFTER fix: the client detaches on view entry, so by the time it
subscribes there is no attached viewer of "Remux" => session_visible=false =>
the cell streams content.

This harness asserts the CORRECT behavior: subscribing while NOT attached to the
pane's session yields session_visible=false. It also documents the buggy path
(attached + subscribe => true).
"""
import json, os, shutil, socket, struct, subprocess, sys, time

BIN, RUNDIR = "target/debug/remux", "/tmp/rmxbug4"
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


def drain_pane_content(sock, buf, timeout=1.5):
    """Return the first PaneContent seen (or None)."""
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
        # Connection A: create + attach "Remux".
        a, abuf = newconn()
        send(a, {"CreateSession": {"name": "Remux", "folder": None}})
        send(a, {"Attach": {"session_name": "Remux"}})
        send(a, {"Resize": {"cols": 100, "rows": 30}})
        time.sleep(0.4)
        # drain A's frames
        a.settimeout(0.2)
        try:
            while True:
                recv(a, abuf)
        except Exception:
            pass

        # Find Remux's active-tab pane id.
        send(a, {"ListSessionTree": None} if False else "ListSessionTree")
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

        # --- Buggy path: SAME attached connection subscribes (models today's client) ---
        send(a, {"SubscribePane": {"pane_id": pane_id, "cols": 40, "rows": 12, "size_demand": False}})
        pc_attached = drain_pane_content(a, abuf)
        assert pc_attached is not None, "no PaneContent while attached"
        vis_attached = pc_attached.get("session_visible", False)
        send(a, {"UnsubscribePane": {"pane_id": pane_id}})

        # --- Correct path: detach first (what the fix makes the client do), then subscribe ---
        send(a, "Detach")
        time.sleep(0.4)
        # Flush ALL queued bytes so we don't read a stale (attached) PaneContent.
        a.settimeout(0.2)
        try:
            while True:
                recv(a, abuf)
        except Exception:
            pass
        send(a, {"SubscribePane": {"pane_id": pane_id, "cols": 40, "rows": 12, "size_demand": False}})
        pc_detached = drain_pane_content(a, abuf)
        assert pc_detached is not None, "no PaneContent after detach"
        vis_detached = pc_detached.get("session_visible", False)

        print(f"attached-subscriber session_visible = {vis_attached}  (bug4: shows placeholder when it should stream)")
        print(f"detached-subscriber session_visible = {vis_detached}  (correct: streams content)")

        ok = (vis_detached is False)
        # The bug is that the *client stays attached*; at the protocol level the
        # attached-subscriber genuinely IS session-visible. The FIX is client-side
        # (detach on view entry). This harness proves detach flips it to false.
        if vis_attached and not vis_detached:
            print("REPRO CONFIRMED: attached=>visible(placeholder), detached=>content. "
                  "Fix = client must Detach on view entry.")
        elif not vis_attached:
            print("NOTE: attached-subscriber already not-visible; mechanism differs — investigate.")
        print("PASS" if ok else "FAIL")
        sys.exit(0 if ok else 1)
    finally:
        p.kill()


if __name__ == "__main__":
    main()
