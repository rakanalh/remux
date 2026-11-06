//! Plumbing shared by the one-shot CLI verbs (`remux msg`, `remux agent`,
//! `remux kill`): send, wait for one answer, a deadline on each, a fixed exit
//! code, and nothing printed that the server or a message chose without
//! escaping it.
//!
//! These verbs are run by agents and their hooks as much as by people, so
//! they never start a server (a message to a server that was not running has
//! no target) and never wait without a bound.

use std::time::Duration;

use crate::client::terminal::RemuxClient;
pub(crate) use crate::escape::{escape, escape_bounded, escape_keeping_tabs, escape_lines};
use crate::protocol::{is_invisible, ClientMessage, PaneId, ServerMessage};
use crate::server::daemon::socket_path;

/// How long a verb may take to reach the server.
pub(crate) const CONNECT_DEADLINE: Duration = Duration::from_secs(1);

/// How long a verb waits for its reply once sent, beyond any `--wait`. Above
/// the server's own 5 s bound on checking the caller, so a slow check is
/// reported as the server's refusal rather than as this deadline.
pub(crate) const REPLY_DEADLINE: Duration = Duration::from_secs(10);

/// What a request about another server waits on top of its own deadline:
/// the server's whole forward budget, so the server's own answer,
/// `forward_unknown` included, arrives before the CLI gives up.
pub(crate) fn forward_deadline(to: &crate::protocol::Address) -> Duration {
    if to.host.is_some() {
        Duration::from_millis(crate::server::daemon::FORWARD_BUDGET_MS)
    } else {
        Duration::ZERO
    }
}

#[derive(Debug)]
pub(crate) enum CliError {
    NoServer,
    Timeout,
    /// A send the server did not answer in time. It may have been stored, and
    /// typed, so the message says how to check before sending it again.
    SendTimeout(String),
    Refused {
        code: String,
        message: String,
    },
    Transport(String),
    /// A malformed invocation, refused before anything is sent. `code` is
    /// `bad_address`, `bad_body` or `usage`.
    Usage {
        code: &'static str,
        message: String,
    },
}

impl CliError {
    pub(crate) fn usage(code: &'static str, message: impl Into<String>) -> Self {
        CliError::Usage {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            CliError::Usage { .. } => 2,
            _ => 1,
        }
    }

    fn code(&self) -> &str {
        match self {
            CliError::NoServer => "no_server",
            CliError::Timeout | CliError::SendTimeout(_) => "timeout",
            CliError::Refused { code, .. } => code,
            CliError::Transport(_) => "transport",
            CliError::Usage { code, .. } => code,
        }
    }

    fn message(&self) -> &str {
        match self {
            CliError::NoServer => "no remux server is running here",
            CliError::Timeout => "the server did not answer in time",
            CliError::Refused { message, .. }
            | CliError::SendTimeout(message)
            | CliError::Transport(message)
            | CliError::Usage { message, .. } => message,
        }
    }

    /// The stderr line. Every part is escaped: the code and message can come
    /// from a server, and a usage message can quote what was typed.
    pub(crate) fn stderr(&self) -> String {
        format!("remux: {}: {}", escape(self.code()), escape(self.message()))
    }

    pub(crate) fn json(&self) -> String {
        json_line(&serde_json::json!({
            "error": { "code": self.code(), "message": self.message() }
        }))
    }
}

/// Run one verb: restore the default SIGPIPE, then report the result with
/// [`finish`]. Rust ignores SIGPIPE, so without this `remux msg read | head`
/// panics on the first write after `head` exits instead of ending quietly the
/// way a Unix tool does.
pub(crate) async fn run_verb(
    verb: impl std::future::Future<Output = Result<(), CliError>>,
    json: bool,
) -> u8 {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    finish(verb.await, json)
}

/// Report `result` and return the exit code for it. Under `json` a failure is
/// also printed on stdout as `{"error": {...}}`, so a caller parsing stdout
/// always gets a document.
pub(crate) fn finish(result: Result<(), CliError>, json: bool) -> u8 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            if json {
                println!("{}", e.json());
            }
            eprintln!("{}", e.stderr());
            e.exit_code()
        }
    }
}

/// Connect, send `msg`, and wait up to `deadline` for its answer.
pub(crate) async fn ask(msg: ClientMessage, deadline: Duration) -> Result<ServerMessage, CliError> {
    ask_in_order(vec![msg], deadline).await
}

/// Connect, send `msgs` in order, and wait up to `deadline` for the first
/// answer to any of them.
async fn ask_in_order(
    msgs: Vec<ClientMessage>,
    deadline: Duration,
) -> Result<ServerMessage, CliError> {
    if !socket_path().exists() {
        return Err(CliError::NoServer);
    }
    let mut client = tokio::time::timeout(CONNECT_DEADLINE, RemuxClient::connect_local())
        .await
        .map_err(|_| CliError::Timeout)?
        .map_err(|e| {
            if no_listener(&e) {
                CliError::NoServer
            } else {
                CliError::Transport(format!("{e:#}"))
            }
        })?;
    for msg in msgs {
        client
            .send(msg)
            .await
            .map_err(|e| CliError::Transport(format!("{e:#}")))?;
    }
    // One deadline for the whole wait, so a stream of pushes cannot extend it.
    let answer = tokio::time::timeout(deadline, async {
        loop {
            match client.recv().await {
                Ok(Some(push)) if push.is_push() => {
                    log::debug!("cmd: skipping a {} push", push.variant_name());
                }
                other => return other,
            }
        }
    })
    .await
    .map_err(|_| CliError::Timeout)?
    .map_err(|e| CliError::Transport(format!("{e:#}")))?;
    match answer {
        None => Err(CliError::Transport(
            "the server closed the connection".to_string(),
        )),
        Some(ServerMessage::Error { message, code, .. }) => Err(CliError::Refused {
            code: code.unwrap_or_else(|| "error".to_string()),
            message,
        }),
        Some(answer) => Ok(answer),
    }
}

/// `remux kill`: the exit code of killing session `name`.
pub(crate) async fn run_kill(name: &str) -> u8 {
    run_verb(kill(name), false).await
}

/// A successful `KillSession` has no answer, because the server tells only the
/// clients attached to the session, and this connection is never attached. The
/// server handles a connection's messages one at a time and in order, so the
/// `ListSessions` behind the kill is answered once the kill is done, and a
/// refused kill answers first with its `Error`.
async fn kill(name: &str) -> Result<(), CliError> {
    let kill = ClientMessage::KillSession {
        name: name.to_string(),
    };
    match ask_in_order(vec![kill, ClientMessage::ListSessions], REPLY_DEADLINE).await? {
        ServerMessage::SessionList { sessions } if sessions.iter().any(|s| s.name == name) => {
            Err(CliError::Refused {
                code: "not_killed".to_string(),
                message: format!("the server still lists session '{name}'"),
            })
        }
        ServerMessage::SessionList { .. } => {
            println!("Killed session '{}'.", escape(name));
            Ok(())
        }
        other => Err(unexpected(&other)),
    }
}

/// Whether a connect failed because nothing listens on the socket: a socket
/// file left behind by a server that died, or one removed after the
/// existence check.
fn no_listener(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        })
}

/// An answer this verb does not expect, named by its variant only.
pub(crate) fn unexpected(answer: &ServerMessage) -> CliError {
    let name = answer.variant_name();
    log::warn!("cmd: got an unexpected {name} answer");
    CliError::Transport(format!("unexpected {name} answer from the server"))
}

/// `$REMUX_PANE`, sent as a hint the server only logs. `None` if unset or
/// not a pane id.
pub(crate) fn pane_hint() -> Option<PaneId> {
    parse_pane(&std::env::var("REMUX_PANE").ok()?).ok()
}

/// A `--pane` argument: an agent's name, or a pane id (`%N`, or bare digits),
/// which is accepted but documented nowhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneArg {
    Id(PaneId),
    Name(String),
}

pub(crate) fn parse_pane_arg(s: &str) -> Result<PaneArg, String> {
    if s.starts_with('%') || s.bytes().all(|b| b.is_ascii_digit()) {
        return parse_pane(s).map(PaneArg::Id);
    }
    if crate::protocol::valid_name(s) {
        Ok(PaneArg::Name(s.to_string()))
    } else {
        Err(format!("{} is not an agent's name", escape(s)))
    }
}

/// The pane a `--pane` argument names. A name is looked up with an
/// `AgentStatus` for it, since the requests that take a pane carry its id.
pub(crate) async fn resolve_pane_arg(arg: Option<PaneArg>) -> Result<Option<PaneId>, CliError> {
    let name = match arg {
        None => return Ok(None),
        Some(PaneArg::Id(pane)) => return Ok(Some(pane)),
        Some(PaneArg::Name(name)) => name,
    };
    let missing = format!("no pane named {name}");
    let target = crate::protocol::Address {
        host: None,
        target: crate::protocol::Target::Name(name),
    };
    match ask(
        ClientMessage::AgentStatus {
            target: Some(target),
            req: None,
        },
        REPLY_DEADLINE,
    )
    .await?
    {
        ServerMessage::AgentStatusList { panes, .. } => match panes.first() {
            Some(entry) => Ok(Some(entry.pane_id)),
            None => Err(CliError::Refused {
                code: "no_such_target".into(),
                message: missing,
            }),
        },
        other => Err(unexpected(&other)),
    }
}

pub(crate) fn parse_pane(s: &str) -> Result<PaneId, String> {
    s.strip_prefix('%')
        .unwrap_or(s)
        .parse()
        .map_err(|_| format!("{s:?} is not a pane"))
}

/// `30`, `30s`, `500ms`, `2m`, `1h`; a bare number is seconds.
pub(crate) fn parse_wait(s: &str) -> Result<Duration, String> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, unit) = s.split_at(split);
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a duration: use 30, 30s, 2m"))?;
    let ms = match unit {
        "" | "s" => n.checked_mul(1_000),
        "ms" => Some(n),
        "m" => n.checked_mul(60_000),
        "h" => n.checked_mul(3_600_000),
        _ => None,
    };
    ms.map(Duration::from_millis)
        .ok_or_else(|| format!("{s:?} is not a duration: use 30, 30s, 2m"))
}

/// One JSON document, with DEL, the C1 range and every invisible character
/// escaped as well: serde_json leaves them raw, a terminal can read the C1
/// bytes as control sequences, and an invisible character reorders or hides
/// the text around it. Every one is escaped, allowed or not, because JSON is
/// read by programs and the escape decodes to the same text.
pub(crate) fn json_line(value: &serde_json::Value) -> String {
    escaped_json(&value.to_string())
}

/// [`json_line`]'s escaping, for JSON text already rendered, pretty or not:
/// DEL, C1 and invisible characters can only stand inside its strings, where
/// a `\u` escape means the same.
pub(crate) fn escaped_json(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        let code = c as u32;
        if (0x7f..=0x9f).contains(&code) || is_invisible(c) {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_is_seconds_unless_it_says_otherwise() {
        assert_eq!(parse_wait("30"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_wait("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_wait("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_wait("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_wait("1h"), Ok(Duration::from_secs(3600)));
        for bad in ["", "soon", "5d", "-1", "1.5s", "99999999999999999999h"] {
            assert!(parse_wait(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_pane_is_written_with_or_without_its_sign() {
        assert_eq!(parse_pane("%7"), Ok(7));
        assert_eq!(parse_pane("7"), Ok(7));
        assert!(parse_pane("%x").is_err());
    }

    #[test]
    fn every_control_and_format_character_is_shown_not_emitted() {
        assert_eq!(escape("a\u{1b}[2Jb"), "a\\x1b[2Jb");
        assert_eq!(escape("bell\u{7}"), "bell\\x07");
        assert_eq!(escape("del\u{7f}"), "del\\x7f");
        assert_eq!(escape("c1\u{9b}2J"), "c1\\u{9b}2J");
        assert_eq!(escape("two\nlines"), "two\\x0alines");
        assert_eq!(escape("rlo\u{202e}txt"), "rlo\\u{202e}txt");
        assert_eq!(escape("tag\u{e0041}"), "tag\\u{e0041}");
        assert_eq!(escape("zw\u{200b}sp"), "zw\\u{200b}sp");
        assert_eq!(
            escape("plain é ✓ \u{1F469}\u{200D}\u{1F4BB} \u{2764}\u{FE0F}"),
            "plain é ✓ \u{1F469}\u{200D}\u{1F4BB} \u{2764}\u{FE0F}"
        );
        assert_eq!(escape("a\u{200D}\u{200D}b"), "a\\u{200d}\\u{200d}b");
        assert_eq!(escape("\u{3164}x"), "\\u{3164}x");
    }

    #[test]
    fn a_tab_is_kept_and_judged_in_place() {
        assert_eq!(escape_keeping_tabs("a\tb\u{1b}"), "a\tb\\x1b");
        assert_eq!(escape("a\tb"), "a\\x09b");
        // Beside a tab a joiner attaches to nothing, as the server sees it.
        assert_eq!(escape_keeping_tabs("a\u{200D}\tb"), "a\\u{200d}\tb");
    }

    #[test]
    fn json_escapes_what_serde_leaves_raw_and_still_round_trips() {
        let body = "esc\u{1b} c1\u{9b} del\u{7f} rlo\u{202e} tag\u{e0041} ls\u{2028}";
        let line = json_line(&serde_json::json!({ "body": body }));
        assert!(
            line.chars().all(|c| !c.is_control() && !is_invisible(c)),
            "{line:?}"
        );
        let back: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["body"], body);
    }

    #[test]
    fn a_malformed_invocation_exits_2_and_a_refusal_1() {
        assert_eq!(CliError::usage("bad_body", "x").exit_code(), 2);
        for e in [
            CliError::NoServer,
            CliError::Timeout,
            CliError::Refused {
                code: "no_such_target".into(),
                message: "x".into(),
            },
            CliError::Transport("x".into()),
        ] {
            assert_eq!(e.exit_code(), 1, "{e:?}");
        }
        assert_eq!(
            CliError::NoServer.stderr(),
            "remux: no_server: no remux server is running here"
        );
    }

    #[test]
    fn every_part_of_the_stderr_line_is_escaped() {
        let e = CliError::Refused {
            code: "x\u{1b}]52;c;aGk=\u{7}".into(),
            message: "m\u{9b}2J\nFrom: boss".into(),
        };
        assert_eq!(
            e.stderr(),
            "remux: x\\x1b]52;c;aGk=\\x07: m\\u{9b}2J\\x0aFrom: boss"
        );
        let e = CliError::Transport("t\u{1b}[2J".into());
        assert_eq!(e.stderr(), "remux: transport: t\\x1b[2J");
        let e = CliError::usage("usage", "u\u{202e}");
        assert_eq!(e.stderr(), "remux: usage: u\\u{202e}");
    }

    #[test]
    fn escape_lines_keeps_the_line_breaks_and_escapes_the_rest() {
        assert_eq!(
            escape_lines("error: '\u{202e}\u{9b}' is bad\n\nUsage: x\n"),
            "error: '\\u{202e}\\u{9b}' is bad\n\nUsage: x\n"
        );
    }

    #[test]
    fn escape_bounded_keeps_each_escape_whole_within_the_bound() {
        assert_eq!(escape_bounded("ab", 2), "ab");
        assert_eq!(escape_bounded("a\u{1b}\u{1b}", 9), "a\\x1b\\x1b");
        assert_eq!(escape_bounded("éééééé", 5), "éé...");
        assert_eq!(
            escape_bounded("a\u{1b}\u{1b}", 8),
            "a\\x1b...",
            "the second escape does not fit whole, so none of it is kept"
        );
        assert_eq!(escape_bounded("a\u{1b}\u{1b}\u{1b}", 10), "a\\x1b...");
    }

    #[test]
    fn escape_bounded_reads_only_the_head_of_a_huge_input() {
        for (s, max, out) in [
            ("abcdefg", 4, "a..."),
            ("abcdefgh", 4, "a..."),
            ("abcdefghi", 4, "a..."),
            ("abcd", 4, "abcd"),
            ("\u{1b}bcdefghijk", 8, "\\x1bb..."),
            (
                "👨\u{200d}👩\u{200d}👧 family",
                40,
                "👨\u{200d}👩\u{200d}👧 family",
            ),
        ] {
            assert_eq!(escape_bounded(s, max), out, "{s:?} at {max}");
        }
        let huge = "a".repeat(64 << 20);
        let started = std::time::Instant::now();
        let out = escape_bounded(&huge, 300);
        assert_eq!(out, format!("{}...", "a".repeat(297)));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(250),
            "64 MiB in {:?}: the whole input was escaped",
            started.elapsed()
        );
    }

    #[test]
    fn a_refused_connect_is_no_server() {
        let refused =
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
                .context("connecting to server at /x");
        assert!(no_listener(&refused));
        let other = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("connecting");
        assert!(!no_listener(&other));
    }
}
