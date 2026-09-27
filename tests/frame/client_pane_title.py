#!/usr/bin/env python3
"""Frame-level test: each client's `pane_title` governs what it is sent.

The server's own template (`srv {command}`) is only the fallback. Three clients
attach to the same session:

  A  sends `PaneTitle { template: "A {command}", host: "" }`
  B  sends `PaneTitle { template: "{host}/{command}", host: "boxb" }`
  C  sends nothing, like an older client or the CLI

  1. each client's pane borders use its own naming (A `A sh`, B `boxb/sh`,
     C `srv sh`), and never another client's
  2. so does each client's session tree
  3. a client that changes its template is re-rendered at once
  4. a separator left dangling by an empty placeholder is trimmed
     (`{title} - {command}` on an untitled pane reads `sh`)
  5. with mixed namings on one session, pane output alone (no Resize) leaves
     each client's border right from diffs, also after the client whose
     naming the shared frame followed has left

Run: python3 tests/frame/client_pane_title.py
"""
import os
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of, only  # noqa: E402

RUNDIR = f"/tmp/rmx-cpt-{os.getpid()}"
COLS, ROWS = 100, 30
FAILS = []


def check(cond, msg):
    print(("PASS " if cond else "FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


class View:
    def __init__(self):
        self.g = [[" "] * COLS for _ in range(ROWS)]
        self.tree = None

    def feed(self, msgs):
        for m in msgs:
            n = name_of(m)
            body = only(m, n)
            if n == "FullRender":
                self.g = [[" "] * COLS for _ in range(ROWS)]
                for y, row in enumerate(body["cells"]):
                    for x, cell in enumerate(row):
                        if y < ROWS and x < COLS:
                            self.g[y][x] = cell.get("c", " ")
            elif n == "RenderDiff":
                for ch in body["changes"]:
                    if ch["y"] < ROWS and ch["x"] < COLS:
                        self.g[ch["y"]][ch["x"]] = ch["cell"].get("c", " ")
            elif n == "SessionTree":
                self.tree = body

    def border(self):
        """The rows above the status bar, where pane borders are drawn."""
        return "\n".join("".join(r) for r in self.g[:-1])


def attach(c, naming):
    c.hello()
    if naming is not None:
        template, host = naming
        c.send({"PaneTitle": {"template": template, "host": host}})
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})


def frame(c, v):
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})
    end = time.time() + 1.0
    while time.time() < end:
        v.feed(c.drain(0.2))
    return v.border()


def tree_names(c, v):
    v.tree = None
    c.send("ListSessionTree")
    end = time.time() + 1.5
    while time.time() < end and v.tree is None:
        try:
            v.feed([c.recv()])
        except socket.timeout:
            pass
    out = []
    for s in (v.tree or {}).get("unfiled", []):
        for t in s["tabs"]:
            out += [p["name"] for p in t["panes"]]
    return out


def run_diffs(srv, sh):
    """5. Mixed namings on one session, repainted by pane output alone (no
    Resize): each client's border must come out right from the diffs it is
    sent, including after the client whose naming the shared frame followed
    has left."""
    setup = Client(srv.sock)
    setup.hello()
    setup.send({"CreateSession": {"name": "diff", "folder": None}})
    setup.drain(0.5)
    namings = {"A": ("A {command}", f"A {sh}"), "B": ("B {command}", f"B {sh}"), "C": (None, f"srv {sh}")}
    cs = {}
    for k, (template, _) in namings.items():
        c = Client(srv.sock)
        c.hello()
        if template:
            c.send({"PaneTitle": {"template": template, "host": ""}})
        c.send({"Attach": {"session_name": "diff"}})
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        cs[k] = (c, View())
    time.sleep(2.0)
    for c, v in cs.values():
        v.feed(c.drain(0.5))

    def top(v):
        return "".join(v.g[0])

    for i in range(5):
        cs["C"][0].send({"Input": {"data": list(f"printf 'L%s:%s\\n' x {i}\r".encode())}})
        time.sleep(0.3)
    time.sleep(0.8)
    for k, (c, v) in cs.items():
        v.feed(c.drain(0.6))
        want = namings[k][1]
        others = [w for j, (_, w) in namings.items() if j != k]
        check(v.border().count("Lx:4") >= 1 and top(v).startswith(f"╭ {want} ─")
              and not any(o in top(v) for o in others),
              f"5 client {k}'s border is {want!r} from diffs alone ({top(v)[:30]!r})")

    for leaving in ("A", "B"):
        c, _ = cs.pop(leaving)
        c.close()
        time.sleep(0.3)
        mark = f"M{leaving}"
        cs["C"][0].send({"Input": {"data": list(f"printf 'M%s\\n' {leaving}\r".encode())}})
        time.sleep(0.8)
        for k, (c, v) in cs.items():
            v.feed(c.drain(0.6))
            want = namings[k][1]
            check(mark in v.border() and top(v).startswith(f"╭ {want} ─"),
                  f"5 after {leaving} left, client {k}'s border is still {want!r} ({top(v)[:30]!r})")
    for c, _ in cs.values():
        c.close()
    setup.close()


def main():
    srv = Server(RUNDIR).start(config='[appearance]\npane_title = "srv {command}"\n')
    try:
        setup = Client(srv.sock)
        setup.hello()
        setup.send({"CreateSession": {"name": "main", "folder": None}})
        setup.drain(0.5)
        clients = {
            "A": (Client(srv.sock), ("A {command}", ""), "A {sh}"),
            "B": (Client(srv.sock), ("{host}/{command}", "boxb"), "boxb/{sh}"),
            "C": (Client(srv.sock), None, "srv {sh}"),
        }
        views = {k: View() for k in clients}
        for k, (c, naming, _) in clients.items():
            attach(c, naming)
        time.sleep(2.5)
        for k, (c, _, _) in clients.items():
            views[k].feed(c.drain(0.5))
        # The shell's name as this server reports it (`sh` on Linux), read
        # through the server's own template by the client that sent none, once
        # the attached pane's names have been read from the process.
        sh = tree_names(clients["C"][0], views["C"])[0].removeprefix("srv ")
        clients = {k: (c, n, w.format(sh=sh)) for k, (c, n, w) in clients.items()}

        others = {k: [w for j, (_, _, w) in clients.items() if j != k] for k in clients}
        for k, (c, _, want) in clients.items():
            border = frame(c, views[k])
            check(want in border and not any(o in border for o in others[k]),
                  f"1 client {k}'s border reads {want!r} and no other client's name")
            names = tree_names(c, views[k])
            check(names == [want], f"2 client {k}'s session tree names the pane {want!r} ({names})")

        a, _, _ = clients["A"]
        a.send({"PaneTitle": {"template": "{title} - {command}", "host": ""}})
        end = time.time() + 1.5
        while time.time() < end:
            views["A"].feed(a.drain(0.2))
        border = views["A"].border()
        check(border.splitlines()[0].startswith(f"╭ {sh} ─"),
              f"3/4 a new template re-renders at once, trimmed to {sh!r} ({border.splitlines()[0]!r})")
        check(f"A {sh}" not in border and f" - {sh}" not in border and f"{sh} -" not in border,
              "4 no dangling separator and nothing of the old template")
        check(tree_names(a, views["A"]) == [sh], f"4 the tree reads {sh!r} too")
        b, _, _ = clients["B"]
        check(f"boxb/{sh}" in frame(b, views["B"]), "3 the other client is unaffected")
        for c, _, _ in clients.values():
            c.close()
        run_diffs(srv, sh)
        check("panicked" not in srv.log(), "no panic in the server log")
    finally:
        srv.kill()
    print(f"\n{'FAILED' if FAILS else 'OK'}: {len(FAILS)} failure(s)")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()
