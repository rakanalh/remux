//! Quoting a word for a POSIX shell, for the two places remux writes a command
//! line: the hook commands it installs and the resume line a restore types.

/// `text` as one `sh` word: single quotes, a quote inside as `'\''`.
pub(crate) fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `text` as one `sh` word, left bare when no character in it is special to
/// `sh`, else as [`sh_quote`] writes it. Not used for the hook commands: an
/// installed command is recognised by comparing it with what [`sh_quote`]
/// renders, so a bare spelling there would make every existing install look
/// foreign.
pub(crate) fn sh_word(text: &str) -> String {
    let bare = !text.is_empty()
        && text.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'_' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b'=' | b',' | b'-'
                )
        });
    if bare {
        text.to_string()
    } else {
        sh_quote(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_word_leaves_the_bare_set_bare_and_quotes_the_rest() {
        for bare in [
            "claude",
            "--resume",
            "--resume=/home/u/s.jsonl",
            "7f3c2a9e-51d4-4b8e-9a61-0c2f5d8e4b17",
            "a_b.c:d@e%f+g=h,i/j-k",
        ] {
            assert_eq!(sh_word(bare), bare);
        }
        assert_eq!(sh_word(""), "''");
        assert_eq!(sh_word("with space"), "'with space'");
        assert_eq!(sh_word("it's"), r"'it'\''s'");
        assert_eq!(sh_word("$HOME"), "'$HOME'");
        assert_eq!(sh_word("a;b"), "'a;b'");
    }
}
