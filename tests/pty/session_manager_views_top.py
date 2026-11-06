#!/usr/bin/env python3
"""The session manager paints the Views group first under the `local` server.

Why a real PTY: the row order the user sees is the painted overlay, and every
row-to-item mapping (the opening snap, chords, Space marks, `va`, Enter) is only
proven by acting on a row found on the screen.

The tree holds names that defeat an alphabetical or an accidental order:
  * folder `archive` (session `filed` inside) and session `alpha` sort BEFORE
    "Views", session `zeta` sorts AFTER it, and `main` is the attached session.
  * remote `mini` is connected through the fake-remote recipe and holds session
    `aardvark`. Views are the local server's, so they never render under it.

Asserted here:
  1. Views is the first child of `local`, above the folder and every session, and
     it opens as one collapsed row with no view or cell rows.
  2. The manager opens with the highlight on `main`, not on the Views row.
  3. `l` on the group shows the view, still collapsed. `l` on the view shows its
     cell.
     Reopening keeps that, and a collapse is remembered the same way.
  4. `sr` on the `alpha` row renames `alpha` and nothing else.
  5. Space on the pane of `zeta` and `va` add THAT pane to the existing view.
  6. Enter on the `zeta` row switches to `zeta`: the reopened manager snaps there.
  7. Enter on the view row enters the view, which shows MAIN:ok and ZETA:ok. Only
     the view can show both, and the zeta session on screen before it has no
     MAIN:ok. This is the evidence for case 5.
  8. A query reaches a view inside the collapsed group and selects it. A query
     matching a view and a session selects the session, and Enter switches to it.
     The next open still shows the group collapsed, so a search leaves no trace.
  9. A query matching a pane that is also a view cell reaches that cell through
     the collapsed rows, selects the pane's own row, and Enter switches to that
     pane's session without opening the view.
 10. With `mini` expanded there is still one Views group, under `local`, above
     `mini`, and `aardvark` is below it.
 11. Rows the user expanded stay expanded across a ViewList push and a session
     tree push from a second client, and the new view arrives collapsed.
 12. With one 3-cell view, the Views row is on screen on open when it fits: at
     24x80 for `main` and for `alpha, main`, and at 28x80 for
     `alpha, beta, main, zeta`. At 24x80 that tree can scroll Views off on open,
     and `k` to the top row must bring it back.
 13. The clients are alive and no log of either server holds a panic.

Markers are assembled by the shell (`printf 'ZETA:%s' ok`), so the typed line
never contains the string searched for. Run from the repo root:
    python3 tests/pty/session_manager_views_top.py
"""
import glob
import os
import re
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
from pty_harness import (  # noqa: E402
    BIN,
    Tui,
    sm_children,
    sm_compose_view,
    sm_goto,
    sm_open,
    sm_selected,
    sm_tree,
)
from harness import Client  # noqa: E402

RUNDIR = "/tmp/rmx-smvt"
REMOTE_DIR = f"{RUNDIR}-remote"
VIEWPORT_DIR = f"{RUNDIR}-vp"
COLLAPSED = "▶"
EXPANDED = "▼"
FAILS = []
CLIENT = None
REMOTE = None


def fail(msg):
    print(f"  FAIL: {msg}")
    FAILS.append(msg)


def check(cond, msg):
    if not cond:
        fail(msg)
    return bool(cond)


def count(t, needle):
    pat = re.compile(re.escape(needle))
    return sum(len(list(pat.finditer(r))) for r in t.rows_text())


def wait_until(t, pred, timeout=6.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            if pred():
                return True
        except AssertionError:
            pass  # the popup is mid-repaint
        t.pump(0.15)
    try:
        return bool(pred())
    except AssertionError:
        return False


def named(name):
    """A predicate for the row labelled `name`, ignoring its triangle and suffix."""
    pat = re.compile(r"(?:^|[\s▼▶*])" + re.escape(name) + r"(?:\s|\*|$)")
    return lambda row: any(True for _ in pat.finditer(row.text))


def rows_named(t, name):
    return [r for r in sm_tree(t) if named(name)(r)]


def row_named(t, name):
    rows = rows_named(t, name)
    assert len(rows) == 1, f"expected one {name!r} row, got {rows} in {sm_tree(t)}"
    return rows[0]


def order(t):
    return [r.text.strip() for r in sm_tree(t) if not r.blank]


def expand_group(t):
    row = sm_goto(t, named("Views"), "the Views group")
    if COLLAPSED in row.text:
        t.send("l", 0.4)


def close_manager(t):
    t.send(b"\x1b", 0.5)
    if not wait_until(t, lambda: not t.has("Session Manager"), 3.0):
        fail("Esc did not close the session manager")


def start_remote():
    """Seed the second server that the `ssh` shim relays to."""
    global REMOTE
    shutil.rmtree(REMOTE_DIR, ignore_errors=True)
    for s in ("run", "state", "data", "config", "home"):
        os.makedirs(f"{REMOTE_DIR}/{s}", exist_ok=True)
    env = {
        **os.environ,
        "HOME": f"{REMOTE_DIR}/home",
        "XDG_RUNTIME_DIR": f"{REMOTE_DIR}/run",
        "XDG_STATE_HOME": f"{REMOTE_DIR}/state",
        "XDG_DATA_HOME": f"{REMOTE_DIR}/data",
        "XDG_CONFIG_HOME": f"{REMOTE_DIR}/config",
        "SHELL": "/bin/sh",
        "ENV": "/dev/null",
        "TERM": "xterm-256color",
    }
    sock = f"{REMOTE_DIR}/run/remux.sock"
    REMOTE = subprocess.Popen([BIN, "server"], env=env,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        if os.path.exists(sock):
            break
        time.sleep(0.05)
    else:
        raise SystemExit("the remote server's socket never appeared")
    time.sleep(0.3)
    seed = Client(sock)
    seed.hello()
    seed.drain(0.3)
    seed.send({"CreateSession": {"name": "aardvark", "folder": None}})
    seed.drain(0.5)
    seed.close()

    os.makedirs(f"{REMOTE_DIR}/bin", exist_ok=True)
    shim = f"{REMOTE_DIR}/bin/ssh"
    with open(shim, "w") as f:
        f.write(
            "#!/bin/sh\n"
            f"export XDG_RUNTIME_DIR={REMOTE_DIR}/run\n"
            f"export XDG_STATE_HOME={REMOTE_DIR}/state\n"
            f"export XDG_DATA_HOME={REMOTE_DIR}/data\n"
            f"exec {BIN} relay\n"
        )
    os.chmod(shim, 0o755)


def build_tree(t):
    """`archive` > `filed`, `alpha`, and `zeta` whose pane prints ZETA:ok."""
    b = Client(f"{t.rundir}/run/remux.sock")
    b.hello()
    b.drain(0.3)
    for name in ("filed", "alpha", "zeta"):
        b.send({"CreateSession": {"name": name, "folder": None}})
    b.drain(0.5)
    b.send({"Command": {"FolderNew": "archive"}})
    b.send({"Command": {"FolderMoveSession": {"session": "filed", "folder": "archive"}}})
    b.send({"Command": {"TabRenameByIndex": {"session": "zeta", "tab_index": 0,
                                             "name": "zetatab"}}})
    b.drain(0.5)
    b.send({"Attach": {"session_name": "zeta"}})
    b.send({"Resize": {"cols": 80, "rows": 24}})
    b.drain(0.5)
    b.send({"Input": {"data": list(b"clear; printf 'ZETA:%s\\n' ok\n")}})
    b.drain(0.8)
    b.close()


def case_order(t):
    tree = [r for r in sm_tree(t) if not r.blank]
    print(f"[1] tree: {order(t)}")
    local = row_named(t, "local")
    group = row_named(t, "Views")
    below_local = tree[tree.index(next(r for r in tree if r.y == local.y)) + 1]
    check(below_local.y == group.y,
          f"the row under `local` is {below_local.text.strip()!r}, not Views")
    for name in ("archive", "filed", "alpha", "main", "zeta"):
        row = row_named(t, name)
        check(group.y < row.y,
              f"Views (row {group.y}) is not above {name!r} (row {row.y})")
    check(COLLAPSED in group.text, f"Views did not open collapsed: {group!r}")
    check(not rows_named(t, "View 1"), f"a view row shows on open: {order(t)}")
    check(not sm_children(t, group), f"Views opened with children: {order(t)}")


def case_expand(t):
    expand_group(t)
    group = row_named(t, "Views")
    if not check(EXPANDED in group.text, f"`l` did not expand Views: {group!r}"):
        return
    print(f"[3] group open: {order(t)}")
    view = row_named(t, "View 1")
    check(view.depth > group.depth,
          f"View 1 is not nested under Views: group={group!r} view={view!r}")
    check(COLLAPSED in view.text, f"View 1 did not start collapsed: {view!r}")
    check(not sm_children(t, view), f"View 1 shows cells while collapsed: {order(t)}")
    sm_goto(t, named("View 1"), "View 1")
    t.send("l", 0.4)
    print(f"[3] view open: {order(t)}")
    view = row_named(t, "View 1")
    check(EXPANDED in view.text, f"`l` did not expand View 1: {view!r}")
    check(len(sm_children(t, view)) == 1, f"View 1 does not show its cell: {order(t)}")


def case_expansion_survives_reopen(t):
    close_manager(t)
    sm_open(t)
    print(f"[3b] reopened: {order(t)}")
    view = rows_named(t, "View 1")
    if check(view, f"Views did not stay open across a reopen: {order(t)}"):
        check(len(sm_children(t, view[0])) == 1,
              f"View 1 did not stay open across a reopen: {order(t)}")
        sm_goto(t, named("View 1"), "View 1")
    t.send("h", 0.4)
    close_manager(t)
    sm_open(t)
    print(f"[3b] view collapsed, reopened: {order(t)}")
    view = rows_named(t, "View 1")
    check(view and COLLAPSED in view[0].text and not sm_children(t, view[0]),
          f"the collapse of View 1 did not survive a reopen: {order(t)}")
    sm_goto(t, named("Views"), "the Views group")
    t.send("h", 0.4)
    close_manager(t)
    sm_open(t)
    print(f"[3b] group collapsed, reopened: {order(t)}")
    group = row_named(t, "Views")
    check(COLLAPSED in group.text and not rows_named(t, "View 1"),
          f"the collapse of Views did not survive a reopen: {order(t)}")


def case_rename_session_row(t):
    before = {name: len(rows_named(t, name)) for name in ("zeta", "main", "View 1")}
    sm_goto(t, named("alpha"), "the alpha session")
    t.send("s", 0.2)
    t.send("r", 0.5)
    if not check(wait_until(t, lambda: t.has("Rename session 'alpha'"), 3.0),
                 "`sr` on the alpha row did not name alpha in its prompt"):
        t.dump("sr prompt")
    t.send("alphabet", 0.3)
    t.send("\r", 0.8)
    renamed = wait_until(
        t, lambda: rows_named(t, "alphabet") and not rows_named(t, "alpha"), 4.0)
    check(renamed, f"alpha was not renamed to alphabet: {order(t)}")
    after = {name: len(rows_named(t, name)) for name in before}
    check(after == before, f"another row changed: {before} -> {after}")


def case_mark_session_pane(t):
    sm_goto(t, named("zetatab"), "the zeta session's tab")
    t.send("l", 0.4)
    tab = sm_selected(t)
    t.send("j", 0.3)
    pane = sm_selected(t)
    check(pane.depth > tab.depth, f"expected zeta's pane under its tab, got {pane!r}")
    t.send(" ", 0.3)
    check(sm_selected(t).marked, f"Space did not mark {pane!r}")
    t.send("v", 0.2)
    t.send("a", 0.6)
    check(t.has("Add Pane to View"), "`va` did not open the view picker")
    t.send("k", 0.3)      # the picker opens on "New view"; `k` wraps to View 1
    t.send("\r", 1.5)    # an add to an existing view does not enter it
    if not check(wait_until(t, lambda: not t.has("Session Manager"), 3.0),
                 "the manager stayed open after `va`"):
        t.dump("after va")


def case_enter_session_row(t):
    sm_open(t)
    sm_goto(t, named("zeta"), "the zeta session")
    t.send("\r", 1.0)
    check(wait_until(t, lambda: not t.has("Session Manager"), 3.0),
          "Enter on the zeta row did not close the manager")
    t.pump(0.5)
    sm_open(t)
    landed = sm_selected(t)
    check(named("zeta")(landed), f"the manager reopened on {landed!r}, not zeta")


def case_enter_view_row(t):
    close_manager(t)
    if not check(count(t, "ZETA:ok") > 0 and count(t, "MAIN:ok") == 0,
                 "the screen is not the zeta session before entering the view"):
        t.dump("before Enter on the view")
    sm_open(t)
    expand_group(t)
    sm_goto(t, named("View 1"), "View 1")
    t.send("\r", 1.5)
    both = wait_until(t, lambda: not t.has("Session Manager")
                      and count(t, "MAIN:ok") > 0 and count(t, "ZETA:ok") > 0, 5.0)
    if not check(both, "the view does not show main's pane and the pane marked in zeta"):
        t.dump("view")
    t.prefix(b"wq", 1.0)


def case_search(t):
    sm_open(t)
    expand_group(t)
    sm_goto(t, named("View 1"), "View 1")
    t.send("v", 0.2)
    t.send("r", 0.5)
    t.send("alphaview", 0.3)
    t.send("\r", 0.8)
    check(wait_until(t, lambda: rows_named(t, "alphaview"), 4.0),
          f"the view was not renamed: {order(t)}")
    sm_goto(t, named("Views"), "the Views group")
    t.send("h", 0.4)
    close_manager(t)

    sm_open(t)
    check(not rows_named(t, "alphaview"), f"the view shows on open: {order(t)}")
    t.send("/", 0.5)
    t.send("alphaview", 1.0)
    print(f"[8] view-only query: {order(t)}")
    landed = sm_selected(t)
    check(named("alphaview")(landed),
          f"a query for a collapsed view selected {landed!r}")
    t.send(b"\x15", 0.6)  # Ctrl-U clears the query

    t.send("alpha", 1.0)
    print(f"[8] view and session query: {order(t)}")
    group = row_named(t, "Views")
    session = row_named(t, "alphabet")
    check(group.y < session.y,
          f"filtered: Views (row {group.y}) is not above alphabet (row {session.y})")
    check(not rows_named(t, "zeta"), "the filter left zeta in the tree")
    landed = sm_selected(t)
    check(named("alphabet")(landed), f"the query selected {landed!r}, not the session")
    t.send("\t", 0.5)     # Tab: Enter reaches the tree, not the search bar
    t.send("\r", 1.0)
    check(wait_until(t, lambda: not t.has("Session Manager"), 3.0),
          "Enter on the filtered alphabet row did not close the manager")
    t.pump(0.5)
    sm_open(t)
    landed = sm_selected(t)
    check(named("alphabet")(landed), f"the manager reopened on {landed!r}, not alphabet")
    check(not rows_named(t, "alphaview"), f"a search left Views open: {order(t)}")


def case_search_pane_in_view(t):
    sm_goto(t, named("zetatab"), "the zeta session's tab")
    if "▼" not in sm_selected(t).text:
        t.send("l", 0.4)
    t.send("j", 0.3)
    t.send("p", 0.2)
    t.send("r", 0.5)
    t.send("zetapane", 0.3)
    t.send("\r", 0.8)
    check(wait_until(t, lambda: rows_named(t, "zetapane"), 4.0),
          f"the pane was not renamed: {order(t)}")
    close_manager(t)

    sm_open(t)
    t.send("/", 0.5)
    t.send("zetapane", 1.0)
    print(f"[9] filtered tree: {order(t)}")
    landed = sm_selected(t)
    rows = rows_named(t, "zetapane")
    print(f"[9] selected {landed!r} of {rows}")
    check(len(rows) == 2, f"expected the cell and the pane in {order(t)}")
    check(rows and landed.y == rows[-1].y,
          f"the query selected {landed!r}, not the pane's own row in {rows}")
    t.send("\t", 0.5)     # Tab: Enter reaches the tree, not the search bar
    t.send("\r", 1.5)
    check(wait_until(t, lambda: not t.has("Session Manager"), 3.0),
          "Enter on the search result did not close the manager")
    t.pump(0.5)
    in_view = count(t, "MAIN:ok") > 0
    if not check(not in_view and count(t, "ZETA:ok") > 0,
                 "Enter on the search result did not show the zeta session alone"):
        t.dump("after Enter on the pane search")
    if in_view:
        t.prefix(b"wq", 1.0)
    sm_open(t)
    landed = sm_selected(t)
    check(named("zeta")(landed), f"the manager reopened on {landed!r}, not zeta")


def case_remote(t):
    sm_goto(t, lambda r: r.depth == sm_tree(t)[0].depth and named("mini")(r), "remote mini")
    t.send("l", 0.9)
    wait_until(t, lambda: rows_named(t, "aardvark"), 6.0)
    print(f"[10] tree: {order(t)}")
    groups = rows_named(t, "Views")
    if not check(len(groups) == 1, f"expected one Views group, got {groups}"):
        return
    local, remote = row_named(t, "local"), row_named(t, "mini")
    session = rows_named(t, "aardvark")
    check(session, f"mini's session never appeared: {order(t)}")
    check(local.y < groups[0].y < remote.y,
          f"Views is not under local and above mini: {order(t)}")
    if session:
        check(groups[0].y < session[0].y, f"Views is not above aardvark: {order(t)}")


def case_refresh_keeps_expansion(t):
    expand_group(t)
    sm_goto(t, named("alphaview"), "the alphaview view")
    t.send("l", 0.4)
    view = row_named(t, "alphaview")
    cells = len(sm_children(t, view))
    check(cells == 2, f"alphaview did not open on its two cells: {order(t)}")

    b = Client(f"{t.rundir}/run/remux.sock")
    b.hello()
    b.drain(0.3)
    b.send({"ViewCreate": {"name": "later"}})
    b.send({"CreateSession": {"name": "omega", "folder": None}})
    b.drain(0.8)
    b.close()
    arrived = wait_until(t, lambda: rows_named(t, "later") and rows_named(t, "omega"), 5.0)
    print(f"[11] tree after the pushes: {order(t)}")
    if not check(arrived, f"the new view and session never both showed: {order(t)}"):
        return
    group, view = row_named(t, "Views"), row_named(t, "alphaview")
    check(EXPANDED in group.text, f"a refresh collapsed Views: {group!r}")
    check(EXPANDED in view.text, f"a refresh collapsed alphaview: {view!r}")
    check(len(sm_children(t, view)) == cells, f"a refresh hid the cells: {order(t)}")
    later = row_named(t, "later")
    check(COLLAPSED in later.text, f"the new view did not start collapsed: {later!r}")


def views_on_open(t, label):
    sm_open(t)
    t.pump(0.8)
    shown = order(t)
    print(f"[12] {label}: {len(shown)} rows {shown} selected={sm_selected(t)!r}")
    check(rows_named(t, "Views"), f"{label}: Views is off screen on open: {shown}")
    close_manager(t)


def add_sessions(t, *names):
    b = Client(f"{t.rundir}/run/remux.sock")
    b.hello()
    b.drain(0.3)
    for name in names:
        b.send({"CreateSession": {"name": name, "folder": None}})
    b.drain(0.8)
    b.close()


def views_reachable_by_scrolling_up(t, label):
    sm_open(t)
    t.pump(0.8)
    print(f"[12] {label}, on open: {order(t)}")
    for _ in range(12):
        if named("local")(sm_selected(t)):
            break
        t.send("k", 0.2)
    shown = order(t)
    print(f"[12] {label}, at the top: {shown} selected={sm_selected(t)!r}")
    check(named("local")(sm_selected(t)), f"{label}: `k` never reached the top: {shown}")
    check(rows_named(t, "Views"), f"{label}: Views is off screen at the top: {shown}")
    close_manager(t)


def case_viewport():
    t = Tui(VIEWPORT_DIR, cols=120, rows=60).start()
    t.pump(1.5)
    t.prefix(b"pv", 1.2)
    t.prefix(b"pv", 1.2)
    sm_compose_view(t, tab="Tab 1", panes=(0, 1, 2), settle=1.5)
    t.prefix(b"wq", 1.0)
    t.resize(80, 24)
    views_on_open(t, "24x80 main")
    add_sessions(t, "alpha")
    views_on_open(t, "24x80 alpha, main")
    add_sessions(t, "beta", "zeta")
    t.resize(80, 28)
    views_on_open(t, "28x80 alpha, beta, main, zeta")
    t.resize(80, 24)
    views_reachable_by_scrolling_up(t, "24x80 alpha, beta, main, zeta")

    check(t.alive(), "the viewport client exited")
    check("panicked at" not in t.log("client") + t.log("server"), "panic in a viewport log")
    t.kill()


def main():
    global CLIENT
    start_remote()
    t = CLIENT = Tui(
        RUNDIR, cols=120, rows=50,
        config='[remotes.mini]\nssh = "whatever"\nremux_path = "remux"\nauto_connect = true\n',
        extra_env={"PATH": f"{REMOTE_DIR}/bin:" + os.environ.get("PATH", "")},
    ).start()

    t.send("clear; printf 'MAIN:%s\\n' ok\r", 0.4)
    if not wait_until(t, lambda: count(t, "MAIN:ok") > 0):
        t.dump("main marker")
        raise SystemExit("FAIL(setup): the pane never printed MAIN:ok")
    build_tree(t)

    sm_compose_view(t, tab="Tab 1", panes=(0,), settle=1.5)
    t.prefix(b"wq", 1.0)
    check(wait_until(t, lambda: not t.has("Session Manager")), "the manager stayed open")

    sm_open(t)
    case_order(t)

    landed = sm_selected(t)
    print(f"[2] opened on {landed!r}")
    check(named("main")(landed), f"the manager opened on {landed!r}, not main")

    case_expand(t)
    case_expansion_survives_reopen(t)
    case_rename_session_row(t)
    case_mark_session_pane(t)
    case_enter_session_row(t)
    case_enter_view_row(t)
    case_search(t)
    case_search_pane_in_view(t)
    case_remote(t)
    case_refresh_keeps_expansion(t)
    close_manager(t)
    case_viewport()
    finish(t)


def finish(t):
    alive = t.alive()
    logs = t.log("client") + t.log("server")
    panic = "panicked at" in logs
    t.kill()
    stop_remote()
    remote_logs = "".join(open(p).read() for p in glob.glob(f"{REMOTE_DIR}/state/remux/*.log"))
    remote_panic = "panicked at" in remote_logs
    print(f"alive={alive} panic={panic} remote_panic={remote_panic} "
          f"remote_log_bytes={len(remote_logs)}")
    if not alive:
        fail("client exited")
    if panic:
        fail("panic in a log")
    if remote_panic:
        fail("panic in a remote log")
    if not remote_logs:
        fail("the remote server wrote no log, so its panic check saw nothing")
    if FAILS:
        print(f"RESULT: FAIL -> {len(FAILS)} check(s)")
        sys.exit(1)
    print("RESULT: PASS (Views render first under local)")


def stop_remote():
    global REMOTE
    if REMOTE is not None:
        REMOTE.kill()
        REMOTE.wait()
        REMOTE = None


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # a step raised, for example the client died mid-run
        print(f"  FAIL: scenario aborted: {e!r}")
        FAILS.append("scenario aborted")
        if CLIENT is not None:
            finish(CLIENT)
        sys.exit(1)
    finally:
        stop_remote()
