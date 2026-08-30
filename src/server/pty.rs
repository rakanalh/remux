use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

use anyhow::{Context, Result};
use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{self, ForkResult, Pid};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Represents an allocated PTY with a running child process.
pub struct Pty {
    /// The master side of the PTY pair.
    pub master_fd: OwnedFd,
    /// The PID of the child process running in the PTY.
    pub child_pid: Pid,
}

impl Pty {
    /// Spawn a new PTY with the given dimensions, optional command and
    /// arguments, and optional working directory.
    ///
    /// If `command` is `None`, the shell from `$SHELL` is used, falling back
    /// to `/bin/sh`. If `cwd` is `Some`, the child process starts in that
    /// directory; otherwise it inherits the parent's working directory.
    ///
    /// `args` is the argument vector AFTER argv[0] -- `["-R", "/tmp/f"]` for
    /// `nvim -R /tmp/f`. It also decides how argv[0] itself is presented, and
    /// the two cases are genuinely different programs:
    ///
    /// * **Empty** (every pane shell, and the `files` plugin's file manager):
    ///   argv[0] is `-<basename>`, the leading-dash LOGIN convention, exactly as
    ///   before this parameter existed.
    /// * **Non-empty** (the `browser` plugin's editor): argv[0] is the plain
    ///   basename. A leading dash tells a SHELL to source its login files; to
    ///   `vim` it is an unrecognised program name, and there is no reason to
    ///   hand one to a program that was invoked to edit a file.
    ///
    /// # Safety
    ///
    /// This function uses `fork()` internally, which is inherently unsafe in
    /// multi-threaded programs. It should be called early, before spawning
    /// other threads, or with careful consideration of the fork-safety
    /// implications.
    pub fn spawn(
        cols: u16,
        rows: u16,
        command: Option<&str>,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> Result<Pty> {
        log::debug!(
            "pty: spawn cols={}, rows={}, command={:?}, args={:?}, cwd={:?}",
            cols,
            rows,
            command,
            args,
            cwd
        );

        // Built BEFORE the fork. `CString::new` allocates, and the child of a
        // fork in a multi-threaded process may only call async-signal-safe
        // functions -- the allocator lock can be held by a thread that does not
        // exist on this side of the fork. Everything below the fork just reads
        // these.
        let c_args: Vec<std::ffi::CString> = args
            .iter()
            .map(|a| std::ffi::CString::new(a.as_str()))
            .collect::<std::result::Result<_, _>>()
            .context("an argument for the pane's command contains a NUL byte")?;

        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let OpenptyResult { master, slave } =
            openpty(&winsize, None).context("failed to open PTY pair")?;

        // SAFETY: We are about to fork. The child process will exec immediately,
        // so we avoid calling any async-signal-unsafe functions beyond what is
        // strictly necessary for setting up the terminal and executing the shell.
        match unsafe { unistd::fork() }.context("fork failed")? {
            ForkResult::Child => {
                // -- Child process --
                // Close master fd in child; we only need the slave side.
                drop(master);

                // Create a new session and set the slave as the controlling terminal.
                unistd::setsid().expect("setsid failed");

                // Set the slave as the controlling terminal via ioctl.
                // SAFETY: TIOCSCTTY is a well-defined ioctl for setting the
                // controlling terminal. The slave fd is valid.
                unsafe {
                    if libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                        libc::_exit(1);
                    }
                }

                // Redirect stdin/stdout/stderr to the slave PTY.
                unistd::dup2(slave.as_raw_fd(), libc::STDIN_FILENO).expect("dup2 stdin failed");
                unistd::dup2(slave.as_raw_fd(), libc::STDOUT_FILENO).expect("dup2 stdout failed");
                unistd::dup2(slave.as_raw_fd(), libc::STDERR_FILENO).expect("dup2 stderr failed");

                // Close the original slave fd if it is not one of 0/1/2.
                if slave.as_raw_fd() > 2 {
                    drop(slave);
                }

                // Change to the requested working directory, falling back
                // to $HOME if the directory does not exist.
                if let Some(dir) = cwd {
                    if std::env::set_current_dir(dir).is_err() {
                        if let Ok(home) = std::env::var("HOME") {
                            let _ = std::env::set_current_dir(home);
                        }
                    }
                }

                // Set TERM to match the escape sequences Remux generates.
                std::env::set_var("TERM", "xterm-256color");

                // Mark the environment so a `remux` launched inside this pane
                // can detect it is nested and refuse (mirrors tmux's $TMUX and
                // zellij's $ZELLIJ). Set here in the child, alongside TERM, so it
                // is inherited by the pane's shell and its descendants.
                std::env::set_var("REMUX", "1");

                // Determine which shell/command to execute.
                let shell = match command {
                    Some(cmd) => cmd.to_string(),
                    None => std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
                };

                let c_shell = std::ffi::CString::new(shell.as_str()).expect("CString::new failed");

                // Spawn the shell as a LOGIN shell. By convention, a shell
                // treats itself as a login shell when its argv[0] begins with a
                // leading dash (e.g. "-zsh"). This is what tmux/screen and
                // login(1) do, and it ensures login-only init files such as
                // ~/.zprofile, ~/.zlogin and ~/.bash_profile are sourced.
                // We still exec the real binary at `c_shell`, but present its
                // argv[0] as "-<basename>".
                //
                // Only when there are no ARGUMENTS, though. A program invoked
                // with a file to edit is not a login shell, and the dash would
                // simply be a program name it does not recognise.
                let shell_basename = std::path::Path::new(&shell)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(shell.as_str());
                let argv0 = if c_args.is_empty() {
                    format!("-{shell_basename}")
                } else {
                    shell_basename.to_string()
                };
                let c_argv0 = std::ffi::CString::new(argv0.as_str()).expect("CString::new failed");

                // exec the shell. On success this does not return; on failure
                // say so on the terminal and leave immediately.
                //
                // Deliberately NOT a panic. This is a forked child, so unwinding
                // runs the parent's hooks and atexit handlers in a process that
                // shares its memory image -- and the panic message goes to the
                // pane, where the user reads a Rust backtrace instead of the
                // name of the program that could not be started. Reachable by
                // ordinary config since the `files` sidebar plugin takes a
                // user-supplied `command`: one typo in `command = "yazi"` used
                // to paint a panic into the panel.
                let mut argv: Vec<&std::ffi::CStr> = Vec::with_capacity(1 + c_args.len());
                argv.push(&c_argv0);
                argv.extend(c_args.iter().map(|a| a.as_c_str()));
                let _ = unistd::execvp(&c_shell, &argv);
                let err = std::io::Error::last_os_error();
                let msg = format!("remux: cannot run {shell:?}: {err}\r\n");
                // SAFETY: a plain `write` to fd 2, which is the PTY slave here.
                // Async-signal-safe, which `println!` and unwinding are not.
                unsafe {
                    libc::write(
                        libc::STDERR_FILENO,
                        msg.as_ptr() as *const libc::c_void,
                        msg.len(),
                    );
                    libc::_exit(127) // 127 = "command not found", as a shell reports it
                }
            }
            ForkResult::Parent { child } => {
                // -- Parent process --
                // Close the slave side; we only communicate through the master.
                drop(slave);

                log::debug!("pty: child process spawned with pid={}", child);

                Ok(Pty {
                    master_fd: master,
                    child_pid: child,
                })
            }
        }
    }

    /// Read output from the PTY master asynchronously.
    ///
    /// Returns the bytes read, or an empty vec on EOF.
    pub async fn read_output(&self) -> Result<Vec<u8>> {
        let fd = self.master_fd.as_raw_fd();

        // Set non-blocking mode, required by AsyncFd.
        // SAFETY: The fd is valid as long as self.master_fd is alive.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // A `BorrowedFd` rather than a bare `RawFd` in a wrapper: this genuinely
        // IS a borrow -- `&self` outlives the await -- and saying so in the type
        // is what makes it safe. The wrapper it replaced carried no lifetime, so
        // the compiler could not tell this legitimate borrow from the one in
        // `start_reader` that outlived its descriptor and cost a pane every byte
        // of its output. Dropping a `BorrowedFd` closes nothing, so the
        // `into_inner()` dance that used to guard each exit is gone too.
        //
        // SAFETY: `fd` comes from `self.master_fd`, which `&self` keeps alive
        // for the whole of this function.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let async_fd = AsyncFd::with_interest(borrowed, Interest::READABLE)
            .context("AsyncFd creation failed")?;

        let mut buf = vec![0u8; 4096];

        loop {
            let mut guard = async_fd
                .readable()
                .await
                .context("waiting for readable failed")?;

            match guard.try_io(|inner| {
                // SAFETY: The fd is valid for the lifetime of the Pty struct,
                // and we are reading into a properly sized buffer.
                let n = unsafe {
                    libc::read(
                        inner.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(0)) => {
                    return Ok(Vec::new());
                }
                Ok(Ok(n)) => {
                    return Ok(buf[..n].to_vec());
                }
                Ok(Err(e)) => {
                    return Err(e).context("read from PTY master failed");
                }
                Err(_would_block) => {
                    // Spurious wakeup; try again.
                    continue;
                }
            }
        }
    }

    /// Write input bytes to the PTY master.
    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            let written = nix::unistd::write(&self.master_fd, &data[offset..])
                .context("write to PTY master failed")?;
            offset += written;
        }
        Ok(())
    }

    /// Resize the PTY to the given dimensions.
    ///
    /// This sends a `TIOCSWINSZ` ioctl to the master fd and then delivers
    /// `SIGWINCH` to the child process group so it can react to the new size.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: TIOCSWINSZ is a well-defined ioctl for setting window size.
        // The master fd is valid and the winsize struct is properly initialized.
        let ret = unsafe {
            libc::ioctl(
                self.master_fd.as_raw_fd(),
                libc::TIOCSWINSZ as libc::c_ulong,
                &winsize as *const Winsize,
            )
        };
        if ret == -1 {
            return Err(std::io::Error::last_os_error()).context("TIOCSWINSZ ioctl failed");
        }

        // Send SIGWINCH to the child process group so the shell re-reads the
        // terminal size.
        let _ = signal::killpg(self.child_pid, Signal::SIGWINCH);

        Ok(())
    }

    /// Check if the child process has exited without blocking.
    ///
    /// Returns `Some(exit_code)` if the child has exited, `None` if it is
    /// still running.
    pub fn try_wait(&self) -> Result<Option<i32>> {
        match waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)).context("waitpid failed")? {
            WaitStatus::Exited(_, code) => Ok(Some(code)),
            WaitStatus::Signaled(_, sig, _) => Ok(Some(128 + sig as i32)),
            WaitStatus::StillAlive => Ok(None),
            _ => Ok(None),
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Best-effort kill of the child process.
        let _ = signal::kill(self.child_pid, Signal::SIGHUP);
        // ...and then get out of the way. The signalled child needs a moment to
        // die before its status can be collected, and this `Drop` is reached
        // from `reap_panes` with the daemon's `panes` lock held: waiting here
        // blocks a runtime thread while that lock is held. The file-manager
        // sidebar panel closes a pane and opens another on every directory
        // change, so that would put a stall under a lock on a routine user
        // action -- harder to attribute later than the zombies it fixes.
        //
        // Nothing is shared with the spawned closure but the pid. That pid is
        // not necessarily ours to collect, and it is not a race we need to win:
        // see `reap_child`, which explains why another waiter getting there
        // first is the expected case rather than a problem.
        //
        // Two properties of `spawn_blocking` worth knowing, both read out of
        // tokio 1.50.0 rather than assumed:
        //
        // - On a runtime that has begun shutting down, the closure is NEVER
        //   RUN. `blocking::pool::spawn_task` shuts the task down and returns
        //   `SpawnError::ShuttingDown`, and the caller deliberately hands back a
        //   `JoinHandle` that never resolves ("Compat: do not panic here"). So
        //   the reap is LOST, not deferred -- and the `Err(_)` arm below does
        //   not cover it, because `Handle::try_current()` still succeeds while a
        //   runtime is shutting down. Harmless only because the process is on
        //   its way out and init reparents and reaps every zombie it leaves.
        // - `spawn_blocking` PANICS on `SpawnError::NoThreads`, i.e. when the OS
        //   refuses a new thread and none is free. Remote, but a panic inside a
        //   `Drop` that is itself running during an unwind aborts the process.
        let pid = self.child_pid;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || reap_child(pid));
            }
            // No runtime at all: nothing left to stall, so wait inline. Blocking
            // briefly is fine here; leaking is not.
            Err(_) => reap_child(pid),
        }
    }
}

/// How long [`reap_child`] waits for a signalled child before escalating, and
/// again before giving up.
///
/// 20ms is the WORST case, not the usual one: the loop polls at 1ms and returns
/// the instant the child is collected, so a shell exiting on SIGHUP costs 1-3ms.
/// Only a child that survives both SIGHUP and SIGKILL -- uninterruptible sleep --
/// pays the full bound.
///
/// It is bounded at all because of where this can run. On the normal path that
/// is a blocking-pool thread, which must not be parked for ever on a child that
/// will not die; on the no-runtime fallback it is `Drop` itself, which the
/// daemon reaches holding the `panes` lock, and a `Drop` that can hang is worth
/// nothing at all.
const REAP_WAIT: std::time::Duration = std::time::Duration::from_millis(20);
const REAP_POLL: std::time::Duration = std::time::Duration::from_millis(1);

/// Wait for an already-signalled child so it does not linger as a zombie.
///
/// A single `waitpid(WNOHANG)` right after the signal is not enough, and that is
/// what this replaced: the child has not exited yet at that instant, so the wait
/// reports `StillAlive`, the child dies a moment later, and nothing ever collects
/// its status. Every pane closed while its shell was still running left a
/// `<defunct>` entry behind for the life of the server -- which the file-manager
/// sidebar panel turns from a curiosity into a real leak, since it kills and
/// respawns its pane every time the focused pane's directory changes.
///
/// Always the SPECIFIC pid, never `waitpid(-1, ..)`: a wildcard reaper would
/// steal children that other code is waiting for.
///
/// **This is not the only waiter, and usually not the winning one.**
/// [`reap_panes`] calls [`Pty::try_wait`] on the same pid immediately before
/// dropping the `Pty`, to read the exit code -- and on the commonest path of all
/// (the PTY channel disconnected *because* the child died) that call collects it
/// and frees the pid before this ever runs. `ECHILD` is therefore SUCCESS, and it
/// is matched explicitly rather than swept up with every other errno, so that a
/// genuine failure is logged instead of silently reading as "reaped".
///
/// [`reap_panes`]: crate::server::daemon
fn reap_child(pid: Pid) {
    for stage in 0..2 {
        let deadline = std::time::Instant::now() + REAP_WAIT;
        loop {
            match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                // Still running: keep polling until this stage's deadline.
                Ok(WaitStatus::StillAlive) => {}
                // Collected here.
                Ok(_) => return,
                // Already collected elsewhere -- see the note above.
                Err(nix::errno::Errno::ECHILD) => return,
                Err(e) => {
                    log::warn!("pty: waitpid({pid}) failed: {e}");
                    return;
                }
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(REAP_POLL);
        }
        // SIGHUP was declined (or arrived while the child was blocked on a
        // signal-ignoring path); escalate once.
        if stage == 0 {
            let _ = signal::kill(pid, Signal::SIGKILL);
        }
    }
    log::warn!("pty: child {pid} did not exit after SIGKILL; leaving it unreaped");
}

/// Spawn a background tokio task that continuously reads from the PTY master
/// and sends output chunks through a channel.
///
/// Returns the task handle and the receiving end of the channel.
///
/// Takes an **owned** descriptor -- give it a `dup` of the master, not the
/// master itself -- and closes it when the task ends. There is no safety
/// contract left for the caller to honour, and that is the point: this used to
/// ask the caller to keep a borrowed fd alive for the task's lifetime, nobody
/// could, and the fd number was reissued to the next PTY while this task's epoll
/// registration still held it. See the comment on `AsyncFd` below.
pub fn start_reader(master_fd: OwnedFd) -> (JoinHandle<()>, mpsc::UnboundedReceiver<Vec<u8>>) {
    let raw = master_fd.as_raw_fd();
    log::debug!("pty: start_reader watching fd={raw}");

    // Set the fd to non-blocking mode, which is required by AsyncFd.
    // SAFETY: `master_fd` owns a valid descriptor for the whole call.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let (tx, rx) = mpsc::unbounded_channel();

    let handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];

        // The task OWNS its descriptor -- a `dup` of the master the caller made
        // for it -- and this is load-bearing, not tidiness.
        //
        // It used to borrow the raw fd. The task then outlived the descriptor:
        // when the pane's `Pty` was dropped the fd was closed, the kernel
        // silently dropped it from the epoll set, and `readable()` parked here
        // forever instead of erroring. The next PTY opened got the SAME fd
        // NUMBER back (descriptors are handed out lowest-free-first), and its
        // reader's registration collided with the parked one's: readiness for
        // the new pane went nowhere and the pane never produced a single byte.
        // Reproduced exactly: fd 13, reused twice, and a file-manager panel
        // that painted blank forever after its second re-target.
        //
        // Owning it closes the hole at the root: `AsyncFd` deregisters before it
        // drops the inner descriptor, so the fd NUMBER cannot be reissued while
        // any registration for it still exists.
        let async_fd = match AsyncFd::with_interest(master_fd, Interest::READABLE) {
            Ok(fd) => fd,
            Err(e) => {
                log::error!("start_reader: failed to create AsyncFd: {e}");
                return;
            }
        };

        loop {
            let mut guard = match async_fd.readable().await {
                Ok(g) => g,
                Err(e) => {
                    log::error!("start_reader: readable() failed: {e}");
                    break;
                }
            };

            match guard.try_io(|inner| {
                // SAFETY: The fd is valid (caller guarantees it) and we read
                // into a properly sized buffer.
                let n = unsafe {
                    libc::read(
                        inner.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(n)) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break; // Receiver dropped
                    }
                }
                Ok(Err(e)) => {
                    log::error!("start_reader: read error: {e}");
                    break;
                }
                Err(_would_block) => {
                    continue; // Spurious wakeup
                }
            }
        }

        // `async_fd` drops here: it deregisters from the reactor FIRST and only
        // then closes the descriptor it owns, which is the ordering that makes
        // the fd number safe to reissue. Nothing to do by hand -- the previous
        // `into_inner()` here existed to stop it closing a fd it did not own.
    });

    (handle, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_exit() {
        // Spawn a shell that immediately exits.
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), &[], None).expect("failed to spawn PTY");
        pty.write_input(b"exit\n").expect("write_input failed");

        // Wait for the child to exit.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let status = pty.try_wait().expect("try_wait failed");
        assert!(status.is_some(), "child should have exited");
    }

    #[test]
    fn resize_does_not_error() {
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), &[], None).expect("failed to spawn PTY");
        pty.resize(120, 40).expect("resize should not fail");
        pty.write_input(b"exit\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[tokio::test]
    async fn read_output_returns_data() {
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), &[], None).expect("failed to spawn PTY");
        pty.write_input(b"echo hello\n").expect("write failed");

        // Give the shell a moment to produce output.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let data = pty.read_output().await.expect("read_output failed");
        assert!(!data.is_empty(), "should have read some output");

        pty.write_input(b"exit\n").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn start_reader_receives_output() {
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), &[], None).expect("failed to spawn PTY");

        let (_handle, mut rx) = start_reader(pty.master_fd.try_clone().unwrap());

        pty.write_input(b"echo test_marker\n")
            .expect("write failed");

        // Collect output for a short while.
        let mut collected = Vec::new();
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(2));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                data = rx.recv() => {
                    match data {
                        Some(d) => {
                            collected.extend_from_slice(&d);
                            let output = String::from_utf8_lossy(&collected);
                            if output.contains("test_marker") {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = &mut timeout => break,
            }
        }

        let output = String::from_utf8_lossy(&collected);
        assert!(
            output.contains("test_marker"),
            "expected 'test_marker' in output, got: {output}"
        );

        pty.write_input(b"exit\n").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn spawns_login_shell() {
        // A login shell has an argv[0] that begins with a leading dash, so
        // `$0` inside the shell reports "-sh" rather than "/bin/sh" or "sh".
        // The sentinel is split across a shell string concatenation
        // ("AR""GV0=") so the assembled token "ARGV0=" appears only in the
        // shell's actual output, never in the (terminal-echoed) command line.
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), &[], None).expect("failed to spawn PTY");

        let (_handle, mut rx) = start_reader(pty.master_fd.try_clone().unwrap());

        pty.write_input(b"echo \"AR\"\"GV0=$0\"\n")
            .expect("write failed");

        let mut collected = Vec::new();
        let mut argv0: Option<String> = None;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(2));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                data = rx.recv() => {
                    match data {
                        Some(d) => {
                            collected.extend_from_slice(&d);
                            let output = String::from_utf8_lossy(&collected);
                            if let Some(start) = output.find("ARGV0=") {
                                let rest = &output[start + "ARGV0=".len()..];
                                if let Some(end) = rest.find(['\r', '\n']) {
                                    argv0 = Some(rest[..end].to_string());
                                    break;
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = &mut timeout => break,
            }
        }

        pty.write_input(b"exit\n").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let argv0 = argv0.unwrap_or_else(|| {
            panic!(
                "did not observe $0 output; got: {}",
                String::from_utf8_lossy(&collected)
            )
        });
        assert!(
            argv0.starts_with('-'),
            "expected a login shell (argv[0] beginning with '-'), got $0={argv0:?}"
        );
    }
}
