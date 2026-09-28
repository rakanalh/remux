//! `/proc/<pid>/stat`, shared by sender attribution and agent detection.

/// The fields of a `/proc/<pid>/stat` line that the server reads.
#[derive(Debug, PartialEq, Eq)]
pub struct StatFields {
    pub ppid: i32,
    pub pgrp: i32,
    pub sid: i32,
    pub tty_nr: u32,
}

/// Parse a `/proc/<pid>/stat` line: `pid (comm) state ppid pgrp session tty_nr ...`.
///
/// **Parsed from the LAST `)`, never by splitting the line.** `comm` is up to
/// 15 bytes of whatever the process last called itself, written raw and
/// unescaped -- it may contain spaces, parentheses or both, and a Node worker
/// thread or a browser helper routinely does. Counting whitespace fields from
/// the start, or stopping at the first `)`, reads some byte of the NAME as a
/// number; for the pgid that answer matches no real group, so every descendant
/// would be filtered out and the agent walk would quietly stop finding
/// anything. A line that does not have the shape is refused rather than
/// guessed at.
///
/// Taken as bytes because the 15-byte truncation can cut a multi-byte
/// character in half. Only the numeric tail after the `)` is decoded.
pub fn proc_stat(line: &[u8]) -> Option<StatFields> {
    let close = line.iter().rposition(|&b| b == b')')?;
    let tail = std::str::from_utf8(&line[close + 1..]).ok()?;
    let mut fields = tail.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;
    let pgrp = fields.next()?.parse().ok()?;
    let sid = fields.next()?.parse().ok()?;
    let tty_nr = fields.next()?.parse::<i32>().ok()? as u32;
    Some(StatFields {
        ppid,
        pgrp,
        sid,
        tty_nr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pgrp(line: &[u8]) -> Option<i32> {
        proc_stat(line).map(|stat| stat.pgrp)
    }

    #[test]
    fn proc_stat_parses_from_the_last_paren() {
        let line = b"1234 (a) b) c) S 1000 1234 1001 34816 1234 4194560 0 0";
        assert_eq!(
            proc_stat(line.as_slice()),
            Some(StatFields {
                ppid: 1000,
                pgrp: 1234,
                sid: 1001,
                tty_nr: 34816
            })
        );
        let spaced = b"77 (Web Content) S 2 77 3 0 -1";
        assert_eq!(
            proc_stat(spaced.as_slice()),
            Some(StatFields {
                ppid: 2,
                pgrp: 77,
                sid: 3,
                tty_nr: 0
            })
        );
        assert_eq!(proc_stat(b"1234 no paren S 1 2 3 4"), None);
        assert_eq!(proc_stat(b"1234 (short) S 1 2 3 x"), None);
        assert_eq!(proc_stat(b"1234 (short) S 1 2"), None);
    }

    #[test]
    fn proc_stat_reads_a_comm_that_is_not_utf8() {
        // A 15-byte `comm` truncated inside a three-byte character.
        let mut line = b"4321 (build-\xe2\x82 x) S 10 4321 11 34816 1".to_vec();
        assert!(std::str::from_utf8(&line).is_err());
        assert_eq!(
            proc_stat(&line),
            Some(StatFields {
                ppid: 10,
                pgrp: 4321,
                sid: 11,
                tty_nr: 34816
            })
        );
        line.extend_from_slice(b" \xff");
        assert_eq!(proc_stat(&line), None);
    }

    /// `/proc/<pid>/stat`, as the kernel writes it: pid, comm in parentheses,
    /// then state, ppid and pgrp.
    #[test]
    fn the_stat_line_gives_up_its_process_group() {
        // state R, ppid 693892, pgrp 693891 -- the THIRD field after the
        // comm, checked against `ps -o pgid=` on a live process.
        assert_eq!(
            pgrp(b"693899 (rtk) R 693892 693891 693890 0 -1 4194304 501 0"),
            Some(693891)
        );
    }

    /// **The parse starts at the LAST `)`, and that is the whole of it.**
    ///
    /// `comm` is up to 15 bytes of whatever the process called itself, and it
    /// is not escaped or quoted: a Node worker thread, a browser tab or
    /// anything else may put a space, a parenthesis or both in there. Splitting
    /// on whitespace, or on the FIRST `)`, reads the wrong field -- and the
    /// wrong field is a pgid that matches nothing, so every descendant is
    /// filtered out and the walk silently stops finding anything.
    #[test]
    fn a_comm_containing_spaces_and_a_paren_does_not_move_the_fields() {
        assert_eq!(pgrp(b"42 (we ) ird) S 7 1234 99 0 -1 0 0 0"), Some(1234));
    }

    #[test]
    fn a_comm_that_is_not_utf8_does_not_hide_the_process_group() {
        assert_eq!(
            pgrp(b"42 (build-\xe2\x82 x) S 7 1234 99 0 -1 0 0 0"),
            Some(1234)
        );
    }

    #[test]
    fn a_stat_line_that_is_not_one_is_refused_rather_than_guessed() {
        assert_eq!(pgrp(b""), None);
        assert_eq!(pgrp(b"42 (sh"), None);
        assert_eq!(pgrp(b"42 (sh) S 7"), None);
        assert_eq!(pgrp(b"42 (sh) S 7 notanumber"), None);
    }
}
