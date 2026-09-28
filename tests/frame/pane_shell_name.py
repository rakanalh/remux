#!/usr/bin/env python3
"""Frame-level test: a fresh pane is named after its shell, never the server.

A new pane's shell is forked from the server, so for a moment before its exec
it is a copy of `remux`. A name read in that moment and cached was how every
new shell pane on macOS came to be called `remux`.

Each pane's name in the session tree is compared with what the shell inside it
says it is (`ps -o comm= -p $$`, basename taken), rather than with a fixed
string, so the check holds on any platform and any `/bin/sh`.

  1. the first pane, and two panes split while the agents pusher is sampling
     every pane, are named by their shell
  2. none of them is ever named `remux`, over 3 seconds of polling
  3. after a restart restores all twelve panes at once, each is named by its
     shell
  4. a pane whose program execs another (`sh -c 'sleep 1; exec sleep 30'`) is
     renamed by what it became
  5. a copy of `sleep` named `2.1.283`, run as `claude` through a symlink and
     (on macOS) through an exec with argv[0] `claude`, is named `claude`, as
     Claude Code is

Run: python3 tests/frame/pane_shell_name.py
"""
import os
import re
import shutil
import socket
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Server, Client, name_of, only  # noqa: E402

RUNDIR = f"/tmp/rmx-psn-{os.getpid()}"
# macOS's `/bin/sh` is a trampoline that execs another shell, which then names
# itself differently to `ps` and to the kernel's executable path. Compare like
# with like by running a real shell there unless told otherwise.
if sys.platform == "darwin":
    os.environ.setdefault("REMUX_TEST_SHELL", "/bin/zsh")
COLS, ROWS = 120, 40
FAILS = []


def check(cond, msg):
    print(("PASS " if cond else "FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


def tree(c):
    c.send("ListSessionTree")
    end = time.time() + 1.5
    while time.time() < end:
        try:
            m = c.recv()
        except socket.timeout:
            continue
        if name_of(m) == "SessionTree":
            return only(m, "SessionTree")
    return None


def pane_names(c):
    t = tree(c) or {}
    out = {}
    for s in t.get("unfiled", []):
        for tab in s["tabs"]:
            for p in tab["panes"]:
                out[p["id"]] = p["name"]
    return out


class Grid:
    def __init__(self):
        self.g = [[" "] * COLS for _ in range(ROWS)]

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

    def shells(self):
        found = []
        for row in ("".join(r) for r in self.g):
            for m in re.finditer(r"SH=([^\s│|]+)", row):
                found.append(os.path.basename(m.group(1)).lstrip("-"))
        return found


def restored_names(srv):
    """3. Restart the server on the same directories, so it restores every
    saved pane at once, and return what the restored panes are named after a
    few seconds. Starting many shells back to back is what lets a name be read
    in the instant between a fork and its exec."""
    p = subprocess.Popen([BIN, "server"], env=srv.env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(200):
            if os.path.exists(srv.sock):
                break
            time.sleep(0.05)
        time.sleep(3.0)
        c = Client(srv.sock)
        c.hello()
        c.send("ListSessionTree")
        names = []
        end = time.time() + 2
        while time.time() < end and not names:
            for m in c.drain(0.2):
                if name_of(m) == "SessionTree":
                    for sess in only(m, "SessionTree").get("unfiled", []):
                        for tab in sess["tabs"]:
                            names += [p_["name"] for p_ in tab["panes"]]
        c.close()
        return names
    finally:
        p.kill()
        p.wait()


def main():
    srv = Server(RUNDIR).start()
    try:
        c = Client(srv.sock)
        c.hello()
        c.send({"CreateSession": {"name": "main", "folder": None}})
        c.send({"Attach": {"session_name": "main"}})
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        c.drain(0.8)
        # The agents pusher names every pane's foreground group up to ten times
        # a second while panes produce output, through the same process-name
        # cache the pane names use. That is what samples a new shell in the
        # instant between its fork and its exec.
        c.send("SubscribeAgents")
        c.send({"Input": {"data": list(b"while :; do printf .; sleep 0.02; done\n")}})
        c.drain(0.5)
        c.send({"Command": "PaneSplitVertical"})
        c.drain(0.6)
        c.send({"Command": "PaneSplitHorizontal"})
        c.drain(0.6)
        # Stop the output loop in the first pane before reading names.
        c.send({"Command": "PaneFocusLeft"})
        c.send({"Input": {"data": list(b"\x03clear\n")}})
        c.drain(0.5)
        c.send({"Command": "PaneFocusRight"})
        c.drain(0.3)

        seen = set()
        end = time.time() + 3.0
        while time.time() < end:
            seen.update(pane_names(c).values())
            time.sleep(0.05)
        check("remux" not in seen, f"2 no pane is ever named remux ({sorted(seen)})")

        grid = Grid()
        for move in ("PaneFocusUp", "PaneFocusLeft", None):
            c.send({"Input": {"data": list(b"printf 'S%s=%s\\n' H \"$(ps -o comm= -p $$)\"\n")}})
            grid.feed(c.drain(0.8))
            if move:
                c.send({"Command": move})
                grid.feed(c.drain(0.4))
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        grid.feed(c.drain(0.8))
        shells = set(grid.shells())
        names = pane_names(c)
        check(len(names) == 3, f"three panes ({names})")
        check(len(shells) == 1, f"every pane runs the same shell ({shells})")
        shell = next(iter(shells), None)
        check(shell is not None and all(n == shell for n in names.values()),
              f"1 every fresh pane is named by its shell {shell!r} ({names})")
        # 4. A pane whose program execs something else after a while: its name
        # must follow the exec. Deterministic, where the fork-to-exec window
        # of a shell is a race.
        cli = Client(srv.sock)
        cli.hello()
        cli.send({"CliSpawn": {"session": "main", "placement": "SplitBelow",
                               "argv": ["/bin/sh", "-c", "sleep 1; exec sleep 30"],
                               "cwd": None}})
        cli.drain(0.5)
        cli.close()
        time.sleep(1.0)
        before = set(pane_names(c).values())
        time.sleep(2.5)
        after = pane_names(c)
        check("sleep" in after.values(),
              f"4 a pane that execs is renamed by what it became ({sorted(before)} -> {after})")

        # 5. A program whose executable has a version for a name, run as
        # `claude`, the way Claude Code's launcher runs it: a symlink named
        # `claude` to `versions/<version>`, and an exec with argv[0] set to
        # `claude`. macOS reports the executable's real path, so a name read
        # from it says the version.
        versions = f"{RUNDIR}/v"
        os.makedirs(versions, exist_ok=True)
        real = f"{versions}/2.1.283"
        shutil.copy("/bin/sleep", real)
        os.chmod(real, 0o755)
        if sys.platform == "darwin":
            # A copied system binary keeps a signature that no longer matches
            # its path, and macOS kills it on exec. Re-sign it ad hoc.
            subprocess.run(["codesign", "--force", "-s", "-", real],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
        link = f"{RUNDIR}/claude"
        os.symlink(real, link)
        launches = {
            "a symlink named claude": [link, "30"],
            "an exec with argv[0] claude": [
                sys.executable, "-c",
                f"import os; os.execv({real!r}, ['claude', '30'])",
            ],
        }
        for how, argv in launches.items():
            if how.startswith("an exec") and sys.platform != "darwin":
                # Linux names a process by its `comm`, which an exec sets from
                # the file's name and not from argv[0]. Real launches go
                # through the symlink, which the first case covers.
                continue
            before_ids = set(pane_names(c))
            cli = Client(srv.sock)
            cli.hello()
            cli.send({"CliSpawn": {"session": "main", "placement": "SplitBelow",
                                   "argv": argv, "cwd": None}})
            cli.drain(0.5)
            cli.close()
            time.sleep(2.5)
            names = pane_names(c)
            new = [n for pid, n in names.items() if pid not in before_ids]
            check(new == ["claude"],
                  f"5 a versioned executable run through {how} is named claude ({new})")

        # Enough panes that a restore starts a burst of shells.
        for _ in range(3):
            c.send({"Command": "TabNew"})
            c.drain(0.4)
            for split in ("PaneSplitVertical", "PaneSplitHorizontal"):
                c.send({"Command": split})
                c.drain(0.4)
        c.close()
        check("panicked" not in srv.log(), "no panic in the server log")
    finally:
        srv.kill()
    restored = restored_names(srv)
    check(len(restored) >= 12 and set(restored) == {shell},
          f"3 every restored pane is named by its shell {shell!r} ({restored})")
    print(f"\n{'FAILED' if FAILS else 'OK'}: {len(FAILS)} failure(s)")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()
