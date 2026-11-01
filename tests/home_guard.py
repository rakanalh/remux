"""Refuse to start a remux process whose HOME is the developer's real home.

The server has a one-time read-only migration that reads the platform data
directory when the XDG data directory has no state.json. On macOS that
directory is `$HOME/Library/Application Support`, so a test server that keeps
the real HOME loads the developer's real sessions. Setting the XDG variables
alone does not prevent that.

Importing this module installs the check on `subprocess.Popen` and
`pexpect.spawn`, so every harness and every hand-built environment is covered
without each call site having to remember it. The check runs when the process is
launched, because several tests set `os.environ["HOME"]` in `main()` after
their imports.
"""
import os
import pwd
import subprocess

# Captured at import, before any test overrides HOME.
REAL_HOMES = frozenset(
    os.path.realpath(p)
    for p in (os.path.expanduser("~"), pwd.getpwuid(os.getuid()).pw_dir)
)


def require_isolated_home(env=None):
    """Raise if `env` (default: this process's environment) would run remux
    against the real home. A missing HOME counts as the real home, because
    remux then falls back to the passwd entry."""
    env = os.environ if env is None else env
    home = env.get("HOME")
    if not home or os.path.realpath(home) in REAL_HOMES:
        raise RuntimeError(
            f"refusing to start remux with HOME={home!r}: that is the real home, "
            "so the server would read the real sessions. Point HOME at a "
            "throwaway directory together with the XDG_* directories.")


def throwaway_home(default):
    """The HOME a test process already chose when it is not the real home, else
    `default`."""
    home = os.environ.get("HOME")
    if home and os.path.realpath(home) not in REAL_HOMES:
        return home
    return default


def _is_remux(argv):
    first = argv[0] if isinstance(argv, (list, tuple)) and argv else argv
    return isinstance(first, (str, bytes, os.PathLike)) and \
        os.path.basename(os.fsdecode(first)) == "remux"


_installed = False


def install():
    global _installed
    if _installed:
        return
    _installed = True

    popen_init = subprocess.Popen.__init__

    def guarded_popen_init(self, args, *a, **kw):
        if _is_remux(args):
            require_isolated_home(kw.get("env"))
        popen_init(self, args, *a, **kw)

    subprocess.Popen.__init__ = guarded_popen_init

    try:
        import pexpect
    except ImportError:
        return
    spawn_init = pexpect.spawn.__init__

    def guarded_spawn_init(self, command, args=None, *a, **kw):
        if _is_remux(command):
            require_isolated_home(kw.get("env"))
        spawn_init(self, command, args if args is not None else [], *a, **kw)

    pexpect.spawn.__init__ = guarded_spawn_init


install()
