//! Showing text someone else wrote on a terminal: every control and
//! invisible character as a visible escape. Used by the CLI for every
//! field it prints and by the server for the text it types
//! (`mail_render`), so it belongs to neither.

use crate::protocol::hidden_chars;

/// `s` with every control character, and every invisible character that
/// [`hidden_chars`] does not allow where it stands, shown as an escape
/// instead of emitted: `\x1b` for C0 and DEL, `\u{9b}` for C1 and
/// `\u{202e}` for an invisible one. Newlines included, so a field printed on
/// one line stays on one line. An allowed joiner or variation selector is
/// printed as itself, so an emoji shows as an emoji.
pub(crate) fn escape(s: &str) -> String {
    escape_text(s, false)
}

/// [`escape`] of `s` in at most `max` characters, `...` included when it
/// had to be cut. Each character's escape is kept whole or dropped whole,
/// so a cut never leaves a partial escape such as `\x1` at the end.
pub(crate) fn escape_bounded(s: &str, max: usize) -> String {
    // The input is the far side's to size. Past `max * 2` characters it is
    // cut whatever follows, so the rest is never escaped or copied.
    let s = match s.char_indices().nth(max.saturating_mul(2)) {
        Some((at, _)) => &s[..at],
        None => s,
    };
    let full = escape(s);
    if full.chars().count() <= max {
        return full;
    }
    let budget = max.saturating_sub(3);
    let mut out = String::new();
    let mut used = 0;
    escape_each(s, false, |piece| {
        let len = piece.chars().count();
        if used + len > budget {
            return false;
        }
        used += len;
        out.push_str(piece);
        true
    });
    out.push_str("...");
    out
}

/// [`escape`], except that a tab is printed as a tab. For a body line: a tab
/// moves the cursor but cannot start a line, and the indentation is what
/// stops a body forging a header.
pub(crate) fn escape_keeping_tabs(s: &str) -> String {
    escape_text(s, true)
}

/// The escaping for [`escape`] and [`escape_keeping_tabs`], over the whole of
/// `s` at once so that each character is judged by the same neighbours the
/// server judged it by.
fn escape_text(s: &str, keep_tabs: bool) -> String {
    let mut out = String::with_capacity(s.len());
    escape_each(s, keep_tabs, |piece| {
        out.push_str(piece);
        true
    });
    out
}

/// Hand `emit` each character of `s` as it is printed, an escape or the
/// character itself, until `emit` returns false.
fn escape_each(s: &str, keep_tabs: bool, mut emit: impl FnMut(&str) -> bool) {
    let hidden: Vec<usize> = hidden_chars(s).map(|(at, _)| at).collect();
    let mut plain = [0u8; 4];
    for (at, c) in s.char_indices() {
        let escaped = match c as u32 {
            0x09 if keep_tabs => None,
            0x00..=0x1f | 0x7f => Some(format!("\\x{:02x}", c as u32)),
            0x80..=0x9f => Some(format!("\\u{{{:x}}}", c as u32)),
            _ if hidden.binary_search(&at).is_ok() => Some(format!("\\u{{{:x}}}", c as u32)),
            _ => None,
        };
        let piece = match &escaped {
            Some(escaped) => escaped.as_str(),
            None => c.encode_utf8(&mut plain),
        };
        if !emit(piece) {
            return;
        }
    }
}

/// [`escape`] applied line by line, for text whose line breaks are layout:
/// clap's usage errors, which quote what was typed.
pub(crate) fn escape_lines(s: &str) -> String {
    s.split('\n').map(escape).collect::<Vec<_>>().join("\n")
}
