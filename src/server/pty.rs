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
                    if libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0) == -1 {
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

                // Determine which shell/command to execute.
                let shell = match command {
                    Some(cmd) => cmd.to_string(),
                    None => std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
                };

                let c_shell = std::ffi::CString::new(shell.as_str()).expect("CString::new failed");

                // exec the shell. On success this does not return.
                // On failure, expect() panics (which is appropriate for a
                // post-fork child process).
                #[allow(unreachable_code)]
                {
                    unistd::execvp(&c_shell, &[&c_shell]).expect("execvp failed");
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
                libc::TIOCSWINSZ,
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
        let _ = waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG));
    }
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
pub fn start_reader(master_fd: RawFd) -> (JoinHandle<()>, mpsc::UnboundedReceiver<Vec<u8>>) {
    // Set the fd to non-blocking mode, which is required by AsyncFd.
    // SAFETY: The fd is valid (caller guarantees).
    unsafe {
        let flags = libc::fcntl(master_fd, libc::F_GETFL);
        libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let (tx, rx) = mpsc::unbounded_channel();

    let handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];

        let wrapper = RawFdWrapper(master_fd);
        let async_fd = match AsyncFd::with_interest(wrapper, Interest::READABLE) {
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
        let raw_fd = pty.master_fd.as_raw_fd();

        let (_handle, mut rx) = start_reader(raw_fd);

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
}
