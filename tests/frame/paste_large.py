#!/usr/bin/env python3
"""A large `Input` reaches the pane WHOLE -- the paste-truncation regression.

The PTY master is `O_NONBLOCK`: `start_reader` sets the flag on a `try_clone`d
descriptor, and a dup SHARES the open file description, so the flag lands on the
pane's one and only master. `Pty::write_input`'s loop was written as if the fd
were blocking, so the first `EAGAIN` -- the moment the slave's input queue fills
and the program in the pane has not drained it yet -- ended the write with the
tail SILENTLY DROPPED (the error is logged and swallowed one frame up, and the
client is never told).

Two user-visible shapes, both reproduced below by size alone:

* a paste bigger than the queue's free space arrives TRUNCATED (strace:
  `write(fd, .., 16000) = 11776` then `= -1 EAGAIN`);
* a paste arriving while the queue is still full is lost ENTIRELY -- the very
  first `write` is the one that returns `EAGAIN`, so ZERO bytes land. That is
  the "the paste did nothing" report rather than the "half of it appeared" one.
  It is also why a truncated bracketed paste reads as nothing happening at all:
  the closing `\\x1b[201~` is in the lost tail, so the shell sits waiting for a
  terminator that will never come and commits none of what it did receive.

The assertion is CONTENT, not length. Every line is numbered, so a writer that
loses a chunk from the middle or reorders two of them fails here -- both of
which keep the byte count a length check would be satisfied by.

`cat > file` is the receiver because the file is written by the program INSIDE
the pane: it cannot be faked by anything the renderer does, and it survives the
pane scrolling the echo away.

**The log checks are a PAIR, and the pairing is the point.** They started as a
single absence check for `write to PTY master failed`, which was `write_input`'s
context string -- and the fix made `write_input` `#[cfg(test)]`, so that string
left the daemon binary entirely and the assertion could never fail again. It
passed on a search that had nothing to find, which is exactly CLAUDE.md's fourth
way a harness goes green while testing nothing. The absence checks below are
therefore pointed at strings the daemon binary really does contain
(`start_writer`'s abandonment warn, `send_input`'s dropped-input warn), and they
are led by a PRESENCE check for the line `start_writer` logs when it is spawned:
that one goes red if the queue is not wired up at all, which is the failure an
absence-only check is structurally blind to. Do not collapse them back into one.

Run: python3 tests/frame/paste_large.py
"""
import os
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client  # noqa: E402

# PID-unique, unlike every other harness here, and the exception is earned. A
# fixed `/tmp/rmx-*` path is this repo's convention and it is fine for a harness
# whose only shared state is the SERVER -- two of those merely race. This one
# has a pane WRITING FILES to a path derived from that directory, so two runs do
# not race, they CORRUPT each other's evidence, and the corruption reads as a
# truncation in the very thing under test.
# `Server.start` opens with `shutil.rmtree(RUNDIR)`, so a second instance
# starting mid-run deletes the first's `workdir` under its feet: its `cat` keeps
# writing to an unlinked inode and the file it later reads is gone, which is
# reported as `received 0` -- indistinguishable, at the assertion, from the
# truncation bug this file exists to catch. Reproduced by running two instances
# 2.6s apart. Worse, a shared path can put two clients on one pane, and the
# stream then contains one payload, the other instance's `cat > ...` command
# line, and the payload again.
#
# `/tmp/rmx-paste-<pid>/run/remux.sock` is ~36 chars, well inside the 108-char
# limit on a Unix socket path.
RUNDIR = f"/tmp/rmx-paste-{os.getpid()}"
WORKDIR = f"{RUNDIR}/workdir"

# Linux's pty input queue is a few KB; the truncation threshold measured on it
# sat near 11.5 KB. The ladder therefore straddles it: the small sizes must have
# passed BEFORE the fix too (a harness that only ever goes green at the end
# cannot show it was measuring the right thing), and the large ones are the
# regression. 128 KB is a realistic paste of a config file or a stack trace.
#
# **Every threshold named above is a LINUX number, and the reported case was a
# Mac.** macOS inherits BSD's `TTYHOG` cap on the input queue -- historically
# 1024 bytes, against Linux's ~11.5 KB -- so the SAME 4 KB paste arrives whole
# through a Linux server and truncated at about 1 KB through a macOS one, from a
# platform constant with no difference in the code path whatsoever. That is what
# disguised a SIZE bug as a local-vs-remote ROUTING one for the user, whose
# remote is a Mac: it "worked locally and not on the remote".
#
# So a reader on a Mac who sees even the 2000-byte case fail is looking at THIS
# bug, not a new one. The ladder is deliberately not made platform-conditional:
# the harness runs where it runs, and the assertion it makes -- the bytes that
# came back are the bytes that went in -- needs no threshold to be true.
SIZES = (2_000, 8_000, 32_000, 128_000)

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def payload(size):
    """`size` bytes of 64-byte NUMBERED lines, so a lost or reordered chunk is
    identifiable rather than merely a smaller number."""
    out = []
    n = 0
    while len(out) * 64 < size:
        out.append(f"line{n:06d}-" + "x" * 52 + "\n")
        n += 1
    return "".join(out).encode()


def settled_size(path, expected, quiet=3.0, limit=60.0):
    """Wait for `path` to reach `expected` bytes, or to stop growing, and return
    the size seen.

    A fixed sleep would race the shell on a loaded machine and turn this into a
    flaky truncation report, which is the one failure mode that must stay
    trustworthy here.

    **A file that does not exist yet is NOT SETTLED, it is NOT STARTED**, and
    conflating the two is what made this harness report a green fix as a total
    loss. The first version seeded `last = -1` and mapped a missing file to `-1`
    as well, so on the very first poll `cur == last` and the "unchanged for
    `quiet` seconds" branch fired: it returned after one second having measured
    nothing at all, the caller sent Ctrl-D into a `cat` that had not yet created
    its file, and every size but the smallest was reported as `received 0`. The
    files on disk afterwards were all exactly right. Whether it went red came
    down to whether the shell won a 1.1s race, so it passed on one machine and
    failed on another against the identical tree.

    Returning as soon as `expected` is reached is what makes the common case
    EXACT rather than inferred -- there is nothing to settle for once every byte
    has landed. The settle-with-timeout below is kept for the case that matters:
    on a REGRESSED build the size stalls partway (11776 on Linux) and must be
    reported as the truncation it is, promptly, instead of hanging out the whole
    limit.
    """
    end = time.time() + limit
    last, stable_since = None, None
    while time.time() < end:
        if not os.path.exists(path):
            # Not started. Deliberately does not touch `stable_since`.
            time.sleep(0.05)
            continue
        cur = os.path.getsize(path)
        if cur >= expected:
            return cur
        if cur != last:
            last, stable_since = cur, time.time()
        elif time.time() - stable_since >= quiet:
            return cur  # stalled short: a truncation, for the caller to compare
        time.sleep(0.05)
    # Out of time. `None` means the file never appeared at all, which is a
    # different failure from a short one and must not read as "0 bytes settled".
    return -1 if last is None else last


def main():
    print(f"rundir: {RUNDIR}")
    shutil.rmtree(RUNDIR, ignore_errors=True)
    srv = Server(RUNDIR).start()
    os.makedirs(WORKDIR, exist_ok=True)
    try:
        cli = Client(srv.sock)
        cli.hello()
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": 100, "rows": 30}})
        cli.drain(1.0)

        for size in SIZES:
            path = f"{WORKDIR}/paste-{size}.txt"
            data = payload(size)
            cli.send({"Input": {"data": list(f"cat > {path}\n".encode())}})
            cli.drain(0.8)
            # ONE message, the way a real paste arrives: crossterm delivers the
            # whole clipboard as a single `Event::Paste`.
            cli.send({"Input": {"data": list(data)}})
            got = settled_size(path, len(data))
            cli.send({"Input": {"data": [4]}})  # Ctrl-D ends `cat`
            cli.drain(1.0)
            actual = open(path, "rb").read() if os.path.exists(path) else b""
            check(
                actual == data,
                f"{len(data)} bytes pasted arrive whole "
                f"(the pane received {len(actual)})",
            )
            if actual != data and actual:
                # Name WHERE it diverged: a truncation and a dropped middle
                # chunk are different bugs and a length alone cannot tell them
                # apart.
                first = next(
                    (i for i in range(min(len(actual), len(data)))
                     if actual[i] != data[i]),
                    min(len(actual), len(data)),
                )
                print(f"      first divergence at byte {first} of {len(data)}")
            if got == -1:
                # Names the one case the content comparison above cannot: the
                # pane never created the file, so there is nothing to diff.
                print("      the file never appeared within the wait limit")

        log = srv.log()
        # PRESENCE FIRST. This is the check that can actually go red if the
        # queue is never wired up, and the absence checks below are worth
        # nothing without it -- see the docstring.
        check(
            "pty: start_writer watching fd=" in log,
            "the writer task is wired up (server.log names start_writer)",
        )
        # The writer abandoned a buffer part-way and kept going: the modern
        # spelling of the bug this file exists for.
        check(
            "write failed after" not in log,
            "no queued PTY write is abandoned mid-way (server.log)",
        )
        # `PaneData::send_input` could not queue at all -- the writer task had
        # already exited. Different cause, same user-visible loss.
        check(
            "input byte(s) for a pane whose writer has exited" not in log,
            "no input is dropped for a departed writer (server.log)",
        )
        # The panic hook routes `std::panic` through the log crate, so this
        # really can go red -- see CLAUDE.md on the 66 sites where it could not.
        check("panicked at" not in log, "no panic in server.log")
    finally:
        srv.kill()
        # In the `finally` so a crash mid-run litters /tmp no more than a clean
        # one does. Kept ONLY on a failure, where the server log and the
        # half-written files ARE the evidence -- an earlier investigation here
        # lost a run's `server.log` to eager cleanup and could not then confirm
        # what had corrupted it. The path is printed below when it is kept.
        if not failures:
            shutil.rmtree(RUNDIR, ignore_errors=True)

    print()
    if failures:
        # Kept on failure: the server log and the half-written files are the
        # evidence. A pid-unique dir would otherwise accumulate in /tmp for ever.
        print(f"FAILED ({len(failures)}): " + "; ".join(failures))
        print(f"      evidence kept in {RUNDIR}")
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
