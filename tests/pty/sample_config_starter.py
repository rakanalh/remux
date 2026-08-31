#!/usr/bin/env python3
"""The sidebar/agents examples in `config.sample.toml` must actually work.

A sample config that does not load is worse than none: it is the file the README
points every new user at, and its examples are the first thing anyone pastes.
So this harness does not restate them -- it EXTRACTS them from
`config.sample.toml` verbatim, uncomments them exactly as a user would, and
starts a real client against the result.

Two modes:

  sample       the two `[[sidebar]]` examples and the `[agents]` block, lifted
               out of the sample file. Asserts the client comes up, all three
               documented panels PAINT (`Sessions`, `Files`, `Agents`) with real
               content in them, and NOTHING is warned about at load.
  placeholder  the same config plus a bottom sidebar running the fourth
               documented plugin, `placeholder` -- the one the sample has no
               example for. Asserts it paints and still warns about nothing.

Run from the repo root:
    python3 tests/pty/sample_config_starter.py sample
    python3 tests/pty/sample_config_starter.py placeholder
    python3 tests/pty/sample_config_starter.py            # both
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SAMPLE = os.path.join(ROOT, "config.sample.toml")
RUNDIR = "/tmp/rmxsamp"

# A commented line that is TOML rather than prose: a table header, a key = value,
# or the blank spacer between two tables. Anything else ends the example block.
TOMLISH = re.compile(r"^(\[[^\]]+\]\]?|[A-Za-z_][A-Za-z0-9_]* *=.*)$")


def extract_blocks(anchors):
    """Lift each anchored comment block out of the sample and uncomment it.

    `anchors` are the literal commented header lines that open an example, e.g.
    `# [[sidebar]]`. Everything from there until the first line that is neither
    blank nor TOML-shaped is taken.
    """
    lines = open(SAMPLE, encoding="utf-8").read().splitlines()
    out = []
    for anchor in anchors:
        starts = [i for i, l in enumerate(lines) if l == anchor]
        if len(starts) != 1:
            raise SystemExit(
                f"expected exactly 1 {anchor!r} in config.sample.toml, found {len(starts)} "
                "-- the sample moved and this harness is no longer reading the example it names"
            )
        block = []
        for line in lines[starts[0]:]:
            if not line.startswith("#"):
                break
            body = line[1:]
            if body.startswith(" "):
                body = body[1:]
            stripped = body.strip()
            if stripped and not TOMLISH.match(stripped):
                break
            block.append(body.rstrip())
        out.append("\n".join(block).rstrip())
    return out


def build_config():
    """The two `[[sidebar]]` examples share an anchor, so take them positionally."""
    lines = open(SAMPLE, encoding="utf-8").read().splitlines()
    starts = [i for i, l in enumerate(lines) if l == "# [[sidebar]]"]
    if len(starts) != 2:
        raise SystemExit(
            f"expected 2 commented `[[sidebar]]` examples in config.sample.toml, found {len(starts)}"
        )
    blocks = []
    for s in starts:
        block = []
        for line in lines[s:]:
            if not line.startswith("#"):
                break
            body = line[1:]
            if body.startswith(" "):
                body = body[1:]
            stripped = body.strip()
            if stripped and not TOMLISH.match(stripped):
                break
            block.append(body.rstrip())
        blocks.append("\n".join(block).rstrip())
    (agents,) = extract_blocks(["# [agents]"])
    return blocks[0], blocks[1], agents


LEFT, RIGHT, AGENTS = build_config()

# The extraction is the part that can silently rot, so check it produced what
# this harness claims to be testing BEFORE anything is started. A block that
# came back empty or half-read would otherwise "pass" by configuring nothing.
for want, where in (
    ('plugin = "sessions"', LEFT),
    ('plugin = "files"', LEFT),
    ('edge = "left"', LEFT),
    ('plugin = "agents"', RIGHT),
    ('edge = "right"', RIGHT),
    ("[[agents.pattern]]", AGENTS),
    ('name = "claude-proceed"', AGENTS),
):
    if want not in where:
        raise SystemExit(f"extraction failed: {want!r} missing from\n{where}")

SAMPLE_CONFIG = f"{LEFT}\n\n{RIGHT}\n\n{AGENTS}\n"

PLACEHOLDER_CONFIG = SAMPLE_CONFIG + """
[[sidebar]]
edge = "bottom"
size = 8

  [[sidebar.panel]]
  plugin = "placeholder"
"""


def bad_lines(text):
    return [l for l in text.splitlines() if " WARN " in l or " ERROR " in l]


def check_quiet(t):
    for which in ("client", "server"):
        bad = bad_lines(t.log(which))
        assert not bad, f"{which}.log complained about the sample config:\n" + "\n".join(bad)


def run(mode):
    config = PLACEHOLDER_CONFIG if mode == "placeholder" else SAMPLE_CONFIG
    print(f"--- {mode} ---")
    print(config)
    t = Tui(RUNDIR, cols=150, rows=44, config=config).start()
    try:
        t.pump(2.5)
        assert t.alive(), "the client died on the sample config"
        rows = t.rows_text()

        for title in ("Sessions", "Agents"):
            assert t.has(title), f"the {title!r} panel never painted\n" + "\n".join(rows)

        # Content, not just chrome: each panel has to have something IN it that
        # only that panel draws. `main` alone would not do -- the status bar
        # carries the session name too, so it is on screen with no sidebar at all.
        assert t.has("\u25bc local"), (
            "the sessions panel painted no tree\n" + "\n".join(rows)
        )
        # The `files` panel's header is the DIRECTORY, not the word "Files", so
        # it is proved by what it lists: the repo the client was started in.
        for entry in ("config.sample.toml", "README.md", "src/"):
            assert t.has(entry), (
                f"the files panel is not listing {entry!r} from the repo it was "
                "started in\n" + "\n".join(rows)
            )
        assert t.has("no agents"), (
            "the agents panel painted no verdict -- with nothing running it must "
            "say `no agents`, and on a server that cannot detect, say so\n"
            + "\n".join(rows)
        )

        if mode == "placeholder":
            assert t.has("Placeholder"), (
                "the placeholder panel never painted\n" + "\n".join(rows)
            )

        check_quiet(t)
        t.dump(mode)
        print(f"OK  {mode}")
    finally:
        t.kill()


if __name__ == "__main__":
    modes = sys.argv[1:] or ["sample", "placeholder"]
    for m in modes:
        run(m)
    print("PASS")
