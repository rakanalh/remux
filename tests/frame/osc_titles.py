#!/usr/bin/env python3
"""Frame-level test: `OSC 0` / `OSC 2` titles name a pane.

Every name is read from `ListSessionTree` (the server's own resolved name) and,
where the case is about what is drawn, from the reconstructed border too.

The rule under test: a title belongs to the process group that set it, and
names the pane only while that group is the effective foreground. A job becomes
the effective foreground once it has held the terminal for the settle window
(1s), so a short command never changes the name.

  0. premise: `/bin/sh` runs a command as its own foreground process group.
  1. an untitled pane is named by its shell, and by a job's process once the
     job has held the foreground for the window.
  5. a title whose program exits INSIDE the window is never adopted.
  4. a program's title names the pane while the program runs, and goes when it
     exits.
 10. a pipeline whose group leader has exited (`true | sleep 4`) is still
     named after a live member (`sleep`), never the literal `shell`.
  2. a title the shell sets is not the name before the window, is the name
     after it (tree, border and `PaneContent`).
  3. a title replaced within 80ms never becomes the name; its replacement does.
  8. under a shell title, `sleep 0.3` never changes the name, and `sleep 2`
     reads exactly [Home, sleep, Home]: no fallback flash in between.
 14. a job that `exec`s (`sh -c 'sleep 1.5; exec sleep 6'`) is renamed by
     what it became.
 15. preexec + precmd titles around `sleep 2` read exactly [Home, sleep, Home].
 12. a title the shell sets right after a job exits (a zsh `precmd`) is
     adopted.
  9. a title written by a program in its last moment (`sh -c 'printf ...'`) is
     not adopted as the shell's. Counted over runs, since it is a race.
  6. a user-set name (`PaneRename`) beats a title.
  7. `[appearance] pane_title = "{command}: {title}"` renders on the border and
     in the tree.
 16. a pane moved into a neighbour's stack (`PaneStackInto*`) keeps a title
     that still tracks live, and carries a custom name; neither the title nor a
     title-derived name is saved.
 13. a title is never written to `state.json`, a custom name is, and a
     restored pane is not named by the old title.
"""
import json, os, re, shutil, socket, subprocess, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Server, Client, name_of, only  # noqa: E402

RUNDIR = f"/tmp/rmx-osct-{os.getpid()}"
# The shell's name as this server reports it, read off the first pane at its
# prompt: `sh` on Linux, where macOS's `/bin/sh` is a trampoline that execs
# another shell under its own name.
SH = "sh"
COLS, ROWS = 100, 30
FAILS = []


def check(cond, msg):
    print(("PASS " if cond else "FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


class View:
    """The grid rebuilt from renders, plus the latest session tree."""

    def __init__(self):
        self.g = [[" "] * COLS for _ in range(ROWS)]
        self.tree = None
        self.content = None

    def feed(self, msgs):
        for m in msgs:
            n = name_of(m)
            body = only(m, n)
            if n == "FullRender":
                for y, row in enumerate(body["cells"]):
                    for x, cell in enumerate(row):
                        if y < ROWS and x < COLS:
                            self.g[y][x] = cell.get("c", " ")
            elif n == "RenderDiff":
                for ch in body["changes"]:
                    if ch["y"] < ROWS and ch["x"] < COLS:
                        self.g[ch["y"]][ch["x"]] = ch["cell"].get("c", " ")
            elif n == "SessionTree":
                self.tree = body
            elif n == "PaneContent":
                self.content = body

    def rows(self):
        return ["".join(r) for r in self.g]

    def has(self, text):
        return any(text in r for r in self.rows())


def fresh_frame(c, v):
    """Re-`Resize` to force a `FullRender`, so no cell is left from before."""
    v.g = [[" "] * COLS for _ in range(ROWS)]
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})
    v.feed(c.drain(0.5))


def pane_entry(c, v, timeout=1.0):
    v.tree = None
    c.send("ListSessionTree")
    end = time.time() + timeout
    while time.time() < end and v.tree is None:
        try:
            v.feed([c.recv()])
        except socket.timeout:
            pass
    if v.tree is None:
        return None
    for s in v.tree["unfiled"] + [s for f in v.tree["folders"] for s in f["sessions"]]:
        if s["name"] == "main":
            return s["tabs"][0]["panes"][0]
    return None


def pane_name(c, v, timeout=1.0):
    entry = pane_entry(c, v, timeout)
    return entry["name"] if entry else None


def typed(c, text):
    c.send({"Input": {"data": list(text.encode())}})


def watch(c, v, seconds, step=0.005, polls=None):
    """Every distinct name seen, in order, polled every `step` for `seconds`."""
    seen = []
    end = time.time() + seconds
    while time.time() < end:
        n = pane_name(c, v)
        if polls is not None:
            polls.append(n)
        if n is not None and (not seen or seen[-1] != n):
            seen.append(n)
        time.sleep(step)
    return seen


def wait_name(c, v, want, timeout):
    end = time.time() + timeout
    while time.time() < end:
        if pane_name(c, v) == want:
            return True
        time.sleep(0.05)
    return False


def attach(c):
    c.hello()
    c.send({"CreateSession": {"name": "main", "folder": None}})
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})
    c.drain(0.8)


def set_shell_title(c, v, a, b):
    """Have the shell title the pane `a + b`, assembled by `printf`."""
    typed(c, f"printf '\\033]2;%s%s\\007' {a} {b}\r")
    return wait_name(c, v, a + b, 2.5)


def run_default(c, v):
    global SH
    time.sleep(1.2)
    SH = pane_name(c, v) or SH
    # 0. premise.
    typed(c, "sh -c 'printf \"%s%s:%s:%s:%s\\n\" J C $$ $(ps -o pgid= -p $$) "
             "$(ps -o tpgid= -p $$)' | tr -d ' '\r")
    time.sleep(0.8)
    v.feed(c.drain(0.3))
    found = None
    for row in v.rows():
        for m in re.finditer(r"JC:(\d+):(\d+):(\d+)", row):
            found = m.groups()
    check(found is not None and found[0] == found[1] == found[2],
          f"0 a command is its own foreground process group ({found})")

    # 1. an untitled pane.
    typed(c, "sleep 3\r")
    time.sleep(0.4)
    early = pane_name(c, v)
    check(early == SH, f"1 a job is not the name before it has held the foreground ({early!r})")
    check(wait_name(c, v, "sleep", 2.0), "1 an untitled pane is then named by its foreground job")
    check(wait_name(c, v, SH, 4.0), "1 and by its shell again at the prompt")

    # 5. setter exits inside the window.
    typed(c, "sh -c 'printf \"\\033]2;%s%s\\007\" Qu ick; sleep 0.3'; sleep 2\r")
    seen = watch(c, v, 2.2, step=0.05)
    check("Quick" not in seen and "sleep" in seen,
          f"5 a title whose program exited in the window is never adopted ({seen})")
    check(wait_name(c, v, SH, 3.0), "5 back at the prompt")

    # 4. a program's title lives as long as the program.
    typed(c, "sh -c 'printf \"\\033]2;%s%s\\007\" Sub Title; sleep 2.5'\r")
    check(wait_name(c, v, "SubTitle", 2.2), "4 a program's title becomes the name")
    check(wait_name(c, v, SH, 3.0), "4 and is dropped when the program exits")
    time.sleep(1.5)
    check(pane_name(c, v) == SH, "4 and stays dropped")

    # 10. a pipeline whose group leader is gone.
    # The leader (`true`) exits before any sample can see it alive.
    typed(c, "true | sleep 4\r")
    seen = watch(c, v, 3.0, step=0.05)
    check("sleep" in seen and "shell" not in seen,
          f"10 a pipeline is named after its group, never `shell` ({seen})")
    check(wait_name(c, v, SH, 3.0), "10 back at the prompt")

    # 14. a job that execs is renamed after the exec.
    typed(c, "sh -c 'sleep 1.5; exec sleep 6'\r")
    check(wait_name(c, v, SH, 1.0), "14 the job is named after its leader before the exec")
    check(wait_name(c, v, "sleep", 4.0), f"14 and after the exec by what it became ({pane_name(c, v)!r})")
    check(wait_name(c, v, SH, 6.0), "14 back at the prompt")

    # 2. a shell title settles.
    sent = time.time()
    typed(c, "printf '\\033]2;%s%s\\007' Hel 'lo Title'\r")
    time.sleep(0.5)
    early = pane_name(c, v)
    check(early != "Hello Title", f"2 the title is not the name inside the window ({early!r})")
    adopted = wait_name(c, v, "Hello Title", 2.0)
    took = time.time() - sent
    check(adopted and took >= 0.9, f"2 the title becomes the name after the window ({took:.2f}s)")
    fresh_frame(c, v)
    check(v.has("Hello Title"), "2 the border shows the title")
    pane_id = pane_entry(c, v)["id"]
    c.send({"SubscribePane": {"pane_id": pane_id, "cols": 60, "rows": 10}})
    v.content = None
    end = time.time() + 1.5
    while time.time() < end and v.content is None:
        v.feed(c.drain(0.1))
    info = (v.content or {}).get("pane_name") or {}
    check(info.get("title") == "Hello Title" and info.get("command") == SH,
          f"2 PaneContent carries the title and command ({info})")
    c.send({"UnsubscribePane": {"pane_id": pane_id}})

    # 3. no flicker.
    typed(c, "printf '\\033]2;%s\\007' FL'ASH'; sleep 0.08; printf '\\033]2;%s\\007' St'eady'\r")
    polls = []
    seen = watch(c, v, 2.2, polls=polls)
    check(len(polls) > 100, f"3 the watch polls often enough to see an 80ms title ({len(polls)} polls)")
    check("FLASH" not in seen, f"3 a title replaced within 80ms never appears ({seen})")
    check("Steady" in seen, f"3 its replacement does ({seen})")

    # 8. a shell title and a job.
    check(set_shell_title(c, v, "Ho", "me"), "8 a shell-set title names the pane at the prompt")
    typed(c, "sleep 0.3\r")
    seen = watch(c, v, 1.2)
    check(seen == ["Home"], f"8 a short job never displaces the shell title ({seen})")
    typed(c, "sleep 2\r")
    seen = watch(c, v, 3.5, step=0.01)
    check(seen == ["Home", "sleep", "Home"],
          f"8 `sleep 2` reads exactly [Home, sleep, Home] ({seen})")

    # 11. a job's own title does not cost the shell its title.
    typed(c, "sh -c 'printf \"\\033]2;%s%s\\007\" Job T; sleep 2.5'\r")
    seen = watch(c, v, 4.5, step=0.01)
    check(seen == ["Home", "JobT", "Home"],
          f"11 a job's title comes and goes, and the shell's is back at once ({seen})")

    # 15. preexec and precmd hooks around a long command: the command's
    # preexec title is the shell's, so it is never shown, and precmd's restores
    # nothing visible.
    # Polled every 100ms: the flash this guards lasted a whole settle window,
    # and polling harder slows the PTY reader enough that the preexec title is
    # sometimes read after `sleep` has the terminal and is taken for its own.
    typed(c, "printf '\\033]2;%s%s\\007' Jo b; sleep 2; printf '\\033]2;%s%s\\007' Ho me\r")
    seen = watch(c, v, 4.5, step=0.1)
    check(seen == ["Home", "sleep", "Home"],
          f"15 preexec/precmd around `sleep 2` reads exactly [Home, sleep, Home] ({seen})")

    # 12. a precmd-style title right after a job.
    typed(c, "sleep 1.5; printf '\\033]2;%s%s\\007' Ho me2\r")
    check(wait_name(c, v, "Home2", 4.5), f"12 a title the shell sets after a job is adopted ({pane_name(c, v)!r})")

    # 9. a title written by a program as it exits is not the shell's. The
    # attribution races the program's exit: the PTY reader tags each read with
    # the foreground group it saw on waking, and a read that wakes after the
    # shell has taken the terminal back tags the program's title as the
    # shell's. So this counts adoptions over independent runs, each from a
    # fresh shell title, and fails when they are the rule rather than the
    # exception: the bug adopts every time, the race rarely.
    runs = []
    for i in range(16):
        set_shell_title(c, v, "Ho", f"me{i}")
        typed(c, "sh -c 'printf \"\\033]2;%s%s\\007\" Ex it'\r")
        time.sleep(1.6)
        runs.append(pane_name(c, v))
    adopted = runs.count("Exit")
    print(f"     9: the exiting title was adopted in {adopted}/{len(runs)} runs")
    check(adopted <= 4, f"9 an exiting program's title is not adopted as the shell's ({runs})")

    # 6. a user-set name wins.
    c.send({"Command": {"PaneRename": "mine"}})
    time.sleep(0.3)
    typed(c, "printf '\\033]2;%s%s\\007' Not 'Me'\r")
    seen = watch(c, v, 1.8, step=0.05)
    check(seen == ["mine"], f"6 a custom name beats the title ({seen})")


def run_template(c, v):
    typed(c, "printf '\\033]2;%s%s\\007' Tm pl\r")
    ok = wait_name(c, v, f"{SH}: Tmpl", 2.5)
    check(ok, f"7 the template names the pane in the tree ({pane_name(c, v)!r})")
    fresh_frame(c, v)
    check(v.has(f"{SH}: Tmpl"), "7 and on the border")


def panes_by_id(c, v):
    """{pane id: (name, focused)} for session `main`, off a fresh tree."""
    pane_entry(c, v)
    out = {}
    for s_ in (v.tree or {}).get("unfiled", []):
        if s_["name"] == "main":
            for t in s_["tabs"]:
                for p_ in t["panes"]:
                    out[p_["id"]] = (p_["name"], p_["is_focused"] and t.get("is_active"))
    return out


def wait_pane_name(c, v, pane_id, want, timeout):
    end = time.time() + timeout
    while time.time() < end:
        if panes_by_id(c, v).get(pane_id, (None,))[0] == want:
            return True
        time.sleep(0.05)
    return False


def run_stack(srv):
    """16. a pane moved into a neighbour's stack keeps a LIVE title, and
    carries a custom name."""
    c = Client(srv.sock)
    attach(c)
    v = View()
    c.send({"Command": "PaneSplitVertical"})
    c.drain(0.6)
    c.send({"Command": "PaneSplitHorizontal"})
    c.drain(0.8)
    panes = panes_by_id(c, v)
    mover = next((pid for pid, (_, focused) in panes.items() if focused), None)
    check(len(panes) == 3 and mover is not None, f"16 three panes, one focused ({panes})")
    typed(c, "printf '\\033]2;%s%s\\007' Stack A\r")
    check(wait_pane_name(c, v, mover, "StackA", 2.5), "16 the pane is named by its title")
    c.send({"Command": "PaneStackIntoLeft"})
    c.drain(0.6)
    after = panes_by_id(c, v)
    check(len(after) == 3 and after.get(mover, (None,))[0] == "StackA",
          f"16 the moved pane keeps its title ({after})")
    typed(c, "printf '\\033]2;%s%s\\007' Stack B\r")
    check(wait_pane_name(c, v, mover, "StackB", 2.5),
          f"16 and its title still tracks live after the move ({panes_by_id(c, v)})")
    c.send({"Command": {"PaneRename": "Keep" + "Me"}})
    c.drain(0.4)
    c.send({"Command": "PaneStackIntoRight"})
    c.drain(0.6)
    check(panes_by_id(c, v).get(mover, (None,))[0] == "KeepMe",
          f"16 a custom name survives a move ({panes_by_id(c, v)})")
    c.send({"Command": "TabNew"})
    time.sleep(1.0)
    saved = state_json(srv.rundir)
    check("KeepMe" in saved and "StackB" not in saved,
          "16 the custom name is saved and the title is not")
    c.close()


def state_json(rundir):
    p = f"{rundir}/data/remux/state.json"
    return open(p).read() if os.path.exists(p) else ""


def run_persistence(srv):
    c = Client(srv.sock)
    attach(c)
    v = View()
    check(set_shell_title(c, v, "Persist", "Me"), "13 the title is the name before the save")
    c.send({"Command": "TabNew"})
    time.sleep(0.8)
    c.send({"Command": {"PaneRename": "Kept" + "Name"}})
    time.sleep(0.3)
    c.send({"Command": "TabNew"})
    time.sleep(1.0)
    saved = state_json(srv.rundir)
    check("KeptName" in saved, "13 a custom name IS saved (so the file is the right one)")
    check("PersistMe" not in saved, "13 the title is not saved")
    c.close()


def main():
    srv = Server(RUNDIR).start()
    try:
        c = Client(srv.sock)
        attach(c)
        run_default(c, View())
        c.close()
        check("panicked" not in srv.log(), "no panic in the server log (default)")
    finally:
        srv.kill()

    srv = Server(RUNDIR).start(config='[appearance]\npane_title = "{command}: {title}"\n')
    try:
        c = Client(srv.sock)
        attach(c)
        run_template(c, View())
        c.close()
        check("panicked" not in srv.log(), "no panic in the server log (template)")
    finally:
        srv.kill()

    srv = Server(RUNDIR).start()
    try:
        run_stack(srv)
        check("panicked" not in srv.log(), "no panic in the server log (stack)")
    finally:
        srv.kill()

    srv = Server(RUNDIR).start()
    try:
        run_persistence(srv)
    finally:
        srv.kill()
    # Restart on the same directories: the restored pane must not wear the title.
    p = subprocess.Popen([BIN, "server"], env=srv.env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(200):
            if os.path.exists(srv.sock):
                break
            time.sleep(0.05)
        time.sleep(0.5)
        c = Client(srv.sock)
        c.hello()
        v = View()
        names = []
        c.send("ListSessionTree")
        end = time.time() + 2
        while time.time() < end and v.tree is None:
            v.feed(c.drain(0.1))
        for s in (v.tree or {}).get("unfiled", []):
            for t in s["tabs"]:
                names += [p_["name"] for p_ in t["panes"]]
        check(names and "PersistMe" not in names and "KeptName" in names,
              f"13 a restored pane keeps its custom name and not the title ({names})")
        c.close()
    finally:
        p.kill()
        p.wait()
        shutil.rmtree(RUNDIR, ignore_errors=True)

    print(f"\n{'FAILED' if FAILS else 'OK'}: {len(FAILS)} failure(s)")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()
