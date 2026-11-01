#!/usr/bin/env python3
"""`remux integrate <agent>-resume`, in a throwaway tree: HOME, the XDG dirs and
every agent config dir point under RUNDIR, and the harness refuses to run
otherwise. No agent runs here; `tests/pty/agent_session_hooks.py` runs the
installed hooks.

Every case that writes diffs the WHOLE throwaway tree before and after, so a
write anywhere but the files the case names is a failure (success criterion
8: no install touches a path outside the resolved agent dir).

claude (the hook in `$CLAUDE_CONFIG_DIR/settings.json`):
  c1  a fresh config dir is Absent
  c2  install into a settings.json holding foreign hooks AND a claude-messaging
      install adds exactly one SessionStart group with the exact command and
      timeout and no matcher; the user's keys keep their order, the foreign
      SessionStart groups survive, and only settings.json changed
  c3  --status is Installed, naming the binary; claude-messaging in the same
      file still reads Installed
  c4  a second install changes no byte, inode or mtime
  c5  --dry-run on a fresh config dir prints the change and writes nothing
  c6  --uninstall removes only ours: the foreign SessionStart groups and the
      messaging hooks survive, messaging still reads Installed, and resume
      reads Absent
  c7  a hard-linked settings.json is refused and neither name is written
  c8  a FIFO at settings.json is refused at once, never opened for a read
  c9  a missing $CLAUDE_CONFIG_DIR is refused and nothing is created
  c10 a cargo build output is refused without --allow-dev-path
  c11 a wrapper around the command (remux-shaped, not remux's) is reported
      Stale, left byte-identical by install and uninstall, and no hook of ours
      is added beside it

codex (the hook in `$CODEX_HOME/hooks.json`, the flag in its config.toml):
  x1  a fresh CODEX_HOME is Absent
  x2  install into a commented config.toml with its own [features] keys adds
      `hooks = true` as one new line and leaves every other byte; hooks.json
      gains exactly one SessionStart group; only those two files changed
  x3  --status is Installed
  x4  a second install changes no byte, inode or mtime anywhere
  x5  --uninstall removes the hook, says it left `hooks = true`, and leaves
      config.toml byte for byte as the install made it
  x6  a config.toml with no [features] gains the table exactly once
  x7  --dry-run writes nothing
  x8  a missing CODEX_HOME is refused and nothing is created
  x9  a hooks.json the install must refuse (a second hard link) leaves
      config.toml untouched too: nothing is half-installed

pi and omp (the extension `extensions/remux-agent-session.ts`; pi at its
default `~/.pi/agent`, omp at `~/$PI_CONFIG_DIR/agent` with a custom name, and
`$PI_CODING_AGENT_DIR` unset, since both agents honour it):
  e1  a fresh agent dir is Absent
  e2  install writes the extension with this binary's path and the agent's
      name baked in, the header first, and nothing else in the tree changes;
      `node --check` accepts it
  e3  --status is Installed
  e4  a second install changes no byte, inode or mtime
  e5  an edited copy of ours is Stale, and install rewrites it to this build's
  e6  a file without remux's header at the path is refused by install and
      left byte-identical by install and uninstall
  e7  --uninstall removes ours and nothing else
  e8  a missing agent dir is refused and nothing is created
  e9  with $PI_CODING_AGENT_DIR set, pi and omp resolve to one dir: refused

opencode (a server plugin in `plugins/`, a TUI plugin at the dir's top level
listed in `tui.jsonc` / `tui.json`):
  q1  a fresh config dir is Absent
  q2  with a commented tui.jsonc and OpenCode's own files (package.json,
      .gitignore, node_modules/) present, install writes both plugins and adds
      one array entry; tui.jsonc keeps every other byte; OpenCode's files and
      the rest of the tree are untouched; `node --check` accepts both plugins
  q3  --status is Installed
  q4  a second install changes no byte, inode or mtime
  q5  --uninstall removes both plugins and the entry, and tui.jsonc is byte for
      byte what it was before the install
  q6  with no tui.json(c), install creates tui.json holding the schema and the
      one entry; with only tui.json, that file is the one edited
  q7  a file without remux's header at either plugin path is refused and
      nothing is written
  q8  a missing config dir is refused and nothing is created
  q9  an uninstall that cannot edit tui.jsonc (it no longer parses) removes
      neither plugin file, so the config never lists a file that is gone

Run: python3 tests/pty/integrate_agent_resume.py [claude codex pi omp opencode ...]
"""
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import BIN  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-integres")
FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "fixtures")
REAL = os.path.realpath(BIN)

FAILURES = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


def shq(text):
    return "'" + text.replace("'", "'\\''") + "'"


class Tree:
    """A throwaway HOME with its agent config dirs, and the environment that
    points every resolver at them."""

    def __init__(self, name):
        self.root = f"{RUNDIR}/{name}"
        shutil.rmtree(self.root, ignore_errors=True)
        shutil.rmtree(f"{self.root}-state", ignore_errors=True)
        self.home = f"{self.root}/home"
        os.makedirs(self.home)
        self.dirs = {
            "CLAUDE_CONFIG_DIR": f"{self.root}/claude",
            "CODEX_HOME": f"{self.root}/codex",
            "PI_CODING_AGENT_DIR": f"{self.root}/pi",
            "OPENCODE_CONFIG_DIR": f"{self.root}/opencode",
        }

    def env(self, drop=()):
        env = {k: v for k, v in os.environ.items() if k not in ("REMUX_SESSION", "REMUX_PANE")}
        # The state dir, where the CLI writes client.log, is throwaway but
        # outside the diffed tree: remux's own log is not an install's write.
        env.update(HOME=self.home, XDG_RUNTIME_DIR=f"{self.root}/run",
                   XDG_STATE_HOME=f"{self.root}-state", XDG_DATA_HOME=f"{self.root}/data",
                   XDG_CONFIG_HOME=f"{self.root}/xdgconfig", **self.dirs)
        for var in drop:
            env.pop(var, None)
        for var, value in env.items():
            # PI_CONFIG_DIR is a directory NAME under HOME, not a path.
            if (var in self.dirs and var != "PI_CONFIG_DIR") or var in ("HOME", "XDG_CONFIG_HOME"):
                assert value.startswith(RUNDIR + "/"), (var, value)
        return env

    def integrate(self, harness, *args, dev=True, drop=()):
        flags = ["--allow-dev-path"] if dev else []
        p = subprocess.run([BIN, "integrate", harness, *args, *flags], env=self.env(drop),
                           capture_output=True, timeout=20, cwd=self.home)
        return p.returncode, p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace")

    def snapshot(self):
        """Every path under the tree: its kind, and a regular file's bytes'
        hash, inode and mtime."""
        out = {}
        for dirpath, dirnames, filenames in os.walk(self.root):
            for name in dirnames + filenames:
                path = os.path.join(dirpath, name)
                st = os.lstat(path)
                if stat.S_ISREG(st.st_mode):
                    with open(path, "rb") as f:
                        digest = hashlib.sha256(f.read()).hexdigest()
                    out[path] = ("file", digest, st.st_ino, st.st_mtime_ns)
                else:
                    out[path] = (stat.S_IFMT(st.st_mode), None, st.st_ino, None)
        return out


def changed(before, after):
    return sorted(p for p in set(before) | set(after) if before.get(p) != after.get(p))


def groups(doc, event):
    return doc.get("hooks", {}).get(event, [])


def commands(doc, event):
    return [h.get("command") for g in groups(doc, event) for h in g.get("hooks", [])]


# -- claude --------------------------------------------------------------------

CLAUDE_RESUME = f"{shq(REAL)} agent session claude --hook || true"


def claude():
    h = "claude-resume"
    t = Tree("claude")
    d = t.dirs["CLAUDE_CONFIG_DIR"]
    settings = f"{d}/settings.json"
    os.makedirs(d)

    rc, out, _ = t.integrate(h, "--status")
    check(rc == 0 and "Absent" in out, f"c1 a fresh config dir is Absent (rc={rc}, out={out.strip()!r})")

    shutil.copy(f"{FIXTURES}/agent_session/claude_settings_foreign.json", settings)
    rc, _, err = t.integrate("claude-messaging")
    check(rc == 0, f"c2 setup: claude-messaging installs into the same file (rc={rc}, err={err.strip()!r})")
    seeded = json.load(open(settings))
    before = t.snapshot()
    rc, out, err = t.integrate(h)
    after = t.snapshot()
    doc = json.load(open(settings))
    ours = [g for g in groups(doc, "SessionStart")
            if [x.get("command") for x in g.get("hooks", [])] == [CLAUDE_RESUME]]
    check(rc == 0 and len(ours) == 1, f"c2 install adds one SessionStart group running ours "
                                      f"(rc={rc}, err={err.strip()!r}, groups={groups(doc, 'SessionStart')})")
    check(bool(ours) and ours[0] == {"hooks": [{"type": "command", "command": CLAUDE_RESUME,
                                                "timeout": 10}]},
          f"c2 ...with timeout 10 and no matcher (got {ours})")
    check(list(doc) == list(seeded), f"c2 the user's keys keep their order ({list(doc)})")
    check(groups(doc, "SessionStart")[:2] == groups(seeded, "SessionStart")
          and {k: v for k, v in doc["hooks"].items() if k != "SessionStart"}
          == {k: v for k, v in seeded["hooks"].items() if k != "SessionStart"},
          "c2 every foreign and messaging hook survives as it was")
    check(changed(before, after) == [settings], f"c2 only settings.json changed ({changed(before, after)})")

    rc, out, _ = t.integrate(h, "--status")
    check(rc == 0 and f"Installed (" in out and REAL in out, f"c3 --status is Installed ({out.strip()!r})")
    rc, out, _ = t.integrate("claude-messaging", "--status")
    check(rc == 0 and "Installed" in out, f"c3 claude-messaging still reads Installed ({out.strip()!r})")

    before = t.snapshot()
    rc, _, _ = t.integrate(h)
    check(rc == 0 and changed(before, t.snapshot()) == [],
          "c4 a second install changes no byte, inode or mtime anywhere")

    fresh = Tree("claude-dry")
    os.makedirs(fresh.dirs["CLAUDE_CONFIG_DIR"])
    before = fresh.snapshot()
    rc, out, _ = fresh.integrate(h, "--dry-run")
    check(rc == 0 and CLAUDE_RESUME.replace("'", "") in out.replace("'", "").replace("\\", "")
          and changed(before, fresh.snapshot()) == [],
          f"c5 --dry-run prints the change and writes nothing ({out.strip()[:120]!r})")

    before = t.snapshot()
    rc, _, err = t.integrate(h, "--uninstall")
    doc = json.load(open(settings))
    check(rc == 0 and CLAUDE_RESUME not in commands(doc, "SessionStart"),
          f"c6 uninstall removes ours (rc={rc}, err={err.strip()!r})")
    check(groups(doc, "SessionStart") == groups(seeded, "SessionStart")
          and {k: v for k, v in doc["hooks"].items() if k != "SessionStart"}
          == {k: v for k, v in seeded["hooks"].items() if k != "SessionStart"},
          "c6 ...and only ours: the foreign groups and the messaging hooks are as they were")
    check(changed(before, t.snapshot()) == [settings], "c6 ...and only settings.json changed")
    rc, out, _ = t.integrate("claude-messaging", "--status")
    check("Installed" in out, f"c6 claude-messaging still reads Installed ({out.strip()!r})")
    rc, out, _ = t.integrate(h, "--status")
    check("Absent" in out, f"c6 claude-resume reads Absent ({out.strip()!r})")

    linked = Tree("claude-link")
    ld = linked.dirs["CLAUDE_CONFIG_DIR"]
    os.makedirs(ld)
    with open(f"{ld}/settings.json", "w") as f:
        f.write("{}\n")
    os.link(f"{ld}/settings.json", f"{linked.root}/other-name.json")
    before = linked.snapshot()
    rc, _, err = linked.integrate(h)
    check(rc != 0 and "link" in err and changed(before, linked.snapshot()) == [],
          f"c7 a hard-linked settings.json is refused and nothing is written (rc={rc}, err={err.strip()!r})")

    fifo = Tree("claude-fifo")
    fd = fifo.dirs["CLAUDE_CONFIG_DIR"]
    os.makedirs(fd)
    os.mkfifo(f"{fd}/settings.json")
    before = fifo.snapshot()
    try:
        rc, _, err = fifo.integrate(h)
        hung = False
    except subprocess.TimeoutExpired:
        rc, err, hung = None, "", True
    check(not hung and rc != 0 and "regular file" in err and changed(before, fifo.snapshot()) == [],
          f"c8 a FIFO at settings.json is refused without blocking (rc={rc}, hung={hung}, err={err.strip()!r})")

    missing = Tree("claude-missing")
    before = missing.snapshot()
    rc, _, err = missing.integrate(h)
    check(rc != 0 and "does not exist" in err
          and not os.path.exists(missing.dirs["CLAUDE_CONFIG_DIR"])
          and changed(before, missing.snapshot()) == [],
          f"c9 a missing config dir is refused and nothing is created (rc={rc}, err={err.strip()!r})")

    dev = Tree("claude-dev")
    os.makedirs(dev.dirs["CLAUDE_CONFIG_DIR"])
    before = dev.snapshot()
    rc, _, err = dev.integrate(h, dev=False)
    check(rc != 0 and "cargo build output" in err and changed(before, dev.snapshot()) == [],
          f"c10 a cargo build output is refused without --allow-dev-path (rc={rc})")


def claude_wrapper():
    t = Tree("claude-wrapper")
    d = t.dirs["CLAUDE_CONFIG_DIR"]
    os.makedirs(d)
    wrapper = "my-wrapper remux agent session claude --hook"
    with open(f"{d}/settings.json", "w") as f:
        json.dump({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": wrapper}]}]}},
                  f, indent=2)
    before = t.snapshot()
    rc, _, err = t.integrate("claude-resume")
    check(rc == 0 and "did not write; left alone" in err and changed(before, t.snapshot()) == [],
          f"c11 install leaves a wrapper alone and adds nothing beside it (rc={rc}, err={err.strip()!r})")
    rc, out, _ = t.integrate("claude-resume", "--status")
    check("Stale" in out and wrapper in out, f"c11 --status names it Stale ({out.strip()!r})")
    rc, _, _ = t.integrate("claude-resume", "--uninstall")
    check(changed(before, t.snapshot()) == [], "c11 uninstall leaves it byte-identical")


CODEX_RESUME = f"{shq(REAL)} agent session codex --hook || true"


def codex():
    h = "codex-resume"
    t = Tree("codex")
    d = t.dirs["CODEX_HOME"]
    os.makedirs(d)
    rc, out, _ = t.integrate(h, "--status")
    check(rc == 0 and "Absent" in out, f"x1 a fresh CODEX_HOME is Absent ({out.strip()!r})")

    seeded = open(f"{FIXTURES}/agent_session/codex_config.toml").read()
    with open(f"{d}/config.toml", "w") as f:
        f.write(seeded)
    before = t.snapshot()
    rc, out, err = t.integrate(h)
    after = t.snapshot()
    config = open(f"{d}/config.toml").read()
    check(rc == 0 and config == seeded.replace("other = false\n", "other = false\nhooks = true\n"),
          f"x2 config.toml gains one `hooks = true` line and keeps every other byte "
          f"(rc={rc}, err={err.strip()!r}, got {config!r})")
    doc = json.load(open(f"{d}/hooks.json")) if os.path.exists(f"{d}/hooks.json") else {}
    check(groups(doc, "SessionStart") == [{"hooks": [{"type": "command", "command": CODEX_RESUME,
                                                      "timeout": 10}]}],
          f"x2 hooks.json holds one SessionStart group running ours (got {doc})")
    check(changed(before, after) == sorted([f"{d}/config.toml", f"{d}/hooks.json"]),
          f"x2 only config.toml and hooks.json changed ({changed(before, after)})")

    rc, out, _ = t.integrate(h, "--status")
    check(rc == 0 and "Installed" in out, f"x3 --status is Installed ({out.strip()!r})")

    before = t.snapshot()
    t.integrate(h)
    check(changed(before, t.snapshot()) == [], "x4 a second install changes no byte, inode or mtime")

    installed = open(f"{d}/config.toml").read()
    before = t.snapshot()
    rc, out, err = t.integrate(h, "--uninstall")
    doc = json.load(open(f"{d}/hooks.json"))
    check(rc == 0 and CODEX_RESUME not in commands(doc, "SessionStart"), f"x5 uninstall removes the hook (rc={rc})")
    check("hooks = true" in err and "left" in err, f"x5 ...and says it left the flag ({err.strip()!r})")
    check(open(f"{d}/config.toml").read() == installed and changed(before, t.snapshot()) == [f"{d}/hooks.json"],
          "x5 ...leaving config.toml exactly as the install made it")

    bare = Tree("codex-bare")
    bd = bare.dirs["CODEX_HOME"]
    os.makedirs(bd)
    with open(f"{bd}/config.toml", "w") as f:
        f.write('# no features yet\nmodel = "o3"\n')
    rc, _, err = bare.integrate(h)
    text = open(f"{bd}/config.toml").read()
    check(rc == 0 and text.startswith('# no features yet\nmodel = "o3"\n') and text.count("[features]") == 1
          and "hooks = true" in text, f"x6 [features] is created once (got {text!r})")
    bare.integrate(h)
    check(open(f"{bd}/config.toml").read() == text, "x6 ...and a re-run adds no second one")

    dry = Tree("codex-dry")
    os.makedirs(dry.dirs["CODEX_HOME"])
    with open(f"{dry.dirs['CODEX_HOME']}/config.toml", "w") as f:
        f.write(seeded)
    before = dry.snapshot()
    rc, out, _ = dry.integrate(h, "--dry-run")
    check(rc == 0 and "hooks = true" in out and changed(before, dry.snapshot()) == [],
          f"x7 --dry-run shows both changes and writes nothing ({out.strip()[:100]!r})")

    linked = Tree("codex-link")
    ld = linked.dirs["CODEX_HOME"]
    os.makedirs(ld)
    with open(f"{ld}/config.toml", "w") as f:
        f.write(seeded)
    with open(f"{ld}/hooks.json", "w") as f:
        f.write("{}\n")
    os.link(f"{ld}/hooks.json", f"{linked.root}/other-name.json")
    before = linked.snapshot()
    rc, _, err = linked.integrate(h)
    check(rc != 0 and "hard link" in err and changed(before, linked.snapshot()) == [],
          f"x9 a refused hooks.json leaves config.toml untouched (rc={rc}, err={err.strip()!r}, "
          f"changed {changed(before, linked.snapshot())})")

    missing = Tree("codex-missing")
    before = missing.snapshot()
    rc, _, err = missing.integrate(h)
    check(rc != 0 and "does not exist" in err and changed(before, missing.snapshot()) == [],
          f"x8 a missing CODEX_HOME is refused (rc={rc}, err={err.strip()!r})")


def extension(agent):
    h = f"{agent}-resume"
    drop = ("PI_CODING_AGENT_DIR",)

    def tree(name):
        t = Tree(name)
        if agent == "omp":
            t.dirs["PI_CONFIG_DIR"] = ".omp-test"
        t.dirs.pop("PI_CODING_AGENT_DIR")
        return t

    def agent_dir(t):
        return f"{t.home}/.pi/agent" if agent == "pi" else f"{t.home}/.omp-test/agent"

    t = tree(agent)
    d = agent_dir(t)
    path = f"{d}/extensions/remux-agent-session.ts"
    os.makedirs(d)
    rc, out, _ = t.integrate(h, "--status", drop=drop)
    check(rc == 0 and "Absent" in out, f"e1 {agent}: a fresh agent dir is Absent ({out.strip()!r})")

    before = t.snapshot()
    rc, _, err = t.integrate(h, drop=drop)
    after = t.snapshot()
    text = open(path).read() if os.path.exists(path) else ""
    check(rc == 0 and text.startswith("// managed by remux; REMUX_INTEGRATION_ID=pi-omp-resume ")
          and f"const REMUX_BIN = {json.dumps(REAL)};" in text and f'const AGENT = "{agent}";' in text,
          f"e2 {agent}: install bakes the binary and the agent in (rc={rc}, err={err.strip()!r})")
    check(changed(before, after) == sorted([path, os.path.dirname(path)]),
          f"e2 {agent}: only the extension (and its new extensions/ dir) appeared ({changed(before, after)})")
    node = subprocess.run(["node", "--check", path], capture_output=True, timeout=30)
    check(node.returncode == 0, f"e2 {agent}: node --check accepts it ({node.stderr[:200]!r})")

    rc, out, _ = t.integrate(h, "--status", drop=drop)
    check(rc == 0 and "Installed" in out, f"e3 {agent}: --status is Installed ({out.strip()!r})")

    before = t.snapshot()
    t.integrate(h, drop=drop)
    check(changed(before, t.snapshot()) == [], f"e4 {agent}: a second install changes nothing")

    with open(path, "w") as f:
        f.write(text.replace("timeout: 3000", "timeout: 9999"))
    rc, out, _ = t.integrate(h, "--status", drop=drop)
    check("Stale" in out and "differs" in out, f"e5 {agent}: an edited copy is Stale ({out.strip()!r})")
    t.integrate(h, drop=drop)
    check(open(path).read() == text, f"e5 {agent}: install rewrites it to this build's")

    before = t.snapshot()
    rc, _, err = t.integrate(h, "--uninstall", drop=drop)
    check(rc == 0 and not os.path.exists(path) and changed(before, t.snapshot()) == [path],
          f"e7 {agent}: uninstall removes ours and nothing else (rc={rc}, err={err.strip()!r})")

    foreign = "// my own extension\nexport default function () {}\n"
    with open(path, "w") as f:
        f.write(foreign)
    before = t.snapshot()
    rc, _, err = t.integrate(h, drop=drop)
    check(rc != 0 and "not a file remux wrote" in err and changed(before, t.snapshot()) == [],
          f"e6 {agent}: a headerless file is refused and untouched (rc={rc}, err={err.strip()!r})")
    rc, _, err = t.integrate(h, "--uninstall", drop=drop)
    check(rc == 0 and "left alone" in err and changed(before, t.snapshot()) == [],
          f"e6 {agent}: uninstall leaves it too, saying so (err={err.strip()!r})")

    missing = tree(f"{agent}-missing")
    before = missing.snapshot()
    rc, _, err = missing.integrate(h, drop=drop)
    check(rc != 0 and "does not exist" in err and changed(before, missing.snapshot()) == [],
          f"e8 {agent}: a missing agent dir is refused (rc={rc}, err={err.strip()!r})")

    both = Tree(f"{agent}-same")
    os.makedirs(both.dirs["PI_CODING_AGENT_DIR"])
    before = both.snapshot()
    rc, _, err = both.integrate(h)
    check(rc != 0 and "pi and omp both resolve to" in err and changed(before, both.snapshot()) == [],
          f"e9 {agent}: one dir for both agents is refused (rc={rc}, err={err.strip()!r})")


def opencode():
    h = "opencode-resume"
    t = Tree("opencode")
    d = t.dirs["OPENCODE_CONFIG_DIR"]
    os.makedirs(f"{d}/node_modules/@opencode-ai/plugin")
    for name, text in ((".gitignore", "node_modules\n"),
                       ("package.json", '{"dependencies":{"@opencode-ai/plugin":"1.18.32"}}\n'),
                       ("node_modules/@opencode-ai/plugin/index.js", "export {}\n")):
        with open(f"{d}/{name}", "w") as f:
            f.write(text)
    server, tui = f"{d}/plugins/remux-agent-session.js", f"{d}/remux-tui-session.js"
    rc, out, _ = t.integrate(h, "--status")
    check(rc == 0 and "Absent" in out, f"q1 a fresh config dir is Absent ({out.strip()!r})")

    seeded = open(f"{FIXTURES}/agent_session/opencode_tui.jsonc").read()
    with open(f"{d}/tui.jsonc", "w") as f:
        f.write(seeded)
    before = t.snapshot()
    rc, _, err = t.integrate(h)
    after = t.snapshot()
    text = open(f"{d}/tui.jsonc").read()
    added = [ln for ln in text.splitlines() if ln not in seeded.splitlines()]
    check(rc == 0 and added == [f'    {json.dumps(tui)},'] and
          [ln for ln in text.splitlines() if ln in seeded.splitlines()] == seeded.splitlines(),
          f"q2 tui.jsonc gains exactly one entry line and keeps every other (rc={rc}, err={err.strip()!r}, added={added})")
    check(changed(before, after) == sorted([f"{d}/plugins", server, tui, f"{d}/tui.jsonc"]),
          f"q2 only the two plugins, plugins/ and tui.jsonc changed ({changed(before, after)})")
    for path in (server, tui):
        body = open(path).read() if os.path.exists(path) else ""
        node = subprocess.run(["node", "--check", path], capture_output=True, timeout=30)
        check(f"const REMUX_BIN = {json.dumps(REAL)};" in body and node.returncode == 0,
              f"q2 {os.path.basename(path)} runs this binary and node accepts it ({node.stderr[:120]!r})")

    rc, out, _ = t.integrate(h, "--status")
    check(rc == 0 and "Installed" in out, f"q3 --status is Installed ({out.strip()!r})")

    before = t.snapshot()
    t.integrate(h)
    check(changed(before, t.snapshot()) == [], "q4 a second install changes nothing")

    rc, _, err = t.integrate(h, "--uninstall")
    check(rc == 0 and not os.path.exists(server) and not os.path.exists(tui),
          f"q5 uninstall removes both plugins (rc={rc}, err={err.strip()!r})")
    check(open(f"{d}/tui.jsonc").read() == seeded,
          f"q5 ...and tui.jsonc is byte for byte as before the install ({open(f'{d}/tui.jsonc').read()!r})")
    rc, out, _ = t.integrate(h, "--status")
    check("Absent" in out, f"q5 --status is Absent again ({out.strip()!r})")

    fresh = Tree("opencode-fresh")
    fd = fresh.dirs["OPENCODE_CONFIG_DIR"]
    os.makedirs(fd)
    rc, _, _ = fresh.integrate(h)
    doc = json.load(open(f"{fd}/tui.json")) if os.path.exists(f"{fd}/tui.json") else None
    check(rc == 0 and doc == {"$schema": "https://opencode.ai/tui.json", "plugin": [f"{fd}/remux-tui-session.js"]},
          f"q6 with no tui.json(c) a new tui.json holds the schema and the entry (got {doc})")
    plain = Tree("opencode-plain")
    pd = plain.dirs["OPENCODE_CONFIG_DIR"]
    os.makedirs(pd)
    with open(f"{pd}/tui.json", "w") as f:
        f.write('{\n  "theme": "x"\n}\n')
    rc, _, _ = plain.integrate(h)
    doc = json.load(open(f"{pd}/tui.json"))
    check(rc == 0 and doc.get("theme") == "x" and doc.get("plugin") == [f"{pd}/remux-tui-session.js"]
          and not os.path.exists(f"{pd}/tui.jsonc"),
          f"q6 with only tui.json, that file gains the entry (got {doc})")

    for label, path in (("server", "plugins/remux-agent-session.js"), ("TUI", "remux-tui-session.js")):
        mine = Tree(f"opencode-mine-{label}")
        md = mine.dirs["OPENCODE_CONFIG_DIR"]
        os.makedirs(os.path.dirname(f"{md}/{path}"), exist_ok=True)
        with open(f"{md}/{path}", "w") as f:
            f.write("// my own plugin\nexport default {}\n")
        before = mine.snapshot()
        rc, _, err = mine.integrate(h)
        check(rc != 0 and "not a file remux wrote" in err and changed(before, mine.snapshot()) == [],
              f"q7 a headerless {label} plugin file is refused and nothing is written (rc={rc}, err={err.strip()!r})")

    broken = Tree("opencode-broken")
    bd = broken.dirs["OPENCODE_CONFIG_DIR"]
    os.makedirs(bd)
    with open(f"{bd}/tui.jsonc", "w") as f:
        f.write(seeded)
    rc, _, _ = broken.integrate(h)
    with open(f"{bd}/tui.jsonc", "w") as f:
        f.write("{ \"plugin\": [ unparseable\n")
    rc, _, err = broken.integrate(h, "--uninstall")
    check(rc != 0 and os.path.exists(f"{bd}/plugins/remux-agent-session.js")
          and os.path.exists(f"{bd}/remux-tui-session.js"),
          f"q9 an uninstall that cannot edit tui.jsonc leaves both plugins (rc={rc}, err={err.strip()!r})")

    missing = Tree("opencode-missing")
    before = missing.snapshot()
    rc, _, err = missing.integrate(h)
    check(rc != 0 and "does not exist" in err and changed(before, missing.snapshot()) == [],
          f"q8 a missing config dir is refused (rc={rc}, err={err.strip()!r})")


MODES = {"claude": lambda: (claude(), claude_wrapper()), "codex": codex, "opencode": opencode,
         "pi": lambda: extension("pi"), "omp": lambda: extension("omp")}


def main():
    os.makedirs(RUNDIR, exist_ok=True)
    for mode in sys.argv[1:] or list(MODES):
        MODES[mode]()
    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
