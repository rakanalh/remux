"""Stand-ins for util-linux and coreutils commands that macOS does not ship.

Each is an argv prefix: append an ABSOLUTE program path and its arguments.
The two `setsid`s always fork, so the program never runs as the caller's
process and always leads a session of its own, whether or not the caller was
a group leader. `setsid(1)` forks only when it must; for every use here (a
command run from a pane, typed at an interactive shell, or under
`( ... & )`) that is the same outcome.
"""
import sys

# `setsid -f`: the parent returns at once.
SETSID_F = [sys.executable, "-c",
            "import os, sys; os.fork() and os._exit(0); os.setsid(); os.execv(sys.argv[1], sys.argv[1:])"]

# `setsid -w`: the parent waits, and exits with the program's status.
SETSID_W = [sys.executable, "-c",
            "import os, sys; pid = os.fork(); pid or (os.setsid(), os.execv(sys.argv[1], sys.argv[1:])); "
            "sys.exit(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))"]

# `timeout <secs>`: TERM the program once <secs> have passed. The first
# argument after the prefix is the number of seconds. The exit status is
# coreutils': 124 when the time ran out, else the program's, 128 + N for a
# program killed by signal N.
TIMEOUT = [sys.executable, "-c",
           "import os, signal, sys; pid = os.fork(); pid or os.execv(sys.argv[2], sys.argv[2:]); fired = []; "
           "signal.signal(signal.SIGALRM, lambda *_: (fired.append(1), os.kill(pid, signal.SIGTERM))); "
           "signal.setitimer(signal.ITIMER_REAL, float(sys.argv[1])); "
           "code = os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]); "
           "sys.exit(124 if fired else 128 - code if code < 0 else code)"]
