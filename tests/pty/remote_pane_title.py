#!/usr/bin/env python3
"""The CLIENT's `pane_title` names panes on a REMOTE server too.

A local server and a "remote" one reached through the fake-remote recipe (an
`ssh` shim that execs `remux relay` against a second isolated server). The
remote has its own template, `remote {command}`, which the client must
override. The client's template is `{host}:{command}!`:

  1  the sessions panel names the remote's pane `mini:sh!` (the tree the remote
     sent this client), and the local one `sh!` (`{host}` is empty there, and
     the dangling `:` is trimmed), which also shows the local server was told
  2  after jumping onto the remote pane, its border, which the REMOTE server
     composites, reads `mini:sh!`, not `remote sh`

Run from the repo root:
    python3 tests/pty/remote_pane_title.py [-v]
"""
import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sidebar_sessions as h  # noqa: E402

h.RUN = f"/tmp/rmx-rpt-{os.getpid()}"
h.SOCK1 = f"{h.RUN}/run/remux.sock"
h.SOCK2 = f"{h.RUN}/run2/remux.sock"

CLIENT_CFG = (
    h.CFG_SIDEBAR_REMOTE
    + """
[appearance]
pane_title = "{host}:{command}!"
"""
)
REMOTE_CFG = """
[appearance]
pane_title = "remote {command}"
"""


def setup():
    h.setup_dirs()
    h.write_config(f"{h.RUN}/cfg-remote", CLIENT_CFG)
    # The shim's remote reads `cfg-none`, which is also the remote server's
    # config dir in `env_remote`.
    h.write_config(f"{h.RUN}/cfg-none", REMOTE_CFG)


def main():
    setup()
    h.start_server(h.SOCK1, h.env_local(f"{h.RUN}/cfg"))
    h.start_server(h.SOCK2, h.env_remote())
    seeds = h.seed_local()
    rseed = h.seed_remote()
    child, screen, pump = h.spawn(h.base_env(f"{h.RUN}/run", f"{h.RUN}/state",
                                             f"{h.RUN}/data", f"{h.RUN}/cfg-remote"))
    try:
        pump(3.0)
        rows = h.panel_rows(screen)
        h.log("panel:", rows)
        local_body = "\n".join(h.content_rows(screen))
        h.check("1 a local pane is named with the client's template, with no dangling separator",
                "╭ sh! " in local_body and ":sh" not in local_body, repr(local_body[:300]))
        # The pane rows sit under their tab rows, so expand the remote's tab.
        child.send(b"\x1b2")
        pump(0.5)
        y = h.panel_row_of(screen, "mini1")
        assert y is not None, rows
        child.send(b"g")
        pump(0.3)
        for _ in range(y - 1):
            child.send(b"j")
            pump(0.1)
        child.send(b"j")
        pump(0.2)
        child.send(b"l")
        pump(0.8)
        rows = h.panel_rows(screen)
        h.log("panel expanded:", rows)
        h.check("1 the remote's pane is named with the client's template and {host}",
                any("mini:sh!" in r for r in rows), rows)
        h.check("1 and not with the remote server's own template",
                not any("remote sh" in r for r in rows), rows)

        child.send(b"j")
        pump(0.2)
        child.send(b"\r")
        pump(2.0)
        body = "\n".join(h.content_rows(screen))
        h.check("2 attached to the remote", "R_ONE" in body, repr(body[-300:]))
        h.check("2 the remote pane's border uses the client's template",
                "mini:sh!" in body and "remote sh" not in body, repr(body[:400]))
        h.check("the client is alive", child.isalive())
        h.check_no_panic(f"{h.RUN}/state", f"{h.RUN}/state2")
    finally:
        h.teardown(child)
        for w in list(seeds) + [rseed]:
            w.close()
        subprocess.run(["pkill", "-x", "-f", f"{h.BIN} relay"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for env in (h.env_local(f"{h.RUN}/cfg"), h.env_remote()):
            subprocess.run([h.BIN, "stop"], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=10)
        shutil.rmtree(h.RUN, ignore_errors=True)
    print(f"\n{'FAILED' if h.FAILURES else 'OK'}: {len(h.FAILURES)} failure(s)")
    sys.exit(1 if h.FAILURES else 0)


if __name__ == "__main__":
    main()
