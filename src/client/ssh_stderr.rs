//! ssh's stderr, kept off the user's terminal.
//!
//! The far side writes to this stream for the whole connection: its rc files,
//! a ProxyCommand, ssh's own warnings. The client is in raw mode for most of
//! that time and owns the terminal, so one `ESC [?1000l` there would turn its
//! mouse reporting off. Each line is escaped, bounded and written to the log
//! instead, and the last few are kept for the error a failed dial shows.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Counts what has been logged, so that the log takes [`LOG_LINES_MAX`] lines
/// of one connection and a count of the rest.
#[derive(Default)]
struct Budget {
    logged: usize,
    suppressed: usize,
}

enum Verdict {
    Log,
    LogLast,
    Drop,
}

impl Budget {
    fn take(&mut self) -> Verdict {
        if self.logged < LOG_LINES_MAX {
            self.logged += 1;
            if self.logged == LOG_LINES_MAX {
                Verdict::LogLast
            } else {
                Verdict::Log
            }
        } else {
            self.suppressed += 1;
            Verdict::Drop
        }
    }
}

/// The drain of one ssh's stderr.
pub struct SshStderr {
    tail: Arc<Mutex<VecDeque<String>>>,
    echo: Arc<AtomicBool>,
    done: Option<oneshot::Receiver<()>>,
}

impl SshStderr {
    /// Start draining `stderr` on its own task, which runs until ssh closes
    /// the stream. It must always read, because ssh blocks once the pipe is
    /// full. Each escaped line also goes to this process's stderr until
    /// [`stop_echo`](Self::stop_echo), or until the client takes the terminal,
    /// whichever comes first.
    pub fn spawn(label: &str, mut stderr: impl AsyncRead + Unpin + Send + 'static) -> Self {
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let echo = Arc::new(AtomicBool::new(true));
        let (done_tx, done) = oneshot::channel();
        let label = label.to_string();
        let task_tail = Arc::clone(&tail);
        let task_echo = Arc::clone(&echo);
        tokio::spawn(async move {
            let mut lines = Lines::default();
            let mut budget = Budget::default();
            let mut chunk = [0u8; 4096];
            let mut emit = |line: String| {
                if task_echo.load(Ordering::Relaxed) && !crate::client::terminal::terminal_taken() {
                    let _ = writeln!(std::io::stderr(), "{line}");
                }
                match budget.take() {
                    Verdict::Log => log::info!("ssh {label}: {line}"),
                    Verdict::LogLast => {
                        log::info!("ssh {label}: {line} (later stderr lines are suppressed)")
                    }
                    Verdict::Drop => {}
                }
                let mut tail = task_tail.lock().unwrap_or_else(|p| p.into_inner());
                if tail.len() == TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            };
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => lines.feed(&chunk[..n], &mut emit),
                }
            }
            lines.flush(&mut emit);
            if budget.suppressed > 0 {
                // The last lines are the ones that say why the connection
                // ended, and a chatty login can use up the budget before them.
                let tail = task_tail.lock().unwrap_or_else(|p| p.into_inner());
                let last = tail.iter().cloned().collect::<Vec<_>>().join(" | ");
                log::info!(
                    "ssh {label}: {} further stderr lines suppressed; the last: {last}",
                    budget.suppressed
                );
            }
            let _ = done_tx.send(());
        });
        Self {
            tail,
            echo,
            done: Some(done),
        }
    }

    pub fn stop_echo(&self) {
        self.echo.store(false, Ordering::Relaxed);
    }

    /// The last lines ssh wrote, joined on one line, once the stream has
    /// closed or `wait` has passed. The bound exists because a process ssh
    /// started (a ProxyCommand, a ControlPersist master) can hold the stream
    /// open after ssh itself exits.
    pub async fn tail(&mut self, wait: Duration) -> String {
        if let Some(done) = self.done.take() {
            let _ = tokio::time::timeout(wait, done).await;
        }
        let tail = self.tail.lock().unwrap_or_else(|p| p.into_inner());
        tail.iter().cloned().collect::<Vec<_>>().join(" | ")
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

    #[test]
    fn the_budget_logs_up_to_its_cap_and_counts_the_rest() {
        let mut budget = Budget::default();
        let verdicts: Vec<_> = (0..LOG_LINES_MAX + 5).map(|_| budget.take()).collect();
        assert!(verdicts[..LOG_LINES_MAX - 1]
            .iter()
            .all(|v| matches!(v, Verdict::Log)));
        assert!(matches!(verdicts[LOG_LINES_MAX - 1], Verdict::LogLast));
        assert!(verdicts[LOG_LINES_MAX..]
            .iter()
            .all(|v| matches!(v, Verdict::Drop)));
        assert_eq!(budget.suppressed, 5);
    }

    #[tokio::test]
    async fn the_tail_keeps_the_last_lines_after_the_stream_closes() {
        let input: &'static [u8] =
            b"one\ntwo\nPermission denied (publickey).\nssh: \x1b]0;x\x07end\n";
        let mut drain = SshStderr::spawn("t", input);
        assert_eq!(
            drain.tail(Duration::from_secs(5)).await,
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
        let tail = drain.tail(Duration::from_millis(200)).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(tail, "held open");
        drop(held);
    }
}
