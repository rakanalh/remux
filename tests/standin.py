"""Installing a stand-in program under a name remux's agent detection sees.

The server names a pane's agent after the leader of the terminal's foreground
process group, and no script can carry a chosen name on both platforms: Linux
names a direct-shebang script after the script, but macOS names a process
after its EXECUTABLE (`proc_pidpath`), which for any script is the
interpreter -- the Xcode `python3` re-execs itself as `Python`. So
`install_wrapper` compiles a binary at the chosen path that runs the real
program as its child and stays the group's leader.
"""
import json
import os
import shutil
import stat
import subprocess
import sys

# Forwards the signals sent to the leader's pid alone, so the child never
# outlives the wrapper that names it. One sent to the whole group, as a
# pane's close does, reaches the child twice, which nothing here minds.
_WRAPPER_C = r"""
#include <errno.h>
#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>

static char *prefix[] = {PREFIX, 0};
static volatile pid_t child;

static void forward(int sig) {
    if (child > 0) kill(child, sig);
}

int main(int argc, char **argv) {
    int fwd[] = {SIGINT, SIGTERM, SIGHUP, SIGQUIT};
    for (unsigned i = 0; i < sizeof fwd / sizeof *fwd; i++) signal(fwd[i], forward);
    unsigned n = sizeof prefix / sizeof *prefix - 1;
    char *args[n + argc];
    for (unsigned i = 0; i < n; i++) args[i] = prefix[i];
    for (int i = 1; i < argc; i++) args[n + i - 1] = argv[i];
    args[n + argc - 1] = 0;
    pid_t pid = fork();
    if (pid == 0) {
        for (unsigned i = 0; i < sizeof fwd / sizeof *fwd; i++) signal(fwd[i], SIG_DFL);
        execv(args[0], args);
        _exit(127);
    }
    child = pid;
    int st;
    while (waitpid(pid, &st, 0) < 0 && errno == EINTR) {}
    if (WIFEXITED(st)) return WEXITSTATUS(st);
    if (WIFSIGNALED(st)) return 128 + WTERMSIG(st);
    return 1;
}
"""


def require_private_dir(path):
    """Refuse a directory another user could write into, and close ours.

    The harness BINDIRs sit at fixed `/tmp` paths and are made with
    `exist_ok=True`, so one another local user created first is accepted
    silently, and they could swap the source, the binary or the program
    between install and the pane running it. A group-writable one of ours
    is what a umask of 002 makes, so it is tightened to 0700 rather than
    refused -- the caller's directory, that is, which nobody else should be
    sharing.

    Every ancestor is checked too: a private directory inside one another
    user owns can be renamed aside, and another put in its place. An
    ancestor may be ours or root's, never writable by others unless it is a
    sticky root directory (`/tmp`), and never group-writable unless ours.
    """
    euid = os.geteuid()
    parent = os.path.dirname(os.path.realpath(path))
    while True:
        up = os.lstat(parent)
        if up.st_uid not in (euid, 0):
            raise SystemExit(f"standin: refusing {path}: its ancestor {parent} is owned by uid {up.st_uid}")
        sticky_root = up.st_uid == 0 and up.st_mode & stat.S_ISVTX
        if not sticky_root and (up.st_mode & 0o002 or (up.st_mode & 0o020 and up.st_uid != euid)):
            raise SystemExit(f"standin: refusing {path}: its ancestor {parent} is writable by others "
                             f"({stat.S_IMODE(up.st_mode):o})")
        if parent == os.path.dirname(parent):
            break
        parent = os.path.dirname(parent)
    st = os.lstat(path)
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
        raise SystemExit(f"standin: refusing {path}: not a real directory")
    if st.st_uid != euid:
        raise SystemExit(f"standin: refusing {path}: owned by uid {st.st_uid}, not {euid}")
    if st.st_mode & 0o002:
        raise SystemExit(f"standin: refusing {path}: writable by others "
                         f"({stat.S_IMODE(st.st_mode):o})")
    os.chmod(path, 0o700)


def install_wrapper(path, prefix, files, fallback):
    """Make `path` a binary that runs `prefix + its own arguments` as its child.

    `prefix[0]` is an absolute path. `files` maps each further path the
    program needs (the script `prefix` runs) to its text, and they are
    written first. Without a working `cc`, `fallback()` is called to install
    a script at `path` instead, which only Linux detects, so on macOS that is
    an error rather than a harness whose every agent case fails for no
    visible reason.
    """
    require_private_dir(os.path.dirname(os.path.abspath(path)))
    src = path + ".c"
    # Nobody else can add a name here once the directory is closed, so
    # removing what is there leaves no link planted before that for the
    # writes below to follow.
    for name in (path, src, *files):
        try:
            os.unlink(name)
        except FileNotFoundError:
            pass
    for name, text in files.items():
        with os.fdopen(os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644), "w") as f:
            f.write(text)
    cc = shutil.which("cc")
    error = "no `cc` on PATH"
    if cc:
        literals = ", ".join(json.dumps(arg) for arg in prefix)
        with open(src, "w") as f:
            f.write(_WRAPPER_C.replace("PREFIX", literals, 1))
        r = subprocess.run([cc, "-O1", "-o", path, src], capture_output=True, text=True)
        os.remove(src)
        if r.returncode == 0:
            return
        error = f"`cc` failed: {r.stderr.strip()[:300]}"
    if sys.platform == "darwin":
        raise SystemExit(f"standin: cannot build {path} ({error}); macOS needs it")
    print(f"note: {os.path.basename(path)} is a script ({error}); detected on Linux only", file=sys.stderr)
    fallback()
