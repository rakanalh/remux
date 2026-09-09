"""Bracketed paste is a HANDSHAKE: remux must wrap a paste only for a pane that
ASKED to be sent the markers (`CSI ? 2004 h`), and send the bare text otherwise.

remux used to do neither half -- mode 2004 fell into the DECSET catch-all, and
the client wrapped every paste unconditionally. A raw-mode reader that never
asked therefore got 12 bytes of garbage around the text: vim read the leading
`ESC` as the Escape key and ran `[200~` as normal-mode commands, an older
readline left a stray `00~` in the line.

**Why this cannot be a frame-level test.** The decision is made in the CLIENT,
off a crossterm `Event::Paste`. The frame harness can only send `Input`, which
is the already-wrapped bytes -- it would pass against a client that still wraps
unconditionally, testing nothing.

The assertion is on what the program INSIDE the pane RECEIVED, read as bytes in
Python, never off the rendered screen: each case runs `cat > <file>` in the
pane, pastes, and the file is then compared byte for byte. A shell cannot echo
a file into existence, and pyte cannot tell us what the line discipline handed
to `cat`.

Four cases:
  1. mode ON  -> the file holds `ESC[200~TEXT ESC[201~`.
  2. mode OFF -> the file holds TEXT and NEITHER marker anywhere (the bug).
  3. mode ON again -> wrapped again. A flag that latches one way passes 1 and 2
     and is the regression most likely to ship; only re-arming catches it.
  4. VIEW -> the paste goes to the FOCUSED CELL's pane by identity, so it must
     use THAT pane's flag. Set up so the two disagree: the foreground pane has
     2004 ON, both cell panes have it OFF. A client that read the foreground
     flag (stale-true from before `Detach`) wraps and goes red.
  5. A BARE mode flip -- `2004l` with NO visible output after it, which is the
     real-world shape (readline emits it, then bash execs a command that prints
     nothing). Cases 1-3 each follow their flip with a `printf`, so every one of
     them is carried to the client by a frame that had something to draw anyway;
     none of them would notice a server that skipped a render with an empty
     diff. This one would. (It RUNS before case 4 -- once a View is composed the
     client stays in it, and this case needs the ordinary foreground path.)

Notes on the shape of each pane command, which is load-bearing:
  * `/bin/sh` is bash on many distros, and readline emits `2004h` before every
    line it reads and `2004l` after. So the mode-setting `printf` and the `cat`
    that reads the paste MUST be on one command line -- the printf then runs
    last, and no prompt intervenes to re-arm the mode before the paste lands.
  * Every marker is ASSEMBLED by the pane (`printf 'ARM:%s\\n' MODEON1`), so the
    string searched for (`ARM:MODEON1`) never appears in the echoed command
    line. A marker typed literally is on screen whether or not anything ran.
  * The wait before each paste is a CONDITION, not a sleep: `ARM:...` is
    printed by the same burst of PTY output that set the mode, so the frame
    carrying it was necessarily composited after the flag changed -- a paste
    fired before it would be testing the old value.
  * `DONE:...` is waited for before the file is read, so we never race `cat`'s
    close; it also proves Ctrl-D reached `cat` through the client, rather than
    leaving us reading a half-written file.
"""
import os
import shutil
import sys
import time

from pty_harness import Tui, sm_compose_view

RUNDIR = "/tmp/rmx-bpaste"
OUTDIR = f"{RUNDIR}/paste"

OPEN_MARK = b"\x1b[200~"
CLOSE_MARK = b"\x1b[201~"


def wait_for(t, needle, timeout=8.0):
    """Pump until `needle` is on screen. A condition, never a fixed sleep."""
    end = time.time() + timeout
    while time.time() < end:
        if t.has(needle):
            return True
        t.pump(0.15)
    return False


def paste(t, text, wait=0.8):
    """Send a real bracketed-paste sequence, as a terminal does on Ctrl+Shift+V.

    crossterm turns this into a single `Event::Paste`, which is the ONLY input
    path that reaches the code under test.
    """
    t.send(OPEN_MARK + text.encode() + CLOSE_MARK, wait)


def arm(t, mode, tag, path):
    """Set 2004 in the pane, announce it, and park `cat` on the given file.

    One command line, deliberately -- see the module docstring.
    """
    hl = "h" if mode else "l"
    t.send(
        f"printf '\\033[?2004{hl}'; printf 'ARM:%s\\n' {tag}; "
        f"cat > {path}; printf 'DONE:%s\\n' {tag}\r",
        0.4,
    )
    if not wait_for(t, f"ARM:{tag}"):
        t.dump(f"arm {tag}")
        raise AssertionError(f"pane never printed ARM:{tag} -- mode {hl} not applied")


def finish(t, tag, wait=1.2):
    """End the pasted line and EOF `cat`, then wait for it to have closed."""
    t.send(b"\r", 0.3)
    t.send(b"\x04", wait)
    return wait_for(t, f"DONE:{tag}")


def read_out(name):
    p = f"{OUTDIR}/{name}"
    if not os.path.exists(p):
        return None
    with open(p, "rb") as f:
        return f.read()


def check_wrapped(label, data, text):
    """The pane asked for markers, so it must have received exactly them."""
    if data is None:
        print(f"FAIL({label}): no output file -- the paste never reached the pane")
        return False
    want = OPEN_MARK + text.encode() + CLOSE_MARK
    ok = want in data
    print(f"{label}: wrapped={ok} bytes={data!r}")
    if not ok:
        print(f"FAIL({label}): expected {want!r} in the pane's input")
    return ok


def check_bare(label, data, text):
    """The pane never asked, so the markers must be nowhere in what it read."""
    if data is None:
        print(f"FAIL({label}): no output file -- the paste never reached the pane")
        return False
    has_text = text.encode() in data
    leaked = OPEN_MARK in data or CLOSE_MARK in data
    print(f"{label}: text={has_text} markers_leaked={leaked} bytes={data!r}")
    if not has_text:
        print(f"FAIL({label}): the pasted text never arrived")
    if leaked:
        print(f"FAIL({label}): bracketed-paste markers sent to a pane that never asked")
    return has_text and not leaked


def main():
    shutil.rmtree(RUNDIR, ignore_errors=True)
    t = Tui(RUNDIR, cols=120, rows=40).start()
    os.makedirs(OUTDIR, exist_ok=True)

    # --- 1. mode ON: the pane asked, so it gets the markers ----------------
    t.send("clear\r", 0.4)
    arm(t, True, "MODEON1", f"{OUTDIR}/on1")
    paste(t, "PASTE_ONE")
    finish(t, "MODEON1")
    case1 = check_wrapped("1 mode-on", read_out("on1"), "PASTE_ONE")

    # --- 2. mode OFF: the bug -- no markers may reach the pane -------------
    arm(t, False, "MODEOFF", f"{OUTDIR}/off")
    paste(t, "PASTE_TWO")
    finish(t, "MODEOFF")
    case2 = check_bare("2 mode-off", read_out("off"), "PASTE_TWO")

    # --- 3. mode ON again: the flag must not have latched off --------------
    arm(t, True, "MODEON2", f"{OUTDIR}/on2")
    paste(t, "PASTE_THREE")
    finish(t, "MODEON2")
    case3 = check_wrapped("3 mode-on-again", read_out("on2"), "PASTE_THREE")

    # --- 5. a BARE mode flip: nothing visible changes after it -------------
    # The marker comes BEFORE the flip, so `2004l` is the last thing the pane
    # emits and it draws nothing. The settle below is a plain pump and not a
    # condition, because there is by construction nothing to wait FOR -- but it
    # errs safe: it can only give the flag more time to arrive, and if no frame
    # ever carries it the paste is wrapped and this case goes red.
    t.send(f"printf 'ARM:%s\\n' BARE; printf '\\033[?2004l'; cat > {OUTDIR}/bare; "
           f"printf 'DONE:%s\\n' BARE\r", 0.4)
    if not wait_for(t, "ARM:BARE"):
        t.dump("bare"); t.kill(); print("FAIL(setup): bare case never armed"); sys.exit(1)
    t.pump(1.5)
    paste(t, "PASTE_BARE")
    finish(t, "BARE")
    case5 = check_bare("5 bare-flip", read_out("bare"), "PASTE_BARE")

    # --- 4. VIEW: the FOCUSED CELL's flag decides, not the foreground's ----
    # Both cell panes park `cat` with 2004 OFF (so whichever cell takes focus,
    # the right answer is "no markers"), while the foreground pane is left
    # parked with 2004 ON. The two flags therefore disagree, which is the only
    # arrangement that can tell the implementations apart.
    t.send(f"printf '\\033[?2004l'; printf 'ARM:%s\\n' CELLA; cat > {OUTDIR}/v_a; "
           f"printf 'DONE:%s\\n' CELLA\r", 0.4)
    if not wait_for(t, "ARM:CELLA"):
        t.dump("cell A"); t.kill(); print("FAIL(setup): cell A never armed"); sys.exit(1)
    t.prefix(b"pv", 0.8)                       # split -> pane 1, now focused
    t.send(f"printf '\\033[?2004l'; printf 'ARM:%s\\n' CELLB; cat > {OUTDIR}/v_b; "
           f"printf 'DONE:%s\\n' CELLB\r", 0.4)
    if not wait_for(t, "ARM:CELLB"):
        t.dump("cell B"); t.kill(); print("FAIL(setup): cell B never armed"); sys.exit(1)
    # A new tab, so the two panes above are NOT session-visible (else the cells
    # render the "Active in session" placeholder instead of the live panes), and
    # so the foreground pane is a different one. `sleep` parks it away from a
    # prompt, pinning its flag ON rather than trusting the shell's own re-arm.
    t.send(b"\x1bt", 0.8)                      # Alt+t: new tab
    t.send("printf '\\033[?2004h'; printf 'ARM:%s\\n' FGON; sleep 300\r", 0.4)
    if not wait_for(t, "ARM:FGON"):
        t.dump("foreground"); t.kill(); print("FAIL(setup): foreground never armed"); sys.exit(1)

    sm_compose_view(t, tab="Tab 1", panes=(0, 1), settle=1.2)
    if not t.has("View 1"):
        t.dump("view setup"); t.kill(); print("FAIL(setup): not in a view"); sys.exit(1)
    # A condition, not `settle`'s sleep: until the first `PaneContent` lands the
    # cell has no snapshot, and a snapshot-less cell falls back to WRAPPING. That
    # would fail this case for the wrong reason on a slow machine.
    if not (wait_for(t, "ARM:CELLA", 5.0) or wait_for(t, "ARM:CELLB", 5.0)):
        t.dump("view setup"); t.kill()
        print("FAIL(setup): no cell is streaming its pane yet"); sys.exit(1)

    paste(t, "PASTE_VIEW")
    t.send(b"\r", 0.3)
    t.send(b"\x04", 1.2)
    if not (wait_for(t, "DONE:CELLA", 4.0) or wait_for(t, "DONE:CELLB", 4.0)):
        t.dump("view paste")
        print("WARN(4 view): neither cell's `cat` closed; reading the files anyway")
    va, vb = read_out("v_a"), read_out("v_b")
    # The paste reached whichever cell had focus; the other file stays empty.
    got = va if (va and va.strip()) else vb
    case4 = check_bare("4 view", got, "PASTE_VIEW")
    if got is not None and (va and va.strip()) and (vb and vb.strip()):
        print("FAIL(4 view): the paste reached BOTH cells")
        case4 = False

    alive = t.alive()
    logs = (t.log("client") + t.log("server")).lower()
    panic = "panicked at" in logs
    t.kill()

    print(f"\ncase1={case1} case2={case2} case3={case3} case4={case4} "
          f"case5={case5} alive={alive} panic={panic}")
    if case1 and case2 and case3 and case4 and case5 and alive and not panic:
        print("PASS: a paste is bracketed only for a pane that asked for it")
        sys.exit(0)
    print("FAIL: bracketed paste does not follow the pane's mode 2004 state")
    sys.exit(1)


if __name__ == "__main__":
    main()
