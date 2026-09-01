#!/usr/bin/env python3
"""The session manager's search bar: filtering, focus, and key routing.

The overlay grew a search row under its top border. It is a plain
`query: String` + `search_focused: bool` on `SessionManagerState` -- deliberately
NOT a `SubMode`, because the query has to survive while the delete / create /
rename sub-modes are up. Only a real PTY can prove the routing: whether a
keystroke lands in the query or fires a command depends on where focus is, and
that is invisible to the frame-level protocol harness.

The tree under test is built by a SECOND client speaking the wire protocol
directly (folder `warehouse` > session `alphasession` > tab `zebratab`), so the
harness never has to drive the manager's own create flows to get the names it
filters on.

Asserted here:
  1. Opening the manager shows the search row UNfocused (no block cursor), with
     the highlight already on the session the client is attached to -- and `j`
     then MOVES that highlight instead of typing a `j` into the query.
  2. `/` focuses the search bar; typing a TAB name filters the tree; the tab's
     session AND folder stay.
  3. Tab unfocuses -- the cursor block goes, and `j` then MOVES the selection
     instead of typing a `j`.
  4. With tree focus and a non-empty query, `d` still opens the delete confirm
     (the proof that the query is not a SubMode).
  5. `/` refocuses the search bar, and `q` typed there does NOT close the popup.
  6. A query matching nothing does not crash, and clearing it brings the tree
     back.
  7. A query matching a SESSION puts the selection ON that session, not on the
     always-visible `local` server row -- Tab then Enter switches to it.
  8. A query matching a session INSIDE A FOLDER selects the session, not the
     folder that is only visible as its ancestor -- Enter switches rather than
     toggling the folder open.
  9. Reopening the manager after switching sessions highlights the session
     switched TO -- the snap follows the client, it is not a fixed row.
 10. The client is still alive and the log has no panic.

Run from the repo root:  python3 tests/pty/session_manager_search.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
from pty_harness import Tui  # noqa: E402
from harness import Client  # noqa: E402

# The session the client auto-creates and attaches to on startup (`main.rs`).
# It is the "current session" the overlay must open highlighting.
CURRENT = "main"
FOLDER = "warehouse"
SESSION = "alphasession"
OTHER = "omegasession"
TAB = "zebratab"
CURSOR = "█"


def sel_row(t, search_y):
    """The index of the highlighted session-manager row.

    The selection is painted by swapping fg/bg (not an SGR reverse), so the only
    way to see it from outside is to look for the row whose background differs
    from the rest of the popup. The search row is the reference: it is always
    plain popup background.
    """
    # Only look inside the popup: everything outside it is a different surface.
    # The popup's own corners are on the title row (the outer pane border box
    # only has corners on the first/last screen rows), so they bound the scan.
    title = t.rows_text()[search_y - 1]
    x0, x1 = title.index("╭") + 1, title.index("╮")
    ref = [t.screen.buffer[search_y][x].bg for x in range(x0, x1)]
    best, best_n = -1, 0
    for y in range(search_y + 1, t.rows):
        # Walk down the popup only: `│` rows are content, `├` rows are the
        # separators, and `╰` is the bottom border (stop there).
        edge = t.rows_text()[y][x0 - 1]
        if edge == "╰":
            break
        if edge != "│":
            continue
        n = sum(1 for i, x in enumerate(range(x0, x1))
                if t.screen.buffer[y][x].bg != ref[i])
        if n > best_n:
            best, best_n = y, n
    # A selected row spans the popup's full inner width; anything less is noise.
    return best if best_n > 20 else -1


def main():
    t = Tui("/tmp/rmxsms", cols=120, rows=40).start()
    fails = []
    b = None

    def fail(msg, label):
        fails.append(msg)
        t.dump(label)

    try:
        # --- build a tree with distinctive names at folder/session/tab depth ---
        b = Client(f"{t.rundir}/run/remux.sock")
        b.hello()
        b.drain(0.3)
        b.send({"CreateSession": {"name": SESSION, "folder": None}})
        b.send({"CreateSession": {"name": OTHER, "folder": None}})
        b.drain(0.5)
        b.send({"Command": {"FolderNew": FOLDER}})
        b.send({"Command": {"FolderMoveSession": {"session": SESSION, "folder": FOLDER}}})
        b.send({"Command": {"TabRenameByIndex": {"session": SESSION, "tab_index": 0,
                                                 "name": TAB}}})
        b.drain(0.8)

        # --- 1. the manager opens on the TREE, highlighting the current session -
        t.prefix(b"xm", 1.2)
        t.pump(0.8)
        if not t.has("Session Manager"):
            print("ABORT: the session manager never opened")
            t.dump("no session manager")
            t.kill()
            sys.exit(1)
        title_y = t.find_row("Session Manager")
        search_y = title_y + 1
        search_row = t.rows_text()[search_y]
        if "/" not in search_row:
            fail(f"no search row under the title (row {search_y}: {search_row!r})", "1: no search row")
        if CURSOR in search_row:
            fail(f"the overlay opened focused on the search bar (row {search_y}: "
                 f"{search_row!r})", "1: search focused on open")
        if not t.has(TAB) or not t.has(FOLDER) or not t.has(OTHER):
            fail("the unfiltered tree is missing rows it should show", "1: tree incomplete")
        # The highlight must be on the CURRENT session, not on row 0 (the always
        # -present `local` server row, which is where the selection used to sit).
        sel_y = sel_row(t, search_y)
        if sel_y == -1:
            fail("no row is highlighted at all", "1: no selection")
        elif CURRENT not in t.rows_text()[sel_y]:
            fail(f"the highlight is not on the current session {CURRENT!r} "
                 f"(row {sel_y}: {t.rows_text()[sel_y].rstrip()!r})",
                 "1: not on the current session")
        if not fails:
            print(f"1. OK: manager open on the tree, highlighting {CURRENT!r}")

        # --- 1b. `j` navigates; it is not typed into the query ----------------
        before = len(fails)
        sel_before = sel_row(t, search_y)
        t.send(b"j", 0.8)
        sel_after = sel_row(t, search_y)
        if sel_before == -1 or sel_after == -1:
            fail(f"could not locate the highlighted row (before={sel_before} "
                 f"after={sel_after})", "1b: no selection")
        elif sel_after == sel_before:
            fail(f"`j` did not move the highlight (stayed at row {sel_before}) "
                 "-- it was typed into the search bar", "1b: j did not move")
        if "(search)" not in t.rows_text()[search_y]:
            fail(f"`j` landed in the query: {t.rows_text()[search_y]!r}",
                 "1b: j typed into the query")
        if len(fails) == before:
            print("1b. OK: `j` moves the highlight and does not reach the query")

        # --- 2. `/` focuses the search bar; a TAB name filters ----------------
        t.send(b"/", 0.8)
        if CURSOR not in t.rows_text()[search_y]:
            fail("`/` did not focus the search bar", "2: no focus")
        t.send(TAB.encode(), 1.0)
        if not t.has(TAB):
            fail(f"the matching tab {TAB!r} is not shown", "2: tab missing")
        if not t.has(SESSION):
            fail(f"the matching tab's session {SESSION!r} was filtered out", "2: session missing")
        if not t.has(FOLDER):
            fail(f"the matching tab's folder {FOLDER!r} was filtered out", "2: folder missing")
        if t.has(OTHER):
            fail(f"the non-matching session {OTHER!r} is still shown", "2: not filtered")
        if not t.has("local"):
            fail("the server row was filtered out (it must always show)", "2: server row gone")
        if not fails:
            print(f"2. OK: `/` focused the search bar and {TAB!r} filtered the "
                  "tree to the match + its ancestors")

        # --- 3. Tab hands focus to the tree; `j` then navigates ---------------
        before = len(fails)
        t.send(b"\t", 0.8)
        search_row = t.rows_text()[search_y]
        if CURSOR in search_row:
            fail("the cursor block is still drawn after Tab", "3: still focused")
        if TAB not in search_row:
            fail(f"the query vanished from the search row: {search_row!r}", "3: query lost")
        sel_before = sel_row(t, search_y)
        t.send(b"j", 0.8)
        sel_after = sel_row(t, search_y)
        if sel_before == -1 or sel_after == -1:
            fail(f"could not locate the highlighted row (before={sel_before} "
                 f"after={sel_after})", "3: no selection")
        elif sel_after == sel_before:
            fail(f"`j` did not move the selection (stayed at row {sel_before})", "3: j did not move")
        if t.rows_text()[search_y].count("j") > TAB.count("j"):
            fail(f"`j` was typed into the query: {t.rows_text()[search_y]!r}", "3: j typed")
        if len(fails) == before:
            print("3. OK: Tab unfocused the search bar; `j` moves the selection")

        # --- 4. with a query active, `d` still opens the delete confirm -------
        before = len(fails)
        # Land on the session row so the delete prompt has something to name.
        for _ in range(6):
            y = sel_row(t, search_y)
            if y != -1 and SESSION in t.rows_text()[y]:
                break
            t.send(b"j", 0.5)
        t.send(b"d", 0.8)
        if not t.has("(y/n)"):
            fail("`d` did not open the delete confirm while a query was active "
                 "-- the query is behaving like a SubMode", "4: no confirm")
        elif SESSION not in "".join(r for r in t.rows_text() if "(y/n)" in r):
            fail("the delete confirm did not name the filtered session -- `j` "
                 "never actually navigated the filtered tree", "4: wrong target")
        if TAB not in t.rows_text()[search_y]:
            fail("the query was wiped by entering the delete sub-mode", "4: query wiped")
        t.send(b"n", 0.8)  # decline
        if t.has("(y/n)"):
            fail("the delete confirm did not dismiss", "4: confirm stuck")
        if len(fails) == before:
            print("4. OK: `d` opens the delete confirm and the query survives it")

        # --- 5. `/` refocuses; `q` typed there does not close -----------------
        before = len(fails)
        t.send(b"/", 0.8)
        if CURSOR not in t.rows_text()[search_y]:
            fail("`/` did not refocus the search bar", "5: no refocus")
        t.send(b"q", 0.8)
        if not t.has("Session Manager"):
            fail("`q` typed into the search bar closed the manager", "5: q closed it")
        if not t.alive():
            fail("the client exited on `q` in the search bar", "5: client dead")
        if len(fails) == before:
            print("5. OK: `/` refocuses the search bar and `q` is plain text there")

        # --- 6. a no-match query is survivable, and clearing restores ---------
        before = len(fails)
        # Ctrl-U clears the whole query in one keystroke; type a fresh one that
        # cannot match anything.
        t.send(b"\x15", 1.0)
        t.send(b"zzzznope", 1.2)
        for missing in (SESSION, OTHER, FOLDER, TAB):
            if t.has(missing):
                fail(f"a no-match query still shows {missing!r}", "6: not filtered")
                break
        if not t.has("local"):
            fail("the server row disappeared on a no-match query", "6: server row gone")
        if not t.alive():
            fail("the client died on a no-match query", "6: client dead")
        t.send(b"\x15", 1.2)
        if not (t.has(SESSION) and t.has(OTHER) and t.has(FOLDER)):
            fail("clearing the query did not bring the tree back", "6: tree not restored")
        if len(fails) == before:
            print("6. OK: a no-match query is harmless and Ctrl-U restores the tree")

        # --- 7. the selection lands on the match, so Enter activates it -------
        # The regression: `on_query_changed` reset `selected` to 0, and row 0 is
        # ALWAYS the `local` server row (server rows ignore the query). Enter
        # there merely toggled the server's expansion, so the first thing a user
        # tries after searching did nothing. OTHER is an unfiled session, so the
        # filtered tree is `local` + that session; Enter must switch to it.
        before = len(fails)
        status_before = t.rows_text()[-1]
        if OTHER in status_before:
            fail(f"precondition: already attached to {OTHER!r} "
                 f"(status {status_before.rstrip()!r})", "7: bad precondition")
        t.send(b"omega", 1.0)          # focus is on the search bar with an empty query
        t.send(b"\t", 0.8)             # hand focus to the tree
        t.send(b"\r", 1.5)             # activate whatever the selection is on
        if t.has("Session Manager"):
            fail("Enter did not activate the match -- the manager is still open, "
                 "so the selection was sitting on the `local` server row and "
                 "Enter just toggled its expansion", "7: manager still open")
        status_after = t.rows_text()[-1]
        if OTHER not in status_after:
            fail(f"the client did not switch to the matched session {OTHER!r} "
                 f"(status bar {status_after.rstrip()!r})", "7: no switch")
        if status_after == status_before:
            fail("the status bar is unchanged -- nothing was activated",
                 "7: status unchanged")
        if not t.alive():
            fail("the client died activating the search match", "7: client dead")
        if len(fails) == before:
            print(f"7. OK: a query matching {OTHER!r} selects it; Enter switches to it")

        # --- 8. a match inside a FOLDER selects the session, not the folder ---
        # The residual of check 7's bug, one level down: "first non-server row"
        # is the FOLDER whenever the match is foldered (ancestors always render),
        # and Enter on a folder just toggles it open. SESSION lives inside
        # FOLDER, so this is the case that must select the session itself.
        before = len(fails)
        t.prefix(b"xm", 1.2)
        t.pump(0.8)
        if not t.has("Session Manager"):
            fail("the manager did not reopen for the foldered-match check",
                 "8: manager did not reopen")
        else:
            # 9. The manager reopened AFTER check 7 switched to OTHER: the
            # highlight must be on OTHER now, not on the session the client
            # started in. A snap that is really a fixed row would still be
            # sitting on `main`, and one that never fires on `local`.
            reopened_sel = sel_row(t, search_y)
            if reopened_sel == -1:
                fail("no row is highlighted in the reopened manager", "9: no selection")
            elif OTHER not in t.rows_text()[reopened_sel]:
                fail(f"the reopened manager does not highlight the session the "
                     f"client switched to ({OTHER!r}); row {reopened_sel} is "
                     f"{t.rows_text()[reopened_sel].rstrip()!r}",
                     "9: highlight did not follow the switch")
            else:
                print(f"9. OK: reopening highlights {OTHER!r}, the session "
                      "switched to")
            t.send(b"/", 0.8)          # into the search bar
            t.send(b"\x15", 0.8)       # a reopened manager starts empty; be sure
            status_before = t.rows_text()[-1]
            if SESSION in status_before:
                fail(f"precondition: already attached to {SESSION!r} "
                     f"(status {status_before.rstrip()!r})", "8: bad precondition")
            t.send(SESSION.encode(), 1.2)
            if not t.has(FOLDER):
                fail(f"the match's folder {FOLDER!r} is not shown -- the fixture "
                     "is not exercising the foldered case", "8: folder missing")
            t.send(b"\t", 0.8)         # hand focus to the tree
            t.send(b"\r", 1.5)         # activate whatever the selection is on
            if t.has("Session Manager"):
                fail("Enter did not activate the match -- the manager is still "
                     f"open, so the selection was sitting on the {FOLDER!r} "
                     "folder row and Enter just toggled its expansion",
                     "8: manager still open")
            status_after = t.rows_text()[-1]
            if SESSION not in status_after:
                fail(f"the client did not switch to the matched session {SESSION!r} "
                     f"(status bar {status_after.rstrip()!r})", "8: no switch")
            if not t.alive():
                fail("the client died activating the foldered search match",
                     "8: client dead")
        if len(fails) == before:
            print(f"8. OK: a query matching foldered {SESSION!r} selects the "
                  f"session, not {FOLDER!r}; Enter switches to it")

        # --- 10. still alive, no panic ---------------------------------------
        if not t.alive():
            fails.append("the client is not alive at the end")
        logs = (t.log("client") + t.log("server")).lower()
        if "panic" in logs:
            fails.append("panic in the logs")

        if fails:
            print("\nFAILURES:")
            for f in fails:
                print(f"  - {f}")
            t.dump("final")
            print("RESULT: FAIL")
            sys.exit(1)
        print("\nRESULT: PASS -- the session manager search bar filters, keeps "
              "ancestors, and routes keys by focus")
    finally:
        if b is not None:
            b.close()
        t.kill()


if __name__ == "__main__":
    main()
