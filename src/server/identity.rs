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
use std::os::fd::{BorrowedFd, OwnedFd};

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

/// The facts attribution needs about one process.
#[derive(Debug, Clone, Copy)]
pub struct ProcInfo {
    pub ppid: i32,
    /// `None` where the platform does not report a session id (macOS).
    pub sid: Option<i32>,
    pub tty: Option<Tty>,
    /// When the process started, on a clock that only has to order a parent
    /// against its child: clock ticks after boot on Linux, microseconds of
    /// wall-clock time on macOS.
    pub start: u64,
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

/// Why a peer is not attributed to a pane. Every variant fails closed: the
/// caller is refused, never attributed to some other pane.
#[derive(Debug, PartialEq, Eq)]
pub enum Unverified {
    /// The platform gave no peer pid for the socket.
    NoPeerPid,
    /// The parent chain reached pid 1 without passing a pane shell, and the
    /// session id names none either: a definite answer.
    NoPaneAncestor,
    /// The walk stopped before it could answer: an ancestor exited while it
    /// was being read, or the chain was longer than [`MAX_GENERATIONS`]. It
    /// says nothing about where the caller is.
    AncestorUnreadable,
    /// The peer itself could not be read: it exited, or a pid namespace hides
    /// it from the server.
    Unreadable,
    /// The process that connected is gone, so whatever now has its pid is
    /// not the sender.
    PeerExited,
    /// A process read during attribution was not the one its pid named when
    /// the check began: a pid was reused, or the socket changed hands.
    PeerChanged,
    /// The pin could not be read or confirmed for a reason that says nothing
    /// about whether the sender is alive.
    CheckFailed,
    /// The check did not finish in time. No verdict was reached, so nothing
    /// is said about the sender.
    TimedOut,
}

impl Unverified {
    pub fn reason(&self) -> &'static str {
        match self {
            Unverified::NoPeerPid => "no peer pid on this platform",
            Unverified::NoPaneAncestor => "not a descendant of any pane",
            Unverified::Unreadable => "the peer process could not be read",
            Unverified::PeerExited => "the sending process exited before it was checked",
            Unverified::PeerChanged | Unverified::AncestorUnreadable => {
                "the process tree changed while the sender was checked"
            }
            Unverified::CheckFailed => "the sending process could not be checked",
            Unverified::TimedOut => "the sender could not be checked in time",
        }
    }

    /// Whether this is a definite answer that the caller is in no pane, the
    /// only failure that may be treated as a caller outside every pane (and so
    /// allowed --pane). Every other failure says nothing about where the
    /// caller is: a pane the server could not check must not gain an
    /// outsider's powers because the server was slow.
    pub fn is_outside(&self) -> bool {
        matches!(self, Unverified::NoPaneAncestor | Unverified::NoPeerPid)
    }

    /// What the caller can do about it, the second half of a
    /// `sender_unverified` refusal.
    pub fn fix(&self) -> &'static str {
        match self {
            Unverified::NoPeerPid => "messaging needs Linux or macOS",
            Unverified::NoPaneAncestor => {
                "run it from inside the pane; do not setsid/daemonize the sender"
            }
            Unverified::Unreadable => "run the sender in the server's pid namespace",
            Unverified::PeerExited => {
                "keep the sender running until its reply; do not hand the socket to another process"
            }
            Unverified::PeerChanged
            | Unverified::AncestorUnreadable
            | Unverified::CheckFailed
            | Unverified::TimedOut => "retry",
        }
    }
}

/// The two reads of a connection's peer that bracket an attribution.
///
/// `begin` names the pid to attribute. `confirm` runs after every process-table
/// read and proves those reads were of the process that is still the peer.
/// Without it, a pid reused between the socket's `connect` and the reads would
/// attribute whatever process now holds the pid, which can be in another pane.
pub trait PeerPin {
    type Mark;
    fn begin(&self) -> Result<(i32, Self::Mark), Unverified>;
    fn confirm(&self, mark: Self::Mark, table: &impl ProcTable) -> Result<(), Unverified>;
}

/// The pane of the process that sent the frame just read on this connection.
///
/// The pin's verdict replaces attribution's, including an `Err`. A peer that
/// was reaped makes the reads say `Unreadable`, but the true reason is that it
/// exited.
pub fn attribute_caller<P: PeerPin>(
    pin: &P,
    procs: &ProcIndex,
    table: &impl ProcTable,
) -> Result<Attribution, Unverified> {
    let (pid, mark) = pin.begin()?;
    let found = attribute(Some(pid), procs, table);
    pin.confirm(mark, table)?;
    found
}

/// A macOS audit token reduced to what identifies a process: its pid, and a
/// version that a reused pid does not share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenId {
    pub pid: i32,
    pub version: u32,
}

pub fn same_process(before: TokenId, after: TokenId) -> Result<(), Unverified> {
    if before == after {
        Ok(())
    } else {
        Err(Unverified::PeerChanged)
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
    let (mut pid, mut current) = (peer, info);
    for depth in 0..=MAX_GENERATIONS {
        if pid <= 1 {
            break;
        }
        if let Some((pane, pane_tty)) = procs.get(pid) {
            return Ok(attributed(pane, pane_tty, How::Walk { depth }));
        }
        if depth == MAX_GENERATIONS {
            return Err(Unverified::AncestorUnreadable);
        }
        let parent = current.ppid;
        // A pane shell is never read: its pid belongs to an unreaped child of
        // the server, so it cannot have been reused.
        if parent > 1 && procs.get(parent).is_none() {
            let above = table.info(parent).ok_or(Unverified::AncestorUnreadable)?;
            // A parent exists before its child, so a "parent" that started
            // later is a reused pid. Equal is fine: a fork within one clock
            // tick is normal.
            if above.start > current.start {
                return Err(Unverified::PeerChanged);
            }
            current = above;
        }
        pid = parent;
    }
    Err(Unverified::NoPaneAncestor)
}

/// What the server holds about a connection's peer, from accept onward.
///
/// Owned by the connection's reader task. A handler attributes the frame it
/// was handed before the reader reads the next one, and the macOS pin depends
/// on that order: its token names whoever used the peer's end LAST.
pub struct Peer {
    pin: Pin,
}

enum Pin {
    /// The connector itself, held by a pidfd, which fails once it is reaped.
    #[cfg(target_os = "linux")]
    Pidfd {
        pid: i32,
        fd: OwnedFd,
    },
    /// A kernel without `SO_PEERPIDFD` (before 6.5): the connector's start
    /// time, read just after accept.
    #[cfg(target_os = "linux")]
    StartTime {
        pid: i32,
        start: u64,
    },
    /// A duplicate of the socket, so that the token can be read again for
    /// every frame without the socket's number being reused under it.
    #[cfg(target_os = "macos")]
    Token {
        sock: OwnedFd,
    },
    /// The socket could not be duplicated for the token reads.
    #[cfg(target_os = "macos")]
    Unavailable,
    None,
}

/// The connector's pidfd, taken in the accept arm, where it costs one
/// non-blocking `getsockopt`. `None` on a kernel before 6.5 and on every
/// platform other than Linux; the connection is then pinned by start time.
pub fn peer_pidfd(sock: BorrowedFd<'_>) -> Option<OwnedFd> {
    #[cfg(target_os = "linux")]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static NO_PIDFD_WARNED: AtomicBool = AtomicBool::new(false);
        match pidfd_of_peer(sock) {
            Ok(fd) => Some(fd),
            // A property of the kernel, not of this connection, so it is said
            // once rather than for every connection.
            Err(("getsockopt", nix::errno::Errno::ENOPROTOOPT)) => {
                if !NO_PIDFD_WARNED.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "identity: this kernel has no SO_PEERPIDFD (ENOPROTOOPT); connections are pinned by start time"
                    );
                }
                None
            }
            Err((call, errno)) => {
                log::warn!("identity: SO_PEERPIDFD {call} failed ({errno}); this connection is pinned by start time");
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sock;
        None
    }
}

/// `SO_PEERPIDFD`, with the failing call named for the log.
#[cfg(target_os = "linux")]
fn pidfd_of_peer(sock: BorrowedFd<'_>) -> Result<OwnedFd, (&'static str, nix::errno::Errno)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let got = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            &mut fd as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if got != 0 {
        return Err(("getsockopt", nix::errno::Errno::last()));
    }
    if fd < 0 {
        return Err(("getsockopt", nix::errno::Errno::EBADF));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Belt and braces: the kernel's pidfd_prepare already sets O_CLOEXEC. A
    // pidfd inherited by a process in a pane would let that process signal
    // the client through it, SIGKILL included.
    nix::fcntl::fcntl(
        fd.as_raw_fd(),
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .map_err(|errno| ("fcntl", errno))?;
    Ok(fd)
}

/// The verdict of `pidfd_send_signal(fd, 0)`. Only `ESRCH` says the process
/// is gone; any other failure says nothing about it, so it is not reported as
/// an exit.
#[cfg(target_os = "linux")]
fn pidfd_signal_verdict(sent: Result<(), nix::errno::Errno>) -> Result<(), Unverified> {
    match sent {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Err(Unverified::PeerExited),
        Err(_) => Err(Unverified::CheckFailed),
    }
}

/// The verdict for a failed `LOCAL_PEERTOKEN`. XNU answers `EINVAL` when the
/// peer's process is gone; anything else (`ENOTCONN`, `ENOPROTOOPT`) says
/// nothing about it. Platform-neutral so it is tested where it cannot run.
fn token_errno_verdict(errno: i32) -> Unverified {
    if errno == libc::EINVAL {
        Unverified::PeerExited
    } else {
        Unverified::CheckFailed
    }
}

impl Peer {
    /// Built at the top of the connection's task, before the handshake.
    /// `pid` is the socket's peer pid and `pidfd` came from [`peer_pidfd`].
    pub fn capture(pid: Option<i32>, pidfd: Option<OwnedFd>, sock: BorrowedFd<'_>) -> Self {
        #[cfg(target_os = "linux")]
        let pin = {
            let _ = sock;
            match (pid, pidfd) {
                (Some(pid), Some(fd)) => Pin::Pidfd { pid, fd },
                (Some(pid), None) => Pin::StartTime {
                    pid,
                    start: Procfs.info(pid).map_or(0, |info| info.start),
                },
                (None, _) => Pin::None,
            }
        };
        #[cfg(target_os = "macos")]
        let pin = {
            let _ = (pid, pidfd);
            match sock.try_clone_to_owned() {
                Ok(sock) => Pin::Token { sock },
                Err(e) => {
                    log::warn!("identity: could not duplicate the socket for its peer token: {e}");
                    Pin::Unavailable
                }
            }
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let pin = {
            let _ = (pid, pidfd, sock);
            Pin::None
        };
        Peer { pin }
    }

    #[cfg(test)]
    pub fn unpinned() -> Self {
        Peer { pin: Pin::None }
    }

    /// Which pin this peer has, for the log.
    pub fn pin_kind(&self) -> &'static str {
        match self.pin {
            #[cfg(target_os = "linux")]
            Pin::Pidfd { .. } => "pidfd",
            #[cfg(target_os = "linux")]
            Pin::StartTime { .. } => "starttime",
            #[cfg(target_os = "macos")]
            Pin::Token { .. } => "token",
            #[cfg(target_os = "macos")]
            Pin::Unavailable => "unavailable",
            Pin::None => "none",
        }
    }
}

impl PeerPin for Peer {
    type Mark = Option<TokenId>;

    fn begin(&self) -> Result<(i32, Option<TokenId>), Unverified> {
        match &self.pin {
            #[cfg(target_os = "linux")]
            Pin::Pidfd { pid, .. } | Pin::StartTime { pid, .. } => Ok((*pid, None)),
            #[cfg(target_os = "macos")]
            Pin::Token { sock } => {
                let token = peer_token(sock)?;
                Ok((token.pid, Some(token)))
            }
            #[cfg(target_os = "macos")]
            Pin::Unavailable => Err(Unverified::CheckFailed),
            Pin::None => Err(Unverified::NoPeerPid),
        }
    }

    fn confirm(&self, mark: Option<TokenId>, table: &impl ProcTable) -> Result<(), Unverified> {
        let _ = (&mark, table);
        match &self.pin {
            #[cfg(target_os = "linux")]
            Pin::Pidfd { fd, .. } => {
                use std::os::fd::AsRawFd;
                // Signal 0 checks without sending. It succeeds for a live
                // process and for a zombie, and a zombie's pid is not reused
                // until it is reaped; it fails with ESRCH once it is.
                let sent = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        0,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
                pidfd_signal_verdict(if sent == 0 {
                    Ok(())
                } else {
                    Err(nix::errno::Errno::last())
                })
            }
            #[cfg(target_os = "linux")]
            Pin::StartTime { pid, start } => match table.info(*pid) {
                None => Err(Unverified::PeerExited),
                Some(info) if info.start != *start => Err(Unverified::PeerChanged),
                Some(_) => Ok(()),
            },
            #[cfg(target_os = "macos")]
            Pin::Token { sock } => {
                let before = mark.ok_or(Unverified::PeerChanged)?;
                same_process(before, peer_token(sock)?)
            }
            #[cfg(target_os = "macos")]
            Pin::Unavailable => Err(Unverified::CheckFailed),
            Pin::None => Err(Unverified::NoPeerPid),
        }
    }
}

/// The audit token of whoever last used the peer's end of `sock`.
#[cfg(target_os = "macos")]
fn peer_token(sock: &OwnedFd) -> Result<TokenId, Unverified> {
    use std::os::fd::AsRawFd;
    // libc declares LOCAL_PEERTOKEN but not the token's type. `val[5]` is the
    // pid and `val[7]` the pid version, per libbsm's audit_token_to_pid and
    // audit_token_to_pidversion.
    #[repr(C)]
    struct AuditToken {
        val: [u32; 8],
    }
    let mut token = AuditToken { val: [0; 8] };
    let mut len = std::mem::size_of::<AuditToken>() as libc::socklen_t;
    let got = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            &mut token as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if got != 0 {
        return Err(token_errno_verdict(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ));
    }
    if len as usize != std::mem::size_of::<AuditToken>() {
        return Err(Unverified::CheckFailed);
    }
    Ok(TokenId {
        pid: token.val[5] as i32,
        version: token.val[7],
    })
}

#[cfg(target_os = "linux")]
pub struct Procfs;

#[cfg(target_os = "linux")]
impl ProcTable for Procfs {
    fn info(&self, pid: i32) -> Option<ProcInfo> {
        if pid <= 0 {
            return None;
        }
        let stat =
            crate::server::procfs::proc_stat(&std::fs::read(format!("/proc/{pid}/stat")).ok()?)?;
        Some(ProcInfo {
            ppid: stat.ppid,
            sid: Some(stat.sid),
            tty: Tty::from_linux_tty_nr(stat.tty_nr),
            start: stat.starttime?,
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
            start: info
                .pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec),
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
        ProcInfo {
            ppid,
            sid,
            tty,
            start: 0,
        }
    }

    fn proc_started(ppid: i32, start: u64) -> ProcInfo {
        ProcInfo {
            ppid,
            sid: Some(4242),
            tty: None,
            start,
        }
    }

    /// A pin whose two reads are scripted, and which records how many table
    /// reads had happened when `confirm` ran.
    struct FakePin {
        begin: Result<i32, fn() -> Unverified>,
        confirm: Option<fn() -> Unverified>,
        reads_at_confirm: std::cell::Cell<Option<usize>>,
    }

    impl FakePin {
        fn new(pid: i32, confirm: Option<fn() -> Unverified>) -> Self {
            FakePin {
                begin: Ok(pid),
                confirm,
                reads_at_confirm: std::cell::Cell::new(None),
            }
        }
    }

    impl PeerPin for FakePin {
        type Mark = ();
        fn begin(&self) -> Result<(i32, ()), Unverified> {
            self.begin.map(|pid| (pid, ())).map_err(|e| e())
        }
        fn confirm(&self, _: (), table: &impl ProcTable) -> Result<(), Unverified> {
            let _ = table;
            self.reads_at_confirm.set(Some(READS.with(|r| r.get())));
            self.confirm.map_or(Ok(()), |e| Err(e()))
        }
    }

    thread_local! {
        static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// A [`FakeTable`] that counts its reads, so a test can tell whether a
    /// pin was confirmed after them.
    struct CountingTable(FakeTable);

    impl ProcTable for CountingTable {
        fn info(&self, pid: i32) -> Option<ProcInfo> {
            READS.with(|r| r.set(r.get() + 1));
            self.0.info(pid)
        }
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
            Err(Unverified::AncestorUnreadable),
            "a walk stopped by the cap never reached pid 1, so it proves nothing"
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
    fn an_unreadable_peer_is_unreadable_and_an_unreadable_ancestor_is_not_a_verdict() {
        let procs = ProcIndex::new([(100, 1, None)]);
        let empty = FakeTable(HashMap::new());
        assert_eq!(
            attribute(Some(500), &procs, &empty),
            Err(Unverified::Unreadable)
        );
        // 501 exited during the check: whether a pane was above it is unknown.
        let broken = FakeTable(HashMap::from([(500, proc(501, Some(4242), None))]));
        assert_eq!(
            attribute(Some(500), &procs, &broken),
            Err(Unverified::AncestorUnreadable)
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

    #[test]
    fn a_reused_parent_pid_is_refused_not_attributed() {
        // 501 is 500's parent by pid, but started after 500 did, so the pid
        // was reused after 500's real parent exited.
        let table = FakeTable(HashMap::from([
            (500, proc_started(501, 100)),
            (501, proc_started(100, 200)),
        ]));
        let procs = ProcIndex::new([(100, 1, None)]);
        assert_eq!(
            attribute(Some(500), &procs, &table),
            Err(Unverified::PeerChanged)
        );
    }

    #[test]
    fn a_parent_started_in_the_same_tick_is_accepted() {
        let table = FakeTable(HashMap::from([
            (500, proc_started(501, 100)),
            (501, proc_started(100, 100)),
        ]));
        let procs = ProcIndex::new([(100, 1, None)]);
        assert_eq!(
            attribute(Some(500), &procs, &table).map(|a| a.how),
            Ok(How::Walk { depth: 2 })
        );
    }

    #[test]
    fn pin_checked_after_the_reads() {
        let procs = ProcIndex::new([(100, 1, None)]);
        let table = CountingTable(FakeTable(HashMap::from([
            (500, proc(501, Some(4242), None)),
            (501, proc(100, Some(4242), None)),
        ])));

        let fine = FakePin::new(500, None);
        READS.with(|r| r.set(0));
        assert_eq!(
            attribute_caller(&fine, &procs, &table).map(|a| a.pane),
            Ok(1)
        );
        assert_eq!(
            fine.reads_at_confirm.get(),
            Some(READS.with(|r| r.get())),
            "the pin was confirmed before the last read"
        );
        assert!(READS.with(|r| r.get()) > 0);

        let exited = FakePin::new(500, Some(|| Unverified::PeerExited));
        assert_eq!(
            attribute_caller(&exited, &procs, &table),
            Err(Unverified::PeerExited),
            "a pane was attributed although the pin failed"
        );
        let changed = FakePin::new(500, Some(|| Unverified::PeerChanged));
        assert_eq!(
            attribute_caller(&changed, &procs, &table),
            Err(Unverified::PeerChanged)
        );

        let gone = FakePin::new(777, Some(|| Unverified::PeerExited));
        assert_eq!(
            attribute_caller(&gone, &procs, &table),
            Err(Unverified::PeerExited),
            "the reads' Unreadable won over the pin"
        );

        let no_pid = FakePin {
            begin: Err(|| Unverified::NoPeerPid),
            confirm: None,
            reads_at_confirm: std::cell::Cell::new(None),
        };
        READS.with(|r| r.set(0));
        assert_eq!(
            attribute_caller(&no_pid, &procs, &table),
            Err(Unverified::NoPeerPid)
        );
        assert_eq!(READS.with(|r| r.get()), 0, "a pin with no pid still read");
    }

    #[test]
    fn only_einval_from_the_peer_token_means_the_peer_exited() {
        assert_eq!(token_errno_verdict(libc::EINVAL), Unverified::PeerExited);
        for errno in [libc::ENOTCONN, libc::ENOPROTOOPT, 0] {
            assert_eq!(token_errno_verdict(errno), Unverified::CheckFailed);
        }
    }

    /// Only a definite "this process is in no pane" may be treated as a
    /// caller outside every pane. A check that failed says nothing about
    /// where the caller is, and a pane must not gain an outsider's powers
    /// because the server was slow.
    #[test]
    fn only_a_definite_answer_is_outside() {
        for (why, outside) in [
            (Unverified::NoPaneAncestor, true),
            (Unverified::NoPeerPid, true),
            (Unverified::AncestorUnreadable, false),
            (Unverified::Unreadable, false),
            (Unverified::PeerExited, false),
            (Unverified::PeerChanged, false),
            (Unverified::CheckFailed, false),
            (Unverified::TimedOut, false),
        ] {
            assert_eq!(why.is_outside(), outside, "{why:?}");
        }
    }

    #[test]
    fn same_process_needs_the_pid_and_the_version_to_match() {
        let t = |pid, version| TokenId { pid, version };
        assert_eq!(same_process(t(5, 9), t(5, 9)), Ok(()));
        assert_eq!(same_process(t(5, 9), t(6, 9)), Err(Unverified::PeerChanged));
        assert_eq!(
            same_process(t(5, 9), t(5, 10)),
            Err(Unverified::PeerChanged),
            "a reused pid with a new version passed"
        );
    }

    /// Real processes and real syscalls, so Linux only. Children are started
    /// with a raw `fork`: `Command::spawn` holds std's environment lock across
    /// its fork, and a concurrent pty test would copy that lock into a child
    /// that then calls `set_var`.
    #[cfg(target_os = "linux")]
    mod live {
        use super::super::*;
        use nix::sys::signal::{kill, Signal};
        use nix::sys::wait::waitpid;
        use nix::unistd::{fork, ForkResult, Pid};
        use std::os::fd::AsFd;

        fn fork_paused() -> Pid {
            match unsafe { fork() }.unwrap() {
                ForkResult::Child => {
                    crate::server::procfs::testing::close_inherited_descriptors();
                    loop {
                        unsafe { libc::pause() };
                    }
                }
                ForkResult::Parent { child } => child,
            }
        }

        #[test]
        fn proc_info_reads_a_comm_that_is_not_utf8() {
            let child = crate::server::procfs::testing::TruncatedCommChild::spawn("procinfo");
            assert!(
                child.comm_is_truncated(),
                "the fixture's comm is not the truncated name, so it tests nothing: {:?}",
                child.comm
            );
            let info = Procfs
                .info(child.pid.as_raw())
                .expect("a process whose comm is not UTF-8 could not be read");
            assert_eq!(info.ppid, std::process::id() as i32);
            assert!(info.start > 0);
        }

        /// Both sides of the pidfd's liveness test: a zombie still holds its
        /// pid, so it passes; once reaped it fails.
        #[test]
        fn pidfd_pin_sees_its_peer_exit() {
            use std::io::Read;
            use std::os::unix::ffi::OsStrExt;
            let dir = std::env::temp_dir().join(format!("rmx-pidfd-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("s");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

            let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (dst, src) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
                *dst = *src as libc::c_char;
            }
            let child = match unsafe { fork() }.unwrap() {
                ForkResult::Child => unsafe {
                    crate::server::procfs::testing::close_inherited_descriptors();
                    let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
                    let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
                    if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, len) == 0 {
                        libc::write(fd, b"x".as_ptr() as *const libc::c_void, 1);
                    }
                    libc::_exit(0)
                },
                ForkResult::Parent { child } => child,
            };
            let (mut stream, _) = listener.accept().unwrap();
            let mut byte = [0u8];
            stream.read_exact(&mut byte).unwrap();

            let pidfd = match pidfd_of_peer(stream.as_fd()) {
                Ok(fd) => fd,
                Err(("getsockopt", nix::errno::Errno::ENOPROTOOPT)) => {
                    eprintln!("SKIP: this kernel has no SO_PEERPIDFD");
                    waitpid(child, None).unwrap();
                    let _ = std::fs::remove_dir_all(&dir);
                    return;
                }
                Err(e) => panic!("SO_PEERPIDFD failed: {e:?}"),
            };
            let peer = Peer::capture(Some(child.as_raw()), Some(pidfd), stream.as_fd());
            assert_eq!(peer.pin_kind(), "pidfd");

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !std::fs::read_to_string(format!("/proc/{}/stat", child.as_raw()))
                .is_ok_and(|s| s.contains(") Z "))
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the child never exited"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let check = |peer: &Peer| {
                let (_, mark) = peer.begin().unwrap();
                peer.confirm(mark, &Procfs)
            };
            let zombie = check(&peer);
            waitpid(child, None).unwrap();
            let reaped = check(&peer);
            let _ = std::fs::remove_dir_all(&dir);

            assert_eq!(zombie, Ok(()), "a zombie's pid was treated as reusable");
            assert_eq!(reaped, Err(Unverified::PeerExited));
        }

        #[test]
        fn only_esrch_from_the_pidfd_means_the_peer_exited() {
            use nix::errno::Errno;
            assert_eq!(pidfd_signal_verdict(Ok(())), Ok(()));
            assert_eq!(
                pidfd_signal_verdict(Err(Errno::ESRCH)),
                Err(Unverified::PeerExited)
            );
            for errno in [Errno::EPERM, Errno::EINVAL, Errno::EBADF] {
                assert_eq!(
                    pidfd_signal_verdict(Err(errno)),
                    Err(Unverified::CheckFailed),
                    "{errno} was reported as an exit"
                );
            }
        }

        /// The fallback for a kernel before 6.5. Its `ENOPROTOOPT` branch in
        /// [`peer_pidfd`] cannot be reached on a kernel that has the option,
        /// so the pin is built directly.
        #[test]
        fn start_time_pin_detects_a_different_start() {
            let child = fork_paused();
            let pid = child.as_raw();
            let start = Procfs.info(pid).expect("the child could not be read").start;
            let pinned = |start| Peer {
                pin: Pin::StartTime { pid, start },
            };
            let check = |peer: Peer| {
                let (_, mark) = peer.begin().unwrap();
                peer.confirm(mark, &Procfs)
            };
            let right = check(pinned(start));
            let off_by_one = check(pinned(start + 1));
            let _ = kill(child, Signal::SIGKILL);
            waitpid(child, None).unwrap();
            let reaped = check(pinned(start));

            assert_eq!(right, Ok(()));
            assert_eq!(off_by_one, Err(Unverified::PeerChanged));
            assert_eq!(reaped, Err(Unverified::PeerExited));
        }
    }
}
