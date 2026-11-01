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

Run: python3 tests/pty/integrate_agent_resume.py [claude ...]
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
            if var in self.dirs or var in ("HOME", "XDG_CONFIG_HOME"):
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


MODES = {"claude": lambda: (claude(), claude_wrapper())}


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
