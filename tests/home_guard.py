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

The variables each agent resolves its config directory from are dropped from
this process's environment at import, and a remux launched with one pointing
inside the real home is refused: `remux integrate` honours them ahead of HOME,
so one inherited from the developer's shell would aim an install at the real
`~/.claude` (or `~/.codex`, ...) whatever HOME says.
"""
import os
import pwd
import subprocess

# Captured at import, before any test overrides HOME.
REAL_HOMES = frozenset(
    os.path.realpath(p)
    for p in (os.path.expanduser("~"), pwd.getpwuid(os.getuid()).pw_dir)
)

AGENT_DIR_VARS = ("CLAUDE_CONFIG_DIR", "CODEX_HOME", "PI_CODING_AGENT_DIR", "PI_CONFIG_DIR",
                  "OPENCODE_CONFIG_DIR")
for _var in AGENT_DIR_VARS:
    os.environ.pop(_var, None)


def _inside_real_home(path):
    path = os.path.realpath(path)
    return any(path == h or path.startswith(h + "/") for h in REAL_HOMES)


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
    for var in AGENT_DIR_VARS:
        value = env.get(var)
        if value and os.path.isabs(value) and _inside_real_home(value):
            raise RuntimeError(
                f"refusing to start remux with {var}={value!r}: it is inside the real "
                "home, so an install would write the developer's own agent config.")


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
