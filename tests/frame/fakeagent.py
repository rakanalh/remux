"""A stand-in agent for the submit harnesses: it records exactly what reaches
its pane's input, and shows the screens a real agent would.

`install(path)` makes `path` a compiled binary that runs this file under
this interpreter as its child, and stays the process group's leader. The
server names the agent after that leader, and no script can carry the name
on both platforms: Linux names a direct-shebang script after the script, but
macOS names a process after its EXECUTABLE (`proc_pidpath`), which for any
script is the interpreter -- the Xcode `python3` re-execs itself as `Python`
-- so no `[agents] commands` entry matches, and every submit is refused
`agent_not_ready` for a reason that looks like a server bug.

Run as `fakeagent <log> <control>`. The terminal is put in raw mode, so every
byte the pane receives arrives unchanged, and each read becomes one line
`RX <time_ns> <hex>`, written to the pane AND appended to `<log>`:

- The log file, not the screen, is what a harness asserts on: it holds the
  exact bytes of each read, where the screen wraps and re-renders them.
- One line per READ, timestamped, is what shows the Enter arrived as a
  separate write, and how long after the text.
- The echo to the pane is the output a real agent produces when a prompt
  lands in it: it moves the pane's last-output clock the way the real echo
  does.

`<control>` is polled for one command per line. A harness writes it whole,
by rename, and the fakeagent takes it by renaming it away before reading,
so a command written while another is being read is never lost:

  paste-on / paste-off   turn bracketed paste mode (2004) on or off
  block                  show a permission prompt ("Do you want to proceed?")
  editblock              show Claude Code's file-edit approval dialog, which
                         no harness pattern matches
  clear                  clear the screen and say "back to work"
  work:<ms>              print "working..." every 100 ms for <ms>
  deaf                  stop reading input, stay in raw mode (an agent hung
                        or busy while text is typed at it)
  resume                start reading again (after deaf or probe-only)
  probe-only             read only what is exactly a paste opener (the
                         server's probe), never what follows it: an agent
                         that hangs right after it starts reading a message
  exit-on-rx             exit right after the next read
  fork-on-rx             on the next read, fork; the parent exits and the
                         child reads on in the same process group: a group
                         that outlives the agent that led it
  exit-after-body        exit right after reading a paste's end: an agent that
                         reads a whole message and dies before its Enter
  deaf-on-rx             stop reading right after the next read: an agent that
                         takes the probe and hangs before the body
  trickle[:<ms>]         read a probe whole, and anything else one byte per
                         <ms> (100 by default): an agent that reads, slowly,
                         for as long as a message takes
  block-on-rx            show the permission prompt the moment the next
                         pasted block's END arrives: a prompt drawn between
                         a typed message and its Enter
  editblock-on-rx        the same, with the file-edit approval dialog
  block-on-probe         show the permission prompt the moment the next
                         read is a paste opener alone: a prompt drawn after
                         the probe and before the message's body
  slow:<bytes>:<ms>      read at most <bytes> at a time, <ms> apart: an agent
                         that reads a long message slower than it arrives
  quit                   exit

Each command is acknowledged with a `CTL <command>` line in `<log>` once its
output is written, so a harness can wait for it rather than sleep.

Two hooks are set by what a typed message SAYS, and fire on the read that is
its Enter, the moment a real agent takes a prompt up:

  GO:<ms>    print "working..." every 100 ms for <ms> (2000 when no number
             follows): an agent that starts its turn at once and prints
             until it ends
  GO:<ms>+<gap>+<ms>
             the same in two bursts with a silent gap between them
  ASK:       show the permission prompt 1 s after the Enter: a turn that
             ends in a question
"""
import json
import os
import re
import select
import shutil
import stat
import subprocess
import sys
import time


PROCEED = "Do you want to proceed?\n> 1. Yes\n  2. No\n"
# Claude Code 2.1.x's dialog for an Edit tool call, option for option.
EDIT = ("Edit file\n src/x.rs\nDo you want to make this edit to x.rs?\n"
        "\u276f 1. Yes\n"
        "  2. Yes, allow all edits during this session (shift+tab)\n"
        "  3. No, and tell Claude what to do differently (esc)\n")


OPENER = b"\x1b[200~"


def queued():
    """Bytes waiting in this terminal's input queue, unread."""
    import fcntl
    import struct
    import termios
    return struct.unpack("i", fcntl.ioctl(0, termios.FIONREAD, b"\0\0\0\0"))[0]


def ctl_count(log, command):
    """How many times the fakeagent logging to `log` acknowledged `command`."""
    try:
        return sum(1 for ln in open(log) if ln.strip() == f"CTL {command}")
    except FileNotFoundError:
        return 0


def tell(ctl, log, command, timeout=5.0, settle=0.4):
    """Give the fakeagent one command, by rename, and wait for its
    acknowledgement, then `settle` seconds for the server to parse the
    output it caused."""
    before = ctl_count(log, command)
    with open(ctl + ".tmp", "w") as f:
        f.write(command + "\n")
    os.replace(ctl + ".tmp", ctl)
    end = time.time() + timeout
    while time.time() < end and ctl_count(log, command) == before:
        time.sleep(0.05)
    time.sleep(settle)


# Forwards the signals sent to the leader's pid alone, so the python child
# never outlives the wrapper that names it. One sent to the whole group, as a
# pane's close does, reaches the child twice, which nothing here minds.
_WRAPPER_C = r"""
#include <errno.h>
#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile pid_t child;

static void forward(int sig) {
    if (child > 0) kill(child, sig);
}

int main(int argc, char **argv) {
    int fwd[] = {SIGINT, SIGTERM, SIGHUP, SIGQUIT};
    for (unsigned i = 0; i < sizeof fwd / sizeof *fwd; i++) signal(fwd[i], forward);
    char *args[argc + 2];
    args[0] = PYTHON;
    args[1] = SCRIPT;
    for (int i = 1; i < argc; i++) args[i + 1] = argv[i];
    args[argc + 1] = 0;
    pid_t pid = fork();
    if (pid == 0) {
        for (unsigned i = 0; i < sizeof fwd / sizeof *fwd; i++) signal(fwd[i], SIG_DFL);
        execv(PYTHON, args);
        _exit(127);
    }
    child = pid;
    int st;
    while (waitpid(pid, &st, 0) < 0 && errno == EINTR) {}
    if (WIFEXITED(st)) return WEXITSTATUS(st);
    if (WIFSIGNALED(st)) return 128 + WTERMSIG(st);
    return 1;
}
"""


def require_private_dir(path):
    """Refuse a directory another user could write into, and close ours.

    The harness BINDIRs sit at fixed `/tmp` paths and are made with
    `exist_ok=True`, so one another local user created first is accepted
    silently, and they could swap the source, the binary or the script
    between install and the pane running it. A group-writable one of ours
    is what a umask of 002 makes, so it is tightened to 0700 rather than
    refused.
    """
    st = os.lstat(path)
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
        raise SystemExit(f"fakeagent: refusing {path}: not a real directory")
    if st.st_uid != os.geteuid():
        raise SystemExit(f"fakeagent: refusing {path}: owned by uid {st.st_uid}, not {os.geteuid()}")
    if st.st_mode & 0o002:
        raise SystemExit(f"fakeagent: refusing {path}: writable by others "
                         f"({stat.S_IMODE(st.st_mode):o})")
    os.chmod(path, 0o700)


def install(path):
    """Install the fakeagent at `path`, named for detection on Linux and macOS.

    Without a working `cc` it falls back to a direct-shebang script, which
    only Linux detects, so on macOS that is an error rather than a harness
    whose every agent case fails for no visible reason.
    """
    require_private_dir(os.path.dirname(os.path.abspath(path)))
    # Nobody else can add a name here once the directory is closed, so
    # removing what is there leaves no link planted before that for the
    # writes below to follow.
    for name in (path, path + ".c", path + ".py"):
        try:
            os.unlink(name)
        except FileNotFoundError:
            pass
    script = path + ".py"
    shutil.copyfile(os.path.abspath(__file__), script)
    cc = shutil.which("cc")
    error = "no `cc` on PATH"
    if cc:
        src = path + ".c"
        with open(src, "w") as f:
            f.write(_WRAPPER_C)
        r = subprocess.run(
            [cc, "-O1", f"-DPYTHON={json.dumps(sys.executable)}", f"-DSCRIPT={json.dumps(script)}",
             "-o", path, src],
            capture_output=True, text=True)
        os.remove(src)
        if r.returncode == 0:
            return
        error = f"`cc` failed: {r.stderr.strip()[:300]}"
    if sys.platform == "darwin":
        raise SystemExit(f"fakeagent: cannot build the stand-in ({error}); macOS needs it")
    print(f"note: fakeagent is a script ({error}); detected on Linux only", file=sys.stderr)
    with open(os.path.abspath(__file__)) as f:
        source = f.read()
    with open(path, "w") as f:
        f.write(f"#!{sys.executable}\n{source}")
    os.chmod(path, 0o755)


def main():
    import tty

    log_path, control_path = sys.argv[1:3]
    tty.setraw(0)
    log = open(log_path, "a", buffering=1)

    def out(text):
        os.write(1, text.replace("\n", "\r\n").encode())

    def ack(command):
        log.write(f"CTL {command}\n")

    def take_control():
        taken = control_path + ".taken"
        try:
            os.rename(control_path, taken)
        except FileNotFoundError:
            return []
        with open(taken) as f:
            commands = f.read().split()
        os.remove(taken)
        return commands

    work_until = 0.0
    next_work = 0.0
    deaf = False
    probe_only = False
    deaf_on_rx = False
    fork_on_rx = False
    exit_after_body = False
    trickle = 0.0
    exit_on_rx = False
    block_on_rx = None
    block_on_probe = False
    read_size, read_gap = 4096, 0.0
    on_enter = None
    ask_at = 0.0
    second_burst = None
    while True:
        if deaf:
            time.sleep(0.02)
            commands = take_control()
            for command in commands:
                if command == "resume":
                    deaf = False
                elif command == "quit":
                    ack(command)
                    return
                ack(command)
            continue
        readable, _, _ = select.select([0], [], [], 0.05)
        if readable and probe_only and queued() != len(OPENER):
            readable = []
            time.sleep(0.02)
        if readable:
            size = read_size
            if trickle:
                size = len(OPENER) if queued() == len(OPENER) else 1
            data = os.read(0, size)
            if not data:
                return
            line = f"RX {time.time_ns()} {data.hex()}\n"
            out(line)
            log.write(line)
            if exit_on_rx:
                return
            if exit_after_body and b"\x1b[201~" in data:
                return
            if fork_on_rx:
                fork_on_rx = False
                if os.fork() > 0:
                    os._exit(0)
            if deaf_on_rx:
                deaf_on_rx = False
                deaf = True
            if trickle:
                time.sleep(trickle)
            if block_on_probe and data == OPENER:
                block_on_probe = False
                out(PROCEED)
            if block_on_rx and b"\x1b[201~" in data:
                out(block_on_rx)
                block_on_rx = None
            go = re.search(rb"GO:(\d*)(?:\+(\d+)\+(\d+))?", data)
            if go:
                then = go.group(2) and (int(go.group(2)) / 1000, int(go.group(3)) / 1000)
                on_enter = ("work", (int(go.group(1) or 2000) / 1000, then))
            elif b"ASK:" in data:
                on_enter = ("ask", 1.0)
            elif on_enter and data == b"\r":
                kind, secs = on_enter
                on_enter = None
                if kind == "work":
                    first, then = secs
                    work_until = time.monotonic() + first
                    next_work = 0.0
                    if then:
                        second_burst = (work_until + then[0], then[1])
                else:
                    ask_at = time.monotonic() + secs
            time.sleep(read_gap)
        now = time.monotonic()
        if second_burst and now >= second_burst[0]:
            work_until = now + second_burst[1]
            next_work = 0.0
            second_burst = None
        if ask_at and now >= ask_at:
            ask_at = 0.0
            out(PROCEED)
        if now < work_until and now >= next_work:
            out("working...\n")
            next_work = now + 0.1
        for command in take_control():
            if command == "paste-on":
                out("\x1b[?2004h")
            elif command == "paste-off":
                out("\x1b[?2004l")
            elif command == "block":
                out(PROCEED)
            elif command == "editblock":
                out(EDIT)
            elif command == "clear":
                out("\x1b[2J\x1b[Hback to work\n")
            elif command.startswith("work:"):
                work_until = time.monotonic() + int(command[5:]) / 1000
                next_work = 0.0
            elif command == "deaf":
                deaf = True
            elif command == "probe-only":
                probe_only = True
            elif command == "resume":
                probe_only = False
            elif command.startswith("slow:"):
                size, gap = command[5:].split(":")
                read_size, read_gap = int(size), int(gap) / 1000
            elif command == "exit-on-rx":
                exit_on_rx = True
            elif command == "exit-after-body":
                exit_after_body = True
            elif command == "fork-on-rx":
                fork_on_rx = True
            elif command == "deaf-on-rx":
                deaf_on_rx = True
            elif command == "trickle" or command.startswith("trickle:"):
                trickle = int(command[8:] or 100) / 1000
            elif command == "block-on-rx":
                block_on_rx = PROCEED
            elif command == "editblock-on-rx":
                block_on_rx = EDIT
            elif command == "block-on-probe":
                block_on_probe = True
            elif command == "quit":
                ack(command)
                return
            ack(command)


if __name__ == "__main__":
    main()
