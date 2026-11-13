//! ssh's stderr, kept off the user's terminal.
//!
//! The far side writes to this stream for the whole connection: its rc files,
//! a ProxyCommand, ssh's own warnings. The client is in raw mode for most of
//! that time and owns the terminal, so one `ESC [?1000l` there would turn its
//! mouse reporting off. Each line is escaped, bounded and written to the log
//! instead, and the last few are kept for the error a failed dial shows.

use std::collections::VecDeque;
use std::io::Write;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::oneshot;

/// Bytes kept of one line. The rest of the line is read and dropped, so a far
/// side that never writes a newline cannot grow the buffer.
const LINE_BYTES_MAX: usize = 1024;

/// Characters of one line as it is logged and shown, escapes included.
const LINE_CHARS_MAX: usize = 200;

/// Lines logged per connection. `client.log` is never rotated, and a
/// connection can live for days under an rc that prints on every login.
const LOG_LINES_MAX: usize = 100;

/// Lines kept for a failed dial's error.
const TAIL_LINES: usize = 3;

/// Splits a byte stream into escaped, bounded lines.
#[derive(Default)]
struct Lines {
    line: Vec<u8>,
}

impl Lines {
    fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(String)) {
        for &b in bytes {
            if b == b'\n' {
                self.flush(&mut emit);
            } else if self.line.len() < LINE_BYTES_MAX {
                self.line.push(b);
            }
        }
    }

    fn flush(&mut self, mut emit: impl FnMut(String)) {
        let text = String::from_utf8_lossy(&self.line);
        let text = text.trim();
        if !text.is_empty() {
            emit(crate::client::cli_common::escape_bounded(
                text,
                LINE_CHARS_MAX,
            ));
        }
        self.line.clear();
    }
}

/// Lines echoed per connection, for a dial with no deadline (`attach-remote`)
/// whose far side keeps writing while the user is still at a prompt.
const ECHO_LINES_MAX: usize = 300;

/// What one connection's stderr has produced, shared between the drain task
/// and the [`SshStderr`] that owns it.
#[derive(Default)]
struct Record {
    tail: VecDeque<String>,
    echo_stopped: bool,
    echoed: usize,
    connected: bool,
    logged: usize,
    suppressed: usize,
    after_handshake: usize,
    summarised: bool,
}

/// Where one line goes. `echo` is printed to this process's stderr and
/// `log` written to the log, each when present.
#[derive(Debug, Default, PartialEq, Eq)]
struct Routed {
    echo: Option<String>,
    log: Option<String>,
}

impl Record {
    /// Route `line`. `taken` is whether the client has taken the terminal.
    fn take(&mut self, line: String, taken: bool) -> Routed {
        let mut routed = Routed::default();
        if !self.echo_stopped && !taken {
            self.echoed += 1;
            if self.echoed <= ECHO_LINES_MAX {
                routed.echo = Some(line.clone());
            } else {
                routed.echo = Some("remux: further ssh output is in client.log".to_string());
                self.echo_stopped = true;
            }
        }
        // Lines before the handshake say why a dial fails, so each is logged.
        // After it they are the far side's rc and ssh's warnings for as long
        // as the connection lives, and the log is readable by its owner for
        // as long as the file exists, so only a count and the tail are kept.
        if self.connected {
            self.after_handshake += 1;
        } else if self.logged < LOG_LINES_MAX {
            self.logged += 1;
            routed.log = Some(if self.logged == LOG_LINES_MAX {
                format!("{line} (later stderr lines are suppressed)")
            } else {
                line.clone()
            });
        } else {
            self.suppressed += 1;
        }
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
        routed
    }

    fn tail_line(&self) -> String {
        self.tail.iter().cloned().collect::<Vec<_>>().join(" | ")
    }

    /// [`tail_line`](Self::tail_line) in at most `max` characters. The oldest
    /// lines go first, because the newest is the one that says why ssh gave
    /// up and the lines before it are often a warning block. A newest line
    /// that alone is too long keeps its start, which is where ssh names the
    /// failure.
    fn tail_within(&self, max: usize) -> String {
        let mut lines: Vec<&str> = self.tail.iter().map(String::as_str).collect();
        while lines.len() > 1 && lines.join(" | ").chars().count() > max {
            lines.remove(0);
        }
        crate::client::cli_common::escape_bounded(&lines.join(" | "), max)
    }

    /// The line logged when the connection ends, once, and only when some
    /// line was not logged on its own.
    fn summary(&mut self) -> Option<String> {
        if self.summarised || (self.suppressed == 0 && self.after_handshake == 0) {
            return None;
        }
        self.summarised = true;
        Some(format!(
            "stderr closed: {} lines suppressed before the handshake, {} after it; the last: {}",
            self.suppressed,
            self.after_handshake,
            self.tail_line()
        ))
    }
}

fn lock(record: &Mutex<Record>) -> std::sync::MutexGuard<'_, Record> {
    record.lock().unwrap_or_else(|p| p.into_inner())
}

/// The drain of one ssh's stderr. Dropping it stops the drain, which closes
/// the read end of the pipe, and logs the summary if the drain did not.
pub struct SshStderr {
    label: String,
    record: Arc<Mutex<Record>>,
    done: Option<oneshot::Receiver<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl SshStderr {
    /// Start draining `stderr` on its own task, which runs until ssh closes
    /// the stream or this is dropped. It must always read, because ssh blocks
    /// once the pipe is full. Each escaped line also goes to this process's
    /// stderr until [`stop_echo`](Self::stop_echo), the handshake, or the
    /// client taking the terminal, whichever comes first.
    pub fn spawn(label: &str, mut stderr: impl AsyncRead + Unpin + Send + 'static) -> Self {
        let record = Arc::new(Mutex::new(Record::default()));
        let (done_tx, done) = oneshot::channel();
        let label = label.to_string();
        let task_record = Arc::clone(&record);
        let task_label = label.clone();
        let task = tokio::spawn(async move {
            let mut lines = Lines::default();
            let mut chunk = [0u8; 4096];
            let mut emit = |line: String| {
                let taken = crate::client::terminal::terminal_taken();
                let routed = lock(&task_record).take(line, taken);
                if let Some(echo) = routed.echo {
                    let _ = writeln!(std::io::stderr(), "{echo}");
                }
                if let Some(log) = routed.log {
                    log::info!("ssh {task_label}: {log}");
                }
            };
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => lines.feed(&chunk[..n], &mut emit),
                }
            }
            lines.flush(&mut emit);
            if let Some(summary) = lock(&task_record).summary() {
                log::info!("ssh {task_label}: {summary}");
            }
            let _ = done_tx.send(());
        });
        Self {
            label,
            record,
            done: Some(done),
            task,
        }
    }

    pub fn stop_echo(&self) {
        lock(&self.record).echo_stopped = true;
    }

    /// The handshake succeeded: stop echoing, and count later lines rather
    /// than logging each.
    pub fn connected(&self) {
        let mut record = lock(&self.record);
        record.echo_stopped = true;
        record.connected = true;
    }

    /// The last lines ssh wrote, joined on one line in at most `max`
    /// characters, once the stream has closed or `wait` has passed. The bound exists because a process ssh
    /// started (a ProxyCommand, a ControlPersist master) can hold the stream
    /// open after ssh itself exits. Past the bound the drain is stopped, so
    /// that process cannot keep this end of the pipe open either.
    pub async fn tail(&mut self, wait: Duration, max: usize) -> String {
        if let Some(done) = self.done.take() {
            if tokio::time::timeout(wait, done).await.is_err() {
                self.task.abort();
            }
        }
        lock(&self.record).tail_within(max)
    }
}

impl Drop for SshStderr {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(summary) = lock(&self.record).summary() {
            log::info!("ssh {}: {summary}", self.label);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Lines::default();
        let mut out = Vec::new();
        for chunk in chunks {
            lines.feed(chunk, |l| out.push(l));
        }
        lines.flush(|l| out.push(l));
        out
    }

    #[test]
    fn lines_are_split_across_chunks_and_escaped() {
        assert_eq!(
            lines_of(&[
                b"LEAK\x1b[?1000",
                b"l\x1b[?1006l\r\nsec",
                b"ond\n\n  \nlast"
            ]),
            ["LEAK\\x1b[?1000l\\x1b[?1006l", "second", "last"]
        );
    }

    #[test]
    fn a_line_without_a_newline_is_bounded() {
        let long = vec![b'a'; 64 * 1024];
        let out = lines_of(&[&long, b"\nnext\n"]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].chars().count(), LINE_CHARS_MAX);
        assert_eq!(out[1], "next");
    }

    #[test]
    fn invalid_utf8_does_not_stop_the_split() {
        assert_eq!(
            lines_of(&[b"\xff\xfe bad\nok\n"]),
            ["\u{fffd}\u{fffd} bad", "ok"]
        );
    }

    fn routed(record: &mut Record, line: &str, taken: bool) -> Routed {
        record.take(line.to_string(), taken)
    }

    #[test]
    fn lines_before_the_handshake_are_logged_up_to_the_cap_then_counted() {
        let mut record = Record::default();
        for n in 0..LOG_LINES_MAX - 1 {
            assert_eq!(
                routed(&mut record, "x", true).log.as_deref(),
                Some("x"),
                "{n}"
            );
        }
        assert_eq!(
            routed(&mut record, "x", true).log.as_deref(),
            Some("x (later stderr lines are suppressed)")
        );
        assert_eq!(routed(&mut record, "y", true).log, None);
        assert_eq!(record.suppressed, 1);
    }

    #[test]
    fn lines_after_the_handshake_are_counted_and_summarised_once() {
        let mut record = Record::default();
        assert_eq!(record.summary(), None, "nothing went unlogged");
        routed(&mut record, "before", true);
        record.connected = true;
        assert_eq!(routed(&mut record, "after one", true), Routed::default());
        routed(&mut record, "after two", true);
        assert_eq!(
            record.summary().as_deref(),
            Some(
                "stderr closed: 0 lines suppressed before the handshake, 2 after it; \
                 the last: before | after one | after two"
            )
        );
        assert_eq!(record.summary(), None, "the drain and Drop both ask");
    }

    #[test]
    fn a_bounded_tail_drops_the_oldest_lines_and_keeps_the_newest() {
        let mut record = Record::default();
        let warning = "** WARNING: connection is not using a post-quantum key exchange algorithm.";
        routed(&mut record, warning, true);
        routed(
            &mut record,
            "** This session may be vulnerable to \"store now, decrypt later\" attacks.",
            true,
        );
        routed(
            &mut record,
            "user@host: Permission denied (publickey).",
            true,
        );
        assert_eq!(
            record.tail_within(150),
            "** This session may be vulnerable to \"store now, decrypt later\" attacks. | \
             user@host: Permission denied (publickey)."
        );
        assert_eq!(
            record.tail_within(60),
            "user@host: Permission denied (publickey)."
        );
        assert_eq!(record.tail_within(20), "user@host: Permis...");
        assert_eq!(record.tail_within(1000), record.tail_line());
    }

    #[test]
    fn the_echo_is_capped_and_stops_once_the_terminal_is_taken() {
        let mut record = Record::default();
        for _ in 0..ECHO_LINES_MAX {
            assert_eq!(routed(&mut record, "x", false).echo.as_deref(), Some("x"));
        }
        assert_eq!(
            routed(&mut record, "x", false).echo.as_deref(),
            Some("remux: further ssh output is in client.log")
        );
        assert_eq!(routed(&mut record, "x", false).echo, None);

        let mut record = Record::default();
        assert_eq!(routed(&mut record, "x", true).echo, None);
        record.connected = true;
        record.echo_stopped = true;
        assert_eq!(routed(&mut record, "x", false).echo, None);
    }

    #[tokio::test]
    async fn the_tail_keeps_the_last_lines_after_the_stream_closes() {
        let input: &'static [u8] =
            b"one\ntwo\nPermission denied (publickey).\nssh: \x1b]0;x\x07end\n";
        let mut drain = SshStderr::spawn("t", input);
        assert_eq!(
            drain.tail(Duration::from_secs(5), 1000).await,
            "two | Permission denied (publickey). | ssh: \\x1b]0;x\\x07end"
        );
    }

    #[tokio::test]
    async fn the_tail_does_not_wait_past_its_bound_on_a_stream_held_open() {
        let (mut held, read) = tokio::io::duplex(64);
        tokio::io::AsyncWriteExt::write_all(&mut held, b"held open\n")
            .await
            .unwrap();
        let mut drain = SshStderr::spawn("t", read);
        let started = std::time::Instant::now();
        let tail = drain.tail(Duration::from_millis(200), 1000).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(tail, "held open");
        drop(held);
    }
}
