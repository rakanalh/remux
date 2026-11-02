"""A stand-in agent that reports its session, run as a JOB at a pane's prompt.

Run as `python3 session_job.py <result> <then> <remux> <agent session args...>`.
It runs `<remux> agent session <args...>` as its own child, so remux's parent
is this process and not the pane's shell, which is what an agent's hook or
plugin looks like to the server. The child's exit code, stdout and stderr are
written to `<result>` as one JSON object.

`<then>` is what the agent does after reporting:

  exit    linger 0.5 s, then exit. The shell takes the foreground back, which
          is the transition the forget rule hangs on. The linger is longer than
          one quiet-pane foreground sample (100 ms), so the server has seen the
          job in the foreground before it ends.
  stay    keep running, as an agent does while its user works with it.
"""
import json
import os
import subprocess
import sys
import time


def main():
    result, then, remux = sys.argv[1:4]
    done = subprocess.run([remux, "agent", "session", *sys.argv[4:]], capture_output=True,
                          timeout=20)
    with open(result + ".tmp", "w") as f:
        json.dump({"rc": done.returncode,
                   "stdout": done.stdout.decode("utf-8", "replace"),
                   "stderr": done.stderr.decode("utf-8", "replace")}, f)
    os.rename(result + ".tmp", result)
    if then == "stay":
        while True:
            time.sleep(1)
    time.sleep(0.5)


if __name__ == "__main__":
    main()
