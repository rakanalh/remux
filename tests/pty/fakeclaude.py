"""A stand-in for Claude Code's hook runner, for the messaging hook harnesses.

`install(path)` writes this file to `path` with `#!<this interpreter>` as its
first line, and the harness installs it as a file NAMED `claude`: a direct
interpreter shebang makes the process's `comm` the script's name, so the
server detects the pane as running `claude` with no config, as it would the
real one.

Run as `claude <control> <log> <fixtures dir> <remux binary>`. It reads the
hook commands from `$HOME/.claude/settings.json` on every run, the way Claude
Code reads them, and runs each through `sh -c` with a payload on stdin,
recording `{"event", "rc", "stdout", "stderr"}` as one JSON line in `<log>`
and `HOOK:<event> rc=<rc>` on the pane.

With `HOOK_KILL_AFTER_MS` in the hook env, each hook run, and everything it
started, is killed that long after it starts: Claude killing a hook at its
timeout.

`<control>` is polled for one command per line, written whole by rename and
taken by rename, so none is lost; each is acknowledged with `{"ctl": cmd}` in
`<log>` once it has run:

  stop                run the Stop hooks with a fresh turn's payload; while a
                      hook answers {"decision":"block"}, print BLOCKED: and run
                      them again with stop_hook_active true, as Claude does,
                      at most 8 times, then print LOOP
  stop:keepfeeding    the same, and after every block send this pane one new
                      message (a sender mailing once per Stop)
  stop:once=<file>    run the Stop hooks once with <file> as the payload
  prompt              run the UserPromptSubmit hooks with a prompt payload
  prompt:wrong        run them with a Stop payload, as a swapped settings
                      entry would
  env:<K>=<V>         set a variable for later hook runs
  unenv:<K>           remove it
  name:<name>         run `remux agent name <name>` from this pane, recording
                      its rc and stderr
  quit                exit
"""
import json
import os
import subprocess
import sys
import time


def install(path):
    """Write the fake to `path` as an executable with a direct shebang."""
    with open(os.path.abspath(__file__)) as f:
        source = f.read()
    with open(path, "w") as f:
        f.write(f"#!{sys.executable}\n{source}")
    os.chmod(path, 0o755)


def main():
    control, log_path, fixtures, remux = sys.argv[1:5]
    env = dict(os.environ)
    own_pane = os.environ.get("REMUX_PANE", "")
    log = open(log_path, "a", buffering=1)

    def note(obj):
        log.write(json.dumps(obj) + "\n")

    def say(text):
        sys.stdout.write(text + "\r\n")
        sys.stdout.flush()

    def commands(event):
        path = os.path.join(env.get("HOME", ""), ".claude", "settings.json")
        try:
            with open(path) as f:
                settings = json.load(f)
        except (OSError, ValueError):
            return []
        return [hook["command"]
                for entry in settings.get("hooks", {}).get(event, [])
                for hook in entry.get("hooks", [])
                if hook.get("type") == "command"]

    def descendants(pid):
        found = []
        try:
            with open(f"/proc/{pid}/task/{pid}/children") as f:
                kids = [int(k) for k in f.read().split()]
        except OSError:
            return found
        for kid in kids:
            found += descendants(kid) + [kid]
        return found

    def run_bounded(cmd, payload, kill_after_s, always_kill=False):
        """Run `cmd` with `payload` on stdin; kill it and everything it
        started after `kill_after_s`, at once with `always_kill`, else only if
        it is still running, as Claude does at a hook's timeout."""
        proc = subprocess.Popen(["sh", "-c", cmd], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, env=env)
        proc.stdin.write(payload)
        proc.stdin.close()
        try:
            if always_kill:
                time.sleep(kill_after_s)
                raise subprocess.TimeoutExpired(cmd, kill_after_s)
            proc.wait(timeout=kill_after_s)
        except subprocess.TimeoutExpired:
            for pid in descendants(proc.pid) + [proc.pid]:
                try:
                    os.kill(pid, 9)
                except OSError:
                    pass
        out, err = proc.communicate()
        return proc.returncode, out.decode("utf-8", "replace"), err.decode("utf-8", "replace")

    def run(event, payload_path, label):
        with open(payload_path, "rb") as f:
            payload = f.read()
        outs = []
        for cmd in commands(event):
            if env.get("HOOK_KILL_AFTER_MS"):
                rc, out, err = run_bounded(cmd, payload, int(env["HOOK_KILL_AFTER_MS"]) / 1000, True)
            else:
                rc, out, err = run_bounded(cmd, payload, 20)
            note({"event": label, "rc": rc, "stdout": out, "stderr": err})
            say(f"HOOK:{label} rc={rc}")
            outs.append(out)
        return outs

    def block_reason(outs):
        for out in outs:
            try:
                obj = json.loads(out)
            except ValueError:
                continue
            if isinstance(obj, dict) and obj.get("decision") == "block":
                return obj.get("reason", "")
        return None

    feeds = [0]

    def feed():
        feeds[0] += 1
        target = own_pane if own_pane.startswith("%") else f"%{own_pane}"
        done = subprocess.run([remux, "msg", "send", target, f"FEED:{feeds[0]}", "--deliver", "mailbox"],
                              env=dict(os.environ), capture_output=True, timeout=20)
        note({"event": "feed", "rc": done.returncode,
              "stderr": done.stderr.decode("utf-8", "replace")})

    def stop(keep_feeding):
        reason = block_reason(run("Stop", os.path.join(fixtures, "claude_stop.json"), "stop"))
        runs = 1
        while reason is not None:
            say("BLOCKED:" + (reason.splitlines()[1] if len(reason.splitlines()) > 1 else reason)[:60])
            if keep_feeding:
                feed()
            if runs >= 8:
                say("LOOP")
                note({"event": "loop"})
                return
            reason = block_reason(run("Stop", os.path.join(fixtures, "claude_stop_active.json"), "stop"))
            runs += 1

    def take():
        taken = control + ".taken"
        try:
            os.rename(control, taken)
        except FileNotFoundError:
            return []
        with open(taken) as f:
            lines = [ln.strip() for ln in f.read().splitlines() if ln.strip()]
        os.remove(taken)
        return lines

    say("FAKE-CLAUDE ready")
    while True:
        time.sleep(0.05)
        for cmd in take():
            if cmd == "stop":
                stop(False)
            elif cmd == "stop:keepfeeding":
                stop(True)
            elif cmd.startswith("stop:once="):
                run("Stop", cmd[len("stop:once="):], "stop")
            elif cmd == "prompt":
                run("UserPromptSubmit", os.path.join(fixtures, "claude_prompt.json"), "prompt")
            elif cmd == "prompt:wrong":
                run("UserPromptSubmit", os.path.join(fixtures, "claude_stop.json"), "prompt")
            elif cmd.startswith("env:"):
                key, _, value = cmd[4:].partition("=")
                env[key] = value
            elif cmd.startswith("unenv:"):
                env.pop(cmd[6:], None)
            elif cmd.startswith("name:"):
                done = subprocess.run([remux, "agent", "name", cmd[5:]], env=env, capture_output=True,
                                      timeout=20)
                note({"event": "name", "rc": done.returncode,
                      "stdout": done.stdout.decode("utf-8", "replace"),
                      "stderr": done.stderr.decode("utf-8", "replace")})
            elif cmd == "quit":
                note({"ctl": cmd})
                return
            note({"ctl": cmd})


if __name__ == "__main__":
    main()
