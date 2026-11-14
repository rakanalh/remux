//! The role's log file, rotated by size while the process runs.
//!
//! `main.rs` pins the logger at `Debug`, so the log grows for as long as the
//! process lives, and the server lives for days. [`RotatingLog`] moves the
//! file to `.1` (and `.1` to `.2`, up to [`KEEP`]) when it reaches its limit.
//!
//! Several processes append to one file: every client writes `client.log`.
//! Each one compares the inode it has open with the inode at the path before
//! every record, so a rotation done by one process makes the others reopen.
//! The rename chain is done under a non-blocking `flock` on the full file, so
//! two processes cannot both shift the chain for one full file.
//!
//! The writer runs under env_logger's mutex, which env_logger takes with
//! `expect`. A panic here would poison it and make every later log call
//! panic, and a `log::` call here would lock it again on the same thread. So
//! nothing here panics or logs; a failure is written into the log file itself.

use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The size at which the log is rotated.
pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Rotated files kept beside the live one: `.1` to `.3`.
pub const KEEP: usize = 3;

/// Overrides [`ROTATE_BYTES`], so that a harness can force a rotation with a
/// few kilobytes of log. It is a test hook, not a setting: panes inherit the
/// server's environment, so every remux process started inside them would
/// rotate at the harness's limit too.
const ROTATE_BYTES_ENV: &str = "REMUX_TEST_LOG_ROTATE_BYTES";

/// How long a failed rename or reopen waits before the next attempt. Without
/// it a log on a read-only directory would retry, and report, on every record.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// The rotation limit: [`ROTATE_BYTES`] unless [`ROTATE_BYTES_ENV`] holds a
/// positive number.
pub fn rotate_bytes() -> u64 {
    std::env::var(ROTATE_BYTES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(ROTATE_BYTES)
}

/// The log file at `path`, created with its directory, readable by its owner
/// alone. It holds what the far side of an ssh connection wrote, which can be
/// a sign-in URL from a ProxyCommand. `mode` applies only to a file this call
/// creates, so an existing directory and file are narrowed as well. A
/// failure to narrow either is ignored, because no log at all costs more than
/// a log in the old mode; [`warn_if_log_is_shared`] reports it.
pub fn open_log(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    Ok(file)
}

/// Warn, in the log itself, when the log at `path` can still be read by
/// other users because [`open_log`] could not narrow it.
pub fn warn_if_log_is_shared(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            log::warn!(
                "log: {} is mode {mode:o}, readable beyond its owner; it holds ssh output",
                path.display()
            );
        }
    }
}

/// An append-only log file that rotates itself at `limit` bytes.
pub struct RotatingLog {
    path: PathBuf,
    file: File,
    limit: u64,
    keep: usize,
    retry_at: Option<Instant>,
    first: bool,
}

impl RotatingLog {
    pub fn open(path: &Path, limit: u64, keep: usize) -> io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            file: open_log(path)?,
            limit,
            keep,
            retry_at: None,
            first: true,
        })
    }

    /// Make the open file the one at the path, and rotate it when it is full.
    /// Called before each record. A record is never split across files
    /// because env_logger's `Pipe` target hands each record to one `write_all`,
    /// which [`Write::write`] below passes on whole.
    fn prepare(&mut self) {
        let first = std::mem::replace(&mut self.first, false);
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        self.retry_at = None;
        let Ok(ours) = self.file.metadata() else {
            return;
        };
        if !self.path_is(&ours) {
            self.reopen();
            return;
        }
        if ours.len() >= self.limit {
            self.rotate(first);
        }
    }

    /// Whether the path still names the file that is open. It does not after
    /// another process rotated the log, or after the user deleted it.
    fn path_is(&self, ours: &Metadata) -> bool {
        std::fs::metadata(&self.path)
            .is_ok_and(|at_path| at_path.dev() == ours.dev() && at_path.ino() == ours.ino())
    }

    /// `at_startup` is true only for the first record this process writes.
    fn rotate(&mut self, at_startup: bool) {
        if !try_lock(&self.file) {
            // Another process is rotating this file. Its rename makes
            // `path_is` false on a later record, which reopens.
            return;
        }
        // The other process can finish its rotation between our size check
        // and our lock. The check is repeated under the lock so that the
        // chain is not shifted a second time for one full file.
        let ours = match self.file.metadata() {
            Ok(m) if self.path_is(&m) && m.len() >= self.limit => m,
            _ => {
                unlock(&self.file);
                self.reopen();
                return;
            }
        };
        // A file this far past the limit when this process first writes was
        // not written under rotation, so it predates it. Kept as `.1` it would
        // stay on disk through `KEEP` more rotations, so it is deleted instead.
        // Later in the run the same size can be another writer's log that grew
        // during a backoff or under a larger limit, and that log is kept.
        let oversized = at_startup && ours.len() > self.limit.saturating_mul(2);
        let moved = if oversized {
            std::fs::remove_file(&self.path)
        } else {
            shift(&self.path, self.keep)
        };
        unlock(&self.file);
        match moved {
            Ok(()) => {
                self.reopen();
                if oversized {
                    self.note(&format!(
                        "dropped the previous log at {} bytes, over twice the rotation limit",
                        ours.len()
                    ));
                }
            }
            Err(e) => self.fail(&format!("cannot rotate {}: {e}", self.path.display())),
        }
    }

    /// On failure the old file stays open and takes the records, so the
    /// process keeps logging.
    fn reopen(&mut self) {
        match open_log(&self.path) {
            Ok(file) => self.file = file,
            Err(e) => self.fail(&format!("cannot reopen {}: {e}", self.path.display())),
        }
    }

    fn fail(&mut self, what: &str) {
        self.note(&format!(
            "{what}; logging to the open file, next attempt in {}s",
            RETRY_AFTER.as_secs()
        ));
        self.retry_at = Some(Instant::now() + RETRY_AFTER);
    }

    fn note(&mut self, what: &str) {
        let _ = writeln!(self.file, "remux log: {what}");
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.prepare();
        self.file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn numbered(path: &Path, n: usize) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Move `path` to `path.1`, `path.1` to `path.2`, and so on, overwriting
/// `path.<keep>`. With `keep == 0` the file is deleted.
fn shift(path: &Path, keep: usize) -> io::Result<()> {
    if keep == 0 {
        return std::fs::remove_file(path);
    }
    for n in (1..keep).rev() {
        match std::fs::rename(numbered(path, n), numbered(path, n + 1)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    std::fs::rename(path, numbered(path, 1))
}

/// False only when another process holds the lock. A filesystem that does not
/// support `flock` (some network mounts) answers with another error, and is
/// rotated unlocked: refusing there would leave the log to grow without bound.
fn try_lock(file: &File) -> bool {
    // SAFETY: `flock` takes a descriptor and flags and touches no memory; the
    // descriptor is valid for as long as `file` is borrowed.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 };
    locked || io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock
}

fn unlock(file: &File) {
    // SAFETY: as in `try_lock`.
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const LINE: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef012345678901234\n";

    fn write_lines(log: &mut RotatingLog, n: usize) {
        for _ in 0..n {
            log.write_all(LINE).unwrap();
            log.flush().unwrap();
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn len(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    #[test]
    fn rotates_at_the_limit_and_keeps_only_keep_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remux").join("server.log");
        let mut log = RotatingLog::open(&path, 1024, 3).unwrap();
        write_lines(&mut log, 200);

        for n in 1..=3 {
            let rotated = numbered(&path, n);
            assert!(rotated.exists(), "{} missing", rotated.display());
            assert_eq!(mode(&rotated), 0o600);
            assert!(len(&rotated) <= 1024 + LINE.len() as u64);
        }
        assert!(!numbered(&path, 4).exists());
        assert_eq!(mode(&path), 0o600);
        assert!(len(&path) <= 1024 + LINE.len() as u64);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            4
        );
    }

    #[test]
    fn a_record_is_never_split_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.log");
        let mut log = RotatingLog::open(&path, 1000, 2).unwrap();
        write_lines(&mut log, 40);
        for p in [path.clone(), numbered(&path, 1), numbered(&path, 2)] {
            let text = std::fs::read(&p).unwrap();
            assert_eq!(text.len() % LINE.len(), 0, "{}", p.display());
            assert!(text.chunks(LINE.len()).all(|c| c == LINE));
        }
    }

    #[test]
    fn logging_continues_into_the_new_file_after_a_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.log");
        let mut log = RotatingLog::open(&path, 512, 3).unwrap();
        write_lines(&mut log, 10);
        assert!(numbered(&path, 1).exists());
        log.write_all(b"AFTER\n").unwrap();
        let live = std::fs::read_to_string(&path).unwrap();
        assert!(live.ends_with("AFTER\n"));
        assert!(!std::fs::read_to_string(numbered(&path, 1))
            .unwrap()
            .contains("AFTER"));
    }

    #[test]
    fn a_second_writer_follows_a_rotation_made_by_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.log");
        let mut a = RotatingLog::open(&path, 512, 3).unwrap();
        let mut b = RotatingLog::open(&path, 512, 3).unwrap();
        write_lines(&mut a, 9);
        assert!(numbered(&path, 1).exists());
        b.write_all(b"FROM-B\n").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("FROM-B"));
        assert!(!std::fs::read_to_string(numbered(&path, 1))
            .unwrap()
            .contains("FROM-B"));
        assert!(!numbered(&path, 2).exists(), "the chain was shifted twice");
    }

    #[test]
    fn a_writer_that_lost_the_lock_does_not_shift_the_chain_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.log");
        let mut a = RotatingLog::open(&path, 512, 3).unwrap();
        let mut b = RotatingLog::open(&path, 512, 3).unwrap();
        write_lines(&mut a, 8);
        assert!(!numbered(&path, 1).exists());
        // `a` holds the lock, as it would mid-rotation in another process.
        assert!(try_lock(&a.file));
        write_lines(&mut b, 1);
        assert!(!numbered(&path, 1).exists(), "rotated without the lock");
        unlock(&a.file);
        write_lines(&mut b, 1);
        write_lines(&mut a, 1);
        assert!(numbered(&path, 1).exists());
        assert!(!numbered(&path, 2).exists(), "the chain was shifted twice");
    }

    #[test]
    fn a_writer_that_checked_before_another_rotated_does_not_shift_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.log");
        let mut a = RotatingLog::open(&path, 512, 3).unwrap();
        let mut b = RotatingLog::open(&path, 512, 3).unwrap();
        write_lines(&mut a, 9);
        assert!(numbered(&path, 1).exists());
        // `b` saw the shared file full before `a` rotated it, and reaches the
        // lock only now.
        b.rotate(false);
        assert!(!numbered(&path, 2).exists(), "the chain was shifted twice");
        b.write_all(b"FROM-B\n").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("FROM-B"));
    }

    #[test]
    fn an_oversized_log_later_in_the_run_is_rotated_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.log");
        let mut log = RotatingLog::open(&path, 1024, 3).unwrap();
        log.write_all(b"FIRST\n").unwrap();
        let mut other = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        other.write_all(&vec![b'x'; 10_000]).unwrap();
        log.write_all(b"NEXT\n").unwrap();
        let rotated = std::fs::read_to_string(numbered(&path, 1)).unwrap();
        assert!(rotated.starts_with("FIRST\n"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "NEXT\n");
    }

    #[test]
    fn an_oversized_log_from_before_rotation_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.log");
        std::fs::write(&path, vec![b'x'; 10_000]).unwrap();
        std::fs::write(numbered(&path, 1), b"older\n").unwrap();
        let mut log = RotatingLog::open(&path, 1024, 3).unwrap();
        log.write_all(b"FIRST\n").unwrap();
        let live = std::fs::read_to_string(&path).unwrap();
        assert!(live.contains("dropped the previous log at 10000 bytes"));
        assert!(live.ends_with("FIRST\n"));
        assert_eq!(
            std::fs::read_to_string(numbered(&path, 1)).unwrap(),
            "older\n"
        );
        assert!(!numbered(&path, 2).exists());
    }

    #[test]
    fn a_log_just_over_the_limit_at_startup_is_rotated_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.log");
        std::fs::write(&path, vec![b'x'; 1500]).unwrap();
        let mut log = RotatingLog::open(&path, 1024, 3).unwrap();
        log.write_all(b"FIRST\n").unwrap();
        assert_eq!(len(&numbered(&path, 1)), 1500);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "FIRST\n");
    }

    #[test]
    fn a_failed_rename_keeps_logging_and_backs_off() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("remux");
        let path = logs.join("server.log");
        let mut log = RotatingLog::open(&path, 512, 3).unwrap();
        std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o500)).unwrap();
        write_lines(&mut log, 20);
        std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o700)).unwrap();

        let live = std::fs::read_to_string(&path).unwrap();
        assert_eq!(live.matches("cannot rotate").count(), 1, "{live}");
        assert_eq!(
            live.lines()
                .filter(|l| l.as_bytes() == &LINE[..LINE.len() - 1])
                .count(),
            20
        );
        assert!(!numbered(&path, 1).exists());
    }

    #[test]
    fn a_deleted_log_is_recreated_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.log");
        let mut log = RotatingLog::open(&path, 1 << 20, 3).unwrap();
        std::fs::remove_file(&path).unwrap();
        log.write_all(b"BACK\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "BACK\n");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn keep_zero_deletes_instead_of_keeping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.log");
        let mut log = RotatingLog::open(&path, 256, 0).unwrap();
        write_lines(&mut log, 10);
        assert!(!numbered(&path, 1).exists());
        assert!(len(&path) <= 256 + LINE.len() as u64);
    }
}
