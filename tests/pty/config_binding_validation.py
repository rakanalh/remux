#!/usr/bin/env python3
"""A typo'd keybinding is reported at config LOAD, not silently at keypress.

`Config::validate` only ever checked `@group` references, so a binding naming a
command that does not exist (`"Alt-y" = "PaneFocusRigth"`) was accepted, and the
mistake surfaced only as a `log::error!` the moment the key was pressed -- the
key just did nothing. Validation now resolves every bound action string through
the one action registry and names the offending binding at startup.

Two modes:
  typo    a config.toml with a bad shortcut AND a bad tree leaf; the test
          asserts BOTH are named in client.log at startup, with their key path.
  valid   a config.toml whose bindings all resolve; the test asserts nothing is
          reported (a valid config must still load silently).

Run from the repo root:
    python3 tests/pty/config_binding_validation.py typo
    python3 tests/pty/config_binding_validation.py valid
    python3 tests/pty/config_binding_validation.py          # both
"""
import sys
from pty_harness import Tui

TYPO_CONFIG = """
[keybindings.command]
"Alt-y" = "PaneFocusRigth"

[keybindings.command.w]
n = "ViewNwe"
"""

VALID_CONFIG = """
[keybindings.command]
"Alt-y" = "PaneFocusRight"

[keybindings.command.w]
n = "ViewNew"
"""


def run(mode):
    config = TYPO_CONFIG if mode == "typo" else VALID_CONFIG
    t = Tui(f"/tmp/rmxcbv{mode}", config=config).start()
    fails = []
    try:
        log = t.log("client")
        reported = [line for line in log.splitlines() if "unknown action" in line]
        print(f"--- {mode}: {len(reported)} reported")
        for line in reported:
            print(f"    {line.strip()}")

        if mode == "typo":
            if not any("'Alt-y'" in l and "'PaneFocusRigth'" in l for l in reported):
                fails.append("bad shortcut 'Alt-y' was not reported by name")
            if not any("'w n'" in l and "'ViewNwe'" in l for l in reported):
                fails.append("bad tree binding 'w n' was not reported by key path")
        else:
            if reported:
                fails.append(f"a VALID config reported problems: {reported}")

        # Either way the client keeps running -- a bad binding is reported,
        # not fatal.
        if not t.alive():
            fails.append("client died")
        if "panic" in (t.log("client") + t.log("server")).lower():
            fails.append("panic in the logs")

        if fails:
            print("\nFAILURES:")
            for f in fails:
                print(f"  - {f}")
            print(f"RESULT: FAIL ({mode})")
            return 1
        print(f"RESULT: PASS ({mode})")
        return 0
    finally:
        t.kill()


def main():
    modes = [sys.argv[1]] if len(sys.argv) > 1 else ["typo", "valid"]
    rc = 0
    for m in modes:
        rc |= run(m)
    sys.exit(rc)


if __name__ == "__main__":
    main()
