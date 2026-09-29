//! Typing a message into an agent's pane: how its bytes are framed.
//!
//! A message is typed as two writes. The first is the body, wrapped in
//! bracketed paste when the pane's program has asked for it (mode 2004), so
//! the agent reads it as one pasted block rather than as keystrokes whose
//! newlines would each submit a line. The second is the Enter, written after a
//! delay: an agent that reads a paste and an Enter in one read can take the
//! Enter as part of the paste, and the paste as not yet submitted. The Enter
//! is never inside the wrap for the same reason.
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
//! Each typed message opens with an envelope line naming it and its sender,
//! so the agent, and the person watching it, can tell another agent's
//! message from its user's.
//!
//! Residuals:
//! - The writer drains in order, so a key a human presses between the two
//!   writes lands between the text and the Enter and is submitted with the
//!   message. Nothing can un-type the text once it is written.
//! - A key a human presses in those 300 ms that the agent has not read yet
//!   makes the check before the Enter see unread input. The server then
//!   flushes it and leaves the message in the mailbox, although the agent
//!   read the text; the message is delivered again by the mailbox.

pub const PASTE_OPEN: &[u8] = b"\x1b[200~";
pub const PASTE_CLOSE: &[u8] = b"\x1b[201~";
pub const FOCUS_IN: &[u8] = b"\x1b[I";

use crate::protocol::PaneId;

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
}

/// The line typed before a message's body: which message it is, who sent it
/// and how to answer. `from_name` is a validated agent name, escaped all the
/// same because the line reaches a terminal.
pub fn envelope(id: crate::protocol::MsgId, from_pane: PaneId, from_name: Option<&str>) -> String {
    let from = match from_name {
        Some(name) => format!("{} (%{from_pane})", crate::client::cli_common::escape(name)),
        None => format!("%{from_pane}"),
    };
    format!("[remux message #{id} from {from}; reply: remux msg send %{from_pane} --reply-to {id}]")
}

/// The two writes for `body`: the text, with its wrap and `focus_in` first
/// when asked, and the Enter, which the caller writes separately after its
/// delay. The envelope opens the text: on a line of its own inside a wrap,
/// and before the body on the same line when plain, where a newline would
/// submit the envelope alone.
pub fn encode(
    body: &str,
    framing: Framing,
    focus_in: bool,
    enter: &[u8],
    envelope: Option<&str>,
) -> (Vec<u8>, Vec<u8>) {
    let mut text = Vec::with_capacity(body.len() + 128);
    if focus_in {
        text.extend_from_slice(FOCUS_IN);
    }
    match framing {
        Framing::Bracketed => {
            text.extend_from_slice(PASTE_OPEN);
            if let Some(line) = envelope {
                text.extend_from_slice(line.as_bytes());
                text.push(b'\n');
            }
            text.extend_from_slice(body.as_bytes());
            text.extend_from_slice(PASTE_CLOSE);
        }
        Framing::Plain => {
            if let Some(line) = envelope {
                text.extend_from_slice(line.as_bytes());
                text.extend_from_slice(" \u{2014} ".as_bytes());
            }
            text.extend_from_slice(body.as_bytes());
        }
    }
    (text, enter.to_vec())
}

/// Why `body` cannot be typed unwrapped: each newline would submit a line of
/// it on its own.
pub fn plain_refusal(body: &str) -> Option<SubmitRefusal> {
    body.contains('\n').then(|| SubmitRefusal {
        code: "bad_body",
        message: "the pane has bracketed paste off, so a message of more than one line cannot be typed into it; it is in the mailbox".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_encoding_wraps_the_body_and_keeps_enter_outside() {
        let (text, enter) = encode("hi\nthere", Framing::Bracketed, false, b"\r", None);
        assert_eq!(text, b"\x1b[200~hi\nthere\x1b[201~");
        assert_eq!(enter, b"\r");
        assert!(!text.ends_with(b"\r"));
    }

    #[test]
    fn plain_encoding_is_the_body_alone() {
        let (text, enter) = encode("hi", Framing::Plain, false, b"\r", None);
        assert_eq!(text, b"hi");
        assert_eq!(enter, b"\r");
    }

    #[test]
    fn focus_in_precedes_the_text() {
        let (text, _) = encode("hi", Framing::Bracketed, true, b"\r", None);
        assert_eq!(text, b"\x1b[I\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn a_custom_enter_is_used_verbatim() {
        let (_, enter) = encode("hi", Framing::Plain, false, b"\n", None);
        assert_eq!(enter, b"\n");
    }

    #[test]
    fn the_envelope_names_the_message_its_sender_and_the_reply() {
        assert_eq!(
            envelope(7, 1, Some("lead")),
            "[remux message #7 from lead (%1); reply: remux msg send %1 --reply-to 7]"
        );
        assert_eq!(
            envelope(8, 3, None),
            "[remux message #8 from %3; reply: remux msg send %3 --reply-to 8]"
        );
        assert_eq!(
            envelope(9, 3, Some("a\u{1b}b")),
            "[remux message #9 from a\\x1bb (%3); reply: remux msg send %3 --reply-to 9]"
        );
    }

    #[test]
    fn the_envelope_opens_the_paste_on_a_line_of_its_own() {
        let (text, _) = encode("hi", Framing::Bracketed, false, b"\r", Some("[env]"));
        assert_eq!(text, b"\x1b[200~[env]\nhi\x1b[201~");
    }

    #[test]
    fn plain_text_keeps_the_envelope_on_the_bodys_line() {
        let (text, _) = encode("hi", Framing::Plain, false, b"\r", Some("[env]"));
        assert_eq!(text, "[env] \u{2014} hi".as_bytes());
        assert!(!text.contains(&b'\n'));
    }

    #[test]
    fn a_multi_line_body_is_refused_plain() {
        assert_eq!(plain_refusal("one\ntwo").map(|r| r.code), Some("bad_body"));
        assert_eq!(plain_refusal("one line"), None);
    }
}
