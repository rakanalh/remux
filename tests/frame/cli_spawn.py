#!/usr/bin/env python3
"""The server half of `remux split` / `remux new-tab`: the `CliSpawn` message.

`CliSpawn` is the one pane-creating message whose session is an ARGUMENT rather
than something read off the connection, because its real sender is a command
line that never attaches. This asserts what that costs the server:

1. It creates a pane in the NAMED session's active tab, from a connection that
   is attached to nothing at all.
2. **The requested argv actually runs in it.** A pane-count assertion passes
   just as happily on a split running a plain login shell, which is the trap
   Phase E caught one layer down -- so the marker the command prints is the
   assertion, and the pane count is only the setup.
3. `-h` and `-v` really do produce different geometry, in the direction remux's
   own naming means (`SplitHorizontal` = top/bottom, the opposite of tmux's
   `split-window -h`). The flag mapping is the one genuinely ambiguous decision
   in this feature; an untested mapping is a coin flip.
4. `cwd` is honoured.
5. An unknown session name is an ERROR that NAMES the session, and spawns
   nothing anywhere -- a `$REMUX_SESSION` left over from a rename is the failure
   this will actually hit, and "not found" without the name explains nothing.
6. `NewTab` adds a TAB rather than splitting the focused pane.

Run: python3 tests/frame/cli_spawn.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = "/tmp/rmx-cli"
WORKDIR = f"{RUNDIR}/workdir"

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def rows_text(msg):
    pc = msg["PaneContent"]
    return ["".join(c["c"] for c in row).rstrip() for row in pc["cells"]]


def tree_of(cli):
    """(tabs, panes-per-tab) for session 'main', from ListSessionTree."""
    cli.send("ListSessionTree")
    for msg in cli.drain(1.5):
        if name_of(msg) != "SessionTree":
            continue
        tree = msg["SessionTree"]
        groups = list(tree["unfiled"])
        for f in tree["folders"]:
            groups += f["sessions"]
        for sess in groups:
            if sess["name"] == "main":
                return [[p["id"] for p in tab["panes"]] for tab in sess["tabs"]]
    return []


def all_pane_ids(tabs):
    return [pid for tab in tabs for pid in tab]


def cli_spawn(placement, argv=None, cwd=None, session="main"):
    """Send one CliSpawn on a FRESH unattached connection, as the real CLI does.

    A fresh connection each time is not tidiness: the whole point of the message
    is that its sender has no attached session, and reusing the attached client
    would quietly test a path the CLI never takes.
    """
    caller = Client(srv.sock)
    caller.hello()
    body = {"session": session, "placement": placement}
    if argv is not None:
        body["argv"] = argv
    if cwd is not None:
        body["cwd"] = cwd
    caller.send({"CliSpawn": body})
    reply = None
    for msg in caller.drain(3.0):
        if name_of(msg) in ("CliSpawned", "Error"):
            reply = msg
            break
    caller.close()
    return reply


def pane_content(cli, pane_id, want, cols=80, rows=20, timeout=4.0):
    """Subscribe to pane_id and wait for `want` to appear in its content."""
    cli.send({"SubscribePane": {"pane_id": pane_id, "cols": cols, "rows": rows,
                                "size_demand": False}})
    end = time.time() + timeout
    seen, dims = [], (0, 0)
    while time.time() < end:
        for msg in cli.drain(0.3):
            if name_of(msg) != "PaneContent":
                continue
            if msg["PaneContent"]["pane_id"] != pane_id:
                continue
            seen = rows_text(msg)
            dims = (msg["PaneContent"]["cols"], msg["PaneContent"]["rows"])
            if any(want in r for r in seen):
                return True, seen, dims
    return False, seen, dims


# A command whose ARGUMENTS are the thing under test: without them, `sh` starts
# a perfectly ordinary shell and every pane-count assertion still passes. The
# trailing `exec sh` keeps the pane alive long enough to be subscribed to and
# read -- a command that exits instantly can be reaped before the frame arrives.
def marker_argv(marker):
    return ["/bin/sh", "-c", f"printf '{marker}\\n'; exec /bin/sh"]


srv = Server(RUNDIR)
try:
    srv.start()
    os.makedirs(WORKDIR, exist_ok=True)

    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 100, "rows": 30}})
    cli.drain(0.8)

    before = tree_of(cli)
    check(before == [[before[0][0]]] if before else False,
          f"the session starts with one tab holding one pane (got {before})")

    # --- 1 + 2: a split from an unattached connection, running the argv ----
    reply = cli_spawn("SplitHorizontal", argv=marker_argv("SPLIT-MARKER"))
    check(name_of(reply) == "CliSpawned",
          f"CliSpawn from an unattached connection is answered CliSpawned (got {reply})")
    new_id = reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None

    after = tree_of(cli)
    check(len(after) == 1 and len(after[0]) == 2,
          f"the split landed in the session's active tab (tabs={after})")
    check(new_id in all_pane_ids(after),
          f"the answered pane id is the one in the tree (id={new_id}, tabs={after})")

    ok, seen, dims_h = pane_content(cli, new_id, "SPLIT-MARKER")
    check(ok, f"the requested argv actually RAN in the new pane (rows={seen})")

    # --- 3: -h and -v are different geometry ------------------------------
    # The split above was horizontal (top/bottom), so the new pane keeps roughly
    # the session's full width. A vertical one must not.
    reply_v = cli_spawn("SplitVertical", argv=marker_argv("VSPLIT-MARKER"))
    check(name_of(reply_v) == "CliSpawned", f"a vertical CliSpawn is answered (got {reply_v})")
    v_id = reply_v["CliSpawned"]["pane_id"] if name_of(reply_v) == "CliSpawned" else None
    ok_v, seen_v, dims_v = pane_content(cli, v_id, "VSPLIT-MARKER")
    check(ok_v, f"the vertical split's argv ran too (rows={seen_v})")
    check(dims_h[0] > dims_v[0],
          f"SplitHorizontal keeps the width and SplitVertical halves it "
          f"(horizontal={dims_h}, vertical={dims_v})")

    # --- 4: cwd ------------------------------------------------------------
    # The pane COMPARES its cwd and prints a short verdict, rather than printing
    # the path for the harness to compare. By this point the session holds four
    # panes, so a pane is narrow enough to wrap a long path across two rows --
    # and a wrapped path fails a row-wise substring search while the cwd is
    # perfectly correct.
    reply_c = cli_spawn(
        "SplitHorizontal", cwd=WORKDIR,
        argv=["/bin/sh", "-c",
              f'[ "$(pwd)" = "{WORKDIR}" ] && printf "CWD-OK\\n" || printf "CWD-BAD\\n"; exec /bin/sh'])
    check(name_of(reply_c) == "CliSpawned", f"a CliSpawn with a cwd is answered (got {reply_c})")
    c_id = reply_c["CliSpawned"]["pane_id"] if name_of(reply_c) == "CliSpawned" else None
    ok_c, seen_c, _ = pane_content(cli, c_id, "CWD-OK")
    check(ok_c, f"the pane started in the requested cwd (rows={seen_c})")

    # --- 5: an unknown session errors and spawns nothing -------------------
    tabs_before_bad = tree_of(cli)
    bad = cli_spawn("SplitHorizontal", argv=marker_argv("NEVER-RUNS"),
                    session="renamed-away")
    check(name_of(bad) == "Error", f"an unknown session name is an Error (got {bad})")
    msg = bad["Error"]["message"] if name_of(bad) == "Error" else ""
    check("renamed-away" in msg,
          f"the error NAMES the session it could not find (message={msg!r})")
    check(tree_of(cli) == tabs_before_bad,
          "a failed CliSpawn creates no pane anywhere")

    # --- 6: NewTab adds a tab rather than splitting ------------------------
    tabs_before_tab = tree_of(cli)
    reply_t = cli_spawn("NewTab", argv=marker_argv("TAB-MARKER"))
    check(name_of(reply_t) == "CliSpawned", f"a NewTab CliSpawn is answered (got {reply_t})")
    t_id = reply_t["CliSpawned"]["pane_id"] if name_of(reply_t) == "CliSpawned" else None
    tabs_after_tab = tree_of(cli)
    check(len(tabs_after_tab) == len(tabs_before_tab) + 1,
          f"NewTab added a tab ({len(tabs_before_tab)} -> {len(tabs_after_tab)})")
    check(tabs_after_tab and tabs_after_tab[-1] == [t_id],
          f"the new tab holds exactly the new pane (tabs={tabs_after_tab}, id={t_id})")
    ok_t, seen_t, _ = pane_content(cli, t_id, "TAB-MARKER")
    check(ok_t, f"the new tab's pane runs the requested argv (rows={seen_t})")

    # --- empty argv still gives a shell ------------------------------------
    reply_s = cli_spawn("SplitHorizontal", argv=[])
    check(name_of(reply_s) == "CliSpawned",
          f"an empty argv is accepted and means the login shell (got {reply_s})")
    s_id = reply_s["CliSpawned"]["pane_id"] if name_of(reply_s) == "CliSpawned" else None
    ok_s, seen_s, _ = pane_content(
        cli, s_id, "SHELL-IS-ALIVE", timeout=4.0)
    if s_id is not None:
        cli.send({"InputToPane": {"pane_id": s_id,
                                  "data": list(b"echo SHELL-IS-ALIVE\n")}})
        ok_s, seen_s, _ = pane_content(cli, s_id, "SHELL-IS-ALIVE")
    check(ok_s, f"the shell in an empty-argv pane responds to input (rows={seen_s})")

    log = srv.log()
    check("panicked at" not in log, "no panic in the server log")
finally:
    srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED:")
    for f in failures:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
