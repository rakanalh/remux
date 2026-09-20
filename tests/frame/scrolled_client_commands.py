"""A scrolled client must still be repainted by the commands that reshape its screen.

`broadcast_full_render` deliberately skips clients whose `scroll_offset != 0` --
that filter exists so another pane's PTY output cannot yank a scrolled reader to
the tail, and it must stay. The side effect is that while a client is scrolled
back, focus/layout/tab commands change the server's state and produce NO frame
at all: the active-pane border stays on the old pane and the session reads as
frozen until the user types (`ClientMessage::Input` already snaps to the tail).

Each case runs against a FRESH server -- `TabNew` moves focus to a pane with no
scrollback and would poison a shared one -- and is asserted twice:

  * at offset 0 (the control), which must pass before and after the fix, and
  * while scrolled, which is the bug.

Run: python3 tests/frame/scrolled_client_commands.py
"""
import os, re, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of, only

RUNDIR_BASE = "/tmp/rmx-scrollcmd"
COLS, ROWS = 100, 30
RENDERS = ("FullRender", "RenderDiff", "ScrollRender")
SCROLL_BY = 10


class Attached:
    """One throwaway server with one attached client, plus render bookkeeping."""

    def __init__(self, rundir):
        self.srv = Server(rundir).start()
        self.c = Client(self.srv.sock)
        self.c.hello()
        self.c.send({"CreateSession": {"name": "main", "folder": None}})
        self.c.send({"Attach": {"session_name": "main"}})
        self.c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        self.fpr = None
        self.settle()

    def pump(self, t=0.6):
        """Drain for t seconds; return (all messages, render tuples).

        A `RenderDiff` may repeat the focused pane rect or omit it, so the last
        one SEEN is carried forward rather than read off the final frame.
        """
        msgs = self.c.drain(t)
        out = []
        for m in msgs:
            n = name_of(m)
            if n in RENDERS:
                body = only(m, n)
                self.fpr = body.get("focused_pane_rect") or self.fpr
                out.append((n, body.get("scroll_offset", 0), self.fpr))
        return msgs, out

    def settle(self, quiet=0.4, rounds=20):
        """Drain until a whole window arrives empty.

        Shell output from the history loop keeps trickling in; a case that sent
        its command into that trickle could not tell the command's frame from
        the echo of the previous step.
        """
        for _ in range(rounds):
            msgs, _ = self.pump(quiet)
            if not msgs:
                return
        raise SystemExit("server never went quiet")

    def command(self, cmd):
        self.c.send({"Command": cmd})

    def close(self):
        log = self.srv.log()
        self.c.close()
        self.srv.kill()
        assert "panicked" not in log, "server panicked:\n" + log


def fill_history(s):
    """Put >1 screen of history into whatever pane currently takes input.

    The marker is assembled by the shell (`printf` + the loop variable), so a
    shell echoing the typed command line cannot produce it.
    """
    s.c.send({"Input": {"data": list(
        b"for i in $(seq 1 200); do printf 'LINE:%s\\n' $i; done\n")}})
    s.settle()


def build(rundir, pre_focus=None):
    """Two side-by-side panes; the focused one holds >1 screen of history."""
    s = Attached(rundir)
    s.command("PaneSplitVertical")
    s.settle()
    if pre_focus:
        # Park focus on the pane the case will scroll, so the command under
        # test is guaranteed to MOVE focus rather than no-op at an edge.
        s.command(pre_focus)
        s.settle()
    fill_history(s)
    return s


def scroll_back(s):
    """Scroll into history and PROVE it took, by reading `scroll_offset`.

    Counting frames would not do: late PTY output produces frames carrying
    `scroll_offset: 0`, so "a frame arrived" is satisfied by a client that
    never scrolled and every later assertion would be vacuous.
    """
    s.c.send({"ScrollDelta": {"delta": SCROLL_BY}})
    _, r = s.pump(0.8)
    assert r, "the scroll produced no render at all -- 'scrolled' would be a lie"
    assert r[-1][1] == SCROLL_BY, \
        f"scroll_offset is {r[-1][1]}, expected {SCROLL_BY} -- client is not scrolled"


def run_case(cmd, scrolled):
    label = f"{cmd}{'/scrolled' if scrolled else '/live'}"
    pre = {"PaneFocusRight": "PaneFocusLeft",
           "PaneFocusLeft": "PaneFocusRight"}.get(cmd)
    slug = cmd.lower() + ("s" if scrolled else "l")
    s = build(f"{RUNDIR_BASE}/{slug}", pre)
    try:
        if scrolled:
            scroll_back(s)
        before = s.fpr
        s.command(cmd)
        _, r = s.pump(1.2)
        assert r, f"{label}: SILENT -- the command produced no frame for this client"
        assert r[-1][1] == 0, \
            f"{label}: client left at scroll_offset={r[-1][1]}, not returned to the live tail"
        if pre:
            # The bug the user reported is the highlight not moving, which a
            # bare frame-count check would pass on.
            assert before and s.fpr and before["x"] != s.fpr["x"], \
                f"{label}: focused_pane_rect did not move ({before} -> {s.fpr})"
    finally:
        s.close()
    print(f"PASS {label}")


def run_negative():
    """`EnterVisualMode` must NOT snap: scrolling back is the point of copy mode.

    The server has no arm for this command, so it renders nothing either way --
    a frame count cannot tell "not snapped" from "filtered out". One more line
    of scroll can: a still-scrolled client reports SCROLL_BY+1, a snapped one 1.
    """
    s = build(f"{RUNDIR_BASE}/negative")
    try:
        scroll_back(s)
        s.command("EnterVisualMode")
        s.pump(0.5)
        s.c.send({"ScrollDelta": {"delta": 1}})
        _, r = s.pump(0.8)
        assert r, "negative/EnterVisualMode: the follow-up scroll produced no render"
        assert r[-1][1] == SCROLL_BY + 1, (
            f"negative/EnterVisualMode: scroll_offset={r[-1][1]}, expected "
            f"{SCROLL_BY + 1} -- the client was snapped to the tail by a "
            "command that must never snap")
    finally:
        s.close()
    print("PASS EnterVisualMode/scrolled (not snapped)")


def run_popup_negative():
    """A command the popup guard REFUSES must not cost the user their scrollback.

    While the popup terminal is up it is the input target, and the guard in
    `handle_command` turns every layout command into a no-op. Snapping for one
    of those would yank a scrolled reader to the tail for a command that then
    does nothing, so the snap sits below the guard.

    The popup is opened BEFORE the scroll on purpose: `PopupToggle` is itself
    classified `returns_to_live_tail`, so opening it while scrolled legitimately
    snaps, and a case built the other way round would test nothing.
    """
    s = build(f"{RUNDIR_BASE}/popup")
    try:
        s.command("PopupToggle")
        s.settle()
        assert "spawned popup pane_id" in s.srv.log(), \
            "popup/PaneFocusRight: the popup never opened -- the case would be vacuous"
        # The popup pane is now the input target, so this history (and the
        # scroll below) belong to the popup, which is where the guard applies.
        fill_history(s)
        scroll_back(s)

        mark = len(s.srv.log())
        s.command("PaneFocusRight")
        s.pump(0.8)
        assert "is a no-op while the popup is open" in s.srv.log()[mark:], \
            "popup/PaneFocusRight: the guard did not refuse the command"

        s.c.send({"ScrollDelta": {"delta": 1}})
        _, r = s.pump(0.8)
        assert r, "popup/PaneFocusRight: the follow-up scroll produced no render"
        assert r[-1][1] == SCROLL_BY + 1, (
            f"popup/PaneFocusRight: scroll_offset={r[-1][1]}, expected "
            f"{SCROLL_BY + 1} -- the client was snapped to the tail for a "
            "command the popup guard then refused")
    finally:
        s.close()
    print("PASS PaneFocusRight/scrolled+popup (refused, so not snapped)")


def session_tree(s):
    """The live session tree, keyed by session name. Wire evidence, not a log."""
    s.c.send("ListSessionTree")
    end = time.time() + 3.0
    while time.time() < end:
        for m in s.c.drain(0.3):
            if name_of(m) == "SessionTree":
                body = only(m, "SessionTree")
                out = {e["name"]: e for e in body.get("unfiled", [])}
                for f in body.get("folders", []):
                    for e in f.get("sessions", []):
                        out[e["name"]] = e
                return out
    raise SystemExit("no SessionTree came back")


def pane_count(tree, name):
    return sum(len(t["panes"]) for t in tree[name]["tabs"])


def run_cross_session_negative():
    """An explicit-target command against ANOTHER session must not snap.

    `PaneNewInTab` and its three siblings are the session manager's: they name
    the session they act on, and it is usually not the requester's. Adding a
    pane to a session this client is not looking at changes nothing on its
    screen, so taking its place in the scrollback away is the popup mistake in
    a different costume.

    Vacuity guard: the session tree is read before and after and session B's
    pane count must have gone up. That is wire evidence the command really ran
    against B -- a command the server merely ignored would leave the client
    unsnapped too, and the case would pass for the wrong reason.
    """
    s = build(f"{RUNDIR_BASE}/xsession")
    try:
        s.c.send({"CreateSession": {"name": "other", "folder": None}})
        s.settle()
        before = session_tree(s)
        assert "other" in before, "session B was never created"
        assert before["main"]["client_count"] == 1, \
            "the client is no longer attached to session A -- the case would be vacuous"
        n_before = pane_count(before, "other")

        scroll_back(s)
        s.command({"PaneNewInTab": {"session": "other", "tab_index": 0}})
        s.pump(1.0)

        after = session_tree(s)
        assert pane_count(after, "other") == n_before + 1, (
            f"cross-session: session B's pane count went {n_before} -> "
            f"{pane_count(after, 'other')} -- the command did not run, so the "
            "case proves nothing")
        assert pane_count(after, "main") == pane_count(before, "main"), \
            "cross-session: the command touched session A after all"

        s.c.send({"ScrollDelta": {"delta": 1}})
        _, r = s.pump(0.8)
        assert r, "cross-session: the follow-up scroll produced no render"
        assert r[-1][1] == SCROLL_BY + 1, (
            f"cross-session: scroll_offset={r[-1][1]}, expected {SCROLL_BY + 1} "
            "-- the client was snapped for a command against another session")
    finally:
        s.close()
    print("PASS PaneNewInTab/scrolled+other-session (not snapped)")


def run_own_session_positive():
    """The same command against the requester's OWN session must still snap.

    Without this, "false when the target differs" and "false for these four
    always" are indistinguishable, and the cross-session negative above would
    pass on an implementation that simply never snaps them.
    """
    s = build(f"{RUNDIR_BASE}/ownsession")
    try:
        tree = session_tree(s)
        panes = [p for t in tree["main"]["tabs"] for p in t["panes"]]
        assert len(panes) == 2, f"expected 2 panes in session A, got {len(panes)}"
        # Close the pane that is NOT holding the scrollback, so the assertion is
        # about the snap rather than about the scrolled pane disappearing.
        victim = next(p for p in panes if not p["is_focused"])

        scroll_back(s)
        s.command({"PaneCloseById": {"session": "main", "pane_id": victim["id"]}})
        _, r = s.pump(1.2)
        assert r, ("own-session: SILENT -- PaneCloseById against the attached "
                   "session produced no frame for this client")
        assert r[-1][1] == 0, \
            f"own-session: client left at scroll_offset={r[-1][1]}, not at the live tail"

        after = session_tree(s)
        assert pane_count(after, "main") == 1, (
            f"own-session: pane count is {pane_count(after, 'main')}, expected 1 "
            "-- the command did not run, so the frame proves nothing")
    finally:
        s.close()
    print("PASS PaneCloseById/scrolled+own-session (snapped)")


POPUP_RESIZE = re.compile(r"popup resize -> \((\d+), (\d+)\)")


def run_popup_resize_positive():
    """The popup guard's REROUTE arm renders, so it must snap too.

    `Resize*` is the one command that gets past the guard and is handled inside
    it: the arm resizes the popup, broadcasts and returns, never reaching the
    snap below `match cmd`. A scrolled client is filtered out of that broadcast,
    so without a snap in the arm the popup resizes and the user sees nothing --
    the reported defect, reached by the only route the guard lets through.

    Vacuity guard: `resize_popup` logs the size it produced, so the case
    compares the size logged before the scroll with the one logged after. A
    frame that arrived without the reroute having run would leave only one such
    line, and an arm that ran but clamped would log the same size twice --
    neither passes. Checking for a frame alone would not distinguish them.
    """
    s = build(f"{RUNDIR_BASE}/popupresize")
    try:
        s.command("PopupToggle")
        s.settle()
        assert "spawned popup pane_id" in s.srv.log(), \
            "popup/ResizeRight: the popup never opened -- the case would be vacuous"
        # The popup pane is the input target, so this gives it the scrollback
        # the scroll below needs.
        fill_history(s)

        s.command({"ResizeRight": 2})
        s.pump(0.8)
        sizes = POPUP_RESIZE.findall(s.srv.log())
        assert len(sizes) == 1, \
            f"popup/ResizeRight: expected one popup resize before the scroll, got {sizes}"

        scroll_back(s)
        s.command({"ResizeRight": 2})
        _, r = s.pump(1.2)

        sizes = POPUP_RESIZE.findall(s.srv.log())
        assert len(sizes) == 2, (
            f"popup/ResizeRight: the reroute arm did not run while scrolled "
            f"(popup resize lines: {sizes})")
        assert sizes[1] != sizes[0], (
            f"popup/ResizeRight: the popup did not actually resize "
            f"({sizes[0]} -> {sizes[1]}) -- the case proves nothing")
        assert r, ("popup/ResizeRight: SILENT -- the popup resized and no frame "
                   "reached this client")
        assert r[-1][1] == 0, \
            f"popup/ResizeRight: client left at scroll_offset={r[-1][1]}, not at the live tail"
    finally:
        s.close()
    print("PASS ResizeRight/scrolled+popup (rerouted, so snapped)")


def tab_count(tree, name):
    return len(tree[name]["tabs"])


def run_tab_new_other_session_negative():
    """`TabNewInSession` against ANOTHER session must not snap."""
    s = build(f"{RUNDIR_BASE}/tnis_other")
    try:
        s.c.send({"CreateSession": {"name": "other", "folder": None}})
        s.settle()
        before = session_tree(s)
        assert before["main"]["client_count"] == 1, \
            "the client is no longer attached to session A -- the case would be vacuous"
        n_before = tab_count(before, "other")

        scroll_back(s)
        s.command({"TabNewInSession": {"session": "other"}})
        s.pump(1.0)

        after = session_tree(s)
        assert tab_count(after, "other") == n_before + 1, (
            f"TabNewInSession/other: session B's tab count went {n_before} -> "
            f"{tab_count(after, 'other')} -- the command did not run")
        assert tab_count(after, "main") == tab_count(before, "main"), \
            "TabNewInSession/other: the command touched session A after all"

        s.c.send({"ScrollDelta": {"delta": 1}})
        _, r = s.pump(0.8)
        assert r, "TabNewInSession/other: the follow-up scroll produced no render"
        assert r[-1][1] == SCROLL_BY + 1, (
            f"TabNewInSession/other: scroll_offset={r[-1][1]}, expected "
            f"{SCROLL_BY + 1} -- snapped for a command against another session")
    finally:
        s.close()
    print("PASS TabNewInSession/scrolled+other-session (not snapped)")


def run_tab_new_own_session_positive():
    """`TabNewInSession` against the OWN session must snap.

    `create_tab_in_session` goes through `create_tab`, which makes the new tab
    ACTIVE -- so the requester's whole screen is replaced. Not snapping here is
    the frozen-session report in its purest form.
    """
    s = build(f"{RUNDIR_BASE}/tnis_own")
    try:
        before = session_tree(s)
        n_before = tab_count(before, "main")

        scroll_back(s)
        s.command({"TabNewInSession": {"session": "main"}})
        _, r = s.pump(1.2)

        assert r, ("TabNewInSession/own: SILENT -- a new ACTIVE tab and no frame "
                   "reached this client")
        assert r[-1][1] == 0, \
            f"TabNewInSession/own: client left at scroll_offset={r[-1][1]}, not at the live tail"

        after = session_tree(s)
        assert tab_count(after, "main") == n_before + 1, (
            f"TabNewInSession/own: tab count is {tab_count(after, 'main')}, "
            f"expected {n_before + 1} -- the command did not run")
    finally:
        s.close()
    print("PASS TabNewInSession/scrolled+own-session (snapped)")


COMMANDS = ["PaneFocusRight", "PaneFocusLeft", "PaneSplitHorizontal",
            "LayoutNext", "PaneToggleZoom", "TabNew"]


def main():
    failures = []
    for scrolled in (False, True):
        for cmd in COMMANDS:
            try:
                run_case(cmd, scrolled)
            except AssertionError as e:
                failures.append(str(e))
                print(f"FAIL {e}")
    for probe in (run_negative, run_popup_negative, run_popup_resize_positive,
                  run_cross_session_negative, run_own_session_positive,
                  run_tab_new_other_session_negative,
                  run_tab_new_own_session_positive):
        try:
            probe()
        except AssertionError as e:
            failures.append(str(e))
            print(f"FAIL {e}")
    if failures:
        print(f"\n{len(failures)} failure(s)")
        return 1
    print("\nALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
