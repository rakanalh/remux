#!/usr/bin/env python3
"""`remux integrate claude-messaging` in a throwaway HOME -- never the real
~/.claude.

  i1  status on a fresh HOME: Absent
  i2  install writes both hooks with the exact commands and keeps the user's
      keys in their order
  i3  status: Installed, naming the binary the hooks run
  i4  a second install leaves the file byte-identical
  i5  --dry-run prints the change and creates nothing
  i6  a remux hook remux did not write is Stale, and install neither rewrites
      it nor adds a second one beside it
  i7  --uninstall removes only remux's hooks; the user's survive; Absent again
  i8  a settings file that is not JSON is refused, named, and left untouched
  i9  an unknown harness, `claude` included, is a usage error naming
      claude-messaging
  i10 a symlinked settings.json is written through; the link survives
  i11 a customised remux hook and a wrapper around one are reported and left
      byte-identical by install and uninstall
  i12 an installed file in another indentation is not rewritten by install
      (bytes, inode and mtime), and --dry-run has nothing to change
  i13 uninstall on a file with no remux hook (and the user's own empty Stop
      list) does not rewrite it
  i14 uninstall drops an event list it emptied, never a user's empty one
  i15 installing hooks that run a cargo build output is refused without
      --allow-dev-path, naming it, and writes nothing
  i16 hooks run an installed copy by its absolute path; the build's own status
      calls them Stale (another binary), then Stale (gone) once it is deleted,
      and an install points them back
  i17 the by-name spelling is ours and Stale; install rewrites it in place to
      the absolute path
  i18 a settings.json linked to a file another user owns (root's /etc/passwd)
      is refused, naming the owner, and nothing is written
  i19 a settings.json linked to nothing is refused, and stays that link
  i20 another integration's SubagentStart/SubagentStop hooks in the same file,
      running the same binary, are untouched by install and by uninstall
  i21 a binary another user could replace is refused, naming the path, and
      nothing is written: in a 0777 directory, under a directory with o+w, or
      anywhere under /tmp; a copy in a 0755 directory is accepted

Copies of the binary go under ~/.cache/rmx-integmsg, never under /tmp: /tmp
is world-writable, so a binary there is refused (i21).

Run: python3 tests/pty/integrate_claude_messaging.py
"""
import json
import os
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import BIN  # noqa: E402

RUNDIR = "/tmp/rmx-integmsg"
# Where copies of the binary live: a tree only this user can write, since the
# installer refuses a binary under any directory others can write.
SAFE = os.path.join(os.path.expanduser("~"), ".cache", "rmx-integmsg")
assert "/.claude" not in SAFE, SAFE
FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "fixtures", "settings")
HARNESS = "claude-messaging"


def shq(text):
    return "'" + text.replace("'", "'\\''") + "'"


def hook_command(binary, verb):
    return f"{shq(os.path.realpath(binary))} msg hook {verb} || true"


STOP = hook_command(BIN, "claude-stop")
PROMPT = hook_command(BIN, "claude-prompt")

FAILURES = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


def fresh_home(name):
    home = f"{RUNDIR}/{name}"
    shutil.rmtree(home, ignore_errors=True)
    os.makedirs(home)
    return home


def env_for(home):
    env = {k: v for k, v in os.environ.items() if k not in ("REMUX_SESSION", "REMUX_PANE")}
    env.update(HOME=home, XDG_RUNTIME_DIR=f"{RUNDIR}/run", XDG_STATE_HOME=f"{RUNDIR}/state",
               XDG_DATA_HOME=f"{RUNDIR}/data", XDG_CONFIG_HOME=f"{RUNDIR}/config")
    return env


def integrate(home, *args, binary=BIN, dev=True):
    env = env_for(home)
    # The one thing this harness must never do is touch the real ~/.claude.
    assert "HOME" in env and env["HOME"].startswith(RUNDIR + "/"), env.get("HOME")
    flags = ["--allow-dev-path"] if dev and args and args[0] == HARNESS else []
    p = subprocess.run([binary, "integrate", *args, *flags], env=env, capture_output=True,
                       timeout=20)
    return p.returncode, p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace")


def settings(home):
    return f"{home}/.claude/settings.json"


def commands(doc, event):
    return [h.get("command") for g in doc.get("hooks", {}).get(event, [])
            for h in g.get("hooks", [])]


def snapshot(path):
    st = os.stat(path)
    return open(path, "rb").read(), st.st_ino, st.st_mtime_ns


shutil.rmtree(RUNDIR, ignore_errors=True)
os.makedirs(RUNDIR)
shutil.rmtree(SAFE, ignore_errors=True)
os.makedirs(SAFE, mode=0o755)


def copy_into(directory, mode=0o755):
    os.makedirs(directory, exist_ok=True)
    os.chmod(directory, mode)
    path = f"{directory}/remux"
    shutil.copy2(BIN, path)
    return path
home = fresh_home("home")

# -- i1 ----------------------------------------------------------------------
rc, out, err = integrate(home, HARNESS, "--status")
check(rc == 0 and "Absent" in out, f"i1 a fresh HOME is Absent (rc={rc}, out={out.strip()!r})")

# -- i2 ----------------------------------------------------------------------
os.makedirs(f"{home}/.claude", mode=0o755)
shutil.copy(f"{FIXTURES}/msg_other_hooks_unsorted.json", settings(home))
user_keys = list(json.load(open(settings(home))).keys())
rc, out, err = integrate(home, HARNESS)
doc = json.load(open(settings(home)))
check(rc == 0 and STOP in commands(doc, "Stop") and PROMPT in commands(doc, "UserPromptSubmit"),
      f"i2 install writes both exact hooks (rc={rc}, err={err.strip()!r})")
check(list(doc.keys()) == user_keys and "notify-send stopped" in commands(doc, "Stop")
      and "echo pre" in commands(doc, "PreToolUse"),
      f"i2 and keeps the user's keys, in order, and their hooks ({list(doc.keys())})")

# -- i3 ----------------------------------------------------------------------
rc, out, _ = integrate(home, HARNESS, "--status")
check(rc == 0 and f"Installed (the hooks run {os.path.realpath(BIN)})" in out,
      f"i3 status: Installed, naming the binary ({out.strip()!r})")

# -- i4 ----------------------------------------------------------------------
before = open(settings(home), "rb").read()
integrate(home, HARNESS)
check(open(settings(home), "rb").read() == before, "i4 a second install changes no byte")

# -- i5 ----------------------------------------------------------------------
dry = fresh_home("dry")
rc, out, _ = integrate(dry, HARNESS, "--dry-run")
check(rc == 0 and settings(dry) in out and "UserPromptSubmit" in out
      and not os.path.exists(settings(dry)),
      f"i5 --dry-run prints the change and writes nothing (rc={rc})")

# -- i6 ----------------------------------------------------------------------
FOREIGN = STOP.replace(" || true", " --json || true")
edited = fresh_home("edited")
os.makedirs(f"{edited}/.claude", mode=0o755)
text = open(settings(home)).read()
assert text.count(json.dumps(STOP)[1:-1]) == 1
open(settings(edited), "w").write(text.replace(json.dumps(STOP)[1:-1], json.dumps(FOREIGN)[1:-1]))
rc, out, _ = integrate(edited, HARNESS, "--status")
check("Stale" in out and "Stop" in out and FOREIGN in out,
      f"i6 a remux hook remux did not write is Stale, quoted ({out.strip()!r})")
rc, out, err = integrate(edited, HARNESS)
stops = commands(json.load(open(settings(edited))), "Stop")
check(rc == 0 and FOREIGN in stops and STOP not in stops and "Stop" in err,
      f"i6 install leaves it, adds no second Stop hook beside it, and says so ({stops}, {err!r})")

# -- i7 ----------------------------------------------------------------------
rc, out, err = integrate(home, HARNESS, "--uninstall")
doc = json.load(open(settings(home)))
ours = [c for e in ("Stop", "UserPromptSubmit") for c in commands(doc, e) if "msg hook" in c]
check(rc == 0 and not ours and "notify-send stopped" in commands(doc, "Stop")
      and "echo pre" in commands(doc, "PreToolUse"),
      f"i7 uninstall removes only remux's hooks (rc={rc}, left={ours})")
rc, out, _ = integrate(home, HARNESS, "--status")
check("Absent" in out, f"i7 and status is Absent again ({out.strip()!r})")

# -- i8 ----------------------------------------------------------------------
open(settings(home), "w").write("{ this is not json")
before = open(settings(home), "rb").read()
rc, out, err = integrate(home, HARNESS)
check(rc != 0 and settings(home) in err and open(settings(home), "rb").read() == before,
      f"i8 invalid JSON is refused, named, untouched (rc={rc}, err={err.strip()[:120]!r})")

# -- i10 ---------------------------------------------------------------------
# A settings.json that is a symlink (a dotfiles checkout) is written THROUGH:
# the link stays a link and the file it names gets the hooks.
linked = fresh_home("linked")
os.makedirs(f"{linked}/dotfiles", mode=0o755)
os.makedirs(f"{linked}/.claude", mode=0o755)
shutil.copy(f"{FIXTURES}/no_hooks.json", f"{linked}/dotfiles/settings.json")
os.symlink(f"{linked}/dotfiles/settings.json", settings(linked))
rc, _, err = integrate(linked, HARNESS)
doc = json.load(open(f"{linked}/dotfiles/settings.json"))
check(rc == 0 and os.path.islink(settings(linked)) and STOP in commands(doc, "Stop"),
      f"i10 a symlinked settings.json is written through, the link kept (rc={rc}, err={err!r})")

# -- i11 ---------------------------------------------------------------------
foreign = fresh_home("foreign")
os.makedirs(f"{foreign}/.claude", mode=0o755)
shutil.copy(f"{FIXTURES}/msg_foreign_hooks.json", settings(foreign))
rc, out, _ = integrate(foreign, HARNESS, "--status")
check("Stale" in out and "Stop" in out and "UserPromptSubmit" in out,
      f"i11 a customised hook and a wrapper are reported Stale ({out.strip()!r})")
before = snapshot(settings(foreign))
rc, _, err = integrate(foreign, HARNESS)
check(rc == 0 and snapshot(settings(foreign)) == before,
      f"i11 install leaves both byte-identical (rc={rc}, err={err!r})")
rc, _, err = integrate(foreign, HARNESS, "--uninstall")
check(rc == 0 and snapshot(settings(foreign)) == before,
      f"i11 uninstall leaves both byte-identical (rc={rc}, err={err!r})")

# -- i12 ---------------------------------------------------------------------
wide = fresh_home("wide")
os.makedirs(f"{wide}/.claude", mode=0o755)
assert integrate(wide, HARNESS)[0] == 0
json.dump(json.load(open(settings(wide))), open(settings(wide), "w"), indent=4)
before = snapshot(settings(wide))
rc, out, _ = integrate(wide, HARNESS, "--status")
check("Installed" in out, f"i12 a 4-space installed file is Installed ({out.strip()!r})")
rc, out, _ = integrate(wide, HARNESS, "--dry-run")
check(rc == 0 and "nothing to change" in out, f"i12 --dry-run has nothing to change ({out!r})")
rc, _, _ = integrate(wide, HARNESS)
check(rc == 0 and snapshot(settings(wide)) == before,
      "i12 install does not rewrite it (bytes, inode, mtime)")

# -- i13 ---------------------------------------------------------------------
plain = fresh_home("plain")
os.makedirs(f"{plain}/.claude", mode=0o755)
shutil.copy(f"{FIXTURES}/msg_no_remux_4space.json", settings(plain))
before = snapshot(settings(plain))
rc, _, _ = integrate(plain, HARNESS, "--uninstall")
check(rc == 0 and snapshot(settings(plain)) == before,
      "i13 uninstall with no remux hook (and the user's own empty Stop list) does not rewrite the file")

# -- i14 ---------------------------------------------------------------------
empties = fresh_home("empties")
os.makedirs(f"{empties}/.claude", mode=0o755)
doc = json.load(open(f"{FIXTURES}/msg_by_name.json"))
doc["hooks"]["PreToolUse"] = []
json.dump(doc, open(settings(empties), "w"), indent=2)
rc, _, _ = integrate(empties, HARNESS, "--uninstall")
hooks = json.load(open(settings(empties))).get("hooks", {})
check(rc == 0 and hooks.get("PreToolUse") == [] and "Stop" not in hooks
      and "UserPromptSubmit" not in hooks,
      f"i14 uninstall drops the lists it emptied and keeps the user's empty one ({hooks})")

# -- i15 ---------------------------------------------------------------------
def is_dev_build(path):
    """The harness's own reading of remux's rule: an ancestor `target` that
    cargo made. Only then is i15's premise true of BIN."""
    parts = os.path.realpath(path).split("/")
    for i, part in enumerate(parts[:-1]):
        if part == "target":
            target = "/".join(parts[:i + 1])
            if any(os.path.exists(f"{target}/{m}") for m in (".rustc_info.json", "CACHEDIR.TAG")) \
                    or os.path.exists("/".join(parts[:i]) + "/Cargo.toml"):
                return True
    return False


devhome = fresh_home("dev")
if is_dev_build(BIN):
    rc, out, err = integrate(devhome, HARNESS, dev=False)
    check(rc != 0 and "--allow-dev-path" in err and os.path.realpath(BIN) in err
          and not os.path.exists(settings(devhome)),
          f"i15 a cargo build output is refused without --allow-dev-path (rc={rc}, err={err!r})")
    rc, out, err = integrate(devhome, HARNESS, "--dry-run", dev=False)
    check(rc != 0 and "--allow-dev-path" in err, f"i15 and so is its --dry-run (rc={rc})")
else:
    print(f"SKIP  i15 needs BIN to be a cargo build output, and {os.path.realpath(BIN)} is not")

# -- i16 ---------------------------------------------------------------------
copyhome = fresh_home("copy")
copy = copy_into(f"{SAFE}/i16")
rc, _, err = integrate(copyhome, HARNESS, binary=copy, dev=False)
stops = commands(json.load(open(settings(copyhome))), "Stop")
check(rc == 0 and stops == [hook_command(copy, "claude-stop")],
      f"i16 an installed copy writes hooks that run it by path (rc={rc}, err={err!r}, {stops})")
rc, out, _ = integrate(copyhome, HARNESS, "--status", binary=copy)
check(f"Installed (the hooks run {os.path.realpath(copy)})" in out,
      f"i16 and its own status is Installed ({out.strip()!r})")
rc, out, _ = integrate(copyhome, HARNESS, "--status")
check("Stale" in out and "not this binary" in out and os.path.realpath(copy) in out,
      f"i16 another build calls them Stale, naming the path ({out.strip()!r})")
os.remove(copy)
rc, out, _ = integrate(copyhome, HARNESS, "--status")
check("Stale" in out and "no longer exists" in out and os.path.realpath(copy) in out,
      f"i16 and once it is deleted, Stale: gone ({out.strip()!r})")
rc, out, _ = integrate(copyhome, HARNESS)
stops = commands(json.load(open(settings(copyhome))), "Stop")
check(rc == 0 and stops == [STOP], f"i16 an install points them back ({stops})")

# -- i17 ---------------------------------------------------------------------
named = fresh_home("named")
os.makedirs(f"{named}/.claude", mode=0o755)
shutil.copy(f"{FIXTURES}/msg_by_name.json", settings(named))
rc, out, _ = integrate(named, HARNESS, "--status")
check("Stale" in out and "by name" in out, f"i17 the by-name spelling is Stale ({out.strip()!r})")
rc, out, _ = integrate(named, HARNESS)
doc = json.load(open(settings(named)))
check(rc == 0 and commands(doc, "Stop") == [STOP]
      and commands(doc, "UserPromptSubmit") == [PROMPT],
      f"i17 install rewrites it in place to the absolute path ({commands(doc, 'Stop')})")

# -- i18 ---------------------------------------------------------------------
rooted = fresh_home("rooted")
os.makedirs(f"{rooted}/.claude", mode=0o755)
os.symlink("/etc/passwd", settings(rooted))
before = snapshot("/etc/passwd")
rc, _, err = integrate(rooted, HARNESS)
check(rc != 0 and "belongs to uid 0" in err and snapshot("/etc/passwd") == before
      and os.path.islink(settings(rooted)),
      f"i18 another user's file is refused, naming the owner (rc={rc}, err={err.strip()!r})")

# -- i19 ---------------------------------------------------------------------
dangling = fresh_home("dangling")
os.makedirs(f"{dangling}/.claude", mode=0o755)
os.symlink(f"{dangling}/dotfiles/settings.json", settings(dangling))
rc, _, err = integrate(dangling, HARNESS)
check(rc != 0 and "does not exist" in err and os.path.islink(settings(dangling))
      and os.readlink(settings(dangling)) == f"{dangling}/dotfiles/settings.json"
      and not os.path.exists(f"{dangling}/dotfiles"),
      f"i19 a dangling link is refused and kept (rc={rc}, err={err.strip()!r})")

# -- i20 ---------------------------------------------------------------------
# The other integration as this same binary would have written it: its
# commands carry the binary's path too, so nothing but the event tells them
# apart from ours.
both = fresh_home("both")
os.makedirs(f"{both}/.claude", mode=0o755)
doc = json.load(open(f"{FIXTURES}/both_integrations.json"))
for event, verb in (("SubagentStart", "open"), ("SubagentStop", "close")):
    doc["hooks"][event][0]["hooks"][0]["command"] = (
        f"{shq(os.path.realpath(BIN))} agent-pane {verb} --harness claude --from-hook || true")
json.dump(doc, open(settings(both), "w"), indent=2)
original = json.load(open(settings(both)))


def theirs(d):
    return {e: d.get("hooks", {}).get(e) for e in ("SubagentStart", "SubagentStop")}


def without_ours(d):
    d = json.loads(json.dumps(d))
    for e in ("Stop", "UserPromptSubmit"):
        d.get("hooks", {}).pop(e, None)
    return d


rc, _, err = integrate(both, HARNESS)
after = json.load(open(settings(both)))
check(rc == 0 and theirs(after) == theirs(original) and commands(after, "Stop") == [STOP],
      f"i20 install repairs ours and leaves SubagentStart/SubagentStop as they were (rc={rc}, err={err!r})")
rc, _, err = integrate(both, HARNESS, "--uninstall")
after = json.load(open(settings(both)))
check(rc == 0 and theirs(after) == theirs(original) and without_ours(after) == without_ours(original)
      and "Stop" not in after["hooks"] and "UserPromptSubmit" not in after["hooks"],
      f"i20 uninstall removes ours and nothing of theirs ({sorted(after.get('hooks', {}))})")

# -- i21 ---------------------------------------------------------------------
ow_binary = copy_into(f"{SAFE}/ow/inner")
os.chmod(f"{SAFE}/ow", 0o757)
for name, binary, offender in (
    ("a 0777 directory", copy_into(f"{SAFE}/open", 0o777), f"{SAFE}/open"),
    ("a directory under one with o+w", ow_binary, f"{SAFE}/ow"),
    ("/tmp", copy_into(f"{RUNDIR}/bin"), "/tmp"),
):
    unsafe = fresh_home("unsafe")
    rc, out, err = integrate(unsafe, HARNESS, binary=binary, dev=False)
    check(rc != 0 and f"{offender} is writable by others" in err and not os.path.exists(f"{unsafe}/.claude"),
          f"i21 a binary in {name} is refused, naming {offender}, nothing written (rc={rc}, err={err.strip()[:160]!r})")
    rc, out, err = integrate(unsafe, HARNESS, "--dry-run", binary=binary, dev=False)
    check(rc != 0 and "writable by others" in err, f"i21 ...and so is its --dry-run (rc={rc})")
safe = fresh_home("safe")
rc, out, err = integrate(safe, HARNESS, binary=copy_into(f"{SAFE}/ok"), dev=False)
check(rc == 0 and commands(json.load(open(settings(safe))), "Stop") == [hook_command(f"{SAFE}/ok/remux", "claude-stop")],
      f"i21 a copy in a 0755 directory only this user can write is accepted (rc={rc}, err={err!r})")
os.chmod(f"{SAFE}/open", 0o755)
os.chmod(f"{SAFE}/ow", 0o755)

# -- i9 ----------------------------------------------------------------------
for bogus in ("bogus", "claude"):
    rc, _, err = integrate(home, bogus)
    check(rc == 2 and HARNESS in err,
          f"i9 `remux integrate {bogus}` is a usage error naming {HARNESS} (rc={rc})")

shutil.rmtree(SAFE, ignore_errors=True)
print()
if FAILURES:
    print(f"{len(FAILURES)} FAILED:")
    for f in FAILURES:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
