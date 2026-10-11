"""A stand-in agent for the submit harnesses: it records exactly what reaches
its pane's input, and shows the screens a real agent would.

`install(path)` writes this file to `path` with `#!<this interpreter>` as its
first line. A DIRECT interpreter shebang is what makes the kernel name the
process after the script: through `#!/usr/bin/env python3` the process is
re-exec'd and its `comm` is `python3`, which no `[agents] commands` entry
matches, and every submit is then refused `agent_not_ready` for a reason that
looks like a server bug.

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
  slow:<bytes>:<ms>      read at most <bytes> at a time, <ms> apart: an agent
                         that reads a long message slower than it arrives
  quit                   exit

Each command is acknowledged with a `CTL <command>` line in `<log>` once its
output is written, so a harness can wait for it rather than sleep.
"""
import os
import select
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


def install(path):
    """Write the fakeagent to `path` as an executable with a direct shebang."""
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
    read_size, read_gap = 4096, 0.0
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
            if block_on_rx and b"\x1b[201~" in data:
                out(block_on_rx)
                block_on_rx = None
            time.sleep(read_gap)
        now = time.monotonic()
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
            elif command == "quit":
                ack(command)
                return
            ack(command)


if __name__ == "__main__":
    main()
