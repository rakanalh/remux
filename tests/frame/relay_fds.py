"""Reading the ssh shim's descriptor listing for msg_remote's case 1r.

The shim writes `ls -l /proc/<pid>/fd` on Linux and `lsof` on macOS, and the
check must read both: a parser that knows only one form finds no leaked
descriptor in the other and passes on nothing.
"""
import re

# Terminal and server files the relay must not inherit. `/dev/tty` covers the
# macOS `/dev/ttysNNN` and the controlling-terminal alias on both platforms.
FORBIDDEN = ("/dev/pts", "/dev/tty", "ptmx", "remux.sock", "state.json")


def listed_fds(lines):
    """(fd, name) for each numbered descriptor in either listing form:
    `ls -l` (`... 3 -> name`) or `lsof` (`sh <pid> <user> 3u <type> ... name`)."""
    fds = []
    for ln in lines:
        m = re.search(r" (\d+) -> (.*)$", ln)
        if m:
            fds.append((int(m.group(1)), m.group(2)))
            continue
        cols = ln.split()
        m = re.fullmatch(r"(\d+)\D*", cols[3]) if len(cols) > 4 else None
        if m:
            fds.append((int(m.group(1)), " ".join(cols[8:]) or cols[-1]))
    return fds


def verdict(lines):
    """`(captured, extra, leaked)` for a listing. `captured` is whether 0, 1
    and 2 were read out of it, without which the other two prove nothing.
    The one descriptor past 2 allowed is the shim's own script, which the
    shell holds open and closes at `exec`."""
    fds = listed_fds(lines)
    captured = {0, 1, 2} <= {fd for fd, _ in fds}
    extra = [(fd, name) for fd, name in fds if fd > 2 and not name.endswith("/ssh")]
    leaked = [k for k in FORBIDDEN if any(k in ln for ln in lines)]
    return captured, extra, leaked
