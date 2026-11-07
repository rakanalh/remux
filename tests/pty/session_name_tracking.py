#!/usr/bin/env python3
"""The client's session names must stay usable after a session is deleted.

The client keeps three names: the session it is on, the session Alt+o returns
to, and the local session a dropped remote falls back to. Attaching to a name
the server no longer has is refused, and a client that has already detached then
sits on a blank screen with its input dropped, so each case here deletes a
session and then takes one of the paths that attaches from a remembered name:
leaving a View, a dropped relay, or Alt+o.

Recovery is proved by typed input. The line assembles `RAN:<tag>:$REMUX_SESSION`,
so the marker cannot be the echo of the line and the session in it comes from the
shell that ran it. A refused `Attach` leaves `session '<name>' not found` in the
client log, which every case also reads, because Alt+o can fail while leaving the
client where it was.

Two kinds of client are covered. One the server moves, which gets a pushed tree.
One the server does not move, which hears nothing: it is inside a View, or on
another server, or on a session that is not the deleted one.

The remote is the fake-`ssh` recipe from CLAUDE.md. Each case leaves one survivor
wherever the server has to choose, because it picks from a `HashMap`.

A race needs a round trip long enough to act inside, so the last cases put a
framing proxy in the `ssh` shim. It delays every frame in both directions and can
put a frame in front of the ones in flight. Each of them first measures the delay,
so none can pass without one.

A refused `Attach` is allowed in `REFUSED_NAME`'s cases only. There the client
cannot know the name is dead until it has tried, and the reply repairs it. Where
that attempt is the path under test, `REFUSAL_REQUIRED` also demands the refusal,
so that a case cannot pass by a route that never reaches the repair.

Run from the repo root, all cases or the ones named:
    python3 tests/pty/session_name_tracking.py [case ...]
"""
import os
import re
import sys
import time
import traceback

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import (  # noqa: E402
    BIN, Tui, kill_relays, sm_compose_view, sm_expand, sm_goto, sm_open, sm_selected, sm_tree,
)
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
from harness import Client, Server  # noqa: E402

RUNDIR = "/tmp/rmx-snt"
REMOTE_RUNDIR = "/tmp/rmx-sntr"
SHIM_DIR = "/tmp/rmx-sntb"

ALT_O = b"\x1bo"

# One way. The round trip must be far longer than a keystroke's handling.
DELAY = 0.4

# The session name a case may see refused. Every other case allows no refusal.
REFUSED_NAME = {
    "hand_switch_then_delete_behind": "main",
    "hand_switch_during_delete": "main",
    "view_exit_session_killed_inside": "main",
    "view_exit_pane_exited_inside": "main",
    "view_deleted_after_session_killed": "main",
    "view_exit_with_no_session_left": "main",
    "owes_both_dead": "r1",
    "stale_jump_attached": "haven",
    "stale_tab_jump_attached": "haven",
    "stale_pane_jump_attached": "haven",
}

# `hand_switch_during_delete` is not here: a tree can tell the client first.
REFUSAL_REQUIRED = set(REFUSED_NAME) - {"hand_switch_during_delete"}

# argv: delay, control FIFO, remux binary. A line on the FIFO goes to the client
# at once: `tree` repeats the last `SessionTree` seen, `frame <json>` sends <json>.
# `cut` stops reading from the client, so its writes fail while what the relay
# sends still arrives, which is a dead ssh whose end of file is not read yet.
# The client treats its first failed write as the drop and ends the transport
# itself, which takes the proxy with it, so the end of file never matters.
# `drop` ends the proxy if it is still there; the cases send it with `gone_ok`,
# because by then it normally is not.
PROXY = r"""
import collections, os, struct, subprocess, sys, threading, time

delay, control, remux = float(sys.argv[1]), sys.argv[2], sys.argv[3]
relay = subprocess.Popen([remux, "relay"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
to_client = threading.Lock()
last_tree = [None]
cut = [False]


def frames(source):
    while True:
        head = source.read(4)
        if len(head) < 4:
            return
        yield head + source.read(struct.unpack(">I", head)[0])


def send(sink, frame, lock):
    with lock:
        sink.write(frame)
        sink.flush()


def pump(source, sink, lock):
    queue, ready = collections.deque(), threading.Condition()

    def writer():
        while True:
            with ready:
                while not queue:
                    ready.wait()
                due, frame = queue.popleft()
            time.sleep(max(0, due - time.time()))
            send(sink, frame, lock)

    threading.Thread(target=writer, daemon=True).start()
    for frame in frames(source):
        if frame[4:].startswith(b'{"SessionTree"'):
            last_tree[0] = frame
        with ready:
            queue.append((time.time() + delay, frame))
            ready.notify()
    if not cut[0]:
        os._exit(0)


def controller():
    for line in os.fdopen(os.open(control, os.O_RDWR), "rb"):
        if line.strip() == b"tree":
            send(sys.stdout.buffer, last_tree[0], to_client)
        elif line.startswith(b"frame "):
            body = line[6:].strip()
            send(sys.stdout.buffer, struct.pack(">I", len(body)) + body, to_client)
        elif line.strip() == b"cut":
            # The pipe loses its last reader once the read in progress returns,
            # so the client's next write but one fails.
            cut[0] = True
            os.dup2(os.open(os.devnull, os.O_RDONLY), 0)
        elif line.strip() == b"drop":
            os._exit(0)


threading.Thread(target=controller, daemon=True).start()
threading.Thread(
    target=pump, args=(sys.stdin.buffer, relay.stdin, threading.Lock()), daemon=True
).start()
pump(relay.stdout, sys.stdout.buffer, to_client)
"""

REMOTE_CONFIG = """
[remotes.mini]
ssh = "whatever"
remux_path = "remux"
auto_connect = true
"""

SIDEBAR_CONFIG = """
[[sidebar]]
edge = "left"
size = 24
visible = true

[[sidebar.panel]]
plugin = "sessions"
weight = 1
"""

fails = []


def check(ok, what):
    print(f"  {'ok  ' if ok else 'FAIL'} {what}")
    if not ok:
        fails.append(what)


def wait_for(t, predicate, timeout=4.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        t.pump(0.1)
    return predicate()


def status_names(t, session):
    return re.search(rf"(^|\s){re.escape(session)} │", t.rows_text()[-1]) is not None


def close_manager(t):
    if t.has("Session Manager"):
        t.send("\x1b", 0.6)


def runs_in(t, label, session, tag):
    """Typed input runs in `session`, which proves more than any painted frame."""
    check(t.alive(), f"{label}: the client is alive")
    if not t.alive():
        return
    t.send(f"printf 'RAN:%s:%s\\n' {tag} \"$REMUX_SESSION\"\r", 0.2)
    marker = f"RAN:{tag}:{session}"
    ran = wait_for(t, lambda: t.has(marker))
    check(ran, f"{label}: typed input ran in {session!r}, {marker!r} is on screen")
    if not ran:
        t.dump(label)


def create(c, name):
    c.send({"CreateSession": {"name": name, "folder": None}})
    time.sleep(0.3)


def kill_session(c, name):
    c.send({"KillSession": {"name": name}})
    time.sleep(0.4)


def is_session_row(row, session):
    """A session row is `▼ haven`, or `▼ * haven (1)` while a client is on it."""
    return re.search(rf"[▼▶*] {re.escape(session)}( \(\d+\))?\s*$", row.text) is not None


def goto_session(t, session):
    return sm_goto(t, lambda r: is_session_row(r, session), f"the row of {session!r}")


def switch_local(t, session):
    sm_open(t)
    goto_session(t, session)
    t.send("\r", 0.6)
    assert wait_for(t, lambda: status_names(t, session)), (
        f"never reached {session!r}: {t.rows_text()[-1]!r}")


def switch_remote(t, session):
    sm_open(t)
    if not any(is_session_row(r, session) for r in sm_tree(t)):
        sm_expand(t, "mini")
    goto_session(t, session)
    t.send("\r", 0.8)
    assert wait_for(t, lambda: status_names(t, session)), (
        f"never reached {session!r}: {t.rows_text()[-1]!r}")


def delete_in_manager(t, session):
    sm_open(t)
    goto_session(t, session)
    t.send("s", 0.3)
    t.send("x", 0.3)
    prompt = f"Delete session '{session}'?"
    assert wait_for(t, lambda: t.has(prompt), 2.0), f"no prompt {prompt!r}"
    t.send("y", 1.0)
    close_manager(t)


def view_over_current_tab(t, tag):
    """Compose a View over the first pane of the expanded session and enter it.

    The marker is printed from inside the cell, so reaching it proves that the
    client is in the View and that the keys go to the pane.
    """
    sm_compose_view(t, tab="Tab 1", panes=(0,), settle=1.5)
    t.send(f"printf 'CELL:%s\\n' {tag}\r", 0.2)
    assert wait_for(t, lambda: t.has(f"CELL:{tag}")), "never reached the View's cell"


def view_ids(c):
    """The ids in the last `ViewList` broadcast, which every client receives."""
    ids = []
    for msg in c.drain(0.5):
        if isinstance(msg, dict) and "ViewList" in msg:
            ids = [v["id"] for v in msg["ViewList"]["views"]]
    return ids


class Rig:
    """A real client on a throwaway local server, with an optional fake remote."""

    def __init__(self, remote=False, config="", delay=0):
        self.remote = None
        self.control = f"{SHIM_DIR}/control"
        env = {}
        if remote:
            self.remote = Server(REMOTE_RUNDIR).start(config="")
            self.rc = Client(self.remote.sock)
            self.rc.hello()
            self.rc.drain(0.2)
            create(self.rc, "r1")
            create(self.rc, "r2")
            os.makedirs(SHIM_DIR, exist_ok=True)
            run = f"exec {BIN} relay\n"
            if delay:
                with open(f"{SHIM_DIR}/proxy.py", "w") as f:
                    f.write(PROXY)
                if os.path.exists(self.control):
                    os.unlink(self.control)
                os.mkfifo(self.control)
                run = f"exec {sys.executable} {SHIM_DIR}/proxy.py {delay} {self.control} {BIN}\n"
            with open(f"{SHIM_DIR}/ssh", "w") as f:
                f.write(
                    "#!/bin/sh\n"
                    f"export XDG_RUNTIME_DIR={REMOTE_RUNDIR}/run\n"
                    f"export XDG_STATE_HOME={REMOTE_RUNDIR}/state\n"
                    f"export XDG_DATA_HOME={REMOTE_RUNDIR}/data\n"
                    + run
                )
            os.chmod(f"{SHIM_DIR}/ssh", 0o755)
            env["PATH"] = f"{SHIM_DIR}:{os.environ['PATH']}"
            config += REMOTE_CONFIG
        self.t = Tui(RUNDIR, cols=120, rows=40, config=config, extra_env=env).start()
        self.c = Client(f"{self.t.rundir}/run/remux.sock")
        self.c.hello()
        self.c.drain(0.3)
        if delay:
            self.t.pump(2 * delay + 1.0)

    def inject(self, line, gone_ok=False):
        # Non-blocking, because a client that dropped the remote, or exited, took
        # the proxy with it, and a blocking open of a FIFO nobody reads would hang
        # the run. `gone_ok` is for an end of file the client need not wait for.
        try:
            fd = os.open(self.control, os.O_WRONLY | os.O_NONBLOCK)
        except OSError as e:
            check(gone_ok, f"the proxy took {line!r} ({e})")
            return
        with os.fdopen(fd, "w") as f:
            f.write(line + "\n")

    def finish(self, refused_name=None, refusal_required=False):
        self.t.kill()
        if self.remote is not None:
            self.remote.kill()
        for which in ("server", "client"):
            log = self.t.log(which)
            check(bool(log), f"{which}.log has content")
            check("panicked at" not in log, f"no panic in {which}.log")
        if self.remote is not None:
            log = open(f"{REMOTE_RUNDIR}/state/remux/server.log").read()
            check("panicked at" not in log, "no panic in the remote's server.log")
        refused = re.findall(r"session '([^']*)' not found", self.t.log("client"))
        if refused_name is None:
            check(not refused, f"no Attach was refused: {refused}")
        else:
            check(set(refused) <= {refused_name}, f"only {refused_name!r} was refused: {refused}")
            if refusal_required:
                check(bool(refused), f"{refused_name!r} was tried and refused")


def view_exit_after_sx_local(r):
    """Case 1: `sx` on the current session, then a View, then `wq`."""
    t, c = r.t, r.c
    create(c, "haven")
    delete_in_manager(t, "main")
    runs_in(t, "moved", "haven", "a1")
    view_over_current_tab(t, "a2")
    t.prefix(b"wq", 1.0)
    runs_in(t, "after wq", "haven", "a3")


def view_exit_after_sx_remote(r):
    """Case 2: the same on a remote."""
    t = r.t
    switch_remote(t, "r1")
    delete_in_manager(t, "r1")
    runs_in(t, "moved", "r2", "b1")
    view_over_current_tab(t, "b2")
    t.prefix(b"wq", 1.0)
    runs_in(t, "after wq", "r2", "b3")


def drop_after_local_sx(r):
    """Case 3: local `sx`, go to a remote, drop the relay."""
    t, c = r.t, r.c
    create(c, "haven")
    delete_in_manager(t, "main")
    runs_in(t, "moved", "haven", "c1")
    switch_remote(t, "r1")
    kill_relays(REMOTE_RUNDIR)
    t.pump(1.5)
    runs_in(t, "after the drop", "haven", "c2")


def drop_after_remote_sx(r):
    """Case 3, the remote not tried by hand: moved on the remote, then dropped."""
    t, c = r.t, r.c
    create(c, "haven")
    switch_remote(t, "r1")
    delete_in_manager(t, "r1")
    runs_in(t, "moved", "r2", "d1")
    kill_relays(REMOTE_RUNDIR)
    t.pump(1.5)
    runs_in(t, "after the drop", "main", "d2")


def drop_with_local_killed_behind(r):
    """Case 3 with a client the server cannot move: it is detached from local."""
    t, c = r.t, r.c
    create(c, "haven")
    switch_remote(t, "r1")
    kill_session(c, "main")
    kill_relays(REMOTE_RUNDIR)
    t.pump(1.5)
    runs_in(t, "after the drop", "haven", "e1")


def drop_with_manager_open(r):
    """A drop with the manager open keeps it open, and lands on a live session."""
    t, c = r.t, r.c
    create(c, "haven")
    switch_remote(t, "r1")
    kill_session(c, "main")
    sm_open(t)
    kill_relays(REMOTE_RUNDIR)
    t.pump(1.5)
    check(t.has("Session Manager"), "the manager stayed open")
    close_manager(t)
    runs_in(t, "after the drop", "haven", "f1")


def alt_o_follows_the_survivor(r):
    """Case 4: the survivor, not the deleted name, is what Alt+o returns to."""
    t, c = r.t, r.c
    create(c, "haven")
    delete_in_manager(t, "main")
    runs_in(t, "moved", "haven", "g1")
    create(c, "third")
    switch_local(t, "third")
    t.send(ALT_O, 1.0)
    runs_in(t, "Alt+o", "haven", "g2")
    t.send(ALT_O, 1.0)
    runs_in(t, "Alt+o back", "third", "g3")


def alt_o_previous_is_the_survivor(r):
    """Moved onto the session Alt+o named: there is nothing left to toggle to."""
    t, c = r.t, r.c
    create(c, "side")
    switch_local(t, "side")
    delete_in_manager(t, "side")
    runs_in(t, "moved", "main", "h1")
    t.send(ALT_O, 0.8)
    runs_in(t, "Alt+o", "main", "h2")
    t.send(ALT_O, 0.8)
    runs_in(t, "Alt+o again", "main", "h3")


def alt_o_previous_deleted_in_manager(r):
    """`sx` on the session Alt+o names, from the session the client stays on."""
    t, c = r.t, r.c
    create(c, "side")
    switch_local(t, "side")
    switch_local(t, "main")
    delete_in_manager(t, "side")
    t.send(ALT_O, 0.8)
    runs_in(t, "Alt+o", "main", "i1")


def hand_switch_then_delete_behind(r):
    """Switched by hand, then the session left behind was deleted elsewhere.

    The client is not moved, so nothing tells it. Alt+o must leave it where it
    is, and its names must still work afterwards.
    """
    t, c = r.t, r.c
    create(c, "haven")
    create(c, "third")
    switch_local(t, "haven")
    kill_session(c, "main")
    t.send(ALT_O, 0.8)
    runs_in(t, "Alt+o", "haven", "j1")
    switch_local(t, "third")
    t.send(ALT_O, 0.8)
    runs_in(t, "Alt+o after a switch", "haven", "j2")


def hand_switch_during_delete(r):
    """The client's own switch and a delete of the session it leaves, together."""
    t, c = r.t, r.c
    create(c, "haven")
    sm_open(t)
    goto_session(t, "haven")
    c.send({"KillSession": {"name": "main"}})
    t.send("\r", 1.0)
    check(wait_for(t, lambda: status_names(t, "haven")), "the switch landed")
    runs_in(t, "hand switch", "haven", "k1")
    t.send(ALT_O, 0.8)
    runs_in(t, "Alt+o", "haven", "k2")


def alt_o_cross_server_target_deleted(r):
    """Alt+o must not leave the remote for a local session that is gone."""
    t, c = r.t, r.c
    create(c, "haven")
    switch_remote(t, "r1")
    kill_session(c, "main")
    t.send(ALT_O, 1.0)
    runs_in(t, "Alt+o", "r1", "l1")


def view_exit_session_killed_inside(r):
    """Case 5: the client is inside the View when its session is deleted."""
    t, c = r.t, r.c
    create(c, "haven")
    view_over_current_tab(t, "m1")
    kill_session(c, "main")
    t.prefix(b"wq", 1.0)
    runs_in(t, "after wq", "haven", "m2")


def view_exit_pane_exited_inside(r):
    """Case 5 by pane exit: the shell in the View's only cell ends the session."""
    t, c = r.t, r.c
    create(c, "haven")
    view_over_current_tab(t, "n1")
    t.send("exit\r", 1.0)
    t.prefix(b"wq", 1.0)
    runs_in(t, "after wq", "haven", "n2")


def view_deleted_after_session_killed(r):
    """Case 5 through the other way out: the View is deleted under the client."""
    t, c = r.t, r.c
    create(c, "haven")
    view_over_current_tab(t, "o1")
    kill_session(c, "main")
    ids = view_ids(c)
    assert len(ids) == 1, f"expected one view, got {ids}"
    c.send({"ViewDelete": {"id": ids[0]}})
    t.pump(1.0)
    runs_in(t, "after the View went", "haven", "o2")


def view_exit_with_no_session_left(r):
    """Nothing is left to show: the client exits, as it does on the last session."""
    t = r.t
    view_over_current_tab(t, "p1")
    t.send("exit\r", 1.0)
    t.prefix(b"wq", 1.0)
    check(wait_for(t, lambda: not t.alive()), "the client exited")
    check(t.child.exitstatus == 0, f"its exit status is 0: {t.child.exitstatus}")
    # A `?` bail also exits, and leaves `result=false` here.
    check("restoring terminal, result=true" in t.log("client"), "the event loop ended cleanly")


def subscribed_client_moved(r):
    """A client with a sessions panel, which trees reach all the time."""
    t, c = r.t, r.c
    create(c, "haven")
    delete_in_manager(t, "main")
    runs_in(t, "moved", "haven", "q1")
    view_over_current_tab(t, "q2")
    t.prefix(b"wq", 1.0)
    runs_in(t, "after wq", "haven", "q3")
    create(c, "third")
    switch_local(t, "third")
    t.send(ALT_O, 1.0)
    runs_in(t, "Alt+o", "haven", "q4")


def normal_alt_o(r):
    """Guard: nothing was deleted, so Alt+o keeps toggling."""
    t, c = r.t, r.c
    create(c, "haven")
    switch_local(t, "haven")
    t.send(ALT_O, 1.0)
    runs_in(t, "Alt+o", "main", "s1")
    t.send(ALT_O, 1.0)
    runs_in(t, "Alt+o back", "haven", "s2")
    t.send(ALT_O, 1.0)
    runs_in(t, "Alt+o again", "main", "s3")


def alt_o_keeps_a_name_on_another_server(r):
    """Guard: a local tree does not list the remote's sessions, and proves nothing."""
    t = r.t
    switch_remote(t, "r1")
    switch_local(t, "main")
    sm_open(t)
    close_manager(t)
    t.send(ALT_O, 1.2)
    runs_in(t, "Alt+o to the remote", "r1", "t1")
    t.send(ALT_O, 1.2)
    runs_in(t, "Alt+o back", "main", "t2")


def alt_o_survivor_of_unrelated_delete(r):
    """Guard: deleting an unrelated session does not clear Alt+o."""
    t, c = r.t, r.c
    create(c, "haven")
    create(c, "other")
    switch_local(t, "haven")
    delete_in_manager(t, "other")
    t.send(ALT_O, 1.0)
    runs_in(t, "Alt+o", "main", "u1")


def normal_view_exit(r):
    """Guard: nothing was deleted, so the View returns to the same session."""
    t, c = r.t, r.c
    create(c, "haven")
    view_over_current_tab(t, "v1")
    t.prefix(b"wq", 1.0)
    runs_in(t, "after wq", "main", "v2")


def normal_view_exit_remote(r):
    """Guard: the same on the remote, which the client must not leave."""
    t = r.t
    switch_remote(t, "r1")
    view_over_current_tab(t, "w1")
    t.prefix(b"wq", 1.0)
    runs_in(t, "after wq", "r1", "w2")


def normal_drop(r):
    """Guard: a drop returns to the local session the client came from."""
    t, c = r.t, r.c
    create(c, "haven")
    switch_remote(t, "r1")
    kill_relays(REMOTE_RUNDIR)
    t.pump(1.5)
    runs_in(t, "after the drop", "main", "x1")


def delay_applied(t):
    """A typed line's echo takes a full round trip, or the proxy is not in the path."""
    start = time.time()
    t.send("printf 'RTT:%s\\n' measured\r", 0)
    seen = wait_for(t, lambda: t.has("RTT:measured"))
    elapsed = time.time() - start
    check(seen and elapsed >= 2 * DELAY * 0.9, f"the round trip took {elapsed:.2f}s")


def on_r1_with_r2_previous(t):
    switch_remote(t, "r2")
    switch_remote(t, "r1")
    delay_applied(t)


def typing_during_view_exit(r):
    """Keys typed behind `wq` run, and the session returns within one round trip."""
    t = r.t
    switch_remote(t, "r1")
    delay_applied(t)
    view_over_current_tab(t, "y1")
    start = time.time()
    t.prefix(b"wq", 0)
    t.send("printf 'RAN:%s:%s\\n' y2 \"$REMUX_SESSION\"\r", 0)
    ran = wait_for(t, lambda: t.has("RAN:y2:r1"), 5.0)
    elapsed = time.time() - start
    check(ran, "the keys typed during the View exit ran in 'r1'")
    check(elapsed < 3 * DELAY + 0.3, f"the session was back after {elapsed:.2f}s")


def alt_o_during_pending_view_exit(r):
    """Alt+o inside the View exit's round trip, with a dead previous session."""
    t = r.t
    on_r1_with_r2_previous(t)
    # After composing, because the manager's tree would tell the client.
    view_over_current_tab(t, "z1")
    kill_session(r.rc, "r2")
    t.prefix(b"wq", 0)
    t.send(ALT_O, 0)
    t.pump(4 * DELAY)
    runs_in(t, "after wq and Alt+o", "r1", "z2")


def owes_both_dead(r):
    """The View exit's session and Alt+o's target are both gone, inside one round trip.

    The Alt+o must not take over from the check that owes the attach, because its
    own target is dead too and nothing would then attach at all.
    """
    t = r.t
    create(r.rc, "r3")
    on_r1_with_r2_previous(t)
    view_over_current_tab(t, "ac1")
    kill_session(r.rc, "r1")
    kill_session(r.rc, "r2")
    t.prefix(b"wq", 0)
    t.send(ALT_O, 0)
    t.pump(4 * DELAY)
    runs_in(t, "after wq and Alt+o", "r3", "ac2")


def runs_on_status(t, label, session, tag):
    """For a tab that moved between sessions, whose shell keeps the
    `$REMUX_SESSION` it started with: the session is the status bar's, and the
    assembled marker only proves the input ran."""
    check(t.alive(), f"{label}: the client is alive")
    if not t.alive():
        return
    check(wait_for(t, lambda: status_names(t, session)),
          f"{label}: the status bar names {session!r} ({t.rows_text()[-1].strip()[:40]!r})")
    t.send(f"printf 'RAN:%s\\n' {tag}\r", 0.2)
    check(wait_for(t, lambda: t.has(f"RAN:{tag}")), f"{label}: typed input ran")


def menu_click(t, label):
    """Click the open context menu's row whose whole text is `label`."""
    rows = t.rows_text()
    top = next(y for y, row in enumerate(rows) if "\u250c" in row)
    left, right = rows[top].index("\u250c"), rows[top].index("\u2510")
    y = top + 1
    while rows[y][left] == "\u2502":
        if rows[y][left + 1:right].strip() == label:
            x = left + 2
            t.send(f"\x1b[<0;{x + 1};{y + 1}M\x1b[<0;{x + 1};{y + 1}m".encode(), 1.5)
            return
        y += 1
    raise AssertionError(f"no menu row {label!r}: {rows[top:y]}")


def tab_move_follows_server(r):
    """The server follows the tab it has active, which the menu's tree did not show.

    The picker is built while Tab 1 is in the background. Another client makes it
    the active tab before the pick, so the server moves this client with it and
    says nothing but a tree. Alt+o must then return to `main`.
    """
    t, c = r.t, r.c
    create(c, "haven")
    switch_local(t, "haven")
    switch_local(t, "main")
    t.send(b"\x1bt", 0.8)
    sm_open(t)
    tree = sm_tree(t)
    main_row = next(i for i, row in enumerate(tree) if is_session_row(row, "main"))
    tab_row = next(row for row in tree[main_row + 1:] if "Tab 1" in row.text)
    sm_goto(t, lambda row: row.y == tab_row.y, "main's Tab 1")
    t.send("t", 0.3)
    t.send("m", 0.8)
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": 120, "rows": 40}})
    time.sleep(0.3)
    c.send({"Command": {"SessionSwitchTab": {"session": "main", "tab_index": 0}}})
    time.sleep(0.5)
    menu_click(t, "haven")
    close_manager(t)
    runs_on_status(t, "the server moved the client", "haven", "ad1")
    t.send(ALT_O, 1.2)
    runs_on_status(t, "Alt+o after the move", "main", "ad2")


def stale_jump_attached(r):
    """A jump from a manager whose tree still lists a deleted session.

    The client stays where it is, and Alt+o still returns to the session it
    named before the jump.
    """
    t, c = r.t, r.c
    create(c, "haven")
    create(c, "other")
    switch_local(t, "other")
    switch_local(t, "main")
    sm_open(t)
    goto_session(t, "haven")
    kill_session(c, "haven")
    check(any(is_session_row(row, "haven") for row in sm_tree(t)), "the manager still lists 'haven'")
    t.send("\r", 1.2)
    runs_in(t, "after the jump", "main", "ae1")
    t.send(ALT_O, 1.2)
    runs_in(t, "Alt+o", "other", "ae2")


def stale_row_jump(r, row_of, tag):
    """`stale_jump_attached`, with the cursor on the row `row_of` picks under haven."""
    t, c = r.t, r.c
    create(c, "haven")
    create(c, "other")
    switch_local(t, "other")
    switch_local(t, "main")
    sm_open(t)
    row = row_of(t)
    kill_session(c, "haven")
    check(sm_selected(t).y == row.y, f"the cursor is on {row.text.strip()!r}")
    t.send("\r", 1.2)
    runs_in(t, "after the jump", "main", f"{tag}1")
    t.send(ALT_O, 1.2)
    runs_in(t, "Alt+o", "other", f"{tag}2")


def havens_tab_row(t):
    tree = sm_tree(t)
    at = next(i for i, row in enumerate(tree) if is_session_row(row, "haven"))
    tab = next(row for row in tree[at + 1:] if "Tab 1" in row.text)
    return sm_goto(t, lambda row: row.y == tab.y, "haven's Tab 1")


def stale_tab_jump_attached(r):
    """A jump to a tab row of a deleted session: the server must refuse the switch too."""
    stale_row_jump(r, havens_tab_row, "aj")


def stale_pane_jump_attached(r):
    """The same from that tab's pane row."""
    def havens_pane_row(t):
        tab = havens_tab_row(t)
        t.send("l", 0.5)
        tree = sm_tree(t)
        pane = tree[next(i for i, row in enumerate(tree) if row.y == tab.y) + 1]
        return sm_goto(t, lambda row: row.y == pane.y, "the pane under haven's Tab 1")

    stale_row_jump(r, havens_pane_row, "ak")


def stale_jump_from_view(r):
    """The same jump from inside a View: it must not leave the client detached."""
    t, c = r.t, r.c
    create(c, "haven")
    view_over_current_tab(t, "af1")
    sm_open(t)
    goto_session(t, "haven")
    kill_session(c, "haven")
    check(any(is_session_row(row, "haven") for row in sm_tree(t)), "the manager still lists 'haven'")
    t.send("\r", 1.2)
    runs_in(t, "after the jump", "main", "af2")


def cut_the_remote(r, t):
    """Make the client's writes to the remote fail before it reads end of file."""
    r.inject("cut")
    t.pump(0.3)
    # The proxy's read in progress takes this one, so the pipe has no reader after it.
    t.send("x", 0.6)


def dropped_writes_logged(t):
    check("failed and was dropped" in t.log("client"),
          "the client logged a dropped write, so the cut took effect")


def drop_race_view_exit(r):
    """`wq` on a remote whose ssh has died, before the client reads end of file."""
    t = r.t
    switch_remote(t, "r1")
    view_over_current_tab(t, "ag1")
    cut_the_remote(r, t)
    t.prefix(b"wq", 0.8)
    check(t.alive(), "the client survived `wq` on a dead pipe")
    dropped_writes_logged(t)
    r.inject("drop", gone_ok=True)
    t.pump(1.5)
    runs_in(t, "after the drop", "main", "ag2")


def drop_race_alt_o(r):
    """Alt+o on a remote whose ssh has died, before the client reads end of file."""
    t = r.t
    switch_remote(t, "r2")
    switch_remote(t, "r1")
    cut_the_remote(r, t)
    t.send(ALT_O, 0.8)
    check(t.alive(), "the client survived Alt+o on a dead pipe")
    dropped_writes_logged(t)
    r.inject("drop", gone_ok=True)
    t.pump(1.5)
    runs_in(t, "after the drop", "main", "ah1")


def drop_race_typing(r):
    """Typing into a remote whose ssh has died, before the client reads end of file."""
    t = r.t
    switch_remote(t, "r1")
    cut_the_remote(r, t)
    t.send("echo lost\r", 0.8)
    check(t.alive(), "the client survived typing on a dead pipe")
    dropped_writes_logged(t)
    r.inject("drop", gone_ok=True)
    t.pump(1.5)
    runs_in(t, "after the drop", "main", "ai1")


def drop_race_no_end_of_file(r):
    """A dead pipe whose end of file never comes: the first failed write is the drop."""
    t = r.t
    switch_remote(t, "r1")
    cut_the_remote(r, t)
    t.send("echo lost\r", 1.5)
    dropped_writes_logged(t)
    runs_in(t, "with the remote's output still open", "main", "al1")


def typing_during_alt_o(r):
    """Keys typed behind Alt+o go to the session it switched to."""
    t = r.t
    on_r1_with_r2_previous(t)
    t.send(ALT_O, 0)
    t.send("printf 'RAN:%s:%s\\n' aa \"$REMUX_SESSION\"\r", 0)
    check(wait_for(t, lambda: t.has("RAN:aa:r2"), 5.0),
          f"the keys ran in 'r2': {[row.strip() for row in t.rows_text() if 'RAN:' in row]}")


def create_with_tree_in_flight(r):
    """A tree from before `CreateSession`, behind an unrelated error, moves nothing."""
    t = r.t
    switch_remote(t, "r1")
    delay_applied(t)
    sm_open(t)
    close_manager(t)
    t.prefix(b"xn", 0.5)
    t.send("fresh", 0.3)
    t.send("\r", 0.1)
    r.inject('frame {"Error":{"message":"pane 7 not found","code":null}}')
    r.inject("tree")
    # Delivered only if the control thread survived the `tree` line, which it does
    # not when the proxy has no tree to repeat.
    r.inject('frame {"Error":{"message":"pane 8 not found","code":null}}')
    check(wait_for(t, lambda: status_names(t, "fresh")), "the client reached 'fresh'")
    log = t.log("client")
    check("Server error: pane 7 not found" in log, "the injected error reached the client")
    check("Server error: pane 8 not found" in log, "the injected tree went out before it")
    runs_in(t, "on the new session", "fresh", "ab1")
    view_over_current_tab(t, "ab2")
    t.prefix(b"wq", 1.5 + 2 * DELAY)
    runs_in(t, "after wq", "fresh", "ab3")


CASES = [
    (view_exit_after_sx_local, {}),
    (view_exit_after_sx_remote, {"remote": True}),
    (drop_after_local_sx, {"remote": True}),
    (drop_after_remote_sx, {"remote": True}),
    (drop_with_local_killed_behind, {"remote": True}),
    (drop_with_manager_open, {"remote": True}),
    (alt_o_follows_the_survivor, {}),
    (alt_o_previous_is_the_survivor, {}),
    (alt_o_previous_deleted_in_manager, {}),
    (hand_switch_then_delete_behind, {}),
    (hand_switch_during_delete, {}),
    (alt_o_cross_server_target_deleted, {"remote": True}),
    (view_exit_session_killed_inside, {}),
    (view_exit_pane_exited_inside, {}),
    (view_deleted_after_session_killed, {}),
    (view_exit_with_no_session_left, {}),
    (subscribed_client_moved, {"config": SIDEBAR_CONFIG}),
    (normal_alt_o, {}),
    (alt_o_keeps_a_name_on_another_server, {"remote": True}),
    (alt_o_survivor_of_unrelated_delete, {}),
    (normal_view_exit, {}),
    (normal_view_exit_remote, {"remote": True}),
    (normal_drop, {"remote": True}),
    (typing_during_view_exit, {"remote": True, "delay": DELAY}),
    (alt_o_during_pending_view_exit, {"remote": True, "delay": DELAY}),
    (owes_both_dead, {"remote": True, "delay": DELAY}),
    (typing_during_alt_o, {"remote": True, "delay": DELAY}),
    (create_with_tree_in_flight, {"remote": True, "delay": DELAY}),
    (tab_move_follows_server, {}),
    (stale_jump_attached, {}),
    (stale_jump_from_view, {}),
    (drop_race_view_exit, {"remote": True, "delay": 0.01}),
    (drop_race_alt_o, {"remote": True, "delay": 0.01}),
    (drop_race_typing, {"remote": True, "delay": 0.01}),
    (stale_tab_jump_attached, {}),
    (stale_pane_jump_attached, {}),
    (drop_race_no_end_of_file, {"remote": True, "delay": 0.01}),
]


def main():
    # `kill_relays` recognises a relay by the name `remux`. Under another name it
    # kills nothing, the drop never happens, and the drop cases fail falsely.
    assert os.path.basename(BIN) == "remux", f"REMUX_BIN must be named remux: {BIN}"
    wanted = set(sys.argv[1:])
    unknown = wanted - {f.__name__ for f, _ in CASES}
    if unknown:
        raise SystemExit(f"unknown cases: {sorted(unknown)}")
    summary = []
    for case, rig_args in CASES:
        if wanted and case.__name__ not in wanted:
            continue
        print(f"== {case.__name__}")
        before = len(fails)
        r = Rig(**rig_args)
        try:
            case(r)
        except Exception:
            traceback.print_exc()
            r.t.dump(case.__name__)
            fails.append(f"{case.__name__}: raised")
        finally:
            r.finish(REFUSED_NAME.get(case.__name__), case.__name__ in REFUSAL_REQUIRED)
        summary.append((case.__name__, len(fails) - before))
    print("\n== summary")
    for name, failed in summary:
        print(f"  {'FAIL' if failed else 'ok  '} {name}")
    print(f"{len(summary) - sum(1 for _, n in summary if n)}/{len(summary)} cases green")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
