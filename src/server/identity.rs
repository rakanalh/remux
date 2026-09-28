//! Which pane a connecting process runs in, decided from the process itself.
//!
//! The answer names the SENDER of a message. It is never read from the
//! caller's environment: a `$REMUX_PANE` that is stale or overridden is exactly
//! how a message arrives from the wrong pane. It is also not authorisation.
//! The socket's uid check has already decided that every peer here is the
//! user, and a same-uid process that lies can `ptrace` the server anyway.
//!
//! Attribution is a pure function over [`ProcTable`], so each ruling is a unit
//! test with an injected table. The platform adapters only read one process.

use std::collections::HashMap;

use crate::protocol::PaneId;

/// How many parent links the walk follows before it gives up.
///
/// A cost bound, not a correctness bound. A real sender is a few generations
/// below its pane's shell, and a cycle in a table that is read one entry at a
/// time (a pid reused mid-walk) must not loop.
pub const MAX_GENERATIONS: u32 = 64;

/// A controlling terminal as `(major, minor)`.
///
/// Decoded, never compared as a raw integer. Linux `tty_nr` is the kernel's
/// `new_encode_dev` layout and `st_rdev` is the C library's `makedev`. The two
/// encodings agree for every major below 4096 on glibc, but that agreement is
/// a property of one libc and not a contract, so each side is decoded by its
/// own rule and the pairs are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tty {
    pub major: u32,
    pub minor: u32,
}

impl Tty {
    /// Decode the `tty_nr` field of `/proc/<pid>/stat`. Zero means the process
    /// has no controlling terminal.
    #[cfg(target_os = "linux")]
    pub fn from_linux_tty_nr(n: u32) -> Option<Self> {
        if n == 0 {
            return None;
        }
        Some(Tty {
            major: (n >> 8) & 0xfff,
            minor: (n & 0xff) | ((n >> 12) & 0xfff00),
        })
    }

    pub fn from_dev_t(dev: libc::dev_t) -> Self {
        Tty {
            major: libc::major(dev) as u32,
            minor: libc::minor(dev) as u32,
        }
    }
}

/// The three facts attribution needs about one process.
#[derive(Debug, Clone, Copy)]
pub struct ProcInfo {
    pub ppid: i32,
    /// `None` where the platform does not report a session id (macOS).
    pub sid: Option<i32>,
    pub tty: Option<Tty>,
}

pub trait ProcTable {
    /// `None` when the process does not exist or cannot be read.
    fn info(&self, pid: i32) -> Option<ProcInfo>;
}

/// Pane shell pid to the pane and its slave device.
///
/// Built fresh for each attribution under the `panes` lock alone and never
/// cached, because a pane can close between two messages.
pub struct ProcIndex {
    by_shell_pid: HashMap<i32, (PaneId, Option<Tty>)>,
}

impl ProcIndex {
    pub fn new(entries: impl IntoIterator<Item = (i32, PaneId, Option<Tty>)>) -> Self {
        ProcIndex {
            by_shell_pid: entries
                .into_iter()
                .map(|(pid, pane, tty)| (pid, (pane, tty)))
                .collect(),
        }
    }

    fn get(&self, pid: i32) -> Option<(PaneId, Option<Tty>)> {
        self.by_shell_pid.get(&pid).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// The peer's session id is a pane shell's pid.
    Sid,
    /// A pane shell is `depth` parent links above the peer. Depth 0 is the
    /// peer itself.
    Walk { depth: u32 },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Attribution {
    pub pane: PaneId,
    pub how: How,
    /// The peer's controlling terminal is not the pane's slave. This is
    /// expected under a nested pty (`script`, `expect`, a multiplexer inside
    /// the pane), where the ancestry still reaches the pane, so it is reported
    /// and never refused.
    pub tty_differs: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Unverified {
    /// The platform gave no peer pid for the socket.
    NoPeerPid,
    /// Neither the session id nor the parent chain reaches a pane shell.
    NoPaneAncestor,
    /// The peer itself could not be read: it exited, or a pid namespace hides
    /// it from the server.
    Unreadable,
}

impl Unverified {
    pub fn reason(&self) -> &'static str {
        match self {
            Unverified::NoPeerPid => "no peer pid",
            Unverified::NoPaneAncestor => "not a descendant of any pane",
            Unverified::Unreadable => "the peer process could not be read",
        }
    }
}

pub fn attribute(
    peer: Option<i32>,
    procs: &ProcIndex,
    table: &impl ProcTable,
) -> Result<Attribution, Unverified> {
    let peer = peer.ok_or(Unverified::NoPeerPid)?;
    let info = table.info(peer).ok_or(Unverified::Unreadable)?;
    let attributed = |pane, pane_tty: Option<Tty>, how| Attribution {
        pane,
        how,
        tty_differs: matches!((info.tty, pane_tty), (Some(a), Some(b)) if a != b),
    };

    if let Some((pane, pane_tty)) = info.sid.and_then(|sid| procs.get(sid)) {
        return Ok(attributed(pane, pane_tty, How::Sid));
    }

    // The walk starts at the peer itself, not its parent. On macOS there is no
    // sid, so a peer that IS the pane's shell would otherwise walk straight
    // past its own pane.
    let mut pid = peer;
    for depth in 0..=MAX_GENERATIONS {
        if pid <= 1 {
            break;
        }
        if let Some((pane, pane_tty)) = procs.get(pid) {
            return Ok(attributed(pane, pane_tty, How::Walk { depth }));
        }
        if depth == MAX_GENERATIONS {
            break;
        }
        pid = if depth == 0 {
            info.ppid
        } else {
            table.info(pid).ok_or(Unverified::NoPaneAncestor)?.ppid
        };
    }
    Err(Unverified::NoPaneAncestor)
}

/// The fields of a `/proc/<pid>/stat` line that attribution reads.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
pub struct StatFields {
    pub ppid: i32,
    pub sid: i32,
    pub tty_nr: u32,
}

/// Parsed from the LAST `)`, for the reason `daemon::pgid_from_stat` gives:
/// `comm` may contain spaces and parentheses. After it the fields run
/// `state ppid pgrp session tty_nr`.
///
/// Taken as bytes because `comm` is whatever the process named itself,
/// truncated to 15 bytes, which can cut a multi-byte character in half. Only
/// the numeric tail after the `)` is decoded.
#[cfg(target_os = "linux")]
pub fn proc_stat(line: &[u8]) -> Option<StatFields> {
    let close = line.iter().rposition(|&b| b == b')')?;
    let tail = std::str::from_utf8(&line[close + 1..]).ok()?;
    let mut fields = tail.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;
    let _pgrp = fields.next()?;
    let sid = fields.next()?.parse().ok()?;
    let tty_nr = fields.next()?.parse::<i32>().ok()? as u32;
    Some(StatFields { ppid, sid, tty_nr })
}

#[cfg(target_os = "linux")]
pub struct Procfs;

#[cfg(target_os = "linux")]
impl ProcTable for Procfs {
    fn info(&self, pid: i32) -> Option<ProcInfo> {
        if pid <= 0 {
            return None;
        }
        let stat = proc_stat(&std::fs::read(format!("/proc/{pid}/stat")).ok()?)?;
        Some(ProcInfo {
            ppid: stat.ppid,
            sid: Some(stat.sid),
            tty: Tty::from_linux_tty_nr(stat.tty_nr),
        })
    }
}

#[cfg(target_os = "linux")]
pub fn system_table() -> impl ProcTable {
    Procfs
}

/// `proc_bsdinfo` has no session id, so on macOS the walk is the only path.
///
/// `proc_pidinfo` returns the number of bytes it wrote, so a short answer is a
/// failure rather than a half-filled struct.
#[cfg(target_os = "macos")]
pub struct Libproc;

#[cfg(target_os = "macos")]
impl ProcTable for Libproc {
    fn info(&self, pid: i32) -> Option<ProcInfo> {
        if pid <= 0 {
            return None;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let wrote = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if wrote != size {
            return None;
        }
        // XNU reports "no controlling terminal" as NODEV, which is all ones,
        // not as 0.
        let tty = (info.e_tdev != 0 && info.e_tdev != u32::MAX)
            .then(|| Tty::from_dev_t(info.e_tdev as libc::dev_t));
        Some(ProcInfo {
            ppid: info.pbi_ppid as i32,
            sid: None,
            tty,
        })
    }
}

#[cfg(target_os = "macos")]
pub fn system_table() -> impl ProcTable {
    Libproc
}

/// A platform that cannot read another process attributes nothing, so every
/// peer there is unverified rather than guessed at.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub struct NoProcTable;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl ProcTable for NoProcTable {
    fn info(&self, _pid: i32) -> Option<ProcInfo> {
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn system_table() -> impl ProcTable {
    NoProcTable
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTable(HashMap<i32, ProcInfo>);

    impl ProcTable for FakeTable {
        fn info(&self, pid: i32) -> Option<ProcInfo> {
            self.0.get(&pid).copied()
        }
    }

    fn proc(ppid: i32, sid: Option<i32>, tty: Option<Tty>) -> ProcInfo {
        ProcInfo { ppid, sid, tty }
    }

    const PTS0: Tty = Tty {
        major: 136,
        minor: 0,
    };
    const PTS1: Tty = Tty {
        major: 136,
        minor: 1,
    };

    /// The peer (pid 10_000) with `top` exactly `len` parent links above it.
    /// No sid in the chain names a pane.
    fn chain(len: i32, top: i32) -> FakeTable {
        let mut t = HashMap::new();
        for pid in 10_000..10_000 + len {
            let parent = if pid + 1 == 10_000 + len {
                top
            } else {
                pid + 1
            };
            t.insert(pid, proc(parent, Some(4242), None));
        }
        t.insert(top, proc(1, Some(top), None));
        FakeTable(t)
    }

    #[test]
    fn sid_hit_with_the_ppid_chain_broken() {
        let table = FakeTable(HashMap::from([(500, proc(999, Some(100), None))]));
        let procs = ProcIndex::new([(100, 1, None)]);
        assert_eq!(
            attribute(Some(500), &procs, &table),
            Ok(Attribution {
                pane: 1,
                how: How::Sid,
                tty_differs: false
            })
        );
    }

    #[test]
    fn walk_hit_at_depth_3_with_the_sid_pointing_nowhere() {
        let table = FakeTable(HashMap::from([
            (500, proc(501, Some(4242), None)),
            (501, proc(502, Some(4242), None)),
            (502, proc(100, Some(4242), None)),
            (100, proc(1, Some(100), None)),
        ]));
        let procs = ProcIndex::new([(100, 1, None)]);
        assert_eq!(
            attribute(Some(500), &procs, &table),
            Ok(Attribution {
                pane: 1,
                how: How::Walk { depth: 3 },
                tty_differs: false
            })
        );
    }

    #[test]
    fn the_pane_shell_itself_is_attributed_without_a_sid() {
        let table = FakeTable(HashMap::from([(100, proc(7, None, None))]));
        let procs = ProcIndex::new([(100, 1, None)]);
        assert_eq!(
            attribute(Some(100), &procs, &table),
            Ok(Attribution {
                pane: 1,
                how: How::Walk { depth: 0 },
                tty_differs: false
            })
        );
    }

    #[test]
    fn walk_is_capped_at_max_generations() {
        let procs = ProcIndex::new([(100, 1, None)]);
        assert_eq!(
            attribute(Some(10_000), &procs, &chain(MAX_GENERATIONS as i32, 100)),
            Ok(Attribution {
                pane: 1,
                how: How::Walk {
                    depth: MAX_GENERATIONS
                },
                tty_differs: false
            })
        );
        assert_eq!(
            attribute(
                Some(10_000),
                &procs,
                &chain(MAX_GENERATIONS as i32 + 1, 100)
            ),
            Err(Unverified::NoPaneAncestor)
        );
    }

    #[test]
    fn a_daemonized_sender_is_no_pane_ancestor() {
        let table = FakeTable(HashMap::from([(500, proc(1, Some(500), None))]));
        let procs = ProcIndex::new([(100, 1, None)]);
        assert_eq!(
            attribute(Some(500), &procs, &table),
            Err(Unverified::NoPaneAncestor)
        );
    }

    #[test]
    fn an_unreadable_peer_is_unreadable_and_an_unreadable_ancestor_is_no_pane_ancestor() {
        let procs = ProcIndex::new([(100, 1, None)]);
        let empty = FakeTable(HashMap::new());
        assert_eq!(
            attribute(Some(500), &procs, &empty),
            Err(Unverified::Unreadable)
        );
        let broken = FakeTable(HashMap::from([(500, proc(501, Some(4242), None))]));
        assert_eq!(
            attribute(Some(500), &procs, &broken),
            Err(Unverified::NoPaneAncestor)
        );
    }

    #[test]
    fn no_peer_pid_is_unverifiable() {
        let procs = ProcIndex::new([(100, 1, None)]);
        let table = FakeTable(HashMap::from([(100, proc(1, Some(100), None))]));
        assert_eq!(attribute(None, &procs, &table), Err(Unverified::NoPeerPid));
    }

    #[test]
    fn a_differing_tty_is_reported_not_refused() {
        let walk_table = |tty| {
            FakeTable(HashMap::from([
                (500, proc(100, Some(4242), tty)),
                (100, proc(1, Some(100), Some(PTS0))),
            ]))
        };
        let differs = |table: &FakeTable, pane_tty| {
            attribute(Some(500), &ProcIndex::new([(100, 1, pane_tty)]), table)
                .map(|a| a.tty_differs)
        };
        assert_eq!(differs(&walk_table(Some(PTS1)), Some(PTS0)), Ok(true));
        assert_eq!(differs(&walk_table(Some(PTS0)), Some(PTS0)), Ok(false));
        assert_eq!(differs(&walk_table(Some(PTS1)), None), Ok(false));
        assert_eq!(differs(&walk_table(None), Some(PTS0)), Ok(false));
    }

    #[test]
    fn a_differing_tty_is_reported_on_the_sid_path_too() {
        let sid_table = |tty| FakeTable(HashMap::from([(500, proc(999, Some(100), tty))]));
        let verdict = |table: &FakeTable| {
            attribute(Some(500), &ProcIndex::new([(100, 1, Some(PTS0))]), table)
                .map(|a| (a.how, a.tty_differs))
        };
        assert_eq!(verdict(&sid_table(Some(PTS1))), Ok((How::Sid, true)));
        assert_eq!(verdict(&sid_table(Some(PTS0))), Ok((How::Sid, false)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_parses_from_the_last_paren() {
        let line = b"1234 (a) b) c) S 1000 1234 1001 34816 1234 4194560 0 0";
        assert_eq!(
            proc_stat(line.as_slice()),
            Some(StatFields {
                ppid: 1000,
                sid: 1001,
                tty_nr: 34816
            })
        );
        let spaced = b"77 (Web Content) S 2 77 3 0 -1";
        assert_eq!(
            proc_stat(spaced.as_slice()),
            Some(StatFields {
                ppid: 2,
                sid: 3,
                tty_nr: 0
            })
        );
        assert_eq!(proc_stat(b"1234 no paren S 1 2 3 4"), None);
        assert_eq!(proc_stat(b"1234 (short) S 1 2 3 x"), None);
        assert_eq!(proc_stat(b"1234 (short) S 1 2"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_reads_a_comm_that_is_not_utf8() {
        // A 15-byte `comm` truncated inside a three-byte character.
        let mut line = b"4321 (build-\xe2\x82 x) S 10 4321 11 34816 1".to_vec();
        assert!(std::str::from_utf8(&line).is_err());
        assert_eq!(
            proc_stat(&line),
            Some(StatFields {
                ppid: 10,
                sid: 11,
                tty_nr: 34816
            })
        );
        line.extend_from_slice(b" \xff");
        assert_eq!(proc_stat(&line), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tty_decodes_linux_tty_nr_and_dev_t_to_the_same_pair() {
        assert_eq!(Tty::from_linux_tty_nr(34816), Some(PTS0));
        assert_eq!(Tty::from_dev_t(libc::makedev(136, 0)), PTS0);
        assert_eq!(Tty::from_linux_tty_nr(0), None);

        let minor = 300u32;
        let tty_nr = (minor & 0xff) | (136 << 8) | ((minor & !0xff) << 12);
        let pts300 = Tty {
            major: 136,
            minor: 300,
        };
        assert_eq!(Tty::from_linux_tty_nr(tty_nr), Some(pts300));
        assert_eq!(Tty::from_dev_t(libc::makedev(136, 300)), pts300);
    }
}
