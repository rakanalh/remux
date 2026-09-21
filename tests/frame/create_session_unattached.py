#!/usr/bin/env python3
"""A pane created while NOBODY is attached: is its output there on attach?

Field report: a pane created with no client attached shows nothing until the
user presses a key. `handle_create_session` spawns the PTY at the creating
client's size (80x24 for a client that has never sent `Resize`), the shell
writes its prompt into that `Screen` while there is no viewer, and the question
this probe answers is whether that prompt survives to the first frame the
attaching client is sent.

Two cases, each on its own throwaway server:

  unattached  CreateSession, wait --wait seconds, THEN Attach and Resize.
  control     CreateSession immediately followed by Attach, no wait.

Three booleans are reported for each, because one of them can lie:

  output-before-resize  the frame Attach itself produced, at the pane's spawn
                        size. This is the honest read: no SIGWINCH has been
                        delivered, so nothing can have re-prompted.
  output-before-input   after the Resize to 100x30. NOT trustworthy alone --
                        the resize delivers SIGWINCH and zsh reprints its
                        prompt on one (bash does not), so a zsh run can show
                        content here that the shell redrew rather than content
                        the server kept.
  output-after-input    only measured when the two above are false: a newline
                        is sent and the screen re-read. True here with the two
                        above false IS the field signature.

"Content" is a non-blank glyph in the INTERIOR of `focused_pane_rect`, shrunk
by one cell on every side. The default border style is ZellijStyle, which draws
a full box plus a title in the top border row, so "any non-space glyph outside
the status bar" is true of an empty pane and would pass on a server that
rendered no shell output at all.

The server log is FOUND, not assumed: `--log`, else any `server.log` under the
rundir, else the `$HOME/.local/state/remux/server.log` that a macOS server falls
back to because `dirs::state_dir()` is None there. That last one is shared with
the user's real remux, so only the tail appended during this run is read, and a
warning names it.

`--churn N` recreates the field sequence before the unattached case: N sessions
are created, attached, waited for a prompt, then closed -- by typing `exit`
(`--churn-mode exit`, the shell releases the descriptors) or by the daemon's own
close path (`--churn-mode close`). The fd timeline is printed afterwards on PASS
as well as FAIL, so a run says whether the probe pane inherited the descriptors
a churned pane gave back. The field pane held 38/39, released by a pane that had
exited 40 seconds earlier; `--wait 40` matches that gap.

`$HOME` is left alone, so the shell under test sources the real user's rc files
and its output is whatever they print -- with a plugin manager that bootstraps
itself, the first "content" seen is that bootstrap rather than a prompt, which
still answers the question being asked.

Self-contained on purpose: it imports nothing from this repo, so it can be
copied to a machine that has only the remux binary and python3. There it needs
`--protocol N`, since `PROTOCOL_VERSION` is read out of `src/protocol.rs` and a
hard-coded copy goes stale silently (the server LOGS a skew and proceeds).

Run: python3 tests/frame/create_session_unattached.py
     python3 tests/frame/create_session_unattached.py --shell /bin/zsh
     python3 tests/frame/create_session_unattached.py --bin /usr/local/bin/remux --protocol 11
"""
import argparse
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import time

COLS, ROWS = 100, 30
SPAWN_COLS, SPAWN_ROWS = 80, 24  # what the server gives a client that never resized


# ---------------------------------------------------------------------------
# wire plumbing
# ---------------------------------------------------------------------------


def read_protocol_version(explicit):
    """`--protocol` wins; otherwise read the constant out of the source tree."""
    if explicit is not None:
        return explicit
    src = os.path.join(
        os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
        "src",
        "protocol.rs",
    )
    if not os.path.exists(src):
        raise SystemExit(f"{src} not found -- pass --protocol N to say what to speak")
    for line in open(src):
        if "pub const PROTOCOL_VERSION" in line:
            return int(line.split("=")[1].strip().rstrip(";"))
    raise SystemExit(f"no PROTOCOL_VERSION in {src} -- pass --protocol N")


def home_fallback_log():
    """Where `main.rs` puts the log when `dirs::state_dir()` gives nothing.

    `dirs::state_dir()` is `$XDG_STATE_HOME` on Linux but **None on macOS**, and
    `main.rs` then falls back to `$HOME/.local/state` -- the user's REAL home,
    outside any isolated rundir. That is why both Macs reported a log with no
    CreateSession in it while every substantive boolean was True: the log was
    written, just not where the harness could isolate it.
    """
    home = os.path.expanduser("~")
    return os.path.join(home, ".local", "state", "remux", "server.log")


class Server:
    def __init__(self, binary, rundir, shell, log_override=None):
        self.binary = binary
        self.rundir = rundir
        self.shell = shell
        self.sock = f"{rundir}/run/remux.sock"
        self.stderr_path = f"{rundir}/server.stderr"
        self.log_override = log_override
        self.fallback = home_fallback_log()
        # Byte offset of the shared fallback BEFORE this server ran. That file
        # belongs to the user's real remux too, so only what was appended after
        # this point may be attributed to this run.
        self.fallback_offset = 0
        self.log_is_shared = False
        self.proc = None
        self._stderr = None

    def start(self):
        if os.path.exists(self.fallback):
            self.fallback_offset = os.path.getsize(self.fallback)
        shutil.rmtree(self.rundir, ignore_errors=True)
        for s in ("run", "state", "data", "config"):
            os.makedirs(f"{self.rundir}/{s}", exist_ok=True)
        env = {
            **os.environ,
            "XDG_RUNTIME_DIR": f"{self.rundir}/run",
            "XDG_STATE_HOME": f"{self.rundir}/state",
            "XDG_DATA_HOME": f"{self.rundir}/data",
            "XDG_CONFIG_HOME": f"{self.rundir}/config",
            "SHELL": self.shell,
            "ENV": "/dev/null",
            "TERM": "xterm-256color",
        }
        self._stderr = open(self.stderr_path, "wb")
        self.proc = subprocess.Popen(
            [self.binary, "server"],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=self._stderr,
        )
        for _ in range(200):
            if os.path.exists(self.sock):
                time.sleep(0.3)
                return self
            time.sleep(0.05)
        self.kill()
        raise SystemExit(f"no socket at {self.sock} after 10s")

    def kill(self):
        if self.proc:
            self.proc.kill()
            self.proc.wait()
            self.proc = None
        if self._stderr:
            self._stderr.close()
            self._stderr = None

    def log_path(self):
        """Where the server's log actually landed.

        NOT `{rundir}/state/remux/server.log` assumed. Three sources, in order:
        an explicit `--log`, the first `server.log` anywhere under the throwaway
        rundir, and only then the un-isolated `$HOME/.local/state` fallback that
        a macOS server uses -- and that one only if it GREW while this server
        ran, since the user's own remux writes to the same file."""
        if self.log_override:
            return self.log_override
        for root, _dirs, files in os.walk(self.rundir):
            if "server.log" in files:
                return os.path.join(root, "server.log")
        if os.path.exists(self.fallback) and os.path.getsize(self.fallback) > self.fallback_offset:
            self.log_is_shared = True
            return self.fallback
        return None

    def all_logs(self):
        """Every *.log under the rundir, for the failure message when no
        server.log is found -- the list is the diagnostic."""
        out = []
        for root, _dirs, files in os.walk(self.rundir):
            for f in files:
                if f.endswith(".log"):
                    out.append(os.path.join(root, f))
        return sorted(out)

    def log(self):
        p = self.log_path()
        if not p or not os.path.exists(p):
            return ""
        with open(p, errors="replace") as f:
            if p == self.fallback and not self.log_override:
                # Read only this run's tail out of the shared file: older lines
                # are another server's, and both the panic search and the fd
                # timeline would otherwise report someone else's session.
                f.seek(self.fallback_offset)
            return f.read()

    def stderr(self):
        p = self.stderr_path
        return open(p, errors="replace").read() if os.path.exists(p) else ""


class Client:
    def __init__(self, sock, protocol):
        self.protocol = protocol
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(sock)
        self.s.settimeout(1.5)
        self.buf = b""

    def send(self, obj):
        b = json.dumps(obj).encode()
        self.s.sendall(struct.pack(">I", len(b)) + b)

    def _fill(self):
        chunk = self.s.recv(65536)
        if not chunk:
            raise ConnectionError("server closed the connection")
        self.buf += chunk

    def recv(self):
        while len(self.buf) < 4:
            self._fill()
        n = struct.unpack(">I", self.buf[:4])[0]
        while len(self.buf) < 4 + n:
            self._fill()
        body, self.buf = self.buf[4 : 4 + n], self.buf[4 + n :]
        return json.loads(body)

    def drain(self, t=0.8):
        """Everything arriving within t seconds. A closed peer RAISES: returning
        [] would make every "nothing arrived" read pass once the server hung up."""
        out = []
        end = time.time() + t
        old = self.s.gettimeout()
        try:
            while time.time() < end:
                self.s.settimeout(0.1)
                try:
                    out.append(self.recv())
                except socket.timeout:
                    pass
        finally:
            self.s.settimeout(old)
        return out

    def hello(self):
        self.send({"protocol_version": self.protocol, "remux_version": "probe"})
        return self.recv()

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


def name_of(msg):
    if isinstance(msg, str):
        return msg
    if isinstance(msg, dict):
        return next(iter(msg.keys()))
    return None


def body_of(msg):
    n = name_of(msg)
    return msg[n] if isinstance(msg, dict) else None


# ---------------------------------------------------------------------------
# grid reconstruction
# ---------------------------------------------------------------------------

BLANK = (" ", "", "\u0000", " ")


class Grid:
    """The client's screen, rebuilt from the render stream.

    Re-sized by every FullRender rather than pinned at 100x30: the attaching
    client is still at its 80x24 default when Attach answers, so the first frame
    is that size and a fixed grid would put the status row in the wrong place.
    """

    def __init__(self):
        self.cols = 0
        self.rows = 0
        self.g = []
        self.pane_rect = None
        self.frames = 0

    def _resize(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]

    def _put(self, y, x, cell):
        if 0 <= y < self.rows and 0 <= x < self.cols:
            self.g[y][x] = cell.get("c", " ") if isinstance(cell, dict) else " "

    def apply(self, msg):
        n, body = name_of(msg), body_of(msg)
        if n == "FullRender":
            cells = body["cells"]
            self._resize(len(cells[0]) if cells else 0, len(cells))
            for y, row in enumerate(cells):
                for x, cell in enumerate(row):
                    self._put(y, x, cell)
        elif n == "RenderDiff":
            for ch in body["changes"]:
                self._put(ch["y"], ch["x"], ch["cell"])
        elif n == "ScrollRender":
            # Content delivered as "shift the rect, here are the new rows".
            # A diff-only reader misses it entirely.
            self._apply_scroll(body)
        else:
            return
        self.frames += 1
        rect = body.get("focused_pane_rect") if isinstance(body, dict) else None
        if rect:
            self.pane_rect = rect

    def _apply_scroll(self, body):
        x0, y0 = body["pane_x"], body["pane_y"]
        w, h = body["pane_width"], body["pane_height"]
        delta, new_rows = body["delta"], body["new_rows"]
        n = abs(delta)
        if delta > 0:
            for y in range(y0, y0 + h - n):
                for x in range(x0, x0 + w):
                    if 0 <= y + n < self.rows and 0 <= x < self.cols:
                        self.g[y][x] = self.g[y + n][x]
            base = y0 + h - n
        else:
            for y in range(y0 + h - 1, y0 + n - 1, -1):
                for x in range(x0, x0 + w):
                    if 0 <= y - n < self.rows and 0 <= x < self.cols:
                        self.g[y][x] = self.g[y - n][x]
            base = y0
        for i, row in enumerate(new_rows):
            for x, cell in enumerate(row):
                self._put(base + i, x0 + x, cell)

    def interior(self):
        """(rows_of_text, description) for the pane's CONTENT area.

        `focused_pane_rect` is ALREADY the content rect: the server subtracts
        the border offsets before putting it on the wire (`daemon.rs`, "content
        area, excluding borders"). So it is used as given -- shrinking it by one
        more cell drops the pane's first row and first column, which is exactly
        where a shell's first prompt sits.
        """
        if self.pane_rect:
            r = self.pane_rect
            y0, y1 = r["y"], r["y"] + r["height"]
            x0, x1 = r["x"], r["x"] + r["width"]
            where = f"focused_pane_rect {r['x']},{r['y']} {r['width']}x{r['height']}"
        else:
            y0, y1 = 1, self.rows - 1  # last row is the status bar
            x0, x1 = 1, self.cols - 1
            where = "fallback: whole grid minus outer ring and status row"
        out = []
        for y in range(max(0, y0), min(self.rows, y1)):
            out.append("".join(self.g[y][max(0, x0) : min(self.cols, x1)]))
        return out, where

    def has_content(self):
        rows, _ = self.interior()
        return any(any(c not in BLANK for c in row) for row in rows)

    def nonblank_rows(self, limit=5):
        rows, _ = self.interior()
        hits = [(i, r.rstrip()) for i, r in enumerate(rows) if r.strip()]
        return hits[:limit]

    def status_row(self):
        return "".join(self.g[self.rows - 1]) if self.rows else ""


# ---------------------------------------------------------------------------
# the cases
# ---------------------------------------------------------------------------


def report(label, grid, phase):
    rows, where = grid.interior()
    hits = grid.nonblank_rows()
    print(f"  [{label}] {phase}: {len(hits)} non-blank interior rows ({where})")
    for i, r in hits:
        print(f"      row {i}: {r[:90]!r}")


def session_names(tree):
    """Every session name a `SessionTree` mentions, live or dormant."""
    out = [e.get("name") for e in tree.get("unfiled", [])]
    for f in tree.get("folders", []):
        out += [s.get("name") for s in f.get("sessions", [])]
    return set(out) | set(tree.get("dormant", []))


def churn_cycle(label, cli, i, mode):
    """Create a session, watch its prompt appear, then close it and wait for the
    session to go away.

    This recreates the field sequence rather than a fresh server: the pane in
    the report was handed fd numbers a pane closed 40 seconds earlier had
    released, so the descriptors the probe pane gets have to be second-hand for
    the run to be about the same thing. `exit` releases them through the SHELL
    (the child exits and the daemon reaps), `PaneClose` through the daemon's own
    close path; the two are different code and both are worth churning.
    """
    name = f"churn{i}"
    cli.send({"CreateSession": {"name": name, "folder": None}})
    cli.send({"Attach": {"session_name": name}})
    # Deliberately NO Resize: it would pin this client at 100x30, and the probe
    # pane would then spawn at the size it already announced, quietly removing
    # the SIGWINCH the unattached case is built to reason about.
    grid = Grid()
    end = time.time() + 5.0
    while time.time() < end:
        for m in cli.drain(0.3):
            grid.apply(m)
        if grid.has_content():
            break
    prompt = grid.has_content()

    if mode == "close":
        cli.send({"Command": "PaneClose"})
    else:
        cli.send({"Input": {"data": list(b"exit\n")}})

    gone = False
    end = time.time() + 5.0
    while time.time() < end and not gone:
        cli.send("ListSessionTree")  # unit ClientMessage -> a bare JSON string
        for m in cli.drain(0.3):
            n, body = name_of(m), body_of(m)
            if n == "SessionTree" and name not in session_names(body):
                gone = True
            elif n == "Event" and body == {"SessionDeleted": name}:
                gone = True
    note = ""
    if prompt and not gone and mode == "exit":
        # "Content on screen" is not "the shell is reading input". A zsh whose rc
        # bootstraps a plugin manager prints for seconds before it reads a key,
        # so the typed `exit` waits in the tty buffer and the session outlives
        # the 5s window. `--churn-mode close` does not depend on the shell.
        note = "  (shell had not started reading input; try --churn-mode close)"
    print(f"  [{label}] churn {name} ({mode}): prompt={prompt} closed={gone}{note}")
    return prompt and gone


def run_case(label, args, protocol, rundir, wait, churn=0, churn_mode="exit"):
    """Returns (passed, result dict)."""
    srv = Server(args.bin, rundir, args.shell, args.log).start()
    cli = None
    result = {
        "before_resize": False,
        "before_input": False,
        "after_input": None,
        "skew": None,
        "alive": True,
        "churn_ok": True,
        "log_shared": False,
    }
    try:
        cli = Client(srv.sock, protocol)
        welcome = cli.hello()
        result["skew"] = welcome.get("protocol_version")

        for i in range(churn):
            if not churn_cycle(label, cli, i, churn_mode):
                result["churn_ok"] = False
        if churn:
            # The field gap between the releasing close and the create was tens
            # of seconds; --wait stands in for it on both sides of the create.
            time.sleep(wait)

        cli.send({"CreateSession": {"name": "probe", "folder": None}})
        cli.drain(0.4)  # the SessionCreated Event; no Attach, so no frames

        if wait:
            print(f"  [{label}] created, waiting {wait}s with nobody attached")
            time.sleep(wait)

        grid = Grid()

        # Phase 1: the frame Attach itself produces, at the pane's SPAWN size.
        # No SIGWINCH has been delivered yet, so nothing on screen can have been
        # redrawn by the shell in response to one.
        cli.send({"Attach": {"session_name": "probe"}})
        end = time.time() + 3.0
        while time.time() < end:
            for m in cli.drain(0.4):
                grid.apply(m)
            if grid.has_content():
                break
        result["before_resize"] = grid.has_content()
        report(label, grid, f"after Attach (grid {grid.cols}x{grid.rows})")

        # Phase 2: after the client announces its real size.
        cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
        end = time.time() + 3.0
        while time.time() < end:
            msgs = cli.drain(0.4)
            for m in msgs:
                grid.apply(m)
            if grid.has_content():
                break
        result["before_input"] = grid.has_content()
        report(label, grid, f"after Resize {COLS}x{ROWS} (grid {grid.cols}x{grid.rows})")

        # Phase 3: only if nothing is on screen -- does typing conjure it?
        if not result["before_input"]:
            cli.send({"Input": {"data": list(b"\n")}})
            end = time.time() + 2.0
            while time.time() < end:
                for m in cli.drain(0.4):
                    grid.apply(m)
                if grid.has_content():
                    break
            result["after_input"] = grid.has_content()
            report(label, grid, "after Input(newline)")

        print(f"  [{label}] status row: {grid.status_row().strip()[:90]!r}")
        print(f"  [{label}] frames applied: {grid.frames}")
    except (ConnectionError, socket.timeout, OSError) as e:
        result["alive"] = False
        print(f"  [{label}] connection error: {e}")
    finally:
        if cli:
            cli.close()
        log = srv.log()
        stderr = srv.stderr()
        srv.kill()

    # `install_panic_logger` routes std::panic through the log crate, so a panic
    # reaches server.log opening with the literal "panicked at". It also reaches
    # the stderr file this harness keeps, so both are searched.
    result["panic"] = "panicked at" in log or "panicked at" in stderr
    # A positive control on the search itself: a grep of a file the evidence
    # cannot reach passes on an empty string for ever. The server logs this line
    # at Debug (and `main.rs` pins the logger at Debug), so its absence means the
    # log being searched is not the log this run wrote.
    result["log_marker"] = 'CreateSession name="probe"' in log
    result["log"] = srv.log_path()
    result["log_shared"] = srv.log_is_shared
    result["other_logs"] = srv.all_logs()
    result["fds"] = fd_table(log)
    passed = result["before_input"] and result["alive"] and not result["panic"]
    return passed, result


FD_RE = re.compile(r"start_(reader|writer) watching fd=(\d+)")
PANE_RE = re.compile(r"spawn_pane pane_id=(\d+)")


def fd_table(log):
    """[(pane_id, kind, fd, raw_line)] in log order.

    `spawn_pane pane_id=` precedes the `start_reader`/`start_writer` lines for
    that pane, so the most recent pane id names the descriptors that follow.
    """
    rows, pane = [], None
    for line in log.splitlines():
        m = PANE_RE.search(line)
        if m:
            pane = m.group(1)
            rows.append((pane, "spawn", None, line.strip()))
            continue
        m = FD_RE.search(line)
        if m:
            rows.append((pane, m.group(1), int(m.group(2)), line.strip()))
    return rows


def print_fd_report(label, rows):
    """Printed on PASS as well as FAIL: whether the probe pane inherited the
    descriptors of a pane that closed earlier is the question the churn exists
    to answer, and a green run is exactly when nobody would go looking."""
    print(f"  [{label}] fd timeline ({len(rows)} lines):")
    for pane, kind, fd, line in rows:
        tag = f"pane {pane}" if pane else "pane ?"
        print(f"      {tag:8} {kind:6} {'' if fd is None else 'fd=' + str(fd)}  | {line[:100]}")
    per_pane = {}
    for pane, kind, fd, _ in rows:
        if fd is not None:
            per_pane.setdefault(pane, []).append((kind, fd))
    panes = list(per_pane)
    if len(panes) < 2:
        print(f"  [{label}] fd reuse: only {len(panes)} pane(s) took descriptors, nothing to compare")
        return
    last = panes[-1]
    earlier = {fd: p for p in panes[:-1] for _, fd in per_pane[p]}
    hits = [(fd, earlier[fd]) for _, fd in per_pane[last] if fd in earlier]
    if hits:
        which = ", ".join(f"fd={fd} previously pane {p}" for fd, p in hits)
        print(f"  [{label}] fd reuse: pane {last} REUSED descriptors of closed panes -- {which}")
    else:
        print(
            f"  [{label}] fd reuse: NONE -- pane {last} took "
            f"{[fd for _, fd in per_pane[last]]}, none of which any earlier pane held"
        )


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--bin", default="target/debug/remux", help="remux binary")
    ap.add_argument(
        "--protocol",
        type=int,
        default=None,
        help="PROTOCOL_VERSION to announce; REQUIRED when src/protocol.rs is absent",
    )
    ap.add_argument("--shell", default="/bin/sh", help="$SHELL for the server")
    ap.add_argument("--rundir", default=None, help="throwaway dir (socket must stay < 100 chars)")
    ap.add_argument(
        "--log",
        default=None,
        help="read the server log from here instead of searching (macOS writes it "
        "outside the rundir: dirs::state_dir() is None there)",
    )
    ap.add_argument("--wait", type=float, default=3.0, help="seconds unattached before Attach")
    ap.add_argument(
        "--churn",
        type=int,
        default=0,
        help="create+close this many sessions BEFORE the unattached case, so the "
        "probe pane is handed second-hand descriptors",
    )
    ap.add_argument(
        "--churn-mode",
        choices=("exit", "close"),
        default="exit",
        help="how a churned pane releases its fds: 'exit' types exit into the "
        "shell, 'close' sends the PaneClose command",
    )
    args = ap.parse_args()

    args.bin = os.path.abspath(args.bin)
    if not os.path.exists(args.bin):
        raise SystemExit(f"no binary at {args.bin}")
    if not os.path.exists(args.shell):
        raise SystemExit(f"no shell at {args.shell}")

    protocol = read_protocol_version(args.protocol)

    rundir = args.rundir
    if rundir is None:
        # PID in the name: two concurrent runs of one harness under a shared
        # path corrupt each other, and the false red mimics the bug under test.
        base = os.environ.get("TMPDIR", "/tmp").rstrip("/")
        rundir = f"{base}/rmxcsu{os.getpid()}"
        if len(f"{rundir}/x/run/remux.sock") >= 100:
            rundir = f"/tmp/rmxcsu{os.getpid()}"
    rundir = os.path.abspath(rundir)
    if len(f"{rundir}/x/run/remux.sock") >= 100:
        raise SystemExit(f"--rundir too long for a unix socket: {rundir}")

    print(f"bin       {args.bin}")
    print(f"mtime     {time.ctime(os.path.getmtime(args.bin))}")
    print(f"protocol  {protocol}{' (--protocol)' if args.protocol is not None else ''}")
    print(f"shell     {args.shell}")
    print(f"rundir    {rundir}")
    print(f"wait      {args.wait}s")
    print(f"churn     {args.churn} cycle(s), mode {args.churn_mode}")
    print()

    churn_note = f", after {args.churn} churn cycle(s) via {args.churn_mode}" if args.churn else ""
    print(f"CASE unattached: CreateSession, wait {args.wait}s, then Attach{churn_note}")
    un_pass, un = run_case(
        "unattached", args, protocol, f"{rundir}/a", args.wait, args.churn, args.churn_mode
    )
    print()
    print("CASE control: CreateSession then Attach immediately")
    ct_pass, ct = run_case("control", args, protocol, f"{rundir}/b", 0)
    print()

    for label, r in (("unattached", un), ("control", ct)):
        print_fd_report(label, r["fds"])
    print()

    for label, r in (("unattached", un), ("control", ct)):
        print(
            f"RESULT[{label}]: output-before-resize={r['before_resize']} "
            f"output-before-input={r['before_input']} "
            f"output-after-input={r['after_input']}"
        )
    print(f"RESULT: output-before-input={un['before_input']} output-after-input={un['after_input']}")
    if not un["before_resize"] and not un["before_input"] and un["after_input"]:
        print("RESULT: FIELD SIGNATURE -- a pane created unattached is blank until a key is pressed")
    for label, r in (("unattached", un), ("control", ct)):
        if r["before_input"] and not r["before_resize"]:
            print(
                f"WARN[{label}]: nothing was on screen until the Resize. The resize delivers "
                "SIGWINCH and zsh reprints its prompt on one, so this PASS may be the shell "
                "redrawing rather than the server keeping the pre-attach output."
            )
    print()

    fails = []
    for label, r, ok in (("unattached", un, un_pass), ("control", ct, ct_pass)):
        mine = []
        if r["skew"] != protocol:
            mine.append(f"[{label}] server answered protocol {r['skew']}, we speak {protocol}")
        if r["panic"]:
            mine.append(f"[{label}] 'panicked at' in {r['log']} or the stderr beside it")
        if r["log_shared"]:
            print(
                f"WARN[{label}]: the log came from {r['log']}, which is NOT inside the "
                "rundir -- dirs::state_dir() is None on macOS, so the server logged to "
                "the real home. Only this run's tail was read; pass --log to override."
            )
        if r["log"] is None:
            mine.append(
                f"[{label}] no server.log under the rundir or at {home_fallback_log()}; "
                f"*.log found under the rundir: {r['other_logs'] or 'none at all'}"
            )
        elif not r["log_marker"]:
            mine.append(
                f"[{label}] {r['log']} does not mention CreateSession -- the panic search "
                "above had nothing to read, so it proves nothing"
            )
        if not r["alive"]:
            mine.append(f"[{label}] lost the connection")
        if not r["churn_ok"]:
            mine.append(f"[{label}] a churn cycle did not reach a prompt or did not close")
        if not ok:
            mine.append(f"[{label}] no pane content before any Input was sent")
        fails += mine
        print(f"{label}: {'PASS' if not mine else 'FAIL'}   log: {r['log']}")
    print()

    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
