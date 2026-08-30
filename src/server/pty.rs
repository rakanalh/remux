use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use anyhow::{Context, Result};
use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{self, ForkResult, Pid};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A thin wrapper around a raw file descriptor that implements `AsRawFd`.
///
/// This is used to register a borrowed raw fd with tokio's `AsyncFd`
/// without transferring ownership. The caller is responsible for ensuring
/// the underlying fd outlives this wrapper.
struct RawFdWrapper(RawFd);

impl AsRawFd for RawFdWrapper {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Represents an allocated PTY with a running child process.
pub struct Pty {
    /// The master side of the PTY pair.
    pub master_fd: OwnedFd,
    /// The PID of the child process running in the PTY.
    pub child_pid: Pid,
}

impl Pty {
    /// Spawn a new PTY with the given dimensions, optional command, and
    /// optional working directory.
    ///
    /// If `command` is `None`, the shell from `$SHELL` is used, falling back
    /// to `/bin/sh`. If `cwd` is `Some`, the child process starts in that
    /// directory; otherwise it inherits the parent's working directory.
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
        cwd: Option<&std::path::Path>,
    ) -> Result<Pty> {
        log::debug!(
            "pty: spawn cols={}, rows={}, command={:?}, cwd={:?}",
            cols,
            rows,
            command,
            cwd
        );

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
                let shell_basename = std::path::Path::new(&shell)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(shell.as_str());
                let login_argv0 = format!("-{shell_basename}");
                let c_argv0 =
                    std::ffi::CString::new(login_argv0.as_str()).expect("CString::new failed");

                // exec the shell. On success this does not return.
                // On failure, expect() panics (which is appropriate for a
                // post-fork child process).
                #[allow(unreachable_code)]
                {
                    unistd::execvp(&c_shell, &[&c_argv0]).expect("execvp failed");
                    // SAFETY: execvp either succeeds (never returns) or expect()
                    // panics. This line is unreachable but satisfies the type
                    // checker.
                    unsafe { libc::_exit(1) }
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

        let wrapper = RawFdWrapper(fd);
        let async_fd = AsyncFd::with_interest(wrapper, Interest::READABLE)
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
                    // Prevent AsyncFd from closing the fd (we don't own it).
                    let _ = async_fd.into_inner();
                    return Ok(Vec::new());
                }
                Ok(Ok(n)) => {
                    let _ = async_fd.into_inner();
                    return Ok(buf[..n].to_vec());
                }
                Ok(Err(e)) => {
                    let _ = async_fd.into_inner();
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
        // ...and then get OUT of the way. The signalled child needs a moment to
        // die before its status can be collected, and this `Drop` is reached
        // from `reap_panes` with the daemon's `panes` lock held: waiting here,
        // even briefly and even bounded, blocks a runtime thread while that lock
        // is held. The file-manager sidebar panel closes a pane and opens
        // another on every directory change, so that would put a stall under a
        // lock on a routine user action -- a stutter that is far harder to
        // attribute later than the zombies it was fixing.
        //
        // Hand the pid to a blocking task instead. Nothing here is shared with
        // it but the pid, and the pid cannot be recycled behind its back: an
        // unreaped child holds its own pid until somebody waits for it, and that
        // somebody is this task.
        let pid = self.child_pid;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || reap_child(pid));
            }
            // No runtime: the daemon is shutting down, and there is nothing left
            // to stall. Blocking briefly is fine here; leaking is not.
            Err(_) => reap_child(pid),
        }
    }
}

/// How long [`reap_child`] waits for a signalled child before escalating, and
/// again before giving up.
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
/// steal children other code is waiting for. One other place waits these pids --
/// `reap_panes` calls [`Pty::try_wait`] just before dropping the `Pty`, to read
/// the exit code -- so losing the race is normal and expected. `ECHILD` is
/// therefore SUCCESS, not an error: it means somebody else collected the child
/// first, which is the outcome this function wanted.
///
/// Bounded at both stages, because it runs on a blocking-pool thread: a child
/// that refuses to die is worth one leaked zombie, but not a thread parked on it
/// forever.
fn reap_child(pid: Pid) {
    for stage in 0..2 {
        let deadline = std::time::Instant::now() + REAP_WAIT;
        loop {
            match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                // Still running: keep waiting until this stage's deadline.
                Ok(WaitStatus::StillAlive) => {}
                // Collected here, or already collected elsewhere (`ECHILD`).
                // Both mean there is no zombie left to worry about.
                _ => return,
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
/// # Safety
///
/// The caller must ensure that `master_fd` remains valid for the lifetime of
/// the returned task. The task does not own the fd and will not close it.
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

        // Prevent the AsyncFd from closing the borrowed fd on drop.
        let _ = async_fd.into_inner();
    });

    (handle, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_exit() {
        // Spawn a shell that immediately exits.
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), None).expect("failed to spawn PTY");
        pty.write_input(b"exit\n").expect("write_input failed");

        // Wait for the child to exit.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let status = pty.try_wait().expect("try_wait failed");
        assert!(status.is_some(), "child should have exited");
    }

    #[test]
    fn resize_does_not_error() {
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), None).expect("failed to spawn PTY");
        pty.resize(120, 40).expect("resize should not fail");
        pty.write_input(b"exit\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[tokio::test]
    async fn read_output_returns_data() {
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), None).expect("failed to spawn PTY");
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
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), None).expect("failed to spawn PTY");

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
        let pty = Pty::spawn(80, 24, Some("/bin/sh"), None).expect("failed to spawn PTY");

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
