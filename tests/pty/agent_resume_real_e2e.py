#!/usr/bin/env python3
"""A REAL, logged-in agent resumed by `remux restart`, end to end. OPT-IN:
nothing runs unless REMUX_REAL_E2E=1, and it is never part of a default sweep,
because it sends the agent one prompt (a real model call on the user's
account).

    REMUX_REAL_E2E=1 python3 tests/pty/agent_resume_real_e2e.py claude|codex

For the agent named, under a throwaway HOME, XDG dirs and agent config dir:

  1. A fresh 0700 directory gets ONE file copied from the real config, the
     credential, at 0600: `~/.claude/.credentials.json` for claude (Linux
     only, see below) or `~/.codex/auth.json` for codex. Nothing else is
     read from the real config, and nothing under it is ever written. The copy
     is removed on every exit path, and the run checks it is gone.
  2. `remux integrate <agent>-resume` installs the hook into that directory
     (CLAUDE_CONFIG_DIR / CODEX_HOME), with onboarding and folder trust
     pre-seeded there.
  3. The real agent starts at a shell pane's prompt, from a work dir, and is
     sent "reply with the word OK". Its SessionStart hook must be Stored.
  4. `remux restart`. The restored pane's shell must be typed the resume line
     (` claude --resume=<id>` / ` codex resume -- <id>`, read off the screen
     as the shell echoes it), and the agent must reopen THE SAME conversation:
     the earlier prompt is on screen again, and the agent's own transcript for
     that id is the one holding it.
  5. The server must log that the agent in the pane's foreground confirmed
     the typed resume, and claude's hook must also report the SAME session
     again (codex runs no SessionStart for `codex resume`, so only the
     foreground confirms it). Either clears the restore's `pending` mark,
     which lives in memory, so a SECOND `remux restart` must type the resume
     again rather than drop the record as "typed last time and never reported
     again": the session survives two restarts.
     An account at its usage limit cannot reply; that is reported, and the
     rest still runs, since the session it opened holds the prompt.
  6. Every process under the throwaway HOME is ended and checked gone.

The attached client exits when its server restarts, so after each restart a
NEW client is attached on a new emulated screen: the conversation found on it
was drawn after the restart, never left over from before it. The agent's
full-screen UI paints over the shell's echo of the typed line before that
client attaches, so the typed line is proved by the resumed PROCESS's argv
(/proc/<pid>/cmdline, or `ps -o args=` on macOS), never by echoed text.

Diagnostics, off by default: E2E_PROBE_HOOK=1 adds a second codex
SessionStart hook to the throwaway hooks.json that records each payload's
SHAPE (keys, source, whether transcript_path is null, whether CODEX_THREAD_ID
matches), never a value; E2E_TURN_AFTER_RESUME=1 sends a second prompt when
no re-report arrives within 20 s of the resume.

It refuses to start when the copied access token has less than 30 minutes
left, or codex's `last_refresh` is more than 7 days old, so that the agent has
no reason to refresh it: a refresh rotates the refresh token and can sign the
user's real install out. Before the copy is removed, its digest is compared
with the original; a difference is reported as a refresh. The real config
files named in REAL_FILES are hashed before and after and must be unchanged
(the user's own running agents can rewrite them, so a change is reported with
the file's mtime).

macOS claude is refused unless REMUX_REAL_E2E_KEYCHAIN=1 is also set: under a
throwaway CLAUDE_CONFIG_DIR, Claude Code's secure storage reads a Keychain
service named for that dir and falls back to the plaintext file, but its write
path, and its device-key store, can create Keychain items. This harness never
touches the Keychain itself; whoever sets that variable snapshots the login
keychain before and after and deletes what the run created. On macOS the real
~/.claude/.credentials.json is not compared (the user's live claude rewrites
it); ~/.claude.json is compared instead.
"""
import atexit
import base64
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

import pexpect
import pyte

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import BIN, Tui  # noqa: E402

REAL_HOME = os.path.expanduser("~")
PROMPT = "reply with the word OK"
CONFIG = """
[general]
save_sessions = true
automatic_restore = true
"""
AGENTS = {
    "claude": {
        "credential": f"{REAL_HOME}/.claude/.credentials.json",
        "dir": "claude", "env": "CLAUDE_CONFIG_DIR", "file": ".credentials.json", "rereports": True,
        "real_files": [f"{REAL_HOME}/.claude/settings.json", f"{REAL_HOME}/.claude/settings.local.json",
                       f"{REAL_HOME}/.claude/.credentials.json"],
    },
    "codex": {
        "credential": f"{REAL_HOME}/.codex/auth.json",
        # Codex runs no SessionStart for `codex resume`, so only the
        # foreground confirms its resume.
        "dir": "codex", "env": "CODEX_HOME", "file": "auth.json", "rereports": False,
        "real_files": [f"{REAL_HOME}/.codex/config.toml", f"{REAL_HOME}/.codex/hooks.json",
                       f"{REAL_HOME}/.codex/auth.json"],
    },
}

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label, flush=True)
    if not cond:
        failures.append(label)


def note(text):
    print(f"E2E   {text}", flush=True)


def digest(path):
    try:
        with open(path, "rb") as f:
            return hashlib.sha256(f.read()).hexdigest()
    except OSError:
        return None


def jwt_exp(token):
    try:
        part = token.split(".")[1]
        part += "=" * (-len(part) % 4)
        return json.loads(base64.urlsafe_b64decode(part)).get("exp")
    except (IndexError, ValueError, AttributeError):
        return None


def refresh_risk(agent, path):
    """Why the credential at `path` could be refreshed during a short run, or
    None. Reads only expiry fields; prints no value."""
    with open(path) as f:
        doc = json.load(f)
    now = time.time()
    if agent == "claude":
        expires = (doc.get("claudeAiOauth") or {}).get("expiresAt")
        if not expires:
            return "the credential names no expiry"
        left = expires / 1000 - now
    else:
        exp = jwt_exp((doc.get("tokens") or {}).get("access_token", ""))
        if not exp:
            return "the access token names no expiry"
        left = exp - now
        last = doc.get("last_refresh")
        if isinstance(last, str):
            try:
                age = now - time.mktime(time.strptime(last[:19], "%Y-%m-%dT%H:%M:%S"))
            except ValueError:
                age = None
            if age is not None and age > 7 * 86400:
                return "codex last refreshed its token more than 7 days ago"
    return None if left > 30 * 60 else "the access token has less than 30 minutes left"


class Credential:
    """The one copied file, removed on every exit path."""

    def __init__(self, agent, spec):
        self.root = tempfile.mkdtemp(prefix="rmx-e2e-cred-", dir="/tmp")
        os.chmod(self.root, 0o700)
        self.dir = os.path.join(self.root, spec["dir"])
        os.makedirs(self.dir, mode=0o700)
        self.copy = os.path.join(self.dir, spec["file"])
        self.source = spec["credential"]
        self.original = digest(self.source)
        fd = os.open(self.copy, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with open(self.source, "rb") as src, os.fdopen(fd, "wb") as dst:
            shutil.copyfileobj(src, dst)
        self.gone = False
        atexit.register(self.remove)
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(sig, self._on_signal)

    def _on_signal(self, signum, _frame):
        self.remove()
        sys.exit(128 + signum)

    def remove(self):
        if self.gone:
            return
        self.gone = True
        refreshed = digest(self.copy) not in (None, self.original)
        if os.path.exists(self.copy):
            size = os.path.getsize(self.copy)
            with open(self.copy, "r+b") as f:
                f.write(b"\0" * size)
                f.flush()
                os.fsync(f.fileno())
        shutil.rmtree(self.root, ignore_errors=True)
        check(not refreshed, "the copied credential was never rewritten, so no refresh rotated the user's token")
        check(not os.path.exists(self.root), f"the throwaway credential dir is gone ({self.root})")


def processes_with_home(home):
    found = []
    if os.path.isdir("/proc"):
        for pid in filter(str.isdigit, os.listdir("/proc")):
            try:
                with open(f"/proc/{pid}/environ", "rb") as f:
                    env = f.read().split(b"\0")
            except OSError:
                continue
            if f"HOME={home}".encode() in env:
                found.append(int(pid))
    else:
        out = subprocess.run(["ps", "-axE", "-o", "pid=,command="], capture_output=True,
                             timeout=20).stdout.decode("utf-8", "replace")
        for line in out.splitlines():
            pid, _, rest = line.strip().partition(" ")
            if rest.startswith("ps -axE"):
                continue
            if pid.isdigit() and f" HOME={home} " in f" {rest} ":
                found.append(int(pid))
    return [p for p in found if p != os.getpid()]


def end_processes(home):
    left = processes_with_home(home)
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for pid in processes_with_home(home):
            try:
                os.kill(pid, sig)
            except OSError:
                pass
        time.sleep(1.0)
    check(processes_with_home(home) == [],
          f"nothing started under the throwaway HOME is left running ({len(left)} ended)")


def real_binary(agent):
    found = shutil.which(agent) or (f"{REAL_HOME}/.local/bin/{agent}" if agent == "claude" else None)
    return os.path.realpath(found) if found and os.path.exists(found) else None


def interpreter_dir(binary):
    """The directory of a `#!/usr/bin/env X` script's X, which the pane's PATH
    must hold for the script to start."""
    with open(binary, "rb") as f:
        first = f.readline(200)
    found = re.match(rb"#!\s*/usr/bin/env\s+(\S+)", first)
    if not found:
        return None
    path = shutil.which(found.group(1).decode())
    return os.path.dirname(os.path.realpath(path)) if path else None


class Run:
    def __init__(self, agent):
        self.agent = agent
        self.spec = AGENTS[agent]
        self.rundir = f"/tmp/rmx-e2e-{agent}"
        self.home = f"{self.rundir}/home"
        self.work = f"{self.home}/work"
        self.bin = f"{self.rundir}-bin"

    def screen(self):
        return self.t.rows_text()

    def show(self, label, rows=None):
        for row in (rows or self.screen()):
            if row.strip():
                note(f"{label} | {row.rstrip()[:130]}")

    def wait(self, predicate, timeout, step=0.3):
        end = time.time() + timeout
        while time.time() < end:
            self.t.pump(step)
            if predicate():
                return True
        return predicate()

    def on_screen(self, needle):
        return any(needle in row for row in self.screen())

    def log(self):
        return self.t.log("server")

    def reports(self, pane):
        return re.findall(rf'agent session: pane %{pane} "{self.agent}" (\w+) (\d+) bytes source=(\S+) -> (\w+)',
                          self.log())

    def record(self, pane):
        try:
            with open(f"{self.rundir}/data/remux/state.json") as f:
                return (json.load(f).get("agent_sessions") or {}).get(str(pane))
        except (OSError, ValueError):
            return None

    def setup(self, cred, binary):
        shutil.rmtree(self.bin, ignore_errors=True)
        os.makedirs(self.bin)
        os.symlink(binary, f"{self.bin}/{self.agent}")
        path = [self.bin]
        interp = interpreter_dir(binary)
        if interp:
            path.append(interp)
        path += ["/usr/bin", "/bin"]
        for var in list(os.environ):
            if var.startswith(("CLAUDE", "CODEX", "ANTHROPIC", "OPENAI", "REMUX_")):
                os.environ.pop(var)
        self.t = Tui(self.rundir, cols=150, rows=45, config=CONFIG,
                     extra_env={"PATH": ":".join(path), self.spec["env"]: cred.dir})
        self.t.start()
        for profile in (".profile", ".zprofile"):
            with open(f"{self.home}/{profile}", "w") as f:
                f.write(f'PATH="{":".join(path)}"; export PATH\n')
        os.makedirs(self.work)
        self.preseed(cred.dir)
        done = subprocess.run([BIN, "integrate", f"{self.agent}-resume", "--allow-dev-path"],
                              env=self.t.env, capture_output=True, timeout=30)
        check(done.returncode == 0, f"{self.agent}-resume installs into the throwaway config dir "
                                    f"(rc={done.returncode}, err={done.stderr.decode()[:200]!r})")
        if os.environ.get("E2E_PROBE_HOOK") == "1" and self.agent == "codex":
            self.add_probe(cred.dir)

    def add_probe(self, config_dir):
        """A second SessionStart hook recording the SHAPE of each payload
        codex sends: its keys, source, whether transcript_path is null, and
        whether CODEX_THREAD_ID equals session_id. No value is recorded."""
        self.probe = f"{self.rundir}/probe.jsonl"
        script = f"{self.rundir}/probe.py"
        with open(script, "w") as f:
            f.write("import json, os, sys\n"
                    "p = json.load(sys.stdin)\n"
                    f"open({self.probe!r}, 'a').write(json.dumps({{'keys': sorted(p), 'source': p.get('source'),"
                    " 'transcript_null': p.get('transcript_path') is None,"
                    " 'thread_matches': os.environ.get('CODEX_THREAD_ID') == p.get('session_id'),"
                    " 'thread_set': 'CODEX_THREAD_ID' in os.environ}) + '\\n')\n")
        path = f"{config_dir}/hooks.json"
        with open(path) as f:
            doc = json.load(f)
        doc["hooks"]["SessionStart"].append(
            {"hooks": [{"type": "command", "command": f"{sys.executable} {script} || true"}]})
        with open(path, "w") as f:
            json.dump(doc, f, indent=2)

    def preseed(self, config_dir):
        if self.agent == "claude":
            # Claude keys trust by the realpath, and macOS's /tmp is /private/tmp.
            trusted = {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}
            with open(f"{config_dir}/.claude.json", "w") as f:
                json.dump({"hasCompletedOnboarding": True, "theme": "dark", "numStartups": 1,
                           "projects": {p: trusted for p in {self.work, os.path.realpath(self.work)}}}, f)
        else:
            with open(f"{config_dir}/config.toml", "w") as f:
                # No update check: its dialog's default action runs
                # `npm install -g @openai/codex`, the user's global install.
                f.write('cli_auth_credentials_store = "file"\n'
                        'check_for_update_on_startup = false\n'
                        f'[projects."{self.work}"]\ntrust_level = "trusted"\n')

    def start_command(self):
        return {"claude": "claude --model haiku", "codex": "codex"}[self.agent]

    def answer_prompts(self):
        """Dismiss a first-run dialog the pre-seeding did not cover; True when
        a key was sent."""
        rows = "\n".join(self.screen())
        # Esc is the update dialog's skip key; Enter there would install.
        if re.search(r"Update available", rows):
            self.t.send("\x1b", 0.8)
            return True
        if re.search(r"t trust all", rows):
            self.t.send("t", 1.5)
            if re.search(r"esc close", "\n".join(self.screen())):
                self.t.send("\x1b", 1.0)
            return True
        # Codex's hook trust: "2" selects "Trust all and continue", and Enter
        # confirms only once that row is the selected one.
        if re.search(r"Hooks need review", rows):
            self.t.send("\r" if re.search(r"\u203a\s*2\. Trust all", rows) else "2", 1.5)
            return True
        for pattern, key in (
                             (r"Do you trust the files in this folder|trust this folder|Yes, proceed", "\r"),
                             (r"Press Enter to continue|press enter to continue", "\r"),
                             (r"Do you trust the contents of this directory", "\r"),
                             (r"Choose the text style|Dark mode", "\r")):
            if re.search(pattern, rows):
                self.t.send(key, 1.5)
                return True
        return False

    def ready(self):
        rows = "\n".join(self.screen())
        if re.search(r"(?i)needs? review|trust all|Do you trust|Press Enter to continue|Update available", rows):
            return False
        if self.agent == "claude":
            return bool(re.search(r"for shortcuts|shift\+tab to cycle|bypass permissions", rows))
        return bool(re.search(r"to send|Ctrl\+J|for shortcuts|context left|›", rows))

    def start_agent(self):
        check(self.wait(lambda: self.on_screen("$ "), 15), "the pane shows a shell prompt")
        self.t.send(f"cd {self.work}\r", 0.5)
        self.t.send(f"{self.start_command()}\r", 1.0)
        # A dialog can open after the composer first paints (codex's hook
        # review does), so ready means ready for 3 s in a row.
        end = time.time() + 90
        streak = 0
        while time.time() < end and streak < 3:
            self.t.pump(1.0)
            streak = 0 if self.answer_prompts() or not self.ready() else streak + 1
        ok = streak >= 3
        check(ok, f"the real {self.agent} started and is waiting for input")
        if not ok:
            self.show("start")
        return ok

    def send_prompt(self):
        self.t.send(PROMPT, 0.8)
        self.t.send("\r", 1.0)

        def replied():
            rows = self.screen()
            asked = [i for i, row in enumerate(rows) if PROMPT in row]
            return bool(asked) and any(re.search(r"\bOK\b", row) and PROMPT not in row
                                       for row in rows[asked[-1] + 1:])
        ok = self.wait(replied, 120, step=1.0)
        if not ok and re.search(r"usage limit", "\n".join(self.screen())):
            note(f"the {self.agent} account is at its usage limit, so it cannot reply; the session "
                 f"it opened still holds the prompt")
            return False
        check(ok, f"the real {self.agent} answered the prompt with OK")
        if not ok:
            self.show("reply")
        return ok

    def transcript_holds_prompt(self, cred, sid):
        """The agent's own transcript for `sid` holds the prompt."""
        if self.agent == "claude":
            root = f"{cred.dir}/projects"
            pattern = f"{sid}.jsonl"
        else:
            root = f"{cred.dir}/sessions"
            pattern = sid
        for dirpath, _, files in os.walk(root):
            for name in files:
                if pattern in name:
                    with open(os.path.join(dirpath, name), errors="replace") as f:
                        if PROMPT in f.read():
                            return os.path.relpath(os.path.join(dirpath, name), cred.dir)
        return None

    def resumed_argv(self, sid):
        """The argv of a process under the throwaway HOME whose arguments hold
        the resume line's words in order, or None."""
        words = self.resume_line(sid).split()[1:]
        for pid in processes_with_home(self.home):
            if os.path.isdir("/proc"):
                try:
                    with open(f"/proc/{pid}/cmdline", "rb") as f:
                        argv = [a.decode(errors="replace") for a in f.read().split(b"\0") if a]
                except OSError:
                    continue
            else:
                out = subprocess.run(["ps", "-o", "args=", "-p", str(pid)], capture_output=True,
                                     timeout=10).stdout.decode(errors="replace").split()
                argv = out
            for i in range(len(argv) - len(words) + 1):
                if argv[i:i + len(words)] == words and any(self.agent in a for a in argv[:i + 1]):
                    return argv
        return None

    def reattach(self):
        """A new client on a new emulated screen, as a user runs `remux` again:
        the attached client exits when its server is restarted. Nothing the
        old client drew can reach this screen."""
        old = self.t.child
        end = time.time() + 10
        while old.isalive() and time.time() < end:
            self.t.pump(0.2)
        check(not old.isalive(), "the attached client exited with the restarted server")
        self.t.screen = pyte.Screen(self.t.cols, self.t.rows)
        self.t.stream = pyte.ByteStream(self.t.screen)
        self.t.child = pexpect.spawn(BIN, [], env=self.t.env, dimensions=(self.t.rows, self.t.cols),
                                     encoding=None)
        self.t.pump(1.5)

    def resume_line(self, sid):
        return {"claude": f"claude --resume={sid}", "codex": f"codex resume -- {sid}"}[self.agent]

    def run(self, cred, binary):
        self.setup(cred, binary)
        if not self.start_agent():
            return
        pane = None

        def stored():
            nonlocal pane
            found = re.findall(rf'agent session: pane %(\d+) "{self.agent}" \w+ \d+ bytes source=\S+ -> Stored',
                               self.log())
            pane = found[-1] if found else None
            return pane is not None
        # Claude reports as its session starts; codex starts its thread, and
        # runs SessionStart, with the first prompt. Either way the record must
        # be there once the agent has answered.
        # Without a reply (an account out of quota) the session still exists,
        # holding the prompt, so the restore can still be shown.
        self.send_prompt()
        check(self.wait(stored, 30), f"the {self.agent} SessionStart hook was Stored ({self.reports(pane) if pane else []})")
        if pane is None:
            self.show("hook")
            note(f"server lines: {re.findall(r'agent session: .*', self.log())[-3:]}")
            return
        record = self.record(pane)
        sid = (record or {}).get("value")
        check(sid is not None and not (record or {}).get("pending"),
              f"state.json holds the session for pane %{pane} ({len(sid or '')} bytes, not pending)")
        if sid is None:
            return
        held = self.wait(lambda: self.transcript_holds_prompt(cred, sid) is not None, 20)
        check(held, f"the agent's transcript for that session holds the prompt "
                    f"({self.transcript_holds_prompt(cred, sid)})")
        before = len(self.reports(pane))

        done = subprocess.run([BIN, "restart"], env=self.t.env, capture_output=True, timeout=60)
        check(done.returncode == 0, f"remux restart exits 0 (rc={done.returncode})")
        self.reattach()
        line = self.resume_line(sid)
        # The shell echoes the typed line, and the agent's full-screen UI paints
        # over it before a new client can attach, so the screen is sampled fast
        # and the PROCESS the line started is the proof.
        echoed = self.wait(lambda: self.on_screen(line), 15, step=0.05)
        typed = self.wait(lambda: re.search(rf'typed resume for pane %{pane} agent="{self.agent}"',
                                            self.log()) is not None, 30)
        check(typed, f"the server typed the resume into pane %{pane}")
        found = []
        self.wait(lambda: found.append(self.resumed_argv(sid)) or found[-1] is not None, 30)
        argv = found[-1] if found else None
        check(argv is not None, f"a process under the throwaway HOME runs `{self.resume_line('<id>')}` "
                                f"with this id (argv {argv and [a.replace(sid, '<id>') for a in argv]})")
        note(f"the shell's echo of ` {self.resume_line('<id>')}` was {'' if echoed else 'NOT '}"
             f"caught on screen before the agent painted over it")
        reopened = self.wait(lambda: not self.answer_prompts() and self.on_screen(PROMPT), 90, step=1.0)
        check(reopened, f"the resumed {self.agent}, on a client attached after the restart, shows the "
                        f"earlier prompt: the same conversation")
        if not reopened:
            self.show("resumed")
        self.show("resumed", [row for row in self.screen() if PROMPT in row or re.search(r"\bOK\b", row)])
        # Whichever comes first clears the mark: an agent that re-reports
        # (claude) usually beats the foreground watch, which then finds
        # nothing pending and logs nothing.
        confirmed = rf'agent session: pane %{pane} "{self.agent}" typed resume confirmed by foreground'
        by_foreground = self.wait(lambda: re.search(confirmed, self.log()) is not None
                                  or (self.spec["rereports"] and len(self.reports(pane)) > before),
                                  60, step=1.0) and re.search(confirmed, self.log()) is not None
        if self.spec["rereports"]:
            note(f"the foreground watch {'confirmed it' if by_foreground else 'was beaten by the re-report'}")
        else:
            check(by_foreground, f"server.log: the {self.agent} in the foreground confirmed the typed resume")
        rereported = self.wait(lambda: not self.answer_prompts() and len(self.reports(pane)) > before,
                               (20 if os.environ.get("E2E_TURN_AFTER_RESUME") else 60)
                               if self.spec["rereports"] else 10, step=1.0)
        if not rereported and os.environ.get("E2E_TURN_AFTER_RESUME"):
            note("no re-report without a turn; sending a second prompt")
            self.t.send("reply with the word OK again", 0.8)
            self.t.send("\r", 1.0)
            rereported = self.wait(lambda: len(self.reports(pane)) > before, 90, step=1.0)
        latest = self.reports(pane)[-1] if self.reports(pane) else None
        if self.spec["rereports"]:
            check(rereported and latest is not None and latest[3] in ("Unchanged", "Stored"),
                  f"the resumed agent's hook reported the session again ({latest})")
        else:
            note(f"the resumed {self.agent} {'re-reported' if rereported else 'did not re-report'} "
                 f"(it is not expected to); the foreground confirmed it")
        # The restore marks the record pending in memory only, so state.json
        # cannot show the mark clearing. A second restart can: a record still
        # pending is dropped instead of typed again.
        typed_before = len(re.findall(rf'typed resume for pane %{pane} ', self.log()))
        done = subprocess.run([BIN, "restart"], env=self.t.env, capture_output=True, timeout=60)
        check(done.returncode == 0, f"a second remux restart exits 0 (rc={done.returncode})")
        self.reattach()
        again = self.wait(lambda: len(re.findall(rf'typed resume for pane %{pane} ', self.log())) > typed_before
                          or "typed last time and never reported again" in self.log(), 30)
        dropped = "typed last time and never reported again" in self.log()
        check(again and not dropped,
              f"a second restart types the resume again, so the session survived two restarts "
              f"(dropped as never confirmed: {dropped})")
        check(self.wait(lambda: self.resumed_argv(sid) is not None, 30),
              f"...and `{self.resume_line('<id>')}` runs again with the same id")
        record = self.record(pane)
        check(record is not None and record.get("value") == sid,
              f"state.json still names the same session for pane %{pane}")
        self.show("final", self.screen()[-14:])
        if getattr(self, "probe", None) and os.path.exists(self.probe):
            for line in open(self.probe):
                note(f"probe: {line.strip()}")
        check("panicked at" not in self.log(), "no panic in server.log")

    def close(self):
        try:
            self.t.send("\x03", 0.5)
            self.t.send("\x03", 0.5)
            self.t.kill()
        except Exception as e:  # noqa: BLE001
            note(f"closing the client: {e}")
        end_processes(self.home)


def main():
    if os.environ.get("REMUX_REAL_E2E") != "1":
        print("SKIP  agent_resume_real_e2e: set REMUX_REAL_E2E=1 to run it (a real model call)")
        return 0
    agents = sys.argv[1:] or ["claude", "codex"]
    for agent in agents:
        spec = AGENTS[agent]
        if agent == "claude" and sys.platform == "darwin":
            if os.environ.get("REMUX_REAL_E2E_KEYCHAIN") != "1":
                print("SKIP  claude on macOS: its secure storage can write Keychain items (see the docstring); "
                      "set REMUX_REAL_E2E_KEYCHAIN=1 to run it anyway")
                continue
            spec = dict(spec, real_files=[p for p in spec["real_files"] if not p.endswith("/.credentials.json")]
                        + [f"{REAL_HOME}/.claude.json"])
        binary = real_binary(agent)
        if not binary:
            print(f"SKIP  {agent}: no real binary")
            continue
        if not os.path.isfile(spec["credential"]):
            print(f"SKIP  {agent}: no credential FILE (keyring storage?), so nothing to copy")
            continue
        risk = refresh_risk(agent, spec["credential"])
        if risk:
            print(f"SKIP  {agent}: {risk}; a run could refresh and rotate the user's token")
            continue
        real = {p: digest(p) for p in spec["real_files"]}
        note(f"{agent}: {binary}")
        cred = Credential(agent, spec)
        run = Run(agent)
        try:
            run.run(cred, binary)
        finally:
            run.close()
            cred.remove()
        changed = [p for p in real if digest(p) != real[p]]
        for p in changed:
            note(f"{p} changed during the run (mtime {time.ctime(os.path.getmtime(p))})")
        check(changed == [], f"{agent}: the real config files are byte for byte as before "
                             f"({sum(1 for v in real.values() if v)} hashed)")
    print(f"\n{len(failures)} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
