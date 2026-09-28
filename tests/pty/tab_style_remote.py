#!/usr/bin/env python3
"""The CLIENT's `tab_style` reaches a REMOTE server, and a reload re-tells it.

A local server and a "remote" one reached through the fake-remote recipe (an
`ssh` shim that execs `remux relay` against a second isolated server). The
client's config says `tab_style = "rounded"`; the remote's config says nothing,
so the remote's own fallback is `plain`. The only pane stack is on the remote,
so any cap glyph on screen was composited by the REMOTE server:

  1  before attaching to the remote, nothing on screen carries a cap glyph
  2  attached to the remote session, its stack's tab strip is closed by
     U+E0B6 / U+E0B4, and the remote's server.log shows it was sent
     `TabStyle { style: Rounded }` (the seeding wire client sends none)
  3  rewriting the client's config to `plain` (no restart) makes the caps go
     and the ` | ` separator come back, and the remote logs `Plain`

Run from the repo root:
    python3 tests/pty/tab_style_remote.py [-v]
"""
import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sidebar_sessions as h  # noqa: E402

h.RUN = f"/tmp/rmx-tsr-{os.getpid()}"
h.SOCK1 = f"{h.RUN}/run/remux.sock"
h.SOCK2 = f"{h.RUN}/run2/remux.sock"

LEFT_CAP, RIGHT_CAP = "", ""


def client_cfg(style):
    return h.CFG_SIDEBAR_REMOTE + f"""
[appearance.theme]
tab_style = "{style}"
"""


def seed_remote_stack():
    """mini1: two panes (R_ONE, R_TWO) stacked into one slot."""
    w = h.Wire(h.SOCK2)
    w.send({"CreateSession": {"name": "mini1", "folder": None}})
    w.send({"Attach": {"session_name": "mini1"}})
    w.send({"Resize": {"cols": 74, "rows": 29}})
    time.sleep(0.4)
    w.type("echo R_ONE\n")
    w.send({"Command": "PaneNew"})
    time.sleep(0.4)
    w.type("echo R_TWO\n")
    w.send({"Command": "PaneStackIntoLeft"})
    time.sleep(0.4)
    return w


def capped_rows(screen):
    return [r for r in h.content_rows(screen) if LEFT_CAP in r and RIGHT_CAP in r]


def any_cap(screen):
    return any(LEFT_CAP in r or RIGHT_CAP in r for r in screen.display)


def remote_log():
    return h.server_log(f"{h.RUN}/state2")


def main():
    h.setup_dirs()
    cfg_dir = f"{h.RUN}/cfg-remote"
    h.write_config(cfg_dir, client_cfg("rounded"))
    local_env = h.env_local(f"{h.RUN}/cfg")
    remote_env = h.env_remote()
    h.start_server(h.SOCK1, local_env)
    h.start_server(h.SOCK2, remote_env)
    seeds = h.seed_local()
    rseed = seed_remote_stack()
    h.check("0 before the client dials, the remote has been told no tab style",
            "TabStyle" not in remote_log(),
            [l for l in remote_log().splitlines() if "TabStyle" in l])
    child, screen, pump = h.spawn(h.base_env(f"{h.RUN}/run", f"{h.RUN}/state",
                                             f"{h.RUN}/data", cfg_dir))
    try:
        pump(3.0)
        rows = h.panel_rows(screen)
        h.log("panel:", rows)
        h.check("1 the remote's session is listed", any("mini1" in r for r in rows), rows)
        h.check("1 no cap glyph on screen while showing the local session",
                not any_cap(screen), "\n".join(screen.display))

        child.send(b"\x1b2")
        pump(0.5)
        h.select_row(child, screen, pump, "mini1")
        child.send(b"\r")
        pump(2.5)
        body = "\n".join(h.content_rows(screen))
        h.log("attached:\n" + body)
        h.check("2 attached to the remote", "R_ONE" in body or "R_TWO" in body,
                repr(body[-400:]))
        capped = capped_rows(screen)
        h.check("2 the remote stack's strip is capped (U+E0B6 ... U+E0B4)",
                len(capped) == 1, "\n".join(h.content_rows(screen)))
        h.check("2 the capped strip has no ' | ' separator",
                all(" | " not in r for r in capped), capped)
        rlog = remote_log()
        h.check("2 the remote server.log shows TabStyle { style: Rounded }",
                "TabStyle { style: Rounded }" in rlog,
                [l for l in rlog.splitlines() if "TabStyle" in l])

        h.write_config(cfg_dir, client_cfg("plain"))
        deadline = time.time() + 6.0
        while time.time() < deadline and capped_rows(screen):
            pump(0.3)
        pump(0.5)
        body = "\n".join(h.content_rows(screen))
        h.log("after reload:\n" + body)
        h.check("3 after the reload to plain, no cap glyph on screen",
                not any_cap(screen), body)
        h.check("3 and the plain strip's ' | ' separator is back",
                any(" | " in r for r in h.content_rows(screen)), body)
        h.check("3 the remote server.log shows TabStyle { style: Plain }",
                "TabStyle { style: Plain }" in remote_log(),
                [l for l in remote_log().splitlines() if "TabStyle" in l])
        h.check("the client is alive", child.isalive())
        h.check_no_panic(f"{h.RUN}/state", f"{h.RUN}/state2")
    finally:
        h.teardown(child)
        for w in list(seeds) + [rseed]:
            w.close()
        subprocess.run(["pkill", "-x", "-f", f"{h.BIN} relay"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for env in (local_env, remote_env):
            subprocess.run([h.BIN, "stop"], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=10)
        shutil.rmtree(h.RUN, ignore_errors=True)
    print(f"\n{'FAILED' if h.FAILURES else 'OK'}: {len(h.FAILURES)} failure(s)")
    sys.exit(1 if h.FAILURES else 0)


if __name__ == "__main__":
    main()
