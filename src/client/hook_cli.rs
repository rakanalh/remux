//! `remux msg hook claude-stop | claude-prompt`: Claude Code's `Stop` hook,
//! which hands an agent the mail that arrived while it worked, and its
//! `UserPromptSubmit` hook, which puts queued mail beside the prompt its user
//! just submitted.
//!
//! A hook must never fail or hang the agent. Every outcome exits 0, and every
//! soft failure (no server, an unverified caller, a timeout, a payload that is
//! not the event's) prints ONE line on stderr and NOTHING on stdout: Claude
//! parses stdout, and a stray line there is the one thing that can break it.
//!
//! The mail is peeked, then acknowledged, then printed. A hook killed between
//! a marking read and its print would have consumed mail the model never saw;
//! killed between the peek and the print, the mail is delivered again on the
//! next Stop. A duplicate beats a loss. The read and the ack name no pane, so
//! the server answers both for the hook's own attributed pane: a hook it
//! cannot place is refused `not_in_pane`, prints nothing, and can never hand
//! an agent mail that is not its own, or the same mail on every Stop.
//!
//! The payload reader is P2's own copy until `feat/subagent-panes` merges, at
//! which point it is unified with `integrate::claude::read_payload`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::client::cli_common::{ask, escape, pane_hint, CliError};
use crate::client::msg_cli::{render_context, render_mark};
use crate::protocol::{ClientMessage, Message, PaneId, ServerMessage};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum HookEvent {
    /// Claude Code's `Stop`: the turn is about to end
    ClaudeStop,
    /// Claude Code's `UserPromptSubmit`: the user submitted a prompt
    ClaudePrompt,
}

impl HookEvent {
    fn verb(self) -> &'static str {
        match self {
            HookEvent::ClaudeStop => "claude-stop",
            HookEvent::ClaudePrompt => "claude-prompt",
        }
    }

    /// The `hook_event_name` the payload must carry.
    fn event_name(self) -> &'static str {
        match self {
            HookEvent::ClaudeStop => "Stop",
            HookEvent::ClaudePrompt => "UserPromptSubmit",
        }
    }
}

/// How long each of the hook's two requests may take for its reply. The
/// payload's wait, two connects and two replies sum to well under
/// [`HOOK_TIMEOUT`], so Claude never kills the hook between its ack and its
/// print, which is the one place a kill would lose mail.
pub(crate) const HOOK_REPLY_DEADLINE: Duration = Duration::from_millis(2500);

/// The `timeout` the hook command is installed with.
pub(crate) const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of a payload read: a `Stop` payload is a few hundred bytes.
const MAX_PAYLOAD: u64 = 1024 * 1024;

/// How long the writer has to close standard input.
const PAYLOAD_WAIT: Duration = Duration::from_millis(1500);

/// The most continuations in a row a `Stop` hook gives a turn. A sender that
/// mails an agent once per Stop would otherwise keep its turn going for ever;
/// past this the mail waits for the user's next prompt.
pub const MAX_CONTINUATIONS: u32 = 3;

/// The two fields kept from Claude's payload. The rest is dropped by the parse
/// and never held, logged or quoted.
#[derive(Debug, serde::Deserialize)]
struct Payload {
    hook_event_name: String,
    #[serde(default)]
    stop_hook_active: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum PayloadError {
    Invalid(&'static str),
    TimedOut,
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayloadError::Invalid(why) => write!(f, "{why}"),
            PayloadError::TimedOut => write!(f, "the hook's payload did not arrive within 2s"),
        }
    }
}

/// A terminal on standard input is someone running the hook by hand: it would
/// wait for an end of input nobody knows to type.
fn not_a_terminal(fd: std::os::fd::RawFd) -> Result<(), PayloadError> {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(fd) } == 1 {
        Err(PayloadError::Invalid(
            "standard input is a terminal, not a hook's payload",
        ))
    } else {
        Ok(())
    }
}

/// Claude's payload, read from standard input on a thread of its own, so a
/// writer that never closes it costs [`PAYLOAD_WAIT`] and not the hook's
/// whole timeout. The thread is left behind on a timeout; the process exits.
fn read_payload() -> Result<Payload, PayloadError> {
    not_a_terminal(libc::STDIN_FILENO)?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut raw = Vec::new();
        let read = std::io::stdin()
            .lock()
            .take(MAX_PAYLOAD + 1)
            .read_to_end(&mut raw)
            .map(|_| raw);
        let _ = tx.send(read);
    });
    match rx.recv_timeout(PAYLOAD_WAIT) {
        Ok(Ok(raw)) => parse_payload(&raw),
        Ok(Err(_)) => Err(PayloadError::Invalid("standard input could not be read")),
        Err(_) => Err(PayloadError::TimedOut),
    }
}

fn parse_payload(raw: &[u8]) -> Result<Payload, PayloadError> {
    if raw.len() as u64 > MAX_PAYLOAD {
        return Err(PayloadError::Invalid("the payload is larger than 1 MiB"));
    }
    serde_json::from_slice(raw).map_err(|_| PayloadError::Invalid("the payload is not JSON"))
}

/// The payload is for this hook's event: a command wired to the wrong event
/// in `settings.json` must not deliver mail where it was not asked for.
fn check_event(event: HookEvent, payload: &Payload) -> Result<(), String> {
    if payload.hook_event_name == event.event_name() {
        Ok(())
    } else {
        Err(format!(
            "this hook is for {}, and the payload is for another event: check the hook's \
             entry in settings.json",
            event.event_name()
        ))
    }
}

/// What the hook prints for `event` around `rendered`, or nothing for no mail.
fn output_for(event: HookEvent, rendered: &str) -> Option<String> {
    if rendered.is_empty() {
        return None;
    }
    let out = match event {
        HookEvent::ClaudeStop => serde_json::json!({ "decision": "block", "reason": rendered }),
        HookEvent::ClaudePrompt => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": rendered,
            }
        }),
    };
    Some(out.to_string())
}

/// Whether a Stop delivers, and the count of continuations to store. A fresh
/// turn always delivers and starts the count again; a continuation delivers
/// while fewer than [`MAX_CONTINUATIONS`] came before it in a row.
fn continuation_verdict(stop_hook_active: bool, stored: u32) -> (bool, u32) {
    if !stop_hook_active {
        (true, 0)
    } else if stored < MAX_CONTINUATIONS {
        (true, stored + 1)
    } else {
        (false, stored)
    }
}

/// Where a pane's continuation count is kept, keyed by the pane the SERVER
/// answered for, never by `$REMUX_PANE`.
fn counter_path(dir: &Path, pane: PaneId) -> PathBuf {
    dir.join(format!("{pane}.json"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Continuations {
    pane: PaneId,
    blocks_in_a_row: u32,
}

/// Whether an opened counter is a regular file of ours. Opened without
/// following a symlink and without blocking, so a FIFO or a device put in its
/// place is found here rather than waited on.
fn ours(file: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    file.metadata()
        .is_ok_and(|meta| meta.file_type().is_file() && meta.uid() == uid)
}

/// The stored count. A file that is missing, unreadable, not this shape, or
/// not a regular file of ours counts as 0: a broken counter can allow a block
/// too many, and must never keep mail from an agent.
fn load_count(path: &Path) -> u32 {
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    else {
        return 0;
    };
    if !ours(&file) {
        return 0;
    }
    let mut raw = String::new();
    if file.take(4096).read_to_string(&mut raw).is_err() {
        return 0;
    }
    serde_json::from_str::<Continuations>(&raw).map_or(0, |c| c.blocks_in_a_row)
}

/// Store the count, readable by its owner only, in a regular file of ours. It
/// is truncated only once it has been checked, so a file that is not ours is
/// refused untouched.
fn store_count(dir: &Path, pane: PaneId, blocks_in_a_row: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let body = serde_json::to_string(&Continuations {
        pane,
        blocks_in_a_row,
    })
    .unwrap_or_default();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(counter_path(dir, pane))?;
    if !ours(&file) {
        return Err(std::io::Error::other("not a regular file of ours"));
    }
    // The mode above applies only when the file is created.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.set_len(0)?;
    file.write_all(body.as_bytes())
}

/// Where the continuation counts live under the state directory `state`, or
/// `None` when there is no private place for them: `state` is the shared
/// `/tmp` fallback, or the hooks directory is not a private directory of
/// ours. The cap is then skipped; it only limits extra blocks.
fn counter_dir_in(state: &Path) -> Option<PathBuf> {
    if state == Path::new("/tmp") {
        return None;
    }
    let dir = state.join("remux").join("hooks");
    match crate::server::daemon::private_dir(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            log::warn!("cmd: msg hook: no continuation cap: {e:#}");
            None
        }
    }
}

/// A pane's stored continuation count, and where to store the next.
struct Counter {
    dir: PathBuf,
    pane: PaneId,
    stored: u32,
}

/// A counter that cannot be read counts as 0, and one that cannot be
/// written, or has no private directory, is not kept at all: the cap then
/// does not apply. It only limits continuations, so failing open never keeps
/// mail from an agent.
impl Counter {
    fn load(pane: PaneId) -> Option<Counter> {
        let dir = counter_dir_in(&crate::paths::state_home())?;
        let stored = load_count(&counter_path(&dir, pane));
        Some(Counter { dir, pane, stored })
    }

    fn store(&self, count: u32) {
        if count != self.stored {
            if let Err(e) = store_count(&self.dir, self.pane, count) {
                log::warn!("cmd: msg hook: could not store the continuation count: {e}");
            }
        }
    }
}

/// What a hook run delivers: what to print, a line for stderr beside it, and
/// the count to store once it has been printed.
struct Delivery {
    out: Option<String>,
    note: Option<String>,
    store: Option<(Counter, u32)>,
}

/// Run the hook for `event`. Always 0: Claude must never see a failed hook.
///
/// The mail is printed before the count is stored: the ack has marked it, and
/// nothing between the ack and the print may fail or wait, or the agent never
/// sees mail that is no longer queued.
pub async fn run_hook(event: HookEvent) -> u8 {
    match hook(event).await {
        Ok(delivery) => {
            if let Some(out) = delivery.out {
                use std::io::Write;
                println!("{out}");
                let _ = std::io::stdout().flush();
            }
            if let Some(note) = delivery.note {
                eprintln!("remux: msg hook {}: {note}", event.verb());
            }
            if let Some((counter, count)) = delivery.store {
                counter.store(count);
            }
        }
        Err(why) => {
            log::info!("cmd: msg hook {} -> not delivered", event.verb());
            eprintln!("remux: msg hook {}: {}", event.verb(), escape(&why));
        }
    }
    0
}

/// What the hook prints and stores, or the one line a soft failure writes to
/// stderr.
async fn hook(event: HookEvent) -> Result<Delivery, String> {
    let payload = read_payload().map_err(|e| e.to_string())?;
    check_event(event, &payload)?;
    let refused = |e: CliError| {
        let line = e.stderr();
        line.strip_prefix("remux: ")
            .map(str::to_string)
            .unwrap_or(line)
    };
    let answer = ask(
        ClientMessage::MsgRead {
            pane: None,
            all: false,
            peek: true,
            wait_ms: None,
            pane_hint: pane_hint(),
        },
        HOOK_REPLY_DEADLINE,
    )
    .await
    .map_err(refused)?;
    let ServerMessage::MsgList { pane, messages, .. } = answer else {
        return Err(format!("unexpected {} answer", answer.variant_name()));
    };
    // Only a Stop continues a turn, so only a Stop counts continuations. A
    // prompt is the user's own turn, and delivers what the cap held back.
    let counter = match event {
        HookEvent::ClaudeStop => Counter::load(pane),
        HookEvent::ClaudePrompt => None,
    };
    if messages.is_empty() {
        log::debug!(
            "cmd: msg hook {} -> 0 queued, stop_hook_active={}",
            event.verb(),
            payload.stop_hook_active
        );
        return Ok(Delivery {
            out: None,
            note: None,
            store: counter.map(|c| (c, 0)),
        });
    }
    let stored = counter.as_ref().map_or(0, |c| c.stored);
    let (deliver, count) = match event {
        HookEvent::ClaudeStop => continuation_verdict(payload.stop_hook_active, stored),
        HookEvent::ClaudePrompt => (true, 0),
    };
    if !deliver {
        log::debug!(
            "cmd: msg hook {} -> {} queued, held: {MAX_CONTINUATIONS} continuations in a row",
            event.verb(),
            messages.len()
        );
        return Err(format!(
            "{MAX_CONTINUATIONS} continuations in a row; {} message(s) left for the next prompt",
            messages.len()
        ));
    }
    let ids: Vec<_> = messages.iter().map(|m| m.id).collect();
    // A harness seam, in debug builds only: pause between the peek and the
    // ack for `REMUX_HOOK_DELAY_MS` (at most 3 s, inside the hook's deadline),
    // so a test can acknowledge the mailbox in between. A release build never
    // reads the variable.
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("REMUX_HOOK_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        tokio::time::sleep(Duration::from_millis(ms.min(1500))).await;
    }
    let acked = ask(
        ClientMessage::MsgAck {
            pane: None,
            ids,
            all: false,
            pane_hint: pane_hint(),
        },
        HOOK_REPLY_DEADLINE,
    )
    .await;
    let acked = lose_the_reply(acked);
    // A refusal says the ack marked nothing, and nothing is shown. An ack
    // that was sent and never answered may have marked the mail all the same,
    // so the mail is shown: a duplicate if it did not, never a loss if it did.
    let note = match acked {
        Ok(ServerMessage::MsgAcked { count: 0, .. }) => {
            return Err(
                "the mail was acknowledged elsewhere before this hook could, so it is \
                        not shown here"
                    .to_string(),
            );
        }
        Ok(ServerMessage::MsgAcked { count, .. }) => {
            log::debug!(
                "cmd: msg hook {} -> {} queued, acked {count}, stop_hook_active={}",
                event.verb(),
                messages.len(),
                payload.stop_hook_active
            );
            None
        }
        Ok(other) => return Err(format!("unexpected {} answer", other.variant_name())),
        Err(e @ CliError::Refused { .. }) => return Err(refused(e)),
        Err(e) => {
            let why = refused(e);
            log::info!(
                "cmd: msg hook {} -> {} queued, ack unconfirmed: {why}",
                event.verb(),
                messages.len()
            );
            Some(format!(
                "ack unconfirmed ({why}); the mail is shown, and may be shown again"
            ))
        }
    };
    // Everything peeked, though the ack may have marked fewer: whatever it
    // marked must be shown, and a message someone else marked in between is
    // shown once more, a duplicate rather than a loss.
    Ok(Delivery {
        out: output_for(event, &render(&messages)),
        note,
        store: counter.map(|c| (c, count)),
    })
}

/// A harness seam, in debug builds only: with `REMUX_HOOK_ACK_REPLY_LOST`
/// set, an ack the server answered is taken as one whose answer never came,
/// so a test sees the mail marked and the hook unsure of it. A release build
/// never reads the variable.
fn lose_the_reply(acked: Result<ServerMessage, CliError>) -> Result<ServerMessage, CliError> {
    #[cfg(debug_assertions)]
    if std::env::var_os("REMUX_HOOK_ACK_REPLY_LOST").is_some() {
        return Err(CliError::Timeout);
    }
    acked
}

fn render(messages: &[Message]) -> String {
    render_context(messages, &render_mark())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::cli_common::CONNECT_DEADLINE;
    use crate::client::msg_cli::RENDER_PREAMBLE;
    use crate::protocol::{MsgState, Sender};

    fn message(id: u64, from: Sender, body: &str) -> Message {
        Message {
            id,
            from,
            to: 2,
            body: body.into(),
            created_ms: 0,
            expires_ms: u64::MAX,
            state: MsgState::Queued,
            reply_to: None,
        }
    }

    fn from_pane(id: u64) -> Message {
        message(
            id,
            Sender::Pane {
                pane_id: 1,
                name: Some("lead".into()),
                session: "main".into(),
            },
            "line one\n\"quoted\" line two",
        )
    }

    #[test]
    fn stop_output_is_a_block_with_the_render_as_reason() {
        let rendered = render(&[from_pane(1), from_pane(2)]);
        let out = output_for(HookEvent::ClaudeStop, &rendered).unwrap();
        assert!(!out.contains('\n'), "one line on stdout: {out}");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["decision"], "block");
        let reason = parsed["reason"].as_str().unwrap();
        assert_eq!(
            reason, rendered,
            "the render verbatim, newlines and quotes too"
        );
        assert!(reason.starts_with(RENDER_PREAMBLE));
    }

    #[test]
    fn prompt_output_wraps_the_render_in_hook_specific_output() {
        let rendered = render(&[from_pane(1)]);
        let out = output_for(HookEvent::ClaudePrompt, &rendered).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            rendered.as_str()
        );
        assert!(parsed.get("additionalContext").is_none() && parsed.get("decision").is_none());
        assert_eq!(output_for(HookEvent::ClaudePrompt, ""), None);
    }

    #[test]
    fn a_stop_payload_on_the_prompt_verb_is_a_soft_failure() {
        let stop = parse_payload(br#"{"hook_event_name":"Stop"}"#).unwrap();
        let why = check_event(HookEvent::ClaudePrompt, &stop).unwrap_err();
        assert!(why.contains("UserPromptSubmit"), "{why}");
        let prompt = parse_payload(
            br#"{"hook_event_name":"UserPromptSubmit","prompt":"a secret the user typed"}"#,
        )
        .unwrap();
        assert!(check_event(HookEvent::ClaudePrompt, &prompt).is_ok());
        assert!(
            !format!("{prompt:?}").contains("secret"),
            "the prompt is not held"
        );
        assert!(check_event(HookEvent::ClaudeStop, &prompt).is_err());
    }

    #[test]
    fn an_empty_mailbox_prints_nothing() {
        assert_eq!(output_for(HookEvent::ClaudeStop, &render(&[])), None);
    }

    #[test]
    fn the_payload_keeps_two_fields_and_drops_the_rest() {
        let raw = br#"{"session_id":"s","transcript_path":"/x","hook_event_name":"Stop",
            "stop_hook_active":true,"last_assistant_message":"a secret"}"#;
        let payload = parse_payload(raw).unwrap();
        assert_eq!(payload.hook_event_name, "Stop");
        assert!(payload.stop_hook_active);
        assert!(
            !format!("{payload:?}").contains("secret"),
            "nothing else is held"
        );
        let missing = parse_payload(br#"{"hook_event_name":"Stop"}"#).unwrap();
        assert!(!missing.stop_hook_active, "absent means a fresh turn");
        assert_eq!(
            parse_payload(b"not json").unwrap_err(),
            PayloadError::Invalid("the payload is not JSON")
        );
        let huge = vec![b' '; MAX_PAYLOAD as usize + 1];
        assert!(matches!(
            parse_payload(&huge),
            Err(PayloadError::Invalid(_))
        ));
    }

    #[test]
    fn a_wrong_event_name_is_a_soft_failure() {
        let payload = parse_payload(br#"{"hook_event_name":"PreToolUse"}"#).unwrap();
        let why = check_event(HookEvent::ClaudeStop, &payload).unwrap_err();
        assert!(why.contains("Stop") && !why.contains("PreToolUse"), "{why}");
        let stop = parse_payload(br#"{"hook_event_name":"Stop"}"#).unwrap();
        assert!(check_event(HookEvent::ClaudeStop, &stop).is_ok());
    }

    #[test]
    fn a_terminal_on_stdin_is_invalid() {
        let pair = nix::pty::openpty(None, None).unwrap();
        use std::os::fd::AsRawFd;
        assert!(matches!(
            not_a_terminal(pair.slave.as_raw_fd()),
            Err(PayloadError::Invalid(_))
        ));
        let (read, _write) = nix::unistd::pipe().unwrap();
        assert_eq!(not_a_terminal(read.as_raw_fd()), Ok(()));
    }

    #[test]
    fn a_fresh_turn_always_delivers_and_resets() {
        for stored in [0, 2, MAX_CONTINUATIONS, 99] {
            assert_eq!(continuation_verdict(false, stored), (true, 0), "{stored}");
        }
    }

    #[test]
    fn the_fourth_continuation_is_refused() {
        let mut stored = continuation_verdict(false, 0).1;
        for n in 1..=MAX_CONTINUATIONS {
            let (deliver, next) = continuation_verdict(true, stored);
            assert!(deliver, "continuation {n}");
            stored = next;
        }
        assert_eq!(
            continuation_verdict(true, stored),
            (false, MAX_CONTINUATIONS)
        );
    }

    #[test]
    fn a_broken_counter_file_counts_as_zero() {
        let dir = std::env::temp_dir().join(format!("remux-hook-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = counter_path(&dir, 7);
        assert_eq!(load_count(&path), 0, "missing");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_count(&path), 0, "unparsable");
        store_count(&dir, 7, 2).unwrap();
        assert_eq!(load_count(&path), 2);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_counter_that_is_a_fifo_is_neither_waited_on_nor_written() {
        let dir = std::env::temp_dir().join(format!("remux-hook-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = counter_path(&dir, 9);
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(load_count(&path), 0);
        assert!(store_count(&dir, 9, 2).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no wait on a FIFO"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_shared_tmp_fallback_has_no_counter() {
        assert!(counter_dir_in(Path::new("/tmp")).is_none());
        let state = std::env::temp_dir().join(format!("remux-hook-state-{}", std::process::id()));
        let dir = counter_dir_in(&state).expect("a private hooks directory");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_remote_sender_is_labelled_by_host() {
        let remote = message(
            3,
            Sender::Remote {
                host: "mini".into(),
                pane_id: 4,
                name: Some("beta".into()),
            },
            "hi",
        );
        let out = output_for(HookEvent::ClaudeStop, &render(&[remote])).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(
            parsed["reason"]
                .as_str()
                .unwrap()
                .contains("From: mini:beta (mini:%4)"),
            "{out}"
        );
    }

    #[test]
    fn the_hook_deadline_fits_the_hook_timeout() {
        let worst = PAYLOAD_WAIT + (CONNECT_DEADLINE + HOOK_REPLY_DEADLINE) * 2;
        assert!(
            worst + Duration::from_secs(1) <= HOOK_TIMEOUT,
            "{worst:?} leaves under a second of {HOOK_TIMEOUT:?}"
        );
    }
}
