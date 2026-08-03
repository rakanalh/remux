"""OSC 52, end to end through the REAL client: an app copies inside a pane and
the payload lands on the outer terminal's clipboard.

The frame harness proves the server emits `CopyToClipboard`. Only a real PTY
proves the last hop -- that the client turns it back into an `OSC 52` write on
its own stdout, which is what actually reaches the system clipboard. `Tui.yanks()`
reads exactly those writes out of the raw byte stream (pyte drops OSC 52), so
this is what a terminal emulator would have put on the clipboard.

Asserted:
  - a small copy round-trips to the outer terminal
  - a multi-KB copy round-trips intact (not truncated to a fragment)
  - a clipboard READ request yields no outer write (nothing can exfiltrate)
  - garbage base64 yields no outer write
  - `allow_app_clipboard = false` yields no outer write
  - the client survives all of it, with no panic in either log

Run: python3 tests/pty/osc52_clipboard.py
"""
import os, sys
from pty_harness import Tui

RUNDIR = "/tmp/rmxosp"
MARKER = "REMUX_CLIP_7788"
BIG = "".join(f"line {i:04} of copied text\n" for i in range(400))

failures = []


def check(ok, label):
    print(f"  {'PASS' if ok else 'FAIL'}  {label}")
    if not ok:
        failures.append(label)


def copy_cmd(literal):
    """Shell that makes the pane copy `literal` via OSC 52, as a program would."""
    quoted = literal.replace("'", "'\\''")
    return (
        "printf '\\033]52;c;%s\\007' "
        f"\"$(printf '%s' '{quoted}' | base64 | tr -d '\\n')\"\r"
    )


def scenario(label, config=None):
    print(f"\n[{label}]")
    tui = Tui(f"{RUNDIR}/{'off' if config else 'on'}", config=config).start()
    return tui


def run_on():
    tui = scenario("allow_app_clipboard default (on)")
    try:
        tui.send(copy_cmd(MARKER), t=1.5)
        got = tui.yanks()
        check(got == [MARKER], f"small copy reaches the outer terminal (got {got!r})")

        # A realistic block copy, staged through a file so one command line does
        # not have to carry 10 KB.
        big_path = f"{tui.rundir}/big.txt"
        with open(big_path, "w") as f:
            f.write(BIG)
        tui.send(
            f"printf '\\033]52;c;%s\\007' \"$(base64 < {big_path} | tr -d '\\n')\"\r",
            t=2.5,
        )
        got = tui.yanks()
        check(
            len(got) == 2 and got[1] == BIG,
            f"multi-KB copy reaches the outer terminal intact "
            f"(got {len(got[1]) if len(got) > 1 else 0} bytes)",
        )

        # A read request must produce nothing new.
        before = len(tui.yanks())
        tui.send("printf '\\033]52;c;?\\007'\r", t=1.5)
        check(len(tui.yanks()) == before, "clipboard read request emits no outer write")

        # Garbage base64 likewise.
        tui.send("printf '\\033]52;c;%s\\007' '!!!not-base64!!!'\r", t=1.5)
        check(len(tui.yanks()) == before, "invalid base64 emits no outer write")

        check(tui.alive(), "client still alive")
        check("panicked" not in tui.log("client"), "no panic in the client log")
        check("panicked" not in tui.log("server"), "no panic in the server log")
    finally:
        tui.kill()


def run_off():
    tui = scenario("allow_app_clipboard = false",
                   config="[general]\nallow_app_clipboard = false\n")
    try:
        tui.send(copy_cmd(MARKER), t=1.8)
        got = tui.yanks()
        check(got == [], f"gate off: nothing reaches the outer terminal (got {got!r})")
        check(tui.alive(), "client still alive")
        check("panicked" not in tui.log("client"), "no panic in the client log")
    finally:
        tui.kill()


run_on()
run_off()

print()
if failures:
    print(f"FAILED ({len(failures)}): " + "; ".join(failures))
    sys.exit(1)
print("ALL PASS")
