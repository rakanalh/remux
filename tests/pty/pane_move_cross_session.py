#!/usr/bin/env python3
"""`Prefix p t` moves the focused pane into ANOTHER session, driven through
the real client.

The frame harness (`tests/frame/pane_move_cross_session.py`) proves the server
half. Only a real PTY sees the picker the client draws, the keys that drive
it, the status bar after the client follows its pane, and `Alt+o` back.

  1. THE ORIGINAL REPORT. A session whose only tab holds only one pane
     ("Macbook", tab "System"), with a second session ("Work") present. The
     picker used to refuse to open here. It now opens, lists `Macbook` with
     nothing under it (its `+ new tab` would be refused) and `Work` with its
     `+ new tab` and tab, and starts on `Work`'s tab.
  2. Esc closes it and nothing is sent.
  3. Enter moves the pane: the status bar names `Work` (the client followed),
     `Macbook` is gone from the session switcher, the moved pane's shell is the
     SAME process (its pid, printed before and after by the shell), its output
     from before the move is still on it, and the client is alive.
  4. From `Work`, now one tab of two panes and the only session: `Work`'s
     `+ new tab` and no other session.
  5. `+ new tab` under a DIFFERENT session ("Third"): the status bar names
     `Third`, which has two tabs.
  6. `Alt+o` returns to the previous session, `Work`.
  7. A connected remote's sessions are never offered: the picker reads the
     tree of the connection it is pinned to, and the remote is another one.
  8. The client is alive, and no log has a panic.
  9. On a terminal shorter than the list, on its own server: after G the
     highlighted row and its session's header are drawn, and g brings the
     first row back.

The opposite of case 1 (one tab, one pane, and NO other session: the picker
closes without opening) is case 2 of `tests/pty/pane_move_to_tab.py`.

Markers are ASSEMBLED by the shell (`printf 'PID:%s' $$`), so a typed command
line can never satisfy a search, and row scans use `finditer`.

Run: python3 tests/pty/pane_move_cross_session.py
"""
import os
import re
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                "frame"))
from pty_harness import BIN, Tui  # noqa: E402
from harness import Client, name_of  # noqa: E402

RUNDIR = "/tmp/rmxpmxp"
REMOTE = "/tmp/rmxpmxq"
OVERFLOW_RUNDIR = "/tmp/rmxpmxw"
COLS, ROWS = 110, 34
ESC = b"\x1b"
TITLE = "Move pane to tab"
SWITCHER = "Switch Session"

CONFIG = """
[remotes.mini]
ssh = "whatever"
remux_path = "remux"
auto_connect = true
"""

fails = []


def check(cond, msg):
    print(("  PASS  " if cond else "  FAIL  ") + msg)
    if not cond:
        fails.append(msg)


def popup(t, title):
    """The rows of the popup titled `title` as [(text, selected)], or None.

    A session header is a rule with the name in it and is reported as
    `HEADER:<name>`. The highlighted row is the one whose background differs
    from the popup's own border cell on that row.
    """
    rows = t.rows_text()
    top = next((y for y, r in enumerate(rows) if title in r and "╭" in r), None)
    if top is None:
        return None
    # The corners nearest the title: on a short terminal the popup's top row is
    # also a pane border's, whose own corners sit outside the popup.
    at = rows[top].index(title)
    x0 = rows[top].rindex("╭", 0, at)
    x1 = rows[top].index("╮", at)
    out = []
    y = top + 1
    while y < len(rows) and rows[y][x0] in "│├":
        line = t.screen.buffer[y]
        text = rows[y][x0 + 1:x1].strip()
        if text.startswith("─"):
            out.append(("HEADER:" + text.strip("─ "), False))
        else:
            out.append((text, line[x0 + 2].bg != line[x0].bg))
        y += 1
    return out


def texts(rows):
    return [text for text, _ in rows or []]


def selected(rows):
    hits = [text for text, sel in rows or [] if sel]
    return hits[0] if len(hits) == 1 else None


def status(t):
    return t.rows_text()[-1]


def status_session(t):
    """The session name the status bar shows, after the mode chip."""
    m = re.search(r"\]\s+(\S+)\s+│", status(t))
    return m.group(1) if m else None


def tab_names(t):
    return re.findall(r"Tab \d+", status(t))


def pids(t, tag):
    """Every `<tag>:<pid>` on screen. `finditer`: panes share rows."""
    rx = re.compile(re.escape(tag) + r":(\d+)")
    return [m.group(1) for r in t.rows_text() for m in rx.finditer(r)]


def open_picker(t):
    t.prefix(b"pt", 1.2)
    return popup(t, TITLE)


def wire(sock):
    c = Client(sock)
    c.hello()
    return c


def session_names(sock):
    c = wire(sock)
    try:
        c.send("ListSessionTree")
        for msg in c.drain(1.5):
            if name_of(msg) == "SessionTree":
                tree = msg["SessionTree"]
                names = [s["name"] for s in tree["unfiled"]]
                for f in tree["folders"]:
                    names += [s["name"] for s in f["sessions"]]
                return names
        return None
    finally:
        c.close()


def start_remote():
    """A second isolated server holding `remote-only`, reached by a fake `ssh`
    that execs the relay against it."""
    shutil.rmtree(REMOTE, ignore_errors=True)
    for s in ("run", "state", "data", "config", "bin"):
        os.makedirs(f"{REMOTE}/{s}", exist_ok=True)
    env = {**os.environ, "XDG_RUNTIME_DIR": f"{REMOTE}/run", "XDG_STATE_HOME": f"{REMOTE}/state",
           "XDG_DATA_HOME": f"{REMOTE}/data", "XDG_CONFIG_HOME": f"{REMOTE}/config",
           "SHELL": "/bin/sh", "ENV": "/dev/null", "TERM": "xterm-256color", "HOME": REMOTE}
    with open(f"{REMOTE}/bin/ssh", "w") as f:
        f.write("#!/bin/sh\n"
                f"export XDG_RUNTIME_DIR={REMOTE}/run\n"
                f"export XDG_STATE_HOME={REMOTE}/state\n"
                f"export XDG_DATA_HOME={REMOTE}/data\n"
                f"export XDG_CONFIG_HOME={REMOTE}/config\n"
                f"exec {BIN} relay\n")
    os.chmod(f"{REMOTE}/bin/ssh", 0o755)
    proc = subprocess.Popen([BIN, "server"], env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sock = f"{REMOTE}/run/remux.sock"
    for _ in range(200):
        if os.path.exists(sock):
            break
        time.sleep(0.05)
    time.sleep(0.3)
    c = wire(sock)
    c.send({"CreateSession": {"name": "remote-only", "folder": None}})
    c.drain(0.6)
    c.close()
    return proc, env


def overflow():
    """[9] More sessions than a small terminal has rows: after G the
    highlighted row is still drawn, so Enter never acts on a row nobody sees."""
    print("[9] a picker taller than the terminal keeps the highlight on screen")
    t = Tui(OVERFLOW_RUNDIR, cols=80, rows=14, extra_env={"HOME": OVERFLOW_RUNDIR})
    try:
        t.start()
        t.pump(1.5)
        c = wire(f"{OVERFLOW_RUNDIR}/run/remux.sock")
        for i in range(10):
            c.send({"CreateSession": {"name": f"s{i:02}", "folder": None}})
        c.drain(1.0)
        c.close()
        rows = open_picker(t)
        check(rows is not None and len(rows) < 1 + 10 * 3,
              f"overflow: the picker is clipped to the terminal ({len(rows or [])} rows drawn)")
        t.send(b"G", 0.6)
        rows = popup(t, TITLE)
        print(f"  after G: {rows}")
        check(selected(rows) == "1: Tab 1" and texts(rows)[-2:] == ["+ new tab", "1: Tab 1"],
              f"overflow: after G the highlighted last row is drawn ({selected(rows)})")
        check("HEADER:s09" in texts(rows), "overflow: and its session's header is drawn")
        t.send(b"g", 0.6)
        rows = popup(t, TITLE)
        check(texts(rows)[:1] == ["HEADER:main"] and selected(rows) is not None,
              f"overflow: g brings the first row back ({texts(rows)[:2]})")
        t.send(ESC, 0.6)
        check(t.alive(), "overflow: the client is alive")
    except Exception as e:
        check(False, f"overflow: the harness could not continue: {e!r}")
    finally:
        t.kill()
    check("panicked at" not in t.log("client"), "overflow: no panic in client.log")


def main():
    overflow()
    remote, remote_env = start_remote()
    t = Tui(RUNDIR, cols=COLS, rows=ROWS, config=CONFIG,
            extra_env={"PATH": f"{REMOTE}/bin:" + os.environ.get("PATH", ""),
                       "HOME": RUNDIR})
    sock = f"{RUNDIR}/run/remux.sock"
    try:
        t.start()
        t.pump(1.5)
        # Name the client's session and its only tab as in the report, then add
        # a second session. The wire client detaches by closing, so it never
        # sizes the frame the PTY client sees.
        c = wire(sock)
        c.send({"Attach": {"session_name": "main"}})
        c.send({"Command": {"SessionRename": "Macbook"}})
        c.send({"Command": {"TabRename": "System"}})
        c.send({"CreateSession": {"name": "Work", "folder": None}})
        c.drain(0.8)
        c.close()
        t.pump(1.0)
        t.send(b"clear; printf 'RAN:%s\\n' before; printf 'PID:%s\\n' $$\r", 1.0)
        pid_before = pids(t, "PID")
        print(f"setup: status={status(t).strip()!r} pid={pid_before}")
        check(status_session(t) == "Macbook" and "System" in status(t),
              f"setup: the client is on Macbook / System ({status(t).strip()!r})")
        check(len(pid_before) == 1, f"setup: the pane printed its pid ({pid_before})")
        check(sorted(n for n in session_names(sock) or []) == ["Macbook", "Work"],
              f"setup: the server holds Macbook and Work ({session_names(sock)})")

        print("[1] the original report: one tab, one pane, another session")
        rows = open_picker(t)
        print(f"  picker: {rows}")
        check(rows is not None, "Prefix p t OPENS the picker")
        check(texts(rows) == ["HEADER:Macbook", "HEADER:Work", "+ new tab", "1: Tab 1"],
              f"Macbook offers nothing, Work offers its new tab and its tab ({texts(rows)})")
        check(selected(rows) == "1: Tab 1",
              f"the highlight starts on Work's tab ({selected(rows)})")

        print("[2] Esc changes nothing")
        t.send(ESC, 0.8)
        check(popup(t, TITLE) is None, "the picker closed")
        check("PaneMoveToTabTarget" not in t.log("server"), "no move was sent")
        check(status_session(t) == "Macbook", "still on Macbook")

        print("[3] Enter moves the pane into Work")
        open_picker(t)
        t.send(b"\r", 1.5)
        print(f"  status: {status(t).strip()!r}")
        check(popup(t, TITLE) is None, "the picker closed")
        check(status_session(t) == "Work", f"the status bar names Work ({status_session(t)})")
        check(session_names(sock) == ["Work"],
              f"the emptied Macbook is gone from the server ({session_names(sock)})")
        t.send(b"printf 'PID2:%s\\n' $$\r", 1.0)
        pid_after = pids(t, "PID2")
        check(pid_after == pid_before and len(pid_after) == 1,
              f"the moved pane runs the SAME shell ({pid_after} vs {pid_before})")
        check(any("RAN:before" in r for r in t.rows_text()),
              "the pane's output from before the move is still on it")
        t.send(b"\x1bh", 0.6)
        t.send(b"printf 'PID3:%s\\n' $$\r", 1.0)
        other_shell = [p for p in pids(t, "PID3") if p not in pid_before]
        check(len(other_shell) == 1,
              f"Work's own shell has another pid, so the pid tells shells apart ({pids(t, 'PID3')})")
        t.send(b"\x1bl", 0.6)
        t.send(b"\x1bs", 1.2)
        sw = popup(t, SWITCHER)
        sw_texts = " ".join(texts(sw))
        print(f"  switcher: {texts(sw)}")
        check(sw is not None and "Work" in sw_texts and "Macbook" not in sw_texts,
              "the session switcher lists Work and not Macbook")
        t.send(ESC, 0.6)
        check(t.alive(), "the client is alive after the move")

        print("[4] from Work, the only local session")
        rows = open_picker(t)
        check(texts(rows) == ["HEADER:Work", "+ new tab"],
              f"Work's new tab and no other session ({texts(rows)})")
        t.send(ESC, 0.6)

        print("[5] + new tab under a different session")
        c = wire(sock)
        c.send({"CreateSession": {"name": "Third", "folder": None}})
        c.drain(0.6)
        c.close()
        rows = open_picker(t)
        check(texts(rows) == ["HEADER:Work", "+ new tab", "HEADER:Third", "+ new tab",
                              "1: Tab 1"], f"Work, then Third ({texts(rows)})")
        t.send(b"G", 0.4)
        t.send(b"k", 0.4)
        rows = popup(t, TITLE)
        sel = [i for i, (_, s) in enumerate(rows or []) if s]
        check(sel == [3], f"G then k lands on Third's `+ new tab` ({rows})")
        t.send(b"\r", 1.5)
        check(status_session(t) == "Third", f"the status bar names Third ({status_session(t)})")
        check(tab_names(t) == ["Tab 1", "Tab 2"], f"Third has two tabs ({tab_names(t)})")
        t.send(b"printf 'PID4:%s\\n' $$\r", 1.0)
        check(pids(t, "PID4") == pid_before, f"still the same shell ({pids(t, 'PID4')})")

        print("[6] Alt+o returns to the previous session")
        t.send(b"\x1bo", 1.5)
        check(status_session(t) == "Work", f"the status bar names Work ({status_session(t)})")

        print("[7] a connected remote's sessions are not offered")
        t.send(b"\x1bs", 1.2)
        sw = popup(t, SWITCHER)
        print(f"  switcher: {texts(sw)}")
        check(sw is not None and any("remote-only" in x for x in texts(sw)),
              "the remote is connected: the switcher lists remote-only")
        t.send(ESC, 0.6)
        rows = open_picker(t)
        print(f"  picker: {rows}")
        check(rows is not None and not any("remote-only" in x for x in texts(rows)),
              f"the picker has no remote-only row ({texts(rows)})")
        t.send(ESC, 0.6)

        print("[8] health")
        check(t.alive(), "the client is alive")
    except Exception as e:
        check(False, f"the harness could not continue: {e!r}")
    finally:
        t.kill()
        try:
            subprocess.run([BIN, "stop"], env=remote_env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=10)
        except Exception:
            pass
        remote.kill()
        remote.wait()

    for which in ("client", "server"):
        check("panicked at" not in t.log(which), f"no panic in {which}.log")
    rlog = f"{REMOTE}/state/remux/server.log"
    check("panicked at" not in (open(rlog).read() if os.path.exists(rlog) else ""),
          "no panic in the remote's server.log")
    server = t.log("server")
    check(re.search(r'PaneMoveToTabTarget \{ tab_id: Some\(\d+\), session: Some\("Work"\) \}', server)
          and re.search(r'PaneMoveToTabTarget \{ tab_id: None, session: Some\("Third"\) \}', server),
          "the server received each move naming its session")

    print()
    if fails:
        print(f"FAIL: {len(fails)} check(s) failed")
        raise SystemExit(1)
    print("OK")


if __name__ == "__main__":
    main()
