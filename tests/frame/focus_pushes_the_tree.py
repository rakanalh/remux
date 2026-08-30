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


def active_tab_id_by_request(cli):
    """Ask for the tree outright. Independent of whether a push happened, which
    is the whole point: it separates "the server did not switch" from "the
    server switched and told nobody"."""
    cli.send("ListSessionTree")
    for msg in cli.drain(1.5):
        if name_of(msg) == "SessionTree":
            tab, _ = focused_of(msg["SessionTree"])
            if tab is not None:
                return tab
    return None


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
    # Land back on the FIRST tab, which is the two-pane one. Load-bearing for
    # the burst below: the alternating `PaneFocusLeft`/`Right` only move focus
    # because that tab has two panes. Edit this sequence and the burst silently
    # becomes 60 no-ops while the test keeps passing.
    cli.send({"Command": "TabNext"})
    cli.drain(1.0)
    cli.send({"Command": "TabPrev"})
    cli.drain(1.0)

    # --- the MOUSE route pushes too ---------------------------------------
    # Everything above sends `Command` messages, so it pins the KEYBOARD route
    # only. `handle_mouse_click` arrives via `ClientMessage::MouseClick` and
    # never reaches `handle_command`'s tail marker, so the tab-bar click is a
    # separate route needing its own coverage -- and it was the one route left
    # uncovered when `ServerState::goto_tab`'s own marker was removed.
    #
    # The click goes on the status row, where the tab strip lives. Nothing else
    # is hit-testable there: a miss is `ClickTarget::None` and does nothing, so
    # scanning for the tab's column cannot disturb anything.
    bar_row = 29  # rows - 1, for the 100x30 negotiated above
    here = active_tab_id_by_request(cli)
    other_x = None
    for x in range(0, 40):
        cli.send({"MouseClick": {"x": x, "y": bar_row, "pane_id": None, "release": False}})
        cli.drain(0.25)
        if active_tab_id_by_request(cli) != here:
            other_x = x
            break
    check(other_x is not None,
          "a click on the status row can reach a tab (the scan found one)")

    if other_x is not None:
        # Back to a known tab, then drain everything so the only tree that can
        # arrive below is one this click caused.
        cli.send({"Command": {"TabGoto": 0}})
        cli.drain(1.2)
        before_tab = active_tab_id_by_request(cli)
        cli.drain(1.2)

        cli.send({"MouseClick": {"x": other_x, "y": bar_row, "pane_id": None, "release": False}})
        pushed = trees(cli.drain(2.0))

        # Independent of the push: did the server actually switch? Without this,
        # a red result could not tell "no push" from "no switch".
        after_tab = active_tab_id_by_request(cli)
        check(after_tab != before_tab,
              f"the click really switched tab server-side ({before_tab} -> {after_tab})")
        check(pushed,
              "a tab-bar CLICK pushes a tree, as the keyboard route does")
        if pushed:
            clicked_tab, _ = focused_of(pushed[-1])
            check(clicked_tab == after_tab,
                  f"and the pushed tree carries the new active tab "
                  f"({clicked_tab} vs {after_tab})")

    # --- a burst must not outrun the 100 ms gate --------------------------
    # What a held-down Alt+h looks like: focus commands as fast as the socket
    # takes them. The push count must be governed by the interval, not by how
    # many commands arrived.
    # The bound is measured against how long the server took to PROCESS the
    # burst, not how fast it was written. Writing is the wrong quantity: each
    # command runs a `broadcast_full_render` over a 100x30 grid plus JSON
    # serialisation, in a DEBUG build, so on a loaded machine the burst can take
    # far longer to work through than to send -- and a fixed constant would then
    # go red while the gate was doing its job perfectly.
    #
    # Processing time is measured by a round-trip that can only be answered
    # after every queued command has been handled, since the server processes
    # one connection's messages in order.
    burst = 60
    start = time.time()
    for i in range(burst):
        cli.send({"Command": "PaneFocusLeft" if i % 2 else "PaneFocusRight"})
    # A barrier that is NOT a tree, so every `SessionTree` counted below is
    # unambiguously a push. The server handles one connection's messages in
    # order, so this reply cannot arrive until the whole burst has been worked
    # through -- which makes its arrival the measurement of processing time.
    cli.send("RequestScrollbackInfo")
    pushes, processing = 0, None
    while processing is None and time.time() - start < 30:
        for msg in cli.drain(0.3):
            if name_of(msg) == "SessionTree":
                pushes += 1
            elif name_of(msg) == "ScrollbackInfo":
                processing = time.time() - start
                break
    check(processing is not None, "the burst was processed within 30s")
    processing = processing or 30.0
    # Whatever trails the last change.
    pushes += len(trees(cli.drain(1.5)))
    # One push per interval the PROCESSING spanned, plus one already in flight
    # when the burst began and one trailing the last change. The `+ 2` is the
    # real claim; the interval term is only what keeps a slow machine from
    # failing a gate that is working.
    ceiling = int(processing / PUSH_INTERVAL) + 2
    check(pushes <= ceiling,
          f"{burst} focus moves took {processing:.2f}s to process and produced "
          f"{pushes} pushes, within the {ceiling} the {int(PUSH_INTERVAL * 1000)}ms "
          f"gate allows -- the gate governs the rate, not the key repeat")
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
