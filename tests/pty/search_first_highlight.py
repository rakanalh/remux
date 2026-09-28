"""The FIRST search in a fresh client highlights every visible match.

Regression: `/ok<Enter>` over output whose matches all fit on screen, with no
scrollback, highlighted nothing (and `n` did not help) until the same search
was issued a second time; over real scrollback only the current match was
highlighted until the first `n`.

Highlights are painted by the CLIENT straight onto the terminal, so only a
real PTY can see them. Each case asserts, after the first Enter and nothing
else, that every on-screen occurrence of the query carries a highlight
background and that exactly one of them -- the current match -- carries the
distinct current-match background.

    python3 tests/pty/search_first_highlight.py [no-scrollback|scrollback|all]
"""
import os, re, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui  # noqa: E402

RUNDIR = "/tmp/rmxsh-first"
QUERY = "ok"
# The default theme's search colours, as pyte reports a truecolor cell.
MATCH_BG = "585b70"
CURRENT_BG = "fab387"

failures = []


def check(cond, msg):
    print(("PASS " if cond else "FAIL ") + msg)
    if not cond:
        failures.append(msg)


def write_gen(path, filler, results):
    # A script rather than a typed printf: the typed command line would itself
    # contain the query and be a match whether or not anything ran.
    with open(path, "w") as f:
        f.write("clear\n")
        for i in range(filler):
            f.write(f"echo 'filler line {i}'\n")
        for i in range(results):
            f.write(f"echo 'test case_{i} ... ok'\n")


def occurrences(tui):
    """(row, col) of every query occurrence in the pane area.

    The last two rows are the search prompt and the status bar, both of which
    echo the query without being pane content.
    """
    hits = []
    for y, row in enumerate(tui.rows_text()[:-2]):
        for m in re.finditer(re.escape(QUERY), row):
            hits.append((y, m.start()))
    return hits


def cell_bg(tui, y, x):
    return tui.screen.buffer[y][x].bg


def run_case(name, filler, results, entry):
    name = f"{name}/{entry}"
    print(f"=== {name} ===")
    rundir = f"{RUNDIR}-{name.replace('/', '-')}"
    tui = Tui(rundir, cols=100, rows=30).start()
    try:
        gen = f"{rundir}/gen.sh"
        write_gen(gen, filler, results)
        tui.send(f"sh {gen}\r", 1.5)
        before = occurrences(tui)
        check(len(before) > 0, f"{name}: output is on screen ({len(before)} occurrences)")
        # Evidence the scrollback case really has matches ABOVE the screen:
        # the first result line must have scrolled off.
        in_history = not any("case_0 " in r for r in tui.rows_text())
        check(in_history == (len(before) < results),
              f"{name}: scrollback holds matches iff not all fit ({len(before)}/{results} on screen)")
        for y, x in before:
            check(cell_bg(tui, y, x) == "default",
                  f"{name}: ({y},{x}) unhighlighted before the search")

        if entry == "leader":
            tui.prefix(b"s", 0.4)
            tui.send("s", 0.4)
        else:
            tui.prefix(b"v", 0.6)
            check("VISUAL" in tui.rows_text()[-1], f"{name}: in Visual mode")
            tui.send("/", 0.4)
        tui.send(QUERY, 0.3)
        tui.send("\r", 1.5)

        hits = occurrences(tui)
        check(len(hits) == len(before),
              f"{name}: same {len(before)} occurrences on screen after Enter (got {len(hits)})")
        bgs = [(y, x, cell_bg(tui, y, x), cell_bg(tui, y, x + len(QUERY) - 1)) for y, x in hits]
        unlit = [(y, x, a) for y, x, a, _ in bgs if a not in (MATCH_BG, CURRENT_BG)]
        check(not unlit, f"{name}: every match highlighted after the FIRST Enter "
                         f"({len(hits) - len(unlit)}/{len(hits)} lit; unlit: {unlit[:5]})")
        partial = [(y, x) for y, x, a, b in bgs if a != b]
        check(not partial, f"{name}: each highlight spans the whole match ({partial[:5]})")
        current = [(y, x) for y, x, a, _ in bgs if a == CURRENT_BG]
        others = [(y, x) for y, x, a, _ in bgs if a == MATCH_BG]
        check(len(current) == 1,
              f"{name}: exactly one match styled as current (got {current})")
        check(len(others) == len(hits) - 1,
              f"{name}: the rest styled as plain matches ({len(others)}/{len(hits) - 1})")
        if current:
            check(current[0] == max(hits),
                  f"{name}: the current match is the bottom-most one ({current[0]} vs {max(hits)})")
        if failures:
            tui.dump(name)
        check(tui.alive(), f"{name}: client still alive")
        log = tui.log("server") + tui.log("client")
        check("panicked" not in log, f"{name}: no panic in the logs")
    finally:
        tui.kill()


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "all"
    if mode in ("no-scrollback", "all"):
        for entry in ("visual", "leader"):
            run_case("no-scrollback", filler=0, results=9, entry=entry)
    if mode in ("scrollback", "all"):
        # 80 older matches push well past the screen, so there is real
        # scrollback holding matches above the visible ones.
        for entry in ("visual", "leader"):
            run_case("scrollback", filler=60, results=80, entry=entry)
    print("FAILED: %d" % len(failures) if failures else "ALL PASS")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
