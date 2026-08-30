#!/usr/bin/env python3
"""A focus change pushes the session tree, and a burst cannot outrun the gate.

The `files` sidebar panel follows the focused pane's directory, and the only
thing that tells it the focus moved is a `SessionTree` push. Nothing in the
focus handlers themselves marks the tree dirty -- `handle_command` does it once
at its tail, for EVERY command, on the reasoning that a command which may have
changed structure may have changed what a subscriber's tree shows. That is a
load-bearing coincidence for this feature and nothing names it, so this pins it:
if a future refactor makes that marker conditional on a command actually
reshaping something, `PaneFocus*` stops pushing and the panel silently stops
following the directory.

(This file exists because the opposite was asserted during Phase C: that focus
changes marked nothing dirty and the sessions panel's `*` marker was therefore
stale. That was wrong -- read from the focus handlers rather than from the tail
of `handle_command` -- and the probe that was supposed to confirm it passed
instead, which is what exposed it. Nothing was broken; this is the test that
proves it.)

Also asserted here, because pushing on a user-repeatable action carries a rate
risk: a held-down `Alt+h` must not produce a push per key repeat.
`SESSION_TREE_PUSH_INTERVAL` (100 ms) is slept AFTER every broadcast and
`Notify` stores at most one permit, so a burst of N focus moves costs at most one
push per interval however fast N arrives.

Run: python3 tests/frame/focus_pushes_the_tree.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = "/tmp/rmx-ftree"
PUSH_INTERVAL = 0.100

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def trees(msgs):
    return [m["SessionTree"] for m in msgs if name_of(m) == "SessionTree"]


def focused_of(tree):
    """(active tab id, focused pane id) for the current session, from the tree."""
    groups = list(tree["unfiled"]) + [s for f in tree["folders"] for s in f["sessions"]]
    for sess in groups:
        if not sess["is_current"]:
            continue
        for tab in sess["tabs"]:
            if not tab.get("is_active"):
                continue
            for pane in tab["panes"]:
                if pane["is_focused"]:
                    return tab["id"], pane["id"]
    return None, None


srv = Server(RUNDIR)
try:
    srv.start()
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 100, "rows": 30}})
    cli.send("SubscribeSessionTree")
    cli.drain(1.0)

    # Two panes, so focus has somewhere to go.
    cli.send({"Command": "PaneSplitVertical"})
    got = trees(cli.drain(1.5))
    check(got, "the split pushed a tree (the subscription is live)")
    tab0, before = focused_of(got[-1])
    check(before is not None,
          f"exactly one pane is marked focused in the active tab (got {before})")

    # --- a focus move, on its own, must push -----------------------------
    for cmd in ("PaneFocusLeft", "PaneFocusRight"):
        cli.send({"Command": cmd})
        got = trees(cli.drain(1.5))
        if got:
            _, after = focused_of(got[-1])
            if after != before:
                break
    else:
        got, after = [], before
    check(got, "a focus change pushes a tree at all")
    check(after != before,
          f"the pushed tree moved the focus marker ({before} -> {after})")

    # --- a tab switch must push, and move `is_active` ---------------------
    cli.send({"Command": "TabNew"})
    cli.drain(1.0)
    cli.send({"Command": "TabPrev"})
    got = trees(cli.drain(1.5))
    check(got, "a tab switch pushes a tree")
    tab_back, _ = focused_of(got[-1])
    check(tab_back == tab0,
          f"the pushed tree moved `is_active` back to the first tab "
          f"({tab_back} vs {tab0})")
    cli.send({"Command": "TabNext"})
    cli.drain(1.0)
    cli.send({"Command": "TabPrev"})
    cli.drain(1.0)

    # --- a burst must not outrun the 100 ms gate --------------------------
    # What a held-down Alt+h looks like: focus commands as fast as the socket
    # takes them. The push count must be governed by the interval, not by how
    # many commands arrived.
    burst = 60
    start = time.time()
    for i in range(burst):
        cli.send({"Command": "PaneFocusLeft" if i % 2 else "PaneFocusRight"})
    pushes = len(trees(cli.drain(2.0)))
    elapsed = time.time() - start
    # The whole burst is written before anything is drained, so every change
    # lands within a few milliseconds of the last: the gate can only fire once
    # for the burst, once for whatever was already in flight, and once trailing.
    # A ceiling derived from the DRAIN window (elapsed / interval, ~22 here)
    # would be satisfied by a build that pushed fifteen times, which is the
    # regression this is guarding against; the constant is the real claim.
    ceiling = 4
    check(pushes <= ceiling,
          f"{burst} focus moves in {elapsed:.2f}s produced {pushes} pushes, "
          f"at most {ceiling} — the gate governs the rate, not the key repeat")
    check(pushes >= 1, f"...and at least one push still arrived (got {pushes})")

    check("panicked at" not in srv.log(), "no panic in the server log")
finally:
    srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED:")
    for f in failures:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
