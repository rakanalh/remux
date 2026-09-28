#!/usr/bin/env python3
"""Reproducible README screenshots, captured from the REAL client headlessly.

Each shot drives `target/debug/remux` through a pseudo-terminal into a pyte
screen, in a throwaway environment (its own XDG dirs, HOME and socket), then
renders that screen to HTML and has headless Chrome photograph it. Nothing here
is a mock-up: what the PNG shows is what the client painted.

Run from the repo root:
    python3 tests/pty/readme_shots.py [shot-name ...]    # no args = every shot
    python3 tests/pty/readme_shots.py --list
    python3 tests/pty/readme_shots.py --gif-selftest      # proves the GIF helper

Options:
    --keep      leave the throwaway run directory behind for inspection
    --out DIR   write PNGs somewhere other than docs/screenshots/

Needs: pexpect, pyte, google-chrome, ffmpeg (GIFs only), and ideally the
JetBrainsMono Nerd Font Mono family (falls back to any installed monospace).
"""
import html
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

import pexpect
import pyte
import pyte.graphics
from PIL import Image

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.abspath(os.environ.get("REMUX_BIN", os.path.join(REPO, "target/debug/remux")))
OUT_DIR = os.path.join(REPO, "docs", "screenshots")
CHROME = shutil.which("google-chrome") or "/usr/bin/google-chrome"

COLS, ROWS = 160, 45


def protocol_version():
    for line in open(os.path.join(REPO, "src", "protocol.rs")):
        if "pub const PROTOCOL_VERSION" in line:
            return int(line.split("=")[1].strip().rstrip(";"))
    raise SystemExit("could not read PROTOCOL_VERSION from src/protocol.rs")


PROTOCOL_VERSION = protocol_version()


# ---------------------------------------------------------------------------
# build
# ---------------------------------------------------------------------------

def build():
    """`cargo build`, then refuse a binary older than the build.

    A failed compile leaves the previous binary in place, and a screenshot of
    stale code is exactly the green-but-wrong result this guards against.
    """
    started = time.time()
    r = subprocess.run(["cargo", "build"], cwd=REPO)
    if r.returncode != 0:
        raise SystemExit("cargo build failed")
    if "REMUX_BIN" not in os.environ and os.path.getmtime(BIN) < started - 1:
        # cargo leaves an up-to-date binary untouched; only a fresh build bumps
        # it. Touching nothing means nothing changed, which is fine -- but make
        # sure the binary is at least as new as every source file.
        newest = max(
            os.path.getmtime(os.path.join(d, f))
            for d, _, fs in os.walk(os.path.join(REPO, "src")) for f in fs
        )
        if os.path.getmtime(BIN) < newest:
            raise SystemExit(f"{BIN} is older than src/ -- the build did not produce it")


# ---------------------------------------------------------------------------
# isolated environment
# ---------------------------------------------------------------------------

PROMPT = r"\[\e[38;2;137;180;250m\]\w \[\e[38;2;203;166;247m\]❯\[\e[0m\] "

GIT_ID = {
    "GIT_AUTHOR_NAME": "Ada Demo", "GIT_AUTHOR_EMAIL": "ada@example.com",
    "GIT_COMMITTER_NAME": "Ada Demo", "GIT_COMMITTER_EMAIL": "ada@example.com",
}


class Env:
    """One throwaway run directory: XDG dirs, HOME, a bin/ of stand-ins."""

    def __init__(self, config, keep=False):
        # Under /tmp, not the scratchpad: the socket path must stay < 108 bytes.
        self.run = tempfile.mkdtemp(prefix="rmx-shot-", dir="/tmp")
        self.keep = keep
        self.home = f"{self.run}/home"
        self.sock = f"{self.run}/run/remux.sock"
        for d in ("run", "state", "data", "config/remux", "bin", "home"):
            os.makedirs(f"{self.run}/{d}", exist_ok=True)
        os.chmod(f"{self.run}/run", 0o700)
        with open(f"{self.run}/config/remux/config.toml", "w") as f:
            f.write(config)
        self.server = None
        self.children = []

    def env(self):
        return {
            **os.environ,
            **GIT_ID,
            "HOME": self.home,
            "PATH": f"{self.run}/bin:" + os.environ.get("PATH", ""),
            "XDG_RUNTIME_DIR": f"{self.run}/run",
            "XDG_STATE_HOME": f"{self.run}/state",
            "XDG_DATA_HOME": f"{self.run}/data",
            "XDG_CONFIG_HOME": f"{self.run}/config",
            "SHELL": "/bin/sh",
            "ENV": "/dev/null",
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "PAGER": "cat",
            "GIT_PAGER": "cat",
            "PS1": PROMPT,
            "LS_COLORS": "di=1;34:ln=36:ex=1;32:*.toml=33:*.md=35:*.rs=38;5;216",
            "REMUX_ALLOW_NESTED": "1",
        }

    def install(self, name, body):
        p = f"{self.run}/bin/{name}"
        with open(p, "w") as f:
            f.write(body)
        os.chmod(p, 0o755)

    def start_server(self):
        self.server = subprocess.Popen(
            [BIN, "server"], env=self.env(), cwd=self.home,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(200):
            if os.path.exists(self.sock):
                time.sleep(0.3)
                return
            time.sleep(0.05)
        raise SystemExit("server socket never appeared")

    def client(self, args=(), cols=COLS, rows=ROWS):
        t = Tui(self, list(args), cols, rows)
        self.children.append(t)
        return t

    def panics(self):
        """Logs that show a panic, or that are missing or empty.

        Both processes log at Debug unconditionally, so an absent or empty log
        means this is reading the wrong path -- and a panic grep over nothing
        would pass for ever.
        """
        found = []
        # A server-only env (the fake remote) never runs a client here.
        for name in ("server.log", "client.log") if self.children else ("server.log",):
            p = f"{self.run}/state/remux/{name}"
            body = open(p, errors="replace").read() if os.path.exists(p) else ""
            if not body:
                found.append(f"{p} (missing or empty)")
            elif "panicked at" in body:
                found.append(p)
        return found

    def teardown(self):
        for t in self.children:
            t.kill()
        try:
            subprocess.run([BIN, "stop"], env=self.env(), timeout=10,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except Exception:
            pass
        if self.server:
            try:
                self.server.wait(timeout=5)
            except Exception:
                self.server.kill()
        # Stand-in agents loop for ever; anything still carrying our run dir in
        # its command line or environment is ours to reap.
        subprocess.run(["pkill", "-f", self.run], stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL)
        if not self.keep:
            shutil.rmtree(self.run, ignore_errors=True)


class Wire:
    """A minimal protocol client, used only to SEED sessions the TUI then shows."""

    def __init__(self, env):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(env.sock)
        self.s.settimeout(3.0)
        self.buf = b""
        self.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "readme-shots"})
        self.recv()

    def send(self, obj):
        b = json.dumps(obj).encode()
        self.s.sendall(struct.pack(">I", len(b)) + b)

    def recv(self):
        while len(self.buf) < 4:
            self.buf += self.s.recv(65536)
        n = struct.unpack(">I", self.buf[:4])[0]
        while len(self.buf) < 4 + n:
            self.buf += self.s.recv(65536)
        body, self.buf = self.buf[4:4 + n], self.buf[4 + n:]
        return json.loads(body)

    def cmd(self, c, settle=0.35):
        self.send({"Command": c})
        time.sleep(settle)

    def type(self, text, settle=0.5):
        self.send({"Input": {"data": list(text.encode())}})
        time.sleep(settle)

    def session(self, name, cols=COLS, rows=ROWS):
        self.send({"CreateSession": {"name": name, "folder": None}})
        self.send({"Attach": {"session_name": name}})
        self.send({"Resize": {"cols": cols, "rows": rows}})
        time.sleep(0.5)

    def tree(self):
        """The server's session tree, skipping the render frames queued ahead of it."""
        self.send("ListSessionTree")
        end = time.time() + 5
        while time.time() < end:
            m = self.recv()
            if isinstance(m, dict) and "SessionTree" in m:
                return m["SessionTree"]
        raise SystemExit("no SessionTree reply")

    def panes(self, session):
        """Pane ids of `session`, tab by tab, in pane order."""
        t = self.tree()
        for s in t["unfiled"] + [s for f in t["folders"] for s in f["sessions"]]:
            if s["name"] == session:
                return [p["id"] for tab in s["tabs"] for p in tab["panes"]]
        raise SystemExit(f"no session {session!r} in the tree")

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


class Tui:
    """The real client binary on a pseudo-terminal, read through pyte."""

    def __init__(self, env, args, cols, rows):
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.child = pexpect.spawn(BIN, args, env=env.env(), cwd=env.home,
                                   dimensions=(rows, cols), encoding=None)

    def pump(self, t=0.5):
        end = time.time() + t
        while time.time() < end:
            try:
                self.stream.feed(self.child.read_nonblocking(65536, 0.1))
            except pexpect.TIMEOUT:
                pass
            except pexpect.EOF:
                raise SystemExit("the client exited")

    def send(self, data, t=0.4):
        self.child.send(data.encode() if isinstance(data, str) else data)
        self.pump(t)

    def rows(self):
        return self.screen.display

    def has(self, needle):
        return any(needle in r for r in self.rows())

    def wait(self, cond, what, timeout=10.0):
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.3)
            if cond():
                return
        self.dump()
        raise SystemExit(f"timed out waiting for {what}")

    def highlighted(self, needle):
        """Whether `needle` sits on a light background: an overlay's cursor row."""
        for y, row in enumerate(self.rows()):
            x = row.find(needle)
            if x >= 0:
                bg = colour(self.screen.buffer[y][x].bg, BG).lstrip("#")
                if sum(int(bg[i:i + 2], 16) for i in (0, 2, 4)) > 3 * 160:
                    return True
        return False

    def choose(self, needle, key="k", tries=20):
        """Move an overlay's cursor with `key` until it rests on `needle`."""
        for _ in range(tries):
            if self.highlighted(needle):
                return
            self.send(key, 0.25)
        self.dump()
        raise SystemExit(f"could not move the cursor onto {needle!r}")

    def dump(self):
        for i, r in enumerate(self.rows()):
            print(f"{i:2} |{r.rstrip()}")

    def kill(self):
        try:
            self.child.terminate(force=True)
        except Exception:
            pass


# ---------------------------------------------------------------------------
# rendering: pyte screen -> HTML -> PNG
# ---------------------------------------------------------------------------

BG = "#1e1f22"
FG = "#cdd6f4"
# Catppuccin Mocha, to match the default Remux theme's own RGB roles.
ANSI16 = [
    "#313244", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
    "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#e6e9ef",
]
_NAMES = ["black", "red", "green", "brown", "blue", "magenta", "cyan", "white"]
PALETTE = {}
for _i, _n in enumerate(_NAMES):
    PALETTE[_n] = ANSI16[_i]
    PALETTE["bright" + _n] = ANSI16[_i + 8]
PALETTE["bfightmagenta"] = ANSI16[13]  # pyte's own typo for SGR 105
# Indexed 0-15 reach us as pyte's fixed xterm hexes; route them to the palette.
for _i, _h in enumerate(pyte.graphics.FG_BG_256[:16]):
    PALETTE.setdefault(_h, ANSI16[_i])

FONT_CANDIDATES = ["JetBrainsMono Nerd Font Mono", "JetBrainsMonoNL Nerd Font Mono",
                   "JetBrains Mono", "DejaVu Sans Mono"]
FONT_PX = 15
CELL_W = FONT_PX * 0.6   # JetBrains Mono's advance is exactly 0.6em
CELL_H = 19
SCALE = 2


def pick_font():
    try:
        families = subprocess.run(["fc-list", ":", "family"], capture_output=True,
                                  text=True).stdout
    except FileNotFoundError:
        families = ""
    for f in FONT_CANDIDATES:
        if f in families:
            return f
    return "monospace"


FONT = pick_font()


def colour(c, default):
    if c == "default":
        return default
    if c in PALETTE:
        return PALETTE[c]
    if len(c) == 6:
        return "#" + c
    return default


def cell_style(ch):
    fg, bg = colour(ch.fg, FG), colour(ch.bg, BG)
    if ch.reverse:
        fg, bg = bg, fg
    css = [f"color:{fg}"]
    if bg != BG:
        css.append(f"background:{bg}")
    if ch.bold:
        css.append("font-weight:700")
    if ch.italics:
        css.append("font-style:italic")
    deco = [d for d, on in (("underline", ch.underscore), ("line-through", ch.strikethrough)) if on]
    if deco:
        css.append("text-decoration:" + " ".join(deco))
    return ";".join(css)


def screen_html(screen, cursor=False):
    """One absolutely positioned run per style change, so nothing reflows.

    Runs are positioned by COLUMN, not laid out inline, so a glyph whose font
    advance differs from the cell (a fallback emoji, a missing icon) can shift
    nothing after it.
    """
    out = []
    for y in range(screen.lines):
        line = screen.buffer[y]
        x = 0
        run, run_x, run_style, run_w = [], 0, None, 0

        def flush():
            if run:
                out.append(
                    f'<span style="left:{run_x * CELL_W}px;top:{y * CELL_H}px;'
                    f'width:{run_w * CELL_W}px;{run_style}">{html.escape("".join(run))}</span>')

        while x < screen.columns:
            ch = line[x]
            w = 1
            if x + 1 < screen.columns and line[x + 1].data == "" and ch.data:
                w = 2
            style = cell_style(ch)
            glyph = ch.data or " "
            if w == 2 or style != run_style:
                flush()
                run, run_x, run_style, run_w = [], x, style, 0
            run.append(glyph)
            run_w += w
            if w == 2:
                flush()
                run, run_style, run_w = [], None, 0
            x += w
        flush()
    if cursor and not screen.cursor.hidden:
        c = screen.cursor
        out.append(f'<span class="cur" style="left:{c.x * CELL_W}px;top:{c.y * CELL_H}px"></span>')
    width, height = screen.columns * CELL_W, screen.lines * CELL_H
    return f"""<!doctype html><html><head><meta charset="utf-8"><style>
html,body{{margin:0;padding:0;background:{BG};overflow:hidden}}
#g{{position:relative;width:{width}px;height:{height}px;
   font-family:"{FONT}",monospace;font-size:{FONT_PX}px;line-height:{CELL_H}px;
   font-variant-ligatures:none;-webkit-font-smoothing:antialiased}}
#g span{{position:absolute;height:{CELL_H}px;white-space:pre;overflow:visible}}
#g .cur{{width:{CELL_W}px;background:{FG};opacity:.85}}
</style></head><body><div id="g">{''.join(out)}</div></body></html>"""


def screenshot(screen, png, cursor=False, workdir=None, crop=None):
    """`crop` is `(x0, y0, x1, y1)` in cells, end-exclusive; `None` keeps the whole screen."""
    if workdir is None:
        with tempfile.TemporaryDirectory(prefix="rmx-html-") as tmp:
            return screenshot(screen, png, cursor, tmp, crop)
    page = os.path.join(workdir, os.path.basename(png) + ".html")
    with open(page, "w") as f:
        f.write(screen_html(screen, cursor))
    width = int(round(screen.columns * CELL_W))
    height = screen.lines * CELL_H
    os.makedirs(os.path.dirname(os.path.abspath(png)), exist_ok=True)
    r = subprocess.run(
        [CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars",
         "--no-first-run", "--no-default-browser-check",
         f"--user-data-dir={workdir}/chrome", f"--force-device-scale-factor={SCALE}",
         f"--window-size={width},{height}", f"--screenshot={os.path.abspath(png)}",
         "--virtual-time-budget=2000", "file://" + page],
        capture_output=True, text=True, timeout=60)
    if r.returncode != 0 or not os.path.exists(png):
        raise SystemExit(f"chrome failed: {r.stderr[-800:]}")
    compress(png, crop)
    return png


def compress(png, crop=None):
    """Crop, then reduce to a 256-colour palette.

    A terminal frame has a few dozen colours plus their anti-aliased edges, so
    a palette PNG is visually identical at a fraction of the RGB size. No
    dithering, because dither noise on flat backgrounds defeats the
    compression and reads as grain.
    """
    img = Image.open(png).convert("RGB")
    if crop:
        x0, y0, x1, y1 = crop
        img = img.crop((round(x0 * CELL_W * SCALE), y0 * CELL_H * SCALE,
                        round(x1 * CELL_W * SCALE), y1 * CELL_H * SCALE))
    img = img.quantize(colors=256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE)
    img.save(png, optimize=True)


def snapshot(screen):
    return [[screen.buffer[y][x].data for x in range(screen.columns)] for y in range(screen.lines)]


def changed_rect(before, screen, pad=1):
    """The cell rectangle whose glyphs differ between `before` and `screen`, padded.

    This is how an overlay shot finds its own crop: whatever the keystroke
    painted is the overlay, wherever the client chose to put it. Colours are
    ignored and the status row skipped, because opening an overlay also
    recolours the focused border and changes the mode badge, and either would
    stretch the crop to the whole screen.
    """
    after = snapshot(screen)
    ys = [y for y in range(screen.lines - 1) if before[y] != after[y]]
    xs = [x for y in ys for x in range(screen.columns) if before[y][x] != after[y][x]]
    if not ys:
        raise SystemExit("the overlay changed nothing on screen")
    return (max(0, min(xs) - pad * 2), max(0, min(ys) - pad),
            min(screen.columns, max(xs) + 1 + pad * 2), min(screen.lines, max(ys) + 1 + pad))


def make_gif(frames, gif, workdir=None):
    """`frames` is [(png_path, seconds), ...]; writes an optimised, looping GIF."""
    if workdir is None:
        with tempfile.TemporaryDirectory(prefix="rmx-gif-") as tmp:
            return make_gif(frames, gif, tmp)
    listing = os.path.join(workdir, "frames.txt")
    with open(listing, "w") as f:
        for png, secs in frames:
            f.write(f"file '{os.path.abspath(png)}'\nduration {secs}\n")
        # The concat demuxer ignores the LAST entry's duration unless the file
        # is listed once more after it.
        f.write(f"file '{os.path.abspath(frames[-1][0])}'\n")
    vf = ("scale=iw/2:-1:flags=lanczos,split[a][b];"
          "[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=none")
    r = subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error", "-f", "concat", "-safe", "0",
         "-i", listing, "-fps_mode", "vfr", "-vf", vf, "-loop", "0", gif],
        capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit(f"ffmpeg failed: {r.stderr[-800:]}")
    return gif


# ---------------------------------------------------------------------------
# demo content
# ---------------------------------------------------------------------------

ESC = "\\033"

# A stand-in agent. Linux names a `#!` script's process after the script, so
# the server's foreground-process detection reads `claude`/`codex` exactly as
# it would for the real thing.
AGENT = r"""#!/bin/sh
dim='\033[2m'; b='\033[1m'; r='\033[0m'; acc='\033[38;2;217;119;87m'
blu='\033[38;2;137;180;250m'; grn='\033[38;2;166;227;161m'; ylw='\033[38;2;249;226;175m'
printf '\033[H\033[2J'
case "$1" in
ask)
  printf "${dim}> add rate limiting to the public routes${r}\n\n"
  printf "${acc}●${r} I'll put a token-bucket limiter in front\n"
  printf "  of the public routes, keyed by client IP.\n\n"
  printf "${acc}●${r} ${b}Read${r}(src/routes.rs)\n"
  printf "  ${dim}⎿  Read 118 lines${r}\n\n"
  printf "${acc}●${r} ${b}Write${r}(src/limiter.rs)\n"
  printf "  ${dim}⎿${r}  Wrote ${b}64${r} lines to src/limiter.rs\n\n"
  printf "${acc}●${r} ${b}Update${r}(src/routes.rs)\n"
  printf "  ${dim}⎿${r}  Added ${b}9${r} lines, removed ${b}2${r} lines\n\n"
  printf "${acc}●${r} Now the tests for burst and refill.\n\n"
  printf "${acc}╭──────────────────────────────────────╮${r}\n"
  printf "${acc}│${r} ${b}Bash command${r}                         ${acc}│${r}\n"
  printf "${acc}│${r}                                      ${acc}│${r}\n"
  printf "${acc}│${r}   ${blu}cargo test -p api limiter${r}          ${acc}│${r}\n"
  printf "${acc}│${r}   ${dim}Run the rate limiter tests${r}         ${acc}│${r}\n"
  printf "${acc}│${r}                                      ${acc}│${r}\n"
  printf "${acc}│${r} Do you want to proceed?              ${acc}│${r}\n"
  printf "${acc}│${r} ${blu}❯ 1. Yes${r}                             ${acc}│${r}\n"
  printf "${acc}│${r}   2. Yes, and don't ask again        ${acc}│${r}\n"
  printf "${acc}│${r}   3. No, tell me what to do          ${acc}│${r}\n"
  printf "${acc}╰──────────────────────────────────────╯${r}\n"
  ;;
work)
  printf "${acc}●${r} Migrating the dashboard to the new API client.\n\n"
  printf "${acc}●${r} ${b}Read${r}(src/api/client.ts)\n"
  printf "${acc}●${r} ${b}Update${r}(src/pages/Dashboard.tsx)\n"
  printf "  ${dim}⎿${r}  Added ${b}18${r} lines, removed ${b}25${r} lines\n\n"
  i=0
  while :; do
    for s in '⠋' '⠙' '⠹' '⠸' '⠼' '⠴' '⠦' '⠧' '⠇' '⠏'; do
      i=$((i+1))
      printf "\r${acc}${s}${r} Running type check… ${dim}(${i}s)${r}   "
      sleep 0.2
    done
  done
  ;;
idle)
  printf "${grn}✔${r} Plan applied: ${b}3${r} added, ${b}1${r} changed, ${b}0${r} destroyed.\n\n"
  printf "${dim}Anything else? (Ctrl+C to exit)${r}\n"
  ;;
esac
while read line; do :; done
"""

# A stand-in `cargo`, so the typed command is the real one a reader would type.
CARGO = r"""#!/bin/sh
g='\033[1;32m'; y='\033[1;33m'; r='\033[0m'; d='\033[2m'
printf "${g}   Compiling${r} api v0.4.2 (~/demo/api)\n"
printf "${g}    Finished${r} \`test\` profile in 3.84s\n"
printf "${g}     Running${r} unittests src/main.rs\n"
printf "running 6 tests\n"
for t in routes::health_ok routes::users_list auth::token_roundtrip \
         db::pool_reuses limiter::refills limiter::burst_denied; do
  printf "test %-24s ... ${g}ok${r}\n" "$t"
done
printf "\ntest result: ${g}ok${r}. 6 passed; 0 failed\n"
"""


# A stand-in `kubectl`, for the infra session.
KUBECTL = r"""#!/bin/sh
g='\033[32m'; y='\033[33m'; r='\033[0m'; b='\033[1m'
printf "${b}NAME                    READY  STATUS     AGE${r}\n"
printf "api-7d9f8b6c4-2xkqp     1/1    ${g}Running${r}    3d4h\n"
printf "api-7d9f8b6c4-9wz7m     1/1    ${g}Running${r}    3d4h\n"
printf "web-5c6b7d9f8-lq4ns     1/1    ${g}Running${r}    26h\n"
printf "worker-6f8d9c7b5-h2x8t  1/1    ${g}Running${r}    11h\n"
printf "postgres-0              1/1    ${g}Running${r}    12d\n"
printf "redis-0                 1/1    ${g}Running${r}    12d\n"
printf "migrate-28761440-vk9jd  0/1    ${y}Completed${r}  42m\n"
"""

# A stand-in `npm`, for the web session.
NPM = r"""#!/bin/sh
c='\033[36m'; g='\033[32m'; d='\033[2m'; r='\033[0m'; b='\033[1m'
printf "\n> web@2.3.0 build\n> vite build\n\n"
printf "${c}vite v6.2.1${r} ${g}building for production...${r}\n"
printf "${g}✓${r} 412 modules transformed.\n"
printf "${d}dist/${r}index.html                  ${d}  0.61 kB │ gzip:  0.37 kB${r}\n"
printf "${d}dist/${r}${c}assets/index-Bq3kT9aP.css${r}   ${d} 18.42 kB │ gzip:  4.11 kB${r}\n"
printf "${d}dist/${r}${c}assets/vendor-D8fZ2lQx.js${r}   ${d}142.87 kB │ gzip: 46.02 kB${r}\n"
printf "${d}dist/${r}${c}assets/index-C1mR7wYs.js${r}    ${d} 64.30 kB │ gzip: 19.88 kB${r}\n"
printf "${g}✓ built in 2.41s${r}\n"
"""

ACCESS_LOG = [
    ("GET", "/health", 200, "0.4ms"), ("GET", "/api/users", 200, "12.8ms"),
    ("POST", "/api/sessions", 201, "48.1ms"), ("GET", "/api/users/42", 200, "6.2ms"),
    ("GET", "/api/orders?page=2", 200, "21.7ms"), ("PUT", "/api/users/42", 204, "18.3ms"),
    ("GET", "/api/missing", 404, "1.1ms"), ("POST", "/api/orders", 201, "63.9ms"),
    ("GET", "/api/users", 429, "0.2ms"), ("GET", "/api/users", 429, "0.2ms"),
    ("GET", "/health", 200, "0.3ms"), ("DELETE", "/api/sessions/9", 204, "9.4ms"),
    ("GET", "/api/orders/77", 200, "14.0ms"), ("POST", "/api/login", 401, "31.5ms"),
    ("POST", "/api/login", 200, "35.2ms"), ("GET", "/api/users", 200, "11.9ms"),
    ("GET", "/metrics", 200, "2.8ms"), ("GET", "/api/orders?page=3", 200, "19.6ms"),
]


def demo_repo(env):
    """~/demo/{api,web,infra}; api is a small git repo with a readable history."""
    e = env.env()
    api = f"{env.home}/demo/api"
    for d in ("src", "tests", "migrations"):
        os.makedirs(f"{api}/{d}", exist_ok=True)
    for d in ("web/src", "infra/modules"):
        os.makedirs(f"{env.home}/demo/{d}", exist_ok=True)

    def git(*a, date=None):
        ee = dict(e)
        if date:
            ee["GIT_AUTHOR_DATE"] = ee["GIT_COMMITTER_DATE"] = date
        subprocess.run(["git", *a], cwd=api, env=ee, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def write(path, text):
        with open(f"{api}/{path}", "w") as f:
            f.write(text)

    git("init", "-q", "-b", "main")
    history = [
        ("Cargo.toml", '[package]\nname = "api"\nversion = "0.1.0"\n', "chore: initial scaffold"),
        ("src/main.rs", "fn main() {}\n", "feat: axum server skeleton"),
        ("src/routes.rs", "// routes\n", "feat: health and users routes"),
        ("src/db.rs", "// db\n", "feat(db): pooled connections"),
        ("migrations/001_users.sql", "create table users();\n", "feat(db): users migration"),
        ("src/auth.rs", "// auth\n", "feat(auth): JWT sessions"),
        ("tests/routes.rs", "// tests\n", "test: route integration"),
        ("README.md", "# api\n", "docs: local setup"),
    ]
    for i, (path, text, msg) in enumerate(history):
        write(path, text)
        git("add", "-A")
        git("commit", "-q", "-m", msg, date=f"2026-09-{10 + i}T10:00:00")
        if i == 5:
            git("tag", "v0.4.0")
    git("checkout", "-q", "-b", "rate-limit")
    write("src/limiter.rs", "// token bucket\n")
    git("add", "-A")
    git("commit", "-q", "-m", "feat: token-bucket limiter", date="2026-09-20T10:00:00")
    write("tests/limiter.rs", "// tests\n")
    git("add", "-A")
    git("commit", "-q", "-m", "test: burst and refill", date="2026-09-21T10:00:00")
    git("checkout", "-q", "main")
    write("src/routes.rs", "// routes v2\n")
    git("add", "-A")
    git("commit", "-q", "-m", "fix: JSON 404 bodies", date="2026-09-22T10:00:00")
    git("merge", "-q", "--no-ff", "rate-limit", "-m", "Merge rate-limit",
        date="2026-09-23T10:00:00")
    write("Cargo.lock", "# lock\n")
    write(".env.example", "DATABASE_URL=\n")
    write(".gitignore", "/target\n")
    write("src/config.rs", "// config\n")
    write("Dockerfile", "FROM rust\n")
    write("Makefile", "all:\n")
    for sub in ("web", "infra"):
        with open(f"{env.home}/demo/{sub}/README.md", "w") as f:
            f.write(f"# {sub}\n")
    os.makedirs(f"{api}/logs", exist_ok=True)
    with open(f"{api}/logs/access.log", "w") as f:
        for i, (method, path, status, took) in enumerate(ACCESS_LOG):
            f.write(f"14:{12 + i // 3:02}:{(i * 17) % 60:02} "
                    f"{method:<6} {path:<18} {status} {took}\n")


def install_standins(env):
    env.install("claude", AGENT)
    env.install("codex", AGENT)
    env.install("cargo", CARGO)
    env.install("kubectl", KUBECTL)
    env.install("npm", NPM)


# (command, text it leaves on screen). Pane N of a seeded workspace runs entry N.
API_PANES = [
    ("claude ask", "Do you want to proceed?"),
    ("cargo test", "test result"),
    ("git log --graph --oneline -n 12", "initial scaffold"),
    ("tail -n 18 logs/access.log", "/metrics"),
    ("ls --color=always", "Cargo.toml"),
    ("kubectl get pods", "postgres-0"),
]


# BSP halves the focused pane each time, so panes 3 and 4 are quarter-width
# and get commands whose output fits without wrapping.
BSP_PANES = [API_PANES[0], API_PANES[1], ("git status -sb", "config.rs"), API_PANES[4]]


def at(dir_):
    return f"cd ~/demo/{dir_} && clear\n"


def seed_api(w, n=4, cols=COLS, rows=ROWS, name="api", tabs=("build", "logs"), focus=0,
             panes=None, before_typing=None):
    """Session `name` whose first tab runs `panes` (default: the first `n` of API_PANES).

    Focus ends on pane `focus` of that tab (`None` leaves it on the last one made).
    `before_typing(w, pane_ids)` runs once the panes exist and before any output,
    for layout changes that would otherwise re-wrap what the panes print.
    """
    panes = panes or API_PANES[:n]
    n = len(panes)
    w.session(name, cols, rows)
    w.cmd({"TabRename": "editor"})
    for i in range(n):
        if i:
            w.cmd("PaneNew")
        w.type(at("api"), 0.3)
    # Typed only once every pane exists: output printed before a later split
    # shrinks its pane gets re-wrapped by reflow into ragged lines.
    ids = w.panes(name)
    if before_typing:
        before_typing(w, ids)
    run_in_panes(w, name, ids, panes)
    for t in tabs:
        w.cmd("TabNew")
        w.cmd({"TabRename": t})
        w.type(at("api"))
    w.cmd({"SessionSwitchTab": {"session": name, "tab_index": 0}}, 0.6)
    if focus is not None:
        pane = w.panes(name)[focus]
        w.cmd({"SessionSwitchPane": {"session": name, "tab_index": 0, "pane_id": pane}}, 0.6)


def run_in_panes(w, session, ids, panes, tab_index=0):
    for pane, (command, _) in zip(ids, panes):
        w.cmd({"SessionSwitchPane": {"session": session, "tab_index": tab_index, "pane_id": pane}},
              0.2)
        w.type("clear\n", 0.3)
        w.type(command + "\n", 0.6)


def seed_others(w, cols=COLS, rows=ROWS):
    """`web` (a working claude, a build) and `infra` (an idle codex, pods)."""
    w.session("web", cols, rows)
    w.cmd({"TabRename": "dev"})
    w.type(at("web"))
    w.type("claude work\n")
    w.cmd("PaneNew")
    w.type(at("web"))
    w.type("npm run build\n")
    w.cmd("TabNew")
    w.cmd({"TabRename": "server"})
    w.type(at("web"))
    w.session("infra", cols, rows)
    w.cmd({"TabRename": "deploy"})
    w.type(at("infra"))
    w.type("codex idle\n")
    w.cmd("PaneNew")
    w.type(at("infra"))
    w.type("kubectl get pods\n")


def workspace(env_, n=4, others=False, cols=COLS, rows=ROWS, **seed):
    """Server up, demo content and stand-ins installed, `api` seeded (plus web/infra)."""
    demo_repo(env_)
    install_standins(env_)
    env_.start_server()
    w = Wire(env_)
    if others:
        seed_others(w, cols, rows)
    seed_api(w, n, cols, rows, **seed)
    return w


def config(layout="bsp", extra=""):
    return f"""
[appearance]
default_layout = "{layout}"
{extra}
[appearance.theme]
tab_style = "rounded"
"""


def attach(env_, needles, session="api", timeout=15):
    """Attach a client and wait until every needle is on screen."""
    tui = env_.client(["attach", session])
    tui.wait(lambda: all(tui.has(n) for n in needles), f"{needles} on screen", timeout)
    tui.pump(0.8)
    return tui


# ---------------------------------------------------------------------------
# shots
# ---------------------------------------------------------------------------

SHOTS = {}


def shot(name):
    def reg(fn):
        SHOTS[name] = fn
        return fn
    return reg


BASE_CONFIG = """
[appearance]
default_layout = "bsp"

[appearance.theme]
tab_style = "rounded"
"""


def panel_marker_colours(tui, x0, x1, y0, y1):
    """Distinct foreground colours of the first glyph of each agent row."""
    seen = set()
    for y in range(y0, y1):
        line = tui.screen.buffer[y]
        for x in range(x0, x1):
            if line[x].data in ("●", "◐", "○", "◉", "•", "■", "▲"):
                seen.add(line[x].fg)
                break
    return seen


@shot("sidebars")
def shot_sidebars(env, out):
    """All three sidebars: sessions (left), agents (right), files (bottom)."""
    left_w, right_w, bottom_h = 28, 32, 10
    config = BASE_CONFIG + f"""
[[sidebar]]
edge = "left"
size = {left_w}
visible = true

  [[sidebar.panel]]
  plugin = "sessions"

[[sidebar]]
edge = "right"
size = {right_w}
visible = true

  [[sidebar.panel]]
  plugin = "agents"

[[sidebar]]
edge = "bottom"
size = {bottom_h}
visible = true

  [[sidebar.panel]]
  plugin = "files"
"""
    env_ = env(config)
    demo_repo(env_)
    install_standins(env_)
    env_.start_server()

    main_cols = COLS - left_w - right_w
    main_rows = ROWS - bottom_h - 1
    home = "~/demo"

    # Every session is seeded at the size the client will give it, so no pane
    # re-flows (and re-wraps its content) when the client attaches.
    def at(dir_):
        return f"cd {home}/{dir_} && clear\n"

    w = Wire(env_)
    # infra: one tab, an idle codex
    w.session("infra", main_cols, main_rows)
    w.cmd({"TabRename": "deploy"})
    w.type(at("infra"))
    w.type("codex idle\n")
    w.cmd("TabNew")
    w.cmd({"TabRename": "plan"})
    w.type(at("infra"))

    # web: dev tab with a working claude, and a server tab
    w.session("web", main_cols, main_rows)
    w.cmd({"TabRename": "dev"})
    w.type(at("web"))
    w.type("claude work\n")
    w.cmd("TabNew")
    w.cmd({"TabRename": "server"})
    w.type(at("web"))

    # api: the one on screen
    w.session("api", main_cols, main_rows)
    w.cmd({"TabRename": "editor"})
    w.type(at("api"))
    w.type("claude ask\n", 0.6)
    w.cmd("PaneNew")
    w.type(at("api"))
    w.type("cargo test\n", 0.6)
    w.cmd("PaneNew")
    w.type(at("api"))
    w.type("git log --graph --oneline -n 9\n", 0.6)
    w.cmd("TabNew")
    w.cmd({"TabRename": "build"})
    w.type(at("api"))
    w.cmd("TabNew")
    w.cmd({"TabRename": "logs"})
    w.type(at("api"))
    w.cmd({"SessionSwitchTab": {"session": "api", "tab_index": 0}}, 0.6)
    w.cmd("PaneFocusLeft")
    w.close()

    tui = env_.client(["attach", "api"])
    tui.pump(2.5)
    tui.wait(lambda: tui.has("Cargo.toml") and tui.has("infra") and tui.has("codex"),
             "all three panels to populate")
    # The agents panel refreshes on its own push; hold out for all three states
    # so the capture never lands between a Working sample and its repaint.
    x0 = COLS - right_w
    tui.wait(lambda: len(panel_marker_colours(tui, x0, COLS, 1, ROWS - bottom_h - 1)) >= 3,
             "three distinct agent state colours", timeout=15)
    tui.pump(1.0)
    screenshot(tui.screen, out)
    return tui


def needles(n):
    return [seen for _, seen in API_PANES[:n]]


def layout_shot(layout, n, visible=None, seed=None):
    def run(env, out):
        env_ = env(config(layout))
        if seed:
            demo_repo(env_)
            install_standins(env_)
            env_.start_server()
            w = Wire(env_)
            seed(w)
        else:
            w = workspace(env_, n)
        w.close()
        tui = attach(env_, needles(n) if visible is None else visible)
        tui.wait(lambda: tui.rows()[-1].rstrip().endswith(layout), f"{layout} on the status bar")
        screenshot(tui.screen, out)
        return tui
    return run


shot("layout-bsp")(layout_shot("bsp", 4, visible=[seen for _, seen in BSP_PANES],
                               seed=lambda w: seed_api(w, panes=BSP_PANES)))
# Master's side columns are narrow, so they get commands whose output fits.
MASTER_PANES = [
    API_PANES[0],
    ("git status -sb", "config.rs"),
    ("cal 9 2026", "September"),
    ("ls --color=always", "Cargo.toml"),
    ("find src tests -name '*.rs'", "tests/routes.rs"),
]


def seed_master(w):
    def claude_to_master(w, ids):
        w.cmd({"SessionSwitchPane": {"session": "api", "tab_index": 0, "pane_id": ids[0]}})
        w.cmd("SetMaster")
    seed_api(w, panes=MASTER_PANES, before_typing=claude_to_master)


def seed_monocle(w):
    # Monocle shows the pane made last. SessionSwitchPane cannot pick another:
    # in a Monocle tab it moves focus but not the displayed pane.
    seed_api(w, panes=API_PANES[1:4] + API_PANES[:1], focus=None)


shot("layout-master")(layout_shot("master", 5, seed=seed_master,
                                  visible=[seen for _, seen in MASTER_PANES]))
shot("layout-monocle")(layout_shot("monocle", 4, visible=["Do you want to proceed?"],
                                   seed=seed_monocle))
shot("layout-grid")(layout_shot("grid", 6))
shot("layout-columns")(layout_shot("columns", 3))
shot("layout-rows")(layout_shot("rows", 3))


@shot("layout-custom")
def shot_layout_custom(env, out):
    """Manual splits, then resized: nothing redistributes them."""
    env_ = env(config("custom"))
    demo_repo(env_)
    install_standins(env_)
    env_.start_server()
    w = Wire(env_)
    w.session("api")
    w.cmd({"TabRename": "editor"})
    w.type(at("api"))
    for split in ("PaneSplitVertical", "PaneSplitHorizontal", "PaneSplitVertical"):
        w.cmd(split)
        w.type(at("api"), 0.3)
    w.cmd({"ResizeRight": 12})
    w.cmd("PaneFocusUp")
    w.cmd({"ResizeDown": 6})
    run_in_panes(w, "api", w.panes("api"),
                 [API_PANES[0], API_PANES[2], API_PANES[1], API_PANES[5]])
    w.cmd("TabNew")
    w.cmd({"TabRename": "logs"})
    w.cmd({"SessionSwitchTab": {"session": "api", "tab_index": 0}}, 0.6)
    w.close()
    tui = attach(env_, ["Do you want to proceed?", "initial scaffold", "test result", "postgres-0"])
    tui.wait(lambda: tui.rows()[-1].rstrip().endswith("custom"), "custom on the status bar")
    screenshot(tui.screen, out)
    return tui


@shot("pane-zoom")
def shot_pane_zoom(env, out):
    """One pane of four zoomed to fill the tab; the status bar flags it."""
    env_ = env(config())
    w = workspace(env_, panes=BSP_PANES, focus=1)
    w.cmd("PaneToggleZoom")
    w.close()
    tui = attach(env_, ["test result"])
    screenshot(tui.screen, out)
    return tui


def seed_stack(w, name="api"):
    """claude on the left; a three-pane stack on the right, git log showing."""
    w.session(name)
    w.cmd({"TabRename": "editor"})
    w.type(at("api"), 0.3)
    w.cmd("PaneNew")
    w.type(at("api"), 0.3)
    w.cmd("PaneStackAdd")
    w.type(at("api"), 0.3)
    w.cmd("PaneStackAdd")
    w.type(at("api"), 0.3)
    ids = w.panes(name)
    run_in_panes(w, name, ids, [API_PANES[0], API_PANES[1], API_PANES[3], API_PANES[2]])
    # Every stacked pane would otherwise be labelled `sh`.
    for pane, label in zip(ids[1:], ("tests", "access.log", "history")):
        w.cmd({"SessionSwitchPane": {"session": name, "tab_index": 0, "pane_id": pane}}, 0.2)
        w.cmd({"PaneRename": label}, 0.2)
    for t in ("build", "logs"):
        w.cmd("TabNew")
        w.cmd({"TabRename": t})
    w.cmd({"SessionSwitchTab": {"session": name, "tab_index": 0}}, 0.6)
    w.cmd({"SessionSwitchPane": {"session": name, "tab_index": 0, "pane_id": ids[3]}}, 0.6)


def stack_shot(tab_style):
    def run(env, out):
        env_ = env(config().replace('tab_style = "rounded"', f'tab_style = "{tab_style}"'))
        demo_repo(env_)
        install_standins(env_)
        env_.start_server()
        w = Wire(env_)
        seed_stack(w)
        w.close()
        tui = attach(env_, ["Do you want to proceed?", "initial scaffold", "access.log"])
        screenshot(tui.screen, out)
        return tui
    return run


shot("stack-rounded")(stack_shot("rounded"))
shot("stack-plain")(stack_shot("plain"))


def border_shot(style):
    def run(env, out):
        env_ = env(config(extra=f'border_style = "{style}"'))
        workspace(env_, panes=BSP_PANES).close()
        tui = attach(env_, [seen for _, seen in BSP_PANES])
        screenshot(tui.screen, out)
        return tui
    return run


shot("border-tmux")(border_shot("tmux_style"))


# Each background tab starts its output only after the seeder has switched
# away, because the foreground tab never records activity.
ACTIVITY_TABS = [
    ("tests", "sleep 3; cargo test"),
    ("deploy", "sleep 3; printf 'deploy needs approval\\a\\n'"),
    ("logs", "sleep 3; while :; do tail -n 1 logs/access.log; sleep 0.4; done"),
]


@shot("tabs-activity")
def shot_tabs_activity(env, out):
    """Background tabs flagged: finished (green), bell (red), new output (yellow)."""
    env_ = env(config())
    w = workspace(env_, 2, tabs=())
    for name, command in ACTIVITY_TABS:
        w.cmd("TabNew")
        w.cmd({"TabRename": name})
        w.type(at("api"))
        w.type(command + "\n")
    w.cmd({"SessionSwitchTab": {"session": "api", "tab_index": 0}}, 0.6)
    w.close()
    tui = attach(env_, needles(2))
    status = lambda: tui.rows()[-1]
    # `tests` turns from activity to finished once it has been quiet for the
    # server's silence threshold, so wait for all three markers together.
    tui.wait(lambda: all(m in status() for m in ("✓", "!", "●")), "all three activity markers",
             timeout=20)
    screenshot(tui.screen, out)
    return tui


@shot("pane-titles")
def shot_pane_titles(env, out):
    """`[appearance] pane_title` naming every pane by its command and directory."""
    env_ = env(config(extra='pane_title = "{session}:{tab} {command} ({cwd})"'))
    w = workspace(env_, panes=BSP_PANES)
    w.close()
    tui = attach(env_, [seen for _, seen in BSP_PANES] + ["api:0 claude (api)"])
    screenshot(tui.screen, out)
    return tui


def seed_view(w, name, panes):
    """A shared view aliasing `panes`; returns its id.

    A pane is a local pane id, or `[{"Remote": name}, id]` for a remote's.
    """
    w.send({"ViewCreate": {"name": name}})
    end = time.time() + 5
    while time.time() < end:
        m = w.recv()
        if isinstance(m, dict) and "ViewCreated" in m:
            vid = m["ViewCreated"]["id"]
            break
    else:
        raise SystemExit("no ViewCreated reply")
    cells = [p if isinstance(p, list) else ["Local", p] for p in panes]
    w.send({"ViewAddCells": {"id": vid, "cells": cells}})
    time.sleep(0.4)
    return vid


REMOTE = "devbox"
REMOTE_CONFIG = f"""
[remotes.{REMOTE}]
ssh = "{REMOTE}"
remux_path = "remux"
auto_connect = true
"""


def fake_remote(env, client_env, cols=COLS, rows=ROWS):
    """A second isolated server posing as `devbox`, reached through an `ssh` shim.

    The shim ignores its arguments and runs `remux relay` against the second
    server's socket, which is what the real `ssh devbox remux relay` would do.
    """
    remote = env(config())
    demo_repo(remote)
    install_standins(remote)
    remote.start_server()
    w = Wire(remote)
    w.session("training", cols, rows)
    w.cmd({"TabRename": "run"})
    w.type(at("api"))
    w.type("claude work\n")
    w.cmd("PaneNew")
    w.type(at("api"))
    w.type("tail -n 18 logs/access.log\n")
    remote.panes = w.panes("training")
    w.session("db", cols, rows)
    w.cmd({"TabRename": "psql"})
    w.type(at("api"))
    w.close()
    re = remote.env()
    exports = " ".join(f"{k}={re[k]}" for k in (
        "HOME", "PATH", "XDG_RUNTIME_DIR", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME"))
    client_env.install("ssh", f"#!/bin/sh\nexport {exports}\nexec {BIN} relay\n")
    return remote


def busy_workspace(env, cfg=None, remote=False, needles=None, views=True, **seed):
    """api + web + infra, three agents in three states, two views, and
    optionally a connected `devbox` remote with a fourth agent."""
    env_ = env((cfg or config()) + (REMOTE_CONFIG if remote else ""))
    if remote:
        fake_remote(env, env_)
    w = workspace(env_, panes=BSP_PANES, others=True, **seed)
    if views:
        web, infra, api = w.panes("web"), w.panes("infra"), w.panes("api")
        seed_view(w, "agents", [api[0], web[0], infra[0]])
        seed_view(w, "builds", [api[1], web[1]])
    w.close()
    return env_, attach(env_, needles or [seen for _, seen in BSP_PANES])


def overlay_shot(keys, ready, cfg=None, then=(), pad=1, full=False, remote=False, **busy):
    """Press `keys` on a busy workspace and photograph what they open.

    `ready` is on-screen text that means the overlay has painted; `then` is
    further (keys, ready) steps, such as typing a filter.
    """
    def run(env, out):
        _, tui = busy_workspace(env, cfg, remote, **busy)
        before = snapshot(tui.screen)
        tui.send(keys)
        tui.wait(lambda: tui.has(ready), repr(ready))
        for k, r in then:
            tui.send(k)
            tui.wait(lambda: tui.has(r), repr(r))
        tui.pump(0.6)
        screenshot(tui.screen, out, crop=None if full else changed_rect(before, tui.screen, pad))
        return tui
    return run


shot("whichkey")(overlay_shot("\x01", "command palette"))
shot("whichkey-centered")(overlay_shot(
    "\x01", "command palette", cfg=config(extra='which_key_position = "centered"'), full=True))
shot("whichkey-full-width")(overlay_shot(
    "\x01", "command palette", cfg=config(extra='which_key_position = "full_width"'), full=True))
shot("command-pallet")(overlay_shot("\x01:", "PaneSplit", then=[("split", "Vertical")]))
shot("session-switcher")(overlay_shot("\x1bs", "training", remote=True))
# `jj` puts the cursor on a row other than the current pane's, whose own
# highlight would otherwise hide it.
shot("agent-switcher")(overlay_shot("\x1ba", "training", then=[("jj", "training")], remote=True))
@shot("session-manager")
def shot_session_manager(env, out):
    """The tree: a folder, local sessions, and the `devbox` remote expanded."""
    # Views and extra tabs are left out so the local tree and the remote both
    # fit in the overlay without scrolling.
    env_, tui = busy_workspace(env, remote=True, views=False, tabs=())
    w = Wire(env_)
    w.cmd({"FolderNew": "services"})
    for session in ("infra", "web"):
        w.cmd({"FolderMoveSession": {"session": session, "folder": "services"}})
    w.close()
    before = snapshot(tui.screen)
    tui.send("\x01xm")
    tui.wait(lambda: tui.has("services") and tui.has(REMOTE), "the tree")
    tui.choose(REMOTE, key="j")
    tui.send("l")
    tui.wait(lambda: tui.has("training"), "the remote expanded")
    tui.pump(0.6)
    screenshot(tui.screen, out, crop=changed_rect(before, tui.screen))
    return tui


shot("move-to-tab")(overlay_shot("\x01pt", "new tab"))


@shot("popup")
def shot_popup(env, out):
    """The popup terminal floating over the layout."""
    _, tui = busy_workspace(env, views=False)
    tui.send("\x1bp")
    tui.wait(lambda: tui.has(" popup "), "the popup")
    tui.send("cd ~/demo/infra && clear\r", 0.6)
    tui.send("kubectl get pods\r")
    tui.wait(lambda: tui.has("postgres-0"), "kubectl output in the popup")
    tui.pump(0.6)
    screenshot(tui.screen, out)
    return tui


def visual_shot(steps, prep=None):
    """Focus the `cargo test` pane, type `prep`, enter Visual mode, then press `steps`."""
    def run(env, out):
        _, tui = busy_workspace(env, views=False, focus=1)
        if prep:
            tui.send(prep, 1.0)
        tui.send("\x01v")
        tui.wait(lambda: "VISUAL" in tui.rows()[-1], "Visual mode")
        for keys, ready in steps:
            tui.send(keys, 0.3)
            if ready:
                tui.wait(lambda: ready(tui), "visual step")
        tui.pump(0.6)
        screenshot(tui.screen, out)
        return tui
    return run


def highlighted_matches(tui, needle):
    """How many on-screen occurrences of `needle` carry a background colour."""
    n = 0
    for y, row in enumerate(tui.rows()[:-2]):
        for x in (i for i in range(len(row)) if row.startswith(needle, i)):
            c = tui.screen.buffer[y][x]
            if c.bg != "default" or c.reverse:
                n += 1
    return n


shot("visual-select")(visual_shot([("k" * 3, None), ("V", None), ("k" * 6, None)]))
# `cat` pushes the cargo output into scrollback, so the search runs over real
# scrollback. Only the current match is highlighted until the first `n`, and
# a search whose matches all sit on screen with no scrollback highlighted none.
shot("search")(visual_shot(
    [("/", lambda t: "SEARCH" in t.rows()[-1]), ("/api/users", None), ("\r", None),
     ("n", lambda t: highlighted_matches(t, "/api/users") >= 4)],
    prep="cat logs/access.log\r"))


def view_shot(cycles=0, layout="grid", ready=("postgres-0", "/metrics")):
    """A view whose cells alias panes from three local sessions and the remote."""
    def run(env, out):
        env_ = env(config() + REMOTE_CONFIG)
        remote = fake_remote(env, env_)
        w = workspace(env_, panes=BSP_PANES, others=True)
        web, infra, api = w.panes("web"), w.panes("infra"), w.panes("api")
        vid = seed_view(w, "ops", [api[0], web[0], infra[1], [{"Remote": REMOTE}, remote.panes[1]]])
        for _ in range(cycles):
            w.send({"ViewCycleLayout": {"id": vid}})
            time.sleep(0.3)
        w.close()
        tui = attach(env_, [seen for _, seen in BSP_PANES])
        tui.send("\x1bs")
        tui.wait(lambda: tui.has("Views"), "the switcher")
        tui.choose("ops")
        tui.send("\r")
        tui.wait(lambda: tui.rows()[-1].rstrip().endswith(layout) and all(map(tui.has, ready)),
                 f"the view in {layout}", timeout=15)
        tui.pump(1.0)
        screenshot(tui.screen, out)
        return tui
    return run


shot("view-grid")(view_shot())
# Grid -> Columns -> Rows -> Bsp -> Master -> Monocle.
shot("view-monocle")(view_shot(cycles=5, layout="monocle", ready=("devbox: training",)))


def sidebar_shot(edge, plugin, size, remote=False, ready=()):
    """One sidebar alone, docked to `edge`, over the busy workspace."""
    def run(env, out):
        cfg = config() + f"""
[[sidebar]]
edge = "{edge}"
size = {size}
visible = true

  [[sidebar.panel]]
  plugin = "{plugin}"
"""
        cols = COLS - size if edge in ("left", "right") else COLS
        rows = ROWS - size - 1 if edge == "bottom" else ROWS
        env_, tui = busy_workspace(env, cfg, remote, views=False, cols=cols, rows=rows,
                                   needles=["Do you want to proceed?", *ready])
        tui.pump(1.0)
        screenshot(tui.screen, out, crop=sidebar_crop(edge, size))
        return tui
    return run


def sidebar_crop(edge, size, neighbour=30):
    """The sidebar's full height, status bar included, plus a slice of the pane beside it."""
    if edge == "left":
        return (0, 0, size + neighbour, ROWS)
    return (COLS - size - neighbour, 0, COLS, ROWS)


shot("sidebar-sessions")(sidebar_shot("left", "sessions", 30, remote=True, ready=("training",)))
# Docked left like the other two, so the neighbouring slice is the start of a
# pane's lines rather than their cut-off ends. `sidebars` shows it on the right.
shot("sidebar-agents")(sidebar_shot("left", "agents", 30, remote=True, ready=("training",)))
shot("sidebar-files")(sidebar_shot("left", "files", 30, ready=("Cargo.toml",)))


# ---------------------------------------------------------------------------
# driver
# ---------------------------------------------------------------------------

def gif_selftest(dest_dir):
    """Three synthetic frames -> PNG -> GIF, to prove the pipeline end to end."""
    os.makedirs(dest_dir, exist_ok=True)
    frames = []
    for i, word in enumerate(("one", "two", "three")):
        s = pyte.Screen(40, 6)
        st = pyte.Stream(s)
        st.feed(f"\x1b[1;3{i + 2}mframe {word}\x1b[0m\r\n" + "▇" * (10 * (i + 1)))
        png = os.path.join(dest_dir, f"frame{i}.png")
        screenshot(s, png, workdir=dest_dir)
        frames.append((png, 0.6 + 0.4 * i))
    gif = make_gif(frames, os.path.join(dest_dir, "selftest.gif"), workdir=dest_dir)
    probe = subprocess.run(
        ["ffprobe", "-v", "error", "-count_frames", "-select_streams", "v:0",
         "-show_entries", "stream=nb_read_frames:format=duration", "-of", "json", gif],
        capture_output=True, text=True)
    print(f"gif: {gif}\n{probe.stdout.strip()}")


def main(argv):
    keep = "--keep" in argv
    out_dir = OUT_DIR
    if "--out" in argv:
        out_dir = argv[argv.index("--out") + 1]
    names = [a for a in argv if not a.startswith("--") and a != out_dir]
    if "--list" in argv:
        print("\n".join(SHOTS))
        return 0
    if "--gif-selftest" in argv:
        dest = names[0] if names else tempfile.mkdtemp(prefix="rmx-gif-")
        gif_selftest(dest)
        return 0
    unknown = [n for n in names if n not in SHOTS]
    if unknown:
        raise SystemExit(f"unknown shot(s): {unknown}; have {list(SHOTS)}")
    build()
    print(f"font: {FONT}")
    failed = []
    for name in names or list(SHOTS):
        envs = []

        def make_env(config):
            e = Env(config, keep=keep)
            envs.append(e)
            return e

        out = os.path.join(out_dir, f"{name}.png")
        try:
            SHOTS[name](make_env, out)
            for e in envs:
                bad = e.panics()
                if bad:
                    failed.append(name)
                    print(f"  PANIC in {bad}")
            print(f"  {name}: {out}")
        except SystemExit as err:
            failed.append(name)
            print(f"  FAILED {name}: {err}")
        finally:
            for e in envs:
                if keep:
                    print(f"  kept {e.run}")
                e.teardown()
    if failed:
        print(f"failed: {failed}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
