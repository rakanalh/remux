//! Typing a message into an agent's pane: how its bytes are framed.
//!
//! A message is typed as a probe, the body and the Enter. The probe is
//! something nothing acts on: the paste opener when the text is wrapped, a
//! space when it is not ([`split_probe`]). The body is wrapped in bracketed
//! paste when the pane's program has asked for it (mode 2004), so the agent
//! reads it as one pasted block rather than as keystrokes whose newlines
//! would each submit a line. The Enter is written after a delay: an
//! agent that reads a paste and an Enter in one read can take the Enter as
//! part of the paste, and the paste as not yet submitted. The Enter is never
//! inside the wrap for the same reason.
//!
//! The body is written only once the agent has read the probe. An agent that
//! is not reading its input (hung, or about to exit) is found out on the
//! probe, which is flushed, so the body is never in its input queue to be
//! read by the shell after it.
//!
//! The body cannot close the wrap early: a send refuses every control
//! character but `\n` and `\t` (`bad_body`), so `ESC[201~` never reaches this
//! module.
//!
//! The server types only into a program that reads what it is typed. Before
//! the text it checks the pane's foreground is still the agent it sampled and
//! that the terminal is not line-buffered (a shell, `read` or a password
//! prompt is reading, not the agent). Before the Enter it checks the agent
//! READ the text: bytes left in the terminal's input queue would be read by
//! whatever runs next, which after the agent exits is the shell, and the
//! Enter would run them. Unread text is flushed and the message stays in the
//! mailbox.
//!
//! Both writes re-check two things in the same `panes` section as the write.
//! The agent must not be waiting on its user (`NeedsInput`): an Enter into a
//! permission prompt answers it, so a message could approve another agent's
//! action. A submit takes a pane as waiting on its user when a configured
//! pattern says so, and also when its screen looks like a numbered selector
//! ([`looks_like_prompt`]), so that a prompt no pattern knows is not
//! approved either. And no human may have typed into the pane since it was
//! sampled, for the text, or since the text was written, for the Enter: the
//! Enter would submit the person's keys with the message. A person's key
//! that arrives while the message is still in the input queue, unread, takes
//! the pane back: the rest of the message is cancelled and flushed before the
//! key is written, so the key never follows the message into the next
//! reader. A key that arrives after the agent read the message is the
//! agent's to handle with it; the message is then the agent's, and is left
//! in its input unsubmitted (`typed_over_read`).
//!
//! Each typed message opens with an envelope line naming it and its sender,
//! so the agent, and the person watching it, can tell another agent's
//! message from its user's.
//!
//! The body goes in chunks ([`split_chunks`]), each written only once the
//! agent has read the one before, so at most one chunk is ever unread. A
//! chunk holds at most one line, and its newline comes first: a line's text
//! never travels with the newline that would run it.
//!
//! Residuals:
//! - An agent that exits part-way through a body hands the terminal to the
//!   shell, which reads the unread chunk. The chunk starts with the newline
//!   that ends a line the AGENT read, so for the shell it ends an empty line,
//!   and the chunk's own line sits in the shell's line unexecuted until
//!   someone presses Enter there. At most that one line; the submit is
//!   answered `read_by_another_program`. The first chunk is the envelope,
//!   whose `# ` makes it a comment to a shell, but only when the envelope is
//!   on: with `envelope = false` the first chunk is the body's own first
//!   line, and an unwrapped message has no envelope line at all.
//! - The probe is taken as read when the input queue is empty, and any
//!   program in the agent's process group could have emptied it: a child
//!   the agent started that reads the terminal takes the probe, and the body
//!   follows into whatever that child does with it.
//! - A flush on a refusal and a person's key can cross: a key that arrives
//!   after the typing is marked ended and before the queue is flushed is
//!   flushed with the message's text.
//! - A key a human presses after the agent read the text stops the Enter,
//!   and the text stays in the agent's input box beside the person's keys,
//!   unsubmitted.
//! - A permission prompt the agent draws after the last check, in the time
//!   between the check and the Enter's write, is still answered by the Enter.
//!   The check and the write share one `panes` section, so the window is the
//!   agent's own: the time between reading the text and drawing a prompt the
//!   server has not yet parsed.
//! - A prompt that is neither named by a pattern nor drawn as a numbered
//!   selector reads as not waiting, and its Enter is typed.

pub const PASTE_OPEN: &[u8] = b"\x1b[200~";
pub const PASTE_CLOSE: &[u8] = b"\x1b[201~";
pub const FOCUS_IN: &[u8] = b"\x1b[I";

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::config::messaging::{
    shipped_rows, valid_enter, Bracketed, MessagingConfig, SubmitRow, WhileWorking,
    MAX_ENTER_DELAY_MS, MIN_ENTER_DELAY_MS,
};
use crate::protocol::{AgentState, Deliver, PaneId};

/// How to type into one agent: its `[messaging.submit.<command>]` row,
/// merged over the shipped row and the defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitRule {
    pub bracketed: Bracketed,
    pub enter: Vec<u8>,
    pub enter_delay: Duration,
    pub focus_in: bool,
    pub while_working: WhileWorking,
    pub envelope: bool,
}

impl Default for SubmitRule {
    fn default() -> Self {
        SubmitRule {
            bracketed: Bracketed::Auto,
            enter: b"\r".to_vec(),
            enter_delay: Duration::from_millis(300),
            focus_in: false,
            while_working: WhileWorking::Mailbox,
            envelope: true,
        }
    }
}

/// Every command's row: the shipped ones with the user's merged over them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitPolicy {
    rows: BTreeMap<String, SubmitRow>,
}

impl SubmitPolicy {
    pub fn from_config(cfg: &MessagingConfig) -> Self {
        let mut rows = shipped_rows();
        for (command, row) in &cfg.submit {
            let merged = rows.get(command).cloned().unwrap_or_default().merged(row);
            rows.insert(command.clone(), merged);
        }
        SubmitPolicy { rows }
    }

    /// The rule for `command`, as `[agents] commands` spells it. A field no
    /// row sets, and an `enter` that could not submit (reported at load),
    /// take the default.
    pub fn rule_for(&self, command: &str) -> SubmitRule {
        let default = SubmitRule::default();
        let Some(row) = self.rows.get(command) else {
            return default;
        };
        SubmitRule {
            bracketed: row.bracketed.unwrap_or(default.bracketed),
            enter: row
                .enter
                .as_deref()
                .filter(|e| valid_enter(e))
                .map_or(default.enter, |e| e.as_bytes().to_vec()),
            enter_delay: row.enter_delay_ms.map_or(default.enter_delay, |ms| {
                Duration::from_millis(ms.clamp(MIN_ENTER_DELAY_MS, MAX_ENTER_DELAY_MS))
            }),
            focus_in: row.focus_in.unwrap_or(default.focus_in),
            while_working: row.while_working.unwrap_or(default.while_working),
            envelope: row.envelope.unwrap_or(default.envelope),
        }
    }
}

/// How long a pane must be keyboard-quiet before a message is typed into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardConfig {
    /// When a client is looking at the pane.
    pub attended: Duration,
    /// When none is.
    pub unattended: Duration,
    /// The longest a submit waits for the human to stop.
    pub max: Duration,
}

/// Whether a submit may type now, given the last time a human typed into the
/// pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardVerdict {
    /// Nobody typed, or the pane has been quiet long enough.
    Ready,
    /// Wait until this instant, when the pane will have been quiet long
    /// enough if nobody types again.
    Wait(std::time::Instant),
    /// The human kept typing past `max`: give up.
    Expired,
}

/// The typing guard: a message is typed only into a pane that has been
/// keyboard-quiet for a window, longer when a client is looking at it, since
/// a person there may be halfway through a line of their own. Keystrokes
/// only: the pane's own output does not hold a submit.
pub fn guard_wait(
    last_human_input: Option<std::time::Instant>,
    attended: bool,
    enqueued_at: std::time::Instant,
    now: std::time::Instant,
    cfg: &GuardConfig,
) -> GuardVerdict {
    let window = if attended {
        cfg.attended
    } else {
        cfg.unattended
    };
    let Some(last) = last_human_input else {
        return GuardVerdict::Ready;
    };
    if now.saturating_duration_since(last) >= window {
        return GuardVerdict::Ready;
    }
    if now.saturating_duration_since(enqueued_at) >= cfg.max {
        return GuardVerdict::Expired;
    }
    GuardVerdict::Wait(last + window)
}

/// The answer to a submit that does not type the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Leave it in the mailbox and say so, under either mode: the reason is a
    /// delivery choice, not a refusal.
    Mailbox(&'static str),
    /// Refuse the send.
    Refuse(SubmitRefusal),
    /// Leave it in the mailbox because its time ran out while it waited on
    /// the reason given.
    TimedOut(&'static str),
}

/// How `refusal` is answered under `deliver`. `Submit` asked to be told, so
/// it is refused; `Auto` never errors on a policy outcome, so the message is
/// reported in the mailbox, where it is, with the refusal as its reason. A
/// pane that closed is an address refusal under both.
pub fn settle(deliver: Deliver, refusal: SubmitRefusal) -> Answer {
    if deliver == Deliver::Submit || refusal.code == "no_such_target" {
        Answer::Refuse(refusal)
    } else {
        Answer::Mailbox(refusal.code)
    }
}

/// How an entry whose deadline passed before it was typed is answered.
/// `Submit` is refused. `Auto` is told the message is in the mailbox and
/// why it waited: `waiting_on`, or `submit_timeout` for an entry that never
/// reached the head of its queue and so never waited on anything itself.
pub fn overdue(
    deliver: Deliver,
    pane: PaneId,
    id: crate::protocol::MsgId,
    waiting_on: Option<&'static str>,
) -> Answer {
    match deliver {
        Deliver::Auto => Answer::TimedOut(waiting_on.unwrap_or("submit_timeout")),
        _ => Answer::Refuse(SubmitRefusal::out_of_time(pane, id)),
    }
}

/// Whether a submit waits instead of being answered `agent_blocked`: it was
/// deferred, and the agent is there and waiting on its user. Only that wait
/// is deferred; a pane that is not running an agent is answered at once.
pub fn waits_while_blocked(
    live: bool,
    agent: bool,
    state: Option<AgentState>,
    defer: bool,
) -> bool {
    defer && live && agent && state == Some(AgentState::NeedsInput)
}

/// The checks made before the typing guard: the pane is there, its
/// foreground is an agent, and the agent is not waiting on its user. `None`
/// means go on to the guard.
pub fn precheck(
    pane: PaneId,
    live: bool,
    agent: bool,
    state: Option<AgentState>,
    deliver: Deliver,
) -> Option<Answer> {
    if !live {
        return Some(settle(deliver, SubmitRefusal::closed(pane)));
    }
    if !agent {
        return Some(settle(deliver, SubmitRefusal::not_ready(pane)));
    }
    (state == Some(AgentState::NeedsInput)).then(|| settle(deliver, SubmitRefusal::blocked(pane)))
}

/// Whether the bottom of an agent's screen looks like a selector waiting on
/// its user: its LAST block of numbered option rows, on consecutive lines,
/// has two or more options, one of them marked as the choice or offering Esc
/// ("3. No, and tell Claude what to do differently (esc)"). Only the last
/// block counts, and only whole: a selector is drawn at the bottom in one
/// piece, while numbered rows further up are the transcript.
///
/// A submit refuses such a pane as `agent_blocked` whatever the configured
/// patterns say. A pattern names a prompt by its wording, and a wording no
/// pattern knows reads `Idle`, where the Enter after the text picks the
/// selector's default, which for an approval is "Yes". This check knows no
/// wording, so it fails closed: a numbered list drawn the same way leaves a
/// message in the mailbox, which is the safe side. It is not part of the
/// agents panel's classification, where a false `NeedsInput` never decays.
pub fn looks_like_prompt(bottom: &[String]) -> bool {
    let block: Vec<(bool, &str)> = bottom
        .iter()
        .rev()
        .skip_while(|line| option_row(line).is_none())
        .map_while(|line| option_row(line))
        .collect();
    block.len() >= 2
        && block
            .iter()
            .any(|(marked, text)| *marked || text.to_ascii_lowercase().contains("(esc)"))
}

/// A numbered option row (`❯ 1. Yes`, `  2) No`): whether it carries a
/// selection marker, and its text after the number. `>` is not a marker:
/// Claude Code draws a past prompt of the user's with it, and a numbered
/// list typed there is not a selector.
fn option_row(line: &str) -> Option<(bool, &str)> {
    let rest = line.trim_start();
    let unmarked = rest.trim_start_matches(['\u{276f}', '\u{203a}']);
    let marked = unmarked.len() != rest.len();
    let rest = unmarked.trim_start();
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=2).contains(&digits) {
        return None;
    }
    let rest = rest[digits..].strip_prefix(['.', ')'])?;
    let text = rest.trim_start();
    (text.len() < rest.len() && !text.is_empty()).then_some((marked, text))
}

/// Whether the working policy is asked before the typing guard, as §8.2
/// orders the checks. Not while a human's last key is recent: the agent's
/// echo of a keystroke is pane output, so the classifier reads `Working`
/// for `working` after every key, and a `mailbox` or `refuse` row would then
/// answer `agent_working` for an agent that is only showing what a person
/// typed. Past `working`, a `Working` verdict cannot be that echo.
pub fn working_first(
    last_human_input: Option<std::time::Instant>,
    now: std::time::Instant,
    working: Duration,
) -> bool {
    last_human_input.is_none_or(|last| now.saturating_duration_since(last) >= working)
}

/// What the row says to do with an agent that is working.
pub fn on_working(
    pane: PaneId,
    state: Option<AgentState>,
    rule: &SubmitRule,
    deliver: Deliver,
) -> Option<Answer> {
    if state != Some(AgentState::Working) {
        return None;
    }
    match rule.while_working {
        WhileWorking::Queue => None,
        WhileWorking::Mailbox => Some(Answer::Mailbox("agent_working")),
        WhileWorking::Refuse => Some(settle(deliver, SubmitRefusal::working(pane))),
    }
}

/// How the body is wrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    Bracketed,
    Plain,
}

/// A submit the server will not make, with its stable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitRefusal {
    pub code: &'static str,
    pub message: String,
}

impl SubmitRefusal {
    pub fn not_ready(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "agent_not_ready",
            message: format!(
                "%{pane} is not running an agent this server knows: its message is in the mailbox"
            ),
        }
    }

    pub fn closed(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "no_such_target",
            message: format!("pane %{pane} closed"),
        }
    }

    pub fn blocked(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "agent_blocked",
            message: format!(
                "%{pane} is waiting on its user's answer: its message is in the mailbox"
            ),
        }
    }

    pub fn attended(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "pane_attended",
            message: format!(
                "a client is looking at %{pane}, and this server does not type into an attended pane: its message is in the mailbox"
            ),
        }
    }

    pub fn out_of_time(pane: PaneId, id: crate::protocol::MsgId) -> Self {
        SubmitRefusal {
            code: "submit_timeout",
            message: format!(
                "message #{id} waited too long to be typed into %{pane}, so it was not typed: it is in the mailbox"
            ),
        }
    }

    pub fn not_read(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "agent_did_not_read",
            message: format!(
                "%{pane}'s agent did not read the message, so it was not submitted: it is in the mailbox"
            ),
        }
    }

    /// The agent read the probe and then stopped part-way through the body.
    /// What it read before it stopped is in its input, unsubmitted, which a
    /// person must clear: sending it again would type it twice.
    pub fn stopped_reading(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "agent_did_not_read",
            message: format!(
                "%{pane}'s agent stopped reading the message part-way, so it was not submitted: what it read may be left in its input, unsent; clear that there, and do not send it again with submit. It is in the mailbox"
            ),
        }
    }

    /// No random source answered for the envelope's mark, and a predictable
    /// mark is a forgeable one, so nothing is typed.
    pub fn no_mark(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "no_randomness",
            message: format!(
                "no random source answered for the envelope's mark, so nothing was typed into %{pane}: it is in the mailbox"
            ),
        }
    }

    pub fn too_many_lines(pane: PaneId, max: usize) -> Self {
        SubmitRefusal {
            code: "too_many_lines",
            message: format!(
                "the message has more lines than the {max} this server types into %{pane}, so it was not typed: it is in the mailbox"
            ),
        }
    }

    pub fn too_slow(pane: PaneId, cap: Duration) -> Self {
        SubmitRefusal {
            code: "agent_too_slow",
            message: format!(
                "%{pane}'s agent was still reading the message after {}s, so it was not submitted, and the part it read is left in its input: clear that before pressing Enter there. It is in the mailbox",
                cap.as_secs()
            ),
        }
    }

    pub fn read_by_another(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "read_by_another_program",
            message: format!(
                "%{pane}'s agent stopped reading before it read the message, and another program may have read it instead; a line of it may be waiting at the shell prompt: clear it with Ctrl-U before pressing Enter. It is in the mailbox"
            ),
        }
    }

    /// `--wait` saw nothing reach the pane after the prompt's echo. The
    /// message was submitted, so it is not in the mailbox to be read again:
    /// it is marked submitted there.
    pub fn stalled(pane: PaneId, id: crate::protocol::MsgId, stall: Duration) -> Self {
        SubmitRefusal {
            code: "agent_prompt_stalled",
            message: format!(
                "%{pane}'s agent showed no sign of taking up message #{id} for {}s after it was \
                 submitted. It was typed and submitted, so do not send it again; look at the pane",
                stall.as_secs_f64()
            ),
        }
    }

    pub fn sender_closed(from: PaneId, id: crate::protocol::MsgId) -> Self {
        SubmitRefusal {
            code: "sender_closed",
            message: format!(
                "pane %{from}, which sent message #{id}, closed before it was typed, so it was not typed: it is in the mailbox"
            ),
        }
    }

    pub fn typed_over(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "human_typing",
            message: format!(
                "someone typed into %{pane} while its message was being typed, so it was not submitted: it is in the mailbox"
            ),
        }
    }

    pub fn human_typing(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "human_typing",
            message: format!(
                "someone kept typing into %{pane}, so the message was not typed: it is in the mailbox"
            ),
        }
    }

    pub fn working(pane: PaneId) -> Self {
        SubmitRefusal {
            code: "agent_working",
            message: format!(
                "%{pane} is working and its policy refuses to type into it: its message is in the mailbox"
            ),
        }
    }
}

/// The text split into its probe and the rest. Wrapped, the probe is the
/// paste opener (after the focus report, when there is one), which an agent
/// takes as the start of a paste and a shell's line editor as a paste of
/// nothing. Unwrapped, it is one space, written ahead of the text: a line
/// editor holds it as a line that runs nothing, and an agent shows it as a
/// leading space of the message.
pub fn split_probe(text: Vec<u8>, framing: Framing, focus_in: bool) -> (Vec<u8>, Vec<u8>) {
    let focus = if focus_in { FOCUS_IN.len() } else { 0 };
    let mut text = text;
    match framing {
        Framing::Bracketed => {
            let rest = text.split_off(focus + PASTE_OPEN.len());
            (text, rest)
        }
        Framing::Plain => {
            let rest = text.split_off(focus);
            text.push(b' ');
            (text, rest)
        }
    }
}

/// The body of a message split for typing: chunks of at most `max` bytes,
/// each holding at most one newline, and that one first. `tail`, the paste's
/// closer when there is one, is never split and ends the last chunk. Always
/// at least one chunk.
pub fn split_chunks(content: &[u8], tail: &[u8], max: usize) -> Vec<Vec<u8>> {
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut current = Vec::with_capacity(max);
    for &b in content {
        if !current.is_empty() && (b == b'\n' || current.len() >= max) {
            chunks.push(std::mem::replace(&mut current, Vec::with_capacity(max)));
        }
        current.push(b);
    }
    current.extend_from_slice(tail);
    chunks.push(current);
    chunks
}

/// The lines typed around a message's body. A body can contain a copy of the
/// opening line, so the opening line alone cannot say where the sender's
/// text ends. Both lines carry the same `mark`, drawn for this typing, which
/// the body was written before and so cannot know: a closing line with any
/// other mark is part of the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Which message it is, who sent it, its mark and how to answer.
    pub open: String,
    /// `# [end of remux message #N, mark M]`: a comment, like the opening
    /// line, to a shell left holding it at its prompt.
    pub close: String,
}

/// The envelope for message `id`. `from_name` is a validated agent name,
/// escaped all the same because the line reaches a terminal.
pub fn envelope(
    id: crate::protocol::MsgId,
    from_pane: PaneId,
    from_name: Option<&str>,
    mark: &str,
) -> Envelope {
    let from = match from_name {
        Some(name) => format!("{} (%{from_pane})", crate::client::cli_common::escape(name)),
        None => format!("%{from_pane}"),
    };
    Envelope {
        open: format!(
            "# [remux message #{id} from {from}, mark {mark}; reply: remux msg send %{from_pane} --reply-to {id}]"
        ),
        close: format!("# [end of remux message #{id}, mark {mark}]"),
    }
}

/// Whether a newline typed into the pane stays inside the message: only when
/// it is wrapped AND the program asked for bracketed paste. A wrap forced on
/// a program that never enabled it is just more bytes to that program, and
/// each newline submits a line of its own.
pub fn newlines_safe(framing: Framing, paste_live: bool) -> bool {
    framing == Framing::Bracketed && paste_live
}

/// The two writes for `body`: the text, with its wrap and `focus_in` first
/// when asked, and the Enter, which the caller writes separately after its
/// delay. The envelope's lines open and close the text, inside the wrap and
/// before the Enter: each on a line of its own when newlines are safe (see
/// [`newlines_safe`]), and on the body's line otherwise, where a newline
/// would submit a line on its own.
pub fn encode(
    body: &str,
    framing: Framing,
    paste_live: bool,
    focus_in: bool,
    enter: &[u8],
    envelope: Option<&Envelope>,
) -> (Vec<u8>, Vec<u8>) {
    let mut text = Vec::with_capacity(body.len() + 128);
    if focus_in {
        text.extend_from_slice(FOCUS_IN);
    }
    if framing == Framing::Bracketed {
        text.extend_from_slice(PASTE_OPEN);
    }
    let lines = newlines_safe(framing, paste_live);
    if let Some(envelope) = envelope {
        text.extend_from_slice(envelope.open.as_bytes());
        if lines {
            text.push(b'\n');
        } else {
            text.extend_from_slice(" \u{2014} ".as_bytes());
        }
    }
    text.extend_from_slice(body.as_bytes());
    if let Some(envelope) = envelope {
        text.push(if lines { b'\n' } else { b' ' });
        text.extend_from_slice(envelope.close.as_bytes());
    }
    if framing == Framing::Bracketed {
        text.extend_from_slice(PASTE_CLOSE);
    }
    (text, enter.to_vec())
}

/// Why `body` cannot be typed where newlines are not safe (see
/// [`newlines_safe`]): each newline would submit a line of it on its own.
pub fn multi_line_refusal(body: &str) -> Option<SubmitRefusal> {
    body.contains('\n').then(|| SubmitRefusal {
        code: "bad_body",
        message: "the pane's program cannot take a pasted block, so a message of more than one line cannot be typed into it; it is in the mailbox".to_string(),
    })
}

/// How `--wait` reads a pane after the Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitCfg {
    /// The classifier's `working_ms`: output this recent reads as `Working`.
    pub working: Duration,
    /// How long after the quiet point the agent has to print something
    /// before the wait ends `agent_prompt_stalled`.
    pub stall: Duration,
    /// How long the agent must hold a state other than `Working` for its
    /// turn to count as ended.
    pub settle: Duration,
}

/// What a step of a [`WaitMachine`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStep {
    Continue,
    /// Nothing reached the pane after the prompt's own echo: the agent did
    /// not take up the message.
    Stalled,
    /// The agent's turn ended in this state.
    Settled(AgentState),
    /// The wait's deadline passed, with the agent last in this state.
    TimedOut(AgentState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitPhase {
    /// Output here can be the agent echoing and redrawing the prompt it was
    /// typed, so it says nothing about the agent taking the message up.
    Echo,
    /// Watching for the first output after the quiet point.
    Stall { quiet_at: Instant },
    /// The agent worked; watching for its turn to end.
    Settle { left_working_at: Option<Instant> },
}

/// How an agent's turn went after a submitted message, told from samples of
/// its pane: when output last reached it, and its state.
///
/// First the echo window. It ends at the quiet point: the first moment, at
/// least `working` after the Enter, at which the pane has been quiet for
/// `working`, or three times `working` after the Enter, whichever is first.
/// The cap is there because an agent that starts printing at once and
/// prints until its turn ends is never quiet in between. Output after the
/// quiet point is the agent working; none for `stall` is a stall. Once the
/// agent has worked, its turn ends when it has held a state other than
/// `Working` for `settle`, and that state is the answer: `NeedsInput` is a
/// turn that ended in a question. The deadline ends the wait in any phase.
///
/// A turn that ends inside the echo window leaves no output after the quiet
/// point, so it reads as a stall: it cannot be told from the echo.
#[derive(Debug, Clone)]
pub struct WaitMachine {
    enter_at: Instant,
    deadline: Instant,
    cfg: WaitCfg,
    phase: WaitPhase,
}

impl WaitMachine {
    /// A wait on a message whose Enter was written at `enter_at`.
    pub fn new(enter_at: Instant, deadline: Instant, cfg: WaitCfg) -> Self {
        WaitMachine {
            enter_at,
            deadline,
            cfg,
            phase: WaitPhase::Echo,
        }
    }

    /// When the message's Enter was written.
    pub fn enter_at(&self) -> Instant {
        self.enter_at
    }

    pub fn cfg(&self) -> WaitCfg {
        self.cfg
    }

    /// Take one sample of the pane: the time, when output last reached it,
    /// and its state.
    pub fn step(&mut self, now: Instant, last_output: Instant, state: AgentState) -> WaitStep {
        if now >= self.deadline {
            return WaitStep::TimedOut(state);
        }
        if self.phase == WaitPhase::Echo {
            let quiet = now >= self.enter_at + self.cfg.working
                && now.saturating_duration_since(last_output) >= self.cfg.working;
            if !quiet && now < self.enter_at + self.cfg.working * 3 {
                return WaitStep::Continue;
            }
            self.phase = WaitPhase::Stall { quiet_at: now };
        }
        if let WaitPhase::Stall { quiet_at } = self.phase {
            if last_output <= quiet_at {
                return if now.saturating_duration_since(quiet_at) >= self.cfg.stall {
                    WaitStep::Stalled
                } else {
                    WaitStep::Continue
                };
            }
            self.phase = WaitPhase::Settle {
                left_working_at: None,
            };
        }
        if let WaitPhase::Settle { left_working_at } = &mut self.phase {
            if state == AgentState::Working {
                *left_working_at = None;
                return WaitStep::Continue;
            }
            let left = *left_working_at.get_or_insert(now);
            if now.saturating_duration_since(left) >= self.cfg.settle {
                return WaitStep::Settled(state);
            }
        }
        WaitStep::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole policy for a pane nobody is typing into, in the order the
    /// submitter asks it then. `None` means type it.
    fn decide(
        pane: PaneId,
        live: bool,
        agent: bool,
        state: Option<AgentState>,
        rule: &SubmitRule,
        deliver: Deliver,
    ) -> Option<Answer> {
        precheck(pane, live, agent, state, deliver)
            .or_else(|| on_working(pane, state, rule, deliver))
    }

    #[test]
    fn the_working_policy_waits_for_the_echo_of_a_recent_key() {
        let now = std::time::Instant::now();
        let working = Duration::from_millis(500);
        assert!(working_first(None, now, working), "nobody typed");
        assert!(
            !working_first(Some(now - Duration::from_millis(100)), now, working),
            "a key 100 ms ago: Working may be its echo"
        );
        assert!(working_first(
            Some(now - Duration::from_millis(500)),
            now,
            working
        ));
    }

    #[test]
    fn bracketed_encoding_wraps_the_body_and_keeps_enter_outside() {
        let (text, enter) = encode("hi\nthere", Framing::Bracketed, true, false, b"\r", None);
        assert_eq!(text, b"\x1b[200~hi\nthere\x1b[201~");
        assert_eq!(enter, b"\r");
        assert!(!text.ends_with(b"\r"));
    }

    #[test]
    fn plain_encoding_is_the_body_alone() {
        let (text, enter) = encode("hi", Framing::Plain, true, false, b"\r", None);
        assert_eq!(text, b"hi");
        assert_eq!(enter, b"\r");
    }

    #[test]
    fn focus_in_precedes_the_text() {
        let (text, _) = encode("hi", Framing::Bracketed, true, true, b"\r", None);
        assert_eq!(text, b"\x1b[I\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn a_custom_enter_is_used_verbatim() {
        let (_, enter) = encode("hi", Framing::Plain, true, false, b"\n", None);
        assert_eq!(enter, b"\n");
    }

    #[test]
    fn the_envelope_names_the_message_its_sender_its_mark_and_the_reply() {
        assert_eq!(
            envelope(7, 1, Some("lead"), "5eed"),
            Envelope {
                open: "# [remux message #7 from lead (%1), mark 5eed; reply: remux msg send %1 --reply-to 7]".into(),
                close: "# [end of remux message #7, mark 5eed]".into(),
            }
        );
        assert_eq!(
            envelope(8, 3, None, "5eed").open,
            "# [remux message #8 from %3, mark 5eed; reply: remux msg send %3 --reply-to 8]"
        );
        assert_eq!(
            envelope(9, 3, Some("a\u{1b}b"), "5eed").open,
            "# [remux message #9 from a\\x1bb (%3), mark 5eed; reply: remux msg send %3 --reply-to 9]"
        );
    }

    fn env() -> Envelope {
        Envelope {
            open: "[env]".into(),
            close: "[end]".into(),
        }
    }

    #[test]
    fn the_envelope_opens_and_closes_the_paste_on_lines_of_their_own() {
        let (text, _) = encode("hi", Framing::Bracketed, true, false, b"\r", Some(&env()));
        assert_eq!(text, b"\x1b[200~[env]\nhi\n[end]\x1b[201~");
    }

    #[test]
    fn a_forced_wrap_keeps_the_envelope_on_the_bodys_line() {
        let (text, _) = encode("hi", Framing::Bracketed, false, false, b"\r", Some(&env()));
        assert_eq!(text, "\x1b[200~[env] \u{2014} hi [end]\x1b[201~".as_bytes());
    }

    #[test]
    fn plain_text_keeps_the_envelope_on_the_bodys_line() {
        let (text, _) = encode("hi", Framing::Plain, true, false, b"\r", Some(&env()));
        assert_eq!(text, "[env] \u{2014} hi [end]".as_bytes());
        assert!(!text.contains(&b'\n'));
    }

    fn policy(toml: &str) -> SubmitPolicy {
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        SubmitPolicy::from_config(&cfg.messaging)
    }

    #[test]
    fn shipped_rows_match_the_spec_table() {
        let p = policy("");
        assert_eq!(p.rule_for("claude").while_working, WhileWorking::Queue);
        assert!(p.rule_for("copilot").focus_in);
        assert_eq!(p.rule_for("copilot").while_working, WhileWorking::Mailbox);
        assert_eq!(p.rule_for("codex").while_working, WhileWorking::Mailbox);
        assert_eq!(p.rule_for("anything-else"), SubmitRule::default());
        assert_eq!(
            SubmitRule::default(),
            SubmitRule {
                bracketed: Bracketed::Auto,
                enter: b"\r".to_vec(),
                enter_delay: Duration::from_millis(300),
                focus_in: false,
                while_working: WhileWorking::Mailbox,
                envelope: true,
            }
        );
    }

    #[test]
    fn a_partial_user_row_keeps_the_shipped_fields() {
        let p = policy("[messaging.submit.claude]\nenter_delay_ms = 500\nenvelope = false");
        let claude = p.rule_for("claude");
        assert_eq!(claude.while_working, WhileWorking::Queue);
        assert_eq!(claude.enter_delay, Duration::from_millis(500));
        assert!(!claude.envelope);
        assert!(p.rule_for("codex").envelope, "another row changed");
    }

    #[test]
    fn the_enter_delay_is_clamped_to_its_range() {
        let p = policy(
            "[messaging.submit.a]\nenter_delay_ms = 12000\n[messaging.submit.b]\nenter_delay_ms = 0\n[messaging.submit.c]\nenter_delay_ms = 700",
        );
        assert_eq!(p.rule_for("a").enter_delay, Duration::from_millis(5000));
        assert_eq!(p.rule_for("b").enter_delay, Duration::from_millis(50));
        assert_eq!(p.rule_for("c").enter_delay, Duration::from_millis(700));
    }

    #[test]
    fn a_bad_enter_falls_back_to_the_default() {
        let p = policy("[messaging.submit.a]\nenter = \"\"\n[messaging.submit.b]\nenter = \"\\n\"");
        assert_eq!(p.rule_for("a").enter, b"\r");
        assert_eq!(p.rule_for("b").enter, b"\n");
    }

    #[test]
    fn decide_orders_foreground_blocked_working() {
        use AgentState::{Idle, NeedsInput, Working};
        use Deliver::{Auto, Submit};
        let rule = |while_working| SubmitRule {
            while_working,
            ..SubmitRule::default()
        };
        let queue = rule(WhileWorking::Queue);
        let mailbox = rule(WhileWorking::Mailbox);
        let refuse = rule(WhileWorking::Refuse);
        let code = |d: Option<Answer>| match d {
            None => "inject".to_string(),
            Some(Answer::Mailbox(reason)) => format!("mailbox:{reason}"),
            Some(Answer::Refuse(r)) => format!("refuse:{}", r.code),
            Some(Answer::TimedOut(reason)) => format!("timed_out:{reason}"),
        };
        for (live, agent, state, rule, deliver, want) in [
            (
                false,
                true,
                Some(Idle),
                &queue,
                Auto,
                "refuse:no_such_target",
            ),
            (
                false,
                true,
                Some(Idle),
                &queue,
                Submit,
                "refuse:no_such_target",
            ),
            (true, false, None, &queue, Submit, "refuse:agent_not_ready"),
            (true, false, None, &queue, Auto, "mailbox:agent_not_ready"),
            (
                true,
                true,
                Some(NeedsInput),
                &queue,
                Submit,
                "refuse:agent_blocked",
            ),
            (
                true,
                true,
                Some(NeedsInput),
                &queue,
                Auto,
                "mailbox:agent_blocked",
            ),
            (true, true, Some(Working), &queue, Submit, "inject"),
            (
                true,
                true,
                Some(Working),
                &mailbox,
                Submit,
                "mailbox:agent_working",
            ),
            (
                true,
                true,
                Some(Working),
                &mailbox,
                Auto,
                "mailbox:agent_working",
            ),
            (
                true,
                true,
                Some(Working),
                &refuse,
                Submit,
                "refuse:agent_working",
            ),
            (
                true,
                true,
                Some(Working),
                &refuse,
                Auto,
                "mailbox:agent_working",
            ),
            (true, true, Some(Idle), &refuse, Submit, "inject"),
        ] {
            assert_eq!(
                code(decide(5, live, agent, state, rule, deliver)),
                want,
                "{live} {agent} {state:?} {:?} {deliver:?}",
                rule.while_working
            );
        }
    }

    fn guard() -> GuardConfig {
        GuardConfig {
            attended: Duration::from_millis(3000),
            unattended: Duration::from_millis(500),
            max: Duration::from_millis(15000),
        }
    }

    #[test]
    fn guard_is_ready_when_nobody_typed() {
        let t0 = std::time::Instant::now();
        assert_eq!(
            guard_wait(None, true, t0, t0, &guard()),
            GuardVerdict::Ready
        );
    }

    #[test]
    fn guard_waits_the_attended_window() {
        let t0 = std::time::Instant::now();
        let last = t0;
        let at = |ms| t0 + Duration::from_millis(ms);
        assert_eq!(
            guard_wait(Some(last), true, t0, at(1000), &guard()),
            GuardVerdict::Wait(at(3000))
        );
        assert_eq!(
            guard_wait(Some(last), true, t0, at(3000), &guard()),
            GuardVerdict::Ready
        );
    }

    #[test]
    fn guard_waits_the_short_window_when_unattended() {
        let t0 = std::time::Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        assert_eq!(
            guard_wait(Some(t0), false, t0, at(200), &guard()),
            GuardVerdict::Wait(at(500))
        );
        assert_eq!(
            guard_wait(Some(t0), false, t0, at(500), &guard()),
            GuardVerdict::Ready
        );
    }

    #[test]
    fn guard_expires_at_the_max() {
        let t0 = std::time::Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        // A human typing every 100 ms keeps the window open; the max ends it.
        assert_eq!(
            guard_wait(Some(at(14_900)), true, t0, at(15_000), &guard()),
            GuardVerdict::Expired
        );
        assert_eq!(
            guard_wait(Some(at(14_900)), true, t0, at(14_950), &guard()),
            GuardVerdict::Wait(at(17_900))
        );
    }

    #[test]
    fn a_numbered_selector_with_a_marker_or_an_esc_option_looks_like_a_prompt() {
        let lines = |ls: &[&str]| ls.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(looks_like_prompt(&lines(&[
            "Edit file",
            " src/x.rs",
            "Do you want to make this edit to x.rs?",
            "\u{276f} 1. Yes",
            "  2. Yes, allow all edits during this session (shift+tab)",
            "  3. No, and tell Claude what to do differently (esc)",
        ])));
        assert!(
            looks_like_prompt(&lines(&[
                "  1. Yes",
                "  2. No, and tell Codex what to do differently (Esc)"
            ])),
            "no marker, an Esc option"
        );
        assert!(
            looks_like_prompt(&lines(&["\u{203a} 1) Yes", "  2) No"])),
            "a marker, no Esc option"
        );
        assert!(
            looks_like_prompt(&lines(&[
                "Would you like to proceed?",
                "",
                "\u{276f} 1. Yes, and auto-accept edits",
                "  2. Yes, and manually approve edits",
                "  3. No, keep planning",
                "",
                "ctrl+g to edit in vim",
            ])),
            "a footer under the selector"
        );
        assert!(
            !looks_like_prompt(&lines(&["> 1. fix X", "  2. add tests", "", "Done: both."])),
            "a past prompt of the user's, drawn with >"
        );
        assert!(
            !looks_like_prompt(&lines(&[
                "\u{276f} 1. Yes",
                "  2. No (esc)",
                "Applied.",
                "Next steps:",
                "1. Run the tests",
                "2. Open a PR",
            ])),
            "an answered selector above a later numbered list"
        );
        assert!(
            !looks_like_prompt(&lines(&["\u{276f} 1. Yes", "between", "  2. No (esc)"])),
            "options that are not on consecutive lines"
        );
        assert!(
            !looks_like_prompt(&lines(&["1. Fix the parser", "2. Add the tests"])),
            "a plain numbered list"
        );
        assert!(
            !looks_like_prompt(&lines(&["\u{276f} 2. Yes, and don't ask again"])),
            "one row is not a selector"
        );
        assert!(
            !looks_like_prompt(&lines(&["\u{276f} 1.5x faster", "  2.0 released"])),
            "a number is not an option without a space after its dot"
        );
        assert!(!looks_like_prompt(&lines(&[
            "\u{23fa} User answered Claude's questions:",
            "  \u{23bf}  \u{b7} Which color do you like? \u{2192} Blue",
        ])));
    }

    #[test]
    fn the_probe_is_the_opener_wrapped_and_a_space_ahead_of_plain_text() {
        let (text, _) = encode("hi", Framing::Bracketed, true, true, b"\r", None);
        let (probe, rest) = split_probe(text.clone(), Framing::Bracketed, true);
        assert_eq!(probe, [FOCUS_IN, PASTE_OPEN].concat());
        assert_eq!([probe, rest].concat(), text, "nothing added, nothing lost");
        let (text, _) = encode("hi", Framing::Bracketed, true, false, b"\r", None);
        assert_eq!(split_probe(text, Framing::Bracketed, false).0, PASTE_OPEN);
        let (text, _) = encode("hi", Framing::Plain, false, true, b"\r", None);
        let (probe, rest) = split_probe(text.clone(), Framing::Plain, true);
        assert_eq!(probe, [FOCUS_IN, b" "].concat());
        assert_eq!(rest, text[FOCUS_IN.len()..]);
        let (text, _) = encode("hi", Framing::Plain, false, false, b"\r", None);
        assert_eq!(
            split_probe(text.clone(), Framing::Plain, false),
            (b" ".to_vec(), text)
        );
    }

    #[test]
    fn a_chunk_holds_at_most_one_line_and_starts_with_its_newline() {
        let chunks = split_chunks(b"# env\nline one\n\nlast", PASTE_CLOSE, 1024);
        let want: Vec<&[u8]> = vec![b"# env", b"\nline one", b"\n", b"\nlast\x1b[201~"];
        assert_eq!(chunks, want);
        let long = vec![b'x'; 2500];
        let chunks = split_chunks(&long, b"", 1024);
        assert_eq!(
            chunks.iter().map(Vec::len).collect::<Vec<_>>(),
            [1024, 1024, 452],
            "a line longer than a chunk is cut at the size"
        );
        assert_eq!(split_chunks(b"", b"", 1024), vec![Vec::<u8>::new()]);
        assert_eq!(
            split_chunks(&long, PASTE_CLOSE, 1024)
                .last()
                .map(|c| c.ends_with(PASTE_CLOSE)),
            Some(true),
            "the closer is never cut"
        );
        for chunk in split_chunks(b"a\nb\nc", b"", 1024) {
            assert!(
                chunk.iter().skip(1).all(|&b| b != b'\n'),
                "a newline only ever opens a chunk: {chunk:?}"
            );
        }
    }

    #[test]
    fn defer_waits_only_for_an_agent_waiting_on_its_user() {
        use AgentState::{Idle, NeedsInput, Working};
        assert!(waits_while_blocked(true, true, Some(NeedsInput), true));
        assert!(!waits_while_blocked(true, true, Some(NeedsInput), false));
        assert!(!waits_while_blocked(true, true, Some(Idle), true));
        assert!(!waits_while_blocked(true, true, Some(Working), true));
        assert!(
            !waits_while_blocked(true, false, None, true),
            "a shell is answered agent_not_ready at once, deferred or not"
        );
        assert!(
            !waits_while_blocked(false, true, Some(NeedsInput), true),
            "a pane that closed"
        );
    }

    #[test]
    fn an_entry_out_of_time_is_refused_or_reported_timed_out() {
        let code = |a: Answer| match a {
            Answer::Refuse(r) => format!("refuse:{}", r.code),
            Answer::Mailbox(reason) => format!("mailbox:{reason}"),
            Answer::TimedOut(reason) => format!("timed_out:{reason}"),
        };
        assert_eq!(
            code(overdue(Deliver::Submit, 5, 9, Some("agent_blocked"))),
            "refuse:submit_timeout"
        );
        assert_eq!(
            code(overdue(Deliver::Auto, 5, 9, Some("agent_blocked"))),
            "timed_out:agent_blocked"
        );
        assert_eq!(
            code(overdue(Deliver::Auto, 5, 9, None)),
            "timed_out:submit_timeout",
            "never a head, so it waited on nothing of its own"
        );
        match overdue(Deliver::Submit, 5, 9, None) {
            Answer::Refuse(r) => {
                assert!(r.message.contains("#9") && r.message.contains("in the mailbox"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_multi_line_body_is_refused_plain() {
        assert_eq!(
            multi_line_refusal("one\ntwo").map(|r| r.code),
            Some("bad_body")
        );
        assert_eq!(multi_line_refusal("one line"), None);
        assert!(newlines_safe(Framing::Bracketed, true));
        assert!(!newlines_safe(Framing::Bracketed, false));
        assert!(!newlines_safe(Framing::Plain, true));
    }

    const WORKING_MS: u64 = 500;

    fn wait_cfg() -> WaitCfg {
        WaitCfg {
            working: Duration::from_millis(WORKING_MS),
            stall: Duration::from_millis(2000),
            settle: Duration::from_millis(500),
        }
    }

    /// Step a machine every 50 ms from the Enter, the way the submitter
    /// samples a pane. The pane printed at `outputs` (ms after the Enter), and
    /// reads `Working` within `WORKING_MS` of its last output and `rest`
    /// after. Its last output before the Enter was 100 ms before it. Returns
    /// the machine's answer and when it gave it, in ms after the Enter.
    fn run_wait(
        outputs: &[u64],
        rest: AgentState,
        deadline_ms: u64,
        machine: impl FnOnce(std::time::Instant, std::time::Instant) -> WaitMachine,
    ) -> (WaitStep, u64) {
        let base = std::time::Instant::now();
        let enter = base + Duration::from_millis(100);
        let at = |ms: u64| enter + Duration::from_millis(ms);
        let mut m = machine(enter, at(deadline_ms));
        for ms in (0..=20_000).step_by(50) {
            let last = outputs
                .iter()
                .filter(|&&o| o <= ms)
                .max()
                .map_or(base, |&o| at(o));
            let state = if at(ms).duration_since(last) < Duration::from_millis(WORKING_MS) {
                AgentState::Working
            } else {
                rest
            };
            match m.step(at(ms), last, state) {
                WaitStep::Continue => {}
                done => return (done, ms),
            }
        }
        panic!("the machine never answered");
    }

    fn wait(outputs: &[u64], rest: AgentState, deadline_ms: u64) -> (WaitStep, u64) {
        run_wait(outputs, rest, deadline_ms, |enter, deadline| {
            WaitMachine::new(enter, deadline, wait_cfg())
        })
    }

    /// An agent that starts printing as soon as it has the prompt and prints
    /// until its turn ends never goes quiet in between. With no cap on the
    /// quiet point it would only go quiet once its work was done, and the
    /// stall that follows would call a finished turn stalled.
    #[test]
    fn a_fast_continuous_agent_settles_idle() {
        let outputs: Vec<u64> = (50..=2050).step_by(100).collect();
        let (step, at) = wait(&outputs, AgentState::Idle, 60_000);
        assert_eq!(step, WaitStep::Settled(AgentState::Idle));
        assert!((3000..=3200).contains(&at), "settled at {at} ms");
    }

    #[test]
    fn a_silent_agent_stalls_after_the_quiet_point() {
        let (step, at) = wait(&[], AgentState::Idle, 60_000);
        assert_eq!(step, WaitStep::Stalled);
        assert_eq!(at, 500 + 2000, "quiet at once, then the whole stall");
    }

    #[test]
    fn the_echo_is_not_the_agents_output() {
        let (step, at) = wait(&[10], AgentState::Idle, 60_000);
        assert_eq!(step, WaitStep::Stalled);
        assert_eq!(
            at,
            550 + 2000,
            "quiet once the echo is a working period old"
        );
    }

    #[test]
    fn needs_input_at_settle_is_an_answer() {
        let (step, at) = wait(&[1000], AgentState::NeedsInput, 60_000);
        assert_eq!(step, WaitStep::Settled(AgentState::NeedsInput));
        assert_eq!(
            at,
            1500 + 500,
            "out of Working at 1500, held for the settle"
        );
    }

    #[test]
    fn working_again_resets_the_settle_hold() {
        // Two bursts, the gap between them reading Idle from 2450 to 2700,
        // shorter than the settle: the turn is not over in the gap, and the
        // hold starts again after the second burst.
        let mut outputs: Vec<u64> = (50..=1950).step_by(100).collect();
        outputs.extend((2700..=3500).step_by(100));
        let (step, at) = wait(&outputs, AgentState::Idle, 60_000);
        assert_eq!(step, WaitStep::Settled(AgentState::Idle));
        assert_eq!(at, 4000 + 500, "held from the end of the second burst");
    }

    #[test]
    fn the_deadline_wins_in_every_phase() {
        let busy: Vec<u64> = (50..=5050).step_by(100).collect();
        assert_eq!(
            wait(&busy, AgentState::Idle, 300),
            (WaitStep::TimedOut(AgentState::Working), 300),
            "while the echo is watched"
        );
        assert_eq!(
            wait(&[], AgentState::Idle, 1000),
            (WaitStep::TimedOut(AgentState::Idle), 1000),
            "while a stall is watched"
        );
        assert_eq!(
            wait(&busy, AgentState::Idle, 2000),
            (WaitStep::TimedOut(AgentState::Working), 2000),
            "while the settle is watched"
        );
        assert_eq!(
            wait(&[1000], AgentState::NeedsInput, 1700),
            (WaitStep::TimedOut(AgentState::NeedsInput), 1700),
            "while a settle is held"
        );
    }

    /// A turn that ends before the quiet point leaves no output after it, so
    /// it reads as a stall: the machine cannot tell such a turn from the
    /// prompt's own echo and redraw.
    #[test]
    fn a_turn_inside_the_echo_window_is_a_known_gap() {
        let outputs: Vec<u64> = (50..=1050).step_by(100).collect();
        let (step, _) = wait(&outputs, AgentState::NeedsInput, 60_000);
        assert_eq!(step, WaitStep::Stalled);
    }
}
