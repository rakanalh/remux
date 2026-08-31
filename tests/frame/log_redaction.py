#!/usr/bin/env python3
"""Frame-level test: the server log must not contain what the user TYPED.

`server.log` is plaintext, unrotated and never pruned -- one user's reached
586 MB in about a day -- and `main.rs` pins the logger at `Debug` with no
`RUST_LOG`, so nothing a user can set turns any of this off. Whatever reaches
it stays on disk indefinitely.

`handle_client_message` logs a summary line for every inbound message. Its
`Input` arm is careful: it prints `Input(N bytes)`, the COUNT and never the
content. Two later message types were not given the same treatment and fell
through to the catch-all `other => log::debug!("... msg={other:?}")`, which
Debug-prints the whole struct:

  1. **`InputToPane { pane_id, data }`** -- how every keystroke reaches a VIEW
     cell and how the `files` panel forwards keys. A password, an API key or an
     SSH passphrase typed into a view landed in the log verbatim.
  2. **`CliSpawn { session, placement, argv, cwd }`** -- a command line the user
     typed, e.g. `remux split -- psql postgresql://user:pass@host/db`. Logged
     TWICE: once by the catch-all and once by `handle_cli_spawn`'s own `info!`.

Both are asserted here two ways round, because "the secret is absent" passes
perfectly on a server that logged NOTHING at all: the redacted form must also
be PRESENT, naming the message and the byte/argument count.

Run: python3 tests/frame/log_redaction.py
"""
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx-logredact"
FAILURES = []

# Distinctive enough that a match cannot be a coincidence, and shaped like the
# thing that actually matters. Neither is ever sent as a whole shell command, so
# neither can arrive in the log by some route other than the one under test.
TYPED_SECRET = b"correct-horse-battery-staple-KEYSTROKE"
ARGV_SECRET = "postgresql://sam:hunter2@db.internal/prod-ARGV"
# Yanked to the clipboard. Assembled by `printf` INSIDE the pane rather than
# typed, so the string on screen and the string in the typed command never
# coincide -- a marker that appears in the command line would be "on screen"
# whether or not the yank did anything.
CLIP_HEAD, CLIP_TAIL = "swordfish", "CLIPBOARD"
CLIP_SECRET = CLIP_HEAD + "-" + CLIP_TAIL


def check(cond, msg):
    if cond:
        print(f"  PASS  {msg}")
    else:
        print(f"  FAIL  {msg}")
        FAILURES.append(msg)


def first_pane_id(c):
    """A pane id, read off the session tree rather than assumed to be 1."""
    c.send("ListSessionTree")
    for msg in c.drain(1.5):
        if name_of(msg) != "SessionTree":
            continue
        tree = msg["SessionTree"]
        for bucket in ("unfiled",):
            for sess in tree.get(bucket) or []:
                for tab in sess.get("tabs") or []:
                    for pane in tab.get("panes") or []:
                        return pane["id"]
        for folder in tree.get("folders") or []:
            for sess in folder.get("sessions") or []:
                for tab in sess.get("tabs") or []:
                    for pane in tab.get("panes") or []:
                        return pane["id"]
    return None


class Grid:
    """The composited frame, rebuilt from `FullRender` + `RenderDiff`.

    Fed by EVERY drain in `run`, not just the one next to the check that needs
    it: the baseline `FullRender` arrives once, right after `Attach`, and an
    earlier `drain` that discarded it left the grid permanently empty -- which
    reported the marker "not on screen" while it was plainly there.
    """

    def __init__(self):
        self.rows = []

    def pump(self, c, t=0.4):
        msgs = c.drain(t)
        for m in msgs:
            n = name_of(m)
            if n == "FullRender":
                self.rows = [
                    "".join(cell["c"] for cell in row) for row in m["FullRender"]["cells"]
                ]
            elif n == "RenderDiff" and self.rows:
                for ch in m["RenderDiff"]["changes"]:
                    y, x = ch["y"], ch["x"]
                    if y < len(self.rows) and x < len(self.rows[y]):
                        row = self.rows[y]
                        self.rows[y] = row[:x] + ch["cell"]["c"] + row[x + 1 :]
        return msgs

    def last_row_with(self, text):
        """The LAST row carrying `text`.

        Last, not first: the shell echoes the command that produces the marker,
        so the echo row and the output row both match, and it is the program's
        own output that should be selected.
        """
        return max((y for y, r in enumerate(self.rows) if text in r), default=None)


def run(srv, result):
    c = Client(srv.sock)
    c.hello()
    c.send({"CreateSession": {"name": "main", "folder": None}})
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": 100, "rows": 30}})
    grid = Grid()
    grid.pump(c, 0.6)

    pane_id = first_pane_id(c)
    check(pane_id is not None, "a pane id was resolved off the session tree")
    if pane_id is None:
        return

    # 4. A CLIPBOARD selection, FIRST -- before anything below moves the focus.
    #    `CliSpawn` creates a pane and a tab, and `Input` goes to whatever is
    #    focused, so running this last typed the marker into a pane that was no
    #    longer on screen and the row scan found nothing.
    #    `CopyToClipboard { data }` carries a selection
    #    the user made, which is content by the same rule -- and a selection is
    #    exactly where a pasted password ends up. Asserted rather than reasoned
    #    about, so that if a future `{:?}` is ever added to that path it goes
    #    red here instead of shipping.
    c.send({"Input": {"data": list(f"printf '%s-%s\\n' {CLIP_HEAD} {CLIP_TAIL}\n".encode())}})
    grid.pump(c, 1.2)
    marker_y = grid.last_row_with(CLIP_SECRET)
    result["marker_row"] = marker_y
    if marker_y is not None:
        # Click, then a non-final drag to set the anchor, then the final drag --
        # the gesture a real mouse makes. A lone final drag establishes no
        # anchor and yanks nothing.
        c.send({"MouseClick": {"x": 1, "y": marker_y}})
        grid.pump(c, 0.2)
        c.send({"MouseDrag": {"start_x": 1, "start_y": marker_y,
                              "end_x": 40, "end_y": marker_y, "is_final": False}})
        grid.pump(c, 0.2)
        c.send({"MouseDrag": {"start_x": 1, "start_y": marker_y,
                              "end_x": 40, "end_y": marker_y, "is_final": True}})
    # Kept, because it is the ONLY positive proof this case has: the clipboard
    # path logs nothing about the data and never did, so there is no redacted
    # form to look for. Without evidence that the server really did extract and
    # send the selection, "the marker is in neither log" is satisfied by a drag
    # that selected nothing at all.
    yanked = [
        m["CopyToClipboard"]["data"]
        for m in grid.pump(c, 0.8)
        if name_of(m) == "CopyToClipboard"
    ]
    result["yanked"] = any(CLIP_SECRET in d for d in yanked)

    # 1. A keystroke into a view cell. No trailing newline: nothing is RUN, the
    #    bytes just reach the PTY -- which is exactly the shape of typing a
    #    passphrase into a cell and is one fewer way for the string to escape
    #    into somewhere this test is not looking.
    c.send({"InputToPane": {"pane_id": pane_id, "data": list(TYPED_SECRET)}})
    grid.pump(c, 0.6)

    # 2. A command line off `remux split`. A fresh unattached connection, as the
    #    real CLI uses.
    cli = Client(srv.sock)
    cli.hello()
    cli.send(
        {
            "CliSpawn": {
                "session": "main",
                "placement": "SplitBelow",
                "argv": ["true", ARGV_SECRET],
                "cwd": None,
            }
        }
    )
    cli.drain(1.2)
    # ...and the OTHER placement, because `NewTab` and the split paths reach the
    # server's pane-spawning code through DIFFERENT functions. Auditing one arm
    # and assuming the other is the same is how three of the five sites this
    # test found survived the first pass.
    cli.send(
        {
            "CliSpawn": {
                "session": "main",
                "placement": "NewTab",
                "argv": ["true", ARGV_SECRET],
                "cwd": None,
            }
        }
    )
    cli.drain(1.2)
    cli.close()

    # 5. `ListDirectory`, which the `files` panel repeats every 2s FOR EVER.
    #    Ten identical polls, then one for a different path.
    for _ in range(10):
        c.send({"ListDirectory": {"path": "/tmp"}})
        c.drain(0.1)
    c.send({"ListDirectory": {"path": "/usr"}})
    c.drain(0.6)
    c.close()


def run_cli(srv):
    """3. The CLIENT half: `remux split -- ...` writes its own `client.log`.

    Run as a real subprocess against the isolated server. `$REMUX_SESSION` is
    injected rather than inherited from a live pane -- proving the variable is
    REAL is `tests/pty/cli_split.py`'s job, and all this needs is the code path
    that logs the command line.
    """
    env = {**srv.env, "REMUX_SESSION": "main"}
    subprocess.run(
        [BIN, "split", "--below", "--", "true", ARGV_SECRET],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=20,
    )


def main():
    # Findings that come from the WIRE rather than from the log, carried out of
    # `run` so the checks can be made after the server is stopped.
    result = {}
    srv = Server(RUNDIR).start()
    try:
        try:
            run(srv, result)
            run_cli(srv)
        except Exception as e:  # noqa: BLE001 -- the log is the point; reach it
            check(False, f"the run completed without an exception ({e!r})")
    finally:
        log = srv.log()
        client_log_path = f"{srv.rundir}/state/remux/client.log"
        client_log = (
            open(client_log_path).read() if os.path.exists(client_log_path) else ""
        )
        srv.kill()

    check("panicked at" not in log, "no panic in the server log")

    # -- the absence half ----------------------------------------------------
    typed = TYPED_SECRET.decode()
    # The BYTE LIST first, because that is the form the leak actually took:
    # `{other:?}` prints a `Vec<u8>` as `[99, 111, 114, ...]`, full fidelity and
    # trivially decoded, while the plain string never appears. A test that only
    # searched for the string PASSED against the leaking server -- which is the
    # branch's own rule about a surplus pass being evidence, in miniature.
    check(
        str(list(TYPED_SECRET)[:6])[:-1] not in log,
        "1 a keystroke sent with InputToPane is NOT in the log as a Debug byte list",
    )
    check(
        typed not in log,
        "1 nor, obviously, as the text itself",
    )
    check(
        ARGV_SECRET not in log,
        "2 an argument passed to CliSpawn is NOT in the server log",
    )

    # -- the presence half ---------------------------------------------------
    #
    # Without these, every check above is satisfied by a server that logged
    # nothing whatsoever -- the same trap as counting log lines to prove a
    # dedup. The line has to stay useful: it is how "why did nothing reach my
    # view cell" is answered at all.
    check(
        "msg=InputToPane" in log,
        "1 but the message IS still logged, by name",
    )
    check(
        f"{len(TYPED_SECRET)} bytes" in log,
        f"1 with its length ({len(TYPED_SECRET)} bytes), which is the diagnostic",
    )
    check(
        "CliSpawn" in log and '"true"' in log,
        "2 and CliSpawn still names the PROGRAM it was asked to run",
    )

    # 3. `client.log`, which is the same kind of file and gets the same rule.
    check(
        client_log != "",
        "3 the CLI wrote a client.log at all (otherwise 3 proves nothing)",
    )
    check(
        ARGV_SECRET not in client_log,
        "3 and `remux split`'s own log does NOT carry the argument either",
    )
    check(
        "cli-spawn" in client_log and '"true"' in client_log,
        "3 while still naming the program it asked for",
    )

    # 4. The clipboard. Both logs: the server extracts the selection, the client
    #    receives it and shells it out over OSC 52.
    check(
        CLIP_SECRET not in log and CLIP_SECRET not in client_log,
        "4 a yanked selection is in NEITHER log",
    )
    # The presence guard for 4 is different in kind: there is no redacted form
    # to look for, because this path logs NOTHING about the data and never did.
    # So what has to be shown is that the yank really happened -- otherwise the
    # check above passes on a drag that selected nothing at all.
    check(
        result.get("yanked") is True,
        f"4 and the server really DID send that selection (row={result.get('marker_row')})",
    )

    # 5. ListDirectory: logged on CHANGE, not on every 2s poll.
    lists = [ln for ln in log.splitlines() if "msg=ListDirectory" in ln]
    tmp = [ln for ln in lists if '"/tmp"' in ln]
    usr = [ln for ln in lists if '"/usr"' in ln]
    check(
        len(tmp) == 1,
        f"5 ten identical ListDirectory polls log ONE line (got {len(tmp)})",
    )
    check(
        len(usr) == 1,
        f"5 and a CHANGE of path still logs its own line (got {len(usr)})",
    )
    check(
        '"/tmp"' in "".join(lists),
        "5 with the path kept, which is the diagnostic",
    )

    print()
    if FAILURES:
        print(f"FAILED ({len(FAILURES)}): " + "; ".join(FAILURES))
        return 1
    print("log_redaction: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
