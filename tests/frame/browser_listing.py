#!/usr/bin/env python3
"""Frame-level test: `ListDirectory` and `OpenInSplit`, the browser panel's
server half (PROTOCOL_VERSION 9).

The panel itself is client-composited and only a PTY can see it
(`tests/pty/sidebar_browser.py`); this covers the half that lives on the SERVER,
which is where the listing and the editor resolution deliberately happen.

What it covers:

  1  a fixture directory lists, sorted directories-first then case-insensitively
     by name, with hidden entries INCLUDED (the client is what hides them)
  2  a nonexistent path answers with `error`, not with an empty list -- an empty
     list that might mean "empty" and might mean "gone" is the ambiguity the
     field exists to remove
  3  a directory the session has nothing to do with still lists rather than
     being refused or panicking
  4  a symlink to a directory is `is_dir` AND `is_symlink`, so Enter descends it
  5  the listing is capped, and says so
  6  `OpenInSplit` creates a pane in the LAYOUT running the requested command
     with the FILE as its argument -- asserted from the composited frame, not
     from the pane count, because a split running a plain shell would pass a
     count check
  7  with no `command`, the editor is the SERVER's `$EDITOR`
  8  with neither, it falls back to `vi` -- checked by the argv the server logs,
     since `vi` may not be installed and its behaviour is not ours to depend on
  9  `OpenInSplit` from a client with no attached session is ignored, not fatal

Run from the repo root:
    python3 tests/frame/browser_listing.py [-v]
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Client, Server, name_of, only  # noqa: E402

RUN = "/tmp/rmxbrowse"
FIX = f"{RUN}/fixture"
BIGDIR = f"{RUN}/big"
VERBOSE = "-v" in sys.argv
FAILURES = []

# The stand-in editors. Each prints a marker and the BASENAME of the file it was
# given, on separate short lines so neither can be broken by the wrap of a
# narrow split -- and then BLOCKS. One that printed and exited would leave a
# dead pane before the assertion ran, and the failure would read as a flake.
def editor(tag):
    return (
        "#!/bin/sh\n"
        f"printf '{tag}\\n'\n"
        "printf 'F=%s\\n' \"$(basename \"$1\")\"\n"
        "while :; do sleep 1; done\n"
    )


def log(*a):
    if VERBOSE:
        print(*a)


def check(name, cond, detail=""):
    if cond:
        print(f"  PASS  {name}")
    else:
        print(f"  FAIL  {name}\n        {detail}")
        FAILURES.append(name)


def build_fixture():
    """A tree with the orderings and the special cases the assertions need."""
    for d in ("bin", "fixture", "fixture/zeta", "fixture/Alpha", "fixture/real",
              "outside"):
        os.makedirs(f"{RUN}/{d}", exist_ok=True)
    for f in ("Beta.txt", "apple.txt", ".hidden"):
        with open(f"{FIX}/{f}", "w") as fh:
            fh.write("x\n")
    link = f"{FIX}/dirlink"
    if not os.path.lexists(link):
        os.symlink(f"{FIX}/real", link)
    with open(f"{RUN}/outside/faraway.txt", "w") as fh:
        fh.write("x\n")
    for name, tag in (("cfg-editor", "EDITING"), ("env-editor", "ENVEDIT")):
        p = f"{RUN}/bin/{name}"
        with open(p, "w") as fh:
            fh.write(editor(tag))
        os.chmod(p, 0o755)
    os.makedirs(BIGDIR, exist_ok=True)
    if len(os.listdir(BIGDIR)) < 5010:
        for i in range(5010):
            open(f"{BIGDIR}/f{i:06}", "w").close()


# ---------------------------------------------------------------------------
# wire helpers
# ---------------------------------------------------------------------------

def listing(c, path):
    c.send({"ListDirectory": {"path": path}})
    end = time.time() + 5.0
    while time.time() < end:
        msg = c.recv()
        if name_of(msg) == "DirectoryListing":
            return only(msg, "DirectoryListing")
    raise SystemExit(f"no DirectoryListing for {path}")


class Grid:
    """The composited screen, rebuilt from FullRender + RenderDiff.

    The editor's own output arrives as a DIFF after the split's FullRender, so a
    check that only read FullRenders would be looking at the frame from the
    instant before the program it is asserting about had printed anything.
    """

    def __init__(self):
        self.rows = []

    def feed(self, msg):
        n = name_of(msg)
        if n == "FullRender":
            self.rows = [[cell["c"] for cell in row]
                         for row in only(msg, "FullRender")["cells"]]
        elif n == "RenderDiff":
            for ch in only(msg, "RenderDiff")["changes"]:
                y, x = ch["y"], ch["x"]
                if y < len(self.rows) and x < len(self.rows[y]):
                    self.rows[y][x] = ch["cell"]["c"]

    def text(self):
        return "\n".join("".join(r) for r in self.rows)


def pump(c, g, t=3.0):
    """Feed everything arriving within `t` seconds into `g`."""
    end = time.time() + t
    old = c.s.gettimeout()
    try:
        while time.time() < end:
            c.s.settimeout(0.2)
            try:
                g.feed(c.recv())
            except Exception:
                pass
    finally:
        c.s.settimeout(old)
    return g.text()


def attached(srv, session="main"):
    """A client attached to a fresh session, and the Grid tracking its screen.

    The grid is fed from the ATTACH onwards rather than started empty at the
    moment of interest: `broadcast_full_render` sends a DIFF when few cells
    changed, so a split's frame usually arrives as changes against a baseline
    the caller must already hold. Starting empty made the assertion read an
    empty screen and fail for a reason that had nothing to do with the split.
    """
    c = Client(srv.sock)
    c.hello()
    c.send({"CreateSession": {"name": session, "folder": None}})
    c.send({"Attach": {"session_name": session}})
    c.send({"Resize": {"cols": 100, "rows": 30}})
    g = Grid()
    pump(c, g, 1.2)
    return c, g


def pane_count(c, session="main"):
    c.send("ListSessionTree")
    end = time.time() + 5.0
    while time.time() < end:
        msg = c.recv()
        if name_of(msg) != "SessionTree":
            continue
        body = only(msg, "SessionTree")
        for s in list(body["unfiled"]) + [s for f in body["folders"] for s in f["sessions"]]:
            if s["name"] == session:
                return sum(len(t["panes"]) for t in s["tabs"])
    raise SystemExit("no SessionTree")


# ---------------------------------------------------------------------------
# the scenario
# ---------------------------------------------------------------------------

def listing_cases(srv):
    c, _ = attached(srv)
    got = listing(c, FIX)
    names = [e["name"] for e in got["entries"]]
    log("listing:", names)
    check("1 directories sort first, then case-insensitively by name",
          names == ["Alpha", "dirlink", "real", "zeta", ".hidden",
                    "apple.txt", "Beta.txt"], names)
    check("1 hidden entries are on the wire (the client is what hides them)",
          ".hidden" in names, names)
    check("1 a successful listing carries no error", got["error"] is None, got)

    gone = listing(c, f"{FIX}/no-such-directory")
    check("2 a missing directory reports why rather than looking empty",
          gone["entries"] == [] and gone["error"] == "not found", gone)

    far = listing(c, f"{RUN}/outside")
    check("3 a directory outside the session's reach still lists",
          [e["name"] for e in far["entries"]] == ["faraway.txt"], far)

    dl = next(e for e in got["entries"] if e["name"] == "dirlink")
    check("4 a symlinked directory is both, so Enter descends it",
          dl["is_dir"] and dl["is_symlink"], dl)
    check("5 an ordinary directory is not flagged truncated",
          got["truncated"] is False, got)

    capped = listing(c, BIGDIR)
    check("5 a directory over the cap is truncated AND says so",
          capped["truncated"] is True and len(capped["entries"]) == 5000,
          {"truncated": capped["truncated"], "n": len(capped["entries"])})
    c.close()


def configured_editor_case(srv):
    c, g = attached(srv)
    before = pane_count(c)
    pump(c, g, 0.5)
    c.send({"OpenInSplit": {"path": f"{FIX}/apple.txt",
                            "command": f"{RUN}/bin/cfg-editor",
                            "vertical": True}})
    body = pump(c, g, 3.0)
    after = pane_count(c)
    log(body)
    check("6 the split exists", after == before + 1, (before, after))
    check("6 and the new pane is RUNNING the requested command, on the file",
          "EDITING" in body and "F=apple.txt" in body, repr(body))
    c.close()


def server_editor_case(srv):
    c, g = attached(srv)
    pump(c, g, 0.5)
    c.send({"OpenInSplit": {"path": f"{FIX}/Beta.txt", "command": None,
                            "vertical": False}})
    body = pump(c, g, 3.0)
    check("7 with no command, the SERVER's own $EDITOR opens the file",
          "ENVEDIT" in body and "F=Beta.txt" in body, repr(body))
    c.close()


def fallback_and_no_session_case(srv):
    c, g = attached(srv)
    pump(c, g, 0.5)
    c.send({"OpenInSplit": {"path": f"{FIX}/apple.txt", "command": None,
                            "vertical": True}})
    pump(c, g, 1.5)
    before = pane_count(c)

    # An UNATTACHED client has no tab to split, and must not split anyone
    # else's. "Ignored, not fatal" cannot be checked by the connection dying:
    # `handle_client_message`'s `Err` is logged and answered with a
    # `ServerMessage::Error`, never a disconnect, so a check that only watched
    # the socket would pass however the arm behaved -- proved by making the arm
    # `bail!`, which left the check green. What IS falsifiable is that no pane
    # appeared anywhere: an implementation reaching for "the first session" or
    # "the active one" would split a session this client never named.
    u = Client(srv.sock)
    u.hello()
    u.send({"OpenInSplit": {"path": f"{FIX}/apple.txt",
                            "command": f"{RUN}/bin/cfg-editor",
                            "vertical": True}})
    time.sleep(1.0)
    after = pane_count(c)
    check("9 OpenInSplit from a client with no attached session splits nothing",
          after == before, (before, after))
    u.close()
    c.close()


def scenario():
    build_fixture()

    srv = Server(f"{RUN}/srv")
    srv.start()
    try:
        listing_cases(srv)
        configured_editor_case(srv)
    finally:
        srv.kill()
    check("no panic in the listing server's log",
          "panicked at" not in srv.log(), srv.log()[-1500:])

    # A fresh server carrying an $EDITOR of its own, so "the server resolved it"
    # and "the request named it" produce DIFFERENT markers.
    os.environ["EDITOR"] = f"{RUN}/bin/env-editor"
    srv2 = Server(f"{RUN}/srv2")
    srv2.start()
    try:
        server_editor_case(srv2)
    finally:
        srv2.kill()
        del os.environ["EDITOR"]
    check("no panic in the $EDITOR server's log",
          "panicked at" not in srv2.log(), srv2.log()[-1500:])

    # And one with no $EDITOR at all, for the `vi` fallback.
    srv3 = Server(f"{RUN}/srv3")
    srv3.start()
    try:
        fallback_and_no_session_case(srv3)
    finally:
        srv3.kill()
    body = srv3.log()
    opens = [l for l in body.splitlines() if "OpenInSplit" in l]
    check("8 with neither a command nor a server $EDITOR, it falls back to vi",
          any('program="vi"' in l for l in opens), opens[-3:])
    check("no panic in the fallback server's log", "panicked at" not in body,
          body[-1500:])


def main():
    if not os.path.exists(BIN):
        raise SystemExit(f"{BIN} not found; run `cargo build` first")
    # The server must log at info level for check 8 to have anything to read.
    os.environ["RUST_LOG"] = os.environ.get("RUST_LOG", "info")
    os.environ.pop("EDITOR", None)
    scenario()
    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
