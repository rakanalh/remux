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
import re
import shlex
import subprocess
import sys

def socket_dir(env):
    """The directory remux binds its socket in under `env`, by the rule in
    `src/paths.rs`: `XDG_RUNTIME_DIR` when it is absolute, else
    `$TMPDIR/remux-<uid>`, with `TMPDIR` only when it is absolute, else `/tmp`."""
    runtime = env.get("XDG_RUNTIME_DIR") or ""
    if os.path.isabs(runtime):
        return os.path.normpath(runtime)
    tmp = env.get("TMPDIR") or ""
    return os.path.join(os.path.normpath(tmp if os.path.isabs(tmp) else "/tmp"), f"remux-{os.getuid()}")


# Captured at import, before any test overrides HOME, XDG_RUNTIME_DIR or TMPDIR.
_REAL_SOCKET_DIR = socket_dir(os.environ)
_REAL_TMPDIR = os.environ.get("TMPDIR") if os.path.isabs(os.environ.get("TMPDIR") or "") else "/tmp"
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


def real_socket_dirs():
    """Where the developer's real server can listen: its socket dir as this
    process started, the default runtime dir, and the fallback under the real
    temp dir and under /tmp. Each resolved."""
    uid = os.getuid()
    return {os.path.realpath(d) for d in (_REAL_SOCKET_DIR, f"/run/user/{uid}",
                                          f"{_REAL_TMPDIR}/remux-{uid}", f"/tmp/remux-{uid}")}


def require_scoped_stop(argv, env=None, cwd=None):
    """Raise if `argv` is a `remux stop` or `restart` that could reach the real
    server. Both signal the pid in the socket directory's `remux.pid`, so the
    socket directory `env` gives, by the product's rule, must not be a real one.
    A relative `XDG_RUNTIME_DIR` is refused except where `_relative_runtime_ok`
    allows it, because an older binary takes it relative to `cwd`."""
    if not isinstance(argv, (list, tuple)) or len(argv) < 2 \
            or os.fsdecode(argv[1]) not in ("stop", "restart"):
        return
    env = os.environ if env is None else env
    command = os.fsdecode(argv[1])
    runtime = env.get("XDG_RUNTIME_DIR") or ""
    if runtime and not os.path.isabs(runtime):
        if not _relative_runtime_ok(runtime, env, cwd):
            raise RuntimeError(f"refusing to start remux {command} with XDG_RUNTIME_DIR={runtime!r}: "
                               "a relative value is allowed only below this run, with a private TMPDIR")
    elif runtime and _climbs(runtime):
        # A `..` component makes a path's meaning depend on the symlinks it climbs through.
        raise RuntimeError(f"refusing to start remux {command} with XDG_RUNTIME_DIR={runtime!r}: "
                           "it has a '..' component")
    tmpdir = env.get("TMPDIR") or ""
    if not os.path.isabs(runtime) and tmpdir and (not os.path.isabs(tmpdir) or _climbs(tmpdir)):
        raise RuntimeError(f"refusing to start remux {command} with TMPDIR={tmpdir!r}: "
                           "not an absolute path without '..'")
    target = socket_dir(env)
    if os.path.realpath(target) in real_socket_dirs():
        raise RuntimeError(f"refusing to start remux {command}: its socket dir {target} "
                           "is where the real server can listen")


def run_dirs():
    """The dirs this run owns: the root `rundirs.run_dir` made and an
    `RMX_RUNDIR` opt-out dir, each resolved. A dir that holds a real socket dir
    is left out, so it cannot vouch for a relative runtime dir."""
    found = []
    rundirs = sys.modules.get("rundirs")
    for candidate in (getattr(rundirs, "_root", None), os.environ.get("RMX_RUNDIR")):
        if not candidate or not os.path.isabs(candidate):
            continue
        resolved = os.path.realpath(candidate)
        if resolved.count("/") < 2:
            continue
        if any(real == resolved or real.startswith(resolved + "/") for real in real_socket_dirs()):
            continue
        found.append(resolved)
    return found


def _relative_runtime_ok(runtime, env, cwd):
    """Whether a relative `XDG_RUNTIME_DIR` provably stays inside this run.

    The current binary ignores it and binds in `$TMPDIR/remux-<uid>`, so TMPDIR
    must be absolute and resolve inside the run. An older binary takes it
    relative to the working directory, so that must resolve inside the run too,
    and the value must only descend from it: no empty, `.` or `..` component and
    nothing the shell computes. Where it lands is resolved and checked, so a
    symlink planted there cannot point it at a real socket dir."""
    if any(part in ("", ".", "..") for part in runtime.split("/")):
        return False
    if any(c in runtime for c in "$`~\\\0"):
        return False
    owned = run_dirs()
    tmpdir = env.get("TMPDIR") or ""
    if not owned or not os.path.isabs(tmpdir) or _climbs(tmpdir):
        return False

    def inside(path):
        return any(path == d or path.startswith(d + "/") for d in owned)

    here = os.path.realpath(os.fsdecode(cwd) if cwd is not None else os.getcwd())
    if not inside(here) or not inside(os.path.realpath(tmpdir)):
        return False
    return os.path.realpath(os.path.join(here, runtime)) not in real_socket_dirs()


def _climbs(path):
    return ".." in path.split("/")


def throwaway_home(default):
    """The HOME a test process already chose when it is not the real home, else
    `default`."""
    home = os.environ.get("HOME")
    if home and os.path.realpath(home) not in REAL_HOMES:
        return home
    return default


# The binary under test, as the harnesses resolve it; `REMUX_BIN` may name it anything.
_BIN = os.path.realpath(os.environ.get("REMUX_BIN", "target/debug/remux"))


def _is_remux_program(word):
    word = os.fsdecode(word)
    name = os.path.basename(word)
    return name == "remux" or name.startswith("remux-") or \
        ("/" in word and os.path.realpath(word) == _BIN)


# The command parser below is shared with `rundirs_selftest.py`, whose static
# guard reads source text with it, so the two layers cannot drift apart. A word
# it does not know (an expression in source) is None, and stops the parse.

SHELLS = {"sh", "bash", "zsh", "dash", "ksh", "fish"}
# Programs that run the rest of their arguments as another command.
WRAPPERS = {"env", "timeout", "nice", "setsid", "nohup", "command", "exec", "stdbuf", "ionice",
            "chrt", "taskset", "sudo", "doas", "time"}
# A wrapper's options that take the next word as their value.
_VALUE_OPTIONS = {
    "env": {"-u", "--unset", "-C", "--chdir", "-S", "--split-string"},
    "timeout": {"-s", "--signal", "-k", "--kill-after"},
    "nice": {"-n", "--adjustment"},
    "exec": {"-a"},
    "stdbuf": {"-i", "-o", "-e"},
    "ionice": {"-c", "-n", "-p", "--class", "--classdata"},
    "sudo": {"-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U"},
    "doas": {"-u", "-C"},
}
# How many plain words a wrapper takes before the command: a duration, a priority, a mask.
_POSITIONAL = {"timeout": 1, "chrt": 1, "taskset": 1}
# Shell words that come before a command without being one.
_RESERVED = {"if", "then", "else", "elif", "do", "while", "until", "!", "{", "}", "time"}
_ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=.*", re.S)


# Marks a value only the shell knows: a `$( )` or backtick substitution, or a
# `$` the shell does not expand. `_expand` refuses to resolve a value holding it.
_MARK = "`"


def _word_at(text, i, word):
    end = i + len(word)
    return text.startswith(word, i) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")) \
        and (end == len(text) or not (text[end].isalnum() or text[end] == "_"))


def _closing_paren(text, start):
    """The index just past the `)` closing a `$(` whose script starts at `start`,
    or len(text). Quoted text, nested parentheses and the `)` ending a `case`
    pattern are skipped."""
    depth, cases, pattern, quote, i = 1, 0, False, None, start
    while i < len(text):
        c = text[i]
        if quote:
            if c == quote:
                quote = None
            elif c == "\\" and quote == '"':
                i += 1
            i += 1
        elif c in "'\"":
            quote = c
            i += 1
        elif c == "\\":
            i += 2
        elif _word_at(text, i, "case"):
            cases += 1
            i += 4
        elif cases and _word_at(text, i, "esac"):
            cases -= 1
            pattern = False
            i += 4
        elif cases and (_word_at(text, i, "in") or text.startswith((";;", ";&"), i)):
            pattern = True
            i += 2
        elif c == "(":
            depth += 0 if pattern else 1
            i += 1
        elif c == ")":
            i += 1
            if pattern:
                pattern = False
                continue
            depth -= 1
            if depth == 0:
                return i
        else:
            i += 1
    return len(text)


def _substitutions(text):
    """(outer, inner): `text` with each `$( )` and backtick substitution, and
    each `$` the shell will not expand (inside single quotes, or after a
    backslash), replaced by the marker, and the scripts the substitutions run.
    `shlex` drops quoting, so this must happen before it."""
    outer, inner, quote, i = [], [], None, 0
    while i < len(text):
        c = text[i]
        if quote == "'":
            if c == "'":
                quote = None
            outer.append(_MARK if c == "$" else c)
            i += 1
        elif c == "\\" and i + 1 < len(text):
            outer.append(_MARK if text[i + 1] in "$`" else text[i:i + 2])
            i += 2
        elif c == "'" and quote is None:
            quote = "'"
            outer.append(c)
            i += 1
        elif c == '"':
            quote = None if quote == '"' else '"'
            outer.append(c)
            i += 1
        elif c == "$" and text[i + 1:i + 2] in ("'", '"'):
            # bash's `$'…'` expands backslash escapes, so `\x2f` hides a `/`
            # from the guard, and `$"…"` is a translated string: neither value
            # can be read off the source.
            outer.append(_MARK)
            i += 1
        elif text.startswith("$(", i):
            end = _closing_paren(text, i + 2)
            inner.append(text[i + 2:end - 1] if text[end - 1:end] == ")" else text[i + 2:])
            outer.append(_MARK)
            i = end
        elif c == "`":
            j = text.find("`", i + 1)
            j = len(text) if j < 0 else j
            inner.append(text[i + 1:j])
            outer.append(_MARK)
            i = j + 1
        else:
            outer.append(c)
            i += 1
    return "".join(outer), inner


def split_shell(text):
    """The simple commands in a shell string, each as a list of words. A
    command substitution stays in its word as a backtick, and the script it
    runs is split into commands of its own."""
    text, inner = _substitutions(text)
    out = []
    for part in re.split(r"[;\n|&()]+", text):
        try:
            words = shlex.split(part)
        except ValueError:
            words = part.split()
        while words and words[0] in _RESERVED:
            words = words[1:]
        if words:
            out.append(words)
    for script in inner:
        out += split_shell(script)
    return out


def unwrap(words):
    """(clear, assigns, argv): the environment changes in front of the real
    program, and its argv. `clear` is `env -i`; an `assigns` value of None is
    `env -u`. A None word stops the parse there."""
    clear, assigns, i = False, {}, 0
    while i < len(words):
        word = words[i]
        if word is None:
            break
        if _ASSIGNMENT.fullmatch(word):
            name, _, value = word.partition("=")
            assigns[name] = value
            i += 1
            continue
        name = os.path.basename(word)
        if name not in WRAPPERS:
            break
        i += 1
        positional = _POSITIONAL.get(name, 0)
        while i < len(words) and words[i] is not None:
            arg = words[i]
            if arg == "--":
                i += 1
                break
            if name == "env" and arg in ("-", "-i", "--ignore-environment"):
                clear, assigns = True, {}
                i += 1
            elif name == "env" and arg in ("-u", "--unset") and i + 1 < len(words):
                if words[i + 1] is not None:
                    assigns[words[i + 1]] = None
                i += 2
            elif name == "env" and _ASSIGNMENT.fullmatch(arg):
                key, _, value = arg.partition("=")
                assigns[key] = value
                i += 1
            elif arg.startswith("-") and arg != "-":
                i += 2 if arg in _VALUE_OPTIONS.get(name, ()) else 1
            elif positional:
                positional -= 1
                i += 1
            else:
                break
    return clear, assigns, words[i:]


def shell_script(argv):
    """(True, script) when `argv` is a shell given a script with `-c`, in any
    option cluster (`-c`, `-lc`, `-ec`); the script is None when it is not a
    literal. (False, None) otherwise."""
    if not argv or argv[0] is None or os.path.basename(argv[0]) not in SHELLS:
        return False, None
    i = 1
    while i < len(argv):
        word = argv[i]
        if word is None:
            return False, None
        if word in ("-o", "+o", "-O", "+O"):
            i += 2
            continue
        if word.startswith(("-", "+")) and not word.startswith("--"):
            if "c" in word[1:]:
                return True, argv[i + 1] if i + 1 < len(argv) else None
            i += 1
            continue
        if word.startswith("--"):
            i += 1
            continue
        return False, None
    return False, None


def commands(words, clear=False, assigns=None, state=None):
    """Every program `words` runs, as (clear, assigns, state, argv), looking past
    wrappers, into a shell's `-c` script and into `eval`. `assigns` holds the
    command's own assignments (`NAME=value cmd`, `env NAME=value cmd`), which it
    gets whatever else happens, and accumulates from outside in. `state` holds a
    script's `export`, `unset` and plain assignments before the command, which
    are not certain to have run (see `script_commands`)."""
    inner_clear, inner, argv = unwrap(words)
    merged = {} if inner_clear else dict(assigns or {})
    merged.update(inner)
    clear = clear or inner_clear
    state = dict(state or {})
    found = [(clear, merged, state, argv)] if argv else []
    is_shell, script = shell_script(argv)
    if is_shell and script is not None:
        found += script_commands(script, clear, merged, state)
    if argv and argv[0] is not None and os.path.basename(argv[0]) == "eval":
        found += script_commands(" ".join(w for w in argv[1:] if w is not None), clear, merged, state)
    return found


def script_commands(text, clear=False, assigns=None, state=None):
    """Every program a shell script runs, with the environment each one gets.

    `export NAME=value`, `export -n NAME`, `unset NAME` and a plain `NAME=value`
    statement are kept as script state for the later commands. `split_shell`
    flattens subshells, pipelines, `&`, `$( )` and `if`, so a statement may not
    have run, or may have run in another process; the state therefore only
    ever adds a check (`_check_launch`), never replaces one. A plain assignment
    reaches a command only when the name is already in the environment, which
    is when the shell exports it; its value is kept as ("plain", value)."""
    state = dict(state or {})
    found = []
    for words in split_shell(text):
        head = words[0]
        if head == "export":
            remove = False
            for word in words[1:]:
                if word is None or word == "--":
                    continue
                if word == "-n":
                    remove = True
                elif word.startswith("-"):
                    continue
                elif "=" in word:
                    name, _, value = word.partition("=")
                    state[name] = None if remove else value
                elif remove:
                    state[word] = None
                elif isinstance(state.get(word), tuple):
                    state[word] = state[word][1]
            continue
        if head == "unset":
            for word in words[1:]:
                if word is not None and not word.startswith("-"):
                    state[word] = None
            continue
        if all(word is not None and _ASSIGNMENT.fullmatch(word) for word in words):
            for word in words:
                name, _, value = word.partition("=")
                state[name] = value if isinstance(state.get(name), str) else ("plain", value)
            continue
        found += commands(words, clear, assigns, state)
    return found


def launches(args, shell=False, split=False):
    """Every program a launch runs. `args` is what Popen or pexpect was given: a
    shell string with `shell`, a string that pexpect splits itself with `split`,
    and otherwise a list of words, or a string that is one program path."""
    if shell and isinstance(args, (list, tuple)) and args:
        # Popen with shell=True runs the first item as the script.
        args = args[0]
    if isinstance(args, (str, bytes, os.PathLike)):
        text = os.fsdecode(args)
        if shell:
            return script_commands(text)
        elif split:
            try:
                lines = [shlex.split(text)]
            except ValueError:
                lines = [text.split()]
        else:
            lines = [[text]]
    elif isinstance(args, (list, tuple)):
        lines = [[os.fsdecode(a) if isinstance(a, (str, bytes, os.PathLike)) else str(a) for a in args]]
    else:
        return []
    found = []
    for words in lines:
        found += commands(words)
    return found


# The names a check reads. A value for one of them that cannot be worked out
# from the source is refused rather than guessed.
_CHECKED = {"XDG_RUNTIME_DIR", "TMPDIR", "HOME"}
_REFERENCE = re.compile(r"\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([A-Za-z_][A-Za-z0-9_]*)\})")


def _expand(value, env):
    """`value` with each `$NAME` and `${NAME}` taken from `env`, or None when
    anything else is left to the shell: a backtick, `$(`, a name `env` does not
    hold, another expansion such as `${NAME:-default}`, or a tilde."""
    if "`" in value:
        return None
    # The shell expands a leading `~`, or one after a `:`, in an assignment.
    if value.startswith("~") or ":~" in value:
        return None
    if "$" in _REFERENCE.sub("", value):
        return None
    for match in _REFERENCE.finditer(value):
        if (match.group(1) or match.group(2)) not in env:
            return None
    return _REFERENCE.sub(lambda m: env[m.group(1) or m.group(2)], value)


def _apply(env, changes):
    """`env` with `changes` applied in order, each value expanded from `env` as
    it stands at that point. An unknown value for a name a check reads raises."""
    for name, value in changes.items():
        if value is None:
            env.pop(name, None)
            continue
        plain = isinstance(value, tuple)
        if plain:
            if name not in env:
                continue
            value = value[1]
        expanded = _expand(value, env)
        if expanded is None:
            if name in _CHECKED:
                raise RuntimeError(f"refusing to start remux with {name}={value!r}: the shell "
                                   "computes it, so where it points cannot be checked")
            env.pop(name, None)
        else:
            env[name] = expanded
    return env


def _substituted_stop(argv):
    """A program the shell substitutes (`$(command -v remux) stop`) given `stop`
    or `restart`: it may be remux, so it is checked as remux."""
    return argv[0] is not None and _MARK in argv[0] and len(argv) >= 2 and argv[1] in ("stop", "restart")


def _check_launch(args, env, shell=False, split=False, cwd=None):
    """Refuse a remux launch that could reach the real home or the real server.
    A command is checked against its environment without the script state
    before it, and again with it, and must pass both: a script statement can
    make a stop refused, never allowed."""
    base = os.environ if env is None else env
    for clear, assigns, state, argv in launches(args, shell, split):
        if not argv or not (_is_remux_program(argv[0]) or _substituted_stop(argv)):
            continue
        start = {} if clear else dict(base)
        envs = [_apply(dict(start), assigns)]
        if state:
            envs.append(_apply(_apply(dict(start), state), assigns))
        for effective in envs:
            require_isolated_home(effective)
            require_scoped_stop(argv, effective, cwd)


_installed = False


def install():
    global _installed
    if _installed:
        return
    _installed = True

    popen_init = subprocess.Popen.__init__

    def guarded_popen_init(self, args, *a, **kw):
        _check_launch(args, kw.get("env"), kw.get("shell", False), cwd=kw.get("cwd"))
        popen_init(self, args, *a, **kw)

    subprocess.Popen.__init__ = guarded_popen_init

    try:
        import pexpect
    except ImportError:
        return
    spawn_init = pexpect.spawn.__init__

    def guarded_spawn_init(self, command, args=None, *a, **kw):
        # pexpect splits a command string into words itself when no args are given.
        if args:
            _check_launch([command, *args], kw.get("env"), cwd=kw.get("cwd"))
        else:
            _check_launch(command, kw.get("env"), split=True, cwd=kw.get("cwd"))
        spawn_init(self, command, args if args is not None else [], *a, **kw)

    pexpect.spawn.__init__ = guarded_spawn_init


install()
