//! Server daemon implementation.
//!
//! This module provides the Remux daemon process, Unix socket communication
//! helpers, and the main server event loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use crate::config::theme::{CompositorTheme, TabStyle};
use crate::config::{BorderStyle, Config, ZoomMode};
use crate::protocol;
use crate::protocol::*;
use crate::screen::Screen;
use crate::server::browse;
use crate::server::compositor::{
    composite, draw_popup, fits_zellij_border, float_zoom_box, hit_test, is_multi_stack,
    pane_content_rect, render_popup_layer, ClickTarget, HitRegions, MouseSelection, PopupLayer,
    StatusInfo, NO_FOCUSED_PANE,
};
use crate::server::identity;
use crate::server::layout::{
    self, CustomLayout, EnabledLayouts, LayoutMode, LayoutNode, MasterLayout, PaneId, Rect,
};
use crate::server::messaging::Messaging;
use crate::server::persistence::{self, PersistedState};
use crate::server::pty::{self, PaneIdentity, Pty};
use crate::server::session::{self, Folder, ServerState, Session};

/// In-memory store of dormant (saved-but-not-live) sessions.
///
/// Populated at startup when `save_sessions = true` and
/// `automatic_restore = false`: the persisted snapshot is loaded here instead
/// of being brought live. Sessions migrate out of this store (into live
/// `ServerState`) when the client resurrects them. Empty in every other mode,
/// so the merge-on-save is a no-op on the default path.
pub type DormantStore = Arc<Mutex<PersistedState>>;

/// Type alias for the per-client previous-frame cache used for diff rendering.
///
/// Keyed by `client_id` (not session name): each client is diffed against what
/// *it* last displayed. Multiple clients of different sizes can attach to one
/// session, so a shared session-keyed baseline would poison diffs across
/// differently-sized clients. All render paths composite at one consistent
/// session render size (min over attached clients), so a given client never
/// mixes differently-sized frames.
pub type PrevFrameCache = Arc<Mutex<HashMap<u64, PrevFrame>>>;

/// What a client was last sent, which is what its next frame is diffed against.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrevFrame {
    /// The frame's cells. For a client that does not place the popup itself,
    /// these include the popup the server drew into them.
    pub cells: Vec<Vec<RenderCell>>,
    /// The popup layer a client that places the popup itself holds, with its
    /// `(cols, rows)`. `None` when no layer is up.
    pub popup: Option<PrevLayer>,
}

/// The popup layer a client holds: its `(cols, rows)` and its cells.
pub type PrevLayer = ((u16, u16), Vec<Vec<RenderCell>>);

/// The name of the process with this pid, e.g. `"zsh"` or `"claude"`.
///
/// Falls back to `"shell"` when it cannot be determined, which is what a pane
/// with no readable process is labelled in the session tree.
///
/// Shared with [`crate::server::agents`], which hands it a foreground process
/// GROUP id rather than a pane's child pid. One reader, two callers -- and the
/// reason this is platform-split rather than Linux-only: agent detection reads
/// `/proc/<pgid>/comm` THROUGH here, so a macOS server that always answered
/// `"shell"` could never match a configured agent command, and
/// [`crate::server::agents::DETECTION_SUPPORTED`] had to report `false` to stop
/// the panel from looking merely empty. Teaching this function macOS is what
/// lets that constant say `true` there. It fixes macOS pane NAMES at the same
/// time, which had the identical fallback.
///
/// The Linux arm: `/proc/<pid>/comm`.
#[cfg(target_os = "linux")]
pub(crate) fn get_process_name(pid: i32) -> String {
    try_process_name(pid).unwrap_or_else(|| "shell".to_string())
}

/// [`get_process_name`] without its `"shell"` fallback: `None` when `pid`
/// cannot be read, which is what a process reaped since it was found reads as.
#[cfg(target_os = "linux")]
fn try_process_name(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| crate::config::pane_title::sanitize_label(s.trim()))
}

/// See the Linux variant for documentation. macOS has no `/proc` and no
/// `comm` a program sets for itself, so a process is named by its current
/// `argv[0]` ([`command_name_from_argv0`]), and by the basename of its
/// executable's path only when `argv[0]` is missing or empty. Both are read
/// on every call.
///
/// `argv[0]` first because the executable's path names what is on disk, not
/// what was run. Claude Code's launcher is a symlink named `claude` to
/// `versions/<version>`, and `proc_pidpath` resolves it, so a pane named from
/// the path read `2.1.283`. Linux reports `claude` there, because `comm` is
/// taken from the name the program was run by.
///
/// **Not through `sysinfo`, which is what this used to do.** `sysinfo` sets a
/// process's name once, when it first sees the pid (`if process.name.is_empty()`
/// in `sysinfo-0.35.2/src/unix/apple/macos/process.rs`), and an `exec` changes
/// neither the pid nor its start time, so the entry is never replaced. A new
/// pane's shell looked at in the instant between the server's fork and the
/// shell's exec was therefore named `remux` for the rest of its life, and a
/// program that execs another kept its first name. Both reads here are the
/// kernel's current answer.
#[cfg(target_os = "macos")]
pub(crate) fn get_process_name(pid: i32) -> String {
    try_process_name(pid).unwrap_or_else(|| "shell".to_string())
}

/// See the Linux variant.
#[cfg(target_os = "macos")]
fn try_process_name(pid: i32) -> Option<String> {
    process_argv0(pid)
        .as_deref()
        .and_then(command_name_from_argv0)
        .or_else(|| executable_name(pid))
}

/// The name a process's `argv[0]` gives it: the basename of a path, without
/// the leading `-` a login shell is run with. `None` for an empty one.
#[cfg(any(target_os = "macos", test))]
fn command_name_from_argv0(argv0: &str) -> Option<String> {
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    let name = crate::config::pane_title::sanitize_label(base.strip_prefix('-').unwrap_or(base));
    (!name.is_empty()).then_some(name)
}

/// `pid`'s current `argv[0]`, read with the `KERN_PROCARGS2` sysctl.
#[cfg(target_os = "macos")]
fn process_argv0(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query; no buffer is written.
    let queried = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if queried == -1 || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is writable for `size` bytes, and the kernel lowers `size`
    // to what it wrote.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read == -1 {
        return None;
    }
    buf.truncate(size);
    argv0_from_procargs(&buf)
}

/// `argv[0]` out of a `KERN_PROCARGS2` buffer: a native-endian `int argc`,
/// then the executable's path, NUL-padded to the next multiple of 8 bytes
/// after its terminator, then `argv` as NUL-terminated strings.
///
/// `argv[0]` is read at exactly that offset, so an EMPTY `argv[0]` comes back
/// empty. Skipping every NUL after the path instead, as `ps` does, takes the
/// next non-empty string for it: `argv[1]`, or with `argc` 1 the first
/// `KEY=VALUE` of the environment, which would then name the pane and be
/// saved to `state.json`. The padding rule was measured on macOS 27 for paths
/// of every length modulo 8. Should the bytes before that offset not all be
/// NUL, the padding is shorter than the rule says, `argv[0]` is not empty,
/// and the first non-NUL byte starts it.
#[cfg(any(target_os = "macos", test))]
fn argv0_from_procargs(buf: &[u8]) -> Option<String> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    if argc < 1 {
        return None;
    }
    let area = &buf[4..];
    let path_end = area.iter().position(|&b| b == 0)?;
    let aligned = (path_end + 1).div_ceil(8) * 8;
    let padding = area.get(path_end..aligned.min(area.len()))?;
    let start = match padding.iter().position(|&b| b != 0) {
        Some(early) => path_end + early,
        None => aligned,
    };
    let args = area.get(start..)?;
    let end = args.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&args[..end]).into_owned())
}

/// The basename of `pid`'s executable, as the kernel reports it now.
#[cfg(target_os = "macos")]
fn executable_name(pid: i32) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;

    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is writable for the length passed.
    let n =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(&buf[..n as usize]));
    path.file_name()
        .map(|f| crate::config::pane_title::sanitize_label(&f.to_string_lossy()))
        .filter(|name| !name.is_empty())
}

/// Fallback for platforms with no known way to name a foreign process. Always
/// `"shell"` -- and [`crate::server::agents::DETECTION_SUPPORTED`] is `false`
/// there, so the agents panel says it cannot know rather than showing an empty
/// list that reads as "no agents".
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn get_process_name(_pid: i32) -> String {
    "shell".to_string()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn try_process_name(_pid: i32) -> Option<String> {
    None
}

/// Every name by which this OS will admit to knowing `pid`.
///
/// [`get_process_name`] alone is not enough to RECOGNISE a program, because the
/// two platforms answer a different question and for some programs the two
/// answers disagree:
///
/// * Linux `/proc/<pid>/comm` is a process TITLE. It starts as the executable's
///   basename, and a process may then set it to whatever it likes
///   (`prctl(PR_SET_NAME)`, which is what `process.title =` compiles down to in
///   every Node/Bun-shaped runtime).
/// * macOS names it by the EXECUTABLE PATH's basename (`proc_pidpath`), which
///   resolves symlinks. It never reads `p_comm`, and no title a process sets
///   for itself can move it.
///
/// Claude Code is exactly the program that falls in the gap. Its installer
/// execs a VERSIONED path with `argv[0] = "claude"`:
///
/// ```text
/// comm=claude exe=/home/rakan/.local/share/claude/versions/2.1.251
///     argv: claude --dangerously-skip-permissions --resume
/// ```
///
/// so Linux says `claude` (the title the binary sets) while macOS says
/// `2.1.251` (the executable's real basename) -- and `2.1.251` matches nothing
/// in `[agents] commands`, which is the whole of the "the agents panel is empty
/// on my Mac" report. `argv[0]` is the name both platforms agree on, so it is
/// carried alongside as the second candidate.
///
/// `name` is the kernel's name, deliberately not [`get_process_name`]'s: on
/// macOS that prefers `argv[0]` for pane names, and detection tries the
/// kernel's name first so that its outcome, and every harness pinning it, is
/// unchanged.
pub(crate) struct ProcessNames {
    /// The kernel's name for the process: `comm` on Linux, the executable's
    /// basename on macOS. Never empty -- `"shell"` when the process cannot be
    /// read at all.
    pub name: String,
    /// The process's `argv[0]`, VERBATIM: a bare word (`claude`), a path
    /// (`/usr/local/bin/claude`), or a login shell's leading-dash convention
    /// (`-zsh`). Reducing it to a name is the caller's job and is a pure
    /// function, which is why it is not done here. `None` when the OS will not
    /// say.
    pub argv0: Option<String>,
}

/// The Linux arm: `comm`, plus the first NUL-separated field of
/// `/proc/<pid>/cmdline`.
///
/// The `cmdline` read is UNCONDITIONAL rather than taken only when `comm` fails
/// to match, and that is a deliberate trade. Making it lazy would save one small
/// procfs read per non-agent pane per sample (single-digit microseconds, against
/// a `tcgetpgrp` syscall and a `comm` read already paid on the same pass) and
/// would save the macOS arm nothing, since its name read does not give
/// `argv[0]`. Cheap where it is paid.
#[cfg(target_os = "linux")]
pub(crate) fn get_process_names(pid: i32) -> ProcessNames {
    ProcessNames {
        name: get_process_name(pid),
        argv0: std::fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .and_then(|raw| {
                let first = raw.split(|b| *b == 0).next()?;
                let argv0 =
                    crate::config::pane_title::strip_unprintable(&String::from_utf8_lossy(first));
                (!argv0.is_empty()).then_some(argv0)
            }),
    }
}

/// See the Linux variant for documentation. On macOS the name is the
/// executable's basename, which is what agent detection matches first, and
/// `argv[0]` is read fresh with the `KERN_PROCARGS2` sysctl on every call, as
/// an `exec` replaces it.
#[cfg(target_os = "macos")]
pub(crate) fn get_process_names(pid: i32) -> ProcessNames {
    ProcessNames {
        name: executable_name(pid).unwrap_or_else(|| "shell".to_string()),
        argv0: process_argv0(pid)
            .map(|a| crate::config::pane_title::strip_unprintable(&a))
            .filter(|a| !a.is_empty()),
    }
}

/// Fallback for platforms with no known way to name a foreign process. The name
/// is `"shell"` and there is no `argv[0]`, so nothing can ever match -- which is
/// what [`crate::server::agents::DETECTION_SUPPORTED`] reports as `false`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn get_process_names(pid: i32) -> ProcessNames {
    ProcessNames {
        name: get_process_name(pid),
        argv0: None,
    }
}

/// The pids `pid` is the direct parent of.
///
/// Used to look THROUGH a launcher: an npm-installed agent is a Node shim that
/// `spawn()`s the real binary and stays alive as its parent, so the foreground
/// process group's LEADER is the shim and the agent is a child inside the same
/// group. [`crate::server::agents::foreground_command`] walks these; the bounds
/// on that walk live there, because they are a property of the sampling loop
/// rather than of the lookup.
///
/// An empty vec means "no children, or this platform will not say", and the two
/// are deliberately not distinguished: every caller does the same thing with
/// both, which is to stop.
///
/// # Linux
///
/// `/proc/<pid>/task/<tid>/children`, for EVERY thread -- a multi-threaded
/// parent may have forked from a worker thread, and that child is listed only
/// under the thread that made it. The file is `CONFIG_PROC_CHILDREN`-gated, so
/// a kernel built without it silently reports nothing and detection falls back
/// to the leader-only behaviour it had before this existed.
///
/// **Bounded, in READS as well as in results.** The caller's walk caps how many
/// processes it will NAME, which says nothing about how many files were opened
/// to find them: a leader with 120 threads is 120 `children` reads for every
/// pane, on every sample, at up to ten a second -- and it is the output-heavy
/// panes (a build, a test run, a browser) that have both the thread count and
/// the pusher ticking. So both the number of threads consulted and the number
/// of children collected stop at [`CHILD_CAP`], the same shape the macOS arm
/// already had.
///
/// The cost of the cap is that a child forked from the 65th thread of a
/// 65-thread launcher is not seen. No shipped agent looks like that -- the ones
/// measured all spawn from the main thread -- and an unbounded per-sample sweep
/// of another program's thread list is the worse of the two risks.
#[cfg(target_os = "linux")]
pub(crate) fn child_pids(pid: i32) -> Vec<i32> {
    if pid <= 0 {
        return Vec::new();
    }
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for task in tasks.flatten().take(CHILD_CAP) {
        let Ok(raw) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        out.extend(
            raw.split_ascii_whitespace()
                .filter_map(|t| t.parse::<i32>().ok()),
        );
        if out.len() >= CHILD_CAP {
            out.truncate(CHILD_CAP);
            break;
        }
    }
    out
}

/// How many children [`child_pids`] will report, and -- on Linux -- how many
/// threads it will ask. Both platforms stop here.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const CHILD_CAP: usize = 64;

/// See the Linux variant. macOS answers this directly, through `libproc`.
///
/// `proc_listchildpids` takes a buffer size in BYTES and returns a COUNT of
/// pids (`libproc.c` divides the byte count by `sizeof(int)` before returning
/// it, exactly as `proc_listallpids` does -- which is how `sysinfo` reads it,
/// `sysinfo-0.35.2/src/unix/apple/macos/process.rs:783`). The buffer is
/// ZEROED and the result filtered rather than trusted verbatim, so a kernel
/// that returned bytes instead would leave zeros to be dropped rather than
/// uninitialised memory to be read as a pid.
///
/// The buffer is fixed: the caller's walk is bounded far below this, so a
/// process with hundreds of children is truncated on purpose rather than
/// retried with a bigger allocation.
///
/// Never RUN on a Mac, only type-checked -- the same standing as
/// `get_process_names`' macOS arm.
#[cfg(target_os = "macos")]
pub(crate) fn child_pids(pid: i32) -> Vec<i32> {
    if pid <= 0 {
        return Vec::new();
    }
    let mut buf = [0i32; CHILD_CAP];
    let n = unsafe {
        libc::proc_listchildpids(
            pid,
            buf.as_mut_ptr() as *mut libc::c_void,
            std::mem::size_of_val(&buf) as libc::c_int,
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    let n = (n as usize).min(CHILD_CAP);
    buf[..n].iter().copied().filter(|p| *p > 0).collect()
}

/// Fallback for platforms with no known way to list a process's children. See
/// [`crate::server::agents::DETECTION_SUPPORTED`]: nothing could match anyway.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn child_pids(_pid: i32) -> Vec<i32> {
    Vec::new()
}

/// The process GROUP `pid` belongs to, or `None` if it cannot be read.
///
/// This is what keeps the job walk to the job: a child of the foreground
/// leader is only part of the same JOB if it is in the same process group, and
/// one the user has parked (`Ctrl-Z`, or `&` under an interactive shell) is
/// not. See [`crate::server::agents::foreground_command`] for why a parked
/// agent must stay unlisted.
#[cfg(target_os = "linux")]
pub(crate) fn process_pgid(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    Some(crate::server::procfs::proc_stat(&stat)?.pgrp)
}

/// See the Linux variant. macOS answers from `libproc`'s BSD info block.
///
/// `proc_pidinfo` returns the number of bytes it wrote, so a short answer is a
/// failure and is treated as one rather than read out of a half-filled struct.
/// Never RUN on a Mac, only type-checked.
#[cfg(target_os = "macos")]
pub(crate) fn process_pgid(pid: i32) -> Option<i32> {
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
    (wrote == size).then_some(info.pbi_pgid as i32)
}

/// Fallback for platforms that cannot say. `None` filters every candidate out,
/// which is the same answer [`child_pids`] already gives there.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn process_pgid(_pid: i32) -> Option<i32> {
    None
}

/// A pane whose program has exited is left out, so that a leftover process
/// still in its session is not attributed to it.
fn proc_index(ps: &HashMap<PaneId, PaneData>) -> identity::ProcIndex {
    identity::ProcIndex::new(ps.iter().filter(|(_, pd)| !pd.exited).map(|(&id, pd)| {
        (
            pd.pty.child_pid.as_raw(),
            id,
            pd.pty.slave_dev.map(identity::Tty::from_dev_t),
        )
    }))
}

/// How long the per-connection identity line may hold up the connection's
/// reader. Past it the peer is logged as unattributed; the line is diagnostic,
/// so a wedged `panes` lock or a slow `/proc` must never cost a connection.
const ATTRIBUTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a message handler waits for its caller to be attributed.
///
/// The bound covers the whole check (the pin's first read, the process table,
/// the pin's second read) and a timeout gives no verdict at all, so it is
/// refused as `sender_unverified` with its own reason. Longer than
/// [`ATTRIBUTION_TIMEOUT`] because here a timeout refuses the user's request
/// rather than dropping a log line, and shorter than the CLI's 10s reply
/// deadline so that the CLI reports the refusal instead of its own timeout.
pub(crate) const CALLER_ATTRIBUTION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// The pane of the process that sent the frame being handled.
///
/// The reader loop awaits the handler that calls this, so the next frame is
/// not read until the verdict is in or the timeout has fired. A task
/// abandoned by the timeout is never awaited again, so its late answer is
/// dropped rather than applied to a later frame.
async fn attribute_frame_caller(
    peer: &Arc<identity::Peer>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) -> Result<identity::Attribution, identity::Unverified> {
    let peer = Arc::clone(peer);
    let checked = tokio::time::timeout(CALLER_ATTRIBUTION_TIMEOUT, async {
        let index = proc_index(&*panes.lock().await);
        tokio::task::spawn_blocking(move || {
            identity::attribute_caller(&*peer, &index, &identity::system_table())
        })
        .await
    })
    .await;
    match checked {
        Ok(Ok(verdict)) => verdict,
        Ok(Err(e)) => {
            log::warn!("identity: attribution task failed: {e}");
            Err(identity::Unverified::CheckFailed)
        }
        Err(_) => Err(identity::Unverified::TimedOut),
    }
}

/// Refuse a caller whose attribution failed for a reason that says nothing
/// about where it is. Only [`identity::Unverified::is_outside`] may be treated
/// as a caller outside every pane, with an outsider's --pane.
fn unverified_refusal(
    caller: &Result<identity::Attribution, identity::Unverified>,
) -> Option<ServerMessage> {
    match caller {
        Err(why) if !why.is_outside() => Some(ServerMessage::refused(
            "sender_unverified",
            format!("{}: {}", why.reason(), why.fix()),
        )),
        _ => None,
    }
}

/// The attribution line a message handler writes. `how` is what the harnesses
/// read to tell the session-id path from the walk.
fn log_caller(client_id: u64, verb: &str, found: &identity::Attribution) {
    log::debug!(
        "identity: client_id={client_id} {verb} caller -> pane %{} by {:?}{}",
        found.pane,
        found.how,
        if found.tty_differs {
            "; its tty differs (nested pty?)"
        } else {
            ""
        }
    );
}

async fn reply(
    client_id: u64,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    msg: ServerMessage,
) {
    if let Some(conn) = clients.lock().await.get(&client_id) {
        let _ = conn.tx.send(msg);
    }
}

/// The panes a message may name: in the table and not exited.
async fn live_panes(
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) -> std::collections::HashSet<PaneId> {
    panes
        .lock()
        .await
        .iter()
        .filter(|(_, pd)| !pd.exited)
        .map(|(&id, _)| id)
        .collect()
}

/// A [`ClientMessage::MsgSend`], unpacked.
struct OutgoingMessage {
    to: Address,
    body: String,
    deliver: Deliver,
    reply_to: Option<MsgRef>,
    pane_hint: Option<PaneId>,
    defer: bool,
    timeout_ms: Option<u64>,
    wait: bool,
    origin: Option<crate::protocol::Origin>,
    req: Option<u64>,
}

/// A [`ClientMessage::AgentName`], unpacked.
struct NameRequest {
    name: Option<String>,
    pane: Option<PaneId>,
    pane_hint: Option<PaneId>,
}

/// A [`ClientMessage::MsgAck`], unpacked.
struct MailboxAck {
    pane: Option<PaneId>,
    ids: Vec<MsgId>,
    all: bool,
    pane_hint: Option<PaneId>,
}

/// A [`ClientMessage::MsgRead`], unpacked.
struct MailboxRead {
    pane: Option<PaneId>,
    all: bool,
    peek: bool,
    wait_ms: Option<u64>,
    pane_hint: Option<PaneId>,
}

/// A caller-supplied string as it may be logged: Debug-quoted, so a newline
/// cannot start a forged log line, and cut to a bounded length.
fn loggable(s: &str) -> String {
    const MAX: usize = 64;
    let quoted = format!("{s:?}");
    if quoted.chars().count() <= MAX {
        quoted
    } else {
        format!("{}...", quoted.chars().take(MAX).collect::<String>())
    }
}

/// Why a popup is refused, as the popup's own pane: it is in no tab and no
/// session tree, so nothing could address it or name where it lives.
const POPUP_REFUSED: &str = "a popup cannot send or receive messages";

fn log_hint(pane_hint: Option<PaneId>, attributed: PaneId) {
    if let Some(hint) = pane_hint.filter(|&hint| hint != attributed) {
        log::info!(
            "msg: pane_hint=%{hint} disagrees with attribution %{attributed}; using the attribution"
        );
    }
}

/// Store a message from the calling pane. The sender is the attribution,
/// never `pane_hint` and never anything else the caller sent; a caller that
/// cannot be attributed is refused rather than sent anonymously.
#[allow(clippy::too_many_arguments)]
async fn handle_msg_send(
    client_id: u64,
    msg: OutgoingMessage,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    store: &'static Mutex<Messaging>,
) -> Option<ServerMessage> {
    use crate::server::messaging::Refusal;

    let link = links()
        .lock()
        .await
        .peer_name(client_id, std::time::Instant::now())
        .map(str::to_string);
    if let Some(link) = link {
        return handle_link_send(client_id, link, msg, state, panes, config, clients, store).await;
    }
    if msg.origin.is_some() {
        log::warn!(
            "msg: origin refused (origin_not_allowed): client_id={client_id} peer_pid={} is not a peer link",
            peer_pid(peer)
        );
        return Some(ServerMessage::refused(
            "origin_not_allowed",
            "only a peer link may forward a message with its origin",
        ));
    }

    let bad_host = msg
        .to
        .host
        .iter()
        .chain(msg.reply_to.as_ref().and_then(|r| r.host.as_ref()))
        .find(|host| !valid_host(host));
    if let Some(host) = bad_host {
        let refusal = Refusal::BadAddress(format!("bad host {}", loggable(host)));
        return Some(ServerMessage::refused(refusal.code(), refusal.message()));
    }
    if let Some(host) = msg.to.host.clone() {
        return send_to_host(
            client_id, host, msg, peer, state, panes, config, clients, store,
        )
        .await;
    }
    // The CLI parses the address, but the wire accepts any string as a name.
    if let Target::Name(name) = &msg.to.target {
        if !valid_name(name) {
            let refusal = Refusal::BadAddress(format!("bad name {}", loggable(name)));
            return Some(ServerMessage::refused(refusal.code(), refusal.message()));
        }
    }

    let from = match attribute_frame_caller(peer, panes).await {
        Ok(found) => found,
        Err(why) => {
            log::info!(
                "msg: sender unverified: client_id={client_id} {}{}",
                why.reason(),
                msg.pane_hint
                    .map(|hint| format!(" (pane_hint=%{hint})"))
                    .unwrap_or_default()
            );
            return Some(ServerMessage::refused(
                "sender_unverified",
                format!("{}: {}", why.reason(), why.fix()),
            ));
        }
    };
    log_caller(client_id, "MsgSend", &from);
    log_hint(msg.pane_hint, from.pane);

    let live = live_panes(panes).await;
    let (resolved, from_name) = {
        let store = store.lock().await;
        (
            store.resolve(&msg.to.target, |pane| live.contains(&pane)),
            store.name_of(from.pane).map(str::to_string),
        )
    };
    let to = match resolved {
        Ok(to) => to,
        Err(refusal) => return Some(ServerMessage::refused(refusal.code(), refusal.message())),
    };
    let (session, to_in_a_tab, from_popup, to_popup) = {
        let st = state.lock().await;
        (
            st.locate_pane(from.pane).map(|(session, _)| session),
            st.locate_pane(to).is_some(),
            st.is_popup(from.pane),
            st.is_popup(to),
        )
    };
    if from_popup {
        return Some(ServerMessage::refused("sender_unverified", POPUP_REFUSED));
    }
    if to_popup {
        return Some(ServerMessage::refused("no_such_target", POPUP_REFUSED));
    }
    let Some(session) = session else {
        return Some(ServerMessage::refused(
            "sender_unverified",
            format!("pane %{} is closing", from.pane),
        ));
    };
    if !to_in_a_tab {
        let refusal = Refusal::NoSuchTarget(format!("%{to}"));
        return Some(ServerMessage::refused(refusal.code(), refusal.message()));
    }

    let bytes = msg.body.len();
    let submit = msg.deliver != Deliver::Mailbox;
    let reply_tx = if submit {
        clients
            .lock()
            .await
            .get(&client_id)
            .map(|conn| conn.tx.clone())
    } else {
        None
    };
    // The parked-send check, the store and the enqueue are one section, so a
    // send refused `bad_request` is never stored: its sender is told it was
    // refused, and a message it cannot know about must not reach the pane.
    let enqueued_at = std::time::Instant::now();
    let deadline = submit_deadline(enqueued_at, msg.defer, msg.timeout_ms);
    let wait_until = msg.wait.then(|| enqueued_at + wait_timeout(msg.timeout_ms));
    let (sent, swept, answered, queued) = {
        let mut store = store.lock().await;
        if submit && (store.has_parked_send(client_id) || forward_parked(client_id)) {
            drop(store);
            log::info!(
                "msg: refused from=%{} to=%{to}: bad_request (a send is already waiting)",
                from.pane
            );
            return Some(ServerMessage::refused(
                "bad_request",
                "this connection already has a message waiting to be typed",
            ));
        }
        let to_name = store.name_of(to).map(str::to_string);
        let body = submit.then(|| msg.body.clone());
        let sent = store
            .send(
                Sender::Pane {
                    pane_id: from.pane,
                    name: from_name.clone(),
                    session,
                },
                to,
                msg.body,
                msg.reply_to,
                now_ms(),
                std::time::Instant::now(),
            )
            .map(|id| (id, to_name));
        let answered = if sent.is_ok() {
            store.answer_waiters(to, now_ms())
        } else {
            Vec::new()
        };
        let queued = match (&sent, body) {
            (Ok((id, _)), Some(body)) => Some(
                store.enqueue_submit(
                    crate::server::messaging::SubmitEntry {
                        id: *id,
                        pane: to,
                        deliver: msg.deliver,
                        body,
                        from: crate::server::messaging::SubmitFrom::Pane {
                            pane: from.pane,
                            name: from_name.clone(),
                        },
                        enqueued_at,
                        defer: msg.defer,
                        deadline,
                        had_reply: reply_tx.is_some(),
                        reply: reply_tx
                            .map(|tx| crate::server::messaging::Reply::new(client_id, tx, None)),
                        put_back: 0,
                        waiting_on: None,
                        wait_until,
                    },
                    config.messaging.submit_queue_len(),
                ),
            ),
            _ => None,
        };
        (sent, store.take_swept(), answered, queued)
    };
    // The panel's unread count changed, though the pane printed nothing.
    mark_agents_dirty();
    for (waiter, messages) in answered {
        log::info!(
            "msg: wait on %{to} answered n={} for client_id={}",
            messages.len(),
            waiter.client_id
        );
        let _ = waiter.tx.send(ServerMessage::MsgList {
            pane: to,
            messages,
            timed_out: false,
            marked: waiter.mark,
        });
    }
    if swept > 0 {
        log::info!("msg: swept {swept} expired message(s)");
    }
    match sent {
        Ok((id, to_name)) => {
            log::info!(
                "msg: id={id} from=%{}({}) to=%{to} bytes={bytes} deliver={:?}{} -> mailbox",
                from.pane,
                from_name.as_deref().unwrap_or("-"),
                msg.deliver,
                if msg.defer && submit { " defer" } else { "" }
            );
            let resolved = Resolved {
                host: None,
                pane_id: to,
                name: to_name,
            };
            answer_enqueued(id, to, resolved, msg.deliver, queued)
        }
        Err(refusal) => {
            log::info!(
                "msg: refused from=%{} to=%{to}: {}",
                from.pane,
                refusal.code()
            );
            Some(ServerMessage::refused(refusal.code(), refusal.message()))
        }
    }
}

/// The longest a dial may count for in a forward's deadline, whatever
/// `[messaging.links] dial_ms` says, so the server always answers a remote
/// send before the CLI gives up on it (`msg_cli::send_deadline`).
pub const LINK_DIAL_BUDGET_MS: u64 = 10_000;

/// How long a forward waits for the far server's answer once the link is up.
pub const FORWARD_ANSWER_MS: u64 = 8_000;

/// The longest the server takes to answer a send to a remote: the caller's
/// attribution, the dial, and the far server's answer. A CLI waiting less
/// would report its own timeout over the server's answer.
pub const FORWARD_BUDGET_MS: u64 =
    CALLER_ATTRIBUTION_TIMEOUT.as_millis() as u64 + LINK_DIAL_BUDGET_MS + FORWARD_ANSWER_MS;

/// How long after a send is stored its answer may take beyond a mailbox
/// send's: a submit's whole typing budget, plus the longer of a deferral and
/// a `--wait` (both count from the store). Zero for the mailbox. The CLI's
/// deadline and a forward's bound are both built on it, so they agree.
pub fn submit_answer_budget(
    deliver: Deliver,
    defer: bool,
    wait: bool,
    timeout_ms: Option<u64>,
) -> std::time::Duration {
    use std::time::Duration;
    let asked = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_SUBMIT_TIMEOUT_MS));
    let max = Duration::from_millis(MAX_WAIT_MS);
    let deferred = if defer {
        asked.clamp(Duration::from_millis(SUBMIT_HOLD_MS), max)
    } else {
        Duration::ZERO
    };
    let waited = if wait { asked.min(max) } else { Duration::ZERO };
    match deliver {
        Deliver::Mailbox => Duration::ZERO,
        Deliver::Submit | Deliver::Auto => {
            Duration::from_millis(SUBMIT_BUDGET_MS) + deferred.max(waited)
        }
    }
}

/// The local connections with a send to another server parked, waiting for
/// the far server's answer: a connection parks one, as for a local submit,
/// though nothing of it is queued here.
static FORWARDS_PARKED: std::sync::Mutex<Option<std::collections::HashSet<u64>>> =
    std::sync::Mutex::new(None);

/// A connection's parked forward, given back when it is dropped.
struct ParkedForward(u64);

impl ParkedForward {
    /// Park a forward for `client_id`, or `None` when it already has one.
    fn take(client_id: u64) -> Option<Self> {
        FORWARDS_PARKED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(Default::default)
            .insert(client_id)
            .then(|| ParkedForward(client_id))
    }
}

/// Whether connection `client_id` has a forward parked.
fn forward_parked(client_id: u64) -> bool {
    FORWARDS_PARKED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|parked| parked.contains(&client_id))
}

impl Drop for ParkedForward {
    fn drop(&mut self) {
        if let Some(parked) = FORWARDS_PARKED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            parked.remove(&self.0);
        }
    }
}

/// How long a forward waits, once attributed, for a link dialled within
/// `dial` to carry it and the far server to answer.
pub fn forward_wait(dial: std::time::Duration) -> std::time::Duration {
    dial.min(std::time::Duration::from_millis(LINK_DIAL_BUDGET_MS))
        + std::time::Duration::from_millis(FORWARD_ANSWER_MS)
}

/// Where a send to `host:<target>` goes: back over the link `host` holds
/// open to this server when there is one, else over a link this server
/// dials to `[remotes.<host>]`.
#[allow(clippy::too_many_arguments)]
async fn send_to_host(
    client_id: u64,
    host: String,
    msg: OutgoingMessage,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    store: &'static Mutex<Messaging>,
) -> Option<ServerMessage> {
    use crate::server::links::{route_for, Route};

    let inbound = links()
        .lock()
        .await
        .live_inbound(&host, std::time::Instant::now());
    match route_for(inbound, config.remotes.contains_key(&host)) {
        Route::Inbound(link_client) => {
            reply_over_inbound(
                client_id,
                host,
                link_client,
                msg,
                peer,
                state,
                panes,
                config,
                clients,
                store,
            )
            .await
        }
        Route::Dial => {
            forward_send(
                client_id, host, msg, peer, state, panes, config, clients, store,
            )
            .await
        }
        Route::Neither => Some(ServerMessage::refused(
            "route_unavailable",
            // `host` passed `valid_host` above, so it prints as itself.
            format!("no link from {host} and no [remotes.{host}]"),
        )),
    }
}

/// The sending pane of a send that leaves this server, and its session, or
/// the refusal to answer instead.
async fn forwarding_sender(
    client_id: u64,
    pane_hint: Option<PaneId>,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) -> Result<(PaneId, String), ServerMessage> {
    let from = match attribute_frame_caller(peer, panes).await {
        Ok(found) => found,
        Err(why) => {
            log::info!(
                "msg: sender unverified: client_id={client_id} {}",
                why.reason()
            );
            return Err(ServerMessage::refused(
                "sender_unverified",
                format!("{}: {}", why.reason(), why.fix()),
            ));
        }
    };
    log_caller(client_id, "MsgSend", &from);
    log_hint(pane_hint, from.pane);
    let (session, from_popup) = {
        let st = state.lock().await;
        (
            st.locate_pane(from.pane).map(|(session, _)| session),
            st.is_popup(from.pane),
        )
    };
    if from_popup {
        return Err(ServerMessage::refused("sender_unverified", POPUP_REFUSED));
    }
    match session {
        Some(session) => Ok((from.pane, session)),
        None => Err(ServerMessage::refused(
            "sender_unverified",
            format!("pane %{} is closing", from.pane),
        )),
    }
}

/// A send to `host`, which holds a live link open to this server: pushed
/// back over that connection as a [`ServerMessage::MsgInbound`], and
/// answered from the ack `host` returns, since only `host` can resolve the
/// target. Mailbox delivery only: `auto` is answered with the reason, and
/// `submit` refused, so nothing is typed by surprise.
#[allow(clippy::too_many_arguments)]
async fn reply_over_inbound(
    client_id: u64,
    host: String,
    link_client: u64,
    msg: OutgoingMessage,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    store: &'static Mutex<Messaging>,
) -> Option<ServerMessage> {
    use crate::server::messaging::Refusal;

    if msg.deliver == Deliver::Submit {
        return Some(ServerMessage::refused(
            "route_unavailable",
            format!(
                "{host}: reached over the link it holds open to this server, which delivers to the mailbox only; send it --deliver mailbox",
                host = loggable(&host)
            ),
        ));
    }
    let to_name = match &msg.to.target {
        Target::Name(name) if !valid_name(name) => {
            let refusal = Refusal::BadAddress(format!("bad name {}", loggable(name)));
            return Some(ServerMessage::refused(refusal.code(), refusal.message()));
        }
        Target::Name(name) => Some(name.clone()),
        Target::Pane(_) => None,
    };
    let (from_pane, session) =
        match forwarding_sender(client_id, msg.pane_hint, peer, state, panes).await {
            Ok(found) => found,
            Err(refusal) => return Some(refusal),
        };
    let (minted, from_name) = {
        let mut store = store.lock().await;
        let from_name = store.name_of(from_pane).map(str::to_string);
        let sender = Sender::Pane {
            pane_id: from_pane,
            name: from_name.clone(),
            session: session.clone(),
        };
        let minted = store
            .check_body(&msg.body)
            .and_then(|()| store.check_rate(&sender, std::time::Instant::now()))
            .map(|()| store.mint_id());
        (minted, from_name)
    };
    let id = match minted {
        Ok(id) => id,
        Err(refusal) => {
            log::info!(
                "msg: refused from=%{from_pane} to {}: {}",
                loggable(&host),
                refusal.code()
            );
            return Some(ServerMessage::refused(refusal.code(), refusal.message()));
        }
    };
    let bytes = msg.body.len();
    let target = msg.to.target.clone();
    let (req, acked) = links().lock().await.expect_ack(link_client);
    let push = ServerMessage::MsgInbound {
        to: msg.to.target.clone(),
        body: msg.body,
        deliver: Deliver::Mailbox,
        reply_to: msg.reply_to.map(|r| MsgRef {
            host: r.host.or_else(|| Some(host.clone())),
            id: r.id,
        }),
        origin: Origin {
            host: config.messaging.host_name(),
            pane_id: from_pane,
            name: from_name,
            session,
            id,
        },
        req,
    };
    let pushed = clients
        .lock()
        .await
        .get(&link_client)
        .is_some_and(|conn| conn.tx.send(push).is_ok());
    if !pushed {
        links().lock().await.forget_ack(req);
        return Some(ServerMessage::refused(
            "route_unavailable",
            format!("{}: the link closed", loggable(&host)),
        ));
    }
    log::info!("msg: id={id} forwarded to {host} over inbound link bytes={bytes}");
    let ack =
        tokio::time::timeout(std::time::Duration::from_millis(FORWARD_ANSWER_MS), acked).await;
    // Pushed, so `host` may have stored it: no answer is an unknown outcome.
    let ack = match ack {
        Ok(Ok(ack)) => ack,
        Ok(Err(_)) => {
            let reason = format!("{}: link lost", loggable(&host));
            return Some(forward_unknown(id, &host, &target, &reason));
        }
        Err(_) => {
            links().lock().await.forget_ack(req);
            let reason = format!("{}: no answer", loggable(&host));
            return Some(forward_unknown(id, &host, &target, &reason));
        }
    };
    if ack.code.is_some() {
        let (code, message) = crate::server::links::far_refusal(
            ack.code.as_deref(),
            ack.message.as_deref().unwrap_or(""),
        );
        log::info!("msg: id={id} refused by {host}: {code}");
        return Some(ServerMessage::refused(
            "remote_refused",
            format!("{host} refused: {code}: {message}"),
        ));
    }
    // A success names where it was stored; one that does not is no answer.
    let (Some(_), Some(pane_id)) = (ack.id, ack.pane_id) else {
        let reason = format!("{}: a bad ack (no id or pane)", loggable(&host));
        return Some(forward_unknown(id, &host, &target, &reason));
    };
    Some(ServerMessage::MsgSent {
        id,
        to: Resolved {
            host: Some(host),
            pane_id,
            name: to_name,
        },
        delivery: Delivery::Forwarded,
        reason: (msg.deliver == Deliver::Auto).then(|| "inbound_mailbox_only".to_string()),
        timed_out: false,
        final_state: None,
        req: None,
    })
}

/// What the peer is told when `inbound = "replies"` refuses a reply for its
/// id: which check failed (no forward from that pane, an id never minted
/// for it, an expired or exhausted one) stays in this server's log, so a
/// peer cannot probe which panes forwarded and which ids are live.
const NOT_EXPECTED: &str = "not a reply this server expects";

/// The refusals of messages that came over dialled links, logged once per
/// link and code in each 10 s window, with a count of the rest when the
/// window turns: the peer decides how many it sends.
#[derive(Default)]
struct RefusalLog {
    windows: HashMap<String, (std::time::Instant, std::collections::HashSet<String>, u64)>,
}

/// What [`RefusalLog::note`] says to log.
#[derive(Debug, PartialEq, Eq)]
struct RefusalNote {
    /// This refusal, the first of its code on its link in the window.
    log_this: bool,
    /// Refusals left unlogged in the window that just ended.
    unlogged_before: u64,
}

impl RefusalLog {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

    fn note(&mut self, link: &str, code: &str, now: std::time::Instant) -> RefusalNote {
        let entry = self
            .windows
            .entry(link.to_string())
            .or_insert_with(|| (now, Default::default(), 0));
        let mut unlogged_before = 0;
        if now.saturating_duration_since(entry.0) >= Self::WINDOW {
            unlogged_before = entry.2;
            *entry = (now, Default::default(), 0);
        }
        let log_this = entry.1.insert(code.to_string());
        if !log_this {
            entry.2 += 1;
        }
        RefusalNote {
            log_this,
            unlogged_before,
        }
    }
}

static INBOUND_REFUSALS: std::sync::Mutex<Option<RefusalLog>> = std::sync::Mutex::new(None);

#[cfg(test)]
mod refusal_log_tests {
    use super::{RefusalLog, RefusalNote};
    use std::time::{Duration, Instant};

    #[test]
    fn each_code_is_logged_once_per_link_per_window_and_the_rest_counted() {
        let mut log = RefusalLog::default();
        let t0 = Instant::now();
        let first = RefusalNote {
            log_this: true,
            unlogged_before: 0,
        };
        assert_eq!(log.note("mini", "origin_not_allowed", t0), first);
        for _ in 0..5 {
            assert!(!log.note("mini", "origin_not_allowed", t0).log_this);
        }
        assert_eq!(
            log.note("mini", "bad_address", t0),
            first,
            "another code is news"
        );
        assert_eq!(
            log.note("mini2", "origin_not_allowed", t0),
            first,
            "another link too"
        );
        let later = t0 + Duration::from_secs(11);
        assert_eq!(
            log.note("mini", "origin_not_allowed", later),
            RefusalNote {
                log_this: true,
                unlogged_before: 5,
            }
        );
    }
}

/// Log a refusal of a message from the dialled link `link`, by `kind`: the
/// code, or for the reply gate which of its checks failed, so each kind is
/// news once per window and a flood of one kind is a count.
fn log_inbound_refusal(link: &str, from_pane: PaneId, kind: &str, code: &str, why: &str) {
    let note = INBOUND_REFUSALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(RefusalLog::default)
        .note(link, kind, std::time::Instant::now());
    if note.unlogged_before > 0 {
        log::info!(
            "msg: inbound from {link}: {} more refusal(s) in the last 10s, not logged one by one",
            note.unlogged_before
        );
    }
    if note.log_this {
        log::info!("msg: inbound from {link}:%{from_pane} refused: {code} ({why})");
    }
}

/// Why a message that came back over a dialled link is refused before it
/// is resolved.
struct InboundRefusal {
    code: &'static str,
    /// Which check, for [`log_inbound_refusal`].
    kind: &'static str,
    /// The detail, for this server's log.
    why: String,
    /// What the peer is told, when it is not `why`.
    told: Option<&'static str>,
}

impl InboundRefusal {
    fn new(code: &'static str, kind: &'static str, why: String) -> Self {
        InboundRefusal {
            code,
            kind,
            why,
            told: None,
        }
    }
}

/// The checks that need nothing but the request and the minted ids. The
/// address checks come first, so a malformed request never counts as a
/// reply to an id.
/// `Some((pane, id))` when the message is a reply the gate admitted on that
/// id, to be counted once it is stored.
async fn gate_inbound(
    req: &crate::server::links::InboundRequest,
    store: &'static Mutex<Messaging>,
) -> Result<Option<(PaneId, MsgId)>, InboundRefusal> {
    use crate::config::Inbound;
    use crate::server::messaging::ReplyGate;

    let link = &req.link;
    if let Some(host) = req
        .reply_to
        .as_ref()
        .and_then(|r| r.host.as_deref())
        .filter(|host| !valid_host(host))
    {
        return Err(InboundRefusal::new(
            "bad_address",
            "reply_to_host",
            format!("bad reply_to host {}", loggable(host)),
        ));
    }
    if let Some(name) = req.origin.name.as_deref().filter(|name| !valid_name(name)) {
        return Err(InboundRefusal::new(
            "bad_address",
            "origin_name",
            format!("bad origin name {}", loggable(name)),
        ));
    }
    if let Target::Name(name) = &req.to {
        if !valid_name(name) {
            return Err(InboundRefusal::new(
                "bad_address",
                "target_name",
                format!("bad name {}", loggable(name)),
            ));
        }
    }
    match req.policy {
        Inbound::Off => Err(InboundRefusal::new(
            "origin_not_allowed",
            "off",
            format!("inbound = off for {link}"),
        )),
        Inbound::Replies | Inbound::Unknown => {
            let Target::Pane(pane) = req.to else {
                return Err(InboundRefusal::new(
                    "origin_not_allowed",
                    "by_name",
                    "inbound = replies admits replies to a pane, not a name".to_string(),
                ));
            };
            let Some(reply_to) = &req.reply_to else {
                return Err(InboundRefusal::new(
                    "origin_not_allowed",
                    "no_reply_to",
                    "a reply needs --reply-to".to_string(),
                ));
            };
            let gate = store
                .lock()
                .await
                .check_reply(link, pane, reply_to.id, now_ms());
            let (kind, why) = match gate {
                Ok(()) => return Ok(Some((pane, reply_to.id))),
                Err(ReplyGate::NoForward) => (
                    "no_forward",
                    format!("no message from %{pane} was forwarded over {link}"),
                ),
                Err(ReplyGate::NotMinted(id)) => {
                    ("not_minted", format!("id {id} was not minted for %{pane}"))
                }
                Err(ReplyGate::Expired(id)) => ("expired", format!("id {id} has expired")),
                Err(ReplyGate::Exhausted(id)) => (
                    "exhausted",
                    format!(
                        "id {id} has had its {} replies",
                        crate::server::messaging::MAX_REPLIES_PER_ID
                    ),
                ),
            };
            Err(InboundRefusal {
                told: Some(NOT_EXPECTED),
                ..InboundRefusal::new("origin_not_allowed", kind, why)
            })
        }
        Inbound::Any => Ok(None),
    }
}

/// A message that came back over a link this server dialled: gated by the
/// link's `inbound`, then stored as a send from that link would be. Answered
/// with the ack the link writes back. Every refusal counts against the
/// link's rate, as a stored message does.
async fn handle_inbound(
    req: crate::server::links::InboundRequest,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    store: &'static Mutex<Messaging>,
) -> crate::server::links::InboundAck {
    use crate::server::links::InboundAck;
    use crate::server::messaging::{Arrival, LinkSide, Refusal};

    let link = req.link.clone();
    let from_pane = req.origin.pane_id;
    let per_10s = config.messaging.link_rate_per_10s();
    let refuse = |code: &str, why: String| {
        log_inbound_refusal(&link, from_pane, code, code, &why);
        InboundAck::refused(code, why)
    };
    if let Err(full) = store
        .lock()
        .await
        .link_has_room(&link, per_10s, std::time::Instant::now())
    {
        return refuse(full.code(), full.message());
    }
    let charged = |store: &'static Mutex<Messaging>, link: String| async move {
        store
            .lock()
            .await
            .charge_link(&link, std::time::Instant::now());
    };
    let admitted = match gate_inbound(&req, store).await {
        Ok(admitted) => admitted,
        Err(refused) => {
            charged(store, link.clone()).await;
            log_inbound_refusal(&link, from_pane, refused.kind, refused.code, &refused.why);
            return InboundAck::refused(
                refused.code,
                refused.told.map_or(refused.why, str::to_string),
            );
        }
    };
    let live = live_panes(panes).await;
    let resolved = store
        .lock()
        .await
        .resolve(&req.to, |pane| live.contains(&pane));
    let to = match resolved {
        Ok(to) => to,
        Err(refusal) => {
            charged(store, link.clone()).await;
            return refuse(refusal.code(), refusal.message());
        }
    };
    if state.lock().await.locate_pane(to).is_none() {
        charged(store, link.clone()).await;
        let refusal = Refusal::NoSuchTarget(format!("%{to}"));
        return refuse(refusal.code(), refusal.message());
    }
    let bytes = req.body.len();
    let (sent, answered) = {
        let mut store = store.lock().await;
        let sent = store.send_arrival(
            Sender::Remote {
                host: link.clone(),
                pane_id: from_pane,
                name: req.origin.name,
            },
            to,
            req.body,
            req.reply_to,
            Arrival {
                link: &link,
                side: LinkSide::Dialled,
                link_rate_per_10s: per_10s,
                origin: MsgRef {
                    host: Some(link.clone()),
                    id: req.origin.id,
                },
            },
            now_ms(),
            std::time::Instant::now(),
        );
        match &sent {
            Ok(_) => {
                if let Some((pane, reply_to)) = admitted {
                    store.count_reply(&link, pane, reply_to);
                }
            }
            Err(Refusal::LinkRateLimited { .. }) => {}
            Err(_) => store.charge_link(&link, std::time::Instant::now()),
        }
        let answered = if sent.is_ok() {
            store.answer_waiters(to, now_ms())
        } else {
            Vec::new()
        };
        (sent, answered)
    };
    mark_agents_dirty();
    for (waiter, messages) in answered {
        let _ = waiter.tx.send(ServerMessage::MsgList {
            pane: to,
            messages,
            timed_out: false,
            marked: waiter.mark,
        });
    }
    match sent {
        Ok(id) => {
            log::info!(
                "msg: id={id} inbound from {link}:%{from_pane} to=%{to} bytes={bytes} -> mailbox"
            );
            InboundAck {
                id: Some(id),
                pane_id: Some(to),
                ..InboundAck::default()
            }
        }
        Err(refusal) => refuse(refusal.code(), refusal.message()),
    }
}

/// How many messages that came back over dialled links may wait to be
/// stored, across every link. Each link already holds at most
/// [`crate::server::links::INBOUND_IN_FLIGHT`] unanswered; this bounds the
/// sum.
const INBOUND_QUEUE: usize = 64;

/// Where every link this server dials hands the messages that come back
/// over it.
static INBOUND: std::sync::OnceLock<crate::server::links::InboundTx> = std::sync::OnceLock::new();

/// Store each message that comes back over a dialled link, one at a time.
async fn run_inbound(
    mut requests: crate::server::links::InboundRx,
    state: Arc<Mutex<ServerState>>,
    panes: Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: Arc<Config>,
) {
    while let Some((req, reply)) = requests.recv().await {
        let ack = handle_inbound(req, &state, &panes, &config, messaging()).await;
        let _ = reply.send(ack);
    }
}

/// A send to `host:<target>` over the link this server dials to
/// `[remotes.<host>]`: attributed and rate-checked here, then forwarded with
/// its origin, and stored only there. Only mailbox delivery crosses a link
/// in this build.
#[allow(clippy::too_many_arguments)]
async fn forward_send(
    client_id: u64,
    host: String,
    msg: OutgoingMessage,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    store: &'static Mutex<Messaging>,
) -> Option<ServerMessage> {
    use crate::server::links::Forward;
    use crate::server::messaging::Refusal;

    let Some(remote) = config.remotes.get(&host).cloned() else {
        return Some(ServerMessage::refused(
            "route_unavailable",
            format!("no remote named {} in [remotes]", loggable(&host)),
        ));
    };
    if let Target::Name(name) = &msg.to.target {
        if !valid_name(name) {
            let refusal = Refusal::BadAddress(format!("bad name {}", loggable(name)));
            return Some(ServerMessage::refused(refusal.code(), refusal.message()));
        }
    }
    let (from_pane, session) =
        match forwarding_sender(client_id, msg.pane_hint, peer, state, panes).await {
            Ok(found) => found,
            Err(refusal) => return Some(refusal),
        };
    // A typed send parks this connection until the far server answers, as a
    // local one does: one at a time, and never beside a local one.
    let submit = msg.deliver != Deliver::Mailbox;
    let _parked = if submit {
        let local = store.lock().await.has_parked_send(client_id);
        match ParkedForward::take(client_id).filter(|_| !local) {
            Some(parked) => Some(parked),
            None => {
                log::info!("msg: refused from=%{from_pane} to {host}: bad_request (a send is already waiting)");
                return Some(ServerMessage::refused(
                    "bad_request",
                    "this connection already has a message waiting to be typed",
                ));
            }
        }
    } else {
        None
    };
    let (minted, from_name) = {
        let mut store = store.lock().await;
        let from_name = store.name_of(from_pane).map(str::to_string);
        let sender = Sender::Pane {
            pane_id: from_pane,
            name: from_name.clone(),
            session: session.clone(),
        };
        (
            store
                .check_rate(&sender, std::time::Instant::now())
                .map(|()| store.mint_for(&host, from_pane, now_ms())),
            from_name,
        )
    };
    let id = match minted {
        Ok(id) => id,
        Err(refusal) => {
            log::info!(
                "msg: refused from=%{} to={}: {}",
                from_pane,
                loggable(&msg.to.to_string()),
                refusal.code()
            );
            return Some(ServerMessage::refused(refusal.code(), refusal.message()));
        }
    };
    let bytes = msg.body.len();
    let target = msg.to.target.clone();
    log::info!(
        "msg: id={id} forwarded to {host} (origin=%{}) bytes={bytes}",
        from_pane
    );
    let host_name = config.messaging.host_name();
    let forwarded = ClientMessage::MsgSend {
        to: Address {
            host: None,
            target: msg.to.target.clone(),
        },
        body: msg.body,
        deliver: msg.deliver,
        // The thread names the server whose id it is, as the far side sees it.
        reply_to: msg.reply_to.map(|r| MsgRef {
            host: r.host.or_else(|| Some(host.clone())),
            id: r.id,
        }),
        pane_hint: None,
        defer: msg.defer,
        timeout_ms: msg.timeout_ms,
        wait: msg.wait,
        origin: Some(Origin {
            host: host_name,
            pane_id: from_pane,
            name: from_name,
            session,
            id,
        }),
        req: None,
    };
    let spec = link_spec(&host, remote, config);
    let dial = spec.dial;
    let (reply, answer) = tokio::sync::oneshot::channel();
    let started = links().lock().await.forward(
        spec,
        Forward {
            msg: forwarded,
            reply,
        },
    );
    if let Some(dial) = started {
        tokio::spawn(crate::server::links::run_link(dial));
    }
    let budget =
        forward_wait(dial) + submit_answer_budget(msg.deliver, msg.defer, msg.wait, msg.timeout_ms);
    let forward = AwaitedForward {
        id,
        host,
        target,
        submit,
        budget,
    };
    if !submit {
        return Some(forward.answer(answer).await);
    }
    // A typed send parks: the connection goes on reading, so a second typed
    // send on it is refused while this one waits, as for a local submit.
    let tx = clients
        .lock()
        .await
        .get(&client_id)
        .map(|conn| conn.tx.clone())?;
    tokio::spawn(async move {
        let message = forward.answer(answer).await;
        let _ = tx.send(message);
        drop(_parked);
    });
    None
}

/// The link to `[remotes.<host>]`, as this server dials it.
fn link_spec(
    host: &str,
    remote: crate::config::RemoteConfig,
    config: &Config,
) -> crate::server::links::LinkSpec {
    crate::server::links::LinkSpec {
        key: host.to_string(),
        remote,
        host_name: config.messaging.host_name(),
        dial: config.messaging.links.dial(),
        ping_ms: config.messaging.links.ping_ms,
        idle: std::time::Duration::from_millis(config.messaging.links.idle_ms),
        on_inbound: INBOUND.get().cloned(),
    }
}

/// `agent status` or `agent wait` about a pane on `[remotes.<host>]`: `msg`,
/// its host already stripped, forwarded over the link, and the far server's
/// answer relayed under `host`. `wait_bound` is how long the far server may
/// take to answer beyond a status, zero for a status; without it the
/// forward would give up before the far server's wait could end. A wait
/// parks this connection, as a typed send does, and is answered by a task;
/// a status is answered here.
#[allow(clippy::too_many_arguments)]
async fn forward_agent_request(
    client_id: u64,
    host: String,
    msg: ClientMessage,
    wait_bound: std::time::Duration,
    config: &Arc<Config>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    store: &'static Mutex<Messaging>,
) -> Option<ServerMessage> {
    let Some(remote) = config.remotes.get(&host).cloned() else {
        return Some(ServerMessage::refused(
            "route_unavailable",
            format!("no remote named {} in [remotes]", loggable(&host)),
        ));
    };
    let (asked, targeted) = match &msg {
        ClientMessage::AgentStatus { target, .. } => ("status", target.is_some()),
        _ => ("wait", true),
    };
    let parked = if asked == "wait" {
        let local = store.lock().await.has_parked_send(client_id);
        match ParkedForward::take(client_id).filter(|_| !local) {
            Some(parked) => Some(parked),
            None => {
                return Some(ServerMessage::refused(
                    "bad_request",
                    "this connection already has a request waiting",
                ))
            }
        }
    } else {
        None
    };
    match &msg {
        ClientMessage::AgentWait { target, state, .. } => log::info!(
            "agent: wait forwarded to {host} target={} state={state:?}",
            loggable(&target.to_string())
        ),
        ClientMessage::AgentStatus { target, .. } => log::info!(
            "agent: status forwarded to {host} target={}",
            target
                .as_ref()
                .map_or("None".to_string(), |t| loggable(&t.to_string()))
        ),
        _ => {}
    }
    let spec = link_spec(&host, remote, config);
    let budget = forward_wait(spec.dial) + wait_bound;
    let (reply, answer) = tokio::sync::oneshot::channel();
    let started = links()
        .lock()
        .await
        .forward(spec, crate::server::links::Forward { msg, reply });
    if let Some(dial) = started {
        tokio::spawn(crate::server::links::run_link(dial));
    }
    let answered = async move {
        use crate::server::links::Undelivered;
        let answer = match tokio::time::timeout(budget, answer).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(Undelivered::Unknown(format!("{host}: link lost"))),
            Err(_) => Err(Undelivered::Unknown(format!(
                "{host}: no answer in {}s",
                budget.as_secs()
            ))),
        };
        relay_agent_answer(&host, asked, targeted, answer)
    };
    let Some(parked) = parked else {
        return Some(answered.await);
    };
    let tx = clients
        .lock()
        .await
        .get(&client_id)
        .map(|conn| conn.tx.clone())?;
    tokio::spawn(async move {
        let _ = tx.send(answered.await);
        drop(parked);
    });
    None
}

/// The longest command a far server's status entry keeps. A process may set
/// its own name, so the far side chooses it, at any length.
const FAR_COMMAND_MAX: usize = 256;

/// The highest tab index a far server's status entry keeps. No server has
/// that many tabs; past it the number is only the far side's to inflate.
const FAR_TAB_INDEX_MAX: usize = 9_999;

/// The far server's answer to a forwarded `agent status` or `agent wait`,
/// as this server answers its own caller: each pane named under `host`, a
/// name kept only when it could be one, the session and command bounded as
/// labels, and at most one entry for a request about one pane. A request
/// that was never answered is `route_unavailable` either way: unlike a
/// send, asking twice has no effect to duplicate.
fn relay_agent_answer(
    host: &str,
    asked: &str,
    targeted: bool,
    answer: crate::server::links::Answer,
) -> ServerMessage {
    use crate::server::links::Undelivered;
    match answer {
        Ok(ServerMessage::AgentStatusList { mut panes, .. }) => {
            if targeted {
                panes.truncate(1);
            }
            log::info!("agent: {host} answered status: {} entries", panes.len());
            for entry in &mut panes {
                entry.host = Some(host.to_string());
                entry.name = entry.name.take().filter(|name| valid_name(name));
                entry.session = crate::server::messaging::session_label(&entry.session);
                entry.command = entry
                    .command
                    .take()
                    .map(|command| command.chars().take(FAR_COMMAND_MAX).collect());
                entry.tab_index = entry.tab_index.min(FAR_TAB_INDEX_MAX);
            }
            ServerMessage::AgentStatusList { panes, req: None }
        }
        Ok(ServerMessage::AgentWaited {
            target,
            state,
            timed_out,
            ..
        }) => {
            log::info!("agent: {host} answered wait: {state:?} timed_out={timed_out}");
            ServerMessage::AgentWaited {
                target: Resolved {
                    host: Some(host.to_string()),
                    pane_id: target.pane_id,
                    name: target.name.filter(|name| valid_name(name)),
                },
                state,
                timed_out,
                req: None,
            }
        }
        Ok(ServerMessage::Error { message, code, .. }) => {
            let (code, message) = crate::server::links::far_refusal(code.as_deref(), &message);
            log::info!("agent: {host} refused {asked}: {code}");
            ServerMessage::refused(
                "remote_refused",
                format!("{host} refused: {code}: {message}"),
            )
        }
        Ok(other) => ServerMessage::refused(
            "route_unavailable",
            format!("{host} answered {}", other.variant_name()),
        ),
        Err(Undelivered::NotSent(reason) | Undelivered::Unknown(reason)) => {
            log::info!("agent: {asked} to {host} not answered: {reason}");
            ServerMessage::refused("route_unavailable", reason)
        }
    }
}

#[cfg(test)]
mod relay_agent_answer_tests {
    use super::{relay_agent_answer, FAR_COMMAND_MAX, FAR_TAB_INDEX_MAX};
    use crate::protocol::{PaneStatusEntry, ServerMessage};

    #[test]
    fn a_far_status_entry_is_bounded_before_it_is_relayed() {
        let far = PaneStatusEntry {
            pane_id: 7,
            session: format!("s\u{1b}[2J{}", "x".repeat(1000)),
            tab_index: usize::MAX,
            command: Some("c".repeat(10_000)),
            state: None,
            name: Some("Not A Name".into()),
            unread: 0,
            host: None,
        };
        let answer = ServerMessage::AgentStatusList {
            panes: vec![far],
            req: Some(3),
        };
        let ServerMessage::AgentStatusList { panes, req } =
            relay_agent_answer("mini", "status", true, Ok(answer))
        else {
            panic!("a list is relayed as a list");
        };
        let entry = &panes[0];
        assert_eq!(req, None);
        assert_eq!(entry.host.as_deref(), Some("mini"));
        assert_eq!(
            entry.name, None,
            "a far name that could not be one is dropped"
        );
        assert!(
            entry.session.chars().count() <= 128 && !entry.session.contains('\u{1b}'),
            "{:?}",
            entry.session
        );
        assert_eq!(
            entry.command.as_deref().map(|c| c.chars().count()),
            Some(FAR_COMMAND_MAX)
        );
        assert_eq!(entry.tab_index, FAR_TAB_INDEX_MAX);
    }
}

/// A forward written to a link, and how to answer its caller.
struct AwaitedForward {
    id: MsgId,
    host: String,
    target: Target,
    submit: bool,
    budget: std::time::Duration,
}

impl AwaitedForward {
    /// The caller's answer, from the far server's answer or its absence.
    async fn answer(
        self,
        answer: tokio::sync::oneshot::Receiver<crate::server::links::Answer>,
    ) -> ServerMessage {
        use crate::server::links::Undelivered;
        let AwaitedForward {
            id,
            host,
            target,
            submit,
            budget,
        } = self;
        let answer = match tokio::time::timeout(budget, answer).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(Undelivered::Unknown(format!("{host}: link lost"))),
            Err(_) => Err(Undelivered::Unknown(format!(
                "{host}: no answer in {}s",
                budget.as_secs()
            ))),
        };
        match answer {
            Ok(ServerMessage::MsgSent {
                to,
                delivery,
                reason,
                timed_out,
                final_state,
                ..
            }) => {
                log::info!("msg: id={id} -> {host}:%{} (forwarded)", to.pane_id);
                // A mailbox send is the far server's to store: forwarded. A
                // typed one is answered with what the far server's typing
                // came to, as a local one would be.
                let (delivery, reason) = if submit {
                    let reason =
                        reason.map(|reason| crate::server::links::far_refusal(Some(&reason), "").0);
                    (delivery, reason)
                } else {
                    (Delivery::Forwarded, None)
                };
                ServerMessage::MsgSent {
                    id,
                    to: Resolved {
                        host: Some(host),
                        pane_id: to.pane_id,
                        // The far server's word for the pane's name, kept
                        // only when it could be one.
                        name: to.name.filter(|name| valid_name(name)),
                    },
                    delivery,
                    reason,
                    timed_out,
                    final_state,
                    req: None,
                }
            }
            Ok(ServerMessage::Error { message, code, .. }) => {
                let (code, message) = crate::server::links::far_refusal(code.as_deref(), &message);
                log::info!("msg: id={id} refused by {host}: {code}");
                ServerMessage::refused(
                    "remote_refused",
                    format!("{host} refused: {code}: {message}"),
                )
            }
            Ok(other) => forward_unknown(
                id,
                &host,
                &target,
                &format!("{host} answered {}", other.variant_name()),
            ),
            Err(Undelivered::NotSent(reason)) => {
                log::info!("msg: id={id} not forwarded: {reason}");
                ServerMessage::refused("route_unavailable", reason)
            }
            Err(Undelivered::Unknown(reason)) => forward_unknown(id, &host, &target, &reason),
        }
    }
}

/// A forward that reached the link and was not answered: the far server may
/// have stored it, so the caller is told how to look before sending it again,
/// which could deliver it twice.
fn forward_unknown(id: MsgId, host: &str, target: &Target, reason: &str) -> ServerMessage {
    log::info!("msg: id={id} outcome unknown: {reason}");
    let look = match target {
        Target::Pane(pane) => format!("`remux msg read --pane %{pane} --all` on {host}"),
        Target::Name(name) => {
            format!("`remux msg read --pane %N --all` on {host}, for the pane named {name}")
        }
    };
    ServerMessage::refused(
        "forward_unknown",
        format!("{reason}; the outcome is unknown: check {look} before re-sending"),
    )
}

/// The connection's peer pid, for a log line: `?` when it cannot be read.
fn peer_pid(peer: &identity::Peer) -> String {
    use identity::PeerPin;
    peer.begin()
        .map_or_else(|_| "?".to_string(), |(pid, _)| pid.to_string())
}

/// How often expired inbound links are swept: well inside the shortest
/// expiry, twice a one-second ping.
const LINK_EXPIRY_SWEEP: std::time::Duration = std::time::Duration::from_millis(250);

/// Log each expired inbound link, and the connections to close for them.
fn log_expired(expired: Vec<(String, std::time::Duration, u64)>) -> Vec<u64> {
    expired
        .into_iter()
        .map(|(name, limit, client_id)| {
            log::info!(
                "peer link {name}: expired (no ping for {}s); closing client_id={client_id}",
                limit.as_secs_f64()
            );
            client_id
        })
        .collect()
}

/// Close each connection in `ids` from this end, so that its far side sees
/// EOF: a lapsed peer link, which the dialling side must re-dial rather than
/// keep pinging while every message on it is refused.
async fn close_connections(clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>, ids: &[u64]) {
    if ids.is_empty() {
        return;
    }
    static NO_CLOSER_WARNED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    let cls = clients.lock().await;
    for id in ids {
        // A connection already gone needs no closing; one still here that
        // has no closer stays open, which is the zombie link this exists
        // to end.
        let Some(conn) = cls.get(id) else { continue };
        match &conn.closer {
            Some(fd) => {
                // SAFETY: `fd` is an open descriptor this connection owns
                // for its whole life; shutdown affects the socket, not the
                // fd.
                unsafe {
                    libc::shutdown(std::os::fd::AsRawFd::as_raw_fd(fd), libc::SHUT_RDWR);
                }
            }
            None if !NO_CLOSER_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) => {
                log::warn!(
                    "peer link: cannot close client_id={id}, which has no second descriptor; \
                     it stays open until its far side closes it (logged once)"
                );
            }
            None => {}
        }
    }
}

/// A `PeerLink`: admit the connection as a link from the server `name`, or
/// refuse it. `None` when it is admitted, since nothing waits on that: the
/// declaring server learns it from the `Pong` to the ping it sends next.
async fn handle_peer_link(
    client_id: u64,
    name: &str,
    ping_ms: u64,
    peer: &Arc<identity::Peer>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Option<ServerMessage> {
    use crate::server::links::Declare;
    use identity::{PeerPin, PeerRefusal};

    if !valid_host(name) {
        return Some(ServerMessage::refused(
            "bad_address",
            format!("bad peer link name {}", loggable(name)),
        ));
    }
    if let Some(held) = links()
        .lock()
        .await
        .peer_name(client_id, std::time::Instant::now())
    {
        return Some(ServerMessage::refused(
            "bad_request",
            format!("this connection is already the peer link {held}"),
        ));
    }
    let seam = peer_seam();
    let server_pid = std::process::id() as i32;
    let peer = Arc::clone(peer);
    let checked = tokio::time::timeout(CALLER_ATTRIBUTION_TIMEOUT, async {
        let index = proc_index(&*panes.lock().await);
        tokio::task::spawn_blocking(move || {
            let table = identity::system_table();
            let (pid, mark) = match peer.begin() {
                Ok(begun) => begun,
                Err(why) => return (None, Err(PeerRefusal::Unverified(why))),
            };
            let verdict = identity::peer_link_check(pid, server_pid, seam, &index, &table);
            let pinned = peer.confirm(mark, &table);
            (
                Some(pid),
                pinned.map_err(PeerRefusal::Unverified).and(verdict),
            )
        })
        .await
    })
    .await;
    let (pid, verdict) = match checked {
        Ok(Ok(found)) => found,
        Ok(Err(e)) => {
            log::warn!("identity: peer link check failed: {e}");
            (
                None,
                Err(PeerRefusal::Unverified(identity::Unverified::CheckFailed)),
            )
        }
        Err(_) => (
            None,
            Err(PeerRefusal::Unverified(identity::Unverified::TimedOut)),
        ),
    };
    let pid = pid.map_or("?".to_string(), |pid| pid.to_string());
    let sshd = match verdict {
        Ok(sshd) => sshd,
        Err(refusal) => {
            log::info!(
                "peer link {name}: refused client_id={client_id} peer_pid={pid}: {} ({})",
                refusal.reason(),
                refusal.ended()
            );
            return Some(ServerMessage::refused(
                "sender_unverified",
                refusal.reason(),
            ));
        }
    };
    let ping = crate::server::links::declared_ping(ping_ms);
    let (declared, mut closing) = {
        let mut links = links().lock().await;
        let closing = log_expired(links.expire(std::time::Instant::now()));
        (links.declare(name, client_id, ping), closing)
    };
    if let Ok(Some(old)) = declared {
        log::info!("peer link {name}: expired; closing client_id={old}");
        closing.push(old);
    }
    close_connections(clients, &closing).await;
    match declared {
        Ok(_) => {
            log::info!(
                "peer link {name}: declared by client_id={client_id} peer_pid={pid} under sshd={sshd}"
            );
            None
        }
        Err(Declare::Held { retry_after }) => Some(ServerMessage::refused(
            "name_taken",
            format!(
                "peer link \"{name}\" is held by a live link; retry after {}s",
                retry_after.as_secs()
            ),
        )),
        Err(Declare::AlreadyPeer(held)) => Some(ServerMessage::refused(
            "bad_request",
            format!("this connection is already the peer link {held}"),
        )),
        Err(Declare::BadName) => Some(ServerMessage::refused(
            "bad_address",
            format!("bad peer link name {}", loggable(name)),
        )),
    }
}

/// A `MsgSend` on a peer link: no pane to attribute, since the relay is
/// sshd's child. The sender is the link's declared name and the origin's
/// pane; `origin.host` is checked and never stored, so one connection names
/// one host. Only mailbox delivery arrives over a link in this build.
#[allow(clippy::too_many_arguments)]
async fn handle_link_send(
    client_id: u64,
    link: String,
    msg: OutgoingMessage,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    store: &'static Mutex<Messaging>,
) -> Option<ServerMessage> {
    use crate::server::messaging::{Arrival, Refusal};

    let Some(origin) = msg.origin else {
        return Some(ServerMessage::refused(
            "bad_request",
            "a peer link forwards every message with its origin",
        ));
    };
    if !valid_host(&origin.host) {
        return Some(ServerMessage::refused(
            "bad_address",
            format!("bad origin host {}", loggable(&origin.host)),
        ));
    }
    if let Some(name) = origin.name.as_deref().filter(|name| !valid_name(name)) {
        return Some(ServerMessage::refused(
            "bad_address",
            format!("bad origin name {}", loggable(name)),
        ));
    }
    if msg.to.host.is_some() {
        return Some(ServerMessage::refused(
            "bad_address",
            "a forwarded message names a pane on this server",
        ));
    }
    if origin.host != link && links().lock().await.first_host_mismatch(client_id) {
        log::info!(
            "peer link {link}: an origin names host {}; stored as {link} (logged once per link)",
            loggable(&origin.host)
        );
    }
    if let Target::Name(name) = &msg.to.target {
        if !valid_name(name) {
            let refusal = Refusal::BadAddress(format!("bad name {}", loggable(name)));
            return Some(ServerMessage::refused(refusal.code(), refusal.message()));
        }
    }
    let live = live_panes(panes).await;
    let resolved = store
        .lock()
        .await
        .resolve(&msg.to.target, |pane| live.contains(&pane));
    let to = match resolved {
        Ok(to) => to,
        Err(refusal) => return Some(ServerMessage::refused(refusal.code(), refusal.message())),
    };
    if state.lock().await.locate_pane(to).is_none() {
        let refusal = Refusal::NoSuchTarget(format!("%{to}"));
        return Some(ServerMessage::refused(refusal.code(), refusal.message()));
    }
    let bytes = msg.body.len();
    let from_pane = origin.pane_id;
    let submit = msg.deliver != Deliver::Mailbox;
    let reply_tx = if submit {
        clients
            .lock()
            .await
            .get(&client_id)
            .map(|conn| conn.tx.clone())
    } else {
        None
    };
    let from = crate::server::messaging::SubmitFrom::Remote {
        link: link.clone(),
        pane: from_pane,
        name: origin.name.clone(),
        origin_id: origin.id,
    };
    let enqueued_at = std::time::Instant::now();
    let deadline = submit_deadline(enqueued_at, msg.defer, msg.timeout_ms);
    let wait_until = msg.wait.then(|| enqueued_at + wait_timeout(msg.timeout_ms));
    // As for a local send: the room to park it is checked in the section
    // that stores it, so a send refused for want of room is never stored.
    let (sent, swept, answered, queued) = {
        let mut store = store.lock().await;
        if let Some(refusal) = submit
            .then(|| store.park_refusal(client_id, &from))
            .flatten()
        {
            drop(store);
            log::info!("msg: refused from {link}:%{from_pane} to=%{to}: queue_full (parked)");
            return Some(link_full(&refusal));
        }
        let to_name = store.name_of(to).map(str::to_string);
        let body = submit.then(|| msg.body.clone());
        let sent = store
            .send_arrival(
                Sender::Remote {
                    host: link.clone(),
                    pane_id: from_pane,
                    name: origin.name,
                },
                to,
                msg.body,
                msg.reply_to,
                Arrival {
                    link: &link,
                    side: crate::server::messaging::LinkSide::Inbound,
                    link_rate_per_10s: config.messaging.link_rate_per_10s(),
                    origin: MsgRef {
                        host: Some(link.clone()),
                        id: origin.id,
                    },
                },
                now_ms(),
                std::time::Instant::now(),
            )
            .map(|id| (id, to_name));
        let answered = if sent.is_ok() {
            store.answer_waiters(to, now_ms())
        } else {
            Vec::new()
        };
        let queued = match (&sent, body) {
            (Ok((id, _)), Some(body)) => Some(
                store.enqueue_submit(
                    crate::server::messaging::SubmitEntry {
                        id: *id,
                        pane: to,
                        deliver: msg.deliver,
                        body,
                        from,
                        enqueued_at,
                        defer: msg.defer,
                        deadline,
                        had_reply: reply_tx.is_some(),
                        reply: reply_tx
                            .map(|tx| crate::server::messaging::Reply::new(client_id, tx, msg.req)),
                        put_back: 0,
                        waiting_on: None,
                        wait_until,
                    },
                    config.messaging.submit_queue_len(),
                ),
            ),
            _ => None,
        };
        (sent, store.take_swept(), answered, queued)
    };
    mark_agents_dirty();
    for (waiter, messages) in answered {
        log::info!(
            "msg: wait on %{to} answered n={} for client_id={}",
            messages.len(),
            waiter.client_id
        );
        let _ = waiter.tx.send(ServerMessage::MsgList {
            pane: to,
            messages,
            timed_out: false,
            marked: waiter.mark,
        });
    }
    if swept > 0 {
        log::info!("msg: swept {swept} expired message(s)");
    }
    match sent {
        Ok((id, to_name)) => {
            log::info!(
                "msg: id={id} from {link}:%{from_pane} to=%{to} bytes={bytes} deliver={:?}{} (client_id={client_id}) -> mailbox",
                msg.deliver,
                if msg.defer && submit { " defer" } else { "" }
            );
            let resolved = Resolved {
                host: None,
                pane_id: to,
                name: to_name,
            };
            answer_enqueued(id, to, resolved, msg.deliver, queued)
        }
        Err(refusal) => {
            log::info!(
                "msg: refused from {link}:%{from_pane} to=%{to}: {}",
                refusal.code()
            );
            Some(ServerMessage::refused(refusal.code(), refusal.message()))
        }
    }
}

/// The answer to a stored send, by what queueing it for typing did: `None`
/// when it was queued, since the submitter answers it once it is typed or
/// cannot be.
fn answer_enqueued(
    id: MsgId,
    to: PaneId,
    resolved: Resolved,
    deliver: Deliver,
    queued: Option<Result<(), crate::server::messaging::EnqueueRefusal>>,
) -> Option<ServerMessage> {
    use crate::server::messaging::EnqueueRefusal;
    use crate::server::submit::{settle, SubmitRefusal};
    match queued {
        None => Some(ServerMessage::MsgSent {
            req: None,
            id,
            to: resolved,
            delivery: Delivery::Mailbox,
            reason: None,
            timed_out: false,
            final_state: None,
        }),
        Some(Ok(())) => {
            mark_submit_dirty();
            None
        }
        // Checked in the same section before the send, so never reached.
        Some(Err(EnqueueRefusal::SendParked)) => Some(ServerMessage::refused(
            "bad_request",
            "this connection already has a message waiting to be typed",
        )),
        Some(Err(refusal @ EnqueueRefusal::LinkFull { .. })) => Some(link_full(&refusal)),
        Some(Err(EnqueueRefusal::QueueFull)) => {
            let refusal = SubmitRefusal {
                code: "queue_full",
                message: format!(
                    "%{to} already has the most messages waiting to be typed, or the most from this sender: this one is in the mailbox"
                ),
            };
            Some(answer_submit(id, resolved, settle(deliver, refusal), None))
        }
    }
}

/// The refusal for a send its peer link has no room to park.
fn link_full(refusal: &crate::server::messaging::EnqueueRefusal) -> ServerMessage {
    use crate::server::messaging::EnqueueRefusal;
    match refusal {
        EnqueueRefusal::LinkFull { link, max } => ServerMessage::refused(
            "queue_full",
            format!("link {link} already has {max} messages waiting here to be typed or answered"),
        ),
        _ => ServerMessage::refused("queue_full", "no room to park this send"),
    }
}

/// Wakes the submitter: a submit was queued, or a pane finished typing.
/// Never fired from pane output, which would wake it once per output chunk
/// while a build scrolls in another pane.
static SUBMIT_DIRTY: tokio::sync::Notify = tokio::sync::Notify::const_new();

fn mark_submit_dirty() {
    SUBMIT_DIRTY.notify_one();
}

/// How often the submitter looks again while a submit is waiting. A guard
/// wait ends at most this late, inside the windows' tolerance.
const SUBMIT_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// Type queued messages into their panes, in order per pane, holding each
/// while its pane is not ready. Runs for the life of the server. Woken by
/// [`SUBMIT_DIRTY`], and every [`SUBMIT_TICK`] while anything is queued or
/// mid-typing; idle, it parks on the `Notify` and costs nothing.
async fn run_submitter(
    state: Arc<Mutex<ServerState>>,
    panes: Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: Arc<Config>,
) {
    let store = messaging();
    loop {
        let has_work = store.lock().await.has_submit_work();
        if has_work {
            tokio::select! {
                _ = SUBMIT_DIRTY.notified() => {}
                _ = tokio::time::sleep(SUBMIT_TICK) => {}
            }
        } else {
            SUBMIT_DIRTY.notified().await;
        }
        let late = store
            .lock()
            .await
            .take_overdue_submits(std::time::Instant::now());
        for entry in late {
            let answer = crate::server::submit::overdue(
                entry.deliver,
                entry.pane,
                entry.id,
                entry.waiting_on,
            );
            reply_submit(entry, answer, None, store).await;
        }
        let heads = store.lock().await.submit_heads();
        for head in heads {
            step_submit(head, &state, &panes, &clients, &config, store).await;
        }
        step_submit_waits(&state, &panes, &config, store).await;
        step_state_waits(&state, &panes, &config, store).await;
    }
}

/// One pass over the turns being waited on: sample each pane once and step
/// its waits, answering those that are over. Two ends are answered at once,
/// `Submitted` with no final state, because the agent's turn can no longer be
/// watched: a pane whose agent is no longer in the foreground
/// (`agent_exited`; the output after it is the shell's), and a pane that is
/// no longer live (`pane_closed_after_submit`), which includes one whose
/// program exited in a tab that is not shown, and so is never reaped. The
/// reaper answers a closing pane's waits the same way, so the answer does not
/// depend on which of the two sees the close first.
///
/// The state is [`submit_state`], as a submit takes it, not the agents
/// panel's: a selector no pattern names, drawn at the end of a turn, is a
/// question to the agent's user, and the wait answers `NeedsInput` for it.
///
/// A wait does not hold its pane's queue. A later message typed into the
/// same agent during the wait makes output of its own, so the state answered
/// can be that of the later message's turn.
async fn step_submit_waits(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    store: &'static Mutex<Messaging>,
) {
    let by_pane = store.lock().await.submit_waits_by_pane();
    for (pane, ids) in by_pane {
        let sample = sample_pane(pane, state, panes, config).await;
        let now = std::time::Instant::now();
        let ended = match (sample.last_output, sample.state) {
            (None, _) => Some("pane_closed_after_submit"),
            (Some(_), None) => Some("agent_exited"),
            (Some(_), Some(_)) => None,
        };
        let mut st = store.lock().await;
        let name = st.name_of(pane).map(str::to_string);
        for id in ids {
            match (ended, sample.last_output, sample.state) {
                (Some(reason), _, _) => {
                    if let Some(wait) = st.take_submit_wait(id) {
                        answer_wait_ended(wait, name.clone(), now, reason);
                    }
                }
                (None, Some(last_output), Some(agent_state)) => {
                    if let Some((wait, step)) =
                        st.step_submit_wait(id, now, last_output, agent_state)
                    {
                        answer_submit_wait(wait, step, name.clone(), now);
                    }
                }
                (None, _, _) => {}
            }
        }
    }
}

/// A submitted message, answered with no final state and why.
fn submitted_because(id: MsgId, pane: PaneId, name: Option<String>, reason: &str) -> ServerMessage {
    ServerMessage::MsgSent {
        req: None,
        id,
        to: Resolved {
            host: None,
            pane_id: pane,
            name,
        },
        delivery: Delivery::Submitted,
        reason: Some(reason.to_string()),
        timed_out: false,
        final_state: None,
    }
}

/// Answer a wait whose turn can no longer be watched, with `reason`. The
/// message was typed and submitted, under `Submit` and `Auto` alike, so it is
/// never an error: an error would have the sender send it again.
fn answer_wait_ended(
    wait: crate::server::messaging::SubmitWait,
    name: Option<String>,
    now: std::time::Instant,
    reason: &str,
) {
    let after = now
        .saturating_duration_since(wait.machine.enter_at())
        .as_millis();
    log::info!(
        "msg: id={} wait ended after {after}ms on %{}: {reason}",
        wait.id,
        wait.pane
    );
    wait.reply
        .send(submitted_because(wait.id, wait.pane, name, reason));
}

/// Answer a wait that is over, and log how it ended. A stall is an error
/// under `Submit`. `Auto` never errors, and the message was typed, so a
/// stall there is `Submitted` with the stall as its reason.
fn answer_submit_wait(
    wait: crate::server::messaging::SubmitWait,
    step: crate::server::submit::WaitStep,
    name: Option<String>,
    now: std::time::Instant,
) {
    use crate::server::submit::{SubmitRefusal, WaitStep};
    let (id, pane) = (wait.id, wait.pane);
    let after = now
        .saturating_duration_since(wait.machine.enter_at())
        .as_millis();
    let sent = |final_state: AgentState, timed_out: bool| ServerMessage::MsgSent {
        req: None,
        id,
        to: Resolved {
            host: None,
            pane_id: pane,
            name: name.clone(),
        },
        delivery: Delivery::Submitted,
        reason: None,
        timed_out,
        final_state: Some(final_state),
    };
    let message = match step {
        WaitStep::Settled(state) => {
            log::info!("msg: id={id} wait -> {state:?} after {after}ms");
            sent(state, false)
        }
        WaitStep::TimedOut(state) => {
            log::info!("msg: id={id} wait timed out after {after}ms (last={state:?})");
            sent(state, true)
        }
        WaitStep::Stalled => {
            log::info!("msg: id={id} wait stalled after {after}ms");
            let refusal = SubmitRefusal::stalled(pane, id, wait.machine.cfg().stall);
            match wait.deliver {
                Deliver::Auto => submitted_because(id, pane, name.clone(), refusal.code),
                _ => ServerMessage::refused(refusal.code, refusal.message),
            }
        }
        WaitStep::Continue => return,
    };
    wait.reply.send(message);
}

/// One pass over one queue head: answer it, hold it, or type it. Logs only on
/// a transition (a new reason to wait, an answer), never per pass.
///
/// The steps take their locks one at a time and never nest them. Only the
/// text is typed here; its Enter, and the answer, come from a task of their
/// own, so one pane's wait for its agent to read never holds another pane's
/// head. The pane stays in flight until then, so nothing behind it is typed.
async fn step_submit(
    head: crate::server::messaging::SubmitHead,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    store: &'static Mutex<Messaging>,
) {
    use crate::server::submit::{
        guard_wait, overdue, precheck, settle, waits_while_blocked, working_first, Answer,
        GuardVerdict, SubmitRefusal,
    };

    let (pane, id) = (head.pane, head.id);
    let sample = sample_pane(pane, state, panes, config).await;
    let rule = submit_policy(config).rule_for(sample.command.as_deref().unwrap_or(""));
    let agent_state = sample.state;
    if waits_while_blocked(
        sample.live,
        sample.command.is_some(),
        agent_state,
        head.defer,
    ) {
        return hold_while_blocked(pane, id, store).await;
    }
    if let Some(answer) = precheck(
        pane,
        sample.live,
        sample.command.is_some(),
        agent_state,
        head.deliver,
    ) {
        return answer_head(pane, id, answer, None, store).await;
    }
    let working = std::time::Duration::from_millis(config.agents.working_ms);
    let now = std::time::Instant::now();
    if working_first(sample.last_human_input, now, working)
        && answer_working(pane, id, agent_state, &rule, head.deliver, store).await
    {
        return;
    }
    let attended = pane_attended(pane, state, clients).await;
    if attended && !config.messaging.submit_when_attended {
        let answer = settle(head.deliver, SubmitRefusal::attended(pane));
        return answer_head(pane, id, answer, None, store).await;
    }
    let guard = config.messaging.typing_guard();
    match guard_wait(
        sample.last_human_input,
        attended,
        head.enqueued_at,
        now,
        &guard,
    ) {
        GuardVerdict::Ready => {}
        GuardVerdict::Expired if !head.defer => {
            let answer = settle(head.deliver, SubmitRefusal::human_typing(pane));
            return answer_head(pane, id, answer, None, store).await;
        }
        // A deferred submit outlasts the guard's cap: its own deadline ends
        // it, in the sweep.
        GuardVerdict::Wait(_) | GuardVerdict::Expired => {
            if store.lock().await.set_waiting(pane, id, "human_typing") {
                log::info!("msg: id={id} submit waiting: human_typing");
            }
            return;
        }
    }
    if answer_working(pane, id, agent_state, &rule, head.deliver, store).await {
        return;
    }
    // Claimed before anything is typed, so a read that took the message first
    // (the pane's own waiting read, or an ack while the guard held it) wins
    // and it is delivered once. The pop, the hold, the claim and the
    // in-flight mark are one section, so the next pass cannot see this head
    // again, and a head the sample took too long over is not typed.
    enum Claim {
        Typing(crate::server::messaging::SubmitEntry),
        OutOfTime(crate::server::messaging::SubmitEntry),
        AlreadyRead(crate::server::messaging::SubmitEntry),
    }
    let claim = {
        let mut st = store.lock().await;
        let Some(mut entry) = st.pop_submit(pane, id) else {
            return;
        };
        if std::time::Instant::now() >= entry.deadline {
            Claim::OutOfTime(entry)
        } else if st.mark_submitted(pane, id) {
            st.begin_typing(pane, entry.reply.take());
            mark_agents_dirty();
            Claim::Typing(entry)
        } else {
            Claim::AlreadyRead(entry)
        }
    };
    let entry = match claim {
        Claim::Typing(entry) => entry,
        Claim::OutOfTime(entry) => {
            let answer = overdue(entry.deliver, pane, id, entry.waiting_on);
            reply_submit(entry, answer, None, store).await;
            mark_submit_dirty();
            return;
        }
        // The target has the message, which is what a submit is for: an
        // error would make the sender retry and deliver it twice.
        Claim::AlreadyRead(entry) => {
            reply_submit(entry, Answer::Mailbox("already_read"), None, store).await;
            mark_submit_dirty();
            return;
        }
    };
    // A mark per typing, never stored: the body was written before it.
    let envelope = match rule.envelope.then(crate::client::msg_cli::render_mark) {
        None => None,
        Some(Some(mark)) => Some(crate::server::submit::envelope(id, &entry.from, &mark)),
        Some(None) => {
            let answer = settle(entry.deliver, SubmitRefusal::no_mark(pane));
            return answer_untyped(entry, answer, None, store).await;
        }
    };
    let command = sample.command.clone().unwrap_or_default();
    let typed = type_text(
        id,
        pane,
        &sample,
        &command,
        &entry.body,
        envelope.as_ref(),
        &rule,
        entry.deliver,
        config,
        panes,
    )
    .await;
    let (bracketed, bytes, enter, text) = match typed {
        Ok(typed) => typed,
        // Nothing was written, so the entry goes back to wait: for the guard
        // again, or, deferred, for the agent's user to answer it.
        Err(TextRefusal::HumanTyped) => return requeue_untyped(entry, false, store).await,
        Err(TextRefusal::Blocked) if entry.defer => {
            return requeue_untyped(entry, false, store).await
        }
        Err(TextRefusal::Working(answer)) => {
            let policy = Some(working_policy(&rule));
            return answer_untyped(entry, answer, policy, store).await;
        }
        Err(TextRefusal::Blocked) => {
            let answer = settle(entry.deliver, SubmitRefusal::blocked(pane));
            return answer_untyped(entry, answer, None, store).await;
        }
        Err(TextRefusal::Refuse(refusal)) => {
            let answer = settle(entry.deliver, refusal);
            return answer_untyped(entry, answer, None, store).await;
        }
    };
    let (pgid, agent) = (sample.pgid, sample.agent);
    let panes = Arc::clone(panes);
    let rules = agent_rules(config);
    // A message typed into an agent that is already working, as the `queue`
    // policy allows, is taken up after the current turn. The output after it
    // is that turn's, so there is nothing a wait could tell from it.
    let wait = entry
        .wait_until
        .filter(|_| agent_state != Some(AgentState::Working))
        .map(|until| (until, config.messaging.wait_cfg(config.agents.working_ms)));
    tokio::spawn(async move {
        let typing = EndTypingOnDrop { pane, armed: true };
        let _ends = TypingEnds(Arc::clone(&text.typing));
        let outcome = deliver_text(
            pane,
            pgid,
            agent,
            &command,
            rules,
            &enter,
            rule.enter_delay,
            bracketed,
            text,
            &panes,
        )
        .await;
        match outcome {
            Typed::Submitted => {
                log::info!(
                    "msg: id={id} submitted to %{pane} (bracketed={}, {bytes} bytes)",
                    if bracketed { "on" } else { "off" }
                );
                let enter_at = std::time::Instant::now();
                // Ending the typing and parking the wait are one section, so
                // the sender's connection always has its send parked.
                let answer = {
                    let mut st = store.lock().await;
                    let reply = st.end_typing(pane);
                    match (wait, reply) {
                        (Some((until, cfg)), Some(reply)) => {
                            st.park_submit_wait(crate::server::messaging::SubmitWait {
                                id,
                                pane,
                                from: entry.from.key(),
                                deliver: entry.deliver,
                                reply,
                                machine: crate::server::submit::WaitMachine::new(
                                    enter_at, until, cfg,
                                ),
                            });
                            None
                        }
                        (_, reply) => Some((reply, st.name_of(pane).map(str::to_string))),
                    }
                };
                match answer {
                    None => log::info!("msg: id={id} waiting for %{pane}'s turn to end"),
                    Some((reply, name)) => {
                        let message = ServerMessage::MsgSent {
                            req: None,
                            id,
                            to: Resolved {
                                host: None,
                                pane_id: pane,
                                name,
                            },
                            delivery: Delivery::Submitted,
                            reason: None,
                            timed_out: false,
                            final_state: None,
                        };
                        send_reply(id, reply, message);
                    }
                }
            }
            // The agent has the message, so it stays Submitted, and an error
            // would make the sender retry and deliver it twice.
            Typed::Read(refusal) => {
                let reason = if refusal.code == "agent_blocked" {
                    "agent_blocked_read"
                } else {
                    "typed_over_read"
                };
                let reply = store.lock().await.end_typing(pane);
                reply_submit(
                    crate::server::messaging::SubmitEntry { reply, ..entry },
                    Answer::Mailbox(reason),
                    None,
                    store,
                )
                .await;
            }
            Typed::PutBack => requeue_untyped(entry, true, store).await,
            // Only the probe reached the agent, so a deferred message waits
            // for its user's answer as it would before anything was typed.
            Typed::Untyped(refusal) if entry.defer && refusal.code == "agent_blocked" => {
                requeue_untyped(entry, true, store).await
            }
            Typed::Untyped(refusal) | Typed::Refused(refusal) => {
                if refusal.code == "agent_did_not_read" {
                    log::info!(
                        "msg: id={id} not read by %{pane}; typed text discarded, left in the mailbox"
                    );
                }
                let answer = settle(entry.deliver, refusal);
                answer_untyped(entry, answer, None, store).await;
            }
        }
        typing.disarm();
        mark_submit_dirty();
    });
}

/// Hold `pane`'s deferred head `id` while its agent waits on its user. The
/// entries behind it that were not deferred are answered `agent_blocked` now,
/// as each would be at the head: nothing is typed, so the order of what is
/// typed is kept.
async fn hold_while_blocked(pane: PaneId, id: MsgId, store: &'static Mutex<Messaging>) {
    use crate::server::submit::{settle, SubmitRefusal};
    let (new, behind) = {
        let mut st = store.lock().await;
        (
            st.set_waiting(pane, id, "agent_blocked"),
            st.take_undeferred_behind(pane, id),
        )
    };
    if new {
        log::info!("msg: id={id} submit waiting: agent_blocked");
    }
    for entry in behind {
        let answer = settle(entry.deliver, SubmitRefusal::blocked(pane));
        reply_submit(entry, answer, None, store).await;
    }
}

/// Move a claimed message back to Queued, the one way a message that was
/// typed for becomes unread again. The agents panel counts it again, so the
/// list is pushed: nothing else would push it, since an agent that did not
/// read the message printed nothing.
fn unsubmit_and_dirty(st: &mut Messaging, pane: PaneId, id: MsgId) {
    if st.unsubmit(pane, id) {
        mark_agents_dirty();
    }
}

/// Answer an entry that was claimed but not written with `answer`.
async fn answer_untyped(
    entry: crate::server::messaging::SubmitEntry,
    answer: crate::server::submit::Answer,
    policy: Option<String>,
    store: &'static Mutex<Messaging>,
) {
    let reply = {
        let mut st = store.lock().await;
        unsubmit_and_dirty(&mut st, entry.pane, entry.id);
        st.end_typing(entry.pane)
    };
    reply_submit(
        crate::server::messaging::SubmitEntry { reply, ..entry },
        answer,
        policy,
        store,
    )
    .await;
    mark_submit_dirty();
}

/// Put back an entry that was claimed but not written, at the front of its
/// queue with its reply, so that it is decided again on the next pass:
/// `after_probe` when its probe had been typed.
///
/// Not when nothing is waiting for it any more. Its sender's pane closed: a
/// message typed later in its name would come from a pane that is gone. Or
/// it was deferred and its sender has stopped waiting for the answer it
/// asked for: it may otherwise wait minutes and be typed long after anyone
/// cares. Either way it stays in the mailbox.
async fn requeue_untyped(
    mut entry: crate::server::messaging::SubmitEntry,
    after_probe: bool,
    store: &'static Mutex<Messaging>,
) {
    use crate::server::submit::{settle, SubmitRefusal};
    let mut st = store.lock().await;
    unsubmit_and_dirty(&mut st, entry.pane, entry.id);
    entry.reply = st.end_typing(entry.pane);
    let id = entry.id;
    if let Some(from) = entry.from.local_pane().filter(|&from| st.is_closed(from)) {
        drop(st);
        let refusal = SubmitRefusal::sender_closed(from, id);
        let answer = settle(entry.deliver, refusal);
        reply_submit(entry, answer, None, store).await;
        mark_submit_dirty();
        return;
    }
    if entry.defer && entry.had_reply && entry.reply.is_none() {
        drop(st);
        log::info!(
            "msg: id={id} not put back: its sender stopped waiting for it; it stays in the mailbox"
        );
        mark_submit_dirty();
        return;
    }
    if after_probe && entry.defer && entry.note_put_back() {
        log::info!("msg: id={id} put back after its probe more than once");
    }
    st.requeue_front(entry);
    drop(st);
    mark_submit_dirty();
}

/// Answer `pane`'s head by its row's `while_working`, if the agent is working
/// and the row does not type into it. Returns whether it was answered.
async fn answer_working(
    pane: PaneId,
    id: MsgId,
    agent_state: Option<AgentState>,
    rule: &crate::server::submit::SubmitRule,
    deliver: Deliver,
    store: &'static Mutex<Messaging>,
) -> bool {
    let Some(answer) = crate::server::submit::on_working(pane, agent_state, rule, deliver) else {
        return false;
    };
    answer_head(pane, id, answer, Some(working_policy(rule)), store).await;
    true
}

/// The row setting that answers a working agent, as the log names it.
fn working_policy(rule: &crate::server::submit::SubmitRule) -> String {
    format!("while_working={:?}", rule.while_working).to_lowercase()
}

/// Ends a pane's typing if the task typing into it stops without doing so
/// itself, which only a panic does: a pane left in flight is never looked at
/// again, and every message queued for it would wait for ever.
struct EndTypingOnDrop {
    pane: PaneId,
    armed: bool,
}

impl EndTypingOnDrop {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for EndTypingOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let pane = self.pane;
            log::warn!("msg: typing into %{pane} stopped unfinished; ending it");
            tokio::spawn(async move {
                messaging().lock().await.end_typing(pane);
                mark_submit_dirty();
            });
        }
    }
}

/// Answer `pane`'s head `id` without typing it, if it is still the head.
async fn answer_head(
    pane: PaneId,
    id: MsgId,
    answer: crate::server::submit::Answer,
    policy: Option<String>,
    store: &'static Mutex<Messaging>,
) {
    let entry = store.lock().await.pop_submit(pane, id);
    if let Some(entry) = entry {
        reply_submit(entry, answer, policy, store).await;
        mark_submit_dirty();
    }
}

/// Answer `entry`'s parked send with `answer`, logged once.
async fn reply_submit(
    entry: crate::server::messaging::SubmitEntry,
    answer: crate::server::submit::Answer,
    policy: Option<String>,
    store: &'static Mutex<Messaging>,
) {
    let name = store.lock().await.name_of(entry.pane).map(str::to_string);
    let resolved = Resolved {
        host: None,
        pane_id: entry.pane,
        name,
    };
    let message = answer_submit(entry.id, resolved, answer, policy);
    send_reply(entry.id, entry.reply, message);
}

/// Send a submit's answer through its parked reply, or note that the sender
/// has gone.
fn send_reply(id: MsgId, reply: Option<crate::server::messaging::Reply>, message: ServerMessage) {
    match reply {
        Some(reply) => {
            reply.send(message);
        }
        None => log::info!("msg: id={id} submit answered with its sender gone (reply detached)"),
    }
}

/// Whether a client is looking at `pane`: it is the input target of a
/// session some client is attached to. `clients` alone, then `state` alone,
/// the order `handle_input` takes them in.
async fn pane_attended(
    pane: PaneId,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> bool {
    let attached: Vec<String> = clients
        .lock()
        .await
        .values()
        .filter_map(|conn| conn.session_name.clone())
        .collect();
    if attached.is_empty() {
        return false;
    }
    let st = state.lock().await;
    attached.iter().any(|name| {
        st.sessions
            .get(name)
            .is_some_and(|session| session.input_target() == Some(pane))
    })
}

/// The server's submit policy, read from the config once like
/// [`agent_rules`], and for the same reason: an edit needs `remux restart`.
static SUBMIT_POLICY: std::sync::OnceLock<crate::server::submit::SubmitPolicy> =
    std::sync::OnceLock::new();

fn submit_policy(config: &Config) -> &'static crate::server::submit::SubmitPolicy {
    SUBMIT_POLICY
        .get_or_init(|| crate::server::submit::SubmitPolicy::from_config(&config.messaging))
}

/// The reply to a submit that did not type the message, logged once. `policy`
/// names the row setting that decided, when one did.
fn answer_submit(
    id: MsgId,
    resolved: Resolved,
    answer: crate::server::submit::Answer,
    policy: Option<String>,
) -> ServerMessage {
    use crate::server::submit::Answer;
    let why = |code: &str| match &policy {
        Some(policy) => format!("{code}, {policy}"),
        None => code.to_string(),
    };
    match answer {
        Answer::Refuse(refusal) => {
            log::info!("msg: id={id} submit refused ({})", why(refusal.code));
            ServerMessage::refused(refusal.code, refusal.message)
        }
        Answer::Mailbox(reason) => {
            log::info!("msg: id={id} submit -> mailbox ({})", why(reason));
            ServerMessage::MsgSent {
                req: None,
                id,
                to: resolved,
                delivery: Delivery::Mailbox,
                reason: Some(reason.to_string()),
                timed_out: false,
                final_state: None,
            }
        }
        Answer::TimedOut(reason) => {
            log::info!(
                "msg: id={id} submit -> mailbox ({}, timed out)",
                why(reason)
            );
            ServerMessage::MsgSent {
                req: None,
                id,
                to: resolved,
                delivery: Delivery::Mailbox,
                reason: Some(reason.to_string()),
                timed_out: true,
                final_state: None,
            }
        }
    }
}

/// Why [`type_text`] did not write the text.
enum TextRefusal {
    /// A person typed into the pane after it was sampled. The submit is put
    /// back to wait for the guard again, not refused.
    HumanTyped,
    /// The agent is waiting on its user. Refused `agent_blocked`, unless
    /// the submit was deferred, when it waits again.
    Blocked,
    Refuse(crate::server::submit::SubmitRefusal),
    /// The agent is working and its row's `while_working` does not type into
    /// it: answered as the row says.
    Working(crate::server::submit::Answer),
}

/// Write the framed text into `pane`, if the program there is still the one
/// sampled (`sample.pgid`), reads its input unbuffered, is not waiting on its
/// user and is not working against its row's `while_working`, and no person
/// has typed into the pane since the sample. Returns
/// whether it was wrapped, its length and the Enter to write after it. Every
/// check and the write happen in one `panes` section, so nothing can change
/// between them. The bytes are never logged: they are the message.
#[allow(clippy::too_many_arguments)]
async fn type_text(
    id: MsgId,
    pane: PaneId,
    sample: &PaneSample,
    command: &str,
    body: &str,
    envelope: Option<&crate::server::submit::Envelope>,
    rule: &crate::server::submit::SubmitRule,
    deliver: Deliver,
    config: &Config,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) -> Result<(bool, usize, Vec<u8>, TypedText), TextRefusal> {
    use crate::config::messaging::Bracketed;
    use crate::server::submit::{
        encode, multi_line_refusal, newlines_safe, on_working, split_chunks, split_probe, Framing,
        SubmitRefusal, PASTE_CLOSE,
    };
    use nix::sys::termios::{tcgetattr, LocalFlags};
    let refuse = |refusal| Err(TextRefusal::Refuse(refusal));

    let mut ps = panes.lock().await;
    let Some(pd) = ps.get_mut(&pane).filter(|pd| !pd.exited) else {
        return refuse(SubmitRefusal::closed(pane));
    };
    if pd.last_human_input != sample.last_human_input {
        return Err(TextRefusal::HumanTyped);
    }
    let master = std::os::fd::AsFd::as_fd(&pd.pty.master_fd);
    let pgid = sample.pgid;
    if pgid.is_none() || nix::unistd::tcgetpgrp(master).ok() != pgid {
        return refuse(SubmitRefusal::not_ready(pane));
    }
    // On a pty the master's termios requests reach the slave's settings. A
    // line-buffered terminal means a shell, `read` or a password prompt is
    // reading: it would take the text as a line of its own the moment the
    // envelope's newline arrives.
    match tcgetattr(master) {
        Ok(termios) if !termios.local_flags.contains(LocalFlags::ICANON) => {}
        _ => {
            return refuse(SubmitRefusal {
                code: "agent_not_reading",
                message: format!(
                    "%{pane}'s terminal is line-buffered, so its agent is not the one reading: its message is in the mailbox"
                ),
            })
        }
    }
    let now = std::time::Instant::now();
    let agent_state = submit_state(pd, command, agent_rules(config), now);
    if agent_state == AgentState::NeedsInput {
        return Err(TextRefusal::Blocked);
    }
    if let Some(answer) = on_working(pane, Some(agent_state), rule, deliver) {
        return Err(TextRefusal::Working(answer));
    }
    let framing = match rule.bracketed {
        Bracketed::On => Framing::Bracketed,
        Bracketed::Off => Framing::Plain,
        Bracketed::Auto if pd.screen.bracketed_paste => Framing::Bracketed,
        Bracketed::Auto => Framing::Plain,
    };
    let paste_live = pd.screen.bracketed_paste;
    if !newlines_safe(framing, paste_live) {
        if let Some(refusal) = multi_line_refusal(body) {
            return refuse(refusal);
        }
    }
    // The terminal from the slave side, where its input queue is: without
    // it nothing can tell whether the agent read the message.
    let Some(slave) = pd
        .pty
        .slave_path
        .as_deref()
        .and_then(open_slave)
        .filter(|slave| is_the_slave(slave, pd.pty.slave_dev))
    else {
        return refuse(SubmitRefusal::not_read(pane));
    };
    let (text, enter) = encode(
        body,
        framing,
        paste_live,
        rule.focus_in,
        &rule.enter,
        envelope,
    );
    let bytes = text.len();
    let (probe, body) = split_probe(text, framing, rule.focus_in);
    let bracketed = framing == Framing::Bracketed;
    let tail = if bracketed { PASTE_CLOSE } else { &[] };
    let content = body.strip_suffix(tail).unwrap_or(&body);
    let chunks = split_chunks(content, tail, CHUNK_MAX);
    if chunks.len() > MAX_CHUNKS {
        return refuse(SubmitRefusal::too_many_lines(pane, MAX_CHUNKS));
    }
    let typing = Arc::new(SubmitTyping {
        id,
        cancel: Arc::new(pty::WriteCancel::default()),
        slave: Arc::new(slave),
        bracketed,
        opener_read: AtomicBool::new(false),
        last_sent: AtomicBool::new(false),
        all_read: AtomicBool::new(false),
        taken_over: AtomicBool::new(false),
        ended: AtomicBool::new(false),
    });
    let written = pd.send_tracked(probe, Arc::clone(&typing.cancel));
    pd.typing = Some(Arc::downgrade(&typing));
    Ok((
        framing == Framing::Bracketed,
        bytes,
        enter,
        TypedText {
            written,
            chunks,
            typing,
            typed_at: now,
        },
    ))
}

/// How long the agent has to read a message's probe before it is judged not
/// reading and the body is not written. Short, because an agent that is not
/// reading may be about to exit, and the probe must be flushed before the
/// shell after it reads the terminal.
const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(750);

/// How long the agent may go without reading any of the message, once the
/// probe is read, before it is judged not to be reading it. Every byte it
/// reads starts the time again, so a slow agent is never judged by this,
/// only one that stopped.
const CONSUME_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// How often the pane is checked while waiting for it to read the text.
const CONSUME_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// How often the pane is checked while a chunk of the body is being read.
/// Short, because every chunk waits for one of these checks before the next
/// is written, and a long body is many chunks.
const CHUNK_POLL: std::time::Duration = std::time::Duration::from_millis(1);

/// How often, between chunks, the screen is read for a prompt. Reading it is
/// the costly part of a check, and the chunks need it only before each write.
const PROMPT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The largest chunk of a body written at once. Small enough to sit whole in
/// the terminal's input queue on Linux and macOS, so that the queue's count
/// shows every byte of it the agent reads.
const CHUNK_MAX: usize = 1024;

/// The longest the agent may take to read a whole body, however steadily.
/// Past it the rest is withdrawn and the message is answered `agent_too_slow`.
/// A body at the 64 KiB limit read at 2.2 KiB a second still fits.
const BODY_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// What one chunk is allowed to cost, written and read, in working out
/// [`MAX_CHUNKS`]: more than a chunk costs against an agent that reads at
/// once on a machine with every CPU busy twice over, so that a body at the
/// limit still fits in [`BODY_CAP`] there.
const CHUNK_COST: std::time::Duration = std::time::Duration::from_millis(20);

/// The most chunks a body may take: as many as fit in [`BODY_CAP`] at
/// [`CHUNK_COST`] each. A body of more lines is refused `too_many_lines`
/// before anything is typed, rather than run into the cap part-way through.
const MAX_CHUNKS: usize = (BODY_CAP.as_millis() / CHUNK_COST.as_millis()) as usize;

/// How long a cancelled write is given to stop before its queue is flushed.
const CANCEL_ACK: std::time::Duration = std::time::Duration::from_millis(1500);

/// How long a deferred submit may wait when its sender names no timeout.
pub const DEFAULT_SUBMIT_TIMEOUT_MS: u64 = 600_000;

/// When a submit queued at `enqueued_at` is answered as out of time. A
/// deferred one waits as long as its sender asked, between [`SUBMIT_HOLD_MS`]
/// and [`MAX_WAIT_MS`]: it waits on a person answering their agent, which
/// can take minutes, and it is never given less time than a submit that did
/// not ask to wait. Any other is held at most [`SUBMIT_HOLD_MS`].
fn submit_deadline(
    enqueued_at: std::time::Instant,
    defer: bool,
    timeout_ms: Option<u64>,
) -> std::time::Instant {
    let ms = if defer {
        timeout_ms
            .unwrap_or(DEFAULT_SUBMIT_TIMEOUT_MS)
            .clamp(SUBMIT_HOLD_MS, MAX_WAIT_MS)
    } else {
        SUBMIT_HOLD_MS
    };
    enqueued_at + std::time::Duration::from_millis(ms)
}

/// How long after a message is sent `--wait` watches for the agent's turn
/// to end: `timeout_ms`, or [`DEFAULT_SUBMIT_TIMEOUT_MS`], at most
/// [`MAX_WAIT_MS`]. Measured from the send, as the CLI's `--timeout` is.
fn wait_timeout(timeout_ms: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_millis(
        timeout_ms
            .unwrap_or(DEFAULT_SUBMIT_TIMEOUT_MS)
            .min(MAX_WAIT_MS),
    )
}

/// The longest a submit is held in its queue before it is typed: the typing
/// guard at its cap, and one tick for the submitter to see it clear. Past
/// this the submit is answered `timeout` and nothing is typed.
pub const SUBMIT_HOLD_MS: u64 =
    crate::config::messaging::MAX_TYPING_GUARD_MS + SUBMIT_TICK.as_millis() as u64;

/// The longest from a submit's arrival to its answer: held for at most
/// [`SUBMIT_HOLD_MS`]; its probe read within [`PROBE_DEADLINE`]; its body read
/// within [`BODY_CAP`]; then the Enter delay at its cap and [`CONSUME_DEADLINE`]
/// for the Enter's own checks; and [`CANCEL_ACK`] for a refusal's cancel. The
/// CLI waits this long beyond its usual reply deadline, so a submit is never
/// reported as having no answer while the server is still able to type it: a
/// retry after such a report would deliver the message twice.
pub const SUBMIT_BUDGET_MS: u64 = SUBMIT_HOLD_MS
    + PROBE_DEADLINE.as_millis() as u64
    + BODY_CAP.as_millis() as u64
    + crate::config::messaging::MAX_ENTER_DELAY_MS
    + CONSUME_DEADLINE.as_millis() as u64
    + CANCEL_ACK.as_millis() as u64;

/// A message being typed into a pane, as [`PaneData::send_human_input`] sees
/// it. The pane holds it weakly: the task typing the message owns it, so the
/// slave descriptor it carries is closed when that task ends. A pane that
/// kept it open would hold its own terminal open, and its master would never
/// read the end of file that closes the pane when its program exits.
struct SubmitTyping {
    id: MsgId,
    /// Stops the message's writes that have not happened yet.
    cancel: Arc<crate::server::pty::WriteCancel>,
    /// The pane's terminal from the slave side, where its input queue is.
    slave: Arc<std::fs::File>,
    /// The message is wrapped in a paste.
    bracketed: bool,
    /// The agent read the paste opener: a paste is open in it until it reads
    /// the closer.
    opener_read: AtomicBool,
    /// The last chunk of the body went to the writer.
    last_sent: AtomicBool,
    /// The agent read the whole body, the paste's closer included.
    all_read: AtomicBool,
    /// A person's key took the pane back before the message was submitted,
    /// and the rest of it was cancelled and flushed.
    taken_over: AtomicBool,
    /// The message is no longer being typed: decided, in the same `panes`
    /// section as its last write when it had one.
    ended: AtomicBool,
}

impl SubmitTyping {
    fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
    }

    /// Whether the agent is inside the message's paste: it read the opener
    /// and not yet the closer.
    fn paste_open(&self) -> bool {
        self.bracketed
            && self.opener_read.load(Ordering::SeqCst)
            && !self.all_read.load(Ordering::SeqCst)
    }
}

/// Marks a [`SubmitTyping`] ended when the task typing it stops, however it
/// stops, so a later key is never taken for one into this message.
struct TypingEnds(Arc<SubmitTyping>);

impl Drop for TypingEnds {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// The probe's write, as `type_text` handed it to the pane's writer, and the
/// rest of the message in the chunks it is written in.
struct TypedText {
    written: tokio::sync::oneshot::Receiver<bool>,
    chunks: Vec<Vec<u8>>,
    typing: Arc<SubmitTyping>,
    /// When the probe was handed to the writer: a person's key after this was
    /// pressed while the message was being typed.
    typed_at: std::time::Instant,
}

/// How typing a message ended, once its probe was written.
enum Typed {
    Submitted,
    /// The agent read the whole message, and it was not submitted: a person
    /// typed, or the agent began waiting on its user. The message is in the
    /// agent's input.
    Read(crate::server::submit::SubmitRefusal),
    /// Nothing of the message but the probe reached the agent, because a
    /// person typed: it waits for the typing guard again.
    PutBack,
    /// Nothing of the message but the probe was written.
    Untyped(crate::server::submit::SubmitRefusal),
    /// Refused once the body was written. What the agent did not read was
    /// cancelled and flushed.
    Refused(crate::server::submit::SubmitRefusal),
}

/// Type the rest of a message whose probe `type_text` wrote, and its Enter.
///
/// The probe must be read first: written, and the input queue found empty
/// on two checks [`CONSUME_POLL`] apart, within [`PROBE_DEADLINE`]. An agent
/// that does not read it is not reading at all, and its probe is flushed with
/// nothing else written.
///
/// The body follows in chunks, each written only once the one before has
/// been read whole (the queue empty on two checks), in the same `panes`
/// section as the checks that allow it: the agent still the reader, not
/// waiting on its user. At most one chunk is ever unread, so an agent that
/// stops reading and exits leaves the shell one chunk, never the rest. An
/// agent that pauses is waited for: only [`CONSUME_DEADLINE`] without a byte
/// read refuses it, or [`BODY_CAP`] for the whole body.
///
/// The Enter is written once the Enter delay has passed after the last chunk
/// was read and the queue is still empty on two checks, in the same `panes`
/// section as the checks; it is refused when the agent has begun waiting on
/// its user, which the Enter would answer.
///
/// On a refusal the rest of the message is cancelled FIRST and the writer's
/// stop awaited, then the queue is flushed: flushed first, the queue would
/// only make room for the writer to write the tail into it. A paste the
/// agent is still inside is then closed, if the agent is still the reader. A
/// person's key before the message is submitted has done all of that
/// already, and its own flush is not repeated, since that would discard the
/// key.
#[allow(clippy::too_many_arguments)]
async fn deliver_text(
    pane: PaneId,
    pgid: Option<nix::unistd::Pid>,
    agent: Option<nix::unistd::Pid>,
    command: &str,
    rules: &crate::server::agents::AgentRules,
    enter: &[u8],
    delay: std::time::Duration,
    bracketed: bool,
    typed: TypedText,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) -> Typed {
    use crate::server::submit::{SubmitRefusal, PASTE_CLOSE};
    use tokio::sync::oneshot::error::TryRecvError;

    let TypedText {
        mut written,
        chunks,
        typing,
        typed_at,
    } = typed;
    let slave = &*typing.slave;
    let typed_after = |pd: &PaneData| pd.last_human_input.is_some_and(|t| t > typed_at);
    let taken_over = || typing.taken_over.load(Ordering::SeqCst);
    let close_paste = |pd: &PaneData| {
        if typing.paste_open() {
            pd.send_input(PASTE_CLOSE);
        }
    };

    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
    let mut probe_written = false;
    let mut read_once = false;
    let mut agent_reading = true;
    let probed = loop {
        if taken_over() {
            return Typed::PutBack;
        }
        if !probe_written {
            match written.try_recv() {
                Ok(true) => probe_written = true,
                Ok(false) | Err(TryRecvError::Closed) => break Err(SubmitRefusal::not_read(pane)),
                Err(TryRecvError::Empty) => {}
            }
        }
        {
            let ps = panes.lock().await;
            let Some(pd) = ps.get(&pane).filter(|pd| !pd.exited) else {
                break Err(SubmitRefusal::closed(pane));
            };
            if !agent_reads(pd, pgid, agent) {
                agent_reading = false;
                break Err(SubmitRefusal::not_read(pane));
            }
            let read = probe_written && unread_input(slave) == Some(0);
            if read {
                typing.opener_read.store(bracketed, Ordering::SeqCst);
            }
            if read && read_once {
                if typed_after(pd) {
                    typing.end();
                    close_paste(pd);
                    return Typed::PutBack;
                }
                let now = std::time::Instant::now();
                if submit_state(pd, command, rules, now) == AgentState::NeedsInput {
                    typing.end();
                    close_paste(pd);
                    return Typed::Untyped(SubmitRefusal::blocked(pane));
                }
                written = pd.send_tracked(chunks[0].clone(), Arc::clone(&typing.cancel));
                typing.last_sent.store(chunks.len() == 1, Ordering::SeqCst);
                break Ok(());
            }
            read_once = read;
        }
        if tokio::time::Instant::now() >= deadline {
            break Err(SubmitRefusal::not_read(pane));
        }
        tokio::time::sleep(CONSUME_POLL).await;
    };
    if let Err(refusal) = probed {
        withdraw(
            &typing,
            &mut written,
            !probe_written,
            agent_reading,
            pane,
            panes,
        )
        .await;
        return Typed::Untyped(refusal);
    }

    let body_deadline = tokio::time::Instant::now() + BODY_CAP;
    let mut next = 1;
    let mut done = false;
    let mut human = false;
    let mut read_once = false;
    let mut level: Option<usize> = None;
    let mut progress_at = tokio::time::Instant::now();
    let mut enter_at: Option<tokio::time::Instant> = None;
    let mut prompt_checked = tokio::time::Instant::now();
    let refusal = loop {
        if taken_over() {
            let refusal = SubmitRefusal::typed_over(pane);
            return if typing.all_read.load(Ordering::SeqCst) {
                Typed::Read(refusal)
            } else {
                Typed::Refused(refusal)
            };
        }
        // Waiting for the writer is part of this pass's wait, so a chunk
        // written last pass is checked the moment its last byte is out.
        let pass_start = tokio::time::Instant::now();
        if !done {
            match tokio::time::timeout(CHUNK_POLL, &mut written).await {
                Ok(Ok(true)) => done = true,
                Ok(Ok(false) | Err(_)) if !taken_over() => {
                    break SubmitRefusal::stopped_reading(pane)
                }
                _ => {}
            }
        }
        let tick = tokio::time::Instant::now();
        {
            let ps = panes.lock().await;
            let Some(pd) = ps.get(&pane).filter(|pd| !pd.exited) else {
                break SubmitRefusal::closed(pane);
            };
            human |= typed_after(pd);
            if !agent_reads(pd, pgid, agent) {
                agent_reading = false;
                break SubmitRefusal::read_by_another(pane);
            }
            let unread = unread_input(slave);
            let empty = done && unread == Some(0);
            if empty || tick.duration_since(prompt_checked) >= PROMPT_POLL {
                prompt_checked = tick;
                let now = std::time::Instant::now();
                if submit_state(pd, command, rules, now) == AgentState::NeedsInput {
                    if next == chunks.len() && empty {
                        typing.all_read.store(true, Ordering::SeqCst);
                    }
                    break SubmitRefusal::blocked(pane);
                }
            }
            if let (Some(n), Some(before)) = (unread, level) {
                if n < before {
                    progress_at = tick;
                }
            }
            level = unread;
            match enter_at {
                // Between chunks, one empty reading means the chunk was read.
                // The last chunk gets a second reading, before the Enter.
                None if empty && next < chunks.len() => {
                    written = pd.send_tracked(chunks[next].clone(), Arc::clone(&typing.cancel));
                    next += 1;
                    typing
                        .last_sent
                        .store(next == chunks.len(), Ordering::SeqCst);
                    done = false;
                    read_once = false;
                    level = None;
                    progress_at = tick;
                }
                None if empty && read_once && next == chunks.len() => {
                    typing.all_read.store(true, Ordering::SeqCst);
                    enter_at = Some(tick + delay);
                    read_once = false;
                }
                None => read_once = empty,
                Some(at) if tick >= at && empty && read_once => {
                    typing.end();
                    if human {
                        break SubmitRefusal::typed_over(pane);
                    }
                    pd.send_input(enter);
                    return Typed::Submitted;
                }
                Some(at) => read_once = empty && tick >= at,
            }
        }
        match enter_at {
            None if tick >= body_deadline => break SubmitRefusal::too_slow(pane, BODY_CAP),
            None if tick.duration_since(progress_at) >= CONSUME_DEADLINE => {
                break SubmitRefusal::stopped_reading(pane)
            }
            Some(at) if tick >= at + CONSUME_DEADLINE => {
                break SubmitRefusal::stopped_reading(pane)
            }
            _ => {}
        }
        if enter_at.is_some() {
            tokio::time::sleep(CONSUME_POLL).await;
        } else {
            tokio::time::sleep_until(pass_start + CHUNK_POLL).await;
        }
    };
    let read_all = typing.all_read.load(Ordering::SeqCst);
    let read_reason = matches!(refusal.code, "agent_blocked" | "human_typing");
    if read_all && agent_reading && read_reason {
        typing.end();
        return Typed::Read(refusal);
    }
    if withdraw(&typing, &mut written, !done, agent_reading, pane, panes).await {
        return Typed::Refused(SubmitRefusal::typed_over(pane));
    }
    Typed::Refused(refusal)
}

/// Withdraw what of a message the agent did not read: cancel the writes,
/// await the writer's stop, flush the queue, and close a paste the agent is
/// still inside, if it is still the reader. Returns whether a person's key
/// had already done this, in which case none of it is repeated.
async fn withdraw(
    typing: &SubmitTyping,
    written: &mut tokio::sync::oneshot::Receiver<bool>,
    writing: bool,
    agent_reading: bool,
    pane: PaneId,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) -> bool {
    typing.end();
    typing.cancel.cancel();
    if writing {
        let _ = tokio::time::timeout(CANCEL_ACK, &mut *written).await;
    }
    if typing.taken_over.load(Ordering::SeqCst) {
        return true;
    }
    flush_unread(typing.id, pane, &typing.slave, agent_reading);
    if agent_reading && typing.paste_open() {
        if let Some(pd) = panes.lock().await.get(&pane).filter(|pd| !pd.exited) {
            pd.send_input(crate::server::submit::PASTE_CLOSE);
        }
    }
    false
}

/// Flush `pane`'s input queue. When there was nothing left to flush because
/// the agent had stopped reading, another program may have read the message,
/// and the log says so.
fn flush_unread(id: MsgId, pane: PaneId, slave: &std::fs::File, agent_reading: bool) {
    let unread = unread_input(slave).unwrap_or(0);
    if let Err(e) = nix::sys::termios::tcflush(slave, nix::sys::termios::FlushArg::TCIFLUSH) {
        log::warn!("msg: flushing %{pane}'s unread input failed: {e}");
    } else if unread == 0 && !agent_reading {
        log::warn!(
            "msg: id={id} %{pane}'s agent stopped reading with the message unread, and nothing was \
             left to flush: another program may have read it"
        );
    }
}

/// Whether the program sampled as `pgid` is still the one reading `pd`'s
/// terminal, unbuffered. The agent itself (`agent`, the process whose name
/// matched) must be alive as well as its group in the foreground: a job
/// outlives any one member (`agent | cat` keeps the terminal while `cat`
/// runs), and a member that goes on reading the terminal raw after the agent
/// is gone is not the agent. The agent rather than the group's leader,
/// because a launcher that starts the agent and exits leaves a live agent in
/// a group without its leader.
fn agent_reads(
    pd: &PaneData,
    pgid: Option<nix::unistd::Pid>,
    agent: Option<nix::unistd::Pid>,
) -> bool {
    let master = std::os::fd::AsFd::as_fd(&pd.pty.master_fd);
    let same = pgid.is_some()
        && nix::unistd::tcgetpgrp(master).ok() == pgid
        && agent.is_some_and(|pid| nix::sys::signal::kill(pid, None).is_ok());
    same && nix::sys::termios::tcgetattr(master).is_ok_and(|t| {
        !t.local_flags
            .contains(nix::sys::termios::LocalFlags::ICANON)
    })
}

/// Whether `slave` is the pane's own terminal: the path was read at spawn,
/// and a path is a name, which a later terminal could have been given.
fn is_the_slave(slave: &std::fs::File, slave_dev: Option<libc::dev_t>) -> bool {
    slave_dev.is_some_and(|dev| {
        nix::sys::stat::fstat(std::os::fd::AsRawFd::as_raw_fd(slave))
            .is_ok_and(|st| st.st_rdev == dev)
    })
}

/// The pane's terminal from the slave side, where its input queue is. Opened
/// without becoming the server's controlling terminal and without blocking.
fn open_slave(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)
        .ok()
}

/// Bytes in the slave's input queue: typed, and not yet read by the program.
///
/// Linux moves bytes written to the master into this queue as deferred work,
/// so a count taken right after a write can be 0 for an unread chunk. Under
/// CPU load that lasts many milliseconds. Polling the slave first makes the
/// count exact, because the line discipline's poll finishes that deferred
/// work before it looks at the queue. The poll reads nothing, so the agent
/// loses no input. macOS writes into the queue directly and needs no such
/// barrier, but the poll is harmless there.
///
/// Callers hold `panes`, so the poll can hold it while the kernel finishes
/// that one piece of deferred work. A poll that fails answers `None`, which
/// no caller takes as read.
fn unread_input(slave: &std::fs::File) -> Option<usize> {
    let mut queued: libc::c_int = 0;
    let fd = std::os::fd::AsRawFd::as_raw_fd(slave);
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for an fd `slave` keeps open, and no wait.
    if unsafe { libc::poll(&mut pfd, 1, 0) } < 0 {
        return None;
    }
    (unsafe { libc::ioctl(fd, libc::FIONREAD, &mut queued) } == 0).then(|| queued.max(0) as usize)
}

/// The mailbox a read or an ack is about, and the caller's own pane, if it is
/// in one.
///
/// `pane` when given, else the caller's own pane. A caller outside any pane
/// must name one. A pane that is not live, is in no tab, or is a popup is
/// `no_such_target`; a closed pane is not live, so this also refuses the
/// tombstoned ones.
async fn mailbox_of(
    client_id: u64,
    verb: &str,
    pane: Option<PaneId>,
    pane_hint: Option<PaneId>,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) -> Result<(PaneId, Option<PaneId>), ServerMessage> {
    let caller = attribute_frame_caller(peer, panes).await;
    if let Ok(found) = &caller {
        log_caller(client_id, verb, found);
        log_hint(pane_hint, found.pane);
    }
    if let Some(refused) = unverified_refusal(&caller) {
        return Err(refused);
    }
    let pane = match (pane, &caller) {
        (Some(pane), _) => pane,
        (None, Ok(found)) => found.pane,
        (None, Err(why)) => {
            return Err(ServerMessage::refused(
                "not_in_pane",
                format!("not in a pane ({}): pass --pane %N", why.reason()),
            ))
        }
    };
    check_addressable(pane, state, panes).await?;
    Ok((pane, caller.ok().map(|found| found.pane)))
}

/// Refuse a pane that mail, a name or a status request cannot address: one
/// that is not live (closed or exited), is in no tab, or is a popup.
async fn check_addressable(
    pane: PaneId,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) -> Result<(), ServerMessage> {
    let live = live_panes(panes).await.contains(&pane);
    let (in_a_tab, popup) = {
        let st = state.lock().await;
        (st.locate_pane(pane).is_some(), st.is_popup(pane))
    };
    if popup {
        return Err(ServerMessage::refused("no_such_target", POPUP_REFUSED));
    }
    if !live || !in_a_tab {
        return Err(ServerMessage::refused(
            "no_such_target",
            format!("no such target: %{pane}"),
        ));
    }
    Ok(())
}

/// Name the caller's pane, or `pane` for a caller outside any pane.
async fn handle_agent_name(
    client_id: u64,
    request: NameRequest,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    store: &Mutex<Messaging>,
) -> ServerMessage {
    let caller = attribute_frame_caller(peer, panes).await;
    if let Ok(found) = &caller {
        log_caller(client_id, "AgentName", found);
        log_hint(request.pane_hint, found.pane);
    }
    if let Some(refused) = unverified_refusal(&caller) {
        return refused;
    }
    let pane = match (request.pane, &caller) {
        (Some(_), Ok(found)) => {
            return ServerMessage::refused(
                "bad_address",
                format!(
                    "--pane is for callers outside a pane; you are %{}",
                    found.pane
                ),
            )
        }
        (Some(pane), Err(_)) => pane,
        (None, Ok(found)) => found.pane,
        (None, Err(why)) => {
            return ServerMessage::refused(
                "not_in_pane",
                format!("not in a pane ({}): pass --pane %N", why.reason()),
            )
        }
    };
    if let Err(refused) = check_addressable(pane, state, panes).await {
        return refused;
    }
    let named = store.lock().await.set_name(pane, request.name.as_deref());
    match named {
        Ok(()) => {
            mark_agents_dirty();
            match &request.name {
                Some(name) => log::info!("msg: pane %{pane} named {}", loggable(name)),
                None => log::info!("msg: pane %{pane} unnamed"),
            }
            ServerMessage::AgentNamed {
                pane,
                name: request.name,
            }
        }
        Err(refusal) => ServerMessage::refused(refusal.code(), refusal.message()),
    }
}

/// Every agent, named pane and pane with mail, or only `target`. Answered per
/// request and never on a timer, so it logs nothing per pane.
/// The pane `address` names on this server, among the `live` ones, or the
/// refusal to answer with: a bad host or name, a remote server, or no such
/// pane.
async fn resolve_target(
    address: &Address,
    live: &std::collections::HashSet<PaneId>,
    store: &Mutex<Messaging>,
) -> Result<PaneId, ServerMessage> {
    use crate::server::messaging::Refusal;
    if let Some(host) = &address.host {
        if !valid_host(host) {
            let refusal = Refusal::BadAddress(format!("bad host {}", loggable(host)));
            return Err(ServerMessage::refused(refusal.code(), refusal.message()));
        }
        return Err(ServerMessage::refused(
            "bad_address",
            "a forwarded request must name a pane on this server",
        ));
    }
    if let Target::Name(name) = &address.target {
        if !valid_name(name) {
            let refusal = Refusal::BadAddress(format!("bad name {}", loggable(name)));
            return Err(ServerMessage::refused(refusal.code(), refusal.message()));
        }
    }
    store
        .lock()
        .await
        .resolve(&address.target, |pane| live.contains(&pane))
        .map_err(|refusal| ServerMessage::refused(refusal.code(), refusal.message()))
}

/// `agent wait`: answer at once when `target`'s agent is already in `want`,
/// else park the caller for the submitter to answer. The state is the one a
/// submit takes ([`submit_state`]), sampled from the one pane, so a selector
/// no pattern names counts as `NeedsInput` here as it does for a submit.
#[allow(clippy::too_many_arguments)]
async fn handle_agent_wait(
    client_id: u64,
    target: Address,
    want: AgentState,
    timeout_ms: Option<u64>,
    req: Option<u64>,
    link: Option<&str>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    store: &'static Mutex<Messaging>,
) -> Option<ServerMessage> {
    let live = live_panes(panes).await;
    let pane = match resolve_target(&target, &live, store).await {
        Ok(pane) => pane,
        Err(refusal) => return Some(refusal),
    };
    let sample = sample_pane(pane, state, panes, config).await;
    if !sample.live {
        return Some(ServerMessage::refused(
            "no_such_target",
            format!("no such target: %{pane}"),
        ));
    }
    let Some(now_state) = sample.state else {
        return Some(ServerMessage::refused(
            "agent_not_ready",
            format!("%{pane} is not running an agent this server knows"),
        ));
    };
    let name = store.lock().await.name_of(pane).map(str::to_string);
    if now_state == want {
        return Some(agent_waited(pane, name, now_state, false));
    }
    let tx = clients.lock().await.get(&client_id).map(|c| c.tx.clone())?;
    let deadline = std::time::Instant::now() + wait_timeout(timeout_ms);
    {
        let mut st = store.lock().await;
        // A peer link parks many, one per `req`, up to its cap; a local
        // connection parks one.
        match link {
            Some(link) => {
                if let Some(full) = st.link_park_refusal(client_id, link) {
                    return Some(link_full(&full));
                }
            }
            None if st.has_parked_send(client_id) || forward_parked(client_id) => {
                return Some(ServerMessage::refused(
                    "bad_request",
                    "this connection already has a request waiting",
                ));
            }
            None => {}
        }
        let wait = crate::server::messaging::StateWait {
            pane,
            want,
            deadline,
            reply: crate::server::messaging::Reply::new(client_id, tx, req),
        };
        if st.park_state_wait(wait).is_err() {
            drop(st);
            log::info!("agent: client_id={client_id} wait on %{pane} refused: busy");
            return Some(ServerMessage::refused(
                "busy",
                format!(
                    "{} callers already wait on agents' states; try again later",
                    crate::server::messaging::MAX_STATE_WAITS
                ),
            ));
        }
    }
    log::info!("agent: client_id={client_id} waits for %{pane} to be {want:?} (now {now_state:?})");
    mark_submit_dirty();
    None
}

fn agent_waited(
    pane: PaneId,
    name: Option<String>,
    state: AgentState,
    timed_out: bool,
) -> ServerMessage {
    ServerMessage::AgentWaited {
        target: Resolved {
            host: None,
            pane_id: pane,
            name,
        },
        state: Some(state),
        timed_out,
        req: None,
    }
}

/// One pass over the `agent wait` callers: sample each pane once, and answer
/// the waits it ends. A pane that is no longer live is `no_such_target`, and
/// one no longer running an agent is `agent_not_ready`: the state waited for
/// can no longer come.
async fn step_state_waits(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    store: &'static Mutex<Messaging>,
) {
    let watched = store.lock().await.state_wait_panes();
    for pane in watched {
        let sample = sample_pane(pane, state, panes, config).await;
        let now = std::time::Instant::now();
        let mut st = store.lock().await;
        match (sample.live, sample.state) {
            (true, Some(agent_state)) => {
                let name = st.name_of(pane).map(str::to_string);
                for (wait, timed_out) in st.answer_state_waits(pane, agent_state, now) {
                    log::info!(
                        "agent: client_id={} wait on %{pane} for {:?} {}",
                        wait.reply.client_id,
                        wait.want,
                        if timed_out { "timed out" } else { "answered" }
                    );
                    let message = agent_waited(pane, name.clone(), agent_state, timed_out);
                    wait.reply.send(message);
                }
            }
            (live, _) => {
                let (code, why) = if live {
                    ("agent_not_ready", format!("the agent in %{pane} exited"))
                } else {
                    ("no_such_target", format!("pane %{pane} closed"))
                };
                for wait in st.take_state_waits(pane) {
                    log::info!(
                        "agent: client_id={} wait on %{pane} ended: {code}",
                        wait.reply.client_id
                    );
                    let message = ServerMessage::refused(code, format!("{why} while waited on"));
                    wait.reply.send(message);
                }
            }
        }
    }
}

/// Whether an `agent status` or `agent wait` that came over the peer link
/// `link` must be refused before it is answered. A link asks about one pane
/// at a time, under the link rate. Every request counts against that rate,
/// a refused one too, so that refusals cannot write log lines faster than
/// answers can.
async fn peer_agent_refusal(
    link: &str,
    untargeted: bool,
    config: &Config,
    store: &Mutex<Messaging>,
) -> Option<ServerMessage> {
    let per_10s = config.messaging.link_rate_per_10s();
    let charged = store
        .lock()
        .await
        .charge_inbound_link(link, per_10s, std::time::Instant::now());
    if let Err(refusal) = charged {
        log::info!("agent: link {link} refused: {}", refusal.code());
        return Some(ServerMessage::refused(refusal.code(), refusal.message()));
    }
    if untargeted {
        log::info!("agent: link {link} asked for every pane: bad_request");
        return Some(ServerMessage::refused(
            "bad_request",
            "a peer link asks about one pane at a time",
        ));
    }
    None
}

async fn handle_agent_status(
    target: Option<Address>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    store: &Mutex<Messaging>,
) -> ServerMessage {
    let live = live_panes(panes).await;
    let wanted = match &target {
        None => None,
        Some(address) => match resolve_target(address, &live, store).await {
            Ok(pane) => Some(pane),
            Err(refusal) => return refusal,
        },
    };

    // It dups the masters itself and holds no lock while it reads them.
    let agents: HashMap<PaneId, AgentEntry> = collect_agents(state, panes, clients, config)
        .await
        .into_iter()
        .map(|entry| (entry.pane_id, entry))
        .collect();
    let now = now_ms();
    let rows: Vec<(PaneId, Option<String>, usize)> = {
        let store = store.lock().await;
        let mut ids: Vec<PaneId> = match wanted {
            Some(pane) => vec![pane],
            None => agents
                .keys()
                .copied()
                .chain(store.panes_with_mail_or_name(now))
                .filter(|pane| live.contains(pane))
                .collect(),
        };
        ids.sort_unstable();
        ids.dedup();
        ids.into_iter()
            .map(|pane| {
                (
                    pane,
                    store.name_of(pane).map(str::to_string),
                    store.unread(pane, now),
                )
            })
            .collect()
    };
    let mut entries = Vec::with_capacity(rows.len());
    {
        let st = state.lock().await;
        for (pane_id, name, unread) in rows {
            let Some((session, tab_index)) = st.locate_pane(pane_id) else {
                if wanted.is_some() {
                    let why = if st.is_popup(pane_id) {
                        POPUP_REFUSED.to_string()
                    } else {
                        format!("no such target: %{pane_id}")
                    };
                    return ServerMessage::refused("no_such_target", why);
                }
                log::debug!("msg: status skips pane %{pane_id}, which is in no tab");
                continue;
            };
            let agent = agents.get(&pane_id);
            entries.push(PaneStatusEntry {
                pane_id,
                session,
                tab_index,
                command: agent.map(|entry| entry.command.clone()),
                state: agent.map(|entry| entry.state),
                name,
                unread,
                host: None,
            });
        }
    }
    ServerMessage::AgentStatusList {
        panes: entries,
        req: None,
    }
}

/// The longest a read may wait for mail, whatever it asks for. A waiter holds
/// only a channel sender, so the bound is about not keeping a forgotten
/// request alive for ever, not about cost.
pub const MAX_WAIT_MS: u64 = 3_600_000;

/// End a parked wait at its deadline, answering it timed out. Returns whether
/// it did: removing the waiter from the store by any other path (answered,
/// its pane closing, its client leaving, superseded) drops the waiter's end
/// of `cancelled`, and the timer then ends at once without answering.
async fn wait_out(
    id: crate::server::messaging::WaiterId,
    pane: PaneId,
    wait_ms: u64,
    cancelled: tokio::sync::oneshot::Receiver<()>,
    store: &'static Mutex<Messaging>,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {}
        _ = cancelled => return false,
    }
    let Some(waiter) = store.lock().await.unpark(id) else {
        return false;
    };
    log::debug!("msg: wait on %{pane} timed out after {wait_ms}ms");
    let _ = waiter.tx.send(ServerMessage::MsgList {
        pane,
        messages: Vec::new(),
        timed_out: true,
        marked: waiter.mark,
    });
    true
}

/// List a mailbox. Only the caller's own mailbox is ever marked read: a read
/// of anyone else's, or from outside any pane, is a peek whatever it asked.
///
/// With `wait_ms`, a read that finds nothing parks instead of answering and
/// returns `None`; see [`crate::server::messaging::Waiter`].
async fn handle_msg_read(
    client_id: u64,
    msg: MailboxRead,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    store: &'static Mutex<Messaging>,
) -> Option<ServerMessage> {
    let (pane, caller) = match mailbox_of(
        client_id,
        "MsgRead",
        msg.pane,
        msg.pane_hint,
        peer,
        state,
        panes,
    )
    .await
    {
        Ok(found) => found,
        Err(refused) => return Some(refused),
    };
    let mark = caller == Some(pane) && !msg.peek;
    // Cloned before the store is locked: a waiter answers through it later.
    let tx = match msg.wait_ms {
        Some(_) => clients
            .lock()
            .await
            .get(&client_id)
            .map(|conn| conn.tx.clone()),
        None => None,
    };
    // The reader that called this is this client's, so it is in the map.
    debug_assert!(
        msg.wait_ms.is_none() || tx.is_some(),
        "a waiting read from client_id={client_id}, which is not in the client map"
    );
    let has_tx = tx.is_some();
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    // The read and the park happen under one lock, and a send delivers and
    // collects waiters under the same lock, so no message can arrive between
    // "nothing to read" and "waiting".
    let (messages, swept, parked) = {
        let mut store = store.lock().await;
        let messages = store.read(pane, msg.all, mark, now_ms());
        let swept = store.take_swept();
        let parked = match (msg.wait_ms, tx) {
            (Some(wait_ms), Some(tx)) if messages.is_empty() => Some((
                store.park(crate::server::messaging::Waiter {
                    client_id,
                    pane,
                    all: msg.all,
                    mark,
                    tx,
                    cancel,
                }),
                wait_ms.min(MAX_WAIT_MS),
            )),
            _ => None,
        };
        (messages, swept, parked)
    };
    if mark && !messages.is_empty() {
        mark_agents_dirty();
    }
    if swept > 0 {
        mark_agents_dirty();
        log::info!("msg: swept {swept} expired message(s)");
    }
    match parked {
        Some((Err(_), _)) => {
            return Some(ServerMessage::refused(
                "no_such_target",
                format!("pane %{pane} closed while waiting"),
            ))
        }
        Some((Ok((id, superseded)), wait_ms)) => {
            if let Some(earlier) = superseded {
                log::debug!(
                    "msg: wait on %{} superseded for client_id={client_id}",
                    earlier.pane
                );
                let _ = earlier.tx.send(ServerMessage::MsgList {
                    pane: earlier.pane,
                    messages: Vec::new(),
                    timed_out: true,
                    marked: earlier.mark,
                });
            }
            log::debug!("msg: wait on %{pane} parked for client_id={client_id} ({wait_ms}ms)");
            tokio::spawn(wait_out(id, pane, wait_ms, cancelled, store));
            return None;
        }
        None => {}
    }
    if msg.wait_ms.is_some() && messages.is_empty() && !has_tx {
        return Some(ServerMessage::MsgList {
            pane,
            messages,
            timed_out: true,
            marked: mark,
        });
    }
    if !messages.is_empty() {
        log::info!("msg: read pane=%{pane} n={} marked={mark}", messages.len());
    }
    Some(ServerMessage::MsgList {
        pane,
        messages,
        timed_out: false,
        marked: mark,
    })
}

/// Mark messages Read without returning them. Naming another pane's mailbox
/// does mark it, unlike a read: an ack exists to clear mail on purpose.
async fn handle_msg_ack(
    client_id: u64,
    ack: MailboxAck,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    store: &Mutex<Messaging>,
) -> ServerMessage {
    if ack.all && !ack.ids.is_empty() {
        return ServerMessage::refused(
            "bad_request",
            "an ack names ids or asks for all of them, not both",
        );
    }
    let (pane, caller) = match mailbox_of(
        client_id,
        "MsgAck",
        ack.pane,
        ack.pane_hint,
        peer,
        state,
        panes,
    )
    .await
    {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    let count = {
        let mut store = store.lock().await;
        if ack.all {
            store.ack_all(pane)
        } else {
            store.ack(pane, &ack.ids)
        }
    };
    if count > 0 {
        mark_agents_dirty();
        log::info!(
            "msg: acked pane=%{pane} n={count} by={}",
            caller.map_or("outside".to_string(), |by| format!("%{by}"))
        );
    }
    ServerMessage::MsgAcked { pane, count }
}

/// Log which pane a new connection's peer runs in, once per connection.
///
/// Diagnostic only. A message's sender is attributed again for each message,
/// because a pane can close between two messages on one connection. The
/// process table is read with no lock held.
async fn log_peer_identity(
    client_id: u64,
    peer_pid: Option<i32>,
    peer: Arc<identity::Peer>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) {
    let pin = peer.pin_kind();
    let verdict = tokio::time::timeout(ATTRIBUTION_TIMEOUT, async {
        let index = proc_index(&*panes.lock().await);
        tokio::task::spawn_blocking(move || {
            identity::attribute_caller(&*peer, &index, &identity::system_table())
        })
        .await
    })
    .await;
    let Ok(verdict) = verdict else {
        log::warn!(
            "identity: client_id={client_id} peer={peer_pid:?} not attributed: timed out after {ATTRIBUTION_TIMEOUT:?}"
        );
        return;
    };
    match verdict {
        Ok(Ok(a)) if a.tty_differs => log::info!(
            "identity: client_id={client_id} peer={peer_pid:?} pin={pin} -> pane %{} by {:?}; its tty differs (nested pty?)",
            a.pane,
            a.how
        ),
        Ok(Ok(a)) => log::debug!(
            "identity: client_id={client_id} peer={peer_pid:?} pin={pin} -> pane %{} by {:?}",
            a.pane,
            a.how
        ),
        Ok(Err(e)) => log::debug!(
            "identity: client_id={client_id} peer={peer_pid:?} pin={pin} not a pane: {}",
            e.reason()
        ),
        Err(e) => log::warn!("identity: client_id={client_id} attribution task failed: {e}"),
    }
}

/// Return the runtime directory used for the socket and pid files.
fn runtime_dir() -> PathBuf {
    xdg_runtime_dir().unwrap_or_else(fallback_runtime_dir)
}

fn xdg_runtime_dir() -> Option<PathBuf> {
    dirs::runtime_dir().or_else(|| std::env::var("XDG_RUNTIME_DIR").ok().map(PathBuf::from))
}

/// `$TMPDIR/remux-<uid>` (TMPDIR defaulting to `/tmp`), for a session with no
/// XDG runtime directory. Per user and private, because the temp directory is
/// shared: a socket straight in `/tmp` could be connected to, or created
/// first, by any other user on the machine.
fn fallback_runtime_dir() -> PathBuf {
    std::env::temp_dir().join(format!("remux-{}", own_uid()))
}

/// Make the runtime directory ready to hold the socket, or to trust a socket
/// found in it, and return it.
///
/// An XDG runtime directory is the login session's, private to this user by
/// the spec, and is used as it is. The fallback is created mode 0700, and then
/// REFUSED unless it is a real directory (not a symlink) owned by this user
/// with no group or other permission: another user who made it first would
/// otherwise own the socket every client of ours connects to.
pub fn prepare_runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = xdg_runtime_dir() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating runtime directory {}", dir.display()))?;
        return Ok(dir);
    }
    let dir = fallback_runtime_dir();
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(e).with_context(|| format!("creating {}", dir.display()));
            }
        }
    }
    check_private_dir(&dir, own_uid())?;
    Ok(dir)
}

/// `dir`, created 0700 if it is missing, then refused unless it is a private
/// directory of ours, by the rules the socket's fallback directory follows.
pub fn private_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }
    check_private_dir(dir, own_uid())
}

/// Whether `dir` is a directory, not a symlink, owned by `uid`, and closed to
/// group and other.
fn check_private_dir(dir: &std::path::Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("inspecting {}", dir.display()))?;
    if !meta.file_type().is_dir() {
        anyhow::bail!("refusing to use {}: it is not a directory", dir.display());
    }
    if meta.uid() != uid {
        anyhow::bail!(
            "refusing to use {}: it is owned by uid {}, not by this user (uid {uid})",
            dir.display(),
            meta.uid()
        );
    }
    if meta.mode() & 0o077 != 0 {
        anyhow::bail!(
            "refusing to use {}: its mode {:o} lets other users in; it must be 0700",
            dir.display(),
            meta.mode() & 0o777
        );
    }
    Ok(())
}

/// This process's effective uid.
fn own_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// Whether a connection from `peer_uid` may talk to a server run by
/// `own_uid`. The socket's own mode keeps other users out already; this is the
/// check that does not depend on a file mode being right.
fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

/// Return the path to the Unix domain socket used for client-server
/// communication.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("remux.sock")
}

/// The file at `path` as (device, inode), to tell it from another file later
/// put at the same path.
fn file_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

/// Remove this server's socket and pid file at shutdown, but only while they
/// are still this server's. A shutdown that runs long (a large save) can end
/// after the next server has bound its own socket at the same path and
/// written its own pid: removing those would leave that server unreachable
/// and `remux stop` unable to find it.
fn remove_own_files(socket_path: &std::path::Path, bound: Option<(u64, u64)>) {
    if bound.is_some() && file_identity(socket_path) == bound {
        let _ = std::fs::remove_file(socket_path);
    } else {
        log::info!(
            "shutdown: {} is no longer the socket this server bound; left in place",
            socket_path.display()
        );
    }
    let pid_file = pid_path();
    let ours = std::process::id().to_string();
    match std::fs::read_to_string(&pid_file) {
        Ok(text) if text.trim() == ours => {
            let _ = std::fs::remove_file(&pid_file);
        }
        Ok(_) => log::info!(
            "shutdown: {} names another process; left in place",
            pid_file.display()
        ),
        Err(_) => {}
    }
}

/// Return the path to the pid file recording the running server's PID.
///
/// Written on startup and removed on graceful shutdown; used by `remux stop`
/// to signal the server (SIGTERM) for a clean save-and-exit.
pub fn pid_path() -> PathBuf {
    runtime_dir().join("remux.pid")
}

/// Write a length-prefixed JSON message to an async writer.
pub async fn write_message<W, T>(writer: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let frame = protocol::encode_message(msg)?;
    writer
        .write_all(&frame)
        .await
        .context("writing message frame")?;
    writer.flush().await.context("flushing writer")?;
    Ok(())
}

/// Read a length-prefixed JSON message from an async reader.
///
/// Returns `Ok(None)` if the connection was closed (EOF on the length header).
/// A frame over [`protocol::MAX_FRAME_BYTES`] is an `Err` before anything is
/// allocated, and every caller drops the connection on an `Err`. This is the
/// one reader: the server's, the TUI client's, a remote's over ssh and the
/// handshake's all go through it.
pub async fn read_message<T>(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    let Some(payload) = read_frame(reader).await? else {
        return Ok(None);
    };
    let msg: T = serde_json::from_slice(&payload).context("deserializing message")?;
    Ok(Some(msg))
}

/// One frame's payload, undecoded, with [`read_message`]'s bounds: `Ok(None)`
/// on EOF before a header. A reader that must outlive a frame it cannot
/// decode reads with this and decodes itself.
pub async fn read_frame(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("reading message header"),
    }

    let len = protocol::decode_message_length(&header);
    if len > protocol::MAX_FRAME_BYTES {
        log::warn!(
            "protocol: refusing a {len}-byte frame (the limit is {}); dropping the connection",
            protocol::MAX_FRAME_BYTES
        );
        anyhow::bail!("frame of {len} bytes is over the limit");
    }
    // Grown as the bytes arrive rather than reserved at the claimed length, so
    // a peer that sends a large header and nothing else costs almost nothing.
    let mut payload = Vec::with_capacity(len.min(64 * 1024));
    let got = AsyncReadExt::take(&mut *reader, len as u64)
        .read_to_end(&mut payload)
        .await
        .context("reading message payload")?;
    if got < len {
        anyhow::bail!("connection closed mid-frame ({got} of {len} bytes)");
    }
    Ok(Some(payload))
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Data associated with a single pane: its PTY and screen buffer.
struct PaneData {
    pty: Pty,
    screen: Screen,
    /// Receiving end for PTY output from the background reader task.
    pty_rx: mpsc::UnboundedReceiver<pty::PtyChunk>,
    /// The background reader task feeding `pty_rx`.
    ///
    /// Held so it can be aborted when the pane goes away. Dropping a
    /// `JoinHandle` detaches the task rather than stopping it, and a reader left
    /// running holds an epoll registration on a descriptor number that the next
    /// PTY will be handed -- see `pty::start_reader` for what that cost.
    reader: tokio::task::JoinHandle<()>,
    /// Sending end of the pane's input queue, drained by `writer`.
    ///
    /// The ONLY way the daemon writes to a pane. `Pty::write_input` is
    /// `#[cfg(test)]` so that stays true by compilation rather than by habit:
    /// the master is non-blocking, and a synchronous loop over it dropped the
    /// tail of any write the slave's input queue could not take -- a truncated
    /// or entirely vanished paste. See [`pty::start_writer`].
    input_tx: mpsc::UnboundedSender<pty::WriteJob>,
    /// The background writer task draining `input_tx`.
    ///
    /// Held for the same reason `reader` is: dropping a `JoinHandle` detaches
    /// the task rather than stopping it, and a writer left running holds an
    /// epoll registration on a descriptor number that the next PTY will be
    /// handed -- see `pty::start_reader` for what that cost.
    writer: tokio::task::JoinHandle<()>,
    /// True once a PTY-forwarding task has been spawned for this pane.
    /// start_pty_forwarding is called from many sites (attach, session/tab
    /// switches); without this guard each call would spawn a competing task
    /// and chunks could be processed out of order, corrupting the stream.
    forwarding_started: bool,
    /// The `session_visible` flag last streamed to this pane's View-cell
    /// subscribers. Tracked so a visibility flip (tab switch / attach / detach)
    /// that does NOT change the pane's size still pushes a fresh `PaneContent`
    /// (so cells flip live between the "Active in session" placeholder and the
    /// streamed content), while steady state stays quiet.
    streamed_session_visible: bool,
    /// When bytes last reached this pane's [`Screen`].
    ///
    /// Per PANE, not per tab. `Tab::activity` deliberately ignores the
    /// foreground tab (`record_pane_activity` returns early for it), which is
    /// exactly the tab an agents panel most needs to be right about, so it
    /// cannot be the source for this.
    last_output: std::time::Instant,
    /// The last agent verdict LOGGED for this pane: its state and its
    /// sample-independent reason.
    ///
    /// Kept only so the classification is logged when it CHANGES rather than
    /// ten times a second. `None` for a pane that has never been classified --
    /// which is most of them, since a pane running no agent is never sampled.
    ///
    /// **Not the explain STRING, which is what this used to be and is why the
    /// dedup did nothing.** `Verdict::why` embeds the elapsed milliseconds, so
    /// two consecutive samples of a pane that had not moved compared UNEQUAL and
    /// both logged -- per agent pane, at up to ten samples a second, into a log
    /// that is never rotated and (`main.rs` pins the logger at `Debug`) can
    /// never be turned down. A user reported 586 MB in about a day.
    /// [`crate::server::agents::Reason`] is the same reason with that number
    /// taken out.
    agent_verdict: Option<(AgentState, crate::server::agents::Reason)>,
    /// When a human last typed into this pane: a client's keystroke, a View
    /// cell's, a key binding's `SendKey`, or the wheel's arrow keys. Set only
    /// by [`Self::send_human_input`]. The typing guard reads it.
    last_human_input: Option<std::time::Instant>,
    /// The message being typed into the pane, while it is. See
    /// [`Self::send_human_input`].
    typing: Option<std::sync::Weak<SubmitTyping>>,
    /// The pane's `OSC 0` / `OSC 2` titles and its foreground history.
    title: crate::server::title::TitleTracker,
    /// Process names and working directory, read outside the `panes` lock by
    /// [`refresh_pane_names`] so that naming a pane never does a syscall.
    names: PaneNames,
    /// The pane's program has exited. Such a pane can stay in the table when
    /// `close_pane` declines to close it (a background tab), and it must not
    /// be a message target or a sender's pane in the meantime.
    exited: bool,
}

/// A pane's cached process names and working directory.
#[derive(Debug, Default)]
struct PaneNames {
    /// The shell's name.
    shell: String,
    /// The foreground job's name, with its process group.
    job: Option<(i32, String)>,
    /// The basename of the shell's working directory.
    cwd: Option<String>,
    read_at: Option<std::time::Instant>,
}

impl PaneNames {
    /// The name of process group `group`: the job's when it is the cached job,
    /// else the shell's.
    fn command(&self, group: i32, shell: i32) -> &str {
        match &self.job {
            Some((pgid, name)) if *pgid == group && group != shell => name,
            _ => &self.shell,
        }
    }

    /// Job `pgid`'s name after a read that found `found` for it, which is
    /// `None` when the group had no live member.
    ///
    /// A job that has exited still names the pane until a sample shows the
    /// shell back in the foreground, so a read in that window keeps the name
    /// the job had. Falling back to the shell's name there would show it for
    /// that window, which is a flicker in the pane's border. A group never
    /// seen alive has no such name, and is named after the shell.
    fn job_after_read(&self, pgid: i32, found: Option<String>, shell: &str) -> (i32, String) {
        match (found, &self.job) {
            (Some(name), _) => (pgid, name),
            (None, Some((cached, name))) if *cached == pgid => (pgid, name.clone()),
            (None, _) => (pgid, shell.to_string()),
        }
    }
}

/// Whether process group `pgid` still has a member. A group whose members are
/// all stopped is alive, and `EPERM` means a member exists that we may not
/// signal.
fn group_alive(pgid: i32) -> bool {
    !matches!(
        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

impl PaneData {
    /// What to re-read into [`PaneData::names`], if anything.
    ///
    /// Everything is re-read when `foreground_changed`, and otherwise once the
    /// cache is older than [`crate::server::title::SETTLE`]: `cd`, and an
    /// `exec` in the shell or in a job, change a name or the directory without
    /// changing the foreground, and a sample taken between a job's fork and
    /// its exec caches the shell's name for the job.
    fn names_to_refresh(
        &self,
        now: std::time::Instant,
        foreground_changed: bool,
    ) -> Option<NameRefresh> {
        let stale = self
            .names
            .read_at
            .is_none_or(|at| now.saturating_duration_since(at) >= crate::server::title::SETTLE);
        (foreground_changed || stale).then(|| NameRefresh {
            child: self.pty.child_pid,
            job: self.title.job(),
        })
    }

    /// The shell's process group. `Pty::spawn` makes the shell a session
    /// leader with `setsid`, so its pid is its process group id.
    fn shell_pgid(&self) -> i32 {
        self.pty.child_pid.as_raw()
    }

    /// The pane's settled title and command name, as of `now`. Reads only
    /// cached state, so it is safe on every render under the `panes` lock.
    fn name_parts(&self, now: std::time::Instant) -> (Option<&str>, &str) {
        let shell = self.shell_pgid();
        let effective = self.title.effective(now, shell);
        (
            self.title.current(effective, shell),
            self.names.command(effective, shell),
        )
    }

    /// Feed one PTY read to the emulator and the title tracker.
    ///
    /// The chunk carries the foreground group the reader saw when it woke, so
    /// a title is attributed to the program that wrote it even if that program
    /// has exited since. The liveness check is a `killpg(0)` that runs only
    /// when the shell takes the foreground back from a job that set a title,
    /// once per such job.
    fn ingest_chunk(&mut self, chunk: &pty::PtyChunk) -> crate::server::title::Transition {
        let shell = self.shell_pgid();
        let transition =
            self.title
                .note_foreground(chunk.foreground, chunk.sampled_at, shell, group_alive);
        self.screen.process_output(&chunk.data);
        if let Some(raw) = self.screen.take_title() {
            let text = crate::server::title::normalise(&raw);
            let setter = match (chunk.foreground.unwrap_or(shell), chunk.foreground_after) {
                (before, Some(after)) if before == shell && after != shell => after,
                (before, _) => before,
            };
            self.title.observe(text, chunk.sampled_at, setter, shell);
        }
        transition
    }

    /// Adopt a title whose settle window is over. Returns whether the pane's
    /// name may have changed, either because an adopted title did or because a
    /// job has now held the foreground long enough to name the pane.
    fn settle_title(&mut self, pane_id: PaneId, now: std::time::Instant) -> bool {
        let job_named = self.title.foreground_due(now);
        if job_named {
            log::debug!("server: pane_id={pane_id} is now named by its foreground job");
        }
        let shell = self.shell_pgid();
        self.title.settle(now, shell) || job_named
    }

    /// Queue bytes for the pane's PTY.
    ///
    /// Never writes the descriptor here. The master is `O_NONBLOCK` and this is
    /// called under the `panes` lock, so the only two things this function could
    /// do itself are drop the bytes it cannot place (the paste-truncation bug)
    /// or block a runtime thread while holding a lock the whole daemon contends
    /// for. It hands them to [`pty::start_writer`]'s task instead.
    ///
    /// **Infallible, and that is the contract rather than an oversight.** The
    /// one thing that can go wrong is a closed channel -- a pane whose
    /// `PaneData` is being torn down -- and it is logged here. Handing the
    /// caller an `Err` for it would be worse than useless: `handle_input`
    /// propagates, so every keystroke aimed at a pane that is already going away
    /// would become an `error handling client message` line, and the two callers
    /// that log an error themselves would carry a branch nothing can reach.
    fn send_input(&self, data: &[u8]) {
        if self.input_tx.send(data.to_vec().into()).is_err() {
            log::warn!(
                "server: dropping {} input byte(s) for a pane whose writer has exited",
                data.len()
            );
        }
    }

    /// [`Self::send_input`] for keys a person pressed, which arms the typing
    /// guard. Every human key path writes through this, and nothing else
    /// does: a submit's own writes arming it would hold the next submit
    /// behind the guard the first one armed.
    ///
    /// A key that arrives while a message is being typed, before it is
    /// submitted or refused, takes the pane back from it: the message's
    /// writes are cancelled, and the key goes to the writer with a flush of
    /// the queue ahead of it, which the writer runs only once the cancelled
    /// write has stopped. Behind the message, the key would be read with it
    /// by the next reader, and after the agent exits that is the shell, which
    /// the key's Enter would make run it. A paste the agent is inside (it
    /// read the opener, and not yet the closer) is closed first, so the key
    /// is never taken as part of it. Only the first key does this.
    fn send_human_input(&mut self, data: &[u8]) {
        self.last_human_input = Some(std::time::Instant::now());
        let typing = self
            .typing
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .filter(|t| !t.ended.load(Ordering::SeqCst))
            .filter(|t| !t.taken_over.swap(true, Ordering::SeqCst));
        let Some(typing) = typing else {
            return self.send_input(data);
        };
        self.typing = None;
        // An empty queue after the writer finished the last job means the
        // agent has read all it was given so far, whatever the typing task
        // has yet to notice: the opener at least, and the closer too once the
        // last chunk went out.
        let unread = unread_input(&typing.slave);
        if unread == Some(0) && typing.cancel.is_written() {
            typing.opener_read.store(typing.bracketed, Ordering::SeqCst);
            if typing.last_sent.load(Ordering::SeqCst) {
                typing.all_read.store(true, Ordering::SeqCst);
            }
        }
        typing.cancel.cancel();
        let mut bytes = Vec::with_capacity(data.len() + 6);
        if typing.paste_open() {
            bytes.extend_from_slice(crate::server::submit::PASTE_CLOSE);
        }
        bytes.extend_from_slice(data);
        log::info!(
            "msg: id={} a person typed before it was submitted; its text is withdrawn, {} \
             unread byte(s) flushed",
            typing.id,
            unread.unwrap_or(0)
        );
        let job = pty::WriteJob {
            data: bytes,
            done: None,
            cancel: None,
            flush_first: Some(Arc::clone(&typing.slave)),
        };
        if self.input_tx.send(job).is_err() {
            log::warn!(
                "server: dropping {} input byte(s) for a pane whose writer has exited",
                data.len()
            );
        }
    }

    /// [`Self::send_input`], told when the last byte is written and able to
    /// be cancelled; see [`pty::WriteJob`]. The receiver reports `false` (or
    /// is closed) when the bytes were not all written.
    fn send_tracked(
        &self,
        data: Vec<u8>,
        cancel: Arc<pty::WriteCancel>,
    ) -> tokio::sync::oneshot::Receiver<bool> {
        let (done, written) = tokio::sync::oneshot::channel();
        cancel.pending();
        let job = pty::WriteJob {
            data,
            done: Some(done),
            cancel: Some(cancel),
            flush_first: None,
        };
        if self.input_tx.send(job).is_err() {
            log::warn!("server: dropping a typed message for a pane whose writer has exited");
        }
        written
    }
}

impl Drop for PaneData {
    fn drop(&mut self) {
        // Stop the reader with the pane. Its descriptor is its own, so the abort
        // is about not leaking a task (and the dup with it) for a pane whose
        // child left a grandchild holding the slave open -- the read would never
        // reach EOF and the task would park forever.
        self.reader.abort();
        // Same for the writer, and it needs the abort MORE than the reader does.
        // Closing `input_tx` (which happens when this struct's fields drop,
        // after this function returns) only ends the task once it is back at
        // `recv()` -- and a task parked in `writable()` on a slave queue nothing
        // is draining never gets there. A child that has exited would fail out
        // with `EIO`, but a stopped process, or a grandchild still holding the
        // slave open, would not, and the task would hold its dup and its
        // reactor registration for the life of the server.
        self.writer.abort();
    }
}

// MouseSelection is imported from compositor.

/// An in-progress mouse drag-selection gesture for a client.
///
/// The gesture's anchor (where the drag started) is stored in *absolute*,
/// eviction-stable coordinates ([`Screen::abs_of_row`]) rather than viewport
/// rows, so that when the view auto-scrolls into scrollback during the drag the
/// anchor stays pinned to the same logical buffer line instead of drifting. The
/// viewport-relative `MouseSelection` is re-derived from this each event.
struct DragSession {
    /// Pane the gesture belongs to; a drag into a different pane starts fresh.
    pane_id: PaneId,
    /// Anchor column in the pane's content area.
    anchor_col: u16,
    /// Anchor row as a stable absolute line id.
    anchor_abs: usize,
    /// The moving end of the selection, in the same absolute, eviction-stable
    /// coordinates as the anchor. Persisted (unlike the old per-call local) so a
    /// wheel-scroll during the drag can extend the selection to the newly
    /// revealed edge without a fresh mouse event -- keeping the highlight and the
    /// yankable range derived from one absolute range.
    end_abs: usize,
    /// The moving end's column in the pane's content area.
    end_col: u16,
}

/// A connected client with metadata about which session it is attached to.
struct ClientConnection {
    session_name: Option<String>,
    /// Sender to push `ServerMessage`s to this client's writer task.
    tx: mpsc::UnboundedSender<ServerMessage>,
    cols: u16,
    rows: u16,
    /// The client's current input mode (e.g. "NORMAL", "COMMAND", "VISUAL").
    mode: String,
    /// Active mouse selection, if any.
    mouse_selection: Option<MouseSelection>,
    /// Search match info: (current_match, total_matches).
    search_info: Option<(usize, usize)>,
    /// Per-(this client, pane) scroll offset into the panes of the ATTACHED
    /// SESSION (0/absent = live view, >0 = scrolled back).
    ///
    /// One offset per client would be wrong in two ways a user sees: the wheel
    /// over a pane that does not own input would move whichever pane does, and
    /// moving focus to a second pane and back would lose the first pane's
    /// position. The offset belongs to the pane, so it is stored per pane.
    ///
    /// Deliberately NOT the same map as [`ClientConnection::pane_scroll`],
    /// which is View-cell state cleared on `UnsubscribePane`: sharing them
    /// would wipe a pane's session scrollback when a View that aliases it is
    /// torn down.
    ///
    /// Entries are clamped at RENDER time (a resize can shrink the scrollback
    /// an entry addresses) and removed when the pane is reaped.
    session_scroll: std::collections::HashMap<PaneId, usize>,
    /// The offset [`ClientConnection::prev_scroll_pane`] last rendered at, for
    /// detecting an incremental scroll worth a `ScrollRender`.
    prev_scroll_offset: usize,
    /// The pane `prev_scroll_offset` was measured on. A `ScrollRender` shifts
    /// the client's existing pixels by a delta, so a delta taken across two
    /// DIFFERENT panes would scroll one pane's rows by the other's movement.
    /// A change of pane forces a full repaint instead.
    prev_scroll_pane: Option<PaneId>,
    /// Set when the client's scroll offset changed; forces the next broadcast
    /// to send a FullRender so the diff baseline can't desync from the client's
    /// screen across a scroll transition.
    needs_full_render: bool,
    /// In-progress mouse drag-selection gesture, if any. Tracks the drag anchor
    /// in eviction-stable absolute coordinates so edge auto-scroll doesn't drift.
    drag: Option<DragSession>,
    /// Armed when a drag is resting on a scrollable content edge: stores the last
    /// drag coordinates `(start_x, start_y, end_x, end_y, popup_local)` so the per-client
    /// reader loop can replay the edge-scroll step on a repeating timer while the
    /// pointer is held still (terminals stop emitting drag events without motion).
    /// `None` disarms the timer -- set at construction, on click, on release, and
    /// whenever the scroll reaches its bound.
    autoscroll_repeat: Option<(u16, u16, u16, u16, bool)>,
    /// Panes this client has subscribed to via `SubscribePane`, mapped to the
    /// subscribing cell's size demand: `Some((cols, rows))` for a cell that
    /// SHOWS the pane and so demands it reflow to fit, or `None` for a
    /// watch-only subscription that imposes no size constraint (a cell hidden by
    /// the view's layout, a session-visible pane, or a plain observer).
    /// Folded into the pane's min-across-viewers effective size by
    /// [`recompute_pane_size`]. Each subscribed pane receives a `PaneContent`
    /// snapshot on subscribe and on every change, regardless of which
    /// session/tab this client has in the foreground.
    subscribed_panes: std::collections::HashMap<PaneId, Option<(u16, u16)>>,
    /// Per-(this client, pane) scroll offset into a subscribed pane's scrollback,
    /// driven by `ScrollPane` (a View cell's mouse wheel). Independent of the
    /// foreground `session_scroll` and of other clients viewing the same pane.
    /// `0`/absent = live view. Cleared on `UnsubscribePane` and pane close.
    pane_scroll: std::collections::HashMap<PaneId, usize>,
    /// Per-(this client, pane) drag-selection over a subscribed pane, in the
    /// pane's own content coordinates. The View-cell analog of
    /// `mouse_selection`: a client displaying a view is detached, so the
    /// session-scoped selection has no pane to attach to. Rendered into the
    /// per-subscriber `PaneContent` by [`stream_pane_content`].
    pane_selection: std::collections::HashMap<PaneId, MouseSelection>,
    /// In-progress pane-scoped drag gesture (a View cell), if any. Separate from
    /// `drag` so a session drag and a cell drag can never be confused for one
    /// another; the anchor is likewise kept in eviction-stable absolute
    /// coordinates so cell edge auto-scroll doesn't drift it.
    pane_drag: Option<DragSession>,
    /// The pane-scoped analog of `autoscroll_repeat`: `(pane_id, start_x,
    /// start_y, end_x, end_y)` in the pane's content coordinates, replayed by
    /// the ticker task while a cell drag rests on a scrollable content edge.
    pane_autoscroll_repeat: Option<(PaneId, u16, u16, u16, u16)>,
    /// Set by `SubscribeSessionTree`: this client receives an unsolicited
    /// [`ServerMessage::SessionTree`] whenever the structure changes, instead
    /// of polling with `ListSessionTree`.
    ///
    /// Per-connection and independent of attachment, exactly like
    /// [`ClientConnection::subscribed_panes`]. Living here rather than in a
    /// side table is what makes disconnect cleanup automatic: dropping the
    /// `ClientConnection` in [`handle_client_disconnect`] drops the
    /// subscription with it, so there is no second registry to leak.
    session_tree_subscribed: bool,
    /// Set by [`ClientMessage::SubscribeAgents`]: this client receives an
    /// unsolicited [`ServerMessage::AgentList`] whenever agent state changes.
    ///
    /// A second flag rather than a mode of `session_tree_subscribed`, because
    /// the two payloads are dirtied by different things -- structure versus
    /// pane output -- and a client wanting one must not pay for the other.
    agents_subscribed: bool,
    /// How this client wants panes named, from [`ClientMessage::PaneTitle`].
    /// `None` until it says, and for ever for an older client or the CLI.
    naming: Option<Naming>,
    /// How this client wants tab chips capped, from
    /// [`ClientMessage::TabStyle`]. `None` until it says, and for ever for an
    /// older client or the CLI.
    tab_style: Option<TabStyle>,
    /// A second descriptor for the connection's socket, which
    /// [`close_connections`] shuts down: the reader then sees EOF and the
    /// connection ends the usual way. Only a peer link that has lapsed is
    /// closed by this server.
    closer: Option<std::os::fd::OwnedFd>,
    /// The popup interior this client asked for with
    /// [`ClientMessage::PopupSize`]. `None` for a client that never asked (an
    /// older client, or the CLI): the server then draws the popup into its
    /// frames itself, as a framed float, and sizes the popup PTY so that
    /// float's interior fits.
    popup_want: Option<(u16, u16)>,
    /// The pane that got this client's last forwarded right press, until the
    /// release is sent. The release goes to that pane alone, wherever the
    /// pointer is by then: a release the application never saw a press for,
    /// or a press with no release, would leave its button state wrong.
    right_press: Option<PaneId>,
}

impl ClientConnection {
    /// How to name panes on what is rendered for this client: its own
    /// template, or this server's for a client that never sent one.
    fn naming(&self) -> Naming {
        self.naming.clone().unwrap_or_else(Naming::server)
    }

    /// The tab style this client is drawn with: its own, or this server's for
    /// a client that never sent one.
    fn tab_style(&self, config: &Config) -> TabStyle {
        self.tab_style.unwrap_or(config.appearance.theme.tab_style)
    }

    /// The theme to composite this client's frames with.
    fn theme(&self, config: &Config) -> CompositorTheme {
        CompositorTheme {
            tab_style: self.tab_style(config),
            ..config.compositor_theme()
        }
    }

    /// This client's scroll offset into `pane_id`, unclamped.
    fn scroll_of(&self, pane_id: PaneId) -> usize {
        self.session_scroll.get(&pane_id).copied().unwrap_or(0)
    }

    /// Store `offset` for `pane_id`, dropping the entry at the live tail so
    /// "is anything scrolled?" stays a test of emptiness.
    fn set_scroll(&mut self, pane_id: PaneId, offset: usize) {
        if offset == 0 {
            self.session_scroll.remove(&pane_id);
        } else {
            self.session_scroll.insert(pane_id, offset);
        }
    }
}

/// The Remux server.
#[derive(Clone)]
pub struct RemuxServer {
    state: Arc<Mutex<ServerState>>,
    panes: Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: Arc<Config>,
    clients: Arc<Mutex<HashMap<u64, ClientConnection>>>,
    /// Monotonically increasing counter for stable client IDs.
    next_client_id: Arc<AtomicU64>,
    /// Previous composite frame per client (keyed by `client_id`), for diff
    /// computation. See [`PrevFrameCache`].
    prev_frames: PrevFrameCache,
    /// Dormant (saved-but-not-live) sessions awaiting resurrection. See
    /// [`DormantStore`].
    dormant: DormantStore,
}

// ---------------------------------------------------------------------------
// Server implementation
// ---------------------------------------------------------------------------

impl RemuxServer {
    /// Create a new server instance.
    fn new(config: Config) -> Self {
        PANE_TITLE.get_or_init(|| config.appearance.pane_title.clone());
        MESSAGING.get_or_init(|| Mutex::new(Messaging::new(config.messaging.limits())));
        Self {
            state: Arc::new(Mutex::new(ServerState::new())),
            panes: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
            clients: Arc::new(Mutex::new(HashMap::new())),
            next_client_id: Arc::new(AtomicU64::new(0)),
            prev_frames: Arc::new(Mutex::new(HashMap::new())),
            dormant: Arc::new(Mutex::new(PersistedState {
                state: ServerState::new(),
                pane_cwds: HashMap::new(),
            })),
        }
    }

    /// Start the server: bind socket, accept connections, run the event loop.
    pub async fn run(config: Config) -> Result<()> {
        let server = Self::new(config);
        let seam =
            identity::seam_from_env(std::env::var("REMUX_TEST_PEER_ANCESTOR").ok().as_deref());
        if let Some(warning) = server.config.messaging.host_name_warning() {
            log::warn!("[messaging] {warning}");
        }
        if let Some(pid) = seam {
            log::warn!("identity: TEST SEAM: pid {pid} counts as a root sshd for peer links");
        }
        let _ = PEER_SEAM.set(seam);

        // Before anything is restored: a server that is about to refuse a
        // live socket must not first spawn every restored pane's shell.
        prepare_runtime_dir()?;
        let path = socket_path();
        // Only a socket nobody listens on is stale. Unlinking a live server's
        // socket would leave that server running, and every session in it,
        // where no client can reach it.
        match tokio::net::UnixStream::connect(&path).await {
            Ok(_) => anyhow::bail!(
                "a remux server is already listening on {}; `remux stop` it first",
                path.display()
            ),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("probing {}", path.display()));
            }
        }

        let listener = UnixListener::bind(&path).context("binding Unix listener")?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting {} to this user", path.display()))?;
        }
        log::info!("server listening on {}", path.display());
        // What shutdown may remove: this file, and no other that a later
        // server binds at the same path.
        let bound = file_identity(&path);

        // Before the restore, which can take a while: a `remux stop` that
        // finds the socket must also find the pid, and the SIGTERM it sends
        // must be held for the main loop rather than kill a server that has
        // no handler yet. tokio keeps a signal until the handler is polled, so
        // a stop during the restore becomes a graceful save once it is done.
        // The handlers come first: a pid file names a server that can take
        // the signal.
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("registering SIGTERM handler")?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("registering SIGINT handler")?;
        // Best-effort: a write failure here must not abort startup.
        let pid_file = pid_path();
        match std::fs::write(&pid_file, std::process::id().to_string()) {
            Ok(()) => log::info!("wrote pid file {}", pid_file.display()),
            Err(e) => log::warn!("failed to write pid file {}: {e}", pid_file.display()),
        }

        // Load persisted state before accepting connections. Behavior depends
        // on two config flags:
        //   save_sessions=false            -> no persistence at all (skip load).
        //   save_sessions, automatic_restore -> bring persisted sessions live.
        //   save_sessions, !automatic_restore -> load persisted sessions as
        //                                        dormant/resurrectable instead.
        if server.config.general.save_sessions {
            match crate::server::persistence::load_state() {
                Ok(Some(persisted)) => {
                    if server.config.general.automatic_restore {
                        log::info!("restoring persisted state");
                        if let Err(e) = restore_state(&server, persisted).await {
                            log::warn!("failed to restore state: {e}, starting fresh");
                        }
                    } else {
                        let count = persisted.state.sessions.len();
                        log::info!(
                            "automatic_restore disabled: loaded {count} session(s) as dormant"
                        );
                        // Reserve live id space above the whole dormant id range
                        // so sessions created before a resurrect never collide
                        // with dormant pane/tab ids in the global pane map.
                        {
                            let mut st = server.state.lock().await;
                            st.reserve_ids_above(&persisted.state);
                        }
                        let mut d = server.dormant.lock().await;
                        *d = persisted;
                    }
                }
                Ok(None) => {
                    log::info!("no persisted state found, starting fresh");
                }
                Err(e) => {
                    log::warn!("failed to load persisted state: {e}, starting fresh");
                }
            }
        } else {
            log::info!("save_sessions disabled: persistence is off");
        }

        // After the restore above has SPAWNED the restored panes: the load
        // prunes against the live pane map, and loading any earlier would
        // prune every mailbox on every restart.
        if server.config.messaging.persist {
            load_mailbox(&server).await;
            spawn_mailbox_saver(Arc::clone(&server.state));
        }

        let own_uid = own_uid();

        // Session-tree push task. Sleeping a full interval after each broadcast
        // is the coalescing: every change arriving during that sleep collapses
        // into the single permit `Notify` holds, so a burst of structural
        // commands costs subscribers one extra push rather than one per
        // command. The first change after a quiet period goes out immediately
        // (the task is parked on `notified()`, which consumes the permit at
        // once), so the common case is not delayed. Idle costs nothing.
        {
            let state = Arc::clone(&server.state);
            let panes = Arc::clone(&server.panes);
            let clients = Arc::clone(&server.clients);
            let dormant = Arc::clone(&server.dormant);
            tokio::spawn(async move {
                loop {
                    SESSION_TREE_DIRTY.notified().await;
                    broadcast_session_tree(&state, &panes, &clients, &dormant).await;
                    tokio::time::sleep(SESSION_TREE_PUSH_INTERVAL).await;
                }
            });
        }

        // Agent push task: the same coalescing shape as the session tree above,
        // on its own signal (see `AGENTS_DIRTY`), plus one addition. Output
        // drives every transition except `Working` -> `Idle`, which is caused by
        // the ABSENCE of output and so can wake nobody. The tick covers exactly
        // that, and is armed only while something is actually `Working`, so an
        // idle server -- or one with no agents panel attached -- parks on the
        // `Notify` and costs nothing.
        {
            let state = Arc::clone(&server.state);
            let panes = Arc::clone(&server.panes);
            let clients = Arc::clone(&server.clients);
            let config = Arc::clone(&server.config);
            tokio::spawn(async move {
                let mut ticking = false;
                loop {
                    if ticking {
                        tokio::select! {
                            _ = AGENTS_DIRTY.notified() => {}
                            _ = tokio::time::sleep(AGENTS_DECAY_TICK) => {}
                        }
                    } else {
                        AGENTS_DIRTY.notified().await;
                    }
                    ticking = broadcast_agents(&state, &panes, &clients, &config).await;
                    tokio::time::sleep(AGENTS_PUSH_INTERVAL).await;
                }
            });
        }

        // Links from other servers that stopped pinging are forgotten, so
        // their names are free and no reply is routed to them.
        let sweep_clients = Arc::clone(&server.clients);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(LINK_EXPIRY_SWEEP).await;
                let expired = links().lock().await.expire(std::time::Instant::now());
                let closing = log_expired(expired);
                close_connections(&sweep_clients, &closing).await;
            }
        });

        // Messages that come back over the links this server dials.
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(INBOUND_QUEUE);
        let _ = INBOUND.set(inbound_tx);
        tokio::spawn(run_inbound(
            inbound_rx,
            Arc::clone(&server.state),
            Arc::clone(&server.panes),
            Arc::clone(&server.config),
        ));

        // The submitter: types queued messages into their panes. Its own
        // `Notify`, never `AGENTS_DIRTY`: that one is `notify_one` with the
        // pusher above as its only waiter, and a second waiter would take its
        // wakeups.
        tokio::spawn(run_submitter(
            Arc::clone(&server.state),
            Arc::clone(&server.panes),
            Arc::clone(&server.clients),
            Arc::clone(&server.config),
        ));

        // Periodic tick that promotes quiet background `Activity` tabs to
        // `Silent` ("finished"). Runs in the real server binary, so
        // `Instant::now()`/`interval` are unrestricted.
        const SILENCE_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(3);
        let mut activity_tick = tokio::time::interval(std::time::Duration::from_secs(1));

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => match stream.peer_cred() {
                            Ok(cred) if peer_allowed(cred.uid(), own_uid) => {
                                // Spawned so that a peer that never sends its Hello
                                // holds only its own task for the handshake timeout,
                                // not the accept loop, the tick or SIGTERM.
                                let pidfd = identity::peer_pidfd(std::os::fd::AsFd::as_fd(&stream));
                                let server = server.clone();
                                tokio::spawn(async move {
                                    server.handle_new_connection(stream, cred.pid(), pidfd).await;
                                });
                            }
                            Ok(cred) => {
                                log::warn!(
                                    "server: refusing a connection from uid {} (this server runs as uid {own_uid})",
                                    cred.uid()
                                );
                            }
                            Err(e) => {
                                log::warn!("server: refusing a connection whose peer is unknown: {e}");
                            }
                        },
                        Err(e) => {
                            log::error!("accept error: {e}");
                        }
                    }
                }
                _ = activity_tick.tick() => {
                    // Scan for background tabs that have gone silent past the
                    // threshold. Only broadcast to sessions that actually
                    // changed, so idle clients aren't woken every second.
                    let affected = {
                        let mut st = server.state.lock().await;
                        st.promote_silent_tabs(std::time::Instant::now(), SILENCE_THRESHOLD)
                    };
                    for session_name in affected {
                        broadcast_full_render(
                            &session_name,
                            &server.state,
                            &server.panes,
                            &server.clients,
                            &server.config,
                            &server.prev_frames,
                        )
                        .await;
                    }
                }
                _ = sigterm.recv() => {
                    log::info!("received SIGTERM, shutting down");
                    break;
                }
                _ = sigint.recv() => {
                    log::info!("received SIGINT, shutting down");
                    break;
                }
            }
        }

        // Graceful shutdown: clean up resources.
        server.shutdown(&path, bound).await;
        Ok(())
    }

    /// Perform graceful shutdown: save state, drop panes, remove socket.
    async fn shutdown(&self, socket_path: &std::path::Path, bound: Option<(u64, u64)>) {
        // Before the save_sessions return below: `persist` is the one switch
        // for this file. With save_sessions off no pane is restored, so the
        // next start prunes every mailbox, which is the right answer without
        // a second switch.
        if self.config.messaging.persist {
            save_mailbox_now(&self.state).await;
        }

        // Persistence is fully off when save_sessions is disabled.
        if !self.config.general.save_sessions {
            log::info!("save_sessions disabled: skipping state save on shutdown");
            remove_own_files(socket_path, bound);
            log::info!("shutdown complete");
            return;
        }

        log::info!("saving state before shutdown...");

        // Save persistent state.
        let state = self.state.lock().await;
        let panes = self.panes.lock().await;

        let mut pane_cwds = std::collections::HashMap::new();
        for (&pane_id, pane_data) in panes.iter() {
            if let Some(cwd) = crate::server::persistence::get_pane_cwd(pane_data.pty.child_pid) {
                pane_cwds.insert(pane_id, cwd);
            }
        }

        if let Ok(mut persisted) =
            crate::server::persistence::PersistedState::from_server(&state, &pane_cwds)
        {
            forget_titles(&mut persisted.state, &panes);
            // Persist live + still-dormant sessions so a live-only save never
            // clobbers un-resurrected dormant sessions on disk.
            {
                let dormant = self.dormant.lock().await;
                merge_dormant_into(&mut persisted, &dormant);
            }
            if let Err(e) = crate::server::persistence::save_state(&persisted) {
                log::error!("failed to save state on shutdown: {e}");
            } else {
                log::info!("state saved successfully");
            }
        }

        // Drop locks before cleanup.
        drop(state);
        drop(panes);

        remove_own_files(socket_path, bound);
        log::info!("shutdown complete");
    }

    /// Handle a newly accepted client connection.
    async fn handle_new_connection(
        &self,
        stream: tokio::net::UnixStream,
        peer_pid: Option<i32>,
        pidfd: Option<std::os::fd::OwnedFd>,
    ) {
        // Before the handshake, while the connector is as fresh as it gets. On
        // a kernel without a pidfd this reads the connector's start time.
        let peer = Arc::new(identity::Peer::capture(
            peer_pid,
            pidfd,
            std::os::fd::AsFd::as_fd(&stream),
        ));
        let closer = std::os::fd::AsFd::as_fd(&stream).try_clone_to_owned().ok();
        let (mut read_half, mut write_half) = stream.into_split();

        // Version handshake: exchange Hello/Welcome as the first frames, directly
        // on the split halves before wiring up the ServerMessage channel or
        // spawning the reader/writer tasks. Bounded by a timeout so a silent peer
        // cannot hold this open indefinitely. The server is lenient about version
        // skew (it logs and proceeds); the client decides whether to abort.
        let handshake = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let hello: Hello = read_message(&mut read_half)
                .await?
                .context("client closed connection during handshake")?;
            log::info!(
                "server: handshake from remux {} protocol v{}",
                hello.remux_version,
                hello.protocol_version
            );
            if hello.protocol_version != PROTOCOL_VERSION {
                log::warn!(
                    "server: protocol version mismatch (client v{}, server v{}); proceeding leniently",
                    hello.protocol_version,
                    PROTOCOL_VERSION
                );
            }
            let welcome = Welcome {
                protocol_version: PROTOCOL_VERSION,
                remux_version: crate::protocol::build_version(),
            };
            write_message(&mut write_half, &welcome).await?;
            anyhow::Ok(())
        })
        .await;
        match handshake {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::warn!("server: handshake failed: {e}; dropping connection");
                return;
            }
            Err(_) => {
                log::warn!("server: handshake timed out; dropping connection");
                return;
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

        let client_id = {
            let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
            let mut clients = self.clients.lock().await;
            clients.insert(
                id,
                ClientConnection {
                    closer,
                    session_name: None,
                    tx,
                    cols: 80,
                    rows: 24,
                    mode: "NORMAL".to_string(),
                    mouse_selection: None,
                    search_info: None,
                    session_scroll: std::collections::HashMap::new(),
                    prev_scroll_offset: 0,
                    prev_scroll_pane: None,
                    needs_full_render: false,
                    drag: None,
                    autoscroll_repeat: None,
                    subscribed_panes: std::collections::HashMap::new(),
                    pane_scroll: std::collections::HashMap::new(),
                    pane_selection: std::collections::HashMap::new(),
                    pane_drag: None,
                    pane_autoscroll_repeat: None,
                    session_tree_subscribed: false,
                    agents_subscribed: false,
                    naming: None,
                    tab_style: None,
                    popup_want: None,
                    right_press: None,
                },
            );
            log::debug!("server: new client connection, assigned client_id={id}");
            id
        };

        // Before the reader task exists, so a one-shot CLI peer cannot have
        // had its reply and exited while its process is being read.
        log_peer_identity(client_id, peer_pid, Arc::clone(&peer), &self.panes).await;

        // Spawn writer task.
        let mut writer = write_half;
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Err(e) = write_message(&mut writer, &msg).await {
                    log::debug!("client writer error: {e}");
                    break;
                }
            }
        });

        // Initial view sync: a freshly connected client immediately learns the
        // current shared views, so a brand-new terminal is in sync with the
        // shared registry before it does anything. Sent unconditionally (an
        // empty registry sends an empty `ViewList`, which is still correct
        // "nothing here" information a client can rely on).
        {
            let st = self.state.lock().await;
            let msg = build_view_list_message(&st);
            drop(st);
            let cls = self.clients.lock().await;
            if let Some(conn) = cls.get(&client_id) {
                let _ = conn.tx.send(msg);
            }
        }

        // Spawn reader task.
        let state = Arc::clone(&self.state);
        let panes = Arc::clone(&self.panes);
        let clients = Arc::clone(&self.clients);
        let config = Arc::clone(&self.config);
        let prev_frames = Arc::clone(&self.prev_frames);
        let dormant = Arc::clone(&self.dormant);

        // Clones for the drag-autoscroll ticker task (see below).
        let ts_state = Arc::clone(&self.state);
        let ts_panes = Arc::clone(&self.panes);
        let ts_clients = Arc::clone(&self.clients);
        let ts_config = Arc::clone(&self.config);
        let ts_prev_frames = Arc::clone(&self.prev_frames);

        tokio::spawn(async move {
            let mut reader = read_half;
            // Owned by the reader for the life of the connection, which keeps
            // the connector pinned for every frame this loop reads.
            // A message handler that attributes its caller does so before this
            // loop reads the next frame, because `handle_client_message` is
            // awaited here rather than spawned. The macOS pin depends on that
            // order: its token names whoever last used the peer's end.
            let peer = peer;
            loop {
                match read_message::<ClientMessage>(&mut reader).await {
                    Ok(Some(msg)) => {
                        if let Err(e) = handle_client_message(
                            client_id,
                            msg,
                            &peer,
                            &state,
                            &panes,
                            &clients,
                            &config,
                            &prev_frames,
                            &dormant,
                        )
                        .await
                        {
                            log::error!("error handling client message: {e}");
                            let cls = clients.lock().await;
                            if let Some(client) = cls.get(&client_id) {
                                let _ = client.tx.send(ServerMessage::Error {
                                    req: None,
                                    message: format!("{e}"),
                                    code: None,
                                });
                            }
                        }
                    }
                    Ok(None) => {
                        log::info!("client {client_id} disconnected");
                        handle_client_disconnect(
                            client_id,
                            &state,
                            &panes,
                            &clients,
                            &config,
                            &prev_frames,
                        )
                        .await;
                        // A hard disconnect of the client that made a pane
                        // session-visible must flip any View cell on that pane
                        // from the "Active in session" placeholder to live
                        // content; the two `handle_detach` sites don't cover this.
                        refresh_subscribed_panes(&state, &panes, &clients, &config).await;
                        break;
                    }
                    Err(e) => {
                        log::error!("error reading from client {client_id}: {e}");
                        handle_client_disconnect(
                            client_id,
                            &state,
                            &panes,
                            &clients,
                            &config,
                            &prev_frames,
                        )
                        .await;
                        // A hard disconnect of the client that made a pane
                        // session-visible must flip any View cell on that pane
                        // from the "Active in session" placeholder to live
                        // content; the two `handle_detach` sites don't cover this.
                        refresh_subscribed_panes(&state, &panes, &clients, &config).await;
                        break;
                    }
                }
            }
        });

        // Spawn the drag-autoscroll ticker task. When a drag rests still on a
        // scrollable content edge, terminals stop sending drag events, so nothing
        // in the reader loop would advance the scroll. This timer replays the
        // edge-scroll step at a constant rate while `autoscroll_repeat` is armed
        // (set/cleared by `handle_mouse_drag` / `handle_mouse_click`).
        //
        // This lives in its own task rather than a `tokio::select!` arm of the
        // reader loop on purpose: `read_message` reads a frame's header and
        // payload into locals across two awaits and is NOT cancel-safe, so if a
        // timer tick cancelled a partially-read frame the consumed bytes would be
        // lost and the protocol stream would desync. A separate task keeps the
        // reader loop's read future alive to completion. The task self-terminates
        // when the client is removed from the map on disconnect.
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(40));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // Read (and drop the lock before calling into handle_mouse_drag,
                // which locks `clients` itself). Break when the client is gone.
                let (armed, pane_armed) = {
                    let cls = ts_clients.lock().await;
                    match cls.get(&client_id) {
                        Some(c) => (c.autoscroll_repeat, c.pane_autoscroll_repeat),
                        None => break,
                    }
                };
                if let Some((sx, sy, ex, ey, popup_local)) = armed {
                    if let Err(e) = handle_mouse_drag(
                        client_id,
                        sx,
                        sy,
                        ex,
                        ey,
                        false,
                        popup_local,
                        &ts_state,
                        &ts_panes,
                        &ts_clients,
                        &ts_config,
                        &ts_prev_frames,
                    )
                    .await
                    {
                        log::error!("autoscroll drag error: {e}");
                    }
                }
                // The same replay for a View cell's gesture: a drag resting on a
                // cell's content edge scrolls that cell's source pane. The two
                // are mutually exclusive in practice (a client is either
                // attached to a session or displaying a view), but each is armed
                // and disarmed by its own handler, so both are simply checked.
                if let Some((pane_id, sx, sy, ex, ey)) = pane_armed {
                    if let Err(e) = handle_pane_mouse_drag(
                        client_id,
                        pane_id,
                        sx,
                        sy,
                        ex,
                        ey,
                        false,
                        &ts_state,
                        &ts_panes,
                        &ts_clients,
                        &ts_config,
                    )
                    .await
                    {
                        log::error!("pane autoscroll drag error: {e}");
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

/// Say which directory a panel asked for -- ONCE per distinct path, per client.
///
/// The `files` panel re-lists itself **every 2 seconds while it is placed, for
/// ever** (see `sidebar/files.rs`), so through the catch-all this was a
/// timer-driven line writing roughly 43k entries a day per placed panel into a
/// log that is never rotated and that `main.rs` pins at `Debug` with no
/// `RUST_LOG` to turn down. That is the same unbounded shape as the agents
/// classification line and the compositor's per-frame lines, and it is fixed
/// the same way: on CHANGE, not on sample.
///
/// The PATH is kept rather than counted. It is the whole diagnostic -- "the
/// panel is empty" is answered by what it asked for -- and a path is not a
/// credential; see the summary match's own comment for where that line is
/// drawn. Nothing here is gated behind a level, because there is no level to
/// gate it behind.
///
/// Keyed per CLIENT, since two clients can browse different directories and a
/// single global "last path" would make them log each other's polls for ever.
/// The cache is CLEARED rather than evicted when it fills, exactly as
/// [`crate::server::agents`]'s detection cache is and for the same reason: it
/// exists to suppress repeats, not to remember, so the worst a clear costs is
/// one repeated line.
fn log_list_directory(client_id: u64, path: &str) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    /// Clients remembered before starting over.
    const CAP: usize = 64;
    static LAST: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();

    let mut last = LAST
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if last.len() >= CAP {
        last.clear();
    }
    if last.get(&client_id).map(String::as_str) == Some(path) {
        return;
    }
    last.insert(client_id, path.to_string());
    drop(last);
    log::debug!("server: client_id={client_id} msg=ListDirectory(path={path:?})");
}

#[allow(clippy::too_many_arguments)]
async fn handle_client_message(
    client_id: u64,
    msg: ClientMessage,
    peer: &Arc<identity::Peer>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    // Log a summary of every incoming client message.
    //
    // **Anything the user TYPED is summarised, never printed.** `server.log` is
    // plaintext, unrotated and never pruned -- one user's reached 586 MB in
    // about a day -- and `main.rs` pins the logger at `Debug` with no
    // `RUST_LOG`, so nothing a user can set turns any of this off. Whatever
    // lands here stays on disk indefinitely.
    //
    // The three arms below carry content; the catch-all `{other:?}` Debug-prints
    // whole structs, so a variant carrying typed bytes MUST have an arm here or
    // it leaks verbatim. `Input` always had one. `InputToPane` and `CliSpawn`
    // arrived later and did not, and both leaked in full: `InputToPane` prints
    // its `Vec<u8>` as `[99, 111, 114, ...]` (full fidelity, trivially decoded)
    // and it is how every keystroke reaches a VIEW cell, so a passphrase typed
    // into a view was on disk; `CliSpawn` printed `argv` as plain strings, and
    // `remux split -- psql postgresql://user:pass@host/db` is an ordinary thing
    // to run.
    //
    // The line drawn is CONTENT, not "is it noisy" and not "did the user name
    // it". NAMES stay -- session, folder, tab, view -- because they are labels
    // the user chose to see in a UI, they are already logged verbatim by their
    // own handlers, and redacting only the copy in this match would be theatre.
    // PATHS stay for the same reason plus one more: a path IS the diagnostic
    // for the panel messages that carry one. A credential is not a name and is
    // not a path.
    match &msg {
        ClientMessage::Input { data } => {
            log::debug!(
                "server: client_id={client_id} msg=Input({} bytes)",
                data.len()
            );
        }
        ClientMessage::InputToPane { pane_id, data } => {
            log::debug!(
                "server: client_id={client_id} msg=InputToPane(pane_id={pane_id}, {} bytes)",
                data.len()
            );
        }
        ClientMessage::CliSpawn {
            session,
            placement,
            argv,
            cwd,
            pane,
        } => {
            // The PROGRAM, then a count. Which binary was asked for is the
            // whole diagnostic value of this line ("it tried to run `psql`");
            // its arguments are where a connection string or an API key lives.
            log::debug!(
                "server: client_id={client_id} msg=CliSpawn(session={session:?}, \
                 pane={pane:?}, placement={placement:?}, program={:?}, {} more arg(s), cwd={})",
                argv.first(),
                argv.len().saturating_sub(1),
                if cwd.is_some() { "set" } else { "none" }
            );
        }
        ClientMessage::ListDirectory { path } => {
            log_list_directory(client_id, path);
        }
        // Every field is named, so a field added later cannot reach the
        // catch-all below: a message body is user content and never logged.
        ClientMessage::MsgSend {
            to,
            body,
            deliver,
            reply_to,
            pane_hint,
            defer,
            timeout_ms,
            wait,
            origin,
            req,
        } => {
            // A host and a pane id are labels, never content.
            let origin = origin.as_ref().map_or("None".to_string(), |o| {
                format!("{}:%{}", loggable(&o.host), o.pane_id)
            });
            log::debug!(
                "server: client_id={client_id} msg=MsgSend(to={}, deliver={deliver:?}, {} bytes, \
                 reply_to={}, pane_hint={pane_hint:?}, defer={defer}, timeout={timeout_ms:?}, \
                 wait={wait}, origin={origin}, req={req:?})",
                loggable(&to.to_string()),
                body.len(),
                reply_to
                    .as_ref()
                    .map_or("None".to_string(), |r| loggable(&format!("{r:?}")))
            );
        }
        ClientMessage::MsgAck {
            pane,
            ids,
            all,
            pane_hint,
        } => {
            log::debug!(
                "server: client_id={client_id} msg=MsgAck(pane={pane:?}, {} id(s), all={all}, \
                 pane_hint={pane_hint:?})",
                ids.len()
            );
        }
        ClientMessage::AgentName {
            name,
            pane,
            pane_hint,
        } => {
            log::debug!(
                "server: client_id={client_id} msg=AgentName(name={}, pane={pane:?}, pane_hint={pane_hint:?})",
                name.as_deref().map_or("None".to_string(), loggable)
            );
        }
        ClientMessage::AgentStatus { target, req } => {
            log::debug!(
                "server: client_id={client_id} msg=AgentStatus(target={}, req={req:?})",
                target
                    .as_ref()
                    .map_or("None".to_string(), |t| loggable(&t.to_string()))
            );
        }
        ClientMessage::AgentWait {
            target,
            state,
            timeout_ms,
            req,
        } => {
            log::debug!(
                "server: client_id={client_id} msg=AgentWait(target={}, state={state:?}, \
                 timeout={timeout_ms:?}, req={req:?})",
                loggable(&target.to_string())
            );
        }
        ClientMessage::MsgRead {
            pane,
            all,
            peek,
            wait_ms,
            pane_hint,
        } => {
            log::debug!(
                "server: client_id={client_id} msg=MsgRead(pane={pane:?}, all={all}, peek={peek}, \
                 wait_ms={wait_ms:?}, pane_hint={pane_hint:?})"
            );
        }
        ClientMessage::ScrollDelta { delta } => {
            log::debug!("server: client_id={client_id} msg=ScrollDelta(delta={delta})");
        }
        ClientMessage::MouseDrag {
            start_x,
            start_y,
            end_x,
            end_y,
            is_final,
            pane_id,
            popup: _,
        } => {
            log::debug!(
                "server: client_id={client_id} msg=MouseDrag(start=({start_x},{start_y}), end=({end_x},{end_y}), is_final={is_final}, pane_id={pane_id:?})"
            );
        }
        // A link pings every second or so: one line each would be a line per
        // ping, for as long as the link lives.
        ClientMessage::Ping { .. } => {}
        // The peer writes the code and the message: only a code spelled like
        // one, and the message's length, reach the log.
        ClientMessage::MsgInboundAck {
            req,
            id,
            pane_id,
            code,
            message,
        } => {
            let code = code.as_deref().map(|c| {
                if crate::server::links::code_like(c) {
                    c
                } else {
                    "?"
                }
            });
            log::debug!(
                "server: client_id={client_id} msg=MsgInboundAck(req={req}, id={}, pane={}, code={}, message={} bytes)",
                id.map_or("-".to_string(), |id| id.to_string()),
                pane_id.map_or("?".to_string(), |pane| format!("%{pane}")),
                code.unwrap_or("-"),
                message.as_deref().map_or(0, str::len)
            );
        }
        other => {
            log::debug!("server: client_id={client_id} msg={other:?}");
        }
    }

    match msg {
        ClientMessage::MsgSend {
            to,
            body,
            deliver,
            reply_to,
            pane_hint,
            defer,
            timeout_ms,
            wait,
            origin,
            req,
        } => {
            let outgoing = OutgoingMessage {
                to,
                body,
                deliver,
                reply_to,
                pane_hint,
                defer,
                timeout_ms,
                wait,
                origin,
                req,
            };
            let answer = handle_msg_send(
                client_id,
                outgoing,
                peer,
                state,
                panes,
                config,
                clients,
                messaging(),
            )
            .await;
            // `None` is a submit that was queued: its answer comes from the
            // submitter, once it is typed or cannot be.
            if let Some(answer) = answer {
                reply(client_id, clients, answer.with_req(req)).await;
            }
            Ok(())
        }
        ClientMessage::MsgRead {
            pane,
            all,
            peek,
            wait_ms,
            pane_hint,
        } => {
            let answer = handle_msg_read(
                client_id,
                MailboxRead {
                    pane,
                    all,
                    peek,
                    wait_ms,
                    pane_hint,
                },
                peer,
                state,
                panes,
                clients,
                messaging(),
            )
            .await;
            // `None` is a read that parked: its answer comes later, from the
            // send that brings mail, the pane closing, or its timeout.
            if let Some(answer) = answer {
                reply(client_id, clients, answer).await;
            }
            Ok(())
        }
        ClientMessage::MsgAck {
            pane,
            ids,
            all,
            pane_hint,
        } => {
            let answer = handle_msg_ack(
                client_id,
                MailboxAck {
                    pane,
                    ids,
                    all,
                    pane_hint,
                },
                peer,
                state,
                panes,
                messaging(),
            )
            .await;
            reply(client_id, clients, answer).await;
            Ok(())
        }
        ClientMessage::AgentName {
            name,
            pane,
            pane_hint,
        } => {
            let answer = handle_agent_name(
                client_id,
                NameRequest {
                    name,
                    pane,
                    pane_hint,
                },
                peer,
                state,
                panes,
                messaging(),
            )
            .await;
            reply(client_id, clients, answer).await;
            Ok(())
        }
        ClientMessage::AgentStatus { target, req } => {
            let link = links()
                .lock()
                .await
                .peer_name(client_id, std::time::Instant::now())
                .map(str::to_string);
            // Forwarded only from a local caller: a host on a request that
            // came over a link is refused by the local handler.
            let remote = target
                .as_ref()
                .and_then(|t| t.host.clone())
                .filter(|host| valid_host(host) && link.is_none());
            let answer = match (remote, link.as_deref()) {
                (Some(host), _) => {
                    let msg = ClientMessage::AgentStatus {
                        target: target.map(|t| Address {
                            host: None,
                            target: t.target,
                        }),
                        req: None,
                    };
                    forward_agent_request(
                        client_id,
                        host,
                        msg,
                        std::time::Duration::ZERO,
                        config,
                        clients,
                        messaging(),
                    )
                    .await
                }
                (None, Some(link)) => Some(
                    match peer_agent_refusal(link, target.is_none(), config, messaging()).await {
                        Some(refusal) => refusal,
                        None => {
                            handle_agent_status(target, state, panes, clients, config, messaging())
                                .await
                        }
                    },
                ),
                (None, None) => Some(
                    handle_agent_status(target, state, panes, clients, config, messaging()).await,
                ),
            };
            if let Some(answer) = answer {
                reply(client_id, clients, answer.with_req(req)).await;
            }
            Ok(())
        }
        ClientMessage::PeerLink { name, ping_ms } => {
            if let Some(answer) =
                handle_peer_link(client_id, &name, ping_ms, peer, panes, clients).await
            {
                reply(client_id, clients, answer).await;
            }
            Ok(())
        }
        ClientMessage::Ping { nonce } => {
            // A link from another server pings to stay declared. One that has
            // lapsed is closed, not answered: a pong would keep its far side
            // Up while every message on it is refused.
            let lapsed = {
                let now = std::time::Instant::now();
                let mut links = links().lock().await;
                links.pinged(client_id, now);
                links.lapsed(client_id, now)
            };
            if lapsed {
                log::info!(
                    "peer link: client_id={client_id} pinged after its link lapsed; closing"
                );
                close_connections(clients, &[client_id]).await;
            } else {
                reply(client_id, clients, ServerMessage::Pong { nonce }).await;
            }
            Ok(())
        }
        ClientMessage::MsgInboundAck {
            req,
            id,
            pane_id,
            code,
            message,
        } => {
            // An ack no send waits for is dropped: the summary line above
            // already logged it, and the peer decides how many it sends.
            if let Some(tx) = links().lock().await.take_ack(client_id, req) {
                let _ = tx.send(crate::server::links::InboundAck {
                    id,
                    pane_id,
                    code,
                    message,
                });
            }
            Ok(())
        }
        ClientMessage::AgentWait {
            target,
            state: want,
            timeout_ms,
            req,
        } => {
            let link = links()
                .lock()
                .await
                .peer_name(client_id, std::time::Instant::now())
                .map(str::to_string);
            let remote = target
                .host
                .clone()
                .filter(|host| valid_host(host) && link.is_none());
            let answer = match (remote, link.as_deref()) {
                (Some(host), _) => {
                    let msg = ClientMessage::AgentWait {
                        target: Address {
                            host: None,
                            target: target.target,
                        },
                        state: want,
                        timeout_ms,
                        req: None,
                    };
                    forward_agent_request(
                        client_id,
                        host,
                        msg,
                        wait_timeout(timeout_ms),
                        config,
                        clients,
                        messaging(),
                    )
                    .await
                }
                (None, link) => {
                    let refused = match link {
                        Some(link) => peer_agent_refusal(link, false, config, messaging()).await,
                        None => None,
                    };
                    match refused {
                        Some(refusal) => Some(refusal),
                        None => {
                            handle_agent_wait(
                                client_id,
                                target,
                                want,
                                timeout_ms,
                                req,
                                link,
                                state,
                                panes,
                                clients,
                                config,
                                messaging(),
                            )
                            .await
                        }
                    }
                }
            };
            // `None` is a wait that was parked: the submitter answers it, or
            // the forward's task does.
            if let Some(answer) = answer {
                reply(client_id, clients, answer.with_req(req)).await;
            }
            Ok(())
        }
        ClientMessage::Attach { session_name } => {
            let result = handle_attach(
                client_id,
                &session_name,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            // The tree's `client_count` and `is_current` are derived from the
            // client map, so attaching changes what every subscriber renders --
            // not only what this client renders.
            mark_focus_changed();
            result
        }
        ClientMessage::Detach => {
            handle_detach(client_id, state, panes, clients, config, prev_frames).await;
            mark_focus_changed();
            // Detaching may make this client's active-tab panes no longer
            // session-visible; re-evaluate subscribed panes so any View cell on
            // them flips from the "Active in session" placeholder to live content.
            refresh_subscribed_panes(state, panes, clients, config).await;
            Ok(())
        }
        ClientMessage::Input { data } => {
            handle_input(client_id, &data, state, panes, clients).await?;
            // Typing leaves scrollback, as it does in tmux and zellij. The
            // server owns `session_scroll`, so the server ends it -- see
            // `snap_client_to_live_tail` for why the client cannot be trusted to
            // ask.
            snap_client_to_live_tail(client_id, state, panes, clients, config, prev_frames).await;
            Ok(())
        }
        ClientMessage::Resize { cols, rows } => {
            handle_resize(
                client_id,
                cols,
                rows,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await
        }
        ClientMessage::Command(cmd) => {
            // Read before `cmd` moves. The repaint belongs AFTER dispatch, and
            // here rather than inside `handle_command`, whose arms return from
            // a dozen places.
            let repaint = cmd.repaints_scrolled_requester();
            let result = handle_command(
                client_id,
                cmd,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            if repaint {
                repaint_scrolled_requester(client_id, state, panes, clients, config, prev_frames)
                    .await;
            }
            result
        }
        ClientMessage::CreateSession { name, folder } => {
            let result = handle_create_session(
                client_id,
                &name,
                folder.as_deref(),
                state,
                panes,
                clients,
                config,
            )
            .await;
            save_if_enabled(state, panes, config, dormant).await;
            mark_session_tree_dirty();
            result
        }
        ClientMessage::ListSessions => handle_list_sessions(client_id, state, clients).await,
        ClientMessage::KillSession { name } => {
            let result = handle_kill_session(&name, state, panes, clients).await;
            save_if_enabled(state, panes, config, dormant).await;
            mark_session_tree_dirty();
            result
        }
        ClientMessage::ListSessionTree => {
            handle_list_session_tree(client_id, state, panes, clients, dormant).await
        }
        ClientMessage::SubscribeSessionTree => {
            {
                let mut cls = clients.lock().await;
                if let Some(conn) = cls.get_mut(&client_id) {
                    conn.session_tree_subscribed = true;
                }
            }
            // Answer at once, so a subscriber's panel is populated immediately
            // rather than staying blank until the next structural change.
            send_session_tree_to(&[client_id], state, panes, clients, dormant).await;
            Ok(())
        }
        ClientMessage::UnsubscribeSessionTree => {
            let mut cls = clients.lock().await;
            if let Some(conn) = cls.get_mut(&client_id) {
                conn.session_tree_subscribed = false;
            }
            Ok(())
        }
        ClientMessage::SubscribeAgents => {
            {
                let mut cls = clients.lock().await;
                if let Some(conn) = cls.get_mut(&client_id) {
                    conn.agents_subscribed = true;
                }
            }
            // Answer at once, so a subscriber's panel is populated immediately
            // rather than staying blank until the next agent output.
            let any_working = send_agents_to(&[client_id], state, panes, clients, config).await;
            // And WAKE THE PUSHER, whatever that answer was.
            //
            // The pusher's decay tick is armed only by what `broadcast_agents`
            // returns. If it last ran with nobody subscribed it parked
            // unarmed -- and `Working` -> `Idle` is caused by the ABSENCE of
            // output, so nothing else was ever going to wake it. A client
            // subscribing inside the working window would be told `Working` and
            // then sit on it for ever, self-healing only if some pane happened
            // to produce output. Narrow at the 500ms default and proportionally
            // wider for anyone who raises `working_ms` to stop flapping.
            //
            // Unconditional rather than `if any_working`: this reply went to ONE
            // client, and the pusher's own next pass is what re-reads the whole
            // subscriber set. `any_working` is logged rather than acted on here
            // because acting on it would be reasoning about the wrong set.
            log::debug!("server: SubscribeAgents client_id={client_id} any_working={any_working}");
            mark_agents_dirty();
            Ok(())
        }
        ClientMessage::UnsubscribeAgents => {
            let mut cls = clients.lock().await;
            if let Some(conn) = cls.get_mut(&client_id) {
                conn.agents_subscribed = false;
            }
            Ok(())
        }
        ClientMessage::PaneTitle { template, host } => {
            let naming = Naming::new(template, host);
            let (session, tree_subscribed) = {
                let mut cls = clients.lock().await;
                let Some(conn) = cls.get_mut(&client_id) else {
                    return Ok(());
                };
                if conn.naming.as_ref() == Some(&naming) {
                    return Ok(());
                }
                conn.naming = Some(naming);
                (conn.session_name.clone(), conn.session_tree_subscribed)
            };
            // Everything already sent to this client was named the old way.
            if let Some(session) = session {
                send_full_render_to_client(
                    client_id,
                    &session,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                    RenderCtx {
                        diff_against_baseline: false,
                        names_are_fresh: false,
                        force_full: true,
                    },
                )
                .await;
            }
            if tree_subscribed {
                send_session_tree_to(&[client_id], state, panes, clients, dormant).await;
            }
            Ok(())
        }
        ClientMessage::TabStyle { style } => {
            let session = {
                let mut cls = clients.lock().await;
                let Some(conn) = cls.get_mut(&client_id) else {
                    return Ok(());
                };
                if conn.tab_style == Some(style) {
                    return Ok(());
                }
                conn.tab_style = Some(style);
                conn.session_name.clone()
            };
            // Everything already sent to this client was capped the old way.
            if let Some(session) = session {
                send_full_render_to_client(
                    client_id,
                    &session,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                    RenderCtx {
                        diff_against_baseline: false,
                        names_are_fresh: false,
                        force_full: true,
                    },
                )
                .await;
            }
            Ok(())
        }
        ClientMessage::ResurrectSession { name } => {
            let result = handle_resurrect_session(
                &name,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            save_if_enabled(state, panes, config, dormant).await;
            mark_session_tree_dirty();
            result
        }
        ClientMessage::RequestScrollback => {
            handle_request_scrollback(client_id, state, panes, clients).await
        }
        ClientMessage::SearchInfo { current, total } => {
            handle_search_info(client_id, current, total, clients).await;
            Ok(())
        }
        ClientMessage::ModeChanged { mode } => {
            handle_mode_changed(client_id, &mode, state, panes, clients, config, prev_frames).await
        }
        // `pane_id: Some(..)` routes the gesture by pane identity in that
        // pane's own content coordinates (a View cell); `None` keeps the
        // original screen-coordinate, foreground-session path.
        ClientMessage::MouseClick {
            x,
            y,
            pane_id,
            release,
            popup,
        } => match pane_id {
            Some(pid) => {
                handle_pane_mouse_click(client_id, pid, x, y, release, state, panes, clients).await
            }
            None => {
                handle_mouse_click(
                    client_id,
                    x,
                    y,
                    release,
                    popup,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                )
                .await
            }
        },
        ClientMessage::RightClick {
            x,
            y,
            release,
            popup,
        } => {
            handle_right_click(
                client_id,
                x,
                y,
                release,
                popup,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await
        }
        ClientMessage::MouseDrag {
            start_x,
            start_y,
            end_x,
            end_y,
            is_final,
            pane_id,
            popup,
        } => match pane_id {
            Some(pid) => {
                handle_pane_mouse_drag(
                    client_id, pid, start_x, start_y, end_x, end_y, is_final, state, panes,
                    clients, config,
                )
                .await
            }
            None => {
                handle_mouse_drag(
                    client_id,
                    start_x,
                    start_y,
                    end_x,
                    end_y,
                    is_final,
                    popup,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                )
                .await
            }
        },
        ClientMessage::PopupSize { cols, rows } => {
            handle_popup_size(
                client_id,
                (cols, rows),
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await
        }
        ClientMessage::ScrollDelta { delta } => {
            // No coordinates, so this scrolls whichever pane owns input.
            handle_scroll_delta(
                client_id,
                delta,
                None,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await
        }
        ClientMessage::MouseScroll { x, y, up, popup } => {
            handle_mouse_scroll(
                client_id,
                x,
                y,
                up,
                popup,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await
        }
        ClientMessage::ScrollReset => {
            log::debug!("server: ScrollReset client_id={client_id}");
            // The client asks for this when it is about to send input, so it
            // means the pane that input goes to. Another pane this client
            // scrolled is reading history and keeps its offset.
            let target = client_input_target(client_id, state, clients).await;
            let session_name = target.as_ref().map(|(sn, _)| sn.clone());
            {
                let mut cls = clients.lock().await;
                if let Some(client) = cls.get_mut(&client_id) {
                    if let Some((_, pane_id)) = target {
                        client.set_scroll(pane_id, 0);
                    }
                    client.prev_scroll_offset = 0;
                    client.needs_full_render = true;
                }
            }
            if let Some(session_name) = session_name {
                send_full_render_to_client(
                    client_id,
                    &session_name,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                    RenderCtx::default(),
                )
                .await;
            }
            Ok(())
        }
        ClientMessage::RequestScrollbackInfo => {
            // Take `clients` and RELEASE it before touching `state`. Holding it
            // across the `state` lock would be a `clients -> state` order, and
            // every other path here -- `handle_list_sessions`, `handle_attach`,
            // `send_session_tree_to` -- takes `state -> clients`. Two tasks on
            // the two orders deadlock the whole daemon, and the session-tree
            // pusher now runs `state -> clients` on a background task whenever
            // anyone is subscribed, which a sidebar does permanently.
            //
            // The pane is resolved by `client_input_target`, which honours the
            // popup. The client asks for this to size its own scrollbar against
            // the offset it holds, and that offset belongs to the pane taking
            // input: while the popup is up, that is the popup.
            if let Some((_sn, fp)) = client_input_target(client_id, state, clients).await {
                let total_lines = {
                    let ps = panes.lock().await;
                    ps.get(&fp).map(|p| p.screen.total_lines()).unwrap_or(0)
                };
                let cls = clients.lock().await;
                if let Some(client) = cls.get(&client_id) {
                    let _ = client
                        .tx
                        .send(ServerMessage::ScrollbackInfo { total_lines });
                }
            }
            Ok(())
        }
        ClientMessage::SubscribePane {
            pane_id,
            cols,
            rows,
            size_demand,
        } => {
            // Subscribing to a pane that is already gone gets an explicit answer,
            // never silence: the snapshot builder returns `None` for a missing
            // pane and the send below is simply skipped, so recording the
            // subscription would leave the cell on `waiting…` forever with no way
            // to learn the truth. Report the death instead and record nothing.
            if !panes.lock().await.contains_key(&pane_id) {
                log::info!("server: SubscribePane pane_id={pane_id} is gone; reporting PaneExited");
                let cls = clients.lock().await;
                if let Some(conn) = cls.get(&client_id) {
                    let _ = conn.tx.send(ServerMessage::Event(SessionEvent::PaneExited {
                        pane_id,
                        exit_code: EXIT_CODE_UNKNOWN,
                    }));
                }
                return Ok(());
            }
            // Record the subscriber's size demand (folded by min-across-viewers
            // sizing) and send an immediate snapshot.
            {
                let mut cls = clients.lock().await;
                if let Some(conn) = cls.get_mut(&client_id) {
                    // The demand is only a claim, like a Resize. Only a size
                    // that is used is clamped, so a placeholder size on a
                    // subscription with no demand is not logged as bad.
                    let demand =
                        size_demand.then(|| clamp_client_size("SubscribePane", cols, rows));
                    // A FRESH subscribe starts at the live view with nothing
                    // selected. A RE-subscribe of a pane this client already
                    // watches does not: the client re-subscribes its cells
                    // whenever the view's focus or geometry changes (the size
                    // demand follows focus), and resetting there would throw away
                    // a scroll position or a drag anchor mid-gesture just because
                    // the user clicked a cell.
                    if conn.subscribed_panes.insert(pane_id, demand).is_none() {
                        conn.pane_scroll.remove(&pane_id);
                        conn.pane_selection.remove(&pane_id);
                    }
                }
            }
            // Snapshot at whatever offset/selection this client now holds.
            stream_pane_content(pane_id, state, panes, clients).await;
            // The new/updated demand may shrink (or release) the pane's effective
            // size; recompute and re-stream if it changed.
            recompute_pane_size(pane_id, state, panes, clients, config).await;
            Ok(())
        }
        ClientMessage::ListDirectory { path } => {
            // Blocking filesystem work off the executor. A `read_dir` over a
            // cold NFS mount or a directory with a hundred thousand entries can
            // take hundreds of milliseconds, and this task also drives every
            // pane's output: blocking it would stall the whole server, not just
            // this listing.
            //
            // THIS machine's home rides along, for the path it might shorten.
            // Resolving its symlinks reads the filesystem too, so it runs here
            // as well, and it is re-read per listing on purpose: a server whose
            // `$HOME` changed under it should not keep answering with the old
            // one for the life of the daemon.
            let requested = path.clone();
            let (listing, home) = tokio::task::spawn_blocking(move || {
                (
                    browse::list_directory(std::path::Path::new(&path)),
                    browse::home_dir(),
                )
            })
            .await
            .unwrap_or_else(|e| {
                (
                    browse::Listing {
                        entries: Vec::new(),
                        error: Some(format!("listing failed: {e}")),
                        truncated: false,
                    },
                    None,
                )
            });
            log::debug!(
                "server: ListDirectory client_id={client_id} path={requested:?} \
entries={} error={:?} truncated={}",
                listing.entries.len(),
                listing.error,
                listing.truncated
            );
            let cls = clients.lock().await;
            if let Some(conn) = cls.get(&client_id) {
                let _ = conn.tx.send(ServerMessage::DirectoryListing {
                    // The REQUESTED path, verbatim. The panel matches its
                    // pending request against it; answering about a path we
                    // resolved differently would leave that match unfindable.
                    path: requested,
                    entries: listing.entries,
                    error: listing.error,
                    truncated: listing.truncated,
                    home,
                });
            }
            Ok(())
        }
        ClientMessage::OpenInSplit {
            path,
            command,
            vertical,
        } => {
            handle_open_in_split(
                client_id,
                &path,
                command.as_deref(),
                vertical,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await
        }
        ClientMessage::CliSpawn {
            session,
            placement,
            argv,
            cwd,
            pane,
        } => {
            handle_cli_spawn(
                client_id,
                &session,
                pane,
                placement,
                &argv,
                cwd.as_deref(),
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await
        }
        ClientMessage::UnsubscribePane { pane_id } => {
            {
                let mut cls = clients.lock().await;
                if let Some(conn) = cls.get_mut(&client_id) {
                    conn.subscribed_panes.remove(&pane_id);
                    conn.pane_scroll.remove(&pane_id);
                    conn.pane_selection.remove(&pane_id);
                    if conn.pane_drag.as_ref().map(|d| d.pane_id) == Some(pane_id) {
                        conn.pane_drag = None;
                        conn.pane_autoscroll_repeat = None;
                    }
                }
            }
            // Dropping a viewer may let the pane grow back; recompute.
            recompute_pane_size(pane_id, state, panes, clients, config).await;
            Ok(())
        }
        ClientMessage::InputToPane { pane_id, data } => {
            // Route input to a pane by identity (View cell), independent of this
            // client's foreground session/tab. No focus lookup -- the target is
            // explicit. We deliberately do NOT replicate handle_input's Ctrl+L
            // scrollback-clearing special case: that is foreground-clear-screen
            // UX and is not wanted for targeted cell input. The pane's own PTY
            // output will trigger the existing forwarding task, which fans out
            // PaneContent to subscribers, so no explicit broadcast is needed.
            let alive = {
                let mut ps = panes.lock().await;
                match ps.get_mut(&pane_id) {
                    Some(pane_data) => {
                        pane_data.send_human_input(&data);
                        true
                    }
                    None => false,
                }
            };
            if !alive {
                // The pane is gone. Answering keeps the keystroke from vanishing
                // without a trace: the sender learns its cell is dead and stops
                // typing into the void.
                log::warn!("server: InputToPane pane_id={pane_id} dropped: pane is gone");
                let cls = clients.lock().await;
                if let Some(conn) = cls.get(&client_id) {
                    let _ = conn.tx.send(ServerMessage::Event(SessionEvent::PaneExited {
                        pane_id,
                        exit_code: EXIT_CODE_UNKNOWN,
                    }));
                }
            }
            Ok(())
        }
        ClientMessage::ScrollPane {
            pane_id,
            up,
            lines,
            x,
            y,
        } => {
            // A View cell's wheel takes the SAME routing decision the session
            // wheel takes (`handle_mouse_scroll`) -- the whole reason the wheel
            // did nothing over a cell running a mouse-aware application is that
            // this path used to skip it and scroll a scrollback the alternate
            // screen does not have.
            let copy_mode = client_in_copy_mode(clients, client_id).await;
            let (max_off, route) = {
                let ps = panes.lock().await;
                match ps.get(&pane_id) {
                    Some(pd) => (
                        pd.screen.max_scroll_offset(),
                        mouse_route(&pd.screen, MouseGesture::Wheel, copy_mode),
                    ),
                    // Pane gone: nothing to scroll.
                    None => return Ok(()),
                }
            };
            // Only a subscribed client may drive a pane it watches (the same
            // guard the scroll path below applies).
            {
                let cls = clients.lock().await;
                match cls.get(&client_id) {
                    Some(conn) if conn.subscribed_panes.contains_key(&pane_id) => {}
                    _ => return Ok(()),
                }
            }
            match route {
                MouseRoute::App { sgr, .. } => {
                    // Cell coordinates arrive content-relative; a report is
                    // 1-based.
                    let bytes = wheel_report(sgr, up, x + 1, y + 1);
                    log::debug!(
                        "server: ScrollPane->app client_id={client_id} pane_id={pane_id} sgr={sgr} up={up} col={} row={}",
                        x + 1,
                        y + 1
                    );
                    return write_to_pane(panes, pane_id, &bytes).await;
                }
                MouseRoute::AltArrows { app_cursor } => {
                    let bytes = alt_scroll_arrows(app_cursor, up, lines.max(1));
                    log::debug!(
                        "server: ScrollPane->alt-arrows client_id={client_id} pane_id={pane_id} app_cursor={app_cursor} up={up}"
                    );
                    return write_keys_to_pane(panes, pane_id, &bytes).await;
                }
                MouseRoute::Remux { .. } => {}
            }
            // Per-(client, pane) scrollback for a View cell. Clamp to the pane's
            // max scroll offset, render a snapshot at the new offset, and send it
            // to THIS client only -- the offset is per-subscriber.
            let new_off = {
                let mut cls = clients.lock().await;
                match cls.get_mut(&client_id) {
                    // Only a subscribed client may scroll a pane it watches.
                    Some(conn) if conn.subscribed_panes.contains_key(&pane_id) => {
                        let cur = conn.pane_scroll.get(&pane_id).copied().unwrap_or(0);
                        let next = if up {
                            (cur + lines as usize).min(max_off)
                        } else {
                            cur.saturating_sub(lines as usize)
                        };
                        if next == 0 {
                            conn.pane_scroll.remove(&pane_id);
                        } else {
                            conn.pane_scroll.insert(pane_id, next);
                        }
                        next
                    }
                    _ => return Ok(()),
                }
            };
            // A live selection has to follow the scroll: extend it while a drag
            // is in flight, drop it otherwise (see
            // `rescope_pane_selection_on_scroll`).
            rescope_pane_selection_on_scroll(client_id, pane_id, up, new_off, panes, clients).await;
            // Repaint through the shared per-subscriber path so the snapshot is
            // rendered at THIS client's new offset WITH its selection applied --
            // a bespoke render here would silently drop the highlight. Other
            // subscribers re-render at their own unchanged state, so they simply
            // receive the frame they already had.
            stream_pane_content(pane_id, state, panes, clients).await;
            Ok(())
        }

        // -- Shared View intents --------------------------------------------
        // Each mutates the server-owned registry (behind the `state` lock) then
        // broadcasts the full `ViewList` to every connected client via the one
        // helper. `ViewCreate` additionally acks the creator directly.
        ClientMessage::ViewCreate { name } => {
            let id = {
                let mut st = state.lock().await;
                st.view_create(name, &config.layouts)
            };
            {
                let cls = clients.lock().await;
                if let Some(conn) = cls.get(&client_id) {
                    let _ = conn.tx.send(ServerMessage::ViewCreated { id });
                }
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewDelete { id } => {
            {
                let mut st = state.lock().await;
                st.view_delete(id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewRename { id, name } => {
            {
                let mut st = state.lock().await;
                st.view_rename(id, name);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewAddCells { id, cells } => {
            {
                let mut st = state.lock().await;
                st.view_add_cells(id, cells);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewRemoveCell { id, cell_id } => {
            {
                let mut st = state.lock().await;
                st.view_remove_cell(id, cell_id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewSetFocus { id, cell_id } => {
            {
                let mut st = state.lock().await;
                st.view_set_focus(id, cell_id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewCycleLayout { id } => {
            {
                let mut st = state.lock().await;
                st.view_cycle_layout(id, &config.layouts);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewToggleZoom { id } => {
            {
                let mut st = state.lock().await;
                st.view_toggle_zoom(id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewSetMaster { id } => {
            {
                let mut st = state.lock().await;
                st.view_set_master(id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewResizeCell {
            id,
            cell_id,
            dir,
            amount,
        } => {
            {
                let mut st = state.lock().await;
                st.view_resize_cell(id, cell_id, dir, amount);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewMoveCell { id, cell_id, dir } => {
            let area = view_reference_area(clients).await;
            {
                let mut st = state.lock().await;
                st.view_move_cell(id, cell_id, dir, area);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_attach(
    client_id: u64,
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    {
        let st = state.lock().await;
        if !st.sessions.contains_key(session_name) {
            let cls = clients.lock().await;
            if let Some(client) = cls.get(&client_id) {
                let _ = client.tx.send(ServerMessage::Error {
                    req: None,
                    message: format!("session '{}' not found", session_name),
                    code: None,
                });
            }
            return Ok(());
        }
    }

    let (cols, rows, left) = {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            if client.session_name.as_deref() != Some(session_name) {
                // These offsets index panes of the session being left. Nothing
                // reads an entry for a pane outside the attached session, so
                // keeping them shows nothing wrong -- but a non-empty map
                // defeats the emptiness check in `snap_client_to_live_tail`,
                // which is what keeps a keystroke off the `state` lock. One
                // switch away from a scrolled pane would otherwise charge this
                // client that lock on every keystroke it ever sends again.
                client.session_scroll.clear();
                client.prev_scroll_offset = 0;
                client.prev_scroll_pane = None;
            }
            let left = client
                .session_name
                .replace(session_name.to_string())
                .filter(|old| old != session_name);
            (client.cols, client.rows, left)
        } else {
            return Ok(());
        }
    };
    if let Some(left) = left {
        refit_popups_after_leave(&left, state, panes, clients, config, prev_frames).await;
    }

    log::debug!("server: client_id={client_id} attach session={session_name:?} dims={cols}x{rows}");

    // The active tab is now being viewed; clear any stale activity marker.
    {
        let mut st = state.lock().await;
        st.clear_active_tab_activity(session_name);
    }

    // Tell the client which border style this session is composited in, BEFORE
    // its first frame. The client seeds its own style from config once and
    // never re-learns it, so without this an attach to a session whose style was
    // toggled leaves the sidebar frame and the pane borders disagreeing.
    {
        let style = {
            let st = state.lock().await;
            st.sessions
                .get(session_name)
                .map(|s| s.border_style.clone())
        };
        if let Some(style) = style {
            let cls = clients.lock().await;
            if let Some(client) = cls.get(&client_id) {
                let _ = client.tx.send(ServerMessage::SessionBorderStyle { style });
            }
        }
    }

    // Resize panes to match the attaching client's terminal dimensions.
    resize_session_panes(session_name, state, panes, clients, config).await?;

    // Invalidate every attached client's baseline: this client's attach may
    // change the session render size (min over clients), so all clients must
    // re-render full at the new size on their next frame.
    invalidate_session_baselines(session_name, clients, prev_frames).await;

    send_full_render_to_client(
        client_id,
        session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        RenderCtx::default(),
    )
    .await;

    start_pty_forwarding(
        session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        dormant,
    )
    .await;
    Ok(())
}

/// Tell every client that session `old` is now `new`. A client keeps the
/// names of the sessions it is attached to and was last on, and keys its popup
/// state by them, and frames only ever name the session as it is now.
fn announce_session_rename(cls: &HashMap<u64, ClientConnection>, old: &str, new: &str) {
    for client in cls.values() {
        let _ = client
            .tx
            .send(ServerMessage::Event(SessionEvent::SessionRenamed {
                old: old.to_string(),
                new: new.to_string(),
            }));
    }
}

async fn handle_detach(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    log::debug!("server: client_id={client_id} detach");
    let left = {
        let mut cls = clients.lock().await;
        cls.get_mut(&client_id)
            .and_then(|client| client.session_name.take())
    };
    if let Some(left) = left {
        refit_popups_after_leave(&left, state, panes, clients, config, prev_frames).await;
    }
}

async fn handle_input(
    client_id: u64,
    data: &[u8],
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let session_name = {
        let cls = clients.lock().await;
        cls.get(&client_id).and_then(|c| c.session_name.clone())
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    // THE input chokepoint: `Session::input_target` routes to the popup while it
    // is visible, without touching `tab.focused_pane`.
    let active_pane = {
        let st = state.lock().await;
        match st
            .sessions
            .get(&session_name)
            .and_then(Session::input_target)
        {
            Some(p) => p,
            None => return Ok(()),
        }
    };

    log::debug!(
        "server: client_id={client_id} input {} bytes -> pane_id={active_pane}",
        data.len()
    );

    let mut ps = panes.lock().await;
    if let Some(pane_data) = ps.get_mut(&active_pane) {
        pane_data.send_human_input(data);

        // Ctrl+L (0x0C / FF): shells' readline/zsh clear-screen typically emits
        // only \e[H\e[2J (no \e[3J), so the scrollback would otherwise survive.
        // Honor the user's expectation that Ctrl+L also drops the pane's
        // scrollback. Gate on NOT being in alt-screen: full-screen apps like
        // vim use Ctrl+L for their own redraw and keep no scrollback, so they
        // must be unaffected. The shell repaints itself, so no render/broadcast
        // is needed; handle_scroll_delta re-clamps any stale client offsets
        // against the now-smaller total line count.
        if data.contains(&0x0C) && !pane_data.screen.alt_screen_active {
            pane_data.screen.scrollback.clear();
        }
    }
    Ok(())
}

/// Return this client's viewport to the live tail, repainting only if it moved.
///
/// **Why the server does this and not the client.** `session_scroll` is
/// server-owned state, and the render messages' `viewport_top` is an absolute
/// line index -- which is exactly `0` at maximum scroll, the same value the live
/// tail reports. A client that inferred "am I scrolled?" from `viewport_top`
/// was therefore blind at precisely the maximum, so it never sent the
/// `ScrollReset` that every one of its unstick paths (typing, Escape back to
/// Normal, leaving Visual, cancelling Search) is gated on. The session then
/// looked completely dead: the application kept answering keystrokes and the
/// server kept rendering, but always at the pinned offset, so the output landed
/// below the viewport and the screen never changed. Ending the scroll here means
/// it ends whatever the client believes.
///
/// The render messages now carry a real `scroll_offset` beside `viewport_top`,
/// so a current client does see the maximum for what it is and sends the
/// `ScrollReset` itself -- arriving *before* the `Input` for the same keystroke,
/// which makes this a no-op on that path. It stays because the field is
/// `#[serde(default)]`: a client older than it still reads 0 and is still blind,
/// and this is what unsticks that client's session anyway.
///
/// A no-op for a client already at the tail, which is the overwhelmingly common
/// case -- so this costs one lock and no repaint per keystroke.
///
/// `handle_command` is the second caller, for the commands
/// [`RemuxCommand::returns_to_live_tail`] names: a command that reshapes the
/// layout makes a scroll pointless in exactly the same way typing does, and a
/// client whose focused pane is scrolled is otherwise skipped by
/// `broadcast_full_render` and sees no frame at all. A command that only moves
/// focus between panes or tabs is NOT in that set, because the offset belongs
/// to the pane being left; it takes the targeted repaint that
/// [`RemuxCommand::repaints_scrolled_requester`] names instead.
///
/// Scoped tightly, so it can only ever undo a scroll the triggering command or
/// keystroke made pointless:
///
/// * Only the requesting client. Another client scrolled back through the same
///   session keeps its offset -- `session_scroll` is per-client and this
///   touches exactly one client's map.
/// * Only the pane that owns input. The keystroke lands in one pane, so only
///   that pane's position is made pointless by it; a second pane this client
///   scrolled back is still being read and keeps its offset.
/// * Only the attached-session viewport. A View cell's scrollback is the
///   per-(client, pane) `pane_scroll`, which this never reads, so cell input
///   (`InputToPane`, a different arm entirely) cannot move a scrolled cell.
/// * Never in an explicit scrollback mode. Visual (remux's copy mode) and Search
///   are sessions in history that the user drives with keys, and yanking them to
///   the bottom mid-selection would destroy the selection. Belt and braces
///   today: `handle_visual_key`/`handle_search_key` return no `SendToPty`, so
///   neither mode can reach this arm -- but the invariant is what makes the rule
///   safe, so it is enforced rather than assumed.
async fn snap_client_to_live_tail(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            // `session_scroll` empty is the per-keystroke common case, and it
            // answers "is anything of this client's scrolled?" without
            // resolving which pane owns input. Resolving that takes the `state`
            // lock, so without this arm every keystroke would contend for it.
            Some(client)
                if client.mode != COPY_MODE
                    && client.mode != SEARCH_MODE
                    && !client.session_scroll.is_empty() => {}
            _ => return,
        }
    }
    let Some((session_name, pane_id)) = client_input_target(client_id, state, clients).await else {
        return;
    };
    {
        let mut cls = clients.lock().await;
        let Some(client) = cls.get_mut(&client_id) else {
            return;
        };
        let offset = client.scroll_of(pane_id);
        if offset == 0 {
            return;
        }
        log::debug!(
            "server: returning client_id={client_id} pane_id={pane_id} to the live tail from offset={offset}"
        );
        client.set_scroll(pane_id, 0);
        client.prev_scroll_offset = 0;
        // The client's diff baseline is a scrolled frame, so the repaint
        // below must be a full one.
        client.needs_full_render = true;
    }
    send_full_render_to_client(
        client_id,
        &session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        RenderCtx::default(),
    )
    .await;
}

/// Send this client its own frame when the pane it now focuses is scrolled
/// back.
///
/// `broadcast_full_render` skips such a client, because that path is also the
/// PTY-output path and repainting there would drag a reader of this pane to the
/// bottom. A focus or tab move is not output, though: it changes which pane is
/// highlighted and, on a tab move, the whole screen. Without a frame the border
/// stays on the pane focus just left and the session reads as frozen, which is
/// what the snap used to prevent at the cost of the user's place in the
/// scrollback.
///
/// A no-op when the focused pane is at the live tail, because the broadcast the
/// command already made covered that client.
async fn repaint_scrolled_requester(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    let Some((session_name, pane_id)) = client_input_target(client_id, state, clients).await else {
        return;
    };
    {
        let mut cls = clients.lock().await;
        let Some(client) = cls.get_mut(&client_id) else {
            return;
        };
        if client.scroll_of(pane_id) == 0 {
            return;
        }
    }
    // Diffed rather than sent whole. The baseline holds what this client has on
    // screen whatever offsets that frame was composited at, so it is a valid
    // thing to diff against, and a command that moved focus by one pane changes
    // a border and little else. The case this saves the most is a command that
    // found nothing to do -- `PaneFocusLeft` against the left edge still reaches
    // here, and its frame is then empty rather than a repaint of the screen.
    send_full_render_to_client(
        client_id,
        &session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        RenderCtx {
            diff_against_baseline: true,
            ..RenderCtx::default()
        },
    )
    .await;
}

/// The session this client is attached to and the pane that owns its keyboard
/// input: the popup while it is up, else the active tab's focused pane.
///
/// Takes `clients` and RELEASES it before `state`. Holding it across the
/// `state` lock would be a `clients -> state` order against the
/// `state -> panes -> clients` every other path here uses, and two tasks on the
/// two orders deadlock the daemon.
async fn client_input_target(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Option<(String, PaneId)> {
    let session_name = {
        let cls = clients.lock().await;
        cls.get(&client_id).and_then(|c| c.session_name.clone())?
    };
    let pane_id = {
        let st = state.lock().await;
        st.sessions
            .get(&session_name)
            .and_then(Session::input_target)?
    };
    Some((session_name, pane_id))
}

/// The largest terminal a client may report, in columns and rows.
///
/// A size is only a claim the client makes, and every frame the server builds
/// for a session allocates its cells at the smallest size reported. These caps
/// sit far past any real terminal, so they only bound what a broken or hostile
/// client can make the server allocate.
const MAX_TERMINAL_COLS: u16 = 2048;
const MAX_TERMINAL_ROWS: u16 = 1024;

/// `(cols, rows)` from a client's `what` message, clamped to `1..=` the
/// terminal maximum on each axis.
fn clamp_client_size(what: &str, cols: u16, rows: u16) -> (u16, u16) {
    let clamped = (
        cols.clamp(1, MAX_TERMINAL_COLS),
        rows.clamp(1, MAX_TERMINAL_ROWS),
    );
    if clamped != (cols, rows) {
        log::warn!(
            "server: {what} {cols}x{rows} is out of range, clamped to {}x{}",
            clamped.0,
            clamped.1
        );
    }
    clamped
}

#[allow(clippy::too_many_arguments)]
async fn handle_resize(
    client_id: u64,
    cols: u16,
    rows: u16,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let (cols, rows) = clamp_client_size("Resize", cols, rows);
    let session_name = {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            client.cols = cols;
            client.rows = rows;
            client.session_name.clone()
        } else {
            None
        }
    };

    log::debug!("server: client_id={client_id} resize cols={cols} rows={rows}");

    if let Some(session_name) = session_name {
        resize_session_panes(&session_name, state, panes, clients, config).await?;
        // Invalidate all attached clients' baselines: a resize changes the
        // session render size, so every client must re-render full at the new
        // size on the broadcast below.
        invalidate_session_baselines(&session_name, clients, prev_frames).await;
        broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
    }
    Ok(())
}

/// Bring the panes and every attached client in line after a command moved the
/// focus or the active tab, as `TabGoto` does: the layout sizes of the newly
/// shown tab, fresh baselines and a frame. Run before the command the focus move
/// was for, so the move is painted even when that command turns out to do
/// nothing.
async fn settle_focus_change(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    mark_focus_changed();
    resize_session_panes(session_name, state, panes, clients, config).await?;
    invalidate_session_baselines(session_name, clients, prev_frames).await;
    broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
    Ok(())
}

/// The index of tab `tab_id` in `session`, `None` when either is gone.
async fn tab_index_by_id(
    state: &Arc<Mutex<ServerState>>,
    session: &str,
    tab_id: u64,
) -> Option<usize> {
    let st = state.lock().await;
    st.sessions
        .get(session)?
        .tabs
        .iter()
        .position(|t| t.id == tab_id)
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    client_id: u64,
    cmd: RemuxCommand,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    let (session_name, cols, rows) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => (c.session_name.clone(), c.cols, c.rows),
            None => return Ok(()),
        }
    };
    // Most commands act on the requesting client's attached session and are
    // ignored when it has none. A few structural commands operate on explicit
    // targets instead (folder create/delete/move, close-tab-by-index), so they
    // are allowed through even without an attached session. This lets the
    // session manager edit folders/tabs on a remote the client has connected to
    // but not switched to (attached). Their arms never read `session_name`.
    let session_name = match session_name {
        Some(s) => s,
        None => {
            if matches!(
                cmd,
                RemuxCommand::FolderNew(_)
                    | RemuxCommand::FolderDelete(_)
                    | RemuxCommand::FolderMoveSession { .. }
                    | RemuxCommand::TabCloseByIndex { .. }
                    | RemuxCommand::SessionRenameByName { .. }
                    | RemuxCommand::FolderRename { .. }
                    | RemuxCommand::TabNewInSession { .. }
                    | RemuxCommand::TabRenameByIndex { .. }
                    | RemuxCommand::PaneNewInTab { .. }
                    | RemuxCommand::PaneCloseById { .. }
                    | RemuxCommand::PaneRenameById { .. }
                    | RemuxCommand::TabMoveByIndex { .. }
                    | RemuxCommand::TabCloseById { .. }
                    | RemuxCommand::TabRenameById { .. }
            ) {
                String::new()
            } else {
                return Ok(());
            }
        }
    };

    log::debug!("server: client_id={client_id} command={cmd:?} session={session_name:?}");

    // -- Commands aimed at a named pane or tab -------------------------------
    //
    // The context menu names what was clicked, and the menu can stay open while
    // the layout changes under it. Each of these resolves its target now, and
    // does nothing when the target is gone, so the inner command can never fall
    // on whatever happens to be focused instead.
    let cmd = match cmd {
        RemuxCommand::OnPane { pane_id, command } => {
            let landed = {
                let mut st = state.lock().await;
                let index = st.sessions.get(&session_name).and_then(|sess| {
                    sess.tabs
                        .iter()
                        .position(|t| t.pane_order.contains(&pane_id))
                });
                match index {
                    None => None,
                    Some(index) => {
                        let tab_changed = st.sessions[&session_name].active_tab != index;
                        let _ = st.goto_tab(&session_name, index);
                        let tab =
                            &mut st.sessions.get_mut(&session_name).expect("session").tabs[index];
                        // A visible popup owns input, and hiding it returns
                        // input to `focused_pane`, so moving that here would send
                        // the user elsewhere when the popup closes. As
                        // `SessionSwitchPane` does, the tab is entered and the
                        // focus stays; the layout commands are then refused by
                        // the popup guard, and a command for another pane is
                        // dropped.
                        let popup_up = tab.popup_visible;
                        let mut moved = tab_changed;
                        if !popup_up && tab.focused_pane != pane_id {
                            // A float zoom snaps back and the command then acts,
                            // as a click outside the box does.
                            tab.release_float_zoom();
                            tab.focus_pane(pane_id);
                            moved = true;
                        }
                        Some((moved, popup_up && tab.focused_pane != pane_id))
                    }
                }
            };
            let Some((moved, foreign_under_popup)) = landed else {
                log::debug!("server: OnPane pane_id={pane_id} is gone, {command:?} dropped");
                return Ok(());
            };
            if moved {
                settle_focus_change(&session_name, state, panes, clients, config, prev_frames)
                    .await?;
            }
            if foreign_under_popup {
                log::debug!(
                    "server: OnPane pane_id={pane_id} is under the popup, {command:?} dropped"
                );
                return Ok(());
            }
            return Box::pin(handle_command(
                client_id,
                *command,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            ))
            .await;
        }
        RemuxCommand::OnTab { tab_id, command } => {
            let landed = {
                let mut st = state.lock().await;
                let index = st
                    .sessions
                    .get(&session_name)
                    .and_then(|sess| sess.tabs.iter().position(|t| t.id == tab_id));
                index.map(|index| {
                    let tab_changed = st.sessions[&session_name].active_tab != index;
                    let _ = st.goto_tab(&session_name, index);
                    tab_changed
                })
            };
            let Some(moved) = landed else {
                log::debug!("server: OnTab tab_id={tab_id} is gone, {command:?} dropped");
                return Ok(());
            };
            if moved {
                settle_focus_change(&session_name, state, panes, clients, config, prev_frames)
                    .await?;
            }
            return Box::pin(handle_command(
                client_id,
                *command,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            ))
            .await;
        }
        RemuxCommand::TabCloseById { session, tab_id } => {
            let Some(tab_index) = tab_index_by_id(state, &session, tab_id).await else {
                return Ok(());
            };
            RemuxCommand::TabCloseByIndex { session, tab_index }
        }
        RemuxCommand::TabRenameById {
            session,
            tab_id,
            name,
        } => {
            let Some(tab_index) = tab_index_by_id(state, &session, tab_id).await else {
                return Ok(());
            };
            RemuxCommand::TabRenameByIndex {
                session,
                tab_index,
                name,
            }
        }
        other => other,
    };

    // -- Popup command guard -----------------------------------------------
    //
    // **THE layout-mutation chokepoint, and the structural half of the hard
    // invariant.** While the active tab's popup is visible it is the input
    // target, so a layout-mutating command would run with the popup as its
    // subject -- and
    // `PaneMove*` (swap_panes / relocate_pane_to_edge), `SetMaster` (promotes a
    // pane), `LayoutNext` (rebuilds from `pane_order`) and the stack ops
    // (splice nodes) would each be a route for the popup pane to get captured by
    // the layout. Blocking them here means no such command can ever run with a
    // popup subject; the other half of the invariant is that nothing ever
    // *inserts* the popup id into `pane_order` or a tree in the first place.
    //
    // Two-way: block -> pass.
    if !session_name.is_empty() {
        let (popup_visible, zoomed) = {
            let st = state.lock().await;
            let sess = st.sessions.get(&session_name);
            (
                sess.is_some_and(|s| s.visible_popup().is_some()),
                sess.and_then(|s| s.tabs.get(s.active_tab))
                    .is_some_and(|t| t.zoomed_pane.is_some()),
            )
        };
        if popup_visible {
            match cmd {
                // A directional focus key dismisses the popup and then steps
                // focus among the tab's panes from `focused_pane`, the pane the
                // user was on before the popup opened. Hiding here, in the same
                // handler, leaves no window between the close and the move for
                // another message to land in. The step is the server's: a
                // client forwards the key only while no sidebar is to take it,
                // because with the popup up the client does not treat the
                // focused pane as being at an edge. So the key never moves the
                // keyboard into a sidebar. The focus handler below sends the
                // frame, so the hide does not.
                // A float zoom takes the popup's place. `PaneToggleZoomFloat`
                // is not in the block list below, and `PaneToggleZoom` is only
                // when the mode is fullscreen. The toggle arm closes the popup
                // under its own lock, so another client cannot reopen it in
                // between.
                RemuxCommand::PaneToggleZoom if config.zoom.mode == ZoomMode::Float => {}
                // Releasing a zoom is always allowed, so a fullscreen zoom taken
                // before the popup opened is not stuck. Only taking a new
                // fullscreen zoom is refused, by the block list below.
                RemuxCommand::PaneToggleZoom | RemuxCommand::PaneToggleZoomFullscreen if zoomed => {
                }
                RemuxCommand::PaneFocusLeft
                | RemuxCommand::PaneFocusRight
                | RemuxCommand::PaneFocusUp
                | RemuxCommand::PaneFocusDown => {
                    hide_visible_popup(
                        &session_name,
                        state,
                        panes,
                        clients,
                        config,
                        prev_frames,
                        false,
                    )
                    .await?;
                }
                // Close closes the popup (resolved via the popup-aware
                // `input_target` below). Resize is NOT rerouted: the client owns
                // the popup's size and adjusts it itself, so the server only
                // keeps the layout from being resized underneath the popup.
                // Block: everything that reshapes the layout tree, `pane_order`,
                // or `zoomed_pane`. Two deliberate additions beyond the layout
                // mutators: `PaneNew` (it pushes to `pane_order`, and the user is
                // looking at the popup), and `PaneRename` -- the rename targets
                // `tab.focused_pane`, so starting one while the popup is up would
                // accumulate a hidden rename whose border cursor is suppressed by
                // the popup's cursor override, with the user's keystrokes going
                // to the popup's shell instead.
                RemuxCommand::ResizeLeft(_)
                | RemuxCommand::ResizeRight(_)
                | RemuxCommand::ResizeUp(_)
                | RemuxCommand::ResizeDown(_)
                | RemuxCommand::PaneRename(_)
                | RemuxCommand::PaneMoveLeft
                | RemuxCommand::PaneMoveRight
                | RemuxCommand::PaneMoveUp
                | RemuxCommand::PaneMoveDown
                | RemuxCommand::PaneSplitVertical
                | RemuxCommand::PaneSplitHorizontal
                | RemuxCommand::PaneNew
                | RemuxCommand::PaneStackAdd
                | RemuxCommand::PaneStackNext
                | RemuxCommand::PaneStackPrev
                | RemuxCommand::PaneStackIntoLeft
                | RemuxCommand::PaneStackIntoRight
                | RemuxCommand::PaneStackIntoUp
                | RemuxCommand::PaneStackIntoDown
                | RemuxCommand::PaneUnstackLeft
                | RemuxCommand::PaneUnstackRight
                | RemuxCommand::PaneUnstackUp
                | RemuxCommand::PaneUnstackDown
                | RemuxCommand::PaneMoveToTabTarget { .. }
                | RemuxCommand::PaneToggleZoom
                | RemuxCommand::PaneToggleZoomFullscreen
                | RemuxCommand::LayoutNext
                | RemuxCommand::SetMaster => {
                    log::debug!("server: command {cmd:?} is a no-op while the popup is open");
                    return Ok(());
                }
                // Pass: PopupToggle, PaneClose (popup-aware target), tab
                // switching (each tab keeps its own popup), and everything
                // non-layout.
                _ => {}
            }
        }
    }

    // A scrolled client is excluded from `broadcast_full_render` -- deliberately,
    // since that is also the PTY-output path -- so a command that reshapes the
    // screen would otherwise produce no frame for it at all. End the scroll
    // before dispatching, for the commands `RemuxCommand::returns_to_live_tail`
    // names. Do not "fix" this by rendering scrolled clients from the broadcast
    // instead; that filter is what keeps another pane's output from yanking a
    // scrolled reader to the bottom.
    //
    // Placed below every early return above -- the popup guard's block list in
    // particular -- so a command that is about to be refused cannot cost the
    // user their place in the scrollback.
    //
    // `PaneMoveToTabTarget` is refused here for the same reason: whether its
    // target tab still exists is only known from the state, not the command.
    if let RemuxCommand::PaneMoveToTabTarget { tab_id, session } = &cmd {
        let target = session::MoveRequest::from((*tab_id, session.clone()));
        let st = state.lock().await;
        let Some(pane) = move_source_pane(&st, &session_name) else {
            return Ok(());
        };
        if let Err(why) = st.check_move_pane_to_tab(&session_name, pane, &target) {
            log::info!(
                "server: PaneMoveToTabTarget pane_id={pane} target={target:?} refused: {why:?}"
            );
            return Ok(());
        }
    }
    if cmd.returns_to_live_tail(&session_name) {
        snap_client_to_live_tail(client_id, state, panes, clients, config, prev_frames).await;
    }

    match cmd {
        RemuxCommand::PopupToggle(position) => {
            if position.is_some() {
                log::debug!("server: PopupToggle position={position:?} ignored, the client places the popup");
            }
            // Lazily spawn the active tab's popup pane on that tab's first
            // use, then just flip visibility. The pane is registered in the pane
            // map ONLY -- never in `tab.pane_order`, never in a layout tree.
            let (spawn, source_pane_id) = {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let active_tab = sess.active_tab;
                let Some(tab) = sess.tabs.get_mut(active_tab) else {
                    return Ok(());
                };
                let source = Some(tab.focused_pane);
                match tab.popup_pane {
                    Some(existing) => {
                        tab.popup_visible = !tab.popup_visible;
                        if tab.popup_visible {
                            tab.release_float_zoom();
                        }
                        log::debug!(
                            "server: PopupToggle session={session_name:?} tab={active_tab} pane_id={existing} visible={}",
                            tab.popup_visible
                        );
                        (None, source)
                    }
                    None => {
                        let new_id = st.next_pane_id();
                        // Re-borrow: `next_pane_id` needed `st` mutably.
                        let Some(tab) = st
                            .sessions
                            .get_mut(&session_name)
                            .and_then(|s| s.tabs.get_mut(active_tab))
                        else {
                            return Ok(());
                        };
                        tab.popup_pane = Some(new_id);
                        tab.popup_visible = true;
                        tab.release_float_zoom();
                        log::debug!(
                            "server: PopupToggle session={session_name:?} tab={active_tab} spawned popup pane_id={new_id}"
                        );
                        (Some(new_id), source)
                    }
                }
            };
            if let Some(new_id) = spawn {
                // Inherit the focused pane's cwd, exactly like a split does.
                let focused_cwd = {
                    let panes_lock = panes.lock().await;
                    source_pane_id
                        .and_then(|id| panes_lock.get(&id))
                        .and_then(|p| persistence::get_pane_cwd(p.pty.child_pid))
                };
                let (interior_cols, interior_rows) =
                    session_popup_interior(&session_name, clients, config).await;
                if let Err(e) = spawn_pane(
                    new_id,
                    interior_cols,
                    interior_rows,
                    None,
                    &[],
                    focused_cwd.as_deref().map(std::path::Path::new),
                    Some(PaneIdentity {
                        session: &session_name,
                        pane: new_id,
                    }),
                    panes,
                    config,
                )
                .await
                {
                    // The tab was given the popup before the spawn. Left there,
                    // it would be a visible popup with no pane: the guard would
                    // block the layout, input would go nowhere, and the next
                    // toggle would only hide it. Looked up by id, because the
                    // active tab may have changed while the spawn was awaited.
                    let mut st = state.lock().await;
                    if let Some(tab) = st
                        .sessions
                        .get_mut(&session_name)
                        .and_then(|s| s.tabs.iter_mut().find(|t| t.popup_pane == Some(new_id)))
                    {
                        tab.take_popup();
                    }
                    log::error!(
                        "server: PopupToggle session={session_name:?} popup pane_id={new_id} failed to spawn: {e:#}"
                    );
                    return Err(e);
                }
                start_pty_forwarding(
                    &session_name,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                    dormant,
                )
                .await;
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            // Showing/hiding the popup changes the layer every asking client
            // holds; force a clean full render so no diff baseline can keep
            // stale popup cells.
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabNew => {
            // Capture the source pane (active tab's focused pane) BEFORE
            // create_tab, which flips active_tab to the new empty tab. The new
            // tab inherits the source pane's cwd.
            let (pane_id, source_pane_id) = {
                let mut st = state.lock().await;
                let source_pane_id = st
                    .sessions
                    .get(&session_name)
                    .and_then(|s| s.tabs.get(s.active_tab))
                    .map(|t| t.focused_pane);
                let tab_count = st
                    .sessions
                    .get(&session_name)
                    .map(|s| s.tabs.len())
                    .unwrap_or(0);
                let tab_name = format!("Tab {}", tab_count + 1);
                let layout_mode = config.new_tab_layout();
                let pane_id = st.create_tab(&session_name, &tab_name, layout_mode)?;
                (pane_id, source_pane_id)
            };
            let focused_cwd = {
                let panes_lock = panes.lock().await;
                source_pane_id
                    .and_then(|id| panes_lock.get(&id))
                    .and_then(|p| persistence::get_pane_cwd(p.pty.child_pid))
            };
            log::debug!("server: TabNew session={session_name:?} new pane_id={pane_id}");
            spawn_pane(
                pane_id,
                cols,
                rows,
                None,
                &[],
                focused_cwd.as_deref().map(std::path::Path::new),
                Some(PaneIdentity {
                    session: &session_name,
                    pane: pane_id,
                }),
                panes,
                config,
            )
            .await?;
            start_pty_forwarding(
                &session_name,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            // Resize the new tab's panes to the drawn content area (mirrors the
            // PaneSplit* path) so the child sees the same rows the compositor
            // draws — otherwise the footer is clipped.
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            // Tab switch: the whole displayed content changes, so invalidate
            // all clients' baselines to force a clean full render.
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabClose => {
            let tab_idx = {
                let st = state.lock().await;
                match st.sessions.get(&session_name) {
                    Some(s) => s.active_tab,
                    None => return Ok(()),
                }
            };
            log::debug!("server: TabClose session={session_name:?} tab_idx={tab_idx}");
            let (pane_ids, deleted) = {
                let mut st = state.lock().await;
                st.close_tab(&session_name, tab_idx)?
            };
            reap_panes(&pane_ids, panes, clients).await;
            if deleted {
                // Last tab closed -> the session was removed. Re-point attached
                // clients onto another session (or notify them if none remain);
                // don't broadcast the now-dead session.
                handle_session_removed(&session_name, state, panes, clients, config, prev_frames)
                    .await;
            } else {
                invalidate_session_baselines(&session_name, clients, prev_frames).await;
                broadcast_full_render(&session_name, state, panes, clients, config, prev_frames)
                    .await;
            }
        }
        RemuxCommand::TabGoto(idx) => {
            {
                let mut st = state.lock().await;
                st.goto_tab(&session_name, idx)?;
            }
            // Resize the newly-active tab's panes to the drawn content area
            // before rendering so the footer isn't clipped.
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabNext => {
            {
                let mut st = state.lock().await;
                let next = {
                    let sess = match st.sessions.get(&session_name) {
                        Some(s) => s,
                        None => return Ok(()),
                    };
                    (sess.active_tab + 1) % sess.tabs.len()
                };
                st.goto_tab(&session_name, next)?;
            }
            // Resize the newly-active tab's panes to the drawn content area
            // before rendering so the footer isn't clipped.
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabPrev => {
            {
                let mut st = state.lock().await;
                let prev = {
                    let sess = match st.sessions.get(&session_name) {
                        Some(s) => s,
                        None => return Ok(()),
                    };
                    if sess.active_tab == 0 {
                        sess.tabs.len().saturating_sub(1)
                    } else {
                        sess.active_tab - 1
                    }
                };
                st.goto_tab(&session_name, prev)?;
            }
            // Resize the newly-active tab's panes to the drawn content area
            // before rendering so the footer isn't clipped.
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabRename(name) => {
            log::debug!("server: TabRename session={session_name:?} new_name={name:?}");
            {
                let mut st = state.lock().await;
                let idx = {
                    match st.sessions.get(&session_name) {
                        Some(s) => s.active_tab,
                        None => return Ok(()),
                    }
                };
                st.rename_tab(&session_name, idx, &name)?;
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneSplitVertical | RemuxCommand::PaneSplitHorizontal => {
            let placement = if matches!(cmd, RemuxCommand::PaneSplitVertical) {
                PanePlacement::SplitVertical
            } else {
                PanePlacement::SplitHorizontal
            };
            create_pane_in_tab(
                &session_name,
                None,
                placement,
                cols,
                rows,
                None,
                &[],
                None,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?;
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneClose => {
            // Popup-aware target: while the popup is open this closes the popup
            // (killing its shell and clearing the state), which `close_pane`
            // handles in its dedicated popup branch.
            let closed_pane = {
                let st = state.lock().await;
                match st
                    .sessions
                    .get(&session_name)
                    .and_then(Session::input_target)
                {
                    Some(p) => p,
                    None => return Ok(()),
                }
            };
            log::debug!("server: PaneClose pane_id={closed_pane}");
            close_pane(closed_pane, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneFocusLeft
        | RemuxCommand::PaneFocusRight
        | RemuxCommand::PaneFocusUp
        | RemuxCommand::PaneFocusDown => {
            let direction = match cmd {
                RemuxCommand::PaneFocusLeft => layout::FocusDirection::Left,
                RemuxCommand::PaneFocusRight => layout::FocusDirection::Right,
                RemuxCommand::PaneFocusUp => layout::FocusDirection::Up,
                RemuxCommand::PaneFocusDown => layout::FocusDirection::Down,
                _ => unreachable!(),
            };
            log::debug!("server: PaneFocus direction={direction:?}");
            let zoom_moved = {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: cols,
                    height: rows.saturating_sub(1),
                };
                // A float zoom snaps back and the key then acts on the layout,
                // from the pane that was floated. A fullscreen zoom is carried
                // along with focus instead, below.
                let released_float = tab.release_float_zoom();
                // Stack-aware directional focus (zellij behavior): step within a
                // multi-pane stack first, else fall back to the spatial neighbor.
                let before = tab.focused_pane;
                if let Some(target) = layout::focus_in_direction(
                    &mut tab.layout,
                    area,
                    tab.focused_pane,
                    direction,
                    0,
                ) {
                    tab.focus_pane(target);
                }
                released_float || zoom_moved_to_another_pane(tab, before)
            };
            if zoom_moved {
                resize_session_panes(&session_name, state, panes, clients, config).await?;
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneMoveLeft
        | RemuxCommand::PaneMoveRight
        | RemuxCommand::PaneMoveUp
        | RemuxCommand::PaneMoveDown => {
            let direction = match cmd {
                RemuxCommand::PaneMoveLeft => layout::FocusDirection::Left,
                RemuxCommand::PaneMoveRight => layout::FocusDirection::Right,
                RemuxCommand::PaneMoveUp => layout::FocusDirection::Up,
                RemuxCommand::PaneMoveDown => layout::FocusDirection::Down,
                _ => unreachable!(),
            };
            log::debug!("server: PaneMove direction={direction:?}");
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: cols,
                    height: rows.saturating_sub(1),
                };
                // Inside a stack of several panes, Left/Right reorder the stack
                // and, at its end, take the pane out on that side. Up/Down
                // take it out at once, because a stack has no vertical order.
                let shift =
                    layout::shift_in_stack(&mut tab.layout, tab.focused_pane, direction.clone());
                match shift {
                    layout::StackShift::Moved(other) => {
                        // The tab stays automatic: Monocle is the only automatic
                        // layout that stacks, and it copies `pane_order` into its
                        // stack verbatim, so the same swap in `pane_order` keeps a
                        // rebuild from undoing the move.
                        if tab.layout_mode.is_automatic() {
                            let focused = tab.focused_pane;
                            let a = tab.pane_order.iter().position(|&p| p == focused);
                            let b = tab.pane_order.iter().position(|&p| p == other);
                            if let (Some(a), Some(b)) = (a, b) {
                                tab.pane_order.swap(a, b);
                            }
                        }
                    }
                    layout::StackShift::AtEdge => unstack_focused(sess, direction, "PaneMove"),
                    layout::StackShift::NotStacked => {
                        if let Some(neighbor) = layout::find_neighbor(
                            &tab.layout,
                            area,
                            tab.focused_pane,
                            direction.clone(),
                            0,
                        ) {
                            // Swap the focused pane with its spatial neighbor in `direction`.
                            // Focus stays on the moved pane (its id is unchanged; only its
                            // slot in the tree changes).
                            if layout::swap_panes(&mut tab.layout, tab.focused_pane, neighbor)
                                && tab.layout_mode.is_automatic()
                            {
                                // A manual move ejects to Custom so an automatic rebuild
                                // from `pane_order` doesn't revert the swap.
                                tab.layout_mode = LayoutMode::Custom(CustomLayout);
                            }
                        } else if let Some(new_tree) =
                            layout::relocate_pane_to_edge(&tab.layout, tab.focused_pane, direction)
                        {
                            // No neighbor in `direction`: the focused pane is at that
                            // edge. Relocate it, restructuring the layout, and always
                            // eject to Custom so it isn't rebuilt away.
                            tab.layout = new_tree;
                            tab.layout_mode = LayoutMode::Custom(CustomLayout);
                        }
                    }
                }
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneStackIntoLeft
        | RemuxCommand::PaneStackIntoRight
        | RemuxCommand::PaneStackIntoUp
        | RemuxCommand::PaneStackIntoDown => {
            let direction = match cmd {
                RemuxCommand::PaneStackIntoLeft => layout::FocusDirection::Left,
                RemuxCommand::PaneStackIntoRight => layout::FocusDirection::Right,
                RemuxCommand::PaneStackIntoUp => layout::FocusDirection::Up,
                RemuxCommand::PaneStackIntoDown => layout::FocusDirection::Down,
                _ => unreachable!(),
            };
            log::debug!("server: PaneStackInto direction={direction:?}");
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: cols,
                    height: rows.saturating_sub(1),
                };
                // The same two rules `create_pane_in_tab` applies to
                // `PaneStackAdd`: a manual stack must eject to Custom or the
                // next automatic rebuild flattens it, and a new arrangement
                // releases the zoom. Applied only when the move happened, so a
                // refused move leaves the tab exactly as it was.
                if layout::stack_into_neighbor(
                    &mut tab.layout,
                    area,
                    tab.focused_pane,
                    direction,
                    0,
                ) {
                    tab.layout_mode = LayoutMode::Custom(CustomLayout);
                    tab.zoomed_pane = None;
                    session::debug_check_invariant(sess, "PaneStackInto");
                }
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneUnstackLeft
        | RemuxCommand::PaneUnstackRight
        | RemuxCommand::PaneUnstackUp
        | RemuxCommand::PaneUnstackDown => {
            let direction = match cmd {
                RemuxCommand::PaneUnstackLeft => layout::FocusDirection::Left,
                RemuxCommand::PaneUnstackRight => layout::FocusDirection::Right,
                RemuxCommand::PaneUnstackUp => layout::FocusDirection::Up,
                RemuxCommand::PaneUnstackDown => layout::FocusDirection::Down,
                _ => unreachable!(),
            };
            log::debug!("server: PaneUnstack direction={direction:?}");
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                unstack_focused(sess, direction, "PaneUnstack");
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneMoveToTabTarget {
            tab_id,
            session: target_session,
        } => {
            let target = session::MoveRequest::from((tab_id, target_session));
            let outcome = {
                let mut st = state.lock().await;
                match move_source_pane(&st, &session_name) {
                    Some(pane) => (
                        pane,
                        st.move_pane_to_tab(
                            &session_name,
                            pane,
                            target.clone(),
                            config.new_tab_layout(),
                        ),
                    ),
                    None => return Ok(()),
                }
            };
            let (pane, moved) = match outcome {
                (pane, Ok(moved)) => {
                    log::debug!(
                        "server: PaneMoveToTabTarget pane_id={pane} target={target:?} -> \
                         tab index {} session={:?} source_removed={}",
                        moved.target_idx,
                        moved.target_session,
                        moved.source_removed.is_some()
                    );
                    (pane, moved)
                }
                (pane, Err(why)) => {
                    log::info!(
                        "server: PaneMoveToTabTarget pane_id={pane} target={target:?} refused: {why:?}"
                    );
                    return Ok(());
                }
            };
            if let Some(popup) = &moved.source_removed {
                reap_panes(popup, panes, clients).await;
            }
            if let Some(popup) = moved.source_tab_popup {
                reap_panes(&[popup], panes, clients).await;
            }
            if moved.target_session != session_name {
                // A client of the source that scrolled the pane back would
                // otherwise keep that offset for a pane it can no longer see.
                // The offset outlives the move, and when the pane is that
                // client's focused pane again the broadcast skips the client,
                // which freezes its screen.
                {
                    let mut cls = clients.lock().await;
                    for c in cls.values_mut() {
                        c.session_scroll.remove(&pane);
                        if c.prev_scroll_pane == Some(pane) {
                            c.prev_scroll_pane = None;
                            c.prev_scroll_offset = 0;
                        }
                    }
                }
                // The requester follows its pane. Re-pointing it BEFORE the
                // source is handled is what keeps it out of
                // `handle_session_removed`, which would land it on an arbitrary
                // session instead of the one it chose.
                handle_attach(
                    client_id,
                    &moved.target_session,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                    dormant,
                )
                .await?;
                if moved.source_removed.is_some() {
                    handle_session_removed(
                        &session_name,
                        state,
                        panes,
                        clients,
                        config,
                        prev_frames,
                    )
                    .await;
                } else {
                    resize_session_panes(&session_name, state, panes, clients, config).await?;
                    invalidate_session_baselines(&session_name, clients, prev_frames).await;
                    broadcast_full_render(
                        &session_name,
                        state,
                        panes,
                        clients,
                        config,
                        prev_frames,
                    )
                    .await;
                }
            }
            resize_session_panes(&moved.target_session, state, panes, clients, config).await?;
            invalidate_session_baselines(&moved.target_session, clients, prev_frames).await;
            broadcast_full_render(
                &moved.target_session,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await;
            mark_agents_dirty();
        }
        RemuxCommand::PaneStackAdd => {
            create_pane_in_tab(
                &session_name,
                None,
                PanePlacement::Stack,
                cols,
                rows,
                None,
                &[],
                None,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?;
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneStackNext => {
            let zoom_moved = {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let before = tab.focused_pane;
                if let Some(next) = tab.layout.stack_next(tab.focused_pane) {
                    tab.focus_pane(next);
                }
                zoom_moved_to_another_pane(tab, before)
            };
            if zoom_moved {
                resize_session_panes(&session_name, state, panes, clients, config).await?;
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneStackPrev => {
            let zoom_moved = {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let before = tab.focused_pane;
                if let Some(prev) = tab.layout.stack_prev(tab.focused_pane) {
                    tab.focus_pane(prev);
                }
                zoom_moved_to_another_pane(tab, before)
            };
            if zoom_moved {
                resize_session_panes(&session_name, state, panes, clients, config).await?;
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::ResizeLeft(amount)
        | RemuxCommand::ResizeRight(amount)
        | RemuxCommand::ResizeUp(amount)
        | RemuxCommand::ResizeDown(amount) => {
            let (direction, delta) = match &cmd {
                RemuxCommand::ResizeLeft(_) => {
                    (layout::Direction::Vertical, -(amount as f32) / 100.0)
                }
                RemuxCommand::ResizeRight(_) => {
                    (layout::Direction::Vertical, amount as f32 / 100.0)
                }
                RemuxCommand::ResizeUp(_) => {
                    (layout::Direction::Horizontal, -(amount as f32) / 100.0)
                }
                RemuxCommand::ResizeDown(_) => {
                    (layout::Direction::Horizontal, amount as f32 / 100.0)
                }
                _ => unreachable!(),
            };
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // Eject to Custom mode on manual resize
                if tab.layout_mode.is_automatic() {
                    tab.layout_mode = LayoutMode::Custom(CustomLayout);
                }
                tab.layout.resize(tab.focused_pane, direction, delta);
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::ToggleStyle => {
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                sess.border_style = match sess.border_style {
                    BorderStyle::ZellijStyle => BorderStyle::TmuxStyle,
                    BorderStyle::TmuxStyle => BorderStyle::ZellijStyle,
                };
                log::debug!("server: ToggleStyle new_style={:?}", sess.border_style);
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SessionDetach => {
            handle_detach(client_id, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SessionList => {
            handle_list_sessions(client_id, state, clients).await?;
        }
        RemuxCommand::SessionRename(new_name) => {
            log::debug!("server: SessionRename old={session_name:?} new={new_name:?}");
            {
                let mut st = state.lock().await;
                st.rename_session(&session_name, &new_name)?;
            }
            let mut cls = clients.lock().await;
            for client in cls.values_mut() {
                if client.session_name.as_deref() == Some(&session_name) {
                    client.session_name = Some(new_name.clone());
                }
            }
            announce_session_rename(&cls, &session_name, &new_name);
        }
        RemuxCommand::PaneRename(name) => {
            let name = crate::config::pane_title::sanitize_label(&name);
            log::debug!("server: PaneRename new_name={name:?}");
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                // Clear rename state now that the rename is committed.
                sess.rename_state = None;
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                layout::set_pane_custom_name(&mut tab.layout, tab.focused_pane, &name);
                layout::set_pane_name(&mut tab.layout, tab.focused_pane, &name);
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SendKey(bytes) => {
            // Forward raw key bytes to the active pane's PTY.
            let pane_id = {
                let st = state.lock().await;
                let sess = match st.sessions.get(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                tab.focused_pane
            };
            let mut panes_lock = panes.lock().await;
            if let Some(pane) = panes_lock.get_mut(&pane_id) {
                pane.send_human_input(&bytes);
            }
        }
        RemuxCommand::PaneNew => {
            create_pane_in_tab(
                &session_name,
                None,
                PanePlacement::Auto,
                cols,
                rows,
                None,
                &[],
                None,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?;
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::LayoutNext => {
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // Snapshot the current custom arrangement *before* touching the
                // tree, so the cycle can return to it later. Order matters: a
                // rebuild below would otherwise overwrite `tab.layout`.
                if matches!(tab.layout_mode, LayoutMode::Custom(_)) {
                    tab.saved_custom_layout = Some(tab.layout.clone());
                }
                // Cycling the layout releases the zoom, for the same reason a
                // new pane does: the tab is showing a different arrangement, and
                // a zoom would hide all of it. Cleared *first*, so the
                // `focus_pane` below can't carry a stale zoom into the restored
                // tree -- and so the cycle can never park a live zoom in Monocle,
                // the one mode that refuses to take one.
                tab.zoomed_pane = None;
                // The last enabled automatic mode returns to Custom only when
                // the saved tree is still restorable (its pane set matches the
                // live panes); otherwise the cycle stays automatic.
                let restorable =
                    saved_custom_is_restorable(&tab.saved_custom_layout, &tab.pane_order);
                tab.layout_mode = next_layout_mode(&tab.layout_mode, restorable, &config.layouts);
                log::debug!("server: LayoutNext new_mode={}", tab.layout_mode.name());
                if tab.layout_mode.is_automatic() {
                    tab.rebuild_if_automatic();
                } else if let Some(saved) = tab.saved_custom_layout.clone() {
                    // Custom: restore the remembered arrangement. Adopt its own
                    // active-pane markers so the focused pane stays visible.
                    // The live names replace the snapshot's, which predate any
                    // rename made while the tab was automatic.
                    tab.replace_layout(saved);
                    if let Some(active) = tab.layout.active_pane() {
                        tab.focus_pane(active);
                    }
                }
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SetMaster => {
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // Snapshot the manual arrangement *before* the rebuild below
                // discards it, exactly as `LayoutNext` does -- otherwise
                // promoting a master out of a hand-built (e.g. stacked) layout
                // would lose it with no way back through the cycle.
                if matches!(tab.layout_mode, LayoutMode::Custom(_)) {
                    tab.saved_custom_layout = Some(tab.layout.clone());
                }
                // Promoting a master rebuilds the arrangement, so it releases
                // the zoom too (see `LayoutNext`).
                tab.zoomed_pane = None;
                // Switch to Master layout if not already in it.
                if !matches!(tab.layout_mode, LayoutMode::Master(_)) {
                    tab.layout_mode = LayoutMode::Master(MasterLayout::default());
                }
                if let LayoutMode::Master(ref mut master_layout) = tab.layout_mode {
                    master_layout.master_pane = Some(tab.focused_pane);
                    tab.rebuild_if_automatic();
                }
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SessionSwitchTab { session, tab_index } => {
            // Attach the client to the specified session and switch to the tab.
            switch_client_session(
                client_id,
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await;
            {
                let mut st = state.lock().await;
                if let Err(e) = st.goto_tab(&session, tab_index) {
                    log::error!("SessionSwitchTab: {e}");
                    return Ok(());
                }
            }
            start_pty_forwarding(
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            resize_session_panes(&session, state, panes, clients, config).await?;
            // Invalidate all attached clients' baselines to force a fresh full
            // render (not a diff against stale content from a previous tab).
            invalidate_session_baselines(&session, clients, prev_frames).await;
            send_full_render_to_client(
                client_id,
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
                RenderCtx::default(),
            )
            .await;
        }
        RemuxCommand::SessionSwitchPane {
            session,
            tab_index,
            pane_id,
        } => {
            // Attach to session, switch tab, and focus pane.
            switch_client_session(
                client_id,
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await;
            {
                let mut st = state.lock().await;
                if let Err(e) = st.goto_tab(&session, tab_index) {
                    log::error!("SessionSwitchPane: goto_tab: {e}");
                    return Ok(());
                }
                if let Some(sess) = st.sessions.get_mut(&session) {
                    if let Some(tab) = sess.tabs.get_mut(sess.active_tab) {
                        // The id comes from the client's copy of the tree, which
                        // can name a pane that closed since. Focusing it would
                        // also carry an active zoom onto a pane the tab does
                        // not own.
                        if !tab.pane_order.contains(&pane_id) {
                            log::warn!(
                                "SessionSwitchPane: pane {pane_id} is not in tab {tab_index} of '{session}'"
                            );
                            return Ok(());
                        }
                        // The tab's visible popup owns input, and hiding it
                        // returns input to `focused_pane`. Moving that here would
                        // send the user somewhere else when the popup closes, so
                        // the switch lands on the tab and its popup only.
                        if tab.popup_visible {
                            log::debug!(
                                "SessionSwitchPane: tab {tab_index} of '{session}' has its popup up, pane {pane_id} is not focused"
                            );
                        } else {
                            // A float zoom snaps back and the switch then acts,
                            // as a click outside the box does. Without this the
                            // float would move to the pane switched to.
                            if tab.focused_pane != pane_id {
                                tab.release_float_zoom();
                            }
                            tab.focus_pane(pane_id);
                        }
                    }
                    session::debug_check_invariant(sess, "SessionSwitchPane");
                }
            }
            start_pty_forwarding(
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            resize_session_panes(&session, state, panes, clients, config).await?;
            // Invalidate all attached clients' baselines to force a fresh full
            // render.
            invalidate_session_baselines(&session, clients, prev_frames).await;
            send_full_render_to_client(
                client_id,
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
                RenderCtx::default(),
            )
            .await;
        }
        RemuxCommand::TabCloseByIndex {
            session: target_session,
            tab_index,
        } => {
            let close_result = {
                let mut st = state.lock().await;
                st.close_tab(&target_session, tab_index)
            };
            match close_result {
                Ok((pane_ids, session_deleted)) => {
                    reap_panes(&pane_ids, panes, clients).await;
                    if session_deleted {
                        // Last tab of the target session closed -> it was
                        // removed. Re-point its attached clients onto another
                        // session (or notify them if none remain).
                        handle_session_removed(
                            &target_session,
                            state,
                            panes,
                            clients,
                            config,
                            prev_frames,
                        )
                        .await;
                    } else {
                        // Closing a tab may switch the active tab. The requesting
                        // client (session manager) is generally viewing a
                        // different session, so its cols/rows don't apply here.
                        // Derive the target session's dims from its own attached
                        // clients (min, as broadcast_full_render does) and resize
                        // the newly-active tab's panes so the footer isn't clipped.
                        let target_dims = {
                            let cls = clients.lock().await;
                            let cols = cls
                                .values()
                                .filter(|c| c.session_name.as_deref() == Some(&target_session))
                                .map(|c| c.cols)
                                .min();
                            let rows = cls
                                .values()
                                .filter(|c| c.session_name.as_deref() == Some(&target_session))
                                .map(|c| c.rows)
                                .min();
                            cols.zip(rows)
                        };
                        if target_dims.is_some() {
                            resize_session_panes(&target_session, state, panes, clients, config)
                                .await?;
                        }
                        invalidate_session_baselines(&target_session, clients, prev_frames).await;
                        broadcast_full_render(
                            &target_session,
                            state,
                            panes,
                            clients,
                            config,
                            prev_frames,
                        )
                        .await;
                    }
                }
                Err(e) => {
                    log::error!("TabCloseByIndex: {e}");
                }
            }
        }
        RemuxCommand::FolderNew(name) => {
            let mut st = state.lock().await;
            st.create_folder(&name)?;
        }
        RemuxCommand::FolderDelete(name) => {
            let deleted_sessions = {
                let mut st = state.lock().await;
                st.delete_folder_cascade(&name)?
            };
            // Clean up panes and notify clients for each deleted session.
            {
                let doomed: Vec<PaneId> = deleted_sessions
                    .iter()
                    .flat_map(|(_, pane_ids)| pane_ids.iter().copied())
                    .collect();
                reap_panes(&doomed, panes, clients).await;
            }
            {
                let mut cls = clients.lock().await;
                for (session_name, _) in &deleted_sessions {
                    for client in cls.values_mut() {
                        if client.session_name.as_deref() == Some(session_name) {
                            client.session_name = None;
                            let _ =
                                client
                                    .tx
                                    .send(ServerMessage::Event(SessionEvent::SessionDeleted(
                                        session_name.clone(),
                                    )));
                        }
                    }
                }
            }
        }
        RemuxCommand::FolderMoveSession { session, folder } => {
            let mut st = state.lock().await;
            st.move_session(&session, folder.as_deref())?;
        }
        RemuxCommand::FolderList => {
            // Handled via SessionList or ListSessionTree.
        }
        RemuxCommand::SessionNew { name, folder } => {
            handle_create_session(
                client_id,
                &name,
                folder.as_deref(),
                state,
                panes,
                clients,
                config,
            )
            .await?;
        }
        RemuxCommand::TabMoveBy(delta) => {
            // `TabMove` takes the destination as a 1-based position and clamps
            // it, so the move is that of the active tab by `delta`.
            let destination = {
                let st = state.lock().await;
                st.sessions.get(&session_name).map(|sess| {
                    let last = sess.tabs.len().saturating_sub(1) as i64;
                    ((sess.active_tab as i64 + i64::from(delta)).clamp(0, last) + 1) as usize
                })
            };
            if let Some(destination) = destination {
                return Box::pin(handle_command(
                    client_id,
                    RemuxCommand::TabMove(destination),
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                    dormant,
                ))
                .await;
            }
        }
        RemuxCommand::PaneEnsureZoom => {
            let already = {
                let st = state.lock().await;
                st.sessions
                    .get(&session_name)
                    .and_then(|s| s.tabs.get(s.active_tab))
                    .is_none_or(|t| t.zoomed_pane.is_some())
            };
            if !already {
                return Box::pin(handle_command(
                    client_id,
                    RemuxCommand::PaneToggleZoom,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                    dormant,
                ))
                .await;
            }
        }
        RemuxCommand::TabMove(idx) => {
            let mut st = state.lock().await;
            if let Some(sess) = st.sessions.get_mut(&session_name) {
                let target = idx.saturating_sub(1).min(sess.tabs.len().saturating_sub(1));
                let current = sess.active_tab;
                if current != target && current < sess.tabs.len() {
                    let tab = sess.tabs.remove(current);
                    sess.tabs.insert(target, tab);
                    sess.active_tab = target;
                }
            }
            drop(st);
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneToggleZoom
        | RemuxCommand::PaneToggleZoomFullscreen
        | RemuxCommand::PaneToggleZoomFloat => {
            let mode = match cmd {
                RemuxCommand::PaneToggleZoomFloat => ZoomMode::Float,
                RemuxCommand::PaneToggleZoomFullscreen => ZoomMode::Fullscreen,
                _ => config.zoom.mode,
            };
            let popup_closed;
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // `Tab::toggle_zoom` owns the rule that only *engaging* a zoom
                // can be refused (Monocle is already fullscreen); releasing one
                // always works, so the zoom can never become unreachable.
                let popup_was_up = tab.popup_visible;
                let changed = tab.toggle_zoom(mode);
                popup_closed = popup_was_up && !tab.popup_visible;
                log::debug!(
                    "server: PaneToggleZoom mode={mode:?} pane={} zoomed={} changed={changed}",
                    tab.focused_pane,
                    tab.zoomed_pane.is_some()
                );
                // Nothing moved (a redundant zoom request in Monocle): skip the
                // resize + repaint entirely, as this arm always has.
                if !changed {
                    return Ok(());
                }
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            if popup_closed {
                // A client that holds a popup layer must not diff against it.
                invalidate_session_baselines(&session_name, clients, prev_frames).await;
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        // -- Explicit-target structural commands ----------------------------
        // These act on a named/indexed target rather than the requester's
        // attached session (the session manager may be attached elsewhere or
        // nowhere). They never read the local `session_name`. On failure they
        // fail silently (log + early return) rather than propagating an error,
        // and after a successful mutation they refresh the *target* session's
        // attached clients (if any) via `refresh_target_session`.
        RemuxCommand::SessionRenameByName { old, new } => {
            log::debug!("server: SessionRenameByName old={old:?} new={new:?}");
            {
                let mut st = state.lock().await;
                if let Err(e) = st.rename_session(&old, &new) {
                    log::info!("SessionRenameByName: {e}");
                    return Ok(());
                }
            }
            // Retarget any clients still attached under the old name so their
            // input/render continues to resolve to the renamed session.
            {
                let mut cls = clients.lock().await;
                for client in cls.values_mut() {
                    if client.session_name.as_deref() == Some(&old) {
                        client.session_name = Some(new.clone());
                    }
                }
                announce_session_rename(&cls, &old, &new);
            }
        }
        RemuxCommand::FolderRename { old, new } => {
            log::debug!("server: FolderRename old={old:?} new={new:?}");
            let mut st = state.lock().await;
            if let Err(e) = st.rename_folder(&old, &new) {
                log::info!("FolderRename: {e}");
            }
        }
        RemuxCommand::TabNewInSession { session } => {
            create_tab_in_session(
                &session,
                None,
                &[],
                None,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?;
        }
        RemuxCommand::TabRenameByIndex {
            session,
            tab_index,
            name,
        } => {
            log::debug!(
                "server: TabRenameByIndex session={session:?} idx={tab_index} name={name:?}"
            );
            {
                let mut st = state.lock().await;
                if let Err(e) = st.rename_tab(&session, tab_index, &name) {
                    log::info!("TabRenameByIndex: {e}");
                    return Ok(());
                }
            }
            refresh_target_session(&session, state, panes, clients, config, prev_frames).await?;
        }
        RemuxCommand::PaneNewInTab { session, tab_index } => {
            // Same placement as `PaneNew`, but on tab `tab_index` of the named
            // target session (`PaneNew` operates on the requester's focused tab),
            // sized from that session's own render size and refreshing only the
            // clients attached to it.
            let (tcols, trows) = session_render_size(&session, clients).await;
            if create_pane_in_tab(
                &session,
                Some(tab_index),
                PanePlacement::Auto,
                tcols,
                trows,
                None,
                &[],
                None,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?
            .is_none()
            {
                return Ok(());
            }
            refresh_target_session(&session, state, panes, clients, config, prev_frames).await?;
        }
        RemuxCommand::PaneCloseById { session, pane_id } => {
            log::debug!("server: PaneCloseById session={session:?} pane_id={pane_id}");
            // If the pane is in the target session's active tab, reuse close_pane
            // (which handles layout collapse, tab removal, last-session removal,
            // client switch, and its own resize/broadcast). If it lives in a
            // background tab, close_pane would silently no-op, so handle that
            // path inline here.
            let in_active_tab = {
                let st = state.lock().await;
                st.sessions
                    .get(&session)
                    .and_then(|s| s.tabs.get(s.active_tab))
                    .map(|t| t.pane_order.contains(&pane_id))
                    .unwrap_or(false)
            };
            if in_active_tab {
                close_pane(pane_id, state, panes, clients, config, prev_frames).await;
            } else {
                // Background-tab path: mirror close_pane's non-active-tab-safe
                // subset (layout collapse + pane_order retain + focus/rebuild,
                // and tab removal if it became empty). A background tab implies
                // at least the active tab also exists, so the session can never
                // be emptied here — the last-session/switch/disconnect branch is
                // unreachable and intentionally omitted.
                let mut to_reap = vec![pane_id];
                {
                    let mut st = state.lock().await;
                    let sess = match st.sessions.get_mut(&session) {
                        Some(s) => s,
                        None => return Ok(()),
                    };
                    let tab_idx = match sess
                        .tabs
                        .iter()
                        .position(|t| t.pane_order.contains(&pane_id))
                    {
                        Some(i) => i,
                        None => {
                            log::info!(
                                "PaneCloseById: pane {pane_id} not found in session '{session}'"
                            );
                            return Ok(());
                        }
                    };
                    let tab = &mut sess.tabs[tab_idx];
                    match tab.detach_pane(pane_id) {
                        Some(nf) => {
                            // As in `close_pane`: only the focused pane closing
                            // moves focus.
                            if tab.focused_pane == pane_id {
                                tab.focus_pane(nf);
                            }
                            tab.rebuild_if_automatic();
                            session::debug_check_invariant(sess, "PaneCloseById");
                        }
                        None => {
                            // Last pane in this background tab -> remove the tab.
                            // Adjust active_tab if it sat after the removed tab
                            // (close_pane never does this — it only removes the
                            // active tab itself).
                            to_reap.extend(sess.tabs.remove(tab_idx).take_popup());
                            if tab_idx < sess.active_tab {
                                sess.active_tab -= 1;
                            }
                        }
                    }
                }
                reap_panes(&to_reap, panes, clients).await;
                refresh_target_session(&session, state, panes, clients, config, prev_frames)
                    .await?;
            }
        }
        RemuxCommand::PaneRenameById {
            session,
            pane_id,
            name,
        } => {
            let name = crate::config::pane_title::sanitize_label(&name);
            log::debug!(
                "server: PaneRenameById session={session:?} pane_id={pane_id} name={name:?}"
            );
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session) {
                    Some(s) => s,
                    None => {
                        log::info!("PaneRenameById: session '{session}' not found");
                        return Ok(());
                    }
                };
                // Locate the tab owning this pane (any tab, not just the active
                // one) and set its custom name, mirroring PaneRename.
                let mut found = false;
                for tab in sess.tabs.iter_mut() {
                    if tab.pane_order.contains(&pane_id) {
                        layout::set_pane_custom_name(&mut tab.layout, pane_id, &name);
                        layout::set_pane_name(&mut tab.layout, pane_id, &name);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::info!("PaneRenameById: pane {pane_id} not found in session '{session}'");
                    return Ok(());
                }
            }
            refresh_target_session(&session, state, panes, clients, config, prev_frames).await?;
        }
        RemuxCommand::TabMoveByIndex {
            session,
            tab_index,
            delta,
        } => {
            log::debug!("server: TabMoveByIndex session={session:?} idx={tab_index} delta={delta}");
            {
                let mut st = state.lock().await;
                if let Err(e) = st.move_tab(&session, tab_index, delta) {
                    log::info!("TabMoveByIndex: {e}");
                    return Ok(());
                }
            }
            refresh_target_session(&session, state, panes, clients, config, prev_frames).await?;
        }
        _ => {
            log::debug!("unhandled command: {cmd:?}");
        }
    }

    // Persist state after every command that may have changed structure. The
    // same hook feeds the session-tree push: a command that may have changed
    // structure is exactly a command that may have changed what a subscriber's
    // tree shows. Deliberately here rather than inside `save_if_enabled`, whose
    // body is skipped entirely when `save_sessions = false` -- pushes must not
    // depend on the persistence toggle. The focus commands (`PaneFocus*`,
    // `TabGoto`, the session switches) mark nothing themselves, so this is also
    // the hook that re-pushes the agent list's `focused` flags.
    save_if_enabled(state, panes, config, dormant).await;
    mark_focus_changed();

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_create_session(
    client_id: u64,
    name: &str,
    folder: Option<&str>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) -> Result<()> {
    let (cols, rows) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => (c.cols, c.rows),
            None => (80, 24),
        }
    };
    let pane_id = {
        let mut st = state.lock().await;
        let border_style = config.appearance.border_style.clone();
        let layout_mode = config.new_tab_layout();
        st.create_session(name, folder, border_style, layout_mode)?
    };
    log::debug!("server: CreateSession name={name:?} folder={folder:?} pane_id={pane_id}");
    spawn_pane(
        pane_id,
        cols,
        rows,
        None,
        &[],
        None,
        Some(PaneIdentity {
            session: name,
            pane: pane_id,
        }),
        panes,
        config,
    )
    .await?;

    // Announce to EVERY client, not just the creator. A session manager open in
    // another terminal has no timer -- every tree refresh is event-driven -- so
    // a creator-only notification left every other terminal's tree stale until
    // some unrelated action happened to refresh it. Symmetric with
    // `SessionDeleted`, which already reaches every client it concerns.
    let cls = clients.lock().await;
    for client in cls.values() {
        let _ = client
            .tx
            .send(ServerMessage::Event(SessionEvent::SessionCreated(
                name.to_string(),
            )));
    }
    Ok(())
}

async fn handle_list_sessions(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let st = state.lock().await;
    let cls = clients.lock().await;
    let entries: Vec<SessionListEntry> = st
        .list_sessions()
        .into_iter()
        .map(|info| {
            let client_count = cls
                .values()
                .filter(|c| c.session_name.as_deref() == Some(&info.name))
                .count();
            SessionListEntry {
                name: info.name,
                folder: info.folder,
                tab_count: info.tab_count,
                client_count,
            }
        })
        .collect();
    if let Some(client) = cls.get(&client_id) {
        let _ = client
            .tx
            .send(ServerMessage::SessionList { sessions: entries });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Session-tree push
// ---------------------------------------------------------------------------

/// Minimum spacing between two `SessionTree` broadcasts, so a burst of
/// structural commands costs subscribers one extra push rather than one push
/// per command.
const SESSION_TREE_PUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Wakes the session-tree push task when something a subscriber's tree
/// *displays* has changed.
///
/// A process-global signal rather than a handle threaded through the call
/// graph, for two reasons:
///
/// 1. One of the change points is [`update_auto_pane_names`], reached from
///    `broadcast_full_render` and `send_full_render_to_client` -- some fifty
///    call sites that would each have to carry a handle they have no other use
///    for.
/// 2. More importantly it keeps [`mark_session_tree_dirty`] *synchronous*. The
///    change points run while the `state`/`panes` guards are held; an async
///    broadcast there would relock them on the same task and deadlock, whereas
///    a bare `notify_one` cannot.
///
/// The coalescing falls out of `Notify`'s semantics: it stores at most one
/// permit, so any number of changes arriving while the pusher is asleep wake it
/// exactly once. The server is a singleton process, which is the scope a global
/// is correct at here.
static SESSION_TREE_DIRTY: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Record that the session tree has changed. Cheap and non-blocking; safe to
/// call while holding any server lock. With no subscribers the pusher wakes,
/// finds none, and parks again.
fn mark_session_tree_dirty() {
    SESSION_TREE_DIRTY.notify_one();
}

/// Build and send [`ServerMessage::SessionTree`] to each client in `targets`,
/// skipping ids that are no longer connected.
///
/// The expensive inputs -- the dormant name list, per-session client counts and
/// one `get_process_name` per live pane -- are snapshotted once and shared;
/// only `build_session_tree` runs per recipient, because `is_current` is
/// recipient-relative. Backpressure is a non-issue and deliberately so: `tx` is
/// an unbounded channel, so the send cannot block the daemon on a slow client,
/// and a send to a dead connection returns `Err`, which is discarded exactly as
/// [`broadcast_view_list`] does.
///
/// Locks in the codebase's `dormant` -> `state` -> `clients` -> `panes` order.
async fn send_session_tree_to(
    targets: &[u64],
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    dormant: &DormantStore,
) {
    if targets.is_empty() {
        return;
    }

    let dormant_names: Vec<String> = {
        let d = dormant.lock().await;
        let mut names: Vec<String> = d.state.sessions.keys().cloned().collect();
        names.sort();
        names
    };

    let st = state.lock().await;
    let cls = clients.lock().await;

    // Compute client counts per session.
    let mut client_counts: HashMap<String, usize> = HashMap::new();
    for c in cls.values() {
        if let Some(ref sn) = c.session_name {
            *client_counts.entry(sn.clone()).or_insert(0) += 1;
        }
    }

    // A user-set custom pane name (PaneRename) takes precedence over the
    // resolved name, matching what the pane border shows. Resolved once per
    // distinct naming among the targets, since each client names panes with
    // its own template.
    let ps = panes.lock().await;
    let resolve_names = |naming: &Naming| {
        let mut pane_names: HashMap<PaneId, String> = HashMap::new();
        for (session_name, sess) in st.sessions.iter() {
            for (tab_index, tab) in sess.tabs.iter().enumerate() {
                for pane_id in layout::all_pane_ids(&tab.layout) {
                    let name = match layout::get_pane_custom_name(&tab.layout, pane_id) {
                        Some(Some(custom)) => custom,
                        _ => match ps.get(&pane_id) {
                            Some(pane) => auto_pane_name(pane, session_name, tab_index, naming),
                            None => continue,
                        },
                    };
                    pane_names.insert(pane_id, name);
                }
            }
        }
        pane_names
    };
    let mut names_by_naming: Vec<(Naming, HashMap<PaneId, String>)> = Vec::new();

    // Working directories, for the `files` sidebar plugin to follow. Resolved
    // ONLY for the pane `build_session_tree` will actually attach one to -- the
    // focused pane of each session's ACTIVE tab -- because `get_pane_cwd` is a
    // `/proc` readlink (or a `sysinfo` process refresh on macOS) per call and a
    // whole-map sweep would pay it for every pane on every push.
    let mut pane_cwds: HashMap<PaneId, String> = HashMap::new();
    for sess in st.sessions.values() {
        let Some(tab) = sess.tabs.get(sess.active_tab) else {
            continue;
        };
        if let Some(pane) = ps.get(&tab.focused_pane) {
            if let Some(cwd) = crate::server::persistence::get_pane_cwd(pane.pty.child_pid) {
                pane_cwds.insert(tab.focused_pane, cwd);
            }
        }
    }

    for &target in targets {
        let Some(client) = cls.get(&target) else {
            continue;
        };
        // `is_current` is per-recipient, so the tree is built per target from
        // the one shared snapshot above.
        let naming = client.naming();
        let at = match names_by_naming.iter().position(|(n, _)| *n == naming) {
            Some(at) => at,
            None => {
                let names = resolve_names(&naming);
                names_by_naming.push((naming, names));
                names_by_naming.len() - 1
            }
        };
        let (folders, unfiled) = st.build_session_tree(
            client.session_name.as_deref(),
            &client_counts,
            &names_by_naming[at].1,
            &pane_cwds,
        );
        let _ = client.tx.send(ServerMessage::SessionTree {
            folders,
            unfiled,
            dormant: dormant_names.clone(),
        });
    }
}

/// Push the current tree to every `SubscribeSessionTree` subscriber.
///
/// Subscribers are read here, at fire time, rather than captured when the
/// change was recorded, so a client that unsubscribed during the coalescing
/// window is simply not in the list. That narrows the race but does not close
/// it: an `UnsubscribeSessionTree` landing between this collect and the send
/// inside [`send_session_tree_to`] still gets one last `SessionTree`. Harmless
/// -- a stale tree is the same payload the client asked for a moment ago -- but
/// a client must tolerate one trailing push rather than treat it as a protocol
/// violation.
async fn broadcast_session_tree(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    dormant: &DormantStore,
) {
    let targets: Vec<u64> = {
        let cls = clients.lock().await;
        cls.iter()
            .filter(|(_, c)| c.session_tree_subscribed)
            .map(|(&id, _)| id)
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    send_session_tree_to(&targets, state, panes, clients, dormant).await;
}

// ---------------------------------------------------------------------------
// Agent push
// ---------------------------------------------------------------------------

/// Minimum spacing between two `AgentList` broadcasts, the same coalescing
/// bound the session tree uses.
const AGENTS_PUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// How often the pusher re-samples while something is `Working`, so the
/// `Working` -> `Idle` decay is actually delivered.
///
/// Needed for that ONE transition and no other: every other change is caused by
/// output, and output calls [`mark_agents_dirty`]. So the tick is armed only
/// while the last list contained a `Working` entry -- an agent sitting on
/// `NeedsInput` overnight, the case this whole panel exists for, costs zero
/// wakeups.
///
/// A server with NO subscriber costs nearly nothing, which is not the same as
/// nothing and should not be claimed as such: `mark_agents_dirty` is called from
/// the PTY forwarding loops unconditionally, so any pane producing output still
/// wakes this task at up to ten times a second. Each of those wakeups finds an
/// empty subscriber set under one `clients` lock and parks again -- deliberately
/// cheap, deliberately not free. Gating the mark on "is anyone subscribed?"
/// would mean taking that lock in the output path instead, which is the more
/// expensive half of the trade.
const AGENTS_DECAY_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// Wakes the agent push task. See [`SESSION_TREE_DIRTY`] for why this shape.
///
/// A SEPARATE signal from the session tree's, deliberately. The mechanism is
/// shared; the signal must not be. Agent state is dirtied by pane OUTPUT, and
/// folding that into `SESSION_TREE_DIRTY` would drive `send_session_tree_to`'s
/// per-pane `/proc` sweep -- taken under `state` + `clients` + `panes` -- from
/// every keystroke echo in every pane.
static AGENTS_DIRTY: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Record that agent state may have changed. Cheap, non-blocking, and safe to
/// call while holding any server lock.
fn mark_agents_dirty() {
    AGENTS_DIRTY.notify_one();
}

/// Push a pane's new name to the session tree and the agents list. The caller
/// re-renders and re-streams the pane itself.
fn mark_pane_retitled() {
    mark_session_tree_dirty();
    mark_agents_dirty();
}

/// Record that which pane some client has in front of it may have changed: the
/// tree's `is_current`/`is_active`/`is_focused`, and [`AgentEntry::focused`].
///
/// The agent list has to be re-pushed for this even though no agent changed.
/// A client holds a `finished` or `exited` notification for a settle window and
/// decides at the end of it whether the pane is focused, from the last list it
/// was sent, so a list that predates the focus change would decide wrongly.
fn mark_focus_changed() {
    mark_session_tree_dirty();
    mark_agents_dirty();
}

/// Whether a foreground transition changed who names the pane, logging it
/// when the source of the name flips back to the shell. The flip to a job is
/// logged by [`PaneData::settle_title`], once the job has held the foreground
/// long enough to name the pane.
fn note_transition(pane_id: PaneId, t: crate::server::title::Transition) -> bool {
    use crate::server::title::Transition;
    match t {
        Transition::None => false,
        Transition::Job(_) => true,
        Transition::Shell { named } => {
            if named {
                log::debug!("server: pane_id={pane_id} is named by its shell again");
            }
            true
        }
    }
}

/// Samples a quiet pane's foreground group from the forwarding loop, through
/// the loop's own duplicate of the master, and without the `panes` lock.
struct ForegroundWatch {
    fd: Option<std::os::fd::OwnedFd>,
    ticks: u32,
}

impl ForegroundWatch {
    /// Every tenth idle poll, so a quiet pane costs one `tcgetpgrp` every
    /// 100ms.
    const EVERY: u32 = 10;

    async fn new(pane_id: PaneId, panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>) -> Self {
        let ps = panes.lock().await;
        let fd = ps
            .get(&pane_id)
            .and_then(|pd| pd.pty.master_fd.try_clone().ok());
        Self { fd, ticks: 0 }
    }

    fn sample(&mut self) -> Option<(Option<i32>, std::time::Instant)> {
        self.ticks = self.ticks.wrapping_add(1);
        if !self.ticks.is_multiple_of(Self::EVERY) {
            return None;
        }
        let fd = self.fd.as_ref()?;
        Some(pty::foreground_of(std::os::fd::AsFd::as_fd(fd)))
    }
}

/// The name of process group `pgid`, whose shell is `shell`, or `None` when
/// the group has no live member. The group leader can exit before the rest of
/// its group (`sleep 0.1 | sleep 5`), so a live member is named when the leader
/// is gone.
fn group_name(pgid: i32, shell: i32) -> Option<String> {
    group_member_name(pgid, shell, process_pgid, child_pids, try_process_name)
}

/// [`group_name`] with its process reads passed in. A member whose name
/// cannot be read was reaped after its group was read, and counts as gone.
fn group_member_name(
    pgid: i32,
    shell: i32,
    pgid_of: impl Fn(i32) -> Option<i32>,
    children: impl Fn(i32) -> Vec<i32>,
    name_of: impl Fn(i32) -> Option<String>,
) -> Option<String> {
    let leader = (pgid_of(pgid) == Some(pgid))
        .then(|| name_of(pgid))
        .flatten();
    leader.or_else(|| {
        children(shell)
            .into_iter()
            .filter(|&child| pgid_of(child) == Some(pgid))
            .find_map(&name_of)
    })
}

/// What [`refresh_pane_names`] reads for, taken under the `panes` lock by
/// [`PaneData::names_to_refresh`].
struct NameRefresh {
    child: nix::unistd::Pid,
    job: Option<i32>,
}

/// Re-read a pane's process names and working directory into
/// [`PaneData::names`], with no lock held while reading. Returns whether a
/// cached value changed.
async fn refresh_pane_names(
    pane_id: PaneId,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    what: NameRefresh,
) -> bool {
    let now = std::time::Instant::now();
    let child = what.child;
    let cwd = persistence::get_pane_cwd(child)
        .map(|c| crate::config::pane_title::basename(&c).to_string());
    let shell = get_process_name(child.as_raw());
    let found = what.job.map(|g| (g, group_name(g, child.as_raw())));
    let mut ps = panes.lock().await;
    let Some(pd) = ps.get_mut(&pane_id) else {
        return false;
    };
    let job = found.map(|(g, name)| pd.names.job_after_read(g, name, &shell));
    let changed = pd.names.cwd != cwd || pd.names.shell != shell || pd.names.job != job;
    pd.names.cwd = cwd;
    pd.names.read_at = Some(now);
    pd.names.shell = shell;
    pd.names.job = job;
    changed
}

/// The server's `[appearance] pane_title`, read once at startup like
/// [`AGENT_RULES`].
static PANE_TITLE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Every mailbox and agent name on this server, set up once from `[messaging]`
/// in [`RemuxServer::new`]. A static rather than a field because pane death
/// reaches [`reap_panes`] through seven call chains that carry only `panes`
/// and `clients`.
///
/// Taken ALONE: never while `state`, `panes` or `clients` is held, and
/// nothing else is taken while it is held, so it adds no lock ordering.
static MESSAGING: std::sync::OnceLock<Mutex<Messaging>> = std::sync::OnceLock::new();

/// The server's links to other servers. Taken alone, like the store.
static LINKS: std::sync::OnceLock<Mutex<crate::server::links::Links>> = std::sync::OnceLock::new();

pub(crate) fn links() -> &'static Mutex<crate::server::links::Links> {
    LINKS.get_or_init(|| Mutex::new(crate::server::links::Links::new()))
}

/// The debug-only test seam for [`identity::peer_link_check`], read once from
/// this server's own environment at startup: a pane cannot set it on a
/// running server, and a release build ignores it.
static PEER_SEAM: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();

fn peer_seam() -> Option<i32> {
    PEER_SEAM.get().copied().flatten()
}

fn messaging() -> &'static Mutex<Messaging> {
    MESSAGING.get_or_init(|| {
        Mutex::new(Messaging::new(
            crate::config::messaging::MessagingConfig::default().limits(),
        ))
    })
}

/// Replace the empty store with what `mailbox.json` holds, pruned to panes
/// that exist and to mail that has not expired, and never issue a pane id the
/// file refers to: a surviving message's sender must not become a new pane.
/// `state.json` carries the pane counter too, but it is not rewritten when a
/// pane is merely created, so after a crash it can be older than this file.
///
/// A pane "exists" if it is live or in a DORMANT session
/// (`automatic_restore = false`): a dormant session is resurrected with its
/// own pane ids, and pruning its mail would lose it for good at the next save.
async fn load_mailbox(server: &RemuxServer) {
    let path = crate::server::persistence::mailbox_path();
    let ttl_ms = server.config.messaging.ttl_ms();
    let Some(file) = tokio::task::spawn_blocking(move || {
        crate::server::persistence::remove_stale_bad_mailbox(&path, ttl_ms);
        crate::server::persistence::load_mailbox_from(&path)
    })
    .await
    .ok()
    .flatten() else {
        return;
    };
    let mut known: std::collections::HashSet<PaneId> =
        server.panes.lock().await.keys().copied().collect();
    {
        let dormant = server.dormant.lock().await;
        known.extend(
            dormant
                .state
                .sessions
                .values()
                .flat_map(|sess| sess.tabs.iter())
                .flat_map(|tab| layout::all_pane_ids(&tab.layout)),
        );
    }
    let high_water = file.pane_high_water;
    let closed = messaging().lock().await.closed_panes();
    let (restored, pruned) = Messaging::restore(
        server.config.messaging.limits(),
        file,
        |pane| known.contains(&pane),
        closed,
        now_ms(),
    );
    log::info!(
        "mailbox: restored {} message(s), {} name(s); pruned {} for closed panes, {} expired, {} invalid, {} over a cap, {} name(s); repaired {} session label(s); dropped {} reserved sender name(s); pane ids reserved through %{high_water}",
        restored.message_count(),
        restored.name_count(),
        pruned.dead_pane,
        pruned.expired,
        pruned.invalid,
        pruned.over_cap,
        pruned.names,
        pruned.relabelled,
        pruned.unnamed
    );
    *messaging().lock().await = restored;
    SAVED_HIGH_WATER.store(high_water, Ordering::Relaxed);
    server
        .state
        .lock()
        .await
        .reserve_pane_ids_through(high_water);
}

/// How long the saver waits after a change before writing, so a burst of
/// sends is one write.
const MAILBOX_SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);

/// Woken whenever a pane is created, so that the file's pane high water
/// follows the pane counter even when no mail moves.
static PANE_CREATED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// The high water last written, so a pane creation writes only when it moved.
static SAVED_HIGH_WATER: AtomicU64 = AtomicU64::new(0);

/// Held from the snapshot through the rename, so writes cannot overtake one
/// another: without it a slow older write could land after a newer one, and
/// the shutdown write could race a debounced write still in flight.
static MAILBOX_SAVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Write `mailbox.json` after each change, debounced. Logs a write at debug
/// only: on a busy pane this runs about once a second.
fn spawn_mailbox_saver(state: Arc<Mutex<ServerState>>) {
    tokio::spawn(async move {
        loop {
            let changes = messaging().lock().await.changes();
            tokio::select! {
                _ = changes.notified() => {}
                _ = PANE_CREATED.notified() => {}
            }
            tokio::time::sleep(MAILBOX_SAVE_DEBOUNCE).await;
            save_mailbox_now(&state).await;
        }
    });
}

/// Sweep expired mail, then write the store if anything changed since the
/// last write, or if the pane counter has passed the high water written.
///
/// The high water is the larger of every pane id the store refers to and the
/// last id the pane counter has issued, read under `state` alone before the
/// store is locked, so a crash can reissue no id this server has handed out.
async fn save_mailbox_now(state: &Arc<Mutex<ServerState>>) {
    save_mailbox_with(
        state,
        messaging(),
        crate::server::persistence::mailbox_path(),
    )
    .await;
}

async fn save_mailbox_with(
    state: &Arc<Mutex<ServerState>>,
    store: &Mutex<Messaging>,
    path: PathBuf,
) {
    let _serial = MAILBOX_SAVE.lock().await;
    let issued = state.lock().await.peek_next_pane_id().saturating_sub(1);
    let (file, swept) = {
        let mut store = store.lock().await;
        store.sweep(now_ms());
        let swept = store.take_swept();
        if swept > 0 {
            mark_agents_dirty();
        }
        let changed = store.take_dirty();
        if !changed && issued <= SAVED_HIGH_WATER.load(Ordering::Relaxed) {
            return;
        }
        let mut file = store.snapshot();
        file.pane_high_water = file.pane_high_water.max(issued);
        (file, swept)
    };
    if swept > 0 {
        log::info!("msg: swept {swept} expired message(s)");
    }
    let count = file.messages.len();
    let high_water = file.pane_high_water;
    let written = tokio::task::spawn_blocking(move || {
        crate::server::persistence::save_mailbox_to(&path, &file)
    })
    .await;
    match written {
        Ok(Ok(())) => {
            SAVED_HIGH_WATER.store(high_water, Ordering::Relaxed);
            log::debug!("mailbox: saved {count} message(s), pane ids through %{high_water}");
        }
        Ok(Err(e)) => log::warn!("mailbox: save failed: {e}"),
        Err(e) => log::warn!("mailbox: save task failed: {e}"),
    }
}

/// Unix milliseconds, the clock mail expiry is measured on.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn pane_title_template() -> Option<&'static str> {
    PANE_TITLE.get().and_then(|t| t.as_deref())
}

/// A `pane_title` template and what `{host}` renders as: how the surfaces the
/// server renders for one client name its panes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Naming {
    template: Option<String>,
    host: String,
}

impl Naming {
    /// A client's naming. `host` is kept only when the template uses it, so
    /// that clients naming panes the same way compare equal and share one
    /// composited frame whatever they call this server.
    fn new(template: Option<String>, host: String) -> Self {
        let uses_host = template.as_deref().is_some_and(|t| t.contains("{host}"));
        Self {
            template,
            host: if uses_host { host } else { String::new() },
        }
    }

    /// This server's own template, for a client that sent none. `{host}` is
    /// empty, since this server is the pane's own.
    fn server() -> Self {
        Self::new(pane_title_template().map(str::to_string), String::new())
    }
}

/// The name a pane that the user has not renamed is shown by on the surfaces
/// the server renders: its border, the Monocle strip and the session tree.
/// Reads only [`PaneData`]'s cached state.
fn auto_pane_name(pd: &PaneData, session: &str, tab: usize, naming: &Naming) -> String {
    let (title, command) = pd.name_parts(std::time::Instant::now());
    crate::config::pane_title::display_name(
        naming.template.as_deref(),
        &crate::config::pane_title::NameParts {
            title,
            command,
            session,
            tab,
            cwd: pd.names.cwd.as_deref(),
            host: &naming.host,
        },
    )
}

/// The raw parts a client renders a View cell's label from.
fn pane_name_info(pd: &PaneData, custom: Option<String>, tab_index: usize) -> PaneNameInfo {
    let (title, command) = pd.name_parts(std::time::Instant::now());
    PaneNameInfo {
        custom,
        title: title.map(str::to_string),
        command: command.to_string(),
        tab_index,
        cwd: pd.names.cwd.clone(),
    }
}

/// The compiled agent rules, built once from the config the server started with.
///
/// A `OnceLock` rather than a field threaded through the call graph: the server
/// reads its config at startup and the rules are immutable for its life, so the
/// alternative is an `Arc` clone at fifty call sites that have no other use for
/// it. An edit to `[agents]` therefore needs `remux restart`, exactly like every
/// other server-side setting.
static AGENT_RULES: std::sync::OnceLock<crate::server::agents::AgentRules> =
    std::sync::OnceLock::new();

fn agent_rules(config: &Config) -> &'static crate::server::agents::AgentRules {
    AGENT_RULES.get_or_init(|| crate::server::agents::AgentRules::from_config(&config.agents))
}

/// The classifier's verdict on `pd` running `command`, as of `now`. The one
/// place a pane's screen is classified, for the agents list and, through
/// [`submit_state`], for submit.
fn pane_verdict(
    pd: &PaneData,
    command: &str,
    rules: &crate::server::agents::AgentRules,
    now: std::time::Instant,
) -> crate::server::agents::Verdict {
    let bottom = rules.visible_bottom(&pd.screen);
    rules.classify(
        command,
        &bottom,
        now.saturating_duration_since(pd.last_output),
    )
}

/// The state a submit takes `pd`'s agent to be in: the classifier's, but
/// `NeedsInput` also when the screen looks like a selector no configured
/// pattern names ([`crate::server::submit::looks_like_prompt`]).
fn submit_state(
    pd: &PaneData,
    command: &str,
    rules: &crate::server::agents::AgentRules,
    now: std::time::Instant,
) -> AgentState {
    let bottom = rules.visible_bottom(&pd.screen);
    if crate::server::submit::looks_like_prompt(&bottom) {
        return AgentState::NeedsInput;
    }
    rules
        .classify(
            command,
            &bottom,
            now.saturating_duration_since(pd.last_output),
        )
        .state
}

/// One pane, as a submit needs it.
struct PaneSample {
    /// In the table, not exited, in a tab and not a popup.
    live: bool,
    /// The foreground's configured command, or `None` when it is no agent.
    command: Option<String>,
    /// The agent's state as a submit takes it ([`submit_state`]), when
    /// `command` is an agent.
    state: Option<AgentState>,
    /// Whether the program in the pane has bracketed paste on, as of now.
    bracketed_paste: bool,
    /// The terminal's foreground process group, read before `command`, so a
    /// later write can check the program it was meant for is still there.
    pgid: Option<nix::unistd::Pid>,
    /// The process whose name matched the agent, which may be a descendant
    /// of the group's leader.
    agent: Option<nix::unistd::Pid>,
    /// When a human last typed into the pane, for the typing guard.
    last_human_input: Option<std::time::Instant>,
    /// When output last reached the pane, for `--wait`. `None` when the
    /// pane is not live.
    last_output: Option<std::time::Instant>,
}

/// One pane, sampled the way [`collect_agents`] samples every pane: the
/// master dup'd under `panes`, the foreground syscalls with no lock held, the
/// tree under `state`, then the screen and clocks under `panes` again. It
/// logs nothing, so a submit re-sampling a pane never logs a classification.
async fn sample_pane(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
) -> PaneSample {
    let gone = PaneSample {
        live: false,
        command: None,
        state: None,
        bracketed_paste: false,
        pgid: None,
        agent: None,
        last_human_input: None,
        last_output: None,
    };
    let rules = agent_rules(config);
    let master = {
        let ps = panes.lock().await;
        ps.get(&pane_id)
            .filter(|pd| !pd.exited)
            .and_then(|pd| pd.pty.master_fd.try_clone().ok())
    };
    let Some(master) = master else {
        return gone;
    };
    let pgid = nix::unistd::tcgetpgrp(std::os::fd::AsFd::as_fd(&master)).ok();
    let found = if rules.is_empty() {
        None
    } else {
        crate::server::agents::foreground_agent(std::os::fd::AsFd::as_fd(&master), rules)
    };
    drop(master);
    let (command, agent) = match found {
        Some((command, pid)) => (Some(command), Some(nix::unistd::Pid::from_raw(pid))),
        None => (None, None),
    };
    let placed = {
        let st = state.lock().await;
        st.locate_pane(pane_id).is_some() && !st.is_popup(pane_id)
    };
    if !placed {
        return gone;
    }
    let ps = panes.lock().await;
    let Some(pd) = ps.get(&pane_id).filter(|pd| !pd.exited) else {
        return gone;
    };
    let now = std::time::Instant::now();
    PaneSample {
        live: true,
        state: command
            .as_deref()
            .map(|command| submit_state(pd, command, rules, now)),
        command,
        bracketed_paste: pd.screen.bracketed_paste,
        pgid,
        agent,
        last_human_input: pd.last_human_input,
        last_output: Some(pd.last_output),
    }
}

/// Every pane running a configured agent command, with its state.
///
/// Sampled in three phases so no `/proc` read or `tcgetpgrp` syscall ever
/// happens under the locks -- `send_session_tree_to` already pays that cost per
/// live pane while holding three of them, and this runs up to ten times a
/// second:
///
/// 1. Under `panes` alone, DUP each PTY master (`try_clone`). Snapshotting raw
///    fd NUMBERS instead would be the Phase C fd-recycling bug in a new place:
///    a pane closing between the snapshot and the read hands its number to the
///    next PTY, and the sample would name the wrong pane's process. Owning the
///    dup is what makes the number meaningful for as long as we hold it.
/// 2. With no locks held, ask each dup for its foreground process group and
///    that group's `comm`. A dead or exiting PTY errors, which reads as "no
///    agent" -- never an unwrap.
/// 3. Only for the panes that matched, take `state` then `panes` (the
///    codebase's order) to resolve session/tab and classify the screen.
async fn collect_agents(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) -> Vec<AgentEntry> {
    let rules = agent_rules(config);
    if rules.is_empty() {
        return Vec::new();
    }

    // Taken and released before `state`: the codebase never nests `clients`
    // inside `state`, and a sweep that did could deadlock against a handler
    // that holds `clients` while it waits for `state`.
    let attached: Vec<String> = {
        let cls = clients.lock().await;
        cls.values()
            .filter_map(|c| c.session_name.clone())
            .collect()
    };

    // Phase 1: dup the masters, briefly.
    let masters: Vec<(PaneId, std::os::fd::OwnedFd)> = {
        let ps = panes.lock().await;
        ps.iter()
            .filter_map(|(&id, pd)| pd.pty.master_fd.try_clone().ok().map(|fd| (id, fd)))
            .collect()
    };

    // Phase 2: no locks held.
    let matched: Vec<(PaneId, String)> = masters
        .iter()
        .filter_map(|(id, fd)| {
            let comm =
                crate::server::agents::foreground_command(std::os::fd::AsFd::as_fd(fd), rules)?;
            Some((*id, comm))
        })
        .collect();
    drop(masters);
    if matched.is_empty() {
        return Vec::new();
    }

    // Phase 3: place each matched pane in the tree, then classify it.
    let mut located: HashMap<PaneId, (String, usize, Option<String>)> = HashMap::new();
    let focused = {
        let st = state.lock().await;
        for (name, sess) in &st.sessions {
            for (tab_index, tab) in sess.tabs.iter().enumerate() {
                for pane_id in layout::all_pane_ids(&tab.layout) {
                    if matched.iter().any(|(id, _)| *id == pane_id) {
                        let custom = layout::get_pane_custom_name(&tab.layout, pane_id).flatten();
                        located.insert(pane_id, (name.clone(), tab_index, custom));
                    }
                }
            }
        }
        focused_panes(&st, &attached)
    };

    let mut entries: Vec<AgentEntry> = Vec::new();
    {
        let now = std::time::Instant::now();
        let mut ps = panes.lock().await;
        for (pane_id, command) in matched {
            // A pane in no layout tree is an AUX pane -- a sidebar's own file
            // manager. It belongs to a panel, not to the session tree, and has
            // no session or tab to jump to, so it is not listed.
            let Some((session, tab_index, custom_name)) = located.get(&pane_id).cloned() else {
                continue;
            };
            let Some(pd) = ps.get_mut(&pane_id) else {
                continue;
            };
            let verdict = pane_verdict(pd, &command, rules, now);
            // The explain path: say WHY, but only on a TRANSITION -- keyed on
            // the state and the sample-independent reason, never on `why`.
            let key = (verdict.state, verdict.reason.clone());
            if pd.agent_verdict.as_ref() != Some(&key) {
                log::debug!(
                    "agents: pane_id={pane_id} {command} -> {:?} ({})",
                    verdict.state,
                    verdict.why
                );
                pd.agent_verdict = Some(key);
            }
            entries.push(AgentEntry {
                pane_id,
                session,
                tab_index,
                command,
                state: verdict.state,
                // From the cache the forwarding loop keeps, so that this sample
                // can neither restart a job's settle window nor do a syscall.
                title: pd.name_parts(now).0.map(str::to_string),
                custom_name,
                cwd: pd.names.cwd.clone(),
                name: None,
                unread: 0,
                focused: focused.contains(&pane_id),
            });
        }
    }
    // The store alone, after `panes` is released, as everywhere.
    let ids: Vec<PaneId> = entries.iter().map(|e| e.pane_id).collect();
    let mail = messaging().lock().await.names_and_unread(&ids, now_ms());
    for entry in &mut entries {
        if let Some((name, unread)) = mail.get(&entry.pane_id) {
            entry.name = name.clone();
            entry.unread = *unread;
        }
    }
    // A stable order, so a refresh never reshuffles the panel under the user's
    // selection. The panel preserves the selection by identity regardless, but
    // a list that jumped around between pushes would be unreadable.
    entries.sort_by(|a, b| {
        (&a.session, a.tab_index, a.pane_id).cmp(&(&b.session, b.tab_index, b.pane_id))
    });
    entries
}

/// The panes some attached client has in front of it: the input target of each
/// session in `attached`, which is popup-aware through [`Session::input_target`].
///
/// A session's focus is shared by every client attached to it, so one entry per
/// session name is enough, and a name that no longer resolves marks nothing.
fn focused_panes(st: &ServerState, attached: &[String]) -> std::collections::HashSet<PaneId> {
    attached
        .iter()
        .filter_map(|name| st.sessions.get(name).and_then(Session::input_target))
        .collect()
}

/// Send the agent list to each client in `targets`. Returns whether any listed
/// agent is `Working` -- what the pusher uses to decide whether to arm its
/// decay tick.
async fn send_agents_to(
    targets: &[u64],
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) -> bool {
    if targets.is_empty() {
        return false;
    }
    let agents = collect_agents(state, panes, clients, config).await;
    let any_working = agents
        .iter()
        .any(|a| a.state == crate::protocol::AgentState::Working);
    let cls = clients.lock().await;
    for &target in targets {
        let Some(client) = cls.get(&target) else {
            continue;
        };
        let _ = client.tx.send(ServerMessage::AgentList {
            agents: agents.clone(),
            detection_supported: crate::server::agents::DETECTION_SUPPORTED,
        });
    }
    any_working
}

/// Push the agent list to every `SubscribeAgents` subscriber.
async fn broadcast_agents(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) -> bool {
    let targets: Vec<u64> = {
        let cls = clients.lock().await;
        cls.iter()
            .filter(|(_, c)| c.agents_subscribed)
            .map(|(&id, _)| id)
            .collect()
    };
    send_agents_to(&targets, state, panes, clients, config).await
}

async fn handle_list_session_tree(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    dormant: &DormantStore,
) -> Result<()> {
    send_session_tree_to(&[client_id], state, panes, clients, dormant).await;
    Ok(())
}

async fn handle_kill_session(
    name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let pane_ids = {
        let mut st = state.lock().await;
        st.delete_session(name)?
    };
    log::debug!(
        "server: KillSession name={name:?} panes_removed={}",
        pane_ids.len()
    );
    reap_panes(&pane_ids, panes, clients).await;
    let mut cls = clients.lock().await;
    for client in cls.values_mut() {
        if client.session_name.as_deref() == Some(name) {
            client.session_name = None;
            let _ = client
                .tx
                .send(ServerMessage::Event(SessionEvent::SessionDeleted(
                    name.to_string(),
                )));
        }
    }
    Ok(())
}

/// Forget a departed client: its connection, and its diff baseline.
///
/// It used to reap the client's **aux panes** here too -- PTYs in no layout
/// tree, reachable from nothing but this connection, so a disconnect that left
/// them behind leaked processes. That was the whole reason this function took
/// `panes`. The old `files` plugin was the only thing that ever asked for one,
/// and it is gone: every pane is now in a session's layout, where a disconnect
/// is not supposed to kill anything.
async fn handle_client_disconnect(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    log::debug!("server: client_id={client_id} disconnected, removing from client map");
    let left = {
        let mut cls = clients.lock().await;
        cls.remove(&client_id).and_then(|c| c.session_name)
    };
    if let Some(name) = links().lock().await.drop_client(client_id) {
        log::info!("peer link {name}: closed (client_id={client_id})");
    }
    let (dropped, (queued, typing, deferred), waits) = {
        let mut store = messaging().lock().await;
        (
            store.drop_waiters_of(client_id),
            store.detach_replies_of(client_id),
            store.drop_submit_waits_of(client_id),
        )
    };
    let state_waits = messaging().lock().await.drop_state_waits_of(client_id);
    if state_waits > 0 {
        log::info!("agent: dropped {state_waits} wait(s) of client_id={client_id} (disconnected)");
    }
    for id in waits {
        log::info!(
            "msg: id={id} wait dropped: client_id={client_id} disconnected before the turn ended"
        );
    }
    if dropped > 0 {
        log::info!("msg: dropped {dropped} waiter(s) of client_id={client_id} (disconnected)");
    }
    if deferred > 0 {
        log::info!(
            "msg: dropped {deferred} deferred submit(s) of client_id={client_id} (disconnected): \
             nobody waits for them now, and they stay in the mailbox"
        );
    }
    if queued + typing > 0 {
        log::info!(
            "msg: detached {} submit reply(ies) of client_id={client_id} (disconnected): {queued} still queued to be typed, {typing} being typed now",
            queued + typing
        );
    }
    // Drop this client's diff baseline so the per-client cache doesn't grow
    // unbounded as clients come and go.
    {
        let mut pf = prev_frames.lock().await;
        pf.remove(&client_id);
    }
    if let Some(left) = left {
        refit_popups_after_leave(&left, state, panes, clients, config, prev_frames).await;
    }
    // Removing the `ClientConnection` above dropped any session-tree
    // subscription with it -- but it also changed the `client_count` every
    // *remaining* subscriber displays, so the survivors need a push. A departed
    // client also stops focusing whatever pane it was attached to.
    mark_focus_changed();
}

async fn handle_request_scrollback(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let session_name = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => c.session_name.clone(),
            None => return Ok(()),
        }
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    // Find the active pane -- popup-aware, so search/copy-mode reads the popup's
    // own scrollback while the popup is the thing on screen.
    let active_pane_id = {
        let st = state.lock().await;
        match st
            .sessions
            .get(&session_name)
            .and_then(Session::input_target)
        {
            Some(p) => p,
            None => return Ok(()),
        }
    };

    // Read scrollback content from the pane's screen.
    let lines: Vec<String> = {
        let ps = panes.lock().await;
        match ps.get(&active_pane_id) {
            Some(pane_data) => {
                let content = pane_data.screen.scrollback_content();
                content.lines().map(|l| l.to_string()).collect()
            }
            None => Vec::new(),
        }
    };

    // Send back to client.
    let cls = clients.lock().await;
    if let Some(client) = cls.get(&client_id) {
        let _ = client.tx.send(ServerMessage::ScrollbackContent { lines });
    }

    Ok(())
}

async fn handle_search_info(
    client_id: u64,
    current: usize,
    total: usize,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    log::debug!("server: SearchInfo client_id={client_id} current={current} total={total}");
    let mut cls = clients.lock().await;
    if let Some(client) = cls.get_mut(&client_id) {
        if total == 0 {
            client.search_info = None;
        } else {
            client.search_info = Some((current, total));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_mode_changed(
    client_id: u64,
    mode: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    log::debug!("server: ModeChanged client_id={client_id} new_mode={mode:?}");
    let session_name = {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            client.mode = mode.to_string();
            // Clear search info when leaving search mode.
            if mode != "SEARCH" {
                client.search_info = None;
            }
            client.session_name.clone()
        } else {
            None
        }
    };

    if let Some(session_name) = session_name {
        broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mouse handling
// ---------------------------------------------------------------------------

/// Build a wheel mouse-report byte sequence for a pane with mouse tracking on.
///
/// Button code is 64 for wheel-up, 65 for wheel-down. `col`/`row` are
/// pane-relative 1-based coordinates. When `sgr` is true, the SGR (1006)
/// encoding is used; otherwise the legacy X10 encoding (offset +32, single
/// byte per field, saturated) is emitted.
fn wheel_report(sgr: bool, up: bool, col: u16, row: u16) -> Vec<u8> {
    let btn: u16 = if up { 64 } else { 65 };
    if sgr {
        format!("\x1b[<{btn};{col};{row}M").into_bytes()
    } else {
        let b = (32u32 + btn as u32).min(255) as u8;
        let c = (32u32 + col as u32).min(255) as u8;
        let r = (32u32 + row as u32).min(255) as u8;
        vec![0x1b, b'[', b'M', b, c, r]
    }
}

/// Build a single arrow-key escape sequence for the alternate-scroll fallback.
///
/// Up = `A`, Down = `B`. With `app_cursor` (DECCKM) the SS3 form `ESC O x` is
/// used, otherwise the CSI form `ESC [ x`.
fn arrow_report(app_cursor: bool, up: bool) -> Vec<u8> {
    let final_byte = if up { b'A' } else { b'B' };
    let mid = if app_cursor { b'O' } else { b'[' };
    vec![0x1b, mid, final_byte]
}

/// The three phases of a left-button gesture, as a mouse-tracking application
/// expects to see them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ButtonPhase {
    /// Button went down.
    Press,
    /// Pointer moved with the button held (only sent to apps that asked for
    /// motion, i.e. modes 1002/1003).
    Motion,
    /// Button came back up.
    Release,
}

/// A mouse button a tracking application can be sent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Button {
    Left,
    Right,
}

impl Button {
    /// The button number of a press in the mouse protocol.
    fn code(self) -> u16 {
        match self {
            Button::Left => 0,
            Button::Right => 2,
        }
    }
}

/// Build a left-button mouse-report byte sequence, the [`wheel_report`] sibling
/// for press/motion/release. `col`/`row` are pane-relative 1-based coordinates.
///
/// SGR (1006) distinguishes press from release by the FINAL BYTE (`M` vs `m`)
/// and so keeps the button number on both; the legacy X10 encoding has no
/// lowercase form and reports a release as button 3 ("no button"). Motion adds
/// the +32 motion bit in both encodings.
fn button_report(sgr: bool, phase: ButtonPhase, col: u16, row: u16) -> Vec<u8> {
    report_for(Button::Left, sgr, phase, col, row)
}

/// [`button_report`] for any [`Button`].
fn report_for(button: Button, sgr: bool, phase: ButtonPhase, col: u16, row: u16) -> Vec<u8> {
    // Motion sets bit 5; X10 spells a release as button 3.
    let btn: u16 = match phase {
        ButtonPhase::Press => button.code(),
        ButtonPhase::Motion => button.code() + 32,
        ButtonPhase::Release if sgr => button.code(),
        ButtonPhase::Release => 3,
    };
    if sgr {
        let final_byte = if phase == ButtonPhase::Release {
            'm'
        } else {
            'M'
        };
        format!("\x1b[<{btn};{col};{row}{final_byte}").into_bytes()
    } else {
        let b = (32u32 + btn as u32).min(255) as u8;
        let c = (32u32 + col as u32).min(255) as u8;
        let r = (32u32 + row as u32).min(255) as u8;
        vec![0x1b, b'[', b'M', b, c, r]
    }
}

/// What a right-button press does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RightPress {
    /// The pane's application gets the button-2 report.
    Forward,
    /// The client opens a context menu.
    Menu,
    Nothing,
}

/// Whether `(x, y)` is on the border drawn inside `rect`: in the rect but
/// outside the content, which sits `inset` cells in from each side.
fn on_pane_border(rect: Rect, inset: u16, x: u16, y: u16) -> bool {
    if inset == 0 {
        return false;
    }
    let inside_content = x >= rect.x + inset
        && x < rect.x + rect.width.saturating_sub(inset)
        && y >= rect.y + inset
        && y < rect.y + rect.height.saturating_sub(inset);
    !inside_content
}

/// The decision for a right-button press on a pane.
///
/// The border always opens the menu, because an application that tracks the
/// mouse cannot see the border and so cannot claim a press on it. The popup's
/// interior belongs to the popup's application alone, so the menu never opens
/// there.
fn right_press_outcome(route: MouseRoute, on_border: bool, in_popup: bool) -> RightPress {
    match (route, on_border, in_popup) {
        (MouseRoute::App { .. }, _, true) => RightPress::Forward,
        (_, _, true) => RightPress::Nothing,
        (_, true, false) => RightPress::Menu,
        (MouseRoute::App { .. }, false, false) => RightPress::Forward,
        _ => RightPress::Menu,
    }
}

/// Whether a right-button release goes to the pane's application. A press that
/// opened the menu has no matching press at the application, so its release
/// must not reach it.
fn right_release_outcome(route: MouseRoute, on_border: bool) -> bool {
    matches!(route, MouseRoute::App { .. }) && !on_border
}

/// Which kind of mouse event is being routed. The two differ only in their
/// no-tracking fallback on the alternate screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MouseGesture {
    /// A wheel notch.
    Wheel,
    /// A left-button press, drag or release.
    Button,
}

/// What remux should do with a mouse event that landed on a pane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MouseRoute {
    /// Forward an encoded mouse report to the application. `sgr` selects the
    /// 1006 encoding over legacy X10; `motion` is whether the app asked for
    /// drag/motion reports (1002/1003) rather than presses only (1000).
    App { sgr: bool, motion: bool },
    /// Alternate-scroll fallback: an alt-screen application with no mouse
    /// tracking gets arrow keys for the wheel, since it has no scrollback of
    /// its own for remux to scroll.
    AltArrows { app_cursor: bool },
    /// Handle the event in remux: scrollback scrolling and text selection.
    ///
    /// `scrollback` is false on the alternate screen, where remux's scrollback
    /// holds the PRIMARY screen's history — text that has nothing to do with
    /// what the pane is showing. Dragging or wheeling into it would replace a
    /// full-screen app's display with unrelated history, and (because an
    /// alt-screen app's own output keeps feeding that scrollback) an edge
    /// auto-scroll aimed at it can never reach the end. So the caller must
    /// neither move the offset nor arm the repeat timer when this is false.
    Remux { scrollback: bool },
}

/// **The one mouse-routing decision in the server.** Every mouse path —
/// session-scoped (`MouseScroll`/`MouseClick`/`MouseDrag` with no `pane_id`)
/// and pane-scoped (`ScrollPane` and the `pane_id` forms, i.e. a View cell) —
/// asks this and nothing else, so a view and a session treat the same pane
/// identically.
///
/// Precedence matches what a terminal multiplexer is expected to do:
/// 0. `copy_mode` (remux's Visual mode) is an explicit "the mouse is MINE"
///    request from the user, so it outranks the application — the same reason
///    the client already refuses to forward the wheel in Visual mode;
/// 1. the application asked for mouse events (1000/1002/1003) — it gets them,
///    and remux does no selection or scrolling of its own;
/// 2. otherwise, on the alternate screen there is no meaningful remux
///    scrollback, so a wheel becomes arrow keys and a drag stays put;
/// 3. otherwise it is a plain shell: remux scrolls and selects as always.
fn mouse_route(screen: &Screen, gesture: MouseGesture, copy_mode: bool) -> MouseRoute {
    if copy_mode {
        // Selection still works over a full-screen app; only the scrolling that
        // would chase the primary screen's history is withheld.
        MouseRoute::Remux {
            scrollback: !screen.alt_screen_active,
        }
    } else if screen.mouse_tracking {
        MouseRoute::App {
            sgr: screen.mouse_sgr,
            motion: screen.mouse_motion,
        }
    } else if screen.alt_screen_active {
        match gesture {
            MouseGesture::Wheel => MouseRoute::AltArrows {
                app_cursor: screen.application_cursor_keys,
            },
            MouseGesture::Button => MouseRoute::Remux { scrollback: false },
        }
    } else {
        MouseRoute::Remux { scrollback: true }
    }
}

/// The width of the border a layout pane's content sits inside, on each side:
/// what the compositor drew the pane with.
fn pane_inset(config: &Config, pane_rect: Rect) -> u16 {
    match config.appearance.border_style {
        BorderStyle::ZellijStyle if fits_zellij_border(pane_rect.width, pane_rect.height) => 1,
        _ => 0,
    }
}

/// Map a full-screen coordinate into the 1-based, content-relative coordinates
/// a mouse report carries, applying the `inset` the content sits inside
/// `pane_rect` with. Shared by every session-scoped forwarding site; the
/// pane-scoped ones receive content coordinates already (the client owns a
/// View cell's geometry) and only add the 1-based bias.
fn report_coords(pane_rect: Rect, inset: u16, x: u16, y: u16) -> (u16, u16) {
    let col = x.saturating_sub(pane_rect.x + inset).saturating_add(1);
    let row = y.saturating_sub(pane_rect.y + inset).saturating_add(1);
    (col, row)
}

/// Write bytes to a pane's PTY, if the pane is still alive.
async fn write_to_pane(
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    pane_id: PaneId,
    bytes: &[u8],
) -> Result<()> {
    let ps = panes.lock().await;
    if let Some(pane_data) = ps.get(&pane_id) {
        pane_data.send_input(bytes);
    }
    Ok(())
}

/// [`write_to_pane`] for bytes that stand in for keys a person pressed: the
/// wheel's arrow keys edit an agent's input line exactly as typed arrows do,
/// so they arm the typing guard. A mouse report is not a key and does not.
async fn write_keys_to_pane(
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    pane_id: PaneId,
    bytes: &[u8],
) -> Result<()> {
    if let Some(pane_data) = panes.lock().await.get_mut(&pane_id) {
        pane_data.send_human_input(bytes);
    }
    Ok(())
}

/// Read the routing decision for a pane, or `None` if the pane is gone.
async fn route_of_pane(
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    pane_id: PaneId,
    gesture: MouseGesture,
    copy_mode: bool,
) -> Option<MouseRoute> {
    let ps = panes.lock().await;
    ps.get(&pane_id)
        .map(|p| mouse_route(&p.screen, gesture, copy_mode))
}

/// remux's Visual mode is its copy-mode, and it claims the mouse (see
/// [`mouse_route`]). The client reports its mode with `ModeChanged` whether it
/// is attached or displaying a view, so this reads the same for both paths.
const COPY_MODE: &str = "VISUAL";

/// The scrollback search prompt. Like [`COPY_MODE`] it is an explicit trip into
/// history, so [`snap_client_to_live_tail`] leaves it alone.
const SEARCH_MODE: &str = "SEARCH";

/// Whether this client currently has the mouse to itself.
async fn client_in_copy_mode(
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    client_id: u64,
) -> bool {
    let cls = clients.lock().await;
    cls.get(&client_id).map(|c| c.mode.as_str()) == Some(COPY_MODE)
}

/// Forward one phase of a left-button gesture to a tracking application.
///
/// Motion is dropped for an application that only asked for press/release
/// (mode 1000). Press and release are always sent: an app that gets a press
/// without its release latches the button down.
async fn forward_button(
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    pane_id: PaneId,
    sgr: bool,
    motion: bool,
    phase: ButtonPhase,
    col: u16,
    row: u16,
) -> Result<()> {
    forward_button_as(panes, pane_id, Button::Left, sgr, motion, phase, col, row).await
}

/// [`forward_button`] for any [`Button`].
#[allow(clippy::too_many_arguments)]
async fn forward_button_as(
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    pane_id: PaneId,
    button: Button,
    sgr: bool,
    motion: bool,
    phase: ButtonPhase,
    col: u16,
    row: u16,
) -> Result<()> {
    if phase == ButtonPhase::Motion && !motion {
        return Ok(());
    }
    let bytes = report_for(button, sgr, phase, col, row);
    log::debug!(
        "server: mouse->app pane_id={pane_id} button={button:?} phase={phase:?} sgr={sgr} col={col} row={row}"
    );
    write_to_pane(panes, pane_id, &bytes).await
}

/// Apply a scroll delta to this client's offset into ONE pane, clamp it to the
/// valid range, and send a render if the offset changed. Shared by the
/// `ScrollDelta` message (keyboard/visual scrolling) and the plain-shell
/// fallback of `MouseScroll`.
///
/// `target_pane` is `None` for a keyboard scroll, which has no coordinates and
/// therefore means the pane that owns input (the popup while it is up). The
/// wheel passes the pane its hit test resolved, which is why the wheel over a
/// pane that does not own input scrolls that pane and not the focused one.
#[allow(clippy::too_many_arguments)]
async fn handle_scroll_delta(
    client_id: u64,
    delta: i32,
    target_pane: Option<PaneId>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let session_name = {
        let cls = clients.lock().await;
        cls.get(&client_id).and_then(|c| c.session_name.clone())
    };
    // Resolved once, then reused for the old offset, the clamp, the
    // selection-extend and the entry this writes back.
    let focused_pane_id = match target_pane {
        Some(p) => Some(p),
        None => match &session_name {
            Some(sn) => {
                let st = state.lock().await;
                st.sessions.get(sn).and_then(Session::input_target)
            }
            None => None,
        },
    };
    let old_offset = {
        let cls = clients.lock().await;
        match (cls.get(&client_id), focused_pane_id) {
            (Some(c), Some(fp)) => c.scroll_of(fp),
            _ => 0,
        }
    };
    let new_offset = if delta > 0 {
        old_offset.saturating_add(delta as usize)
    } else {
        old_offset.saturating_sub((-delta) as usize)
    };
    // Clamp to max scrollable range
    let max_offset = if let Some(fp) = focused_pane_id {
        let ps = panes.lock().await;
        // Clamp against the scrolled pane's inner grid height
        // (screen.rows == grid.len()), which is exactly the number of content
        // rows blit_screen draws for the pane. Using the client terminal rows
        // here would over-subtract (it ignores the status bar and pane
        // borders), leaving the earliest scrollback lines unreachable at max
        // scroll.
        ps.get(&fp)
            .map(|p| p.screen.max_scroll_offset())
            .unwrap_or(0)
    } else {
        0
    };
    // Everything `max_scrollable` is made of, so a report of "it stops scrolling
    // too early" can be diagnosed from one log line instead of a round trip.
    // `max_scrollable` is exactly `scrollback.len()` (grid.len() == rows), so a
    // small value is always a small scrollback, never a clamp: `alt` tells you
    // the history is parked and deliberately unreachable, a `region` other than
    // 0..rows-1 tells you the application suppressed accumulation (only a scroll
    // region starting at row 0 feeds scrollback), and `evicted` tells you the
    // limit was hit.
    let detail = if let Some(fp) = focused_pane_id {
        let ps = panes.lock().await;
        ps.get(&fp)
            .map(|p| {
                let s = &p.screen;
                format!(
                    " pane_id={fp} alt={} sb={} evicted={} grid={} rows={} cols={} region={}..{}",
                    s.alt_screen_active,
                    s.scrollback.len(),
                    s.lines_evicted,
                    s.grid.len(),
                    s.rows,
                    s.cols,
                    s.scroll_top,
                    s.scroll_bottom,
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let clamped = new_offset.min(max_offset);
    let changed = clamped != old_offset;
    log::debug!(
        "server: ScrollDelta client_id={client_id} delta={delta} new_offset={clamped} max_scrollable={max_offset}{detail}"
    );
    {
        let mut cls = clients.lock().await;
        if let (Some(client), Some(fp)) = (cls.get_mut(&client_id), focused_pane_id) {
            client.set_scroll(fp, clamped);
            client.needs_full_render = true;
        }
    }
    // If a mouse drag-selection is active on the scrolled pane, extend it to the
    // newly revealed edge so a scroll (mouse wheel or keyboard) grows the
    // selection to cover the scrolled-into text -- mirroring drag-autoscroll. The
    // anchor stays pinned in absolute coords; the moving end follows the scroll,
    // and both the highlight and the yankable range derive from that one range.
    if changed {
        if let Some(fp) = focused_pane_id {
            extend_selection_on_scroll(client_id, fp, delta > 0, clamped, panes, clients).await;
        }
    }
    // Only render if the offset actually changed (skip at boundary).
    if changed {
        if let Some(session_name) = session_name {
            send_full_render_to_client(
                client_id,
                &session_name,
                state,
                panes,
                clients,
                config,
                prev_frames,
                RenderCtx::default(),
            )
            .await;
        }
    }
    Ok(())
}

/// Extend an active mouse drag-selection to the edge revealed by a scroll, so a
/// wheel/keyboard scroll while a selection is live grows it to cover the
/// scrolled-into text (mirroring drag-autoscroll). `up` is the scroll direction
/// (`true` = back into history, revealing the TOP edge). The anchor is left
/// pinned in absolute coords; only the moving end (`DragSession::end_abs`) and
/// the re-projected `MouseSelection` are updated. A no-op unless the client has a
/// drag gesture on `pane_id`. Locks `clients` then `panes` then `clients` again,
/// never nested, matching the surrounding lock discipline.
async fn extend_selection_on_scroll(
    client_id: u64,
    pane_id: PaneId,
    up: bool,
    new_offset: usize,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    // Read the anchor of any active drag on this pane.
    let anchor = {
        let cls = clients.lock().await;
        match cls.get(&client_id).and_then(|c| c.drag.as_ref()) {
            Some(d) if d.pane_id == pane_id => Some((d.anchor_col, d.anchor_abs)),
            _ => None,
        }
    };
    let (anchor_col, anchor_abs) = match anchor {
        Some(a) => a,
        None => return,
    };
    // Project the anchor and the revealed edge into the new viewport using the
    // pane's own geometry (rows/cols == the pane's content area).
    let projected = {
        let ps = panes.lock().await;
        ps.get(&pane_id).map(|pd| {
            let screen = &pd.screen;
            let rows = screen.rows;
            let cols = screen.cols;
            // The moving end pins to the revealed edge: top row when scrolling
            // back, bottom row when scrolling forward.
            let end_row = if up { 0 } else { rows.saturating_sub(1) };
            let end_col = if up { 0 } else { cols.saturating_sub(1) };
            let end_abs = screen.abs_of_row(new_offset, end_row);
            let anchor_row = screen
                .row_of_abs(new_offset, anchor_abs)
                .clamp(0, rows.saturating_sub(1) as i64) as u16;
            (end_row, end_col, end_abs, anchor_row)
        })
    };
    let (end_row, end_col, end_abs, anchor_row) = match projected {
        Some(v) => v,
        None => return,
    };
    // Commit the new end (absolute) and the re-projected viewport selection.
    let mut cls = clients.lock().await;
    if let Some(client) = cls.get_mut(&client_id) {
        if let Some(d) = client.drag.as_mut() {
            if d.pane_id == pane_id {
                d.end_abs = end_abs;
                d.end_col = end_col;
            }
        }
        client.mouse_selection = Some(MouseSelection {
            pane_id,
            start: (anchor_col, anchor_row),
            end: (end_col, end_row),
        });
    }
}

/// Keep a View cell's selection honest across a `ScrollPane` (wheel) step.
///
/// The pane-scoped counterpart of [`extend_selection_on_scroll`], with the extra
/// case that path does not have to handle:
///
/// * **A drag is in flight** -- the wheel EXTENDS the selection to the newly
///   revealed edge, exactly as a session drag does, so the highlight keeps
///   matching what a release would yank. The anchor is absolute, so it stays
///   pinned to its logical line.
/// * **No drag, but a selection is still up** (only possible with
///   `mouse_auto_yank = false`, which leaves the highlight for keyboard
///   adjustment) -- the selection is CLEARED. `MouseSelection` is
///   viewport-relative, so scrolling the content out from under it would leave
///   the grey block sitting on whatever text happened to land in those rows.
async fn rescope_pane_selection_on_scroll(
    client_id: u64,
    pane_id: PaneId,
    up: bool,
    new_offset: usize,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    let anchor = {
        let cls = clients.lock().await;
        match cls.get(&client_id).and_then(|c| c.pane_drag.as_ref()) {
            Some(d) if d.pane_id == pane_id => Some((d.anchor_col, d.anchor_abs)),
            _ => None,
        }
    };
    let (anchor_col, anchor_abs) = match anchor {
        Some(a) => a,
        None => {
            // No gesture to extend: a leftover highlight cannot survive the
            // scroll, so drop it rather than let it point at the wrong text.
            let mut cls = clients.lock().await;
            if let Some(client) = cls.get_mut(&client_id) {
                client.pane_selection.remove(&pane_id);
            }
            return;
        }
    };
    let projected = {
        let ps = panes.lock().await;
        ps.get(&pane_id).map(|pd| {
            let screen = &pd.screen;
            let (rows, cols) = (screen.rows, screen.cols);
            // The moving end pins to the revealed edge: the top row scrolling
            // back into history, the bottom row scrolling forward.
            let end_row = if up { 0 } else { rows.saturating_sub(1) };
            let end_col = if up { 0 } else { cols.saturating_sub(1) };
            let end_abs = screen.abs_of_row(new_offset, end_row);
            let anchor_row = screen
                .row_of_abs(new_offset, anchor_abs)
                .clamp(0, rows.saturating_sub(1) as i64) as u16;
            (end_row, end_col, end_abs, anchor_row)
        })
    };
    let (end_row, end_col, end_abs, anchor_row) = match projected {
        Some(v) => v,
        None => return,
    };
    let mut cls = clients.lock().await;
    if let Some(client) = cls.get_mut(&client_id) {
        if let Some(d) = client.pane_drag.as_mut() {
            if d.pane_id == pane_id {
                d.end_abs = end_abs;
                d.end_col = end_col;
            }
        }
        client.pane_selection.insert(
            pane_id,
            MouseSelection {
                pane_id,
                start: (anchor_col, anchor_row),
                end: (end_col, end_row),
            },
        );
    }
}

/// Route a mouse wheel event. If the pane under the cursor has mouse tracking
/// enabled, forward a wheel report to its application. Otherwise, if it is on
/// the alternate screen, emit the alternate-scroll arrow-key fallback. For a
/// plain shell, fall back to remux's own scrollback scroll.
#[allow(clippy::too_many_arguments)]
async fn handle_mouse_scroll(
    client_id: u64,
    x: u16,
    y: u16,
    up: bool,
    popup_local: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let (session_name, cols, rows, mode, naming, theme, asking) = {
        let cls = clients.lock().await;
        let client = match cls.get(&client_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        (
            client.session_name.clone(),
            client.cols,
            client.rows,
            client.mode.clone(),
            client.naming(),
            client.theme(config),
            client.popup_want.is_some(),
        )
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    // Build composite to get hit regions and pane rects.
    update_auto_pane_names(&session_name, state, panes).await;
    let (_cells, _cx, _cy, _cv, _cs, _fpr, mut hit_regions, pane_rects, _ack, _bp, layer, _status) =
        build_composite(
            &session_name,
            cols,
            rows,
            &mode,
            state,
            panes,
            config,
            None,
            None,
            &HashMap::new(),
            &theme,
            &naming,
        )
        .await;

    // A client that sent interior-local coordinates did so because it held a
    // popup layer; if the popup is gone by now those coordinates mean nothing
    // to the layout, so the event is dropped.
    if popup_local && layer.is_none() {
        return Ok(());
    }
    let popup = with_float_zoom(
        mouse_popup(layer.as_ref(), popup_local, asking, (cols, rows), config),
        &hit_regions,
    );
    if let Some((rect, _)) = popup.hit {
        // Stack/tab labels the popup covers must stop being clickable, or a
        // click inside the popup could activate a hidden pane behind it.
        hit_regions.stack_regions.retain(|r| {
            !(r.y >= rect.y
                && r.y < rect.y + rect.height
                && r.x_end > rect.x
                && r.x_start < rect.x + rect.width)
        });
    }

    // Find the pane under the cursor; fall back to whichever pane owns input
    // (the popup while it is up, else the active tab's focused pane).
    let target = hit_test_with_popup(x, y, &hit_regions, &pane_rects, &popup);
    let target_pane = match target {
        ClickTarget::Pane(id) => id,
        _ => {
            let st = state.lock().await;
            match st
                .sessions
                .get(&session_name)
                .and_then(Session::input_target)
            {
                Some(fp) => fp,
                None => return Ok(()),
            }
        }
    };

    // Find the target pane's rect for coordinate mapping.
    let (pane_rect, inset) = match rect_of_pane(&pane_rects, &popup, config, target_pane) {
        Some(r) => r,
        None => return Ok(()),
    };

    // The shared routing decision (see `mouse_route`).
    let route =
        match route_of_pane(panes, target_pane, MouseGesture::Wheel, mode == COPY_MODE).await {
            Some(r) => r,
            None => return Ok(()),
        };

    match route {
        MouseRoute::App { sgr, .. } => {
            let (col, row) = report_coords(pane_rect, inset, x, y);
            let bytes = wheel_report(sgr, up, col, row);
            log::debug!(
                "server: MouseScroll->app client_id={client_id} pane_id={target_pane} sgr={sgr} up={up} col={col} row={row}"
            );
            write_to_pane(panes, target_pane, &bytes).await
        }
        MouseRoute::AltArrows { app_cursor } => {
            // Alternate-scroll fallback: three arrow keys per wheel notch.
            let bytes = alt_scroll_arrows(app_cursor, up, WHEEL_LINES);
            log::debug!(
                "server: MouseScroll->alt-arrows client_id={client_id} pane_id={target_pane} app_cursor={app_cursor} up={up}"
            );
            write_keys_to_pane(panes, target_pane, &bytes).await
        }
        MouseRoute::Remux { .. } => {
            // Plain shell: preserve remux's own scrollback scroll (delta ±3).
            let delta = if up {
                WHEEL_LINES as i32
            } else {
                -(WHEEL_LINES as i32)
            };
            log::debug!(
                "server: MouseScroll->remux-scroll client_id={client_id} pane_id={target_pane} delta={delta}"
            );
            handle_scroll_delta(
                client_id,
                delta,
                Some(target_pane),
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await
        }
    }
}

/// Lines a single wheel notch moves — the scrollback delta remux applies itself,
/// and the number of arrow keys the alternate-scroll fallback sends.
const WHEEL_LINES: u16 = 3;

/// `lines` repetitions of the alternate-scroll arrow key.
fn alt_scroll_arrows(app_cursor: bool, up: bool, lines: u16) -> Vec<u8> {
    let one = arrow_report(app_cursor, up);
    let mut bytes = Vec::with_capacity(one.len() * lines as usize);
    for _ in 0..lines {
        bytes.extend_from_slice(&one);
    }
    bytes
}

/// How the active tab's popup takes part in one mouse event.
struct MousePopup {
    /// The popup pane while the popup is visible. The "popup owns input" guards
    /// key on this, whether or not the event can hit the popup.
    pane: Option<PaneId>,
    /// The popup's rect in the event's coordinate space and the inset of its
    /// content inside that rect. `None` for a client that places the popup
    /// itself but sent screen coordinates: it has no rect the server could
    /// hit-test against.
    hit: Option<(Rect, u16)>,
    /// The event's coordinates are interior-local, so the popup is its target
    /// with no hit test.
    local: bool,
}

/// Resolve [`MousePopup`] for a client from the layer `build_composite`
/// returned.
///
/// * `local` (the client sent `popup: true`): the rect is the layer's own
///   `(0, 0, cols, rows)` with no inset.
/// * A client that places the popup itself, with screen coordinates: no rect.
/// * A client that does not: the rect of the float the server drew for it, with
///   the inset [`pane_inset`] gives a layout pane of that rect.
fn mouse_popup(
    layer: Option<&PopupLayer>,
    local: bool,
    asking: bool,
    (cols, rows): (u16, u16),
    config: &Config,
) -> MousePopup {
    let Some(layer) = layer else {
        return MousePopup {
            pane: None,
            hit: None,
            local: false,
        };
    };
    let hit = if local {
        Some((
            Rect {
                x: 0,
                y: 0,
                width: layer.cols,
                height: layer.rows,
            },
            0,
        ))
    } else if asking {
        None
    } else {
        let area = Rect {
            x: 0,
            y: 0,
            width: cols,
            height: rows.saturating_sub(1),
        };
        let rect = layout::popup_rect(area, legacy_popup_pct(config));
        Some((rect, pane_inset(config, rect)))
    };
    MousePopup {
        pane: Some(layer.pane_id),
        hit,
        local,
    }
}

/// Let a float zoom take part in a mouse event as the popup does.
///
/// The float is a box over the layout with a pane in it, which is what a popup
/// is to every mouse rule: a hit inside it is the floated pane's, a press
/// outside it dismisses it, and the box covers whatever is under it. The popup
/// and a float zoom are never both up, so a float only fills in a `MousePopup`
/// that has no popup. The inset is the one [`draw_popup`] frames the box with.
fn with_float_zoom(popup: MousePopup, regions: &HitRegions) -> MousePopup {
    match (popup.pane, regions.float) {
        (None, Some((pane, rect))) => MousePopup {
            pane: Some(pane),
            hit: Some((rect, u16::from(fits_zellij_border(rect.width, rect.height)))),
            local: false,
        },
        _ => popup,
    }
}

/// Popup-aware hit test: **the mouse chokepoint.**
///
/// The popup floats above everything, so any coordinate inside its rect resolves
/// to the popup pane and is checked BEFORE the layout's own regions. Doing the
/// popup check here (rather than splicing the popup into `pane_rects`) is what
/// lets `pane_rects` keep meaning "the layout's rects, popup-independent".
fn hit_test_with_popup(
    x: u16,
    y: u16,
    regions: &HitRegions,
    pane_rects: &[(PaneId, Rect)],
    popup: &MousePopup,
) -> ClickTarget {
    if let Some(pane_id) = popup.pane {
        if popup.local {
            return ClickTarget::Pane(pane_id);
        }
        if let Some((r, _)) = popup.hit {
            if x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height {
                return ClickTarget::Pane(pane_id);
            }
        }
    }
    hit_test(x, y, regions, pane_rects)
}

/// The screen rect of `pane_id` for coordinate mapping and the inset of its
/// content, popup included. The popup is never in `pane_rects`, so it must be
/// resolved separately. A client that places the popup itself has no popup rect
/// for a screen-coordinate event, so the popup resolves to `None` there.
fn rect_of_pane(
    pane_rects: &[(PaneId, Rect)],
    popup: &MousePopup,
    config: &Config,
    pane_id: PaneId,
) -> Option<(Rect, u16)> {
    if popup.pane == Some(pane_id) {
        return popup.hit;
    }
    pane_rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .map(|(_, r)| (*r, pane_inset(config, *r)))
}

/// Disarm the drag-autoscroll repeat timer for a client. Called from the early
/// returns in [`handle_mouse_drag`] that can be reached while a drag is armed
/// (the target pane's process exited, the drag no longer hits a pane, etc.), so
/// the ticker task stops re-invoking a drag that can never make progress instead
/// of spinning at the tick rate.
async fn disarm_autoscroll(clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>, client_id: u64) {
    let mut cls = clients.lock().await;
    if let Some(c) = cls.get_mut(&client_id) {
        c.autoscroll_repeat = None;
    }
}

/// Hide the active tab's popup, if it is visible, and bring every client in
/// line: the layout sizes come back, and no diff baseline keeps popup cells.
/// Returns whether a popup was hidden.
///
/// `render` sends the clients the frame without the popup. A caller that
/// broadcasts a frame itself right after passes `false`, so the clients get one
/// frame and not two.
///
/// For an action that dismisses the popup and then acts as if it had not been
/// there. `PopupToggle` cannot serve, because it flips: run after another
/// client's close, it would open the popup again.
async fn hide_visible_popup(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    render: bool,
) -> Result<bool> {
    let hidden = {
        let mut st = state.lock().await;
        let tab = st
            .sessions
            .get_mut(session_name)
            .and_then(|s| s.tabs.get_mut(s.active_tab));
        match tab {
            Some(tab) if tab.popup_visible => {
                tab.popup_visible = false;
                true
            }
            _ => false,
        }
    };
    if hidden {
        log::debug!("server: popup of session={session_name:?} dismissed");
        resize_session_panes(session_name, state, panes, clients, config).await?;
        invalidate_session_baselines(session_name, clients, prev_frames).await;
        if render {
            broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
        }
    }
    Ok(hidden)
}

/// Snap the active tab's float zoom back into its slot, if it has one, and
/// bring every client in line: the floated pane gets its slot's size again, and
/// the frame no longer holds the box. Returns whether a float was released.
///
/// `render` is as for [`hide_visible_popup`].
async fn release_float_zoom(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    render: bool,
) -> Result<bool> {
    let released = {
        let mut st = state.lock().await;
        st.sessions
            .get_mut(session_name)
            .and_then(|s| s.tabs.get_mut(s.active_tab))
            .is_some_and(session::Tab::release_float_zoom)
    };
    if released {
        log::debug!("server: float zoom of session={session_name:?} dismissed");
        resize_session_panes(session_name, state, panes, clients, config).await?;
        if render {
            broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
        }
    }
    Ok(released)
}

/// Close whichever box is over the active tab's layout: the popup, or a float
/// zoom. At most one is up. Returns whether one was closed.
async fn dismiss_overlay(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    render: bool,
) -> Result<bool> {
    if hide_visible_popup(
        session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        render,
    )
    .await?
    {
        return Ok(true);
    }
    release_float_zoom(
        session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        render,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn handle_mouse_click(
    client_id: u64,
    x: u16,
    y: u16,
    release: bool,
    popup_local: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    handle_mouse_button(
        Button::Left,
        client_id,
        x,
        y,
        release,
        popup_local,
        state,
        panes,
        clients,
        config,
        prev_frames,
    )
    .await
}

/// A right-button press or release: routed to the pane's application, or
/// answered with a [`ServerMessage::ContextMenu`]. See [`right_press_outcome`].
#[allow(clippy::too_many_arguments)]
async fn handle_right_click(
    client_id: u64,
    x: u16,
    y: u16,
    release: bool,
    popup_local: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    handle_mouse_button(
        Button::Right,
        client_id,
        x,
        y,
        release,
        popup_local,
        state,
        panes,
        clients,
        config,
        prev_frames,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn handle_mouse_button(
    button: Button,
    client_id: u64,
    x: u16,
    y: u16,
    release: bool,
    popup_local: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let (session_name, cols, rows, mode, naming, theme, asking) = {
        let mut cls = clients.lock().await;
        let client = match cls.get_mut(&client_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        // Clear any active selection on press. Also end any in-progress drag
        // gesture so the next drag starts a fresh anchor (a click always begins
        // a new selection in the same or a different pane). A RELEASE never
        // clears: with `mouse_auto_yank = false` the drag that just ended left a
        // highlight up on purpose, and wiping it here would undo that.
        if !release && button == Button::Left {
            client.mouse_selection = None;
            client.drag = None;
        }
        // Either half of a click ends any in-progress edge auto-scroll.
        client.autoscroll_repeat = None;
        (
            client.session_name.clone(),
            client.cols,
            client.rows,
            client.mode.clone(),
            client.naming(),
            client.theme(config),
            client.popup_want.is_some(),
        )
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    // Build composite to get hit regions and pane rects.
    update_auto_pane_names(&session_name, state, panes).await;
    let (_cells, _cx, _cy, _cv, _cs, _fpr, mut hit_regions, pane_rects, _ack, _bp, layer, _status) =
        build_composite(
            &session_name,
            cols,
            rows,
            &mode,
            state,
            panes,
            config,
            None,
            None,
            &HashMap::new(),
            &theme,
            &naming,
        )
        .await;

    // A client that sent interior-local coordinates did so because it held a
    // popup layer; if the popup is gone by now those coordinates mean nothing
    // to the layout, so the event is dropped.
    if popup_local && layer.is_none() {
        return Ok(());
    }
    let popup = with_float_zoom(
        mouse_popup(layer.as_ref(), popup_local, asking, (cols, rows), config),
        &hit_regions,
    );
    if let Some((rect, _)) = popup.hit {
        // Stack/tab labels the popup covers must stop being clickable, or a
        // click inside the popup could activate a hidden pane behind it.
        hit_regions.stack_regions.retain(|r| {
            !(r.y >= rect.y
                && r.y < rect.y + rect.height
                && r.x_end > rect.x
                && r.x_start < rect.x + rect.width)
        });
    }

    let target = hit_test_with_popup(x, y, &hit_regions, &pane_rects, &popup);
    log::debug!(
        "server: MouseClick client_id={client_id} x={x} y={y} release={release} target={target:?}"
    );

    if button == Button::Right {
        // Copy mode, a pending key chord, a prompt and the session manager each
        // own the mouse or the keyboard, and a menu opened over them could not
        // be answered. The press is dropped, focus included.
        if mode != "NORMAL" {
            return Ok(());
        }
        return right_click_target(
            client_id,
            target,
            &popup,
            hit_regions.float.map(|(id, _)| id),
            &pane_rects,
            (x, y),
            release,
            &session_name,
            state,
            panes,
            clients,
            config,
            prev_frames,
        )
        .await;
    }

    // A press that is not the popup's own dismisses it, then lands as if the
    // popup had never been there. The client keeps the popup's interior and
    // frame to itself, so a press that reaches here with `popup: false` is
    // outside the box; for a client that does not place the popup, the hit test
    // above has said so. Done in this handler, so the focus change below sees
    // the popup already hidden. A release never dismisses: it ends a gesture
    // whose press already had its say.
    //
    // A press on a tab label only switches tabs. Each tab keeps its popup, as
    // it does across a keyboard tab switch, so the tab left keeps its popup up.
    //
    // The arms below send the frame after a focus change or a stack label
    // press, so only a press with no such arm renders here.
    let on_popup = matches!(target, ClickTarget::Pane(id) if popup.pane == Some(id));
    let dismisses =
        !release && popup.pane.is_some() && !on_popup && !matches!(target, ClickTarget::Tab(_));
    let renders_below = matches!(target, ClickTarget::Pane(_) | ClickTarget::StackLabel(_));
    let dismissed = dismisses
        && dismiss_overlay(
            &session_name,
            state,
            panes,
            clients,
            config,
            prev_frames,
            !renders_below,
        )
        .await?;
    // The broadcast above skips a client scrolled back in the pane the popup
    // covered, so a press with no arm of its own repaints that requester here.
    if dismissed && !renders_below {
        repaint_scrolled_requester(client_id, state, panes, clients, config, prev_frames).await;
    }
    let popup_pane = if dismissed { None } else { popup.pane };

    // A pane whose application asked for mouse events gets the press/release
    // itself (the same policy the wheel and a View cell's click follow). Focus
    // still moves first, exactly as tmux does: clicking a pane selects it AND
    // the application sees the click. While the popup is up it owns input, so
    // only the popup's own pane may be driven -- the same rule the focus arms
    // below apply, and forwarding to a pane hidden behind the popup would break
    // it.
    if let ClickTarget::Pane(pane_id) = target {
        if popup_pane.is_none() || popup_pane == Some(pane_id) {
            if let (Some(MouseRoute::App { sgr, motion }), Some((rect, inset))) = (
                route_of_pane(panes, pane_id, MouseGesture::Button, mode == COPY_MODE).await,
                rect_of_pane(&pane_rects, &popup, config, pane_id),
            ) {
                let (col, row) = report_coords(rect, inset, x, y);
                let phase = if release {
                    ButtonPhase::Release
                } else {
                    ButtonPhase::Press
                };
                forward_button(panes, pane_id, sgr, motion, phase, col, row).await?;
            }
        }
    }
    // A release only forwards; it must not re-run the focus/tab side effects a
    // press already applied.
    if release {
        return Ok(());
    }

    match target {
        // A press on the popup itself (a server-drawn popup's own cells) has
        // nothing to focus: the popup already owns input. Any other press
        // dismissed the popup above, so `popup_pane` is `None` for it.
        ClickTarget::Pane(_) | ClickTarget::StackLabel(_) if popup_pane.is_some() => {}
        ClickTarget::Pane(pane_id) => {
            let mut st = state.lock().await;
            let sess = match st.sessions.get_mut(&session_name) {
                Some(s) => s,
                None => return Ok(()),
            };
            let tab = match sess.tabs.get_mut(sess.active_tab) {
                Some(t) => t,
                None => return Ok(()),
            };
            // A dismissed popup leaves the frame to this arm, even when the
            // pane clicked already had the focus.
            if tab.focused_pane != pane_id || dismissed {
                tab.focus_pane(pane_id);
                drop(st);
                // `PaneTreeEntry::is_focused` is in the pushed payload, so
                // click-to-focus is a tree change exactly as keyboard focus is.
                mark_focus_changed();
                broadcast_full_render(&session_name, state, panes, clients, config, prev_frames)
                    .await;
                repaint_scrolled_requester(client_id, state, panes, clients, config, prev_frames)
                    .await;
            }
        }
        ClickTarget::Tab(tab_index) => {
            {
                let mut st = state.lock().await;
                let _ = st.goto_tab(&session_name, tab_index);
            }
            // The clicked tab's focused pane may be one this client scrolled
            // back, which the broadcast below would skip.
            // `TabTreeEntry::is_active` is in the pushed payload, so a tab-bar
            // click is a tree change exactly as the keyboard routes are -- and
            // this arm has to say so itself. `handle_mouse_click` is reached
            // from `ClientMessage::MouseClick`, never from `handle_command`,
            // whose tail is what marks the tree dirty for every command.
            mark_focus_changed();
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
            repaint_scrolled_requester(client_id, state, panes, clients, config, prev_frames).await;
        }
        ClickTarget::StackLabel(pane_id) => {
            // Activate the stacked pane.
            let mut st = state.lock().await;
            let sess = match st.sessions.get_mut(&session_name) {
                Some(s) => s,
                None => return Ok(()),
            };
            let tab = match sess.tabs.get_mut(sess.active_tab) {
                Some(t) => t,
                None => return Ok(()),
            };
            tab.focus_pane(pane_id);
            drop(st);
            // Changes both the stack's active pane and `is_focused`; both are
            // in the pushed payload.
            mark_focus_changed();
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
            repaint_scrolled_requester(client_id, state, panes, clients, config, prev_frames).await;
        }
        ClickTarget::None => {}
    }

    Ok(())
}

/// The right-button half of [`handle_mouse_button`]: `target` is what the
/// press hit, and `at` is where, in the screen coordinates the client sent.
///
/// A press that opens the menu first moves focus to the pane or tab it hit, as a
/// left press does, and the menu's commands name that pane or tab, so a layout
/// change while the menu is open cannot redirect them. While the popup is up it
/// owns input and the layout commands are refused, so a press on a pane outside
/// it does nothing, as does a press outside a float zoom; a tab label still
/// answers. A press that does nothing leaves the mouse selection alone.
///
/// A forwarded press is remembered, and its release goes to that pane alone.
#[allow(clippy::too_many_arguments)]
async fn right_click_target(
    client_id: u64,
    target: ClickTarget,
    popup: &MousePopup,
    float_pane: Option<PaneId>,
    pane_rects: &[(PaneId, Rect)],
    at: (u16, u16),
    release: bool,
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let (x, y) = at;
    if release {
        let pressed = clients
            .lock()
            .await
            .get_mut(&client_id)
            .and_then(|c| c.right_press.take());
        let Some(pane_id) = pressed else {
            return Ok(());
        };
        let (Some(route), Some((rect, inset))) = (
            route_of_pane(panes, pane_id, MouseGesture::Button, false).await,
            rect_of_pane(pane_rects, popup, config, pane_id),
        ) else {
            return Ok(());
        };
        if let MouseRoute::App { sgr, motion } = route {
            let (col, row) = report_coords(rect, inset, x, y);
            forward_button_as(
                panes,
                pane_id,
                Button::Right,
                sgr,
                motion,
                ButtonPhase::Release,
                col,
                row,
            )
            .await?;
        }
        return Ok(());
    }
    // A fresh press starts a new gesture.
    if let Some(c) = clients.lock().await.get_mut(&client_id) {
        c.right_press = None;
    }
    let menu_for = |pane_id: PaneId| {
        open_pane_menu(
            client_id,
            pane_id,
            at,
            session_name,
            state,
            panes,
            clients,
            config,
            prev_frames,
        )
    };
    match target {
        ClickTarget::Pane(pane_id) => {
            // A float zoom fills `popup.pane` too, but it is a pane of the
            // layout: its frame opens the menu and its interior routes like any
            // pane's.
            let in_popup = popup.pane == Some(pane_id) && float_pane != Some(pane_id);
            if popup.pane.is_some() && popup.pane != Some(pane_id) {
                return Ok(());
            }
            let (Some(route), Some((rect, inset))) = (
                route_of_pane(panes, pane_id, MouseGesture::Button, false).await,
                rect_of_pane(pane_rects, popup, config, pane_id),
            ) else {
                return Ok(());
            };
            let on_border = !in_popup && on_pane_border(rect, inset, x, y);
            match right_press_outcome(route, on_border, in_popup) {
                RightPress::Forward => {
                    let MouseRoute::App { sgr, motion } = route else {
                        return Ok(());
                    };
                    clear_mouse_gesture(clients, client_id).await;
                    let (col, row) = report_coords(rect, inset, x, y);
                    if let Some(c) = clients.lock().await.get_mut(&client_id) {
                        c.right_press = Some(pane_id);
                    }
                    forward_button_as(
                        panes,
                        pane_id,
                        Button::Right,
                        sgr,
                        motion,
                        ButtonPhase::Press,
                        col,
                        row,
                    )
                    .await?;
                }
                RightPress::Menu => {
                    clear_mouse_gesture(clients, client_id).await;
                    menu_for(pane_id).await;
                }
                RightPress::Nothing => {}
            }
        }
        ClickTarget::StackLabel(pane_id) if popup.pane.is_none() => {
            clear_mouse_gesture(clients, client_id).await;
            menu_for(pane_id).await;
        }
        ClickTarget::Tab(index) => {
            let opened = {
                let mut st = state.lock().await;
                match st.goto_tab(session_name, index) {
                    Ok(()) => st
                        .sessions
                        .get(session_name)
                        .and_then(|s| s.tabs.get(index).map(|t| (t.id, s.tabs.len()))),
                    Err(_) => None,
                }
            };
            let Some((tab_id, count)) = opened else {
                return Ok(());
            };
            clear_mouse_gesture(clients, client_id).await;
            mark_focus_changed();
            broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
            repaint_scrolled_requester(client_id, state, panes, clients, config, prev_frames).await;
            send_context_menu(
                clients,
                client_id,
                at,
                ContextTarget::Tab {
                    tab_id,
                    index,
                    count,
                },
            )
            .await;
        }
        ClickTarget::StackLabel(_) | ClickTarget::None => {}
    }
    Ok(())
}

/// End the client's selection and drag, as a press that acts does.
async fn clear_mouse_gesture(clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>, client_id: u64) {
    if let Some(c) = clients.lock().await.get_mut(&client_id) {
        c.mouse_selection = None;
        c.drag = None;
    }
}

/// Focus `pane_id` as a left press would, then ask the client to open the pane
/// menu.
#[allow(clippy::too_many_arguments)]
async fn open_pane_menu(
    client_id: u64,
    pane_id: PaneId,
    at: (u16, u16),
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    let (refocused, facts) = {
        let mut st = state.lock().await;
        let Some(tab) = st
            .sessions
            .get_mut(session_name)
            .and_then(|s| s.tabs.get_mut(s.active_tab))
        else {
            return;
        };
        let refocused = tab.focused_pane != pane_id;
        if refocused {
            tab.focus_pane(pane_id);
        }
        (
            refocused,
            ContextTarget::Pane {
                pane_id,
                zoomed: tab.zoomed_pane.is_some(),
                zoom_mode: config.zoom.mode,
                monocle: matches!(tab.layout_mode, LayoutMode::Monocle(_)),
            },
        )
    };
    if refocused {
        mark_focus_changed();
        broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
        repaint_scrolled_requester(client_id, state, panes, clients, config, prev_frames).await;
    }
    send_context_menu(clients, client_id, at, facts).await;
}

async fn send_context_menu(
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    client_id: u64,
    (x, y): (u16, u16),
    target: ContextTarget,
) {
    let cls = clients.lock().await;
    if let Some(client) = cls.get(&client_id) {
        let _ = client.tx.send(ServerMessage::ContextMenu { x, y, target });
    }
}

/// Whether a focus change just carried the zoom from `before` onto another pane.
///
/// The newly zoomed pane is painted full-area but its PTY still has the size of
/// its slot in the layout, so the caller must run `resize_session_panes`. A
/// stack's hidden panes are sized to the stack, never to the zoomed area, so
/// this holds for a step within the zoomed stack as well.
fn zoom_moved_to_another_pane(tab: &session::Tab, before: PaneId) -> bool {
    tab.zoomed_pane.is_some() && tab.focused_pane != before
}

/// Pure decision for the layout cycle's next mode, factored out for testing.
///
/// The automatic cycle is `Bsp -> Master -> Monocle -> Grid -> Columns -> Rows
/// -> Bsp`, skipping whatever `enabled` excludes. Two custom-aware detours ride
/// on top: leaving `Custom` starts the automatic cycle at the first enabled
/// mode, and leaving the last enabled mode returns to `Custom` when a
/// restorable custom layout was remembered (`has_saved_custom`).
fn next_layout_mode(
    current: &LayoutMode,
    has_saved_custom: bool,
    enabled: &EnabledLayouts,
) -> LayoutMode {
    if has_saved_custom && current.is_last_enabled(enabled) {
        return LayoutMode::Custom(CustomLayout);
    }
    current.next_enabled(enabled)
}

/// Return true if `saved` can be restored as the current custom layout, i.e.
/// it exists and its leaf pane-id set exactly equals the live `pane_order`.
///
/// The comparison is order-independent: the saved tree's traversal order need
/// not match `pane_order`'s insertion order. A mismatch (panes added/removed
/// while in an automatic layout) makes the snapshot stale and non-restorable.
fn saved_custom_is_restorable(saved: &Option<LayoutNode>, pane_order: &[PaneId]) -> bool {
    let Some(tree) = saved else {
        return false;
    };
    let mut saved_ids = layout::all_pane_ids(tree);
    saved_ids.sort_unstable();
    let mut live_ids = pane_order.to_vec();
    live_ids.sort_unstable();
    saved_ids == live_ids
}

#[allow(clippy::too_many_arguments)]
async fn handle_mouse_drag(
    client_id: u64,
    start_x: u16,
    start_y: u16,
    end_x: u16,
    end_y: u16,
    is_final: bool,
    popup_local: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    log::debug!(
        "server: MouseDrag client_id={client_id} start=({start_x},{start_y}) end=({end_x},{end_y}) is_final={is_final} popup={popup_local}"
    );
    let (session_name, cols, rows, mode, naming, theme, asking) = {
        let cls = clients.lock().await;
        let client = match cls.get(&client_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        (
            client.session_name.clone(),
            client.cols,
            client.rows,
            client.mode.clone(),
            client.naming(),
            client.theme(config),
            client.popup_want.is_some(),
        )
    };
    let session_name = match session_name {
        Some(s) => s,
        None => {
            disarm_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };

    // Build composite to get pane rects for coordinate mapping.
    update_auto_pane_names(&session_name, state, panes).await;
    let (_cells, _cx, _cy, _cv, _cs, _fpr, mut hit_regions, pane_rects, _ack, _bp, layer, _status) =
        build_composite(
            &session_name,
            cols,
            rows,
            &mode,
            state,
            panes,
            config,
            None,
            None,
            &HashMap::new(),
            &theme,
            &naming,
        )
        .await;

    // A client that sent interior-local coordinates did so because it held a
    // popup layer; if the popup is gone by now those coordinates mean nothing
    // to the layout, so the event is dropped.
    if popup_local && layer.is_none() {
        disarm_autoscroll(clients, client_id).await;
        return Ok(());
    }
    let popup = with_float_zoom(
        mouse_popup(layer.as_ref(), popup_local, asking, (cols, rows), config),
        &hit_regions,
    );
    if let Some((rect, _)) = popup.hit {
        // Stack/tab labels the popup covers must stop being clickable, or a
        // click inside the popup could activate a hidden pane behind it.
        hit_regions.stack_regions.retain(|r| {
            !(r.y >= rect.y
                && r.y < rect.y + rect.height
                && r.x_end > rect.x
                && r.x_start < rect.x + rect.width)
        });
    }

    // Find which pane the drag started in (the popup first, when it is up).
    let start_target = hit_test_with_popup(start_x, start_y, &hit_regions, &pane_rects, &popup);
    let target_pane = match start_target {
        ClickTarget::Pane(id) => id,
        _ => {
            disarm_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };

    // Find the pane's rect for coordinate mapping.
    let (pane_rect, inset) = match rect_of_pane(&pane_rects, &popup, config, target_pane) {
        Some(r) => r,
        None => {
            disarm_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };

    // The shared routing decision (see `mouse_route`). An application that asked
    // for mouse events owns the gesture: it gets the motion/release report and
    // remux selects nothing, so the same drag that would highlight text in a
    // shell drives the application's own selection instead.
    let route =
        match route_of_pane(panes, target_pane, MouseGesture::Button, mode == COPY_MODE).await {
            Some(r) => r,
            None => {
                disarm_autoscroll(clients, client_id).await;
                return Ok(());
            }
        };
    if let MouseRoute::App { sgr, motion } = route {
        disarm_autoscroll(clients, client_id).await;
        let (col, row) = report_coords(pane_rect, inset, end_x, end_y);
        let phase = if is_final {
            ButtonPhase::Release
        } else {
            ButtonPhase::Motion
        };
        return forward_button(panes, target_pane, sgr, motion, phase, col, row).await;
    }
    // False on the alternate screen: remux's scrollback belongs to the PRIMARY
    // screen there, so an edge drag must neither scroll into it nor arm the
    // repeat timer for a scroll that can never finish.
    let may_scroll = matches!(route, MouseRoute::Remux { scrollback: true });

    // The inset the content sits inside the rect with.
    let border_offset: u16 = inset;
    let content_width = pane_rect.width.saturating_sub(border_offset * 2);
    let content_height = pane_rect.height.saturating_sub(border_offset * 2);

    // Map screen coordinates to pane-local coordinates (relative to content
    // area). Columns clamp to the content width; the start row clamps into the
    // content area too, but the end row is kept both raw (for edge detection)
    // and as a clamped display row.
    let local_start_x = start_x
        .saturating_sub(pane_rect.x + border_offset)
        .min(content_width.saturating_sub(1));
    let local_start_y = start_y
        .saturating_sub(pane_rect.y + border_offset)
        .min(content_height.saturating_sub(1));
    let local_end_x = end_x
        .saturating_sub(pane_rect.x + border_offset)
        .min(content_width.saturating_sub(1));
    let raw_end_row = end_y.saturating_sub(pane_rect.y + border_offset);
    let end_row = raw_end_row.min(content_height.saturating_sub(1));

    // Edge detection: is the drag end on the top or bottom content row?
    let at_top = end_row == 0;
    let at_bottom = content_height > 0 && end_row == content_height - 1;

    // The scroll offset that maps these screen coordinates to lines is the one
    // the client holds for the pane being dragged over, focused or not: a
    // selection started in a pane scrolled back through history must land on
    // the text the user can see there.
    let (scroll_offset, gesture) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => (
                c.scroll_of(target_pane),
                c.drag
                    .as_ref()
                    .map(|d| (d.pane_id, d.anchor_col, d.anchor_abs)),
            ),
            None => return Ok(()),
        }
    };
    // A drag into a different pane (or with no active gesture) starts fresh.
    let new_gesture = match gesture {
        Some((pid, _, _)) => pid != target_pane,
        None => true,
    };
    let base_offset = scroll_offset;

    // Compute the new scroll offset (focused pane only), the anchor in absolute
    // coordinates, the end point in absolute coordinates, and the anchor's row
    // projected back into the current viewport. abs_of_row / row_of_abs are pure
    // over (scrollback len, eviction count, rows), so this is one lock scope.
    // Yields `None` (via labeled break) if the target pane vanished, so the
    // panes lock is released before we disarm autoscroll (which locks `clients`)
    // -- avoids nesting the two locks.
    let computed = 'compute: {
        let ps = panes.lock().await;
        let screen = match ps.get(&target_pane) {
            Some(p) => &p.screen,
            None => break 'compute None,
        };

        // Max scroll offset (top of scrollback); also gates whether edge
        // auto-scroll should keep repeating (see `autoscroll_repeat` below).
        let screen_max_scroll_offset = screen.max_scroll_offset();

        // Anchor: reuse the stored one, or capture a fresh anchor from the drag
        // start (in the pre-scroll view) for a new gesture.
        let (anchor_col, anchor_abs) = if new_gesture {
            (local_start_x, screen.abs_of_row(base_offset, local_start_y))
        } else {
            let (_, col, abs) = gesture.unwrap();
            (col, abs)
        };

        // Edge auto-scroll. A final drag (button release) never scrolls: the
        // selection must end exactly where the user let go, so the yanked range
        // matches the highlight shown at release instead of pulling in one
        // extra edge line.
        let new_offset = if !is_final && may_scroll {
            if at_top {
                (scroll_offset + 1).min(screen_max_scroll_offset)
            } else if at_bottom {
                scroll_offset.saturating_sub(1)
            } else {
                scroll_offset
            }
        } else {
            scroll_offset
        };
        let new_base = new_offset;

        let end_abs = screen.abs_of_row(new_base, end_row);
        let anchor_row_i64 = screen.row_of_abs(new_base, anchor_abs);
        Some((
            new_offset,
            anchor_col,
            anchor_abs,
            end_abs,
            anchor_row_i64,
            screen_max_scroll_offset,
        ))
    };
    let (new_offset, anchor_col, anchor_abs, end_abs, anchor_row_i64, screen_max_scroll_offset) =
        match computed {
            Some(v) => v,
            None => {
                disarm_autoscroll(clients, client_id).await;
                return Ok(());
            }
        };

    // Project the anchor into the current viewport for the (viewport-relative)
    // MouseSelection: an anchor scrolled above the viewport clamps to row 0,
    // one scrolled below clamps to the last content row.
    let anchor_row = anchor_row_i64.clamp(0, content_height.saturating_sub(1) as i64) as u16;
    let end_col = local_end_x;

    // Commit the gesture, the new scroll offset, and the derived selection.
    let offset_changed = new_offset != scroll_offset;
    {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            if new_gesture {
                client.drag = Some(DragSession {
                    pane_id: target_pane,
                    anchor_col,
                    anchor_abs,
                    end_abs,
                    end_col,
                });
            } else if let Some(d) = client.drag.as_mut() {
                // Continuing gesture: keep the moving end in absolute coords so a
                // wheel-scroll (which has no mouse coords) can extend from it.
                d.end_abs = end_abs;
                d.end_col = end_col;
            }
            if offset_changed {
                client.set_scroll(target_pane, new_offset);
                client.needs_full_render = true;
            }
            client.mouse_selection = Some(MouseSelection {
                pane_id: target_pane,
                start: (anchor_col, anchor_row),
                end: (end_col, end_row),
            });
            // Arm/disarm the repeating edge-scroll timer. Keep firing only while
            // resting on a content edge that still has room to scroll; disarm
            // exactly at the scrollback top / live bottom so the timer stops
            // instead of spinning. A final drag never arms.
            client.autoscroll_repeat = if !is_final
                && may_scroll
                && ((at_top && new_offset < screen_max_scroll_offset)
                    || (at_bottom && new_offset > 0))
            {
                Some((start_x, start_y, end_x, end_y, popup_local))
            } else {
                None
            };
        }
    }

    if is_final {
        // Mouse button released -- decide based on mouse_auto_yank config.
        if config.general.mouse_auto_yank {
            // Extract text over the absolute selection range so the yank is
            // correct even when it spans scrollback.
            let selected_text = {
                let ps = panes.lock().await;
                if let Some(pane_data) = ps.get(&target_pane) {
                    extract_selection_text(
                        &pane_data.screen,
                        anchor_col,
                        anchor_abs,
                        end_col,
                        end_abs,
                    )
                } else {
                    String::new()
                }
            };

            if !selected_text.is_empty() {
                let cls = clients.lock().await;
                if let Some(client) = cls.get(&client_id) {
                    let _ = client.tx.send(ServerMessage::CopyToClipboard {
                        data: selected_text,
                    });
                }
            }

            // Clear selection state after copying.
            {
                let mut cls = clients.lock().await;
                if let Some(client) = cls.get_mut(&client_id) {
                    client.mouse_selection = None;
                }
            }
        }
        // When mouse_auto_yank is false, selection stays visible for keyboard
        // adjustment in visual mode. No copy, no clear.

        // Always end the drag gesture on release.
        {
            let mut cls = clients.lock().await;
            if let Some(client) = cls.get_mut(&client_id) {
                client.drag = None;
            }
        }
    }

    // Re-render for THIS client so its scrolled view + selection show. Use the
    // per-client path (like handle_scroll_delta): broadcast_full_render renders
    // at offset 0 and would not reflect this client's auto-scrolled viewport.
    send_full_render_to_client(
        client_id,
        &session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        RenderCtx::default(),
    )
    .await;

    Ok(())
}

/// Disarm the pane-scoped drag-autoscroll repeat timer for a client. The
/// [`disarm_autoscroll`] analog for a View cell's gesture.
async fn disarm_pane_autoscroll(
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    client_id: u64,
) {
    let mut cls = clients.lock().await;
    if let Some(c) = cls.get_mut(&client_id) {
        c.pane_autoscroll_repeat = None;
    }
}

/// A left-click inside a View cell: clear that cell's selection and end any
/// gesture on it, so the drag that follows starts from a fresh anchor.
///
/// Sent on the press, and also on a release that never moved -- which is a click,
/// not a selection. That second case is not just tidiness: it is what disarms the
/// repeat timer for a gesture that wandered onto a content edge and came back, so
/// the cell stops auto-scrolling once the button is up. The disarm therefore
/// happens unconditionally; only the (comparatively costly) repaint is skipped
/// when there was no selection to clear.
///
/// The pane-scoped counterpart of [`handle_mouse_click`]. There is no hit
/// testing to do -- the client already resolved which cell was clicked against
/// its own cell rects -- and no focus to move: cell focus is shared view state
/// driven by `ViewSetFocus`, not by this message.
#[allow(clippy::too_many_arguments)]
async fn handle_pane_mouse_click(
    client_id: u64,
    pane_id: PaneId,
    x: u16,
    y: u16,
    release: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let (had_selection, copy_mode) = {
        let mut cls = clients.lock().await;
        match cls.get_mut(&client_id) {
            // Only a subscribed client may drive a pane it watches (same guard
            // as `ScrollPane`).
            Some(conn) if conn.subscribed_panes.contains_key(&pane_id) => {
                let copy_mode = conn.mode == COPY_MODE;
                // A release leaves an intentionally-kept highlight alone; see
                // the same rule in `handle_mouse_click`.
                let had = if release {
                    false
                } else {
                    conn.pane_selection.remove(&pane_id).is_some()
                };
                if !release && conn.pane_drag.as_ref().map(|d| d.pane_id) == Some(pane_id) {
                    conn.pane_drag = None;
                }
                conn.pane_autoscroll_repeat = None;
                (had, copy_mode)
            }
            _ => return Ok(()),
        }
    };
    // Same policy as a session pane: an application that asked for mouse events
    // gets the press/release. Cell coordinates arrive content-relative, so they
    // only need the 1-based bias a report carries.
    if let Some(MouseRoute::App { sgr, motion }) =
        route_of_pane(panes, pane_id, MouseGesture::Button, copy_mode).await
    {
        // A highlight left over from before the application claimed the mouse
        // still has to be repainted away.
        if had_selection {
            stream_pane_content(pane_id, state, panes, clients).await;
        }
        let phase = if release {
            ButtonPhase::Release
        } else {
            ButtonPhase::Press
        };
        return forward_button(panes, pane_id, sgr, motion, phase, x + 1, y + 1).await;
    }
    // Only repaint when something actually changed: a plain click-to-focus in a
    // view is the common case and should cost no extra frame.
    if had_selection {
        stream_pane_content(pane_id, state, panes, clients).await;
    }
    Ok(())
}

/// A left-drag inside a View cell: select text in that cell's source pane.
///
/// The pane-scoped counterpart of [`handle_mouse_drag`], and deliberately built
/// from the same pieces -- [`Screen::abs_of_row`]/[`Screen::row_of_abs`] for an
/// eviction-stable anchor, [`extract_selection_text`] for the yank, and
/// `mouse_auto_yank` for the release semantics -- so a cell selects, scrolls and
/// copies exactly like a normal pane does. The differences are all consequences
/// of the client being *detached* while a view is up:
///
/// * coordinates arrive already content-relative (the client owns the cell
///   geometry; the server has no layout rect for a cell), so there is no
///   composite/hit-test step;
/// * the scroll offset auto-scroll moves is the per-(client, pane) `pane_scroll`
///   that the wheel already drives, not the foreground `session_scroll`;
/// * the repaint is a per-subscriber `PaneContent`, not a session frame.
#[allow(clippy::too_many_arguments)]
async fn handle_pane_mouse_drag(
    client_id: u64,
    pane_id: PaneId,
    start_x: u16,
    start_y: u16,
    end_x: u16,
    end_y: u16,
    is_final: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) -> Result<()> {
    // Current per-(client, pane) offset and the state of any gesture already
    // running on this pane. A drag on a pane this client does not subscribe to
    // is ignored, mirroring `ScrollPane`.
    //
    // The lookup yields an Option and the "ignored" branch is handled AFTER the
    // block, so the `clients` guard is dropped before `disarm_pane_autoscroll`
    // takes it again: a match arm owns the scrutinee, so `cls` would still be
    // live inside it, and `tokio::sync::Mutex` is not reentrant. Calling disarm
    // from inside the match parked this task forever *holding* `clients`, which
    // wedged every other task that needed the map -- the whole daemon, not just
    // this client. Do not re-inline it.
    let subscribed = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) if c.subscribed_panes.contains_key(&pane_id) => Some((
                c.pane_scroll.get(&pane_id).copied().unwrap_or(0),
                c.pane_drag
                    .as_ref()
                    .filter(|d| d.pane_id == pane_id)
                    .map(|d| (d.anchor_col, d.anchor_abs)),
                c.mode == COPY_MODE,
            )),
            _ => None,
        }
    };
    let Some((scroll_offset, gesture, copy_mode)) = subscribed else {
        disarm_pane_autoscroll(clients, client_id).await;
        return Ok(());
    };
    let new_gesture = gesture.is_none();

    // The shared routing decision (see `mouse_route`), identical to the one the
    // session path makes -- which is the whole point: a cell aliasing a pane
    // running a mouse-aware application drives it, instead of selecting text
    // over the top of it.
    let route = match route_of_pane(panes, pane_id, MouseGesture::Button, copy_mode).await {
        Some(r) => r,
        None => {
            disarm_pane_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };
    if let MouseRoute::App { sgr, motion } = route {
        disarm_pane_autoscroll(clients, client_id).await;
        let phase = if is_final {
            ButtonPhase::Release
        } else {
            ButtonPhase::Motion
        };
        return forward_button(panes, pane_id, sgr, motion, phase, end_x + 1, end_y + 1).await;
    }
    // No scrolling into the primary screen's history from an alt-screen cell,
    // and therefore no repeat timer either (see `MouseRoute::Remux`).
    let may_scroll = matches!(route, MouseRoute::Remux { scrollback: true });

    // Everything that needs the pane's screen, in one lock scope: the content
    // size to clamp against, the anchor/end in absolute coordinates, and the
    // scroll bound that gates the repeat timer.
    let computed = 'compute: {
        let ps = panes.lock().await;
        let screen = match ps.get(&pane_id) {
            Some(pd) => &pd.screen,
            None => break 'compute None,
        };
        let content_width = screen.cols;
        let content_height = screen.rows;
        if content_width == 0 || content_height == 0 {
            break 'compute None;
        }
        let max_scroll = screen.max_scroll_offset();
        // The stored offset can outlive the scrollback it addressed; clamp it
        // before deriving anything from it (`stream_pane_content` clamps the
        // same way when rendering).
        let base_offset = scroll_offset.min(max_scroll);

        let local_start_x = start_x.min(content_width - 1);
        let local_start_y = start_y.min(content_height - 1);
        let end_col = end_x.min(content_width - 1);
        let end_row = end_y.min(content_height - 1);

        // Edge auto-scroll: resting on the top/bottom content row pulls history
        // in. A final drag (release) never scrolls -- the yanked range must match
        // the highlight the user saw when letting go.
        let at_top = end_row == 0;
        let at_bottom = end_row == content_height - 1;
        let new_offset = if is_final || !may_scroll {
            base_offset
        } else if at_top {
            (base_offset + 1).min(max_scroll)
        } else if at_bottom {
            base_offset.saturating_sub(1)
        } else {
            base_offset
        };

        // Anchor in eviction-stable absolute coordinates, captured in the
        // PRE-scroll view for a fresh gesture and reused thereafter, so
        // auto-scrolling under the pointer never drags the anchor along.
        let (anchor_col, anchor_abs) = match gesture {
            Some(g) => g,
            None => (local_start_x, screen.abs_of_row(base_offset, local_start_y)),
        };
        let end_abs = screen.abs_of_row(new_offset, end_row);
        let anchor_row_i64 = screen.row_of_abs(new_offset, anchor_abs);
        // Project the anchor back into the post-scroll viewport for the
        // (viewport-relative) highlight: an anchor scrolled off the top clamps
        // to row 0, one below the fold to the last content row.
        let anchor_row = anchor_row_i64.clamp(0, content_height as i64 - 1) as u16;
        Some((
            new_offset, max_scroll, anchor_col, anchor_abs, anchor_row, end_col, end_row, end_abs,
            at_top, at_bottom,
        ))
    };
    let (
        new_offset,
        max_scroll,
        anchor_col,
        anchor_abs,
        anchor_row,
        end_col,
        end_row,
        end_abs,
        at_top,
        at_bottom,
    ) = match computed {
        Some(v) => v,
        None => {
            disarm_pane_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };

    // Commit the gesture, the new offset and the derived highlight.
    {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            if new_gesture {
                client.pane_drag = Some(DragSession {
                    pane_id,
                    anchor_col,
                    anchor_abs,
                    end_abs,
                    end_col,
                });
            } else if let Some(d) = client.pane_drag.as_mut() {
                d.end_abs = end_abs;
                d.end_col = end_col;
            }
            if new_offset == 0 {
                client.pane_scroll.remove(&pane_id);
            } else {
                client.pane_scroll.insert(pane_id, new_offset);
            }
            client.pane_selection.insert(
                pane_id,
                MouseSelection {
                    pane_id,
                    start: (anchor_col, anchor_row),
                    end: (end_col, end_row),
                },
            );
            // Keep the repeat timer firing only while resting on an edge that
            // still has somewhere to go, so it stops at the scrollback top /
            // live bottom instead of spinning.
            client.pane_autoscroll_repeat = if !is_final
                && may_scroll
                && ((at_top && new_offset < max_scroll) || (at_bottom && new_offset > 0))
            {
                Some((pane_id, start_x, start_y, end_x, end_y))
            } else {
                None
            };
        }
    }

    if is_final {
        if config.general.mouse_auto_yank {
            // Extract over the ABSOLUTE range so a selection dragged through
            // scrollback yanks what it covered, not what is on screen now.
            let selected_text = {
                let ps = panes.lock().await;
                match ps.get(&pane_id) {
                    Some(pd) => {
                        extract_selection_text(&pd.screen, anchor_col, anchor_abs, end_col, end_abs)
                    }
                    None => String::new(),
                }
            };
            let mut cls = clients.lock().await;
            if let Some(client) = cls.get_mut(&client_id) {
                if !selected_text.is_empty() {
                    let _ = client.tx.send(ServerMessage::CopyToClipboard {
                        data: selected_text,
                    });
                }
                client.pane_selection.remove(&pane_id);
            }
        }
        // With mouse_auto_yank off the highlight stays up for keyboard
        // adjustment, exactly as in a normal pane. Either way the gesture ends.
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            client.pane_drag = None;
        }
    }

    // Repaint: every subscriber renders at its own offset/selection, so this
    // shows the highlight (and any auto-scroll) to this client alone.
    stream_pane_content(pane_id, state, panes, clients).await;
    Ok(())
}

/// Extract text from a pane's screen buffer between two selection endpoints
/// given in absolute, eviction-stable line coordinates (see
/// [`Screen::abs_of_row`]). Working in absolute space means the yank is correct
/// even when the selection spans scrollback that has scrolled out of the
/// viewport during a drag.
fn extract_selection_text(
    screen: &Screen,
    anchor_col: u16,
    anchor_abs: usize,
    end_col: u16,
    end_abs: usize,
) -> String {
    // Normalize so the earlier point (in reading order) comes first.
    let (first_abs, first_col, last_abs, last_col) =
        if (anchor_abs, anchor_col) <= (end_abs, end_col) {
            (anchor_abs, anchor_col, end_abs, end_col)
        } else {
            (end_abs, end_col, anchor_abs, anchor_col)
        };

    let mut result = String::new();
    let mut first_line = true;
    for abs in first_abs..=last_abs {
        let row_data = match screen.line_at(screen.array_index_of_abs(abs)) {
            Some(r) => r,
            None => continue,
        };
        let col_start = if abs == first_abs {
            first_col as usize
        } else {
            0
        };
        let col_end = if abs == last_abs {
            (last_col as usize + 1).min(row_data.len())
        } else {
            row_data.len()
        };
        let col_start = col_start.min(row_data.len());
        let col_end = col_end.max(col_start);

        let text: String = row_data[col_start..col_end].iter().map(|c| c.c).collect();
        if !first_line {
            result.push('\n');
        }
        first_line = false;
        result.push_str(text.trim_end());
    }
    result
}

// ---------------------------------------------------------------------------
// Pane management helpers
// ---------------------------------------------------------------------------

/// Where a newly created pane goes in its tab's layout tree.
///
/// The three manual placements splice the tree by hand and therefore **eject the
/// tab to `Custom`** (see [`create_pane_in_tab`]); `Auto` is the layout-driven
/// one that lets an automatic mode rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanePlacement {
    /// Manual vertical split of the focused pane (left/right).
    SplitVertical,
    /// Manual horizontal split of the focused pane (top/bottom).
    SplitHorizontal,
    /// Manual splice into the focused pane's stack node.
    Stack,
    /// Layout-driven: rebuild from the automatic mode, or -- already in
    /// `Custom` -- a vertical split of the focused pane.
    Auto,
}

impl PanePlacement {
    /// Whether this placement edits the tree by hand rather than letting the
    /// layout mode build it.
    fn is_manual(self) -> bool {
        !matches!(self, PanePlacement::Auto)
    }

    /// The placement to apply in a tab laid out by `mode`. A split along the
    /// axis that Columns or Rows already lays panes out on asks for exactly
    /// what the layout's own rebuild gives, so it becomes `Auto` and the tab
    /// keeps its mode instead of ejecting to `Custom`.
    fn for_layout(self, mode: &LayoutMode) -> Self {
        match (self, mode) {
            (PanePlacement::SplitVertical, LayoutMode::Columns(_))
            | (PanePlacement::SplitHorizontal, LayoutMode::Rows(_)) => PanePlacement::Auto,
            _ => self,
        }
    }

    /// The PTY's initial size, given the session's render size. A split hands
    /// the new pane about half the axis it divides; the others start full size.
    /// Only the seed size -- `resize_session_panes` corrects it from the real
    /// layout immediately afterwards.
    fn spawn_size(self, cols: u16, rows: u16) -> (u16, u16) {
        match self {
            PanePlacement::SplitVertical => (cols / 2, rows),
            PanePlacement::SplitHorizontal => (cols, rows / 2),
            PanePlacement::Stack | PanePlacement::Auto => (cols, rows),
        }
    }
}

/// **The one pane-creation path.** Insert a new pane into `session_name`'s tab
/// (`tab_index`, or the active tab when `None`) according to `placement`, spawn
/// its PTY inheriting the previously focused pane's CWD, and start forwarding
/// its output.
///
/// Every `RemuxCommand` that creates a pane funnels through here, which is what
/// keeps the two rules below true for all of them:
///
/// * **A manual placement ejects the tab to `Custom`**, except a split along
///   the Columns/Rows axis, which [`PanePlacement::for_layout`] turns into
///   `Auto`. `PaneSplit*` and `PaneStackAdd` mutate the tree directly, so
///   leaving the tab in an automatic mode would let the next rebuild (`PaneNew` / `LayoutNext` / `SetMaster`)
///   silently discard the arrangement the user just made -- and, because
///   `saved_custom_layout` is only snapshotted when the mode is *already*
///   `Custom`, discard it unrecoverably. Stacking used to skip this and lose
///   stacks exactly that way.
/// * **A new pane always clears the zoom.** The tab is showing a new
///   arrangement, so the old full-area pane is no longer what the user asked
///   for.
///
/// Returns the new pane's id, or `None` when the session/tab could not be
/// resolved. The caller owns the refresh tail (`resize_session_panes` +
/// `broadcast_full_render`, or `refresh_target_session` for another session's
/// tab), since which clients to repaint is a caller-side decision.
///
/// `command`/`args`/`cwd` are the OPTIONAL overrides. Every `RemuxCommand` that
/// creates a pane passes `(None, &[], None)` and gets what it always got: the
/// configured shell, in the previously focused pane's directory. The one caller
/// that passes them is `OpenInSplit`, which needs a specific editor, on a
/// specific file, in that file's directory -- and it comes through HERE rather
/// than down a second pane-creation path, because a split that skipped this
/// function would eject neither the tab to `Custom` nor the zoom.
/// Create a new tab in `session_name`, and spawn its first pane.
///
/// The counterpart to [`create_pane_in_tab`] for the one case that function
/// cannot serve: a tab's FIRST pane is created by `ServerState::create_tab`,
/// which mints the pane id as part of building the tab, so there is no existing
/// pane in that tab to split. This is therefore not a second pane-creation path
/// in the sense ruling 4 forbids -- `TabNew`/`TabNewInSession` have always
/// worked this way -- it is that path, extracted so `remux new-tab` can hand it
/// a command instead of copying it.
///
/// `command`/`args`/`cwd` mean exactly what they mean in `create_pane_in_tab`:
/// `None`/empty gives the login shell, and a `None` cwd inherits the session's
/// previously-focused pane, so a new tab opens where the old one was.
///
/// Returns the new pane's id, or `None` when the session does not exist or the
/// tab could not be created -- both logged, neither an error to the caller,
/// matching how every other session-targeted command treats a name that has
/// gone away.
#[allow(clippy::too_many_arguments)]
async fn create_tab_in_session(
    session_name: &str,
    command: Option<&str>,
    args: &[String],
    cwd: Option<&std::path::Path>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<Option<PaneId>> {
    // Use the TARGET session's own render dimensions: the requester may be
    // attached elsewhere, or -- for a CLI invocation -- nowhere at all.
    let (tcols, trows) = session_render_size(session_name, clients).await;
    // Capture the source pane (the active tab's focused pane) BEFORE create_tab
    // flips active_tab to the new empty tab.
    let (pane_id, source_pane_id) = {
        let mut st = state.lock().await;
        let source_pane_id = st
            .sessions
            .get(session_name)
            .and_then(|s| s.tabs.get(s.active_tab))
            .map(|t| t.focused_pane);
        let tab_count = match st.sessions.get(session_name) {
            Some(s) => s.tabs.len(),
            None => {
                log::info!("create_tab_in_session: session '{session_name}' not found");
                return Ok(None);
            }
        };
        let tab_name = format!("Tab {}", tab_count + 1);
        let layout_mode = config.new_tab_layout();
        match st.create_tab(session_name, &tab_name, layout_mode) {
            Ok(pid) => (pid, source_pane_id),
            Err(e) => {
                log::info!("create_tab_in_session: {e}");
                return Ok(None);
            }
        }
    };
    // An explicit `cwd` wins over inheritance and the inheritance is not even
    // computed then -- the same rule `create_pane_in_tab` follows, for the same
    // reason.
    let focused_cwd = match cwd {
        Some(_) => None,
        None => {
            let panes_lock = panes.lock().await;
            source_pane_id
                .and_then(|id| panes_lock.get(&id))
                .and_then(|p| persistence::get_pane_cwd(p.pty.child_pid))
        }
    };
    // `args` COUNTED, not printed: `remux new-tab -- <command line>` reaches
    // here, and its arguments are where a credential lives. See
    // `handle_client_message`'s summary match for the rule.
    log::debug!(
        "server: create_tab_in_session session={session_name:?} new pane_id={pane_id} \
command={command:?} ({} arg(s))",
        args.len()
    );
    spawn_pane(
        pane_id,
        tcols,
        trows,
        command,
        args,
        cwd.or(focused_cwd.as_deref().map(std::path::Path::new)),
        Some(PaneIdentity {
            session: session_name,
            pane: pane_id,
        }),
        panes,
        config,
    )
    .await?;
    // create_tab makes the new tab active, so forwarding starts for its
    // pane here. (For a mutation on a non-active tab, forwarding would
    // instead begin on the next SessionSwitchTab; the guard makes the
    // repeat call safe.)
    start_pty_forwarding(
        session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        dormant,
    )
    .await;
    refresh_target_session(session_name, state, panes, clients, config, prev_frames).await?;
    Ok(Some(pane_id))
}

#[allow(clippy::too_many_arguments)]
async fn create_pane_in_tab(
    session_name: &str,
    tab_index: Option<usize>,
    mut placement: PanePlacement,
    cols: u16,
    rows: u16,
    command: Option<&str>,
    args: &[String],
    cwd: Option<&std::path::Path>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<Option<PaneId>> {
    let (new_pane_id, source_pane_id) = {
        let mut st = state.lock().await;
        let new_pane_id = st.next_pane_id();
        let sess = match st.sessions.get_mut(session_name) {
            Some(s) => s,
            None => {
                log::info!("create_pane_in_tab: session '{session_name}' not found");
                return Ok(None);
            }
        };
        let index = tab_index.unwrap_or(sess.active_tab);
        let tab = match sess.tabs.get_mut(index) {
            Some(t) => t,
            None => {
                log::info!("create_pane_in_tab: tab index {index} out of range");
                return Ok(None);
            }
        };
        let focused = tab.focused_pane;
        placement = placement.for_layout(&tab.layout_mode);
        // Eject to Custom before touching the tree, so `is_automatic()` below
        // sees the post-eject mode.
        if placement.is_manual() && tab.layout_mode.is_automatic() {
            tab.layout_mode = LayoutMode::Custom(CustomLayout);
        }
        // Push first: an automatic rebuild reads `pane_order`.
        tab.pane_order.push(new_pane_id);
        match placement {
            PanePlacement::SplitVertical => {
                tab.layout.split_vertical(focused, new_pane_id);
            }
            PanePlacement::SplitHorizontal => {
                tab.layout.split_horizontal(focused, new_pane_id);
            }
            PanePlacement::Stack => {
                tab.layout.add_to_stack(focused, new_pane_id);
            }
            PanePlacement::Auto => {
                tab.place_auto(new_pane_id);
            }
        }
        tab.focus_new_pane(new_pane_id);
        log::debug!(
            "server: create_pane_in_tab session={session_name} tab={index} \
             placement={placement:?} new_pane_id={new_pane_id} from focused={focused}"
        );
        session::debug_check_invariant(sess, "create_pane_in_tab");
        (new_pane_id, focused)
    };

    // An explicit `cwd` wins over inheritance, and the inheritance is not even
    // computed then. Inheriting the focused pane's directory is right for "give
    // me another shell here" and wrong for "open THIS file": the editor gets an
    // absolute path either way, but a relative `:e` inside it should resolve
    // beside the file, not beside whatever pane happened to have focus.
    let source_cwd = match cwd {
        Some(_) => None,
        None => {
            let panes_lock = panes.lock().await;
            panes_lock
                .get(&source_pane_id)
                .and_then(|p| persistence::get_pane_cwd(p.pty.child_pid))
        }
    };
    let (spawn_cols, spawn_rows) = placement.spawn_size(cols, rows);
    spawn_pane(
        new_pane_id,
        spawn_cols,
        spawn_rows,
        command,
        args,
        cwd.or(source_cwd.as_deref().map(std::path::Path::new)),
        Some(PaneIdentity {
            session: session_name,
            pane: new_pane_id,
        }),
        panes,
        config,
    )
    .await?;
    start_pty_forwarding(
        session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        dormant,
    )
    .await;
    Ok(Some(new_pane_id))
}

/// Open `path` in a new split of `client_id`'s attached session, running an
/// editor resolved ON THIS SERVER.
///
/// Server-side resolution is the point rather than an implementation detail:
/// the file is on this machine, so the editor that opens it has to be too. A
/// client-side `$EDITOR` would name whatever the user has installed on the
/// machine they are sitting at, which for a remote session is not where the
/// file is.
///
/// The split itself goes through [`create_pane_in_tab`], the one pane-creation
/// path, so it ejects the tab to `Custom` and clears the zoom exactly as a
/// hand-typed `PaneSplitVertical` does. The refresh tail is this function's,
/// matching the `PaneSplit*` command arm it mirrors.
///
/// A client with no attached session is ignored, as every other pane-creating
/// path ignores it: there is no tab to split.
#[allow(clippy::too_many_arguments)]
async fn handle_open_in_split(
    client_id: u64,
    path: &str,
    command: Option<&str>,
    vertical: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    let (session_name, cols, rows) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => (c.session_name.clone(), c.cols, c.rows),
            None => return Ok(()),
        }
    };
    let Some(session_name) = session_name else {
        log::debug!("server: OpenInSplit from client_id={client_id} with no attached session");
        return Ok(());
    };
    let mut argv = browse::editor_argv(command, path);
    // `editor_argv` always yields at least the program and the file.
    let program = argv.remove(0);
    // The file's own directory, so a relative `:e` inside the editor resolves
    // beside the file rather than beside whatever pane had focus.
    let parent = std::path::Path::new(path).parent().map(|p| p.to_path_buf());
    let placement = if vertical {
        PanePlacement::SplitVertical
    } else {
        PanePlacement::SplitHorizontal
    };
    log::info!(
        "server: OpenInSplit client_id={client_id} session={session_name:?} path={path:?} \
program={program:?} args={argv:?} vertical={vertical}"
    );
    create_pane_in_tab(
        &session_name,
        None,
        placement,
        cols,
        rows,
        Some(&program),
        &argv,
        parent.as_deref(),
        state,
        panes,
        clients,
        config,
        prev_frames,
        dormant,
    )
    .await?;
    resize_session_panes(&session_name, state, panes, clients, config).await?;
    broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
    Ok(())
}

/// Create a pane in `session` on behalf of a command line -- `remux split`,
/// `remux new-tab`.
///
/// The only pane-creating handler that takes its session as an ARGUMENT rather
/// than reading it off the connection, because its sender is a CLI invocation
/// that never attaches: it connects, asks, reads the answer and exits. The
/// caller sends the `REMUX_SESSION` and `REMUX_PANE` the pane's own
/// `Pty::spawn` exported.
///
/// **The SESSION comes from `pane` first.** Both variables were fixed when the
/// caller's pane was spawned, but only the pane id stays true: a pane moved to
/// another session, or a renamed session, keeps its id while the name goes
/// stale. So a live `pane` wins over a disagreeing `session`, and a pane that
/// is absent or dead falls back to `session`, exactly as before. Pane ids are
/// never reused within a server's lifetime, so a dead id cannot name another
/// pane by accident.
///
/// **The SPOT within the session is the active tab's focused pane**, which is
/// what `create_pane_in_tab` already splits when handed no `tab_index`, and not
/// the caller's pane. `REMUX_PANE` is an identity here, never a target: focus
/// is where the user is looking now, and tmux resolves `split-window` the same
/// way.
///
/// A `Some(pane_id)` answer becomes [`ServerMessage::CliSpawned`]; every failure
/// becomes a [`ServerMessage::Error`] that NAMES the session, and the pane when
/// one was sent, since "session not found" without the name explains nothing.
#[allow(clippy::too_many_arguments)]
async fn handle_cli_spawn(
    client_id: u64,
    session: &str,
    pane: Option<PaneId>,
    placement: CliPlacement,
    argv: &[String],
    cwd: Option<&str>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    // An empty argv means the login shell, exactly as an interactive split gets
    // it: `spawn_pane` falls back to the configured default shell, then $SHELL.
    let (command, args) = match argv.split_first() {
        Some((program, rest)) => (Some(program.as_str()), rest),
        None => (None, &[][..]),
    };
    let cwd_path = cwd.map(std::path::Path::new);
    let named = session;
    let resolved = match pane {
        Some(pane) => {
            let st = state.lock().await;
            st.session_of_pane(pane).map(str::to_owned)
        }
        None => None,
    };
    let session = resolved.as_deref().unwrap_or(named);
    // Redacted the same way as the summary line in `handle_client_message`, and
    // for the same reason: `argv` is a command line the user typed, so its
    // ARGUMENTS are where a connection string or an API key lives. The program
    // is what anyone reading this line needs. `cwd` is a path, not a
    // credential, and is the other half of "why did it fail to start".
    log::info!(
        "server: CliSpawn client_id={client_id} session={named:?} pane={pane:?} \
resolved={session:?} placement={placement:?} program={:?} ({} more arg(s)) cwd={cwd:?}",
        argv.first(),
        argv.len().saturating_sub(1)
    );

    let spawned = match placement {
        CliPlacement::NewTab => {
            create_tab_in_session(
                session,
                command,
                args,
                cwd_path,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?
        }
        CliPlacement::SplitRight | CliPlacement::SplitBelow => {
            // The session's own render size, not the caller's: this connection is
            // attached to nothing, so it has no dimensions worth the name. The
            // `resize_session_panes` tail corrects whatever this was anyway.
            let (cols, rows) = session_render_size(session, clients).await;
            // The ONE place the CLI's vocabulary meets the layout engine's, and
            // the mapping looks backwards until you know why: `PanePlacement`
            // names the DIVIDER, `CliPlacement` names where the pane LANDS. A
            // vertical divider puts the new pane to the RIGHT; a horizontal one
            // puts it BELOW. Written out so nobody has to re-derive it, and so
            // nobody "fixes" the apparent mismatch by pairing the like-sounding
            // names.
            let placement = match placement {
                CliPlacement::SplitRight => PanePlacement::SplitVertical,
                _ => PanePlacement::SplitHorizontal,
            };
            let created = create_pane_in_tab(
                session,
                None,
                placement,
                cols,
                rows,
                command,
                args,
                cwd_path,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?;
            if created.is_some() {
                // The refresh tail the `PaneSplit*` arm runs. Without it the pane
                // exists and no attached client repaints -- the split would appear
                // only on the next unrelated redraw.
                resize_session_panes(session, state, panes, clients, config).await?;
                broadcast_full_render(session, state, panes, clients, config, prev_frames).await;
            }
            created
        }
    };

    let cls = clients.lock().await;
    if let Some(conn) = cls.get(&client_id) {
        let msg = match spawned {
            Some(pane_id) => ServerMessage::CliSpawned { pane_id },
            None => ServerMessage::Error {
                req: None,
                message: match pane.filter(|_| resolved.is_none()) {
                    Some(pane) => format!(
                        "no session named '{session}' on this server \
                         (pane {pane} no longer exists)"
                    ),
                    None => format!("no session named '{session}' on this server"),
                },
                code: None,
            },
        };
        let _ = conn.tx.send(msg);
    }
    Ok(())
}

#[cfg(test)]
tokio::task_local! {
    /// Set in a test to make every `spawn_pane` in that task fail, as a real
    /// PTY or fork failure would. Task-local, so tests running in parallel
    /// never see each other's.
    static SPAWN_FAILS: bool;
}

/// Spawn the PTY for `pane_id` and register it.
///
/// `args` is the command's argument vector after argv[0], and is empty for
/// every pane whose command is a shell or a self-contained TUI. It is non-empty
/// for exactly one caller -- the `files` panel's "open this file in an editor"
/// -- and it exists because `Pty::spawn` execs a single-element argv: without
/// it, a `command` of `"vim /tmp/x"` would look for a BINARY of that name rather
/// than running `vim` on the file.
///
/// `identity` is what the child is told it is, as `REMUX_SESSION`/`REMUX_PANE`.
/// A PARAMETER rather than derived from `pane_id` here because only the caller
/// knows what the pane should call itself -- see [`PaneIdentity`].
#[allow(clippy::too_many_arguments)]
async fn spawn_pane(
    pane_id: PaneId,
    cols: u16,
    rows: u16,
    command: Option<&str>,
    args: &[String],
    cwd: Option<&std::path::Path>,
    identity: Option<PaneIdentity<'_>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
) -> Result<()> {
    let cmd = command.or(config.general.default_shell.as_deref());
    // `args` is COUNTED, never printed -- the same redaction as the `CliSpawn`
    // lines this is downstream of, and required for the same reason: the only
    // way user-supplied args reach here is `remux split -- <command line>`, and
    // that is where a connection string or an API key lives. The COMMAND is
    // printed, because "what did it try to run" is the whole diagnostic.
    log::debug!(
        "server: spawn_pane pane_id={pane_id} dims={cols}x{rows} cmd={cmd:?} \
         ({} arg(s)) cwd={cwd:?} identity={identity:?}",
        args.len()
    );
    #[cfg(test)]
    if SPAWN_FAILS.try_with(|fails| *fails).unwrap_or(false) {
        anyhow::bail!("spawn_pane pane_id={pane_id}: injected failure");
    }
    let pty_instance = Pty::spawn(cols, rows, cmd, args, cwd, identity)?;
    // The reader gets its OWN descriptor (a dup), so the fd number cannot be
    // reissued to a later PTY while the reader's epoll registration still
    // exists. See `start_reader`.
    let reader_fd = pty_instance
        .master_fd
        .try_clone()
        .context("failed to duplicate the PTY master for the reader")?;
    let (reader_handle, pty_rx) = pty::start_reader(reader_fd);
    // ...and so does the writer, for the same reason and with the same
    // discipline: an `AsyncFd` must own what it registers.
    let writer_fd = pty_instance
        .master_fd
        .try_clone()
        .context("failed to duplicate the PTY master for the writer")?;
    let (writer_handle, input_tx) = pty::start_writer(writer_fd);
    let screen = Screen::new(cols, rows, config.general.scrollback_lines);

    let mut ps = panes.lock().await;
    ps.insert(
        pane_id,
        PaneData {
            pty: pty_instance,
            screen,
            pty_rx,
            reader: reader_handle,
            input_tx,
            writer: writer_handle,
            forwarding_started: false,
            streamed_session_visible: false,
            last_output: std::time::Instant::now(),
            agent_verdict: None,
            last_human_input: None,
            typing: None,
            title: crate::server::title::TitleTracker::default(),
            names: PaneNames {
                // Until the forwarding loop reads the real name, the program
                // the pane was spawned with.
                shell: cmd
                    .map(str::to_string)
                    .or_else(|| std::env::var("SHELL").ok())
                    .map(|c| crate::config::pane_title::basename(&c).to_string())
                    .unwrap_or_default(),
                ..PaneNames::default()
            },
            exited: false,
        },
    );
    drop(ps);
    PANE_CREATED.notify_one();
    Ok(())
}

async fn resize_session_panes(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) -> Result<()> {
    // Home allotments from the session's active-tab layout (the size the panes
    // get from attached clients viewing this session).
    let rects = active_tab_content_sizes(session_name, state, clients, config).await;
    if rects.is_empty() {
        return Ok(());
    }
    // Whether this session has an attached client viewing it. Its active-tab
    // panes (the `rects` here) are then "session-visible": driven at full size by
    // the real session, so View-cell size demands are IGNORED for them (a cell
    // shows the "Active in session" placeholder instead of shrinking the pane).
    // Only when NO client is attached does a View cell drive a background pane's
    // size (the honest shared-pane reflow), so fold demands in only then.
    let has_client = {
        let cls = clients.lock().await;
        cls.values()
            .any(|c| c.session_name.as_deref() == Some(session_name))
    };
    let sub_mins = if has_client {
        HashMap::new()
    } else {
        // Fold in every View-cell size demand BEFORE touching the panes lock, so
        // we never nest clients under panes.
        let cls = clients.lock().await;
        let mut m: HashMap<PaneId, (u16, u16)> = HashMap::new();
        for (pane_id, _, _) in &rects {
            if let Some(d) = subscriber_min_demand_locked(&cls, *pane_id) {
                m.insert(*pane_id, d);
            }
        }
        m
    };

    {
        let mut ps = panes.lock().await;
        for (pane_id, content_cols, content_rows) in rects {
            if let Some(pane_data) = ps.get_mut(&pane_id) {
                let (mut cols, mut rows) = (content_cols, content_rows);
                if let Some((sc, sr)) = sub_mins.get(&pane_id) {
                    cols = cols.min(*sc);
                    rows = rows.min(*sr);
                }
                let inner_cols = cols.max(1);
                let inner_rows = rows.max(1);
                log::debug!(
                    "resize_session_panes: pane_id={}, content={}x{}, pty/screen resize to cols={} rows={}",
                    pane_id, content_cols, content_rows, inner_cols, inner_rows
                );
                let _ = pane_data.pty.resize(inner_cols, inner_rows);
                pane_data.screen.resize(inner_cols, inner_rows);
            }
        }
    }
    resize_session_popups(session_name, state, panes, clients, config).await;

    // A resize can change active-tab membership / attachment for this or other
    // sessions (tab switch, attach), flipping which subscribed panes are
    // session-visible; re-evaluate every subscribed pane so cells that just
    // gained or lost visibility flip live. Cheap: only live View cells subscribe.
    refresh_subscribed_panes(state, panes, clients, config).await;
    Ok(())
}

/// Size every tab's popup PTY to the session's shared popup interior, so a tab
/// switch never resizes a popup. Returns whether any popup changed size.
///
/// A dedicated step, deliberately NOT folded into `active_tab_content_sizes`:
/// that function means "the active tab's panes" and feeds the View-cell size
/// fold, which the popup (which no View cell can reference) has no business
/// entering.
///
/// A popup already at the shared size is left alone. `Pty::resize` signals
/// SIGWINCH unconditionally, and a terminal resize reaches here twice (the
/// `Resize`, then the `PopupSize` for the new box), so without the check the
/// popup's shell would redraw once for a size it already had.
async fn resize_session_popups(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Config,
) -> bool {
    let popups: Vec<PaneId> = {
        let st = state.lock().await;
        st.sessions
            .get(session_name)
            .map(|s| s.tabs.iter().filter_map(|t| t.popup_pane).collect())
            .unwrap_or_default()
    };
    if popups.is_empty() {
        return false;
    }
    let (inner_cols, inner_rows) = session_popup_interior(session_name, clients, config).await;
    let mut changed = false;
    let mut ps = panes.lock().await;
    for popup_id in popups {
        let Some(pane_data) = ps.get_mut(&popup_id) else {
            continue;
        };
        if (pane_data.screen.cols, pane_data.screen.rows) == (inner_cols, inner_rows) {
            continue;
        }
        log::debug!(
            "resize_session_popups: popup pane_id={popup_id} pty/screen resize to cols={inner_cols} rows={inner_rows}"
        );
        let _ = pane_data.pty.resize(inner_cols, inner_rows);
        pane_data.screen.resize(inner_cols, inner_rows);
        changed = true;
    }
    changed
}

/// Attach `client_id` to `session_name` for a command that names its target,
/// refitting the popups of the session it leaves.
async fn switch_client_session(
    client_id: u64,
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    let left = {
        let mut cls = clients.lock().await;
        cls.get_mut(&client_id).and_then(|client| {
            client
                .session_name
                .replace(session_name.to_string())
                .filter(|old| old != session_name)
        })
    };
    if let Some(left) = left {
        refit_popups_after_leave(&left, state, panes, clients, config, prev_frames).await;
    }
}

/// Bring `session_name`'s popups in line with the clients still attached to it,
/// after one of them left: detached, disconnected or attached elsewhere.
///
/// The shared popup interior is the minimum over attached clients, so the
/// departure of the client that asked for the smallest one grows it. Nothing
/// else would resize the popups until the next resize of a client left behind.
async fn refit_popups_after_leave(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    if resize_session_popups(session_name, state, panes, clients, config).await {
        log::debug!("server: a client left '{session_name}', its popups were resized");
        invalidate_session_baselines(session_name, clients, prev_frames).await;
        broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
    }
}

/// The content (blit) size each pane in a session's active tab gets from the
/// clients attached to that session -- the pane's "home allotment". Empty when
/// the session is unknown, has no active tab, or (via `session_render_size`'s
/// 80x24 fallback) would be meaningless. Border-style aware, mirroring the
/// composite render path so the PTY/screen match the visible content area.
///
/// A stack's hidden panes are included, at the size of the pane the stack
/// shows (see [`layout::compute_stack_member_rects`]), so revealing one never
/// needs a resize. Under zoom only the zoomed pane is listed, and the others
/// keep their unzoomed size, so a command that moves the zoom to another pane
/// must resize (see [`zoom_moved_to_another_pane`]).
async fn active_tab_content_sizes(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Config,
) -> Vec<(PaneId, u16, u16)> {
    let (cols, rows) = session_render_size(session_name, clients).await;
    let st = state.lock().await;
    let sess = match st.sessions.get(session_name) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let tab = match sess.tabs.get(sess.active_tab) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let content_rows = rows.saturating_sub(1);
    let area = Rect {
        x: 0,
        y: 0,
        width: cols,
        height: content_rows,
    };
    // The SAME layout the render path composites with -- under zoom that is a
    // synthetic single-pane stack, so a zoomed *stacked* pane is sized as the
    // single pane it is painted as (asking the real tree here is what left it a
    // row short of its painted area under the tmux border style).
    let effective_layout = tab.effective_layout();
    let pane_rects = layout::compute_stack_member_rects(&effective_layout, area, 0);

    let mut content_rects = Vec::new();
    for &(pane_id, rect) in &pane_rects {
        let content = pane_content_rect(
            &sess.border_style,
            rect,
            is_multi_stack(&effective_layout, pane_id),
        );
        content_rects.push((pane_id, content.width, content.height));
    }
    // A floated pane has the interior of its box, not of its slot. Every other
    // pane keeps the size the real layout gives it.
    if let Some(floated) = tab.float_zoomed() {
        let (_, interior) = float_zoom_box(area, config.zoom.float_pct());
        if let Some(entry) = content_rects.iter_mut().find(|(id, _, _)| *id == floated) {
            *entry = (floated, interior.width, interior.height);
        }
    }
    content_rects
}

/// The minimum `(cols, rows)` demanded across every client's *sized*
/// (`Some`) subscription to `pane_id`, so the pane fits every cell that shows
/// it. `None` when no client demands a size (either unsubscribed, or only
/// watch-only subscriptions). Takes the already-held
/// clients guard to keep lock ordering explicit at the call sites.
fn subscriber_min_demand_locked(
    cls: &HashMap<u64, ClientConnection>,
    pane_id: PaneId,
) -> Option<(u16, u16)> {
    let mut out: Option<(u16, u16)> = None;
    for conn in cls.values() {
        if let Some(Some((c, r))) = conn.subscribed_panes.get(&pane_id) {
            out = Some(match out {
                Some((oc, or)) => (oc.min(*c), or.min(*r)),
                None => (*c, *r),
            });
        }
    }
    out
}

/// Whether `pane_id` is "session-visible": present in the ACTIVE TAB of at least
/// one attached client's session. View-cell subscriptions do NOT count -- they
/// key off `subscribed_panes`, not `session_name`. A session-visible pane is
/// driven at full size by its real session, so a View cell must neither shrink
/// it nor render its streamed content (the cell shows an "Active in session"
/// placeholder instead). This is the message-facing companion of
/// [`pane_home_allotment`]: it uses the same active-tab-membership + attachment
/// test, but is independent of layout rect availability (e.g. under zoom a
/// non-focused active-tab pane has no home rect yet is still session-visible),
/// so the two never disagree about visibility.
async fn pane_session_visible(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> bool {
    let session_name = {
        let st = state.lock().await;
        st.sessions
            .values()
            .find(|sess| {
                sess.tabs
                    .get(sess.active_tab)
                    .map(|t| t.pane_order.contains(&pane_id))
                    .unwrap_or(false)
            })
            .map(|s| s.name.clone())
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return false,
    };
    let cls = clients.lock().await;
    cls.values()
        .any(|c| c.session_name.as_deref() == Some(session_name.as_str()))
}

/// The pane's home allotment (its size from any attached client viewing the
/// session's active tab), or `None` when the pane is not visible on any
/// attached client (background tab, or a session with no attached client). Such
/// a pane has NO home constraint, so a watch-only View cell never reflows it.
async fn pane_home_allotment(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Config,
) -> Option<(u16, u16)> {
    // Which session owns the pane, and does any client have it attached?
    let session_name = {
        let st = state.lock().await;
        let mut found = None;
        for sess in st.sessions.values() {
            if sess
                .tabs
                .get(sess.active_tab)
                .map(|t| t.pane_order.contains(&pane_id))
                .unwrap_or(false)
            {
                found = Some(sess.name.clone());
                break;
            }
        }
        found
    }?;
    let has_client = {
        let cls = clients.lock().await;
        cls.values()
            .any(|c| c.session_name.as_deref() == Some(session_name.as_str()))
    };
    if !has_client {
        return None;
    }
    active_tab_content_sizes(&session_name, state, clients, config)
        .await
        .into_iter()
        .find(|(pid, _, _)| *pid == pane_id)
        .map(|(_, c, r)| (c, r))
}

/// Recompute a single pane's effective size = componentwise min of its home
/// allotment and every sized View-cell demand, then resize the PTY/screen and
/// re-stream a fresh `PaneContent` to subscribers if it changed. When nothing
/// constrains the pane (no home, no sized subscriber) it is left untouched --
/// so merely watching a pane (a hidden cell or a plain observer, `None` demand)
/// never reflows it. Called on subscribe/unsubscribe (the home path uses
/// `resize_session_panes`).
async fn recompute_pane_size(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) {
    // A session-visible pane is driven at full size by its real session, so its
    // View-cell subscriptions are treated as size_demand:false regardless -- the
    // cell shows an "Active in session" placeholder and imposes no constraint.
    let session_visible = pane_session_visible(pane_id, state, clients).await;
    let home = pane_home_allotment(pane_id, state, clients, config).await;
    let sub = if session_visible {
        None
    } else {
        let cls = clients.lock().await;
        subscriber_min_demand_locked(&cls, pane_id)
    };
    // Effective size = home when session-visible (demand ignored), else the
    // componentwise min of home and the sized demand. `None` when nothing
    // constrains the pane (a non-visible watch-only cell, or a session-visible
    // pane with no home rect under zoom) -- leave the pane untouched then.
    let effective = match (home, sub) {
        (Some((hc, hr)), Some((sc, sr))) => Some((hc.min(sc), hr.min(sr))),
        (Some(h), None) => Some(h),
        (None, Some(s)) => Some(s),
        (None, None) => None,
    };
    // Resize only if a size constraint exists and it actually changed, to avoid
    // needless PTY churn / SIGWINCH.
    let resized = if let Some((ec, er)) = effective.map(|(c, r)| (c.max(1), r.max(1))) {
        let mut ps = panes.lock().await;
        match ps.get_mut(&pane_id) {
            Some(pd) if pd.screen.cols != ec || pd.screen.rows != er => {
                log::debug!(
                    "recompute_pane_size: pane_id={pane_id} {}x{} -> {ec}x{er}",
                    pd.screen.cols,
                    pd.screen.rows
                );
                let _ = pd.pty.resize(ec, er);
                pd.screen.resize(ec, er);
                true
            }
            _ => false,
        }
    } else {
        false
    };
    // Detect a session-visibility flip since the last stream, so a tab
    // switch / attach / detach that changes visibility WITHOUT changing size
    // still pushes a fresh PaneContent and the cell flips live.
    let visibility_changed = {
        let mut ps = panes.lock().await;
        match ps.get_mut(&pane_id) {
            Some(pd) => {
                let changed = pd.streamed_session_visible != session_visible;
                pd.streamed_session_visible = session_visible;
                changed
            }
            None => false,
        }
    };
    if resized || visibility_changed {
        // Re-stream the snapshot (with the current session_visible flag) to every
        // subscriber right away, so a cell that reflowed a pane -- or whose pane
        // just flipped visibility -- updates without waiting for the next PTY
        // output.
        stream_pane_content(pane_id, state, panes, clients).await;
    }
}

/// Re-evaluate every currently-subscribed pane's effective size and
/// session-visibility, pushing a fresh `PaneContent` to its subscribers when
/// either changed (via [`recompute_pane_size`]). Called after any event that can
/// change active-tab membership or client attachment (tab switch, attach,
/// detach, disconnect) but does NOT itself resize the pane -- so View cells flip
/// promptly between live content and the "Active in session" placeholder. The
/// subscribed set is only the live View cells, so this is cheap.
async fn refresh_subscribed_panes(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) {
    let pane_ids: Vec<PaneId> = {
        let cls = clients.lock().await;
        let mut set = std::collections::HashSet::new();
        for conn in cls.values() {
            for pid in conn.subscribed_panes.keys() {
                set.insert(*pid);
            }
        }
        set.into_iter().collect()
    };
    for pid in pane_ids {
        recompute_pane_size(pid, state, panes, clients, config).await;
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Compute the consistent render size for a session: the minimum cols and rows
/// over all clients currently attached to it. Falls back to 80x24 if no clients
/// are attached. Every render path for a session composites at this size so a
/// client never mixes differently-sized frames. With a single attached client
/// this equals that client's size (behavior unchanged from a single baseline).
async fn session_render_size(
    session_name: &str,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> (u16, u16) {
    let cls = clients.lock().await;
    let cols = cls
        .values()
        .filter(|c| c.session_name.as_deref() == Some(session_name))
        .map(|c| c.cols)
        .min()
        .unwrap_or(80);
    let rows = cls
        .values()
        .filter(|c| c.session_name.as_deref() == Some(session_name))
        .map(|c| c.rows)
        .min()
        .unwrap_or(24);
    (cols, rows)
}

/// The interior of the framed float a client that never sent
/// [`ClientMessage::PopupSize`] is drawn, for a composite of `cols` x `rows`.
/// The server draws that float itself, so it is also the size that client's
/// popup terminal has to fit.
fn legacy_popup_interior(cols: u16, rows: u16, config: &Config) -> (u16, u16) {
    let area = Rect {
        x: 0,
        y: 0,
        width: cols,
        height: rows.saturating_sub(1),
    };
    let rect = layout::popup_rect(area, legacy_popup_pct(config));
    // The SAME threshold `draw_popup` frames with, so the PTY is never sized
    // to a region that was not painted.
    let (inner_cols, inner_rows) = if fits_zellij_border(rect.width, rect.height) {
        (rect.width - 2, rect.height - 2)
    } else {
        (rect.width, rect.height)
    };
    (inner_cols.max(1), inner_rows.max(1))
}

/// The `(width_pct, height_pct)` a legacy client's float is sized with.
fn legacy_popup_pct(config: &Config) -> (u8, u8) {
    (
        config.appearance.popup_width_pct,
        config.appearance.popup_height_pct,
    )
}

/// The terminal size every popup of a session runs at: the smallest interior
/// any attached client asks for. A client that never asked contributes the
/// interior of its legacy float. With no client attached it falls back to the
/// 80x24 render size, like [`session_render_size`].
fn shared_popup_interior(
    wants: &[Option<(u16, u16)>],
    render_size: (u16, u16),
    config: &Config,
) -> (u16, u16) {
    let legacy = || legacy_popup_interior(render_size.0, render_size.1, config);
    wants
        .iter()
        .map(|want| want.unwrap_or_else(legacy))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
        .unwrap_or_else(legacy)
}

/// [`shared_popup_interior`] over the clients attached to `session_name`.
async fn session_popup_interior(
    session_name: &str,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Config,
) -> (u16, u16) {
    let render_size = session_render_size(session_name, clients).await;
    let wants: Vec<Option<(u16, u16)>> = {
        let cls = clients.lock().await;
        cls.values()
            .filter(|c| c.session_name.as_deref() == Some(session_name))
            .map(|c| c.popup_want)
            .collect()
    };
    shared_popup_interior(&wants, render_size, config)
}

/// Where a pane is, for a View cell's label: its session's and tab's names,
/// the tab's index, and the name the user gave the pane.
#[derive(Debug, Default)]
struct PaneLabels {
    session: String,
    tab: String,
    tab_index: usize,
    custom_name: Option<String>,
}

/// Resolve a pane's [`PaneLabels`] by scanning every session's tabs for the one
/// whose `pane_order` contains `pane_id`. Empty when the pane can't be located
/// (already closed): the client then falls back to `waiting…`.
fn pane_labels(st: &ServerState, pane_id: PaneId) -> PaneLabels {
    for sess in st.sessions.values() {
        for (tab_index, tab) in sess.tabs.iter().enumerate() {
            if tab.panes().contains(&pane_id) {
                return PaneLabels {
                    session: sess.name.clone(),
                    tab: tab.name.clone(),
                    tab_index,
                    custom_name: layout::get_pane_custom_name(&tab.layout, pane_id).flatten(),
                };
            }
        }
    }
    PaneLabels::default()
}

/// Deliver an application's `OSC 52` clipboard write (drained from the pane's
/// screen with [`Screen::take_clipboard`]) to the clients that should act on it,
/// as the same `CopyToClipboard` a Remux yank sends.
///
/// **Routing rule: the pane must be one the user is looking at.** A client gets
/// the write when the pane is the one its own keystrokes would reach — the
/// `input_target` of the session it is attached to — or when it is *showing* the
/// pane in a View cell (a `Some(size)` subscription; `None` is watch-only). A
/// pane in a background tab, in a session nobody has in the foreground, or in a
/// View cell hidden by the layout is skipped: a program left running out of
/// sight must not be able to quietly take over the clipboard. Using
/// `input_target` rather than the raw focused pane also means a pane shadowed by
/// a visible popup does not count as focused, matching where input goes.
///
/// Each matching client is sent exactly one message even if both rules apply.
async fn deliver_app_clipboard(
    pane_id: PaneId,
    data: String,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    // Sessions in which this pane is the current input target. Taken before the
    // clients lock: state-then-clients, never nested the other way.
    let focused_in: Vec<String> = {
        let st = state.lock().await;
        st.sessions
            .iter()
            .filter(|(_, sess)| sess.input_target() == Some(pane_id))
            .map(|(name, _)| name.clone())
            .collect()
    };

    let cls = clients.lock().await;
    for client in cls.values() {
        let attached_and_focused = client
            .session_name
            .as_ref()
            .is_some_and(|name| focused_in.iter().any(|f| f == name));
        let showing_in_view = matches!(client.subscribed_panes.get(&pane_id), Some(Some(_)));
        if attached_and_focused || showing_in_view {
            let _ = client
                .tx
                .send(ServerMessage::CopyToClipboard { data: data.clone() });
        }
    }
}

/// What makes one subscriber's `PaneContent` differ from another's: the
/// scrollback offset it is rendered at, plus its selection flattened to
/// `(start_col, start_row, end_col, end_row)`. Used to memoize the renders in
/// [`stream_pane_content`] so identical viewing states cost one snapshot.
type RenderKey = (usize, Option<(u16, u16, u16, u16)>);

/// Render and send a fresh `PaneContent` for `pane_id` to every client
/// subscribed to it, **rendered per subscriber**.
///
/// Each subscriber sees the pane through its own per-(client, pane) scrollback
/// offset and its own cell drag-selection, so a single shared snapshot is wrong:
/// broadcasting one render at offset 0 is what made a scrolled-back View cell
/// snap to the live tail on the next byte of PTY output. Renders are memoized on
/// `(offset, selection)`, so the common case (all subscribers live, nothing
/// selected) still costs exactly one snapshot.
///
/// A no-op when nothing subscribes -- the snapshot is skipped entirely rather
/// than built and dropped. Acquires the clients, state and panes locks
/// independently (never nested), honoring the codebase's lock ordering.
async fn stream_pane_content(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    // Every subscriber with the viewing state that makes its snapshot differ:
    // its own scrollback offset and its own cell selection. Both are
    // per-(client, pane), so one shared snapshot cannot serve them all -- that
    // was the bug where any PTY output yanked a scrolled-back cell to the live
    // tail, because the fanout rendered once at offset 0 and cloned it.
    let subs: Vec<(u64, usize, Option<MouseSelection>)> = {
        let cls = clients.lock().await;
        cls.iter()
            .filter(|(_, c)| c.subscribed_panes.contains_key(&pane_id))
            .map(|(id, c)| {
                (
                    *id,
                    c.pane_scroll.get(&pane_id).copied().unwrap_or(0),
                    c.pane_selection.get(&pane_id).cloned(),
                )
            })
            .collect()
    };
    if subs.is_empty() {
        return;
    }
    let PaneLabels {
        session: session_name,
        tab: tab_name,
        tab_index,
        custom_name,
    } = {
        let st = state.lock().await;
        pane_labels(&st, pane_id)
    };
    let session_visible = pane_session_visible(pane_id, state, clients).await;

    // Render, then send: the messages are owned, so the panes lock is released
    // before the clients lock is taken again (never nested).
    let outgoing: Vec<(u64, ServerMessage)> = {
        let ps = panes.lock().await;
        let (screen, pane_name) = match ps.get(&pane_id) {
            Some(pd) => (&pd.screen, pane_name_info(pd, custom_name, tab_index)),
            // Pane gone between the subscriber scan and here: nothing to send.
            None => return,
        };
        // A stored offset can outlive the scrollback it pointed into (eviction,
        // or a reflow-shrinking resize), so clamp at render time instead of
        // trying to keep every client's map correct on every mutation.
        let max_off = screen.max_scroll_offset();
        // One render per DISTINCT (offset, selection). The steady state is every
        // subscriber live and unselected, which collapses back to exactly one.
        let mut cache: Vec<(RenderKey, ServerMessage)> = Vec::new();
        let mut out = Vec::with_capacity(subs.len());
        for (cid, off, sel) in subs {
            let off = off.min(max_off);
            let key = (
                off,
                sel.as_ref()
                    .map(|s| (s.start.0, s.start.1, s.end.0, s.end.1)),
            );
            let msg = match cache.iter().find(|(k, _)| *k == key) {
                Some((_, m)) => m.clone(),
                None => {
                    let snap = crate::server::compositor::render_pane_snapshot_selected(
                        screen,
                        off,
                        sel.as_ref(),
                    );
                    let m = ServerMessage::PaneContent {
                        pane_id,
                        cols: snap.cols,
                        rows: snap.rows,
                        cells: snap.cells,
                        cursor_x: snap.cursor_x,
                        cursor_y: snap.cursor_y,
                        cursor_visible: snap.cursor_visible,
                        application_cursor_keys: snap.application_cursor_keys,
                        bracketed_paste: snap.bracketed_paste,
                        session_name: session_name.clone(),
                        tab_name: tab_name.clone(),
                        session_visible,
                        pane_name: Some(pane_name.clone()),
                    };
                    cache.push((key, m.clone()));
                    m
                }
            };
            out.push((cid, msg));
        }
        out
    };
    let cls = clients.lock().await;
    for (cid, msg) in outgoing {
        if let Some(conn) = cls.get(&cid) {
            let _ = conn.tx.send(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Shared view helpers
// ---------------------------------------------------------------------------

/// Build the `ViewList` message that mirrors the current shared-view registry.
/// Pure over a locked `ServerState`; the full [`LayoutMode`]/`custom_tree`
/// travel so a client can composite without an extra round trip.
fn build_view_list_message(st: &ServerState) -> ServerMessage {
    let views = st
        .views
        .iter()
        .map(|v| ViewInfo {
            id: v.id,
            name: v.name.clone(),
            cells: v
                .cells
                .iter()
                .map(|c| CellInfo {
                    id: c.id,
                    conn: c.conn.clone(),
                    pane_id: c.pane_id,
                })
                .collect(),
            layout: v.layout.clone(),
            custom_tree: v.custom_tree.clone(),
            focused: v.focused,
            zoomed: v.zoomed,
        })
        .collect();
    ServerMessage::ViewList { views }
}

/// Broadcast the full current `ViewList` to EVERY connected client, so the
/// shared registry stays consistent across all terminals. Called after every
/// view mutation. Locks `state` to snapshot the message, then `clients` to fan
/// out — never nested, matching the codebase lock ordering.
async fn broadcast_view_list(
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    let msg = {
        let st = state.lock().await;
        build_view_list_message(&st)
    };
    let cls = clients.lock().await;
    for conn in cls.values() {
        let _ = conn.tx.send(msg.clone());
    }
}

/// A reference area for view geometry (the `find_neighbor` search in
/// `ViewMoveCell`): the componentwise min terminal size across all connected
/// clients (fallback 80x24), minus one row for the view's status bar — matching
/// the client's `cells_area` convention. See the Phase-2 TODO on
/// [`ServerState::view_move_cell`]: a shared view ultimately needs its own
/// canonical area rather than one derived from the live client population.
async fn view_reference_area(clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>) -> Rect {
    let cls = clients.lock().await;
    let cols = cls.values().map(|c| c.cols).min().unwrap_or(80);
    let rows = cls.values().map(|c| c.rows).min().unwrap_or(24);
    Rect {
        x: 0,
        y: 0,
        width: cols,
        height: rows.saturating_sub(1),
    }
}

/// Refresh a target session after a structural mutation performed on behalf of
/// a client that may be attached to a *different* session (or to none) -- e.g.
/// the session manager editing a session it is not viewing.
///
/// If the target session has any attached clients, this resizes its active
/// tab's panes to those clients' dimensions, invalidates their render
/// baselines, and broadcasts a fresh full render. When no client is attached,
/// there is nothing to display, so it is a no-op (the mutation is still
/// persisted by the caller via `save_if_enabled`). Mirrors the
/// `target_dims.is_some()` guard used by the `TabCloseByIndex` handler.
async fn refresh_target_session(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let has_clients = {
        let cls = clients.lock().await;
        cls.values()
            .any(|c| c.session_name.as_deref() == Some(session_name))
    };
    if !has_clients {
        return Ok(());
    }
    resize_session_panes(session_name, state, panes, clients, config).await?;
    invalidate_session_baselines(session_name, clients, prev_frames).await;
    broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
    Ok(())
}

/// Invalidate the previous-frame baselines of every client attached to a
/// session by removing their `client_id` entries. Each affected client then
/// receives a fresh FULL render on its next frame (instead of a diff against a
/// stale, possibly wrong-size baseline). Used on attach/resize/tab changes.
///
/// Collects the client ids under the `clients` lock, drops it, then locks
/// `prev_frames` — this avoids ever holding both locks nested, keeping lock
/// ordering uniform with the render paths and preventing deadlock.
async fn invalidate_session_baselines(
    session_name: &str,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    prev_frames: &PrevFrameCache,
) {
    let ids: Vec<u64> = {
        let cls = clients.lock().await;
        cls.iter()
            .filter(|(_, c)| c.session_name.as_deref() == Some(session_name))
            .map(|(id, _)| *id)
            .collect()
    };
    let mut pf = prev_frames.lock().await;
    for id in ids {
        pf.remove(&id);
    }
}

/// Record the popup interior a client wants and bring the popup PTYs and every
/// affected client in line with it.
///
/// The popup terminal is sized to the smallest interior any attached client
/// asks for, so this only resizes when the shared interior actually changed.
/// The sender is ALWAYS answered with a full render from a cleared baseline,
/// even when nothing changed on the server: the sender's box may have moved,
/// and it has masked away stale content under the old box that only a full
/// frame repaints.
async fn handle_popup_size(
    client_id: u64,
    (cols, rows): (u16, u16),
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let (cols, rows) = clamp_client_size("PopupSize", cols, rows);
    let session_name = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => c.session_name.clone(),
            None => return Ok(()),
        }
    };
    let before = match &session_name {
        Some(name) => Some(session_popup_interior(name, clients, config).await),
        None => None,
    };
    {
        let mut cls = clients.lock().await;
        if let Some(c) = cls.get_mut(&client_id) {
            c.popup_want = Some((cols, rows));
        }
    }
    log::debug!("server: PopupSize client_id={client_id} want={cols}x{rows}");
    let Some(session_name) = session_name else {
        return Ok(());
    };
    let after = session_popup_interior(&session_name, clients, config).await;
    if before != Some(after) {
        resize_session_panes(&session_name, state, panes, clients, config).await?;
        // Every client's popup layer changed size, which each answers with a
        // full frame of its own. The sender is one of them, so this is its
        // answer too, unless the broadcast skipped it (it does so for a client
        // whose input pane is scrolled). Every frame sent stores a baseline, so
        // a sender still without one was not answered.
        invalidate_session_baselines(&session_name, clients, prev_frames).await;
        broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        if prev_frames.lock().await.contains_key(&client_id) {
            return Ok(());
        }
    } else {
        prev_frames.lock().await.remove(&client_id);
    }
    send_full_render_to_client(
        client_id,
        &session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        RenderCtx::default(),
    )
    .await;
    Ok(())
}

/// What the caller of [`send_full_render_to_client`] already knows, so the
/// render can skip work the caller has done or afford a cheaper message.
///
/// `RenderCtx::default()` is "assume nothing": refresh the pane names and send
/// whole cells. Most callers use it, because for them this is a one-shot
/// repaint where neither saving pays for a claim that could be wrong.
#[derive(Clone, Copy, Default)]
struct RenderCtx {
    /// Send a `RenderDiff` when the client's saved baseline still describes its
    /// screen. Set by the two paths that repaint a client the shared broadcast
    /// skipped: the per-client broadcast, which runs once per PTY output chunk
    /// for as long as a non-focused pane stays scrolled, and
    /// [`repaint_scrolled_requester`], whose command often changes a border and
    /// nothing else.
    ///
    /// Safe on any path, because a baseline is only ever stored as a frame the
    /// client was actually sent. It is left off elsewhere because those callers
    /// gain nothing measurable from it.
    diff_against_baseline: bool,
    /// The caller has already run [`update_auto_pane_names`] for this pass.
    /// That function sweeps `/proc` for every pane in the tab, so running it
    /// twice for one frame doubles the syscalls it costs.
    names_are_fresh: bool,
    /// The client's `needs_full_render`, when the caller consumed the flag
    /// instead of leaving it. A diff against a baseline the flag distrusts
    /// would leave whatever the client has on screen that the baseline does not
    /// describe.
    force_full: bool,
}

#[allow(clippy::too_many_arguments)]
async fn send_full_render_to_client(
    client_id: u64,
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    ctx: RenderCtx,
) {
    // Render at the consistent session size (min over all attached clients),
    // not the caller's own dimensions, so this client's frames never mix sizes
    // with the shared broadcast path. With a single client this equals the
    // caller's size, so single-client behavior is unchanged.
    let (cols, rows) = session_render_size(session_name, clients).await;
    log::debug!(
        "server: send_full_render_to_client client_id={client_id} session={session_name:?} dims={cols}x{rows}"
    );
    let (mode, selection, client_search_info, client_scroll, naming, theme, asking) = {
        let cls = clients.lock().await;
        let client = cls.get(&client_id);
        let mode = client
            .map(|c| c.mode.clone())
            .unwrap_or_else(|| "NORMAL".to_string());
        let selection = client.and_then(|c| c.mouse_selection.clone());
        let si = client.and_then(|c| c.search_info);
        let scroll = client.map(|c| c.session_scroll.clone()).unwrap_or_default();
        let naming = client
            .map(ClientConnection::naming)
            .unwrap_or_else(Naming::server);
        let theme = client
            .map(|c| c.theme(config))
            .unwrap_or_else(|| config.compositor_theme());
        let asking = client.is_some_and(|c| c.popup_want.is_some());
        (mode, selection, si, scroll, naming, theme, asking)
    };
    // The pane that owns input is the one the wire's single `scroll_offset`
    // describes: the client gates its `ScrollReset` on that field, and the
    // reset it then asks for applies to the pane its keystrokes reach. It is
    // also the pane a `ScrollRender` delta shifts, and the one whose scroll
    // hides the cursor.
    let focused_pane = {
        let st = state.lock().await;
        st.sessions
            .get(session_name)
            .and_then(Session::input_target)
    };
    // Clamped exactly as `build_composite` clamps the map it renders, so the
    // field cannot report a position the frame does not show.
    let scroll_offset = match focused_pane {
        Some(fp) => {
            let ps = panes.lock().await;
            let max = ps
                .get(&fp)
                .map(|p| p.screen.max_scroll_offset())
                .unwrap_or(0);
            client_scroll.get(&fp).copied().unwrap_or(0).min(max)
        }
        None => 0,
    };
    // Update auto-detected pane names before rendering.
    if !ctx.names_are_fresh {
        update_auto_pane_names(session_name, state, panes).await;
    }
    let (
        cells,
        cursor_x,
        cursor_y,
        cursor_visible,
        cursor_style,
        focused_pane_rect,
        _hit_regions,
        _pane_rects,
        application_cursor_keys,
        bracketed_paste,
        mut popup_layer,
        status,
    ) = build_composite(
        session_name,
        cols,
        rows,
        &mode,
        state,
        panes,
        config,
        selection.as_ref(),
        client_search_info,
        &client_scroll,
        &theme,
        &naming,
    )
    .await;
    // A scrolled pane that owns input has no live cursor to show.
    let cursor_visible = if scroll_offset > 0 {
        if let Some(layer) = popup_layer.as_mut() {
            layer.cursor_visible = false;
        }
        false
    } else {
        cursor_visible
    };
    let placed = place_popup(
        &cells,
        (cursor_x, cursor_y, cursor_visible, cursor_style),
        focused_pane_rect,
        popup_layer.as_ref(),
        asking,
        (cols, rows),
        &mode,
        &theme,
        config,
    );

    // Compute viewport_top: the scrollback line index of the first displayed line.
    let viewport_top = {
        if let Some(fp) = focused_pane {
            let ps = panes.lock().await;
            ps.get(&fp)
                .map(|p| {
                    if scroll_offset == 0 {
                        // Not scrolled: blit_screen fast path reads from grid[0]
                        // which is line_at(scrollback.len())
                        p.screen.scrollback.len()
                    } else {
                        // Scrolled: blit_screen_scrolled computes
                        // view_top = total - scroll_offset - pane_h
                        let total = p.screen.total_lines();
                        let pane_h = placed
                            .layer
                            .map(|l| l.rows as usize)
                            .or(placed.focused_pane_rect.map(|r| r.height as usize))
                            .unwrap_or(rows.saturating_sub(1) as usize);
                        total.saturating_sub(scroll_offset).saturating_sub(pane_h)
                    }
                })
                .unwrap_or(0)
        } else {
            0
        }
    };

    // Decide under the `clients` lock and release it before `prev_frames`:
    // the two are never held together anywhere in this file, which is what
    // keeps this path from deadlocking against `invalidate_session_baselines`.
    let plan = {
        let mut cls = clients.lock().await;
        let Some(client) = cls.get_mut(&client_id) else {
            return;
        };
        // A `ScrollRender` tells the client to shift the pixels it already has
        // by a delta. Across a change of focused pane the two offsets belong to
        // different panes, so their difference describes no movement at all and
        // would shift one pane's rows by the other's scroll.
        let same_pane = client.prev_scroll_pane == focused_pane;
        let prev_so = if same_pane {
            client.prev_scroll_offset
        } else {
            0
        };
        client.prev_scroll_offset = scroll_offset;
        client.prev_scroll_pane = focused_pane;

        // Detect incremental scroll for ScrollRender optimization.
        let delta = scroll_offset as i64 - prev_so as i64;
        let abs_delta = delta.unsigned_abs() as usize;

        let use_scroll_render = scroll_render_allowed(
            same_pane,
            scroll_offset,
            prev_so,
            viewport_top,
            abs_delta,
            &placed,
        );

        log::debug!(
            "server: render decision client_id={client_id} scroll_offset={scroll_offset} prev_so={prev_so} delta={delta} use_scroll_render={use_scroll_render}"
        );
        (client.tx.clone(), use_scroll_render, delta, abs_delta)
    };
    let (tx, use_scroll_render, delta, abs_delta) = plan;

    if use_scroll_render {
        // `use_scroll_render` already required a rect, so this branch cannot be
        // reached without one.
        if let Some(fpr) = placed.focused_pane_rect {
            let cells = &placed.cells;
            let (cursor_x, cursor_y, cursor_visible, cursor_style) = (
                placed.cursor_x,
                placed.cursor_y,
                placed.cursor_visible,
                placed.cursor_style,
            );
            let px = fpr.x as usize;
            let py = fpr.y as usize;
            let pw = fpr.width as usize;
            let ph = fpr.height as usize;

            let new_rows: Vec<Vec<RenderCell>> = if delta > 0 {
                // Scrolled UP (deeper into history) — new rows at TOP of pane
                (0..abs_delta)
                    .map(|i| {
                        let y = py + i;
                        if y < cells.len() && px + pw <= cells[y].len() {
                            cells[y][px..px + pw].to_vec()
                        } else {
                            vec![RenderCell::default(); pw]
                        }
                    })
                    .collect()
            } else {
                // Scrolled DOWN (toward live view) — new rows at BOTTOM of pane
                (0..abs_delta)
                    .map(|i| {
                        let y = py + ph - abs_delta + i;
                        if y < cells.len() && px + pw <= cells[y].len() {
                            cells[y][px..px + pw].to_vec()
                        } else {
                            vec![RenderCell::default(); pw]
                        }
                    })
                    .collect()
            };

            let _ = tx.send(ServerMessage::ScrollRender {
                pane_x: fpr.x,
                pane_y: fpr.y,
                pane_width: fpr.width,
                pane_height: fpr.height,
                delta: delta as i16,
                new_rows,
                cursor_x,
                cursor_y,
                cursor_visible,
                cursor_style,
                focused_pane_rect: Some(fpr),
                application_cursor_keys,
                bracketed_paste,
                viewport_top,
                scroll_offset,
            });
            // A `ScrollRender` repaints the focused pane's rect only, so the
            // rest of this client's screen is still the previous frame. Saving
            // `cells` as the baseline would claim otherwise. The baseline is
            // left as it was, which is the last whole frame this client got.
            return;
        }
    }

    // Read before `prev_frames` is locked, to keep the lock order of the render
    // paths: never `state` under `prev_frames`.
    let live_popups = session_popups(session_name, state).await;
    let float_zoom = session_float_zoom(session_name, state).await;
    // The baseline must equal what this client has on screen, because the next
    // broadcast diffs against it. A `FullRender` and a `RenderDiff` both leave
    // the client showing exactly what `frame_message` stores, so both save it.
    let mut pf = prev_frames.lock().await;
    let baseline = if ctx.diff_against_baseline && !ctx.force_full {
        pf.get(&client_id)
    } else {
        None
    };
    let meta = FrameMeta {
        application_cursor_keys,
        bracketed_paste,
        viewport_top,
        scroll_offset,
        status,
        live_popups,
        float_zoom,
    };
    let (msg, stored) = frame_message(placed, &meta, baseline, &format!("client_id={client_id}"));
    let _ = tx.send(msg);
    pf.insert(client_id, stored);
}

async fn broadcast_full_render(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    let (cols, rows, mode, selection, si, client_count, naming, theme) = {
        let cls = clients.lock().await;
        let attached: Vec<_> = cls
            .values()
            .filter(|c| c.session_name.as_deref() == Some(session_name))
            .collect();
        if attached.is_empty() {
            return;
        }
        let count = attached.len();
        let cols = attached.iter().map(|c| c.cols).min().unwrap_or(80);
        let rows = attached.iter().map(|c| c.rows).min().unwrap_or(24);
        // Use the mode and selection from the first attached client.
        let first = attached.first();
        let mode = first
            .map(|c| c.mode.clone())
            .unwrap_or_else(|| "NORMAL".to_string());
        let selection = first.and_then(|c| c.mouse_selection.clone());
        let si = first.and_then(|c| c.search_info);
        // The shared frame names panes and caps tabs the way the first client
        // does. A client that differs in either is composited on its own, below.
        let naming = first.map(|c| c.naming()).unwrap_or_else(Naming::server);
        let theme = first
            .map(|c| c.theme(config))
            .unwrap_or_else(|| config.compositor_theme());
        (cols, rows, mode, selection, si, count, naming, theme)
    };

    log::debug!("server: broadcast_full_render session={session_name:?} clients={client_count}");

    // Update auto-detected pane names before rendering.
    update_auto_pane_names(session_name, state, panes).await;

    let (
        cells,
        cursor_x,
        cursor_y,
        cursor_visible,
        cursor_style,
        focused_pane_rect,
        _hit_regions,
        pane_rects,
        application_cursor_keys,
        bracketed_paste,
        popup_layer,
        status,
    ) = build_composite(
        session_name,
        cols,
        rows,
        &mode,
        state,
        panes,
        config,
        selection.as_ref(),
        si,
        &HashMap::new(),
        &theme,
        &naming,
    )
    .await;

    // Compute viewport_top for live view (scroll_offset=0): first displayed line index.
    let viewport_top = {
        let st = state.lock().await;
        let fp = st
            .sessions
            .get(session_name)
            .and_then(|s| s.tabs.get(s.active_tab))
            .map(|t| t.focused_pane);
        drop(st);
        if let Some(fp) = fp {
            let ps = panes.lock().await;
            ps.get(&fp)
                .map(|p| {
                    // Live view (offset=0): blit reads from grid[0] = line_at(scrollback.len())
                    p.screen.scrollback.len()
                })
                .unwrap_or(0)
        } else {
            0
        }
    };

    // What this client can SEE scrolled. An offset on a pane the frame does not
    // draw changes nothing in it, so it must not cost that client the shared
    // frame.
    //
    // Taken from the rects `build_composite` just returned rather than from the
    // layout tree, which makes that true BY CONSTRUCTION instead of by a list
    // of exceptions: `compute_layout` emits one entry per pane it gave area to,
    // so a zoom contributes only the zoomed pane and a stack only its active
    // one. Reading the tree with `all_pane_ids` gets the zoom right and the
    // stack wrong, and a pane buried in a stack then costs its client the
    // shared frame for a byte-identical result.
    let visible_panes: Vec<PaneId> = pane_rects
        .iter()
        .map(|(id, _)| *id)
        .chain(popup_layer.as_ref().map(|layer| layer.pane_id))
        .collect();
    let input_target = {
        let st = state.lock().await;
        st.sessions
            .get(session_name)
            .and_then(Session::input_target)
    };

    // The composite `cells` is identical for every live client (all at the
    // session render size), so it is computed once above. The diff, however, is
    // per-client: each client is diffed against *its own* baseline so a client
    // whose size differs from another's never diffs against a poisoned frame.
    //
    // Collect the eligible clients and their senders under the `clients` lock,
    // then drop it before locking `prev_frames`. This keeps lock ordering
    // uniform (never `clients`+`prev_frames` nested) and avoids deadlock
    // against `invalidate_session_baselines`.
    //
    // Three outcomes, because a scroll now belongs to a pane rather than to the
    // client:
    //
    // * The pane that owns input is scrolled: skipped, exactly as before. This
    //   is also the PTY-output path, so repainting here would let a second
    //   pane's output drag the reader of this one to the bottom.
    // * Some other visible pane is scrolled: the shared frame would paint that
    //   pane live and destroy the position, so this client is composited on its
    //   own, below, and diffed against its own baseline.
    // * Nothing visible is scrolled: the shared frame, as before.
    //
    // KNOWN AND ACCEPTED, and the two cases differ: a scrolled FOCUSED pane
    // freezes the whole of that client's screen, because it is skipped here
    // entirely and nothing else repaints it. A scrolled NON-focused pane keeps
    // repainting, but at its offset FROM THE BOTTOM, so what it shows drifts
    // upward as that pane's own output grows. tmux pins history to a line
    // instead of to a distance from the tail. Do not fix either one here.
    //
    // The offsets are compared CLAMPED, the way `build_composite` renders them.
    // An entry can outlive the scrollback it addressed, and the one that does
    // so silently is a pane that entered an alternate-screen application after
    // being scrolled: its max is 0 while the entry is not, so an unclamped test
    // would route its client through the per-client path for a frame identical
    // to the shared one, and keep doing it until somebody scrolled that pane
    // again.
    let visible_max: HashMap<PaneId, usize> = {
        let ps = panes.lock().await;
        visible_panes
            .iter()
            .filter_map(|id| ps.get(id).map(|p| (*id, p.screen.max_scroll_offset())))
            .collect()
    };
    let clamped_scroll = |c: &ClientConnection, pane_id: PaneId| -> usize {
        let max = visible_max.get(&pane_id).copied().unwrap_or(0);
        c.scroll_of(pane_id).min(max)
    };

    let mut per_client: Vec<(u64, bool)> = Vec::new();
    let targets: Vec<(u64, mpsc::UnboundedSender<ServerMessage>, bool, bool)> = {
        let mut cls = clients.lock().await;
        let mut shared = Vec::new();
        for (id, c) in cls.iter_mut() {
            if c.session_name.as_deref() != Some(session_name) {
                continue;
            }
            if input_target.is_some_and(|fp| clamped_scroll(c, fp) != 0) {
                continue;
            }
            let force_full = c.needs_full_render;
            c.needs_full_render = false; // consume it
            if visible_panes.iter().any(|p| clamped_scroll(c, *p) != 0)
                || c.naming() != naming
                || c.tab_style(config) != theme.tab_style
            {
                per_client.push((*id, force_full));
            } else {
                shared.push((*id, c.tx.clone(), force_full, c.popup_want.is_some()));
            }
        }
        shared
    };

    // As in `send_full_render_to_client`: read before `prev_frames` is locked.
    let live_popups = session_popups(session_name, state).await;
    let float_zoom = session_float_zoom(session_name, state).await;
    let mut pf = prev_frames.lock().await;
    for (cid, tx, force_full, asking) in targets {
        // Force a full render when this client has no baseline yet, or when its
        // baseline's dimensions differ from the current frame (a size change,
        // e.g. after another client of a different size attached/detached).
        // compute_diff never clears cells that exist only in a larger prev
        // frame, so a diff across a size change would leave stale content.
        // A client that just returned from scrolling must get a full repaint;
        // its diff baseline may not match its on-screen state.
        let baseline = if force_full { None } else { pf.get(&cid) };
        let placed = place_popup(
            &cells,
            (cursor_x, cursor_y, cursor_visible, cursor_style),
            focused_pane_rect,
            popup_layer.as_ref(),
            asking,
            (cols, rows),
            &mode,
            &theme,
            config,
        );
        // Only live-tail clients are targeted here (see the `scroll_offset == 0`
        // filter above).
        let meta = FrameMeta {
            application_cursor_keys,
            bracketed_paste,
            viewport_top,
            scroll_offset: 0,
            status: status.clone(),
            live_popups: live_popups.clone(),
            float_zoom,
        };
        let (msg, stored) = frame_message(
            placed,
            &meta,
            baseline,
            &format!("broadcast client_id={cid}"),
        );
        let _ = tx.send(msg);
        pf.insert(cid, stored);
    }
    // Released before the per-client renders: `send_full_render_to_client`
    // takes this lock itself.
    drop(pf);

    for (cid, force_full) in per_client {
        send_full_render_to_client(
            cid,
            session_name,
            state,
            panes,
            clients,
            config,
            prev_frames,
            RenderCtx {
                diff_against_baseline: true,
                names_are_fresh: true,
                force_full,
            },
        )
        .await;
    }
}

/// Refresh the name of every pane in the session's active tab that the user
/// has not renamed, from [`auto_pane_name`].
///
/// Also marks the session tree dirty, but ONLY when a name actually changed.
/// This runs on every render and every mouse event, so notifying
/// unconditionally would be a push storm that the coalescing would merely hide;
/// a process name changing (a shell becoming `vim`) is a real change to the
/// pane labels a subscriber's tree displays, and is rare.
async fn update_auto_pane_names(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) {
    let mut st = state.lock().await;
    let sess = match st.sessions.get_mut(session_name) {
        Some(s) => s,
        None => return,
    };
    let tab_index = sess.active_tab;
    let tab = match sess.tabs.get_mut(tab_index) {
        Some(t) => t,
        None => return,
    };

    // Collect pane IDs that need auto-detected names.
    let pane_ids = layout::all_pane_ids(&tab.layout);
    let ps = panes.lock().await;

    // Skip the pane being actively renamed -- its name is managed by the rename flow.
    let renaming_pane = sess.rename_state.as_ref().map(|(pid, _)| *pid);

    // The names kept in the layout are the server's own. They feed the
    // tree-dirty check below and are replaced per client when a frame is
    // composited (see `build_composite`).
    let server_naming = Naming::server();
    let mut changed = false;
    for pane_id in pane_ids {
        if renaming_pane == Some(pane_id) {
            continue;
        }
        // Only update if there's no custom name set.
        let custom = layout::get_pane_custom_name(&tab.layout, pane_id);
        if custom == Some(None) || custom.is_none() {
            // No custom name -- auto-detect from process.
            if let Some(pane_data) = ps.get(&pane_id) {
                let name = auto_pane_name(pane_data, session_name, tab_index, &server_naming);
                if layout::get_pane_name(&tab.layout, pane_id).as_deref() != Some(name.as_str()) {
                    changed = true;
                }
                layout::set_pane_name(&mut tab.layout, pane_id, &name);
            }
        }
    }

    // Synchronous, so calling it under the `state`/`panes` guards still held
    // here is safe -- an async broadcast would relock them and deadlock.
    if changed {
        mark_session_tree_dirty();
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_composite(
    session_name: &str,
    cols: u16,
    rows: u16,
    mode: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    selection: Option<&MouseSelection>,
    search_info: Option<(usize, usize)>,
    // This client's per-pane scroll offsets, unclamped. Empty for a caller that
    // only wants the geometry, which does not depend on them.
    client_scroll: &HashMap<PaneId, usize>,
    compositor_theme: &crate::config::theme::CompositorTheme,
    // How the client this frame is for names its panes.
    naming: &Naming,
) -> (
    Vec<Vec<RenderCell>>,
    u16,
    u16,
    bool,
    u8,
    Option<PaneRect>,
    HitRegions,
    Vec<(PaneId, Rect)>,
    bool, // application_cursor_keys
    bool, // bracketed_paste
    // The active tab's visible popup, rendered interior-local. It is NOT drawn
    // into the frame: the cursor, the focused pane rect and `pane_rects` all
    // describe the layout as if no popup were up, and [`place_popup`] puts the
    // layer over the frame per client.
    Option<PopupLayer>,
    // What the status bar row shows. `None` only when there is no session or
    // tab to describe, which is also when the frame is blank.
    Option<StatusInfo>,
) {
    let st = state.lock().await;
    let sess = match st.sessions.get(session_name) {
        Some(s) => s,
        None => {
            return (
                vec![vec![RenderCell::default(); cols as usize]; rows as usize],
                0,
                0,
                false,
                0,
                None,
                HitRegions::default(),
                Vec::new(),
                // No pane to read an input mode off; not a claim about one.
                false,
                false,
                None,
                None,
            );
        }
    };
    let tab = match sess.tabs.get(sess.active_tab) {
        Some(t) => t,
        None => {
            return (
                vec![vec![RenderCell::default(); cols as usize]; rows as usize],
                0,
                0,
                false,
                0,
                None,
                HitRegions::default(),
                Vec::new(),
                // No pane to read an input mode off; not a claim about one.
                false,
                false,
                None,
                None,
            );
        }
    };

    let content_rows = rows.saturating_sub(1);
    let area = Rect {
        x: 0,
        y: 0,
        width: cols,
        height: content_rows,
    };

    let ps = panes.lock().await;
    let mut pane_screens: HashMap<PaneId, &Screen> = HashMap::new();
    let mut effective_layout = tab.effective_layout();
    // The names the layout holds are rendered with the server's template, so
    // the ones the user did not set are rendered again for this client. The
    // pane being renamed keeps what the rename flow put there.
    let renaming_pane = sess.rename_state.as_ref().map(|(pid, _)| *pid);
    for pane_id in layout::all_pane_ids(&effective_layout) {
        if renaming_pane == Some(pane_id)
            || matches!(
                layout::get_pane_custom_name(&tab.layout, pane_id),
                Some(Some(_))
            )
        {
            continue;
        }
        if let Some(pd) = ps.get(&pane_id) {
            let name = auto_pane_name(pd, session_name, sess.active_tab, naming);
            if layout::get_pane_name(&effective_layout, pane_id).as_deref() != Some(&name) {
                layout::set_pane_name(effective_layout.to_mut(), pane_id, &name);
            }
        }
    }
    let mut pane_rects = layout::compute_layout(&effective_layout, area, 0);
    for (pane_id, _rect) in &pane_rects {
        if let Some(pane_data) = ps.get(pane_id) {
            pane_screens.insert(*pane_id, &pane_data.screen);
        }
    }

    // A float zoom draws the focused pane over the layout, in a box of its own.
    // The pane keeps its slot in the layout, which shows an empty frame in the
    // colour of an unfocused pane: the pane's screen has the size of the box,
    // not of the slot, so it cannot fill the slot.
    let float = tab
        .float_zoomed()
        .filter(|id| ps.contains_key(id))
        .map(|id| (id, float_zoom_box(area, config.zoom.float_pct())));
    let placeholder = float.and_then(|(id, _)| {
        let (_, slot) = pane_rects.iter().find(|(pid, _)| *pid == id)?;
        let content = pane_content_rect(
            &sess.border_style,
            *slot,
            is_multi_stack(&effective_layout, id),
        );
        Some((
            id,
            Screen::new(content.width.max(1), content.height.max(1), 0),
        ))
    });
    let layout_screens = match &placeholder {
        Some((id, blank)) => {
            let mut screens = pane_screens.clone();
            screens.insert(*id, blank);
            std::borrow::Cow::Owned(screens)
        }
        None => std::borrow::Cow::Borrowed(&pane_screens),
    };

    let status_info = StatusInfo {
        mode: mode.to_string(),
        session_name: session_name.to_string(),
        tabs: sess
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let name = if i == sess.active_tab && t.zoomed_pane.is_some() {
                    format!("{} Z", t.name)
                } else {
                    t.name.clone()
                };
                (name, i == sess.active_tab, t.activity)
            })
            .collect(),
        layout_mode: tab.layout_mode.name().to_string(),
        search_info,
    };

    // The visible popup (if any), resolved once.
    let popup = tab.visible_popup().filter(|id| ps.contains_key(id));

    // The pane that owns input: the popup while it is up, else the focused one.
    let input_pane = popup.unwrap_or(tab.focused_pane);

    // Clamp here rather than on every mutation of the client's map: an offset
    // can outlive the scrollback it addressed (eviction, or a resize whose
    // reflow shortens the history), and render time is the one place that
    // always sees the current bound.
    let scroll_offsets: HashMap<PaneId, usize> = client_scroll
        .iter()
        .filter_map(|(pane_id, offset)| {
            // The popup is not in the layout, so it is not in `pane_screens`.
            let max = pane_screens
                .get(pane_id)
                .copied()
                .or_else(|| ps.get(pane_id).map(|p| &p.screen))
                .map(Screen::max_scroll_offset)?;
            let clamped = (*offset).min(max);
            (clamped > 0).then_some((*pane_id, clamped))
        })
        .collect();

    // No layout pane wears the focused frame while a float zoom does, and the
    // layout's selection highlight must not land in the empty slot: the float
    // highlights its own selection.
    let (layout_focus, layout_selection) = match float {
        Some((id, _)) => (NO_FOCUSED_PANE, selection.filter(|sel| sel.pane_id != id)),
        None => (tab.focused_pane, selection),
    };
    let (mut cells, mut hit_regions) = composite(
        &effective_layout,
        &layout_screens,
        area,
        &sess.border_style,
        &status_info,
        cols,
        rows,
        0,
        layout_focus,
        layout_selection,
        &scroll_offsets,
        compositor_theme,
    );

    if let Some((id, (rect, _))) = float {
        if let Some(pane_data) = ps.get(&id) {
            let layer = render_popup_layer(
                id,
                &pane_data.screen,
                scroll_offsets.get(&id).copied().unwrap_or(0),
                selection,
            );
            let name = layout::get_pane_name(&effective_layout, id)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| id.to_string());
            draw_popup(&mut cells, rect, &layer, &name, mode, compositor_theme);
        }
        // The box covers stack labels, and a press on one must not activate a
        // pane hidden behind the float.
        hit_regions.stack_regions.retain(|r| {
            !(r.y >= rect.y
                && r.y < rect.y + rect.height
                && r.x_end > rect.x
                && r.x_start < rect.x + rect.width)
        });
        hit_regions.float = Some((id, rect));
    }

    // The popup layer is a separate buffer, NOT a post-pass over `cells`: a
    // client that places the popup itself draws it over its own sidebars, which
    // the composite never contains. No pane rect changes, so the popup steals no
    // space and zoom state is untouched.
    let popup_layer = popup.and_then(|popup_id| {
        let popup_screen = &ps.get(&popup_id)?.screen;
        Some(render_popup_layer(
            popup_id,
            popup_screen,
            scroll_offsets.get(&popup_id).copied().unwrap_or(0),
            selection,
        ))
    });

    // If there is an active rename, place the cursor in the pane's border
    // at the end of the typed text instead of inside the shell content.
    let rename_cursor = sess.rename_state.as_ref().and_then(|(rename_pane_id, _)| {
        // Only ZellijStyle has visible borders where we can position the cursor.
        if !matches!(sess.border_style, BorderStyle::ZellijStyle) {
            return None;
        }
        pane_rects
            .iter()
            .find(|(id, _)| id == rename_pane_id)
            .map(|(_, rect)| {
                let name_len = layout::get_pane_name(&tab.layout, *rename_pane_id)
                    .unwrap_or_default()
                    .len() as u16;
                // Cursor goes after "╭ " + name text = x + 1 (corner) + 1 (space) + name_len
                let cx = rect.x + 2 + name_len;
                let cy = rect.y;
                (cx, cy, true)
            })
    });

    // Compute border offsets for the focused pane (shared by rect and cursor).
    let focused_rect_and_offsets = pane_rects
        .iter()
        .find(|(id, _)| *id == tab.focused_pane)
        .map(|(_, rect)| {
            let (x_off, y_off, x_off_end, y_off_end) = match &sess.border_style {
                BorderStyle::ZellijStyle => {
                    if fits_zellij_border(rect.width, rect.height) {
                        (1u16, 1u16, 1u16, 1u16) // 1-cell border on each side
                    } else {
                        (0, 0, 0, 0)
                    }
                }
                BorderStyle::TmuxStyle => {
                    let has_tab_bar = layout::find_stack_for_pane(&tab.layout, tab.focused_pane)
                        .map(|panes| panes.len() > 1)
                        .unwrap_or(false);
                    if has_tab_bar {
                        (0, 1, 0, 0) // tab bar takes 1 row at top
                    } else {
                        (0, 0, 0, 0)
                    }
                }
            };
            (rect, x_off, y_off, x_off_end, y_off_end)
        });

    // The focused pane's rect for the client (content area, excluding borders),
    // as if no popup were up. `place_popup` replaces it while a popup is.
    //
    // `tab.layout`, not `effective_layout`, because the `PaneFocus*` handler
    // walks the unzoomed tree: a zoomed pane fills the area, but the handler
    // still moves focus (and the zoom) to its real neighbours.
    let focused_pane_rect = if let Some((_, (_, interior))) = float {
        // The floated pane is the focused one. A `PaneFocus*` releases the
        // float before it steps, so a direction with no pane is still the
        // server's to answer, and the client must not read the float as the
        // edge of the screen.
        Some(PaneRect {
            x: interior.x,
            y: interior.y,
            width: interior.width,
            height: interior.height,
            neighbors: FocusNeighbors {
                left: true,
                right: true,
                up: true,
                down: true,
            },
        })
    } else {
        let has = |dir| layout::focus_target(&tab.layout, area, tab.focused_pane, dir, 0).is_some();
        let neighbors = FocusNeighbors {
            left: has(layout::FocusDirection::Left),
            right: has(layout::FocusDirection::Right),
            up: has(layout::FocusDirection::Up),
            down: has(layout::FocusDirection::Down),
        };
        focused_rect_and_offsets.map(|(rect, x_off, y_off, x_off_end, y_off_end)| PaneRect {
            x: rect.x + x_off,
            y: rect.y + y_off,
            width: rect.width.saturating_sub(x_off + x_off_end),
            height: rect.height.saturating_sub(y_off + y_off_end),
            neighbors,
        })
    };

    // Cursor of the layout: an in-progress rename ahead of the focused pane.
    let (cursor_x, cursor_y, cursor_visible, cursor_style) = if let Some(rc) = rename_cursor {
        (rc.0, rc.1, rc.2, 0u8)
    } else if let Some((id, (_, interior))) = float {
        match ps.get(&id) {
            Some(pane_data) => (
                interior.x
                    + pane_data
                        .screen
                        .cursor_x
                        .min(interior.width.saturating_sub(1)),
                interior.y
                    + pane_data
                        .screen
                        .cursor_y
                        .min(interior.height.saturating_sub(1)),
                pane_data.screen.cursor_visible,
                pane_data.screen.cursor_style,
            ),
            None => (0, 0, false, 0),
        }
    } else if let Some(pane_data) = ps.get(&tab.focused_pane) {
        if let Some((rect, x_off, y_off, x_off_end, y_off_end)) = focused_rect_and_offsets {
            let content_w = rect.width.saturating_sub(x_off + x_off_end);
            let content_h = rect.height.saturating_sub(y_off + y_off_end);
            (
                rect.x
                    + x_off
                    + std::cmp::min(pane_data.screen.cursor_x, content_w.saturating_sub(1)),
                rect.y
                    + y_off
                    + std::cmp::min(pane_data.screen.cursor_y, content_h.saturating_sub(1)),
                pane_data.screen.cursor_visible,
                pane_data.screen.cursor_style,
            )
        } else {
            (0, 0, false, 0)
        }
    } else {
        (0, 0, false, 0)
    };

    // DECCKM follows input, so it comes from the popup while it owns input.
    let application_cursor_keys = ps
        .get(&input_pane)
        .map(|p| p.screen.application_cursor_keys)
        .unwrap_or(false);
    // Bracketed paste is an input mode too, so the same rule applies: a paste
    // goes wherever the keystrokes go, which is the popup while it owns input.
    let bracketed_paste = ps
        .get(&input_pane)
        .map(|p| p.screen.bracketed_paste)
        .unwrap_or(false);

    // The rect the floated pane answers mouse events at is its box. Its slot
    // is empty, and a press there is outside the float.
    if let Some((id, (rect, _))) = float {
        if let Some(entry) = pane_rects.iter_mut().find(|(pid, _)| *pid == id) {
            entry.1 = rect;
        }
    }

    (
        cells,
        cursor_x,
        cursor_y,
        cursor_visible,
        cursor_style,
        focused_pane_rect,
        hit_regions,
        pane_rects,
        application_cursor_keys,
        bracketed_paste,
        popup_layer,
        Some(status_info),
    )
}

/// A frame as one client receives it, after [`place_popup`].
struct PlacedFrame<'a> {
    /// The cells the client is sent and diffed against. Borrowed from the shared
    /// composite unless the server had to draw a popup into them.
    cells: std::borrow::Cow<'a, [Vec<RenderCell>]>,
    cursor_x: u16,
    cursor_y: u16,
    cursor_visible: bool,
    cursor_style: u8,
    focused_pane_rect: Option<PaneRect>,
    /// The popup layer, set only for a client that places the popup itself.
    /// A client that does not has the popup drawn into `cells` instead.
    layer: Option<&'a PopupLayer>,
}

/// What a frame carries besides its cells, the same for every client.
struct FrameMeta {
    application_cursor_keys: bool,
    bracketed_paste: bool,
    viewport_top: usize,
    scroll_offset: usize,
    status: Option<StatusInfo>,
    live_popups: Vec<PaneId>,
    float_zoom: bool,
}

/// Every popup pane `session_name` has, on any tab, visible or not.
async fn session_popups(session_name: &str, state: &Arc<Mutex<ServerState>>) -> Vec<PaneId> {
    let st = state.lock().await;
    st.sessions
        .get(session_name)
        .map(|s| s.tabs.iter().filter_map(|t| t.popup_pane).collect())
        .unwrap_or_default()
}

/// Whether `session_name`'s active tab shows a float zoom.
async fn session_float_zoom(session_name: &str, state: &Arc<Mutex<ServerState>>) -> bool {
    let st = state.lock().await;
    st.sessions
        .get(session_name)
        .and_then(|s| s.tabs.get(s.active_tab))
        .is_some_and(|t| t.float_zoomed().is_some())
}

/// Put the popup into the frame for one client.
///
/// `cursor` and `focused_pane_rect` are the layout's, as if no popup were up.
///
/// * No popup: the frame is untouched and borrowed.
/// * A client that never sent `PopupSize` (`asking == false`): the server draws
///   the full-framed float into a copy of the frame, at the rect the config's
///   percentages give, and reports the popup's interior and cursor instead of
///   the layout's. The copy is what gets diffed and stored, so the baseline
///   stays equal to what this client has on screen.
/// * A client that asked: the frame stays as it is and the layer travels
///   beside it. The client owns the cursor and the focused pane rect while it
///   holds a layer, so they are reported as hidden and absent.
#[allow(clippy::too_many_arguments)]
fn place_popup<'a>(
    cells: &'a [Vec<RenderCell>],
    cursor: (u16, u16, bool, u8),
    focused_pane_rect: Option<PaneRect>,
    layer: Option<&'a PopupLayer>,
    asking: bool,
    (cols, rows): (u16, u16),
    mode: &str,
    theme: &CompositorTheme,
    config: &Config,
) -> PlacedFrame<'a> {
    let (cursor_x, cursor_y, cursor_visible, cursor_style) = cursor;
    let Some(layer) = layer else {
        return PlacedFrame {
            cells: std::borrow::Cow::Borrowed(cells),
            cursor_x,
            cursor_y,
            cursor_visible,
            cursor_style,
            focused_pane_rect,
            layer: None,
        };
    };
    if asking {
        return PlacedFrame {
            cells: std::borrow::Cow::Borrowed(cells),
            cursor_x,
            cursor_y,
            cursor_visible: false,
            cursor_style,
            focused_pane_rect: None,
            layer: Some(layer),
        };
    }
    let mut drawn = cells.to_vec();
    let area = Rect {
        x: 0,
        y: 0,
        width: cols,
        height: rows.saturating_sub(1),
    };
    let rect = layout::popup_rect(area, legacy_popup_pct(config));
    let interior = draw_popup(&mut drawn, rect, layer, "popup", mode, theme);
    PlacedFrame {
        cells: std::borrow::Cow::Owned(drawn),
        cursor_x: interior.x + layer.cursor_x.min(interior.width.saturating_sub(1)),
        cursor_y: interior.y + layer.cursor_y.min(interior.height.saturating_sub(1)),
        cursor_visible: layer.cursor_visible,
        cursor_style: layer.cursor_style,
        focused_pane_rect: Some(PaneRect {
            x: interior.x,
            y: interior.y,
            width: interior.width,
            height: interior.height,
            // `PaneFocus*` closes the popup rather than moving inside it, so
            // the popup has no neighbour in any direction.
            neighbors: FocusNeighbors::default(),
        }),
        layer: None,
    }
}

/// Whether a scroll step can go out as a `ScrollRender`, which tells the client
/// to shift the pixels it has by a delta, instead of a whole frame.
///
/// The client's `render_scroll` is a no-op when `abs_delta` reaches the pane's
/// height (it expects the caller to fall back to a full repaint), so the delta
/// must be strictly smaller than the focused pane's content height.
///
/// At the very top of history (`viewport_top == 0`, i.e. the scroll offset has
/// reached its maximum so the first scrollback line is the top visible row) a
/// `FullRender` is forced. That repaints the client authoritatively at the
/// scroll boundary instead of relying on an accumulated series of incremental
/// shifts to have landed the earliest lines exactly right.
///
/// Never while the popup owns input for a client that places it itself: the
/// shift would scroll the layout pane underneath the layer. A client the server
/// draws the popup for keeps the incremental scroll, because the popup is then
/// part of its frame.
fn scroll_render_allowed(
    same_pane: bool,
    scroll_offset: usize,
    prev_scroll_offset: usize,
    viewport_top: usize,
    abs_delta: usize,
    placed: &PlacedFrame<'_>,
) -> bool {
    same_pane
        && placed.layer.is_none()
        && scroll_offset > 0
        && prev_scroll_offset > 0
        && viewport_top > 0
        && abs_delta > 0
        && abs_delta <= 10
        && placed
            .focused_pane_rect
            .is_some_and(|r| abs_delta < r.height as usize)
}

/// The wire form of a popup layer: all of it when `prev` is `None`, else only
/// what changed against the layer the client already holds.
fn popup_cells(layer: &PopupLayer, prev: Option<&[Vec<RenderCell>]>) -> Box<PopupCells> {
    let changes = prev.map(|prev| compute_diff(prev, &layer.cells));
    let threshold = layer.cols as usize * layer.rows as usize / 2;
    let (cells, changes) = match changes {
        Some(changes) if changes.len() <= threshold => (None, changes),
        _ => (Some(layer.cells.clone()), Vec::new()),
    };
    Box::new(PopupCells {
        pane_id: layer.pane_id,
        cols: layer.cols,
        rows: layer.rows,
        cursor_x: layer.cursor_x,
        cursor_y: layer.cursor_y,
        cursor_visible: layer.cursor_visible,
        cursor_style: layer.cursor_style,
        cells,
        changes,
    })
}

/// The message that brings a client from `baseline` to `placed`, and the
/// baseline to store once it is sent.
///
/// A diff is only sent when it can be exact: the baseline must exist and match
/// in size, and the popup layer must neither appear, disappear nor change size.
/// `compute_diff` never clears cells that exist only in a larger previous
/// frame, and a layer transition repaints the region the client masked away
/// under the box, so each of those sends a `FullRender`.
fn frame_message(
    placed: PlacedFrame<'_>,
    meta: &FrameMeta,
    baseline: Option<&PrevFrame>,
    log_tag: &str,
) -> (ServerMessage, PrevFrame) {
    let cells = &placed.cells;
    let layer_size = placed.layer.map(|l| (l.cols, l.rows));
    let usable = baseline.filter(|prev| {
        let same_size = prev.cells.len() == cells.len()
            && prev.cells.first().map(Vec::len) == cells.first().map(Vec::len);
        let same_layer = prev.popup.as_ref().map(|(size, _)| *size) == layer_size;
        same_size && same_layer
    });
    let threshold = cells.len() * cells.first().map(Vec::len).unwrap_or(0) / 2;
    let diff = usable.and_then(|prev| {
        let changes = compute_diff(&prev.cells, cells);
        if changes.len() > threshold {
            log::debug!(
                "server: {log_tag} render=Full (diff {} changes > threshold {threshold})",
                changes.len()
            );
            None
        } else {
            log::debug!(
                "server: {log_tag} render=Diff ({} changes, threshold {threshold})",
                changes.len()
            );
            Some((prev, changes))
        }
    });
    let msg = match diff {
        Some((prev, changes)) => ServerMessage::RenderDiff {
            changes,
            cursor_x: placed.cursor_x,
            cursor_y: placed.cursor_y,
            cursor_visible: placed.cursor_visible,
            cursor_style: placed.cursor_style,
            focused_pane_rect: placed.focused_pane_rect,
            application_cursor_keys: meta.application_cursor_keys,
            bracketed_paste: meta.bracketed_paste,
            viewport_top: meta.viewport_top,
            scroll_offset: meta.scroll_offset,
            status: meta.status.clone(),
            live_popups: meta.live_popups.clone(),
            float_zoom: meta.float_zoom,
            popup: placed.layer.map(|layer| {
                popup_cells(
                    layer,
                    prev.popup.as_ref().map(|(_, cells)| cells.as_slice()),
                )
            }),
        },
        None => {
            if usable.is_none() {
                log::debug!(
                    "server: {log_tag} render=Full (no baseline, size changed or popup layer changed)"
                );
            }
            ServerMessage::FullRender {
                cells: cells.to_vec(),
                cursor_x: placed.cursor_x,
                cursor_y: placed.cursor_y,
                cursor_visible: placed.cursor_visible,
                cursor_style: placed.cursor_style,
                focused_pane_rect: placed.focused_pane_rect,
                application_cursor_keys: meta.application_cursor_keys,
                bracketed_paste: meta.bracketed_paste,
                viewport_top: meta.viewport_top,
                scroll_offset: meta.scroll_offset,
                status: meta.status.clone(),
                popup: placed.layer.map(|layer| popup_cells(layer, None)),
                live_popups: meta.live_popups.clone(),
                float_zoom: meta.float_zoom,
            }
        }
    };
    let stored = PrevFrame {
        cells: placed.cells.into_owned(),
        popup: placed.layer.map(|l| ((l.cols, l.rows), l.cells.clone())),
    };
    (msg, stored)
}

fn compute_diff(prev: &[Vec<RenderCell>], curr: &[Vec<RenderCell>]) -> Vec<CellChange> {
    let mut changes = Vec::new();
    for (y, row) in curr.iter().enumerate() {
        let prev_row = prev.get(y);
        for (x, cell) in row.iter().enumerate() {
            let prev_cell = prev_row.and_then(|r| r.get(x));
            if prev_cell != Some(cell) {
                // If this is a continuation cell (width 0) following a wide lead,
                // also repaint the lead so the wide glyph covers this column.
                // (The client skips width-0 changes, so a lone continuation change
                // would otherwise leave half a stale glyph.) Only push the lead
                // when it did not already differ this pass, to avoid duplicates.
                if cell.width == 0 && x > 0 {
                    if let Some(lead) = row.get(x - 1) {
                        if lead.width == 2 {
                            let prev_lead = prev_row.and_then(|r| r.get(x - 1));
                            if prev_lead == Some(lead) {
                                changes.push(CellChange {
                                    x: (x - 1) as u16,
                                    y: y as u16,
                                    cell: lead.clone(),
                                });
                            }
                        }
                    }
                }
                changes.push(CellChange {
                    x: x as u16,
                    y: y as u16,
                    cell: cell.clone(),
                });
            }
        }
    }
    changes
}

// ---------------------------------------------------------------------------
// Pane close helper
// ---------------------------------------------------------------------------

/// Close a pane, updating layout and session state. If the pane is the last
/// pane in its tab, the tab is closed. If the last tab closes, the session is
/// left empty.
/// Handle the after-effects of a session being removed (its last tab/pane was
/// closed and the session no longer exists in `state`).
///
/// Picks the next available session; if one exists, every client that was
/// attached to `removed` is re-pointed onto it and given a fresh full render.
/// If no sessions remain, those clients are detached and notified with a
/// `SessionDeleted` event so they can fall back or exit.
///
/// This is the shared post-removal logic used by pane close, tab close, and
/// close-tab-by-index.
async fn handle_session_removed(
    removed: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    let next_session = {
        let st = state.lock().await;
        st.sessions.keys().next().cloned()
    };
    match next_session {
        Some(next) => {
            // Switch all clients that were on the removed session to the next one.
            {
                let mut cls = clients.lock().await;
                for c in cls.values_mut() {
                    if c.session_name.as_deref() == Some(removed) {
                        c.session_name = Some(next.clone());
                        // As in `handle_attach`: these offsets index panes of
                        // the session being left.
                        c.session_scroll.clear();
                        c.prev_scroll_offset = 0;
                        c.prev_scroll_pane = None;
                    }
                }
            }
            // Clients now display a different session; invalidate their
            // baselines (from the old session) so they get a clean full render.
            invalidate_session_baselines(&next, clients, prev_frames).await;
            let _ = resize_session_panes(&next, state, panes, clients, config).await;
            broadcast_full_render(&next, state, panes, clients, config, prev_frames).await;
        }
        None => {
            // No sessions left -- notify affected clients so they disconnect
            // (or fall back to another server).
            let mut cls = clients.lock().await;
            for c in cls.values_mut() {
                if c.session_name.as_deref() == Some(removed) {
                    c.session_name = None;
                    let _ = c.tx.send(ServerMessage::Event(SessionEvent::SessionDeleted(
                        removed.to_string(),
                    )));
                }
            }
        }
    }
    // Either way the clients that were on `removed` now have a different pane
    // in front of them, or none.
    mark_focus_changed();
}

/// `exit_code` reported with [`SessionEvent::PaneExited`] when the pane's real
/// exit status is not observable: it was closed by command while its child was
/// still running, or it had already been reaped when a client asked about it.
/// Real codes are `0..=255` (a signalled child reports `128 + signo`), so this
/// can never collide with one.
const EXIT_CODE_UNKNOWN: i32 = -1;

/// Tell every client subscribed to one of `exits` that the pane is gone,
/// dropping the subscription in the same step.
///
/// This is the notification half of pane death. Without it a View cell aliasing
/// the pane cannot tell a dead pane from a quiet one: it sits on `waiting for …`
/// (or a frozen last snapshot painted as if it were live) forever, and every
/// keystroke typed into it vanishes. The event goes ONLY to clients that
/// actually held a subscription -- they are the ones who were being lied to --
/// and the send is fused with the subscription drop so a subscriber can never be
/// forgotten before it is told.
async fn notify_panes_exited(
    exits: &[(PaneId, i32)],
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    if exits.is_empty() {
        return;
    }
    let mut cls = clients.lock().await;
    for conn in cls.values_mut() {
        for &(pane_id, exit_code) in exits {
            conn.pane_scroll.remove(&pane_id);
            conn.session_scroll.remove(&pane_id);
            conn.pane_selection.remove(&pane_id);
            if conn.subscribed_panes.remove(&pane_id).is_some() {
                let _ = conn.tx.send(ServerMessage::Event(SessionEvent::PaneExited {
                    pane_id,
                    exit_code,
                }));
            }
            // A gesture on a pane that just went away can never make progress;
            // drop it (and its repeat timer) so the ticker stops replaying it.
            if conn.pane_drag.as_ref().map(|d| d.pane_id) == Some(pane_id) {
                conn.pane_drag = None;
                conn.pane_autoscroll_repeat = None;
            }
        }
    }
}

/// Drop `pane_ids` from the pane table and notify their subscribers.
///
/// Every path that destroys a pane (shell exit, `PaneClose`, `PaneCloseById`,
/// tab close, session/folder delete, the popup) funnels through here, so the
/// `PaneExited` notification cannot be forgotten by a new close path.
async fn reap_panes(
    pane_ids: &[PaneId],
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    if pane_ids.is_empty() {
        return;
    }
    // Read each child's status before dropping its `PaneData` (dropping the
    // `Pty` SIGHUPs the child). A pane closed by command still has a live child
    // and reports `EXIT_CODE_UNKNOWN`; nothing consumes the code today, it is
    // carried for the client's benefit.
    let exits: Vec<(PaneId, i32)> = {
        let mut ps = panes.lock().await;
        pane_ids
            .iter()
            .map(|&pane_id| {
                let code = match ps.remove(&pane_id) {
                    Some(pd) => pd
                        .pty
                        .try_wait()
                        .ok()
                        .flatten()
                        .unwrap_or(EXIT_CODE_UNKNOWN),
                    // Already reaped by another path -- still notify, a client
                    // may be holding a subscription to it.
                    None => EXIT_CODE_UNKNOWN,
                };
                (pane_id, code)
            })
            .collect()
    };
    // Taken under `clients` alone (the panes lock above is released) to keep the
    // locks unnested.
    notify_panes_exited(&exits, clients).await;
    {
        let mut store = messaging().lock().await;
        for &pane_id in pane_ids {
            for waiter in store.take_waiters_for(pane_id) {
                log::info!(
                    "msg: wait on %{pane_id} ended: the pane closed (client_id={})",
                    waiter.client_id
                );
                let _ = waiter.tx.send(ServerMessage::refused(
                    "no_such_target",
                    format!("pane %{pane_id} closed while waiting"),
                ));
            }
            let orphans = store.take_submits_from(pane_id);
            if !orphans.is_empty() {
                log::info!(
                    "msg: pane %{pane_id} closed with {} of its message(s) waiting to be typed \
                     elsewhere; they stay in the mailbox",
                    orphans.len()
                );
            }
            for entry in orphans {
                let resolved = Resolved {
                    host: None,
                    pane_id: entry.pane,
                    name: store.name_of(entry.pane).map(str::to_string),
                };
                let refusal =
                    crate::server::submit::SubmitRefusal::sender_closed(pane_id, entry.id);
                let answer = crate::server::submit::settle(entry.deliver, refusal);
                let message = answer_submit(entry.id, resolved, answer, None);
                send_reply(entry.id, entry.reply, message);
            }
            for wait in store.take_state_waits(pane_id) {
                log::info!(
                    "agent: client_id={} wait on %{pane_id} ended: the pane closed",
                    wait.reply.client_id
                );
                wait.reply.send(ServerMessage::refused(
                    "no_such_target",
                    format!("pane %{pane_id} closed while waited on"),
                ));
            }
            let name = store.name_of(pane_id).map(str::to_string);
            for wait in store.take_submit_waits(pane_id) {
                answer_wait_ended(
                    wait,
                    name.clone(),
                    std::time::Instant::now(),
                    "pane_closed_after_submit",
                );
            }
            let (queued, replies) = store.take_submits(pane_id);
            if queued > 0 {
                log::info!(
                    "msg: pane %{pane_id} closed with {queued} message(s) waiting to be typed"
                );
            }
            for reply in replies {
                reply.send(ServerMessage::refused(
                    "no_such_target",
                    format!("pane %{pane_id} closed while its message was queued"),
                ));
            }
            let (dropped, name) = store.drop_pane(pane_id);
            if dropped > 0 || name.is_some() {
                log::info!(
                    "msg: pane %{pane_id} closed; dropped {dropped} message(s){}",
                    name.map(|name| format!(", name {}", loggable(&name)))
                        .unwrap_or_default()
                );
            }
        }
    }
    // Every path that destroys a pane funnels through here, so this is the one
    // place an agent that has GONE has to be noticed. Nothing else would: the
    // pusher is woken by output, and a dead pane produces none.
    mark_agents_dirty();
}

/// Run the NOTIFICATION half of pane death for a pane whose PTY exited but that
/// [`close_pane`] declined to close.
///
/// `close_pane` only knows the session's ACTIVE tab, so a pane living in a
/// background tab is left in the pane table with a dead PTY. Views exist to
/// watch exactly such panes, so their subscribers must still be told -- a cell
/// must not sit on a frozen snapshot just because its pane's tab wasn't in the
/// foreground. Layout state is deliberately untouched here (the pane staying in
/// its tab's `pane_order` is a separate, pre-existing bug); this only stops the
/// lying. A no-op when `close_pane` did reap the pane.
async fn notify_if_close_declined(
    pane_id: PaneId,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    let declined = {
        let ps = panes.lock().await;
        ps.contains_key(&pane_id)
    };
    if declined {
        log::info!(
            "server: pane_id={pane_id} PTY exited but close_pane declined it; notifying subscribers"
        );
        // Deliberately NOT `try_wait`: the `Pty` stays in the table here (that is
        // what "declined" means), and reaping the child would leave `Pty::drop`
        // to SIGHUP -- and `get_pane_cwd` to read /proc for -- a pid the OS may
        // have recycled by then. Nothing consumes the code.
        notify_panes_exited(&[(pane_id, EXIT_CODE_UNKNOWN)], clients).await;
    }
}

/// Take the focused pane of `sess`'s active tab out of its stack into its own
/// slot on the `direction` side. `PaneUnstack*` and a `PaneMoveLeft`/`Right`
/// that reaches the end of its stack both come here, so they leave the tab in
/// the same state.
fn unstack_focused(sess: &mut Session, direction: layout::FocusDirection, context: &str) {
    let Some(tab) = sess.tabs.get_mut(sess.active_tab) else {
        return;
    };
    // Custom for the reason `PaneStackInto*` gives: the next
    // automatic rebuild would otherwise re-stack or re-flow the
    // panes and undo the split.
    if layout::unstack_pane(&mut tab.layout, tab.focused_pane, direction) {
        tab.layout_mode = LayoutMode::Custom(CustomLayout);
        tab.zoomed_pane = None;
        session::debug_check_invariant(sess, context);
    }
}

/// The pane `PaneMoveToTabTarget` moves: the attached session's active tab's
/// focused pane, resolved when the command arrives because focus is where the
/// user is now.
fn move_source_pane(st: &ServerState, session_name: &str) -> Option<PaneId> {
    let pane = st
        .sessions
        .get(session_name)
        .and_then(|s| s.tabs.get(s.active_tab))
        .map(|t| t.focused_pane);
    if pane.is_none() {
        log::info!(
            "server: PaneMoveToTabTarget refused: session {session_name:?} has no active tab"
        );
    }
    pane
}

async fn close_pane(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    /// What to do after closing a pane.
    enum CloseAction {
        /// Normal close -- broadcast a full render for the current session.
        Broadcast,
        /// Last pane/tab in the session was closed and the session was removed
        /// from `state`; run the shared post-removal logic (switch clients to
        /// another session, or disconnect them if none remain).
        SessionRemoved,
        /// Nothing to do (pane not found, etc.).
        NoBroadcast,
    }

    // Panes to reap beyond `pane_id` itself (the popup, when the whole session
    // goes away -- it is in no layout tree, so nothing else would reclaim it).
    let mut also_reap: Vec<PaneId> = Vec::new();

    // The session is looked up here, under the lock that also closes the pane,
    // rather than taken from the caller: a session the caller read under an
    // earlier lock can be stale by now, because the pane may have moved to
    // another session in between. A pane in no session is already being reaped
    // by whatever removed it.
    let (session_name, action) = {
        let mut st = state.lock().await;
        let Some(session_name) = st.session_of_pane(pane_id).map(str::to_owned) else {
            return;
        };
        let sess = match st.sessions.get_mut(&session_name) {
            Some(s) => s,
            None => return,
        };

        // The popup pane first: it is session-scoped and lives in NO tab, so the
        // layout path below would bail out on it (`pane_order` never contains
        // it) and leave a dead PTY behind with the popup still "open". This is
        // both the `PaneClose`-while-popup-open path and the popup shell's own
        // exit path, and it must work from any tab.
        // A hidden popup's shell can exit while another tab is active, so the
        // owner is looked up among ALL tabs.
        let popup_owner = sess.tabs.iter().position(|t| t.popup_pane == Some(pane_id));
        let action = if let Some(owner) = popup_owner {
            sess.tabs[owner].take_popup();
            log::debug!("server: close_pane closed popup pane_id={pane_id}");
            CloseAction::Broadcast
        } else {
            let tab = match sess.tabs.get_mut(sess.active_tab) {
                Some(t) => t,
                None => return,
            };

            // Check if this pane actually belongs to the current tab.
            if !tab.pane_order.contains(&pane_id) {
                return;
            }

            if let Some(nf) = tab.detach_pane(pane_id) {
                // `nf` is what the tree shows in the closed pane's place, not a
                // focus decision: only the focused pane closing moves focus.
                if tab.focused_pane == pane_id {
                    tab.focus_pane(nf);
                }
                tab.rebuild_if_automatic();
                session::debug_check_invariant(sess, "close_pane");
                CloseAction::Broadcast
            } else {
                // Last pane in the tab was closed. Close the tab.
                let tab_idx = sess.active_tab;
                if sess.tabs.len() > 1 {
                    also_reap.extend(sess.tabs.remove(tab_idx).take_popup());
                    if sess.active_tab >= sess.tabs.len() {
                        sess.active_tab = sess.tabs.len().saturating_sub(1);
                    }
                    CloseAction::Broadcast
                } else {
                    // Last tab in the session -- remove the session entirely,
                    // with its folder entry and its popup.
                    also_reap.extend(st.remove_emptied_session(&session_name));
                    CloseAction::SessionRemoved
                }
            }
        };
        (session_name, action)
    };
    // Drop the pane(s) and TELL their subscribers. A subscription to a dead pane
    // must not linger, and a subscriber dropped without a word is exactly the
    // silent failure that left a View cell on `waiting…` forever.
    let mut reaped = Vec::with_capacity(1 + also_reap.len());
    reaped.push(pane_id);
    reaped.extend(also_reap);
    reap_panes(&reaped, panes, clients).await;
    match action {
        CloseAction::Broadcast => {
            let _ = resize_session_panes(&session_name, state, panes, clients, config).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        CloseAction::SessionRemoved => {
            handle_session_removed(&session_name, state, panes, clients, config, prev_frames).await;
        }
        CloseAction::NoBroadcast => {}
    }
}

// ---------------------------------------------------------------------------
// PTY forwarding
// ---------------------------------------------------------------------------

async fn start_pty_forwarding(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) {
    let pane_ids = {
        let st = state.lock().await;
        let sess = match st.sessions.get(session_name) {
            Some(s) => s,
            None => return,
        };
        let tab = match sess.tabs.get(sess.active_tab) {
            Some(t) => t,
            None => return,
        };
        let mut ids = layout::all_pane_ids(&tab.layout);
        // The popup pane is in no layout tree, so it would never get a
        // forwarding task (and never show output) unless added here. Doing it
        // here keeps every existing call site correct; the `forwarding_started`
        // guard below makes the repeat safe.
        ids.extend(tab.popup_pane);
        ids
    };

    log::debug!("server: start_pty_forwarding session={session_name:?} pane_ids={pane_ids:?}");

    for pane_id in pane_ids {
        start_forwarding_for_pane(
            pane_id,
            session_name.to_string(),
            state,
            panes,
            clients,
            config,
            prev_frames,
            dormant,
        )
        .await;
    }
}

/// Start the one forwarding task for `pane_id`: pump its PTY into its `Screen`,
/// stream the result to subscribers, and handle the pane's death.
///
/// Each batch re-composites the frame of the session that holds the pane, and
/// the pane's death runs the full `close_pane` path (layout repair,
/// persistence, the tree push) in that session. Both ask
/// [`ServerState::session_of_pane`] at the time of the event, because a pane
/// can be moved to another session while this task runs. `session_name` is
/// the session the pane was spawned in: it names the pane in the log, and it
/// is the fallback for a render when the pane is in no session.
///
/// It used to be an `Option`, `None` meaning an **auxiliary** pane -- one in no
/// layout and on no frame, whose death was `reap_panes` alone. Aux panes existed
/// only to host the old `files` plugin's file manager and went with it; every
/// pane now belongs to a session, so the parameter no longer asks a question.
///
/// Split out of [`start_pty_forwarding`] rather than inlined: the loop body is
/// the emulator's whole ingest path (batching, DSR replies, OSC 52, activity
/// marking, subscriber streaming), and it is spawned once per pane id.
#[allow(clippy::too_many_arguments)]
async fn start_forwarding_for_pane(
    pane_id: PaneId,
    session_name: String,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) {
    // Enforce exactly one forwarding task per pane. Check-and-set the
    // guard under the panes lock before spawning: if a task already
    // exists, skip this pane so we don't spawn a competing task that
    // could process PTY chunks out of order.
    {
        let mut ps = panes.lock().await;
        match ps.get_mut(&pane_id) {
            Some(pane_data) => {
                if pane_data.forwarding_started {
                    return; // already has its forwarding task
                }
                pane_data.forwarding_started = true;
            }
            None => return,
        }
    }

    let state = Arc::clone(state);
    let panes = Arc::clone(panes);
    let clients = Arc::clone(clients);
    let config = Arc::clone(config);
    let prev_frames = Arc::clone(prev_frames);
    let dormant = Arc::clone(dormant);

    {
        tokio::spawn(async move {
            let mut watch = ForegroundWatch::new(pane_id, &panes).await;
            let first = {
                let ps = panes.lock().await;
                ps.get(&pane_id)
                    .and_then(|pd| pd.names_to_refresh(std::time::Instant::now(), true))
            };
            if let Some(what) = first {
                refresh_pane_names(pane_id, &panes, what).await;
            }
            loop {
                let recv_result = {
                    let mut ps = panes.lock().await;
                    if let Some(pane_data) = ps.get_mut(&pane_id) {
                        match pane_data.pty_rx.try_recv() {
                            Ok(data) => Some(Ok(data)),
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                Some(Err(()))
                            }
                        }
                    } else {
                        break;
                    }
                };

                match recv_result {
                    Some(Ok(first_chunk)) => {
                        // Drain all available data from the channel before
                        // processing, so rapid output is batched.
                        let mut chunks = vec![first_chunk];
                        {
                            let mut ps = panes.lock().await;
                            if let Some(pane_data) = ps.get_mut(&pane_id) {
                                while let Ok(more) = pane_data.pty_rx.try_recv() {
                                    chunks.push(more);
                                }
                            }
                        }

                        let (responses, bell, clipboard, retitled, fg_changed, refresh) = {
                            let mut ps = panes.lock().await;
                            if let Some(pane_data) = ps.get_mut(&pane_id) {
                                let now = std::time::Instant::now();
                                let mut fg_changed = false;
                                for chunk in &chunks {
                                    let t = pane_data.ingest_chunk(chunk);
                                    fg_changed |= note_transition(pane_id, t);
                                }
                                pane_data.last_output = now;
                                let retitled = pane_data.settle_title(pane_id, now);
                                let refresh = pane_data.names_to_refresh(now, fg_changed);
                                // Output is what makes an agent `Working`, and
                                // what clears a `NeedsInput` prompt off the
                                // screen. Safe under the lock for the same
                                // reason `mark_session_tree_dirty` is.
                                mark_agents_dirty();
                                let bell = pane_data.screen.take_bell();
                                // Always drained, even when the config gate is
                                // off, so a disallowed write cannot sit pending
                                // and land the moment it is switched back on.
                                let clipboard = pane_data.screen.take_clipboard();
                                (
                                    pane_data.screen.take_responses(),
                                    bell,
                                    clipboard,
                                    retitled,
                                    fg_changed,
                                    refresh,
                                )
                            } else {
                                (Vec::new(), false, None, false, false, None)
                            }
                        };
                        let renamed = match refresh {
                            Some(what) => refresh_pane_names(pane_id, &panes, what).await,
                            None => false,
                        };
                        if retitled || fg_changed || renamed {
                            mark_pane_retitled();
                        }
                        // Record background-tab activity (no-op if this pane's
                        // tab is the foreground/active tab). Panes lock is
                        // released above; acquire state alone to preserve the
                        // state-before-panes lock order used elsewhere.
                        let owner = {
                            let mut st = state.lock().await;
                            st.record_pane_activity(pane_id, bell, std::time::Instant::now());
                            st.session_of_pane(pane_id).map(str::to_owned)
                        };
                        // Write any pending responses (e.g., DSR replies) back to the PTY.
                        if !responses.is_empty() {
                            let ps = panes.lock().await;
                            if let Some(pane_data) = ps.get(&pane_id) {
                                for resp in &responses {
                                    pane_data.send_input(resp);
                                }
                            }
                        }
                        // An application asked for the system clipboard via OSC 52.
                        if let Some(text) = clipboard {
                            if config.general.allow_app_clipboard {
                                deliver_app_clipboard(pane_id, text, &state, &clients).await;
                            }
                        }
                        // Stream to the pane's View-cell subscribers, each at its
                        // own scroll offset and selection. Self-limiting: with no
                        // subscriber it returns before snapshotting, so a pane
                        // nobody watches costs nothing per PTY batch.
                        stream_pane_content(pane_id, &state, &panes, &clients).await;
                        broadcast_full_render(
                            owner.as_deref().unwrap_or(&session_name),
                            &state,
                            &panes,
                            &clients,
                            &config,
                            &prev_frames,
                        )
                        .await;
                    }
                    Some(Err(())) => {
                        // Channel disconnected - process has exited.
                        log::debug!(
                            "server: PTY channel disconnected for pane_id={pane_id} session={session_name}"
                        );
                        if let Some(pd) = panes.lock().await.get_mut(&pane_id) {
                            pd.exited = true;
                        }
                        close_pane(pane_id, &state, &panes, &clients, &config, &prev_frames).await;
                        notify_if_close_declined(pane_id, &panes, &clients).await;
                        save_if_enabled(&state, &panes, &config, &dormant).await;
                        // A pane dying on its own removes a row from every
                        // subscriber's tree, and no command ran to say so.
                        mark_session_tree_dirty();
                        break;
                    }
                    None => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        // A quiet pane gets no tagged reads, so its foreground
                        // is sampled here, and the settle windows end here.
                        let sample = watch.sample();
                        let (retitled, fg_changed, refresh) = {
                            let mut ps = panes.lock().await;
                            match ps.get_mut(&pane_id) {
                                Some(pane_data) => {
                                    let fg_changed = sample.is_some_and(|(fg, at)| {
                                        let shell = pane_data.shell_pgid();
                                        let t = pane_data.title.note_foreground(
                                            fg,
                                            at,
                                            shell,
                                            group_alive,
                                        );
                                        note_transition(pane_id, t)
                                    });
                                    let now = std::time::Instant::now();
                                    (
                                        pane_data.settle_title(pane_id, now),
                                        fg_changed,
                                        pane_data.names_to_refresh(now, fg_changed),
                                    )
                                }
                                None => break,
                            }
                        };
                        let renamed = match refresh {
                            Some(what) => refresh_pane_names(pane_id, &panes, what).await,
                            None => false,
                        };
                        if retitled || fg_changed || renamed {
                            mark_pane_retitled();
                            stream_pane_content(pane_id, &state, &panes, &clients).await;
                            let owner = {
                                let st = state.lock().await;
                                st.session_of_pane(pane_id).map(str::to_owned)
                            };
                            broadcast_full_render(
                                owner.as_deref().unwrap_or(&session_name),
                                &state,
                                &panes,
                                &clients,
                                &config,
                                &prev_frames,
                            )
                            .await;
                        }
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Event-driven save helper
// ---------------------------------------------------------------------------

/// Save the server state to disk if `save_sessions` is enabled.
///
/// This captures the current working directory of every pane and writes
/// the full server state (live sessions merged with any still-dormant
/// sessions) to the persistence file. It is called after
/// every structural change (session/tab/pane create/close/rename).
async fn save_if_enabled(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    dormant: &DormantStore,
) {
    if !config.general.save_sessions {
        log::debug!("server: save_if_enabled skipped (save_sessions=false)");
        return;
    }
    log::debug!("server: save_if_enabled saving state");
    let st = state.lock().await;
    let ps = panes.lock().await;
    let mut pane_cwds = HashMap::new();
    for (&pane_id, pane_data) in ps.iter() {
        if let Some(cwd) = crate::server::persistence::get_pane_cwd(pane_data.pty.child_pid) {
            pane_cwds.insert(pane_id, cwd);
        }
    }
    if let Ok(mut persisted) =
        crate::server::persistence::PersistedState::from_server(&st, &pane_cwds)
    {
        forget_titles(&mut persisted.state, &ps);
        // Persist live + still-dormant sessions so a live-only save never
        // clobbers un-resurrected dormant sessions on disk. No-op (byte-identical
        // to a live-only save) whenever the dormant store is empty, which is the
        // case on the default automatic_restore path and when save_sessions is
        // the only persistence toggle.
        {
            let d = dormant.lock().await;
            merge_dormant_into(&mut persisted, &d);
        }
        if let Err(e) = crate::server::persistence::save_state(&persisted) {
            log::error!("failed to save state: {e}");
        }
    }
}

/// Replace every pane name that did not come from the user with the pane's
/// process name, in a copy of the state about to be saved. A window title is
/// runtime-only: it can hold anything a program chose to show, such as a
/// summary of what the user asked an agent, and it would otherwise be written
/// to `state.json` and shown again by a pane that no longer runs that program.
fn forget_titles(state: &mut ServerState, ps: &HashMap<PaneId, PaneData>) {
    for sess in state.sessions.values_mut() {
        for tab in sess.tabs.iter_mut() {
            for pane_id in layout::all_pane_ids(&tab.layout) {
                if let Some(Some(_)) = layout::get_pane_custom_name(&tab.layout, pane_id) {
                    continue;
                }
                let name = ps
                    .get(&pane_id)
                    .map(|pd| pd.names.shell.clone())
                    .unwrap_or_default();
                layout::set_pane_name(&mut tab.layout, pane_id, &name);
            }
        }
    }
}

/// Deep-clone a [`ServerState`] via serde (it doesn't derive `Clone`).
fn clone_server_state(state: &ServerState) -> Option<ServerState> {
    let json = serde_json::to_string(state).ok()?;
    serde_json::from_str(&json).ok()
}

/// Merge the still-dormant sessions/folders/cwds into a live snapshot so a
/// save writes the union of live and dormant state. Live entries win on any
/// name collision; folders are unioned by `session_ids`.
fn merge_dormant_into(target: &mut PersistedState, dormant: &PersistedState) {
    if dormant.state.sessions.is_empty() && dormant.state.folders.is_empty() {
        return;
    }
    let dstate = match clone_server_state(&dormant.state) {
        Some(s) => s,
        None => {
            log::error!("merge_dormant_into: failed to clone dormant state; saving live-only");
            return;
        }
    };
    for (name, session) in dstate.sessions {
        target.state.sessions.entry(name).or_insert(session);
    }
    for (fname, folder) in dstate.folders {
        let entry = target
            .state
            .folders
            .entry(fname.clone())
            .or_insert_with(|| Folder {
                name: fname,
                session_ids: Vec::new(),
            });
        for sid in folder.session_ids {
            if !entry.session_ids.contains(&sid) {
                entry.session_ids.push(sid);
            }
        }
    }
    for (&pid, cwd) in &dormant.pane_cwds {
        target.pane_cwds.entry(pid).or_insert_with(|| cwd.clone());
    }
    // Keep id counters above any merged pane/tab id.
    target.state.ensure_id_counters();
}

// ---------------------------------------------------------------------------
// State restore on startup
// ---------------------------------------------------------------------------

/// Bring a single session (already present in live `ServerState`) to life:
/// spawn a PTY for each of its panes (using saved CWDs) and start PTY
/// forwarding for them. Panes are initially sized 80x24; they are resized when
/// the first client attaches and sends a `Resize`.
///
/// This is the shared per-session materialization path used by both startup
/// `automatic_restore` ([`restore_state`]) and on-demand
/// [`handle_resurrect_session`].
#[allow(clippy::too_many_arguments)]
async fn materialize_session(
    session_name: &str,
    pane_cwds: &HashMap<u64, String>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    // Collect this session's pane ids across all tabs from live state.
    let pane_ids: Vec<PaneId> = {
        let st = state.lock().await;
        match st.sessions.get(session_name) {
            // `Tab::panes()`, not the layout tree: the two must agree (the
            // structural invariant), and reading the same side as every other
            // consumer is what keeps a restored pane from silently getting no
            // PTY when they ever drift.
            Some(sess) => sess.tabs.iter().flat_map(|t| t.panes().to_vec()).collect(),
            None => {
                log::warn!("materialize_session: session '{session_name}' not in live state");
                return Ok(());
            }
        }
    };

    // Spawn PTYs for all panes. Panes start at 80x24 and are resized on attach.
    let default_cols: u16 = 80;
    let default_rows: u16 = 24;
    for pane_id in &pane_ids {
        let cwd = pane_cwds.get(pane_id).cloned();
        let cwd_path = cwd.as_deref().map(std::path::Path::new);
        if let Err(e) = spawn_pane(
            *pane_id,
            default_cols,
            default_rows,
            None,
            &[],
            cwd_path,
            Some(PaneIdentity {
                session: session_name,
                pane: *pane_id,
            }),
            panes,
            config,
        )
        .await
        {
            log::warn!("failed to spawn PTY for restored pane {pane_id}: {e}");
        }
    }

    for pane_id in pane_ids {
        start_forwarding_for_pane(
            pane_id,
            session_name.to_string(),
            state,
            panes,
            clients,
            config,
            prev_frames,
            dormant,
        )
        .await;
    }

    Ok(())
}

/// Restore server state from a persisted snapshot on startup
/// (`automatic_restore = true`).
///
/// Replaces the server's state with the deserialized state, then materializes
/// every session via the shared [`materialize_session`] path.
async fn restore_state(
    server: &RemuxServer,
    persisted: crate::server::persistence::PersistedState,
) -> Result<()> {
    let mut restored_state = persisted.state;
    restored_state.ensure_id_counters();
    // `pane_order` is `#[serde(default)]`, so a snapshot written before the
    // field existed restores empty alongside a full tree. Repair on the way in,
    // so the structural invariant holds from the first frame and every consumer
    // of the pane set (PTY materialization, View cell titles) sees the same
    // panes.
    for sess in restored_state.sessions.values_mut() {
        for tab in &mut sess.tabs {
            tab.reconcile_pane_order();
        }
    }

    let session_names: Vec<String> = restored_state.sessions.keys().cloned().collect();

    // Replace the server state.
    {
        let mut st = server.state.lock().await;
        *st = restored_state;
    }

    for session_name in &session_names {
        materialize_session(
            session_name,
            &persisted.pane_cwds,
            &server.state,
            &server.panes,
            &server.clients,
            &server.config,
            &server.prev_frames,
            &server.dormant,
        )
        .await?;
    }

    log::info!("restored {} sessions", session_names.len());
    Ok(())
}

/// Move the named session from a dormant snapshot into live `ServerState`,
/// recreating folder membership. Returns the CWDs of the session's panes (for
/// PTY spawning) on success, or `None` if the name isn't dormant or a live
/// session already exists under that name.
///
/// Pure state manipulation with no PTY side effects, so it is unit-testable
/// independently of [`materialize_session`]. Guards on the live collision
/// *before* removing from the dormant snapshot so a name clash never silently
/// drops the dormant session.
fn take_dormant_session(
    dormant: &mut PersistedState,
    live: &mut ServerState,
    name: &str,
) -> Option<HashMap<u64, String>> {
    if live.sessions.contains_key(name) {
        log::warn!("resurrect: live session '{name}' already exists; ignoring");
        return None;
    }
    let mut session = match dormant.state.sessions.remove(name) {
        Some(s) => s,
        None => {
            log::warn!("resurrect: no dormant session '{name}'");
            return None;
        }
    };

    // Same deserialization repair as `restore_state`: a dormant snapshot comes
    // off disk and may predate `pane_order`.
    for tab in &mut session.tabs {
        tab.reconcile_pane_order();
    }

    // Detach from any dormant folder membership and collect its pane CWDs.
    let folder = session.folder.clone();
    if let Some(ref fname) = folder {
        if let Some(f) = dormant.state.folders.get_mut(fname) {
            f.session_ids.retain(|s| s != name);
        }
    }
    let pane_ids: Vec<u64> = session
        .tabs
        .iter()
        .flat_map(|t| layout::all_pane_ids(&t.layout))
        .collect();
    let mut cwds = HashMap::new();
    for pid in &pane_ids {
        if let Some(cwd) = dormant.pane_cwds.remove(pid) {
            cwds.insert(*pid, cwd);
        }
    }

    // Insert into live state, recreating folder membership if needed.
    if let Some(ref fname) = folder {
        let entry = live.folders.entry(fname.clone()).or_insert_with(|| Folder {
            name: fname.clone(),
            session_ids: Vec::new(),
        });
        if !entry.session_ids.iter().any(|s| s == name) {
            entry.session_ids.push(name.to_string());
        }
    }
    live.sessions.insert(name.to_string(), session);
    live.ensure_id_counters();
    Some(cwds)
}

/// Materialize a dormant session into a live session on client request.
///
/// Migrates the session out of the dormant store into live `ServerState` (via
/// [`take_dormant_session`]) and brings it live via the shared
/// [`materialize_session`] path. No-op if the name isn't dormant or a live
/// session already exists under that name.
async fn handle_resurrect_session(
    name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    // Lock state before dormant to match `save_if_enabled`'s ordering and avoid
    // any lock-order inversion.
    let cwds = {
        let mut st = state.lock().await;
        let mut d = dormant.lock().await;
        match take_dormant_session(&mut d, &mut st, name) {
            Some(c) => c,
            None => return Ok(()),
        }
    };

    log::info!("resurrecting dormant session '{name}'");
    materialize_session(
        name,
        &cwds,
        state,
        panes,
        clients,
        config,
        prev_frames,
        dormant,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        arrow_report, button_report, compute_diff, group_member_name, merge_dormant_into,
        mouse_route, next_layout_mode, on_pane_border, report_for, right_press_outcome,
        right_release_outcome, saved_custom_is_restorable, take_dormant_session, wheel_report,
        Button, ButtonPhase, MouseGesture, MouseRoute, PaneNames, PanePlacement, RightPress,
    };
    use crate::protocol::RenderCell;
    use crate::screen::Screen;
    use crate::server::layout::{
        BspLayout, ColumnsLayout, CustomLayout, EnabledLayouts, GridLayout, LayoutMode, LayoutNode,
        MasterLayout, MonocleLayout, Rect, RowsLayout,
    };
    use crate::server::persistence::PersistedState;
    use crate::server::session::ServerState;
    use std::collections::HashMap;

    #[tokio::test]
    async fn a_typing_task_that_panics_still_ends_its_panes_typing() {
        use crate::server::messaging::SubmitEntry;
        let pane = 9_000_001;
        // The store is the process-wide one, and every `Rig` test closing its
        // panes runs `reap_panes`, which removes the mail each closed pane
        // SENT. Rig panes are numbered from 1, so a sender id in that range
        // loses this test's entry whenever such a test finishes alongside it.
        let sender = 9_000_002;
        let store = super::messaging();
        {
            let mut st = store.lock().await;
            st.begin_typing(pane, None);
            let queued = SubmitEntry {
                id: 1,
                pane,
                deliver: crate::protocol::Deliver::Submit,
                body: String::new(),
                from: crate::server::messaging::SubmitFrom::pane(sender),
                enqueued_at: std::time::Instant::now(),
                defer: false,
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
                reply: None,
                had_reply: false,
                put_back: 0,
                waiting_on: None,
                wait_until: None,
            };
            st.enqueue_submit(queued, 8).unwrap();
            assert!(st.submit_heads().iter().all(|h| h.pane != pane));
        }
        let typing = super::EndTypingOnDrop { pane, armed: true };
        let task = tokio::spawn(async move {
            let _typing = typing;
            panic!("the typing task stops");
        });
        assert!(task.await.is_err());
        for _ in 0..100 {
            if store
                .lock()
                .await
                .submit_heads()
                .iter()
                .any(|h| h.pane == pane)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the pane is still typing, so its queue is never looked at");
    }

    #[test]
    fn the_most_chunks_fit_the_body_cap_with_room_to_spare() {
        use super::{BODY_CAP, MAX_CHUNKS};
        assert_eq!(MAX_CHUNKS, 1500);
        let idle = std::time::Duration::from_micros(4100);
        let loaded = std::time::Duration::from_micros(12800);
        assert!(
            idle * (MAX_CHUNKS as u32) * 3 <= BODY_CAP,
            "a body at the limit, at the cost per chunk measured idle, takes under a third of the cap"
        );
        assert!(
            loaded * (MAX_CHUNKS as u32) <= BODY_CAP,
            "a body at the limit, at the cost per chunk measured under load, fits in the cap"
        );
    }

    #[test]
    fn a_deferred_submit_is_bounded_by_its_own_timeout_not_the_hold() {
        use super::{submit_deadline, DEFAULT_SUBMIT_TIMEOUT_MS, MAX_WAIT_MS, SUBMIT_HOLD_MS};
        let t0 = std::time::Instant::now();
        let at = |ms| t0 + std::time::Duration::from_millis(ms);
        assert_eq!(submit_deadline(t0, false, None), at(SUBMIT_HOLD_MS));
        assert_eq!(
            submit_deadline(t0, false, Some(1)),
            at(SUBMIT_HOLD_MS),
            "a timeout bounds only a deferred wait"
        );
        assert_eq!(
            submit_deadline(t0, true, Some(0)),
            at(SUBMIT_HOLD_MS),
            "never less than a submit that did not ask to wait"
        );
        assert_eq!(submit_deadline(t0, true, Some(1500)), at(SUBMIT_HOLD_MS));
        assert_eq!(
            submit_deadline(t0, true, Some(SUBMIT_HOLD_MS * 2)),
            at(SUBMIT_HOLD_MS * 2),
            "past the hold"
        );
        assert_eq!(
            submit_deadline(t0, true, None),
            at(DEFAULT_SUBMIT_TIMEOUT_MS)
        );
        assert_eq!(submit_deadline(t0, true, Some(u64::MAX)), at(MAX_WAIT_MS));
    }

    #[test]
    fn a_wait_runs_for_its_own_timeout_with_no_floor() {
        use super::{wait_timeout, DEFAULT_SUBMIT_TIMEOUT_MS, MAX_WAIT_MS};
        let ms = std::time::Duration::from_millis;
        assert_eq!(wait_timeout(Some(1500)), ms(1500), "not raised to the hold");
        assert_eq!(wait_timeout(None), ms(DEFAULT_SUBMIT_TIMEOUT_MS));
        assert_eq!(wait_timeout(Some(u64::MAX)), ms(MAX_WAIT_MS));
    }

    #[test]
    fn the_typing_guard_at_any_setting_ends_inside_the_hold() {
        let cfg = crate::config::messaging::MessagingConfig {
            typing_guard_max_ms: u64::MAX,
            ..Default::default()
        };
        assert!(
            cfg.typing_guard().max < std::time::Duration::from_millis(super::SUBMIT_HOLD_MS),
            "a guard that outlasts the hold answers `timeout` where it should answer `human_typing`"
        );
    }

    #[test]
    fn a_split_along_the_columns_or_rows_axis_becomes_auto() {
        assert_eq!(
            PanePlacement::SplitVertical.for_layout(&LayoutMode::Columns(ColumnsLayout)),
            PanePlacement::Auto
        );
        assert_eq!(
            PanePlacement::SplitHorizontal.for_layout(&LayoutMode::Rows(RowsLayout)),
            PanePlacement::Auto
        );
    }

    #[test]
    fn every_other_split_keeps_its_manual_placement() {
        let modes = [
            LayoutMode::Bsp(BspLayout),
            LayoutMode::Master(MasterLayout::default()),
            LayoutMode::Monocle(MonocleLayout),
            LayoutMode::Grid(GridLayout),
            LayoutMode::Custom(CustomLayout),
        ];
        for mode in &modes {
            for p in [
                PanePlacement::SplitVertical,
                PanePlacement::SplitHorizontal,
                PanePlacement::Stack,
            ] {
                assert_eq!(p.for_layout(mode), p, "{p:?} in {}", mode.name());
            }
        }
        assert_eq!(
            PanePlacement::SplitHorizontal.for_layout(&LayoutMode::Columns(ColumnsLayout)),
            PanePlacement::SplitHorizontal
        );
        assert_eq!(
            PanePlacement::SplitVertical.for_layout(&LayoutMode::Rows(RowsLayout)),
            PanePlacement::SplitVertical
        );
        for mode in [
            LayoutMode::Columns(ColumnsLayout),
            LayoutMode::Rows(RowsLayout),
        ] {
            assert_eq!(PanePlacement::Stack.for_layout(&mode), PanePlacement::Stack);
        }
    }

    #[test]
    fn a_host_the_template_does_not_use_does_not_split_the_frame() {
        use super::Naming;
        let local = Naming::new(Some("{command}: {title}".into()), String::new());
        let remote = Naming::new(Some("{command}: {title}".into()), "mini".into());
        assert_eq!(local, remote);
        let with_host = |host: &str| Naming::new(Some("{host}:{command}".into()), host.into());
        assert_ne!(with_host(""), with_host("mini"));
        assert_eq!(
            Naming::new(None, "mini".into()),
            Naming::new(None, String::new())
        );
    }

    fn names_with_job(job: Option<(i32, &str)>) -> PaneNames {
        PaneNames {
            shell: "sh".into(),
            job: job.map(|(g, n)| (g, n.to_string())),
            ..PaneNames::default()
        }
    }

    const SHELL_PID: i32 = 100;
    const LEADER: i32 = 200;
    const MEMBER: i32 = 201;

    fn pgid_in_job(pid: i32) -> Option<i32> {
        matches!(pid, LEADER | MEMBER).then_some(LEADER)
    }

    fn shell_children(_shell: i32) -> Vec<i32> {
        vec![LEADER, MEMBER]
    }

    #[test]
    fn a_leader_reaped_before_its_name_is_read_is_not_a_live_member() {
        let name = group_member_name(
            LEADER,
            SHELL_PID,
            |pid| (pid == LEADER).then_some(LEADER),
            |_| Vec::new(),
            |_| None,
        );
        assert_eq!(name, None);
    }

    #[test]
    fn a_group_is_named_by_a_member_whose_name_can_be_read() {
        let name = group_member_name(LEADER, SHELL_PID, pgid_in_job, shell_children, |pid| {
            (pid == MEMBER).then(|| "sleep".to_string())
        });
        assert_eq!(name.as_deref(), Some("sleep"));
    }

    #[test]
    fn a_live_leader_names_its_group() {
        let name = group_member_name(LEADER, SHELL_PID, pgid_in_job, shell_children, |pid| {
            Some(format!("p{pid}"))
        });
        assert_eq!(name.as_deref(), Some("p200"));
    }

    #[test]
    fn a_job_that_exits_before_the_shell_is_back_keeps_its_name() {
        let names = names_with_job(Some((200, "sleep")));
        assert_eq!(
            names.job_after_read(200, None, "sh"),
            (200, "sleep".to_string())
        );
    }

    #[test]
    fn a_live_job_is_named_by_what_the_read_found() {
        let names = names_with_job(Some((200, "sh")));
        assert_eq!(
            names.job_after_read(200, Some("sleep".into()), "sh"),
            (200, "sleep".to_string())
        );
    }

    #[test]
    fn a_job_never_seen_alive_falls_back_to_the_shell_name() {
        let names = names_with_job(Some((100, "vim")));
        assert_eq!(
            names.job_after_read(200, None, "sh"),
            (200, "sh".to_string())
        );
    }

    #[test]
    fn argv0_names_a_process_by_its_basename_without_a_login_dash() {
        use super::command_name_from_argv0 as name;
        assert_eq!(name("claude").as_deref(), Some("claude"));
        assert_eq!(
            name("/Users/u/.local/bin/claude").as_deref(),
            Some("claude")
        );
        assert_eq!(name("-zsh").as_deref(), Some("zsh"));
        assert_eq!(name("/bin/-weird").as_deref(), Some("weird"));
        assert_eq!(name("--x").as_deref(), Some("-x"), "only one login dash");
        assert_eq!(name(""), None);
        assert_eq!(name("-"), None);
        assert_eq!(name("/usr/bin/"), None);
    }

    #[test]
    fn argv0_is_read_out_of_a_procargs_buffer() {
        use super::argv0_from_procargs as argv0;
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/Users/u/.local/share/claude/versions/2.1.283\0\0\0");
        buf.extend_from_slice(b"claude\0--resume\0HOME=/Users/u\0");
        assert_eq!(argv0(&buf).as_deref(), Some("claude"));
        let mut none = 0i32.to_ne_bytes().to_vec();
        none.extend_from_slice(b"/bin/x\0\0");
        assert_eq!(argv0(&none), None, "argc 0");
        assert_eq!(argv0(&[1, 0]), None, "too short");
    }

    #[test]
    fn an_empty_argv0_is_read_as_empty_not_as_the_next_string() {
        use super::argv0_from_procargs as argv0;
        // A 12-byte path, its NUL and three bytes of padding reach offset 16,
        // where argv[0] starts: here an empty one, so its NUL is at 16.
        let buf = |argc: i32, after: &[u8]| {
            let mut b = argc.to_ne_bytes().to_vec();
            b.extend_from_slice(b"/tmp/x/sssss\0\0\0\0");
            b.extend_from_slice(after);
            b
        };
        assert_eq!(argv0(&buf(1, b"\0SECRET=hunter2\0")).as_deref(), Some(""));
        assert_eq!(argv0(&buf(2, b"\x005\0USER=u\0")).as_deref(), Some(""));
        assert_eq!(argv0(&buf(2, b"x\x005\0")).as_deref(), Some("x"));
        // A path whose NUL lands on a multiple of 8 is padded by a whole 8.
        let mut b = 1i32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/tmp/x/sssssssss\0\0\0\0\0\0\0\0");
        b.extend_from_slice(b"\0HOME=/Users/u\0");
        assert_eq!(argv0(&b).as_deref(), Some(""));
        // Shorter padding than the rule: the first non-NUL byte is argv[0].
        let mut short = 1i32.to_ne_bytes().to_vec();
        short.extend_from_slice(b"/tmp/x/sssss\0claude\0");
        assert_eq!(argv0(&short).as_deref(), Some("claude"));
    }

    #[test]
    fn only_a_private_directory_of_ours_holds_the_socket() {
        use std::os::unix::fs::PermissionsExt;
        let uid = super::own_uid();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("remux-test");
        std::fs::create_dir(&dir).unwrap();
        let set = |mode| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode));

        set(0o700).unwrap();
        assert!(super::check_private_dir(&dir, uid).is_ok());

        set(0o755).unwrap();
        let err = super::check_private_dir(&dir, uid).unwrap_err().to_string();
        assert!(err.contains("755") && err.contains("0700"), "{err}");

        set(0o700).unwrap();
        let err = super::check_private_dir(&dir, uid + 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("owned by uid"), "{err}");

        let link = root.path().join("remux-link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let err = super::check_private_dir(&link, uid)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not a directory"),
            "a symlink is refused: {err}"
        );
    }

    #[test]
    fn only_this_user_may_connect() {
        assert!(super::peer_allowed(1000, 1000));
        assert!(!super::peer_allowed(1001, 1000));
        assert!(!super::peer_allowed(0, 1000), "not even root");
    }

    /// On macOS a pane is named by `argv[0]`: the `KERN_PROCARGS2` read must
    /// succeed and name the process, not fall back to the executable's path.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_names_a_process_by_its_argv0() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sleep")
            .arg0("claude")
            .arg("5")
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = child.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(200));
        let argv0 = super::process_argv0(pid);
        let name = super::get_process_name(pid);
        let exe = super::executable_name(pid);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(argv0.as_deref(), Some("claude"), "the sysctl read argv[0]");
        assert_eq!(
            exe.as_deref(),
            Some("sleep"),
            "the path would have said sleep"
        );
        assert_eq!(name, "claude");
    }

    /// A long `argv[0]` keeps its basename: detection and naming take the
    /// basename of it, so it must not be cut to a label's length first.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_long_argv0_path_keeps_its_basename() {
        use std::os::unix::process::CommandExt;
        let long = format!("/{}/claude", "d".repeat(300));
        let mut child = std::process::Command::new("/bin/sleep")
            .arg0(&long)
            .arg("5")
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = child.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(200));
        let names = super::get_process_names(pid);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(names.argv0.as_deref(), Some(long.as_str()));
    }

    /// A process first looked at before an `exec` is named by what it became
    /// after it. A new pane's shell is first seen straight after the fork,
    /// still named after the server, and a name cached from that sample is
    /// how every such pane on macOS came to be called `remux`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_process_is_renamed_by_its_exec() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 0.6; exec sleep 5"])
            .spawn()
            .expect("spawn /bin/sh");
        let pid = child.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(200));
        let before = super::get_process_name(pid);
        let before_names = super::get_process_names(pid);
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let after = super::get_process_name(pid);
        let after_names = super::get_process_names(pid);
        let _ = child.kill();
        let _ = child.wait();
        assert_ne!(before, "sleep", "the shell has not exec'd yet");
        assert_ne!(before_names.name, "sleep");
        assert_eq!(after, "sleep");
        assert_eq!(after_names.name, "sleep");
        assert_eq!(after_names.argv0.as_deref(), Some("sleep"));
    }

    /// Build a `ServerState` with the named sessions (each optionally filed
    /// under a folder), returning it alongside the first pane id of the first
    /// listed session (handy for CWD assertions).
    fn state_with_sessions(sessions: &[(&str, Option<&str>)]) -> ServerState {
        let mut st = ServerState::new();
        for (name, folder) in sessions {
            if let Some(f) = folder {
                if !st.folders.contains_key(*f) {
                    st.create_folder(f).unwrap();
                }
            }
            st.create_session(
                name,
                *folder,
                crate::config::BorderStyle::ZellijStyle,
                Default::default(),
            )
            .unwrap();
        }
        st
    }

    #[test]
    fn take_dormant_session_migrates_one_session_from_persisted_state() {
        // A dormant snapshot with two sessions, one filed under "work".
        let dstate = state_with_sessions(&[("alpha", Some("work")), ("beta", None)]);
        let alpha_pane =
            crate::server::layout::all_pane_ids(&dstate.sessions["alpha"].tabs[0].layout)[0];
        let mut cwds = HashMap::new();
        cwds.insert(alpha_pane, "/tmp/alpha".to_string());
        let mut dormant = PersistedState {
            state: dstate,
            pane_cwds: cwds,
        };

        let mut live = ServerState::new();
        let got =
            take_dormant_session(&mut dormant, &mut live, "alpha").expect("alpha should resurrect");

        // alpha migrated into live with its folder membership recreated.
        assert!(live.sessions.contains_key("alpha"));
        assert!(live
            .folders
            .get("work")
            .unwrap()
            .session_ids
            .contains(&"alpha".to_string()));
        assert_eq!(got.get(&alpha_pane).unwrap(), "/tmp/alpha");

        // alpha (and its cwd) removed from dormant; beta remains dormant.
        assert!(!dormant.state.sessions.contains_key("alpha"));
        assert!(dormant.state.sessions.contains_key("beta"));
        assert!(dormant.pane_cwds.is_empty());

        // A name already live returns None (no double-resurrect).
        assert!(take_dormant_session(&mut dormant, &mut live, "alpha").is_none());
        // An unknown name returns None.
        assert!(take_dormant_session(&mut dormant, &mut live, "nope").is_none());
    }

    #[test]
    fn merge_dormant_into_unions_live_and_dormant() {
        let mut target = PersistedState {
            state: state_with_sessions(&[("live1", None)]),
            pane_cwds: HashMap::new(),
        };
        let dormant = PersistedState {
            state: state_with_sessions(&[("dorm1", Some("f"))]),
            pane_cwds: HashMap::new(),
        };

        merge_dormant_into(&mut target, &dormant);

        assert!(target.state.sessions.contains_key("live1"));
        assert!(target.state.sessions.contains_key("dorm1"));
        assert!(target
            .state
            .folders
            .get("f")
            .unwrap()
            .session_ids
            .contains(&"dorm1".to_string()));
    }

    #[test]
    fn merge_dormant_into_is_noop_when_dormant_empty() {
        // This is the default automatic_restore path: dormant is empty, so the
        // save must be byte-identical to a live-only save.
        let mut target = PersistedState {
            state: state_with_sessions(&[("live1", None), ("live2", None)]),
            pane_cwds: HashMap::new(),
        };
        let before = serde_json::to_string(&target).unwrap();

        let empty = PersistedState {
            state: ServerState::new(),
            pane_cwds: HashMap::new(),
        };
        merge_dormant_into(&mut target, &empty);

        let after = serde_json::to_string(&target).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn compute_diff_emits_wide_lead_when_only_continuation_differs() {
        let lead = RenderCell {
            c: '中',
            width: 2,
            ..RenderCell::default()
        };
        let continuation = RenderCell {
            c: ' ',
            width: 0,
            ..RenderCell::default()
        };
        let narrow_a = RenderCell {
            c: 'a',
            ..RenderCell::default()
        };

        // Lead unchanged; only the continuation column differs (it used to hold a
        // narrow char, now it is the wide glyph's continuation). The client skips
        // width-0 changes, so the diff must ALSO re-emit the unchanged lead to
        // repaint the whole glyph and cover the continuation column.
        let prev = vec![vec![lead.clone(), narrow_a.clone()]];
        let curr = vec![vec![lead.clone(), continuation.clone()]];

        let changes = compute_diff(&prev, &curr);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].x, 0);
        assert_eq!(changes[0].cell.width, 2);
        assert_eq!(changes[1].x, 1);
        assert_eq!(changes[1].cell.width, 0);

        // When the lead itself also changed, it is emitted once (at x=0) and not
        // duplicated by the continuation handling at x=1.
        let prev2 = vec![vec![narrow_a.clone(), narrow_a.clone()]];
        let curr2 = vec![vec![lead.clone(), continuation.clone()]];
        let changes2 = compute_diff(&prev2, &curr2);
        assert_eq!(changes2.len(), 2);
        assert_eq!(changes2[0].x, 0);
        assert_eq!(changes2[1].x, 1);
    }

    #[test]
    fn compute_diff_emits_cell_when_only_combining_differs() {
        // A base glyph gaining a combining mark differs only in `combining`.
        // `RenderCell` derives `PartialEq` over all fields, so the diff must
        // detect and emit the changed cell.
        let base = RenderCell {
            c: 'e',
            ..RenderCell::default()
        };
        let accented = RenderCell {
            c: 'e',
            combining: vec!['\u{301}'],
            ..RenderCell::default()
        };

        let prev = vec![vec![base.clone()]];
        let curr = vec![vec![accented.clone()]];
        let changes = compute_diff(&prev, &curr);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].x, 0);
        assert_eq!(changes[0].cell.combining, vec!['\u{301}']);

        // Losing the mark is likewise detected.
        let changes_back = compute_diff(&curr, &prev);
        assert_eq!(changes_back.len(), 1);
        assert!(changes_back[0].cell.combining.is_empty());
    }

    #[test]
    fn test_wheel_report_sgr() {
        // Wheel up: button 64, SGR encoding, 1-based coords.
        assert_eq!(wheel_report(true, true, 5, 10), b"\x1b[<64;5;10M".to_vec());
        // Wheel down: button 65.
        assert_eq!(wheel_report(true, false, 1, 1), b"\x1b[<65;1;1M".to_vec());
    }

    #[test]
    fn test_wheel_report_legacy() {
        // Legacy X10: ESC [ M then (32+btn), (32+col), (32+row).
        assert_eq!(
            wheel_report(false, true, 5, 10),
            vec![0x1b, b'[', b'M', 32 + 64, 32 + 5, 32 + 10]
        );
        // Coordinates saturate into a single byte.
        assert_eq!(
            wheel_report(false, false, 300, 400),
            vec![0x1b, b'[', b'M', 32 + 65, 255, 255]
        );
    }

    #[test]
    fn test_arrow_report() {
        assert_eq!(arrow_report(false, true), vec![0x1b, b'[', b'A']);
        assert_eq!(arrow_report(false, false), vec![0x1b, b'[', b'B']);
        assert_eq!(arrow_report(true, true), vec![0x1b, b'O', b'A']);
        assert_eq!(arrow_report(true, false), vec![0x1b, b'O', b'B']);
    }

    #[test]
    fn test_button_report_sgr() {
        // SGR keeps the button number on a release and marks it with a
        // lowercase final byte; motion sets the +32 motion bit.
        assert_eq!(
            button_report(true, ButtonPhase::Press, 10, 5),
            b"\x1b[<0;10;5M".to_vec()
        );
        assert_eq!(
            button_report(true, ButtonPhase::Motion, 10, 6),
            b"\x1b[<32;10;6M".to_vec()
        );
        assert_eq!(
            button_report(true, ButtonPhase::Release, 10, 6),
            b"\x1b[<0;10;6m".to_vec()
        );
    }

    #[test]
    fn test_button_report_legacy() {
        // X10 has no lowercase form: a release is button 3 ("no button").
        assert_eq!(
            button_report(false, ButtonPhase::Press, 5, 10),
            vec![0x1b, b'[', b'M', 32, 32 + 5, 32 + 10]
        );
        assert_eq!(
            button_report(false, ButtonPhase::Motion, 5, 10),
            vec![0x1b, b'[', b'M', 32 + 32, 32 + 5, 32 + 10]
        );
        assert_eq!(
            button_report(false, ButtonPhase::Release, 5, 10),
            vec![0x1b, b'[', b'M', 32 + 3, 32 + 5, 32 + 10]
        );
        // Coordinates saturate into a single byte, as in `wheel_report`.
        assert_eq!(
            button_report(false, ButtonPhase::Press, 300, 400),
            vec![0x1b, b'[', b'M', 32, 255, 255]
        );
    }

    #[test]
    fn right_button_reports_use_button_two() {
        assert_eq!(
            report_for(Button::Right, true, ButtonPhase::Press, 10, 5),
            b"\x1b[<2;10;5M".to_vec()
        );
        assert_eq!(
            report_for(Button::Right, true, ButtonPhase::Release, 10, 5),
            b"\x1b[<2;10;5m".to_vec()
        );
        assert_eq!(
            report_for(Button::Right, false, ButtonPhase::Press, 5, 10),
            vec![0x1b, b'[', b'M', 32 + 2, 32 + 5, 32 + 10]
        );
        // X10 still spells every release as button 3.
        assert_eq!(
            report_for(Button::Right, false, ButtonPhase::Release, 5, 10),
            vec![0x1b, b'[', b'M', 32 + 3, 32 + 5, 32 + 10]
        );
    }

    #[test]
    fn border_is_inside_the_rect_and_outside_the_inset_content() {
        let r = Rect {
            x: 10,
            y: 5,
            width: 20,
            height: 8,
        };
        assert!(on_pane_border(r, 1, 10, 7), "left edge");
        assert!(on_pane_border(r, 1, 29, 7), "right edge");
        assert!(on_pane_border(r, 1, 15, 5), "top edge");
        assert!(on_pane_border(r, 1, 15, 12), "bottom edge");
        assert!(!on_pane_border(r, 1, 11, 6), "first content cell");
        assert!(!on_pane_border(r, 1, 28, 11), "last content cell");
        assert!(!on_pane_border(r, 0, 10, 5), "no inset means no border");
    }

    #[test]
    fn right_press_goes_to_a_tracking_app_but_opens_the_menu_on_the_border() {
        let app = MouseRoute::App {
            sgr: true,
            motion: false,
        };
        let shell = MouseRoute::Remux { scrollback: true };
        assert_eq!(right_press_outcome(app, false, false), RightPress::Forward);
        assert_eq!(right_press_outcome(app, true, false), RightPress::Menu);
        assert_eq!(right_press_outcome(shell, false, false), RightPress::Menu);
        assert_eq!(right_press_outcome(shell, true, false), RightPress::Menu);
    }

    #[test]
    fn right_press_in_the_popup_never_opens_the_menu() {
        let app = MouseRoute::App {
            sgr: false,
            motion: true,
        };
        let shell = MouseRoute::Remux { scrollback: true };
        assert_eq!(right_press_outcome(app, false, true), RightPress::Forward);
        assert_eq!(right_press_outcome(shell, false, true), RightPress::Nothing);
    }

    #[test]
    fn right_release_is_forwarded_only_to_a_tracking_app_off_the_border() {
        let app = MouseRoute::App {
            sgr: true,
            motion: false,
        };
        let shell = MouseRoute::Remux { scrollback: true };
        assert!(right_release_outcome(app, false));
        assert!(!right_release_outcome(app, true));
        assert!(!right_release_outcome(shell, false));
    }

    /// The one routing decision every mouse path shares. A plain shell scrolls
    /// and selects in remux; an application that asked for mouse events gets
    /// them; an alt-screen application that did not gets the arrow fallback for
    /// the wheel and, crucially, a `scrollback: false` verdict for buttons --
    /// remux's scrollback there belongs to the primary screen, so an edge drag
    /// must not chase it (the drag-autoscroll spin).
    #[test]
    fn test_mouse_route() {
        let mut s = Screen::new(80, 24, 100);
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, false),
            MouseRoute::Remux { scrollback: true }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::Remux { scrollback: true }
        );

        // Alt screen, no tracking (e.g. `less`): arrows for the wheel, no
        // scrollback for a drag.
        s.process_output(b"\x1b[?1049h");
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, false),
            MouseRoute::AltArrows { app_cursor: false }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::Remux { scrollback: false }
        );
        s.process_output(b"\x1b[?1h");
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, false),
            MouseRoute::AltArrows { app_cursor: true }
        );

        // Tracking wins over everything, and carries the encoding + whether the
        // app asked for motion.
        s.process_output(b"\x1b[?1000h");
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::App {
                sgr: false,
                motion: false
            }
        );
        s.process_output(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, false),
            MouseRoute::App {
                sgr: true,
                motion: true
            }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::App {
                sgr: true,
                motion: true
            }
        );

        // Copy mode (Visual) outranks the application: the user asked for the
        // mouse explicitly. Selection still works, but not the scrolling that
        // would chase the primary screen's history under a full-screen app.
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, true),
            MouseRoute::Remux { scrollback: false }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, true),
            MouseRoute::Remux { scrollback: false }
        );

        // Leaving the alt screen releases the app's claim on the mouse.
        s.process_output(b"\x1b[?1049l");
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::Remux { scrollback: true }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, true),
            MouseRoute::Remux { scrollback: true }
        );
    }

    // -- Layout cycle (Alt+Space) -------------------------------------------

    #[test]
    fn next_layout_mode_automatic_cycle() {
        // Bsp -> Master -> Monocle -> Grid -> Columns -> Rows regardless of the
        // saved-custom flag.
        let all = EnabledLayouts::default();
        assert!(matches!(
            next_layout_mode(&LayoutMode::Bsp(BspLayout), false, &all),
            LayoutMode::Master(_)
        ));
        assert!(matches!(
            next_layout_mode(&LayoutMode::Master(MasterLayout::default()), false, &all),
            LayoutMode::Monocle(_)
        ));
        // Monocle now always advances to Grid, even with a saved custom layout.
        assert!(matches!(
            next_layout_mode(&LayoutMode::Monocle(MonocleLayout), false, &all),
            LayoutMode::Grid(_)
        ));
        assert!(matches!(
            next_layout_mode(&LayoutMode::Monocle(MonocleLayout), true, &all),
            LayoutMode::Grid(_)
        ));
        for saved in [false, true] {
            assert!(matches!(
                next_layout_mode(&LayoutMode::Grid(GridLayout), saved, &all),
                LayoutMode::Columns(_)
            ));
            assert!(matches!(
                next_layout_mode(&LayoutMode::Columns(ColumnsLayout), saved, &all),
                LayoutMode::Rows(_)
            ));
        }
    }

    #[test]
    fn next_layout_mode_custom_starts_cycle_at_bsp() {
        let all = EnabledLayouts::default();
        assert!(matches!(
            next_layout_mode(&LayoutMode::Custom(CustomLayout), false, &all),
            LayoutMode::Bsp(_)
        ));
    }

    #[test]
    fn next_layout_mode_rows_returns_to_custom_only_when_saved() {
        let all = EnabledLayouts::default();
        // Rows is the last automatic before wrap. With a restorable saved custom
        // layout, it wraps back to Custom.
        assert!(matches!(
            next_layout_mode(&LayoutMode::Rows(RowsLayout), true, &all),
            LayoutMode::Custom(_)
        ));
        // Without one, Rows wraps to Bsp as usual.
        assert!(matches!(
            next_layout_mode(&LayoutMode::Rows(RowsLayout), false, &all),
            LayoutMode::Bsp(_)
        ));
    }

    #[test]
    fn next_layout_mode_returns_to_custom_after_the_last_enabled_mode() {
        let enabled = EnabledLayouts {
            columns: false,
            rows: false,
            ..EnabledLayouts::default()
        };
        assert!(matches!(
            next_layout_mode(&LayoutMode::Grid(GridLayout), true, &enabled),
            LayoutMode::Custom(_)
        ));
        assert!(matches!(
            next_layout_mode(&LayoutMode::Grid(GridLayout), false, &enabled),
            LayoutMode::Bsp(_)
        ));
        assert!(matches!(
            next_layout_mode(&LayoutMode::Monocle(MonocleLayout), true, &enabled),
            LayoutMode::Grid(_)
        ));
        // A tab left on a mode disabled since is past the last enabled one too.
        assert!(matches!(
            next_layout_mode(&LayoutMode::Rows(RowsLayout), true, &enabled),
            LayoutMode::Custom(_)
        ));
    }

    #[test]
    fn next_layout_mode_custom_starts_at_the_first_enabled_mode() {
        let enabled = EnabledLayouts {
            bsp: false,
            ..EnabledLayouts::default()
        };
        assert!(matches!(
            next_layout_mode(&LayoutMode::Custom(CustomLayout), false, &enabled),
            LayoutMode::Master(_)
        ));
    }

    #[test]
    fn next_layout_mode_with_one_enabled_mode_toggles_with_custom() {
        let enabled = EnabledLayouts {
            bsp: false,
            master: false,
            monocle: false,
            grid: false,
            columns: true,
            rows: false,
        };
        let columns = LayoutMode::Columns(ColumnsLayout);
        assert!(matches!(
            next_layout_mode(&columns, false, &enabled),
            LayoutMode::Columns(_)
        ));
        assert!(matches!(
            next_layout_mode(&columns, true, &enabled),
            LayoutMode::Custom(_)
        ));
        assert!(matches!(
            next_layout_mode(&LayoutMode::Custom(CustomLayout), true, &enabled),
            LayoutMode::Columns(_)
        ));
    }

    /// A two-pane custom stack whose traversal order differs from the caller's
    /// `pane_order` (panes `[2, 1]` vs insertion order `[1, 2]`).
    fn custom_stack_2_1() -> LayoutNode {
        LayoutNode::Stack {
            panes: vec![2, 1],
            names: vec![],
            custom_names: vec![],
            active: 0,
        }
    }

    #[test]
    fn saved_custom_restorable_ignores_pane_order() {
        // Same set, different order -> restorable.
        let saved = Some(custom_stack_2_1());
        assert!(saved_custom_is_restorable(&saved, &[1, 2]));
    }

    #[test]
    fn saved_custom_not_restorable_when_pane_added() {
        let saved = Some(custom_stack_2_1());
        assert!(!saved_custom_is_restorable(&saved, &[1, 2, 3]));
    }

    #[test]
    fn saved_custom_not_restorable_when_pane_removed() {
        let saved = Some(custom_stack_2_1());
        assert!(!saved_custom_is_restorable(&saved, &[1]));
    }

    #[test]
    fn saved_custom_not_restorable_when_none() {
        assert!(!saved_custom_is_restorable(&None, &[1, 2]));
    }

    mod framing {
        use super::super::read_message;
        use crate::protocol::{ClientMessage, MAX_FRAME_BYTES};

        #[tokio::test]
        async fn a_frame_over_the_cap_is_refused_before_it_is_read() {
            for len in [u32::MAX, MAX_FRAME_BYTES as u32 + 1] {
                let header = len.to_be_bytes();
                let mut wire: &[u8] = &header;
                // A reader holding only the header fails at the payload anyway,
                // so the error has to be the cap's, not the end of input.
                let err = read_message::<ClientMessage>(&mut wire)
                    .await
                    .expect_err("an oversized frame was accepted");
                assert!(
                    format!("{err:#}").contains("over the limit"),
                    "a {len}-byte frame got past the cap and failed later: {err:#}"
                );
            }
        }

        /// A peer that promises a frame and hangs up partway is a closed
        /// connection, and says so, rather than an I/O error about EOF.
        #[tokio::test]
        async fn a_frame_cut_short_is_a_connection_closed_mid_frame() {
            for claimed in [10u32, MAX_FRAME_BYTES as u32] {
                let mut wire = claimed.to_be_bytes().to_vec();
                wire.extend_from_slice(b"{\"Det");
                let mut reader: &[u8] = &wire;
                let err = read_message::<ClientMessage>(&mut reader)
                    .await
                    .expect_err("a truncated frame was accepted");
                assert!(
                    format!("{err:#}").contains("connection closed mid-frame (5 of"),
                    "claimed {claimed}: {err:#}"
                );
            }
        }

        #[tokio::test]
        async fn a_frame_under_the_cap_still_reads() {
            let frame = crate::protocol::encode_message(&ClientMessage::Detach).unwrap();
            let mut wire: &[u8] = &frame;
            let got = read_message::<ClientMessage>(&mut wire).await.unwrap();
            assert!(matches!(got, Some(ClientMessage::Detach)));
        }
    }

    mod identity_line {
        use std::collections::HashMap;
        use std::sync::Arc;

        use tokio::sync::Mutex;

        use super::super::{log_peer_identity, ATTRIBUTION_TIMEOUT};
        use crate::server::identity::Peer;

        #[tokio::test(start_paused = true)]
        async fn a_wedged_panes_lock_does_not_hold_up_the_connection() {
            let panes = Arc::new(Mutex::new(HashMap::new()));
            let _wedged = panes.lock().await;
            let done = tokio::time::timeout(
                ATTRIBUTION_TIMEOUT * 2,
                log_peer_identity(0, Some(1), Arc::new(Peer::unpinned()), &panes),
            )
            .await;
            assert!(done.is_ok(), "the identity line waited on the panes lock");
        }
    }

    mod mailbox_save {
        use std::sync::Arc;

        use tokio::sync::Mutex;

        use super::super::{save_mailbox_with, MAILBOX_SAVE};
        use crate::protocol::Sender;
        use crate::server::messaging::Messaging;
        use crate::server::session::ServerState;

        /// A save waits for one already in flight, so an older write can
        /// never land after a newer one, and the shutdown write never races
        /// the debounced one.
        #[tokio::test(start_paused = true)]
        async fn a_save_waits_for_the_one_in_flight() {
            let dir = std::env::temp_dir().join(format!("rmx-savelock-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let path = dir.join("mailbox.json");
            let state = Arc::new(Mutex::new(ServerState::new()));
            let store: &'static Mutex<Messaging> = Box::leak(Box::new(Mutex::new(Messaging::new(
                crate::config::messaging::MessagingConfig::default().limits(),
            ))));
            store
                .lock()
                .await
                .send(
                    Sender::Pane {
                        pane_id: 1,
                        name: None,
                        session: "main".into(),
                    },
                    2,
                    "a".into(),
                    None,
                    0,
                    std::time::Instant::now(),
                )
                .unwrap();

            let in_flight = MAILBOX_SAVE.lock().await;
            let save = {
                let path = path.clone();
                tokio::spawn(async move { save_mailbox_with(&state, store, path).await })
            };
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            assert!(
                !save.is_finished(),
                "a save ran while another held the lock"
            );
            assert!(!path.exists());
            drop(in_flight);
            save.await.unwrap();
            assert!(path.exists(), "the save did not run once the lock was free");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    mod waiting {
        use tokio::sync::Mutex;

        use super::super::wait_out;
        use crate::protocol::{Sender, ServerMessage};
        use crate::server::messaging::{Messaging, Waiter};

        fn store() -> &'static Mutex<Messaging> {
            Box::leak(Box::new(Mutex::new(Messaging::new(
                crate::config::messaging::MessagingConfig::default().limits(),
            ))))
        }

        fn waiter(
            pane: u64,
        ) -> (
            Waiter,
            tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
            tokio::sync::oneshot::Receiver<()>,
        ) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (cancel, cancelled) = tokio::sync::oneshot::channel();
            let w = Waiter {
                client_id: 1,
                pane,
                all: false,
                mark: false,
                tx,
                cancel,
            };
            (w, rx, cancelled)
        }

        /// An answered waiter's timer ends at once, without answering again;
        /// a waiter nobody answers is answered timed out at its deadline.
        #[tokio::test(start_paused = true)]
        async fn answering_a_waiter_ends_its_timer_early() {
            const HOUR_MS: u64 = 3_600_000;
            let store = store();
            let (w, mut answers, cancelled) = waiter(2);
            let id = store.lock().await.park(w).unwrap().0;
            let timer = tokio::spawn(wait_out(id, 2, HOUR_MS, cancelled, store));
            tokio::task::yield_now().await;
            let started = tokio::time::Instant::now();
            {
                let mut s = store.lock().await;
                s.send(
                    Sender::Pane {
                        pane_id: 1,
                        name: None,
                        session: "main".into(),
                    },
                    2,
                    "hi".into(),
                    None,
                    0,
                    std::time::Instant::now(),
                )
                .unwrap();
                drop(s.answer_waiters(2, 0));
            }
            assert!(
                !timer.await.unwrap(),
                "the answered waiter's timer answered it again"
            );
            assert!(
                started.elapsed() < std::time::Duration::from_millis(HOUR_MS),
                "the timer ran to its deadline"
            );
            assert!(answers.try_recv().is_err(), "a timed-out answer was sent");

            let (w, mut answers, cancelled) = waiter(3);
            let id = store.lock().await.park(w).unwrap().0;
            assert!(wait_out(id, 3, HOUR_MS, cancelled, store).await);
            assert!(matches!(
                answers.try_recv(),
                Ok(ServerMessage::MsgList {
                    timed_out: true,
                    ..
                })
            ));
        }
    }

    /// A caller whose attribution FAILED is not a caller outside every pane:
    /// it may be a pane the server was too slow to check. Its --pane requests
    /// are refused, never honoured.
    mod unverified_caller {
        use std::collections::HashMap;
        use std::sync::Arc;

        use tokio::sync::Mutex;

        use super::super::{
            handle_agent_name, handle_msg_ack, MailboxAck, NameRequest, CALLER_ATTRIBUTION_TIMEOUT,
        };
        use crate::protocol::ServerMessage;
        use crate::server::identity::Peer;
        use crate::server::messaging::Messaging;
        use crate::server::session::ServerState;

        fn refused_unverified(answer: &ServerMessage) -> bool {
            matches!(
                answer,
                ServerMessage::Error { code: Some(code), message, .. }
                    if code == "sender_unverified" && message.contains("in time")
            )
        }

        #[tokio::test(start_paused = true)]
        async fn a_timed_out_check_does_not_grant_outside_powers() {
            let state = Arc::new(Mutex::new(ServerState::new()));
            let panes = Arc::new(Mutex::new(HashMap::new()));
            let store = Mutex::new(Messaging::new(
                crate::config::messaging::MessagingConfig::default().limits(),
            ));
            let peer = Arc::new(Peer::unpinned());
            // Held for the whole test, so every attribution times out.
            let _wedged = panes.lock().await;
            let bound = CALLER_ATTRIBUTION_TIMEOUT * 4;

            let named = tokio::time::timeout(
                bound,
                handle_agent_name(
                    0,
                    NameRequest {
                        name: Some("stolen".to_string()),
                        pane: Some(5),
                        pane_hint: None,
                    },
                    &peer,
                    &state,
                    &panes,
                    &store,
                ),
            )
            .await
            .expect("the name request went on past the failed check");
            assert!(refused_unverified(&named), "{named:?}");

            let acked = tokio::time::timeout(
                bound,
                handle_msg_ack(
                    0,
                    MailboxAck {
                        pane: Some(5),
                        ids: Vec::new(),
                        all: true,
                        pane_hint: None,
                    },
                    &peer,
                    &state,
                    &panes,
                    &store,
                ),
            )
            .await
            .expect("the ack went on past the failed check");
            assert!(refused_unverified(&acked), "{acked:?}");
        }
    }

    mod focused_panes {
        use super::super::focused_panes;
        use crate::config::BorderStyle;
        use crate::server::layout::LayoutMode;
        use crate::server::session::ServerState;

        fn state_with(sessions: &[&str]) -> ServerState {
            let mut st = ServerState::new();
            for name in sessions {
                st.create_session(name, None, BorderStyle::ZellijStyle, LayoutMode::default())
                    .unwrap();
            }
            st
        }

        fn attached(names: &[&str]) -> Vec<String> {
            names.iter().map(|n| n.to_string()).collect()
        }

        #[test]
        fn the_focused_pane_of_an_attached_session_is_focused() {
            let st = state_with(&["main"]);
            let pane = st.sessions["main"].tabs[0].focused_pane;
            let focused = focused_panes(&st, &attached(&["main"]));
            assert!(focused.contains(&pane));
            assert_eq!(focused.len(), 1);
        }

        #[test]
        fn a_session_no_client_is_attached_to_marks_nothing() {
            let st = state_with(&["main", "other"]);
            let other = st.sessions["other"].tabs[0].focused_pane;
            let focused = focused_panes(&st, &attached(&["main"]));
            assert!(!focused.contains(&other));
            assert!(focused_panes(&st, &[]).is_empty());
        }

        #[test]
        fn the_focused_pane_of_a_tab_in_the_background_is_not_focused() {
            let mut st = state_with(&["main"]);
            let first_tab = st.sessions["main"].tabs[0].focused_pane;
            let second_tab = st
                .create_tab("main", "Tab 2", LayoutMode::default())
                .unwrap();
            st.sessions.get_mut("main").unwrap().active_tab = 1;
            let focused = focused_panes(&st, &attached(&["main"]));
            assert!(focused.contains(&second_tab));
            assert!(!focused.contains(&first_tab));
        }

        #[test]
        fn a_visible_popup_takes_the_focus_from_its_tab() {
            let mut st = state_with(&["main"]);
            let tab_pane = st.sessions["main"].tabs[0].focused_pane;
            let tab = &mut st.sessions.get_mut("main").unwrap().tabs[0];
            tab.popup_pane = Some(999);
            tab.popup_visible = true;
            let focused = focused_panes(&st, &attached(&["main"]));
            assert!(focused.contains(&999));
            assert!(!focused.contains(&tab_pane));
        }

        #[test]
        fn clients_on_different_sessions_each_mark_their_own_pane() {
            let st = state_with(&["main", "other"]);
            let main = st.sessions["main"].tabs[0].focused_pane;
            let other = st.sessions["other"].tabs[0].focused_pane;
            let focused = focused_panes(&st, &attached(&["main", "other"]));
            assert!(focused.contains(&main));
            assert!(focused.contains(&other));
        }

        #[test]
        fn a_client_attached_to_a_session_that_is_gone_marks_nothing() {
            let st = state_with(&["main"]);
            assert!(focused_panes(&st, &attached(&["deleted"])).is_empty());
        }
    }

    /// `process_pgid` through the real `/proc` read, which only exists on Linux.
    #[cfg(target_os = "linux")]
    mod stat {
        #[test]
        fn a_live_process_whose_comm_is_not_utf8_reports_its_group() {
            let child = crate::server::procfs::testing::TruncatedCommChild::spawn("comm");
            assert!(
                child.comm_is_truncated(),
                "the fixture's comm is not the truncated name, so it tests nothing: {:?}",
                child.comm
            );
            assert_eq!(
                super::super::process_pgid(child.pid.as_raw()),
                Some(nix::unistd::getpgrp().as_raw())
            );
        }
    }
}

/// Tests for where the popup is sized, drawn and clicked, and for the per-tab
/// popup lifecycle through the real command handlers.
#[cfg(test)]
mod popup_tests {
    use super::*;
    use crate::server::layout::LayoutMode;

    fn config() -> Config {
        Config::default()
    }

    fn cell(c: char) -> RenderCell {
        RenderCell {
            c,
            ..RenderCell::default()
        }
    }

    fn layer(cols: u16, rows: u16, fill: char) -> PopupLayer {
        PopupLayer {
            pane_id: 9,
            cols,
            rows,
            cells: vec![vec![cell(fill); cols as usize]; rows as usize],
            cursor_x: 1,
            cursor_y: 0,
            cursor_visible: true,
            cursor_style: 3,
        }
    }

    fn blank_frame(cols: u16, rows: u16) -> Vec<Vec<RenderCell>> {
        vec![vec![cell('.'); cols as usize]; rows as usize]
    }

    fn meta() -> FrameMeta {
        FrameMeta {
            application_cursor_keys: false,
            bracketed_paste: true,
            viewport_top: 0,
            scroll_offset: 0,
            status: None,
            live_popups: Vec::new(),
            float_zoom: false,
        }
    }

    fn place<'a>(
        cells: &'a [Vec<RenderCell>],
        layer: Option<&'a PopupLayer>,
        asking: bool,
    ) -> PlacedFrame<'a> {
        place_popup(
            cells,
            (5, 6, true, 1),
            Some(PaneRect {
                x: 1,
                y: 1,
                width: 4,
                height: 4,
                neighbors: FocusNeighbors::default(),
            }),
            layer,
            asking,
            (cells[0].len() as u16, cells.len() as u16),
            "NORMAL",
            &CompositorTheme::default(),
            &config(),
        )
    }

    // -- Shared popup interior ------------------------------------------------

    #[test]
    fn the_shared_interior_is_the_smallest_any_client_asks_for() {
        let cfg = config();
        let got = shared_popup_interior(&[Some((100, 30)), Some((80, 40))], (160, 48), &cfg);
        assert_eq!(got, (80, 30));
    }

    #[test]
    fn a_legacy_client_contributes_its_framed_floats_interior() {
        let cfg = config();
        // popup_rect of a 160x47 area at 80% is 128x37; the frame takes 2 each.
        assert_eq!(legacy_popup_interior(160, 48, &cfg), (126, 35));
        let got = shared_popup_interior(&[None, Some((200, 100))], (160, 48), &cfg);
        assert_eq!(got, (126, 35));
        let got = shared_popup_interior(&[None, Some((50, 10))], (160, 48), &cfg);
        assert_eq!(got, (50, 10));
    }

    #[test]
    fn with_no_client_the_shared_interior_falls_back_to_the_80x24_float() {
        let cfg = config();
        assert_eq!(
            shared_popup_interior(&[], (80, 24), &cfg),
            legacy_popup_interior(80, 24, &cfg)
        );
    }

    // -- Placing the popup ----------------------------------------------------

    #[test]
    fn no_popup_leaves_the_frame_borrowed_and_untouched() {
        let cells = blank_frame(20, 10);
        let placed = place(&cells, None, true);
        assert!(matches!(placed.cells, std::borrow::Cow::Borrowed(_)));
        assert_eq!(
            (placed.cursor_x, placed.cursor_y, placed.cursor_visible),
            (5, 6, true)
        );
        assert!(placed.focused_pane_rect.is_some());
        assert!(placed.layer.is_none());
    }

    #[test]
    fn a_client_that_asked_keeps_the_frame_and_gets_the_layer_beside_it() {
        let cells = blank_frame(20, 10);
        let l = layer(6, 3, 'x');
        let placed = place(&cells, Some(&l), true);
        assert!(matches!(placed.cells, std::borrow::Cow::Borrowed(_)));
        assert_eq!(placed.cells.as_ref(), cells.as_slice());
        assert!(placed.layer.is_some());
        // The layer owns the cursor and the focused pane rect.
        assert!(!placed.cursor_visible);
        assert_eq!(placed.focused_pane_rect, None);
    }

    #[test]
    fn a_legacy_client_gets_the_framed_float_with_the_layer_top_left_in_it() {
        let cells = blank_frame(60, 20);
        let l = layer(6, 3, 'x');
        let placed = place(&cells, Some(&l), false);
        assert!(placed.layer.is_none(), "the layer is drawn into the frame");
        // popup_rect of 60x19 at 80% is 48x15 at (6, 2); interior (7,3) 46x13.
        let interior_x = 7usize;
        let interior_y = 3usize;
        for dy in 0..3 {
            for dx in 0..6 {
                assert_eq!(placed.cells[interior_y + dy][interior_x + dx].c, 'x');
            }
        }
        // The interior beyond the (smaller) layer is the clear fill, not the
        // layout underneath.
        assert_eq!(placed.cells[interior_y][interior_x + 6].c, ' ');
        assert_eq!(placed.cells[interior_y + 3][interior_x].c, ' ');
        // The frame is drawn around it.
        assert_eq!(placed.cells[2][6].c, '\u{256D}');
        // The layout outside the box is untouched.
        assert_eq!(placed.cells[0][0].c, '.');
        // The cursor and focused rect are the popup's.
        assert_eq!(
            (placed.cursor_x, placed.cursor_y),
            (interior_x as u16 + 1, interior_y as u16)
        );
        assert_eq!(placed.cursor_style, 3);
        let fpr = placed.focused_pane_rect.expect("popup interior");
        assert_eq!((fpr.x, fpr.y), (7, 3));
    }

    // -- What a frame carries -------------------------------------------------

    fn prev_of(placed: PlacedFrame<'_>) -> PrevFrame {
        frame_message(placed, &meta(), None, "test").1
    }

    #[test]
    fn a_first_frame_is_full_and_carries_the_whole_layer() {
        let cells = blank_frame(20, 10);
        let l = layer(4, 2, 'x');
        let (msg, stored) = frame_message(place(&cells, Some(&l), true), &meta(), None, "test");
        let ServerMessage::FullRender { popup, .. } = msg else {
            panic!("expected a FullRender");
        };
        let popup = popup.expect("layer");
        assert_eq!((popup.cols, popup.rows), (4, 2));
        assert_eq!(popup.cells, Some(l.cells.clone()));
        assert_eq!(stored.popup, Some(((4, 2), l.cells.clone())));
    }

    #[test]
    fn the_same_size_layer_is_sent_as_a_diff() {
        let cells = blank_frame(20, 10);
        let l = layer(40, 20, 'x');
        let prev = prev_of(place(&cells, Some(&l), true));
        let mut changed = l.clone();
        changed.cells[0][0] = cell('y');
        let (msg, _) = frame_message(
            place(&cells, Some(&changed), true),
            &meta(),
            Some(&prev),
            "t",
        );
        let ServerMessage::RenderDiff { popup, changes, .. } = msg else {
            panic!("expected a RenderDiff");
        };
        assert!(changes.is_empty(), "the frame itself did not change");
        let popup = popup.expect("layer");
        assert_eq!(popup.cells, None);
        assert_eq!(popup.changes.len(), 1);
        assert_eq!((popup.changes[0].x, popup.changes[0].y), (0, 0));
    }

    #[test]
    fn a_layer_that_changes_size_forces_a_full_render_with_the_whole_layer() {
        let cells = blank_frame(20, 10);
        let prev = prev_of(place(&cells, Some(&layer(4, 2, 'x')), true));
        let bigger = layer(5, 3, 'x');
        let (msg, _) = frame_message(
            place(&cells, Some(&bigger), true),
            &meta(),
            Some(&prev),
            "t",
        );
        let ServerMessage::FullRender { popup, .. } = msg else {
            panic!("a size change must repaint the uncovered region");
        };
        assert_eq!(popup.expect("layer").cells, Some(bigger.cells));
    }

    #[test]
    fn a_layer_that_appears_or_disappears_forces_a_full_render() {
        let cells = blank_frame(20, 10);
        let none = prev_of(place(&cells, None, true));
        let l = layer(4, 2, 'x');
        let (msg, stored) = frame_message(place(&cells, Some(&l), true), &meta(), Some(&none), "t");
        assert!(matches!(msg, ServerMessage::FullRender { .. }), "appear");
        let (msg, _) = frame_message(place(&cells, None, true), &meta(), Some(&stored), "t");
        let ServerMessage::FullRender { popup, .. } = msg else {
            panic!("disappear must be a FullRender");
        };
        assert_eq!(popup, None, "a hidden popup travels as None");
    }

    #[test]
    fn an_unchanged_frame_and_layer_diff_to_nothing() {
        let cells = blank_frame(20, 10);
        let l = layer(4, 2, 'x');
        let prev = prev_of(place(&cells, Some(&l), true));
        let (msg, _) = frame_message(place(&cells, Some(&l), true), &meta(), Some(&prev), "t");
        let ServerMessage::RenderDiff { changes, popup, .. } = msg else {
            panic!("expected a RenderDiff");
        };
        assert!(changes.is_empty());
        let popup = popup.expect("layer");
        assert!(popup.changes.is_empty() && popup.cells.is_none());
    }

    #[test]
    fn a_legacy_frames_baseline_is_the_blitted_frame() {
        let cells = blank_frame(60, 20);
        let l = layer(6, 3, 'x');
        let prev = prev_of(place(&cells, Some(&l), false));
        assert_eq!(prev.popup, None);
        assert_eq!(prev.cells[3][7].c, 'x', "the popup is in the stored frame");
        // The same popup again diffs to nothing against that baseline.
        let (msg, _) = frame_message(place(&cells, Some(&l), false), &meta(), Some(&prev), "t");
        let ServerMessage::RenderDiff { changes, popup, .. } = msg else {
            panic!("expected a RenderDiff");
        };
        assert!(changes.is_empty());
        assert_eq!(popup, None);
    }

    // -- The ScrollRender gate ------------------------------------------------

    #[test]
    fn scroll_render_is_withheld_from_a_client_that_places_the_popup_itself() {
        let cells = blank_frame(60, 20);
        let l = layer(6, 3, 'x');
        let legacy = place(&cells, Some(&l), false);
        let asking = place(&cells, Some(&l), true);
        // A step of 1 within a pane taller than the step.
        assert!(scroll_render_allowed(true, 3, 2, 10, 1, &legacy));
        assert!(!scroll_render_allowed(true, 3, 2, 10, 1, &asking));
        // With no popup an asking client scrolls incrementally as before.
        let none = place(&cells, None, true);
        assert!(scroll_render_allowed(true, 3, 2, 10, 1, &none));
    }

    // -- Mouse ----------------------------------------------------------------

    #[test]
    fn a_popup_click_is_interior_local_with_zero_inset() {
        let l = layer(40, 10, 'x');
        let popup = mouse_popup(Some(&l), true, true, (160, 48), &config());
        assert!(popup.local);
        let (rect, inset) = popup.hit.expect("local rect");
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (0, 0, 40, 10));
        assert_eq!(inset, 0);
        // (0,0) is the first cell, so the 1-based report is (1,1).
        assert_eq!(report_coords(rect, inset, 0, 0), (1, 1));
        assert_eq!(report_coords(rect, inset, 3, 2), (4, 3));
    }

    #[test]
    fn a_screen_click_from_a_client_that_placed_the_popup_has_no_popup_rect() {
        let l = layer(40, 10, 'x');
        let popup = mouse_popup(Some(&l), false, true, (160, 48), &config());
        assert_eq!(popup.pane, Some(9), "the popup still owns input");
        assert!(popup.hit.is_none());
        let regions = HitRegions::default();
        let rects = [(
            1,
            Rect {
                x: 0,
                y: 0,
                width: 160,
                height: 47,
            },
        )];
        assert_eq!(
            hit_test_with_popup(10, 10, &regions, &rects, &popup),
            ClickTarget::Pane(1),
            "the layout underneath, not the popup"
        );
    }

    #[test]
    fn a_legacy_client_hits_the_composite_float_with_its_frame_inset() {
        let l = layer(40, 10, 'x');
        let popup = mouse_popup(Some(&l), false, false, (160, 48), &config());
        let (rect, inset) = popup.hit.expect("legacy rect");
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (16, 5, 128, 37));
        assert_eq!(inset, 1);
        let regions = HitRegions::default();
        assert_eq!(
            hit_test_with_popup(20, 10, &regions, &[], &popup),
            ClickTarget::Pane(9)
        );
    }

    #[test]
    fn a_local_event_without_a_layer_is_not_treated_as_a_popup() {
        let popup = mouse_popup(None, true, true, (160, 48), &config());
        assert!(!popup.local);
        assert_eq!(popup.pane, None);
    }

    // -- Real handlers, real PTYs ---------------------------------------------

    struct Rig {
        state: Arc<Mutex<ServerState>>,
        panes: Arc<Mutex<HashMap<PaneId, PaneData>>>,
        clients: Arc<Mutex<HashMap<u64, ClientConnection>>>,
        config: Arc<Config>,
        prev_frames: PrevFrameCache,
        dormant: DormantStore,
        rx: mpsc::UnboundedReceiver<ServerMessage>,
    }

    const CLIENT: u64 = 1;

    /// A client attached to `session`, as the handshake leaves one.
    fn connection(
        session: &str,
        cols: u16,
        rows: u16,
        want: Option<(u16, u16)>,
        tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> ClientConnection {
        ClientConnection {
            session_name: Some(session.to_string()),
            tx,
            cols,
            rows,
            mode: "NORMAL".to_string(),
            mouse_selection: None,
            search_info: None,
            session_scroll: HashMap::new(),
            prev_scroll_offset: 0,
            prev_scroll_pane: None,
            needs_full_render: false,
            drag: None,
            autoscroll_repeat: None,
            subscribed_panes: HashMap::new(),
            pane_scroll: HashMap::new(),
            pane_selection: HashMap::new(),
            pane_drag: None,
            pane_autoscroll_repeat: None,
            session_tree_subscribed: false,
            agents_subscribed: false,
            naming: None,
            tab_style: None,
            closer: None,
            popup_want: want,
            right_press: None,
        }
    }

    impl Rig {
        /// One session "main" whose first pane runs `/bin/sh` in `cwd`, and one
        /// attached client of `cols` x `rows`.
        async fn new(cols: u16, rows: u16, want: Option<(u16, u16)>, cwd: &std::path::Path) -> Rig {
            let mut cfg = Config::default();
            cfg.general.default_shell = Some("/bin/sh".to_string());
            let config = Arc::new(cfg);
            let state = Arc::new(Mutex::new(ServerState::new()));
            let panes = Arc::new(Mutex::new(HashMap::new()));
            let pane_id = state
                .lock()
                .await
                .create_session(
                    "main",
                    None,
                    BorderStyle::ZellijStyle,
                    LayoutMode::default(),
                )
                .expect("session");
            spawn_pane(
                pane_id,
                cols,
                rows - 1,
                None,
                &[],
                Some(cwd),
                Some(PaneIdentity {
                    session: "main",
                    pane: pane_id,
                }),
                &panes,
                &config,
            )
            .await
            .expect("spawn");
            let (tx, rx) = mpsc::unbounded_channel();
            let clients = Arc::new(Mutex::new(HashMap::new()));
            clients
                .lock()
                .await
                .insert(CLIENT, connection("main", cols, rows, want, tx));
            Rig {
                state,
                panes,
                clients,
                config,
                prev_frames: Arc::new(Mutex::new(HashMap::new())),
                dormant: Arc::new(Mutex::new(PersistedState {
                    state: ServerState::new(),
                    pane_cwds: HashMap::new(),
                })),
                rx,
            }
        }

        async fn command(&self, cmd: RemuxCommand) {
            handle_command(
                CLIENT,
                cmd,
                &self.state,
                &self.panes,
                &self.clients,
                &self.config,
                &self.prev_frames,
                &self.dormant,
            )
            .await
            .expect("command");
        }

        /// A second tab whose pane runs `/bin/sh` in `cwd`; it becomes active.
        async fn new_tab(&self, cwd: &std::path::Path) -> PaneId {
            let pane_id = self
                .state
                .lock()
                .await
                .create_tab("main", "Tab 2", LayoutMode::default())
                .expect("tab");
            let (cols, rows) = {
                let cls = self.clients.lock().await;
                (cls[&CLIENT].cols, cls[&CLIENT].rows)
            };
            spawn_pane(
                pane_id,
                cols,
                rows - 1,
                None,
                &[],
                Some(cwd),
                Some(PaneIdentity {
                    session: "main",
                    pane: pane_id,
                }),
                &self.panes,
                &self.config,
            )
            .await
            .expect("spawn");
            pane_id
        }

        /// Attach another client of `cols` x `rows` to "main". Returns what it
        /// is sent.
        async fn add_client(
            &self,
            id: u64,
            cols: u16,
            rows: u16,
            want: Option<(u16, u16)>,
        ) -> mpsc::UnboundedReceiver<ServerMessage> {
            let (tx, rx) = mpsc::unbounded_channel();
            self.clients
                .lock()
                .await
                .insert(id, connection("main", cols, rows, want, tx));
            rx
        }

        /// Split tab 0's focused pane with a pane running `/bin/sh` in `cwd`.
        /// The new pane is focused, as a split focuses it.
        async fn split(&self, cwd: &std::path::Path) -> PaneId {
            self.split_tab(0, cwd).await
        }

        /// [`Rig::split`] on tab `tab`.
        async fn split_tab(&self, tab: usize, cwd: &std::path::Path) -> PaneId {
            let pane_id = {
                let mut st = self.state.lock().await;
                let pane_id = st.next_pane_id();
                let tab = &mut st.sessions.get_mut("main").unwrap().tabs[tab];
                let focused = tab.focused_pane;
                tab.layout.split_vertical(focused, pane_id);
                tab.pane_order.push(pane_id);
                tab.focus_pane(pane_id);
                pane_id
            };
            spawn_pane(
                pane_id,
                50,
                29,
                None,
                &[],
                Some(cwd),
                Some(PaneIdentity {
                    session: "main",
                    pane: pane_id,
                }),
                &self.panes,
                &self.config,
            )
            .await
            .expect("spawn");
            pane_id
        }

        /// A second session whose pane runs `/bin/sh` in `cwd`.
        async fn new_session(&self, name: &str, cwd: &std::path::Path) {
            let pane_id = self
                .state
                .lock()
                .await
                .create_session(name, None, BorderStyle::ZellijStyle, LayoutMode::default())
                .expect("session");
            spawn_pane(
                pane_id,
                100,
                29,
                None,
                &[],
                Some(cwd),
                Some(PaneIdentity {
                    session: name,
                    pane: pane_id,
                }),
                &self.panes,
                &self.config,
            )
            .await
            .expect("spawn");
        }

        async fn popup_of(&self, tab: usize) -> Option<PaneId> {
            self.state.lock().await.sessions["main"].tabs[tab].popup_pane
        }

        async fn screen_size(&self, pane: PaneId) -> (u16, u16) {
            let ps = self.panes.lock().await;
            let s = &ps[&pane].screen;
            (s.cols, s.rows)
        }

        fn drain(&mut self) -> Vec<ServerMessage> {
            let mut out = Vec::new();
            while let Ok(m) = self.rx.try_recv() {
                out.push(m);
            }
            out
        }

        async fn finish(self) {
            let ids: Vec<PaneId> = self.panes.lock().await.keys().copied().collect();
            reap_panes(&ids, &self.panes, &self.clients).await;
        }
    }

    fn canonical(p: &std::path::Path) -> std::path::PathBuf {
        std::fs::canonicalize(p).expect("canonicalize")
    }

    /// The cwd of `pane`'s shell, once it reads as `dir`, or what it last read
    /// after a few seconds. A pane's shell changes directory after the fork,
    /// so a read straight after `spawn_pane` can still see the daemon's own.
    async fn cwd_once_in(
        rig: &Rig,
        pane: PaneId,
        dir: &std::path::Path,
    ) -> Option<std::path::PathBuf> {
        let want = canonical(dir);
        let mut last = None;
        for _ in 0..100 {
            last = {
                let ps = rig.panes.lock().await;
                persistence::get_pane_cwd(ps[&pane].pty.child_pid)
            }
            .map(|c| canonical(std::path::Path::new(&c)));
            if last.as_ref() == Some(&want) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        last
    }

    #[tokio::test]
    async fn toggling_on_two_tabs_spawns_two_popups_in_each_tabs_focused_cwd() {
        let a = tempfile::tempdir().expect("dir a");
        let b = tempfile::tempdir().expect("dir b");
        let rig = Rig::new(100, 30, None, a.path()).await;

        let pane_a = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        assert_eq!(
            cwd_once_in(&rig, pane_a, a.path()).await,
            Some(canonical(a.path()))
        );
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let first = rig.popup_of(0).await.expect("tab 0 popup");
        let pane_b = rig.new_tab(b.path()).await;
        assert_eq!(
            cwd_once_in(&rig, pane_b, b.path()).await,
            Some(canonical(b.path()))
        );
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let second = rig.popup_of(1).await.expect("tab 1 popup");
        assert_ne!(first, second);

        assert_eq!(
            cwd_once_in(&rig, first, a.path()).await,
            Some(canonical(a.path()))
        );
        assert_eq!(
            cwd_once_in(&rig, second, b.path()).await,
            Some(canonical(b.path()))
        );

        // Leaving tab 1 and coming back finds both popups as they were.
        rig.command(RemuxCommand::TabGoto(0)).await;
        let st = rig.state.lock().await;
        let sess = &st.sessions["main"];
        assert_eq!(sess.visible_popup(), Some(first));
        assert!(sess.tabs[1].popup_visible);
        drop(st);
        rig.finish().await;
    }

    #[tokio::test]
    async fn closing_the_popup_returns_input_to_the_pane_focused_before_it() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let left = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        let middle = rig.split(dir.path()).await;
        rig.split(dir.path()).await;
        let focused = || {
            let state = rig.state.clone();
            async move { state.lock().await.sessions["main"].tabs[0].focused_pane }
        };
        let focus = |pane: PaneId| {
            let state = rig.state.clone();
            async move {
                state.lock().await.sessions.get_mut("main").unwrap().tabs[0].focus_pane(pane);
            }
        };
        let click_left = || {
            let (state, panes, clients, config, pf) = (
                rig.state.clone(),
                rig.panes.clone(),
                rig.clients.clone(),
                rig.config.clone(),
                rig.prev_frames.clone(),
            );
            async move {
                for release in [false, true] {
                    handle_mouse_click(
                        CLIENT, 2, 5, release, false, &state, &panes, &clients, &config, &pf,
                    )
                    .await
                    .expect("click");
                }
            }
        };
        let switch_to_left = RemuxCommand::SessionSwitchPane {
            session: "main".to_string(),
            tab_index: 0,
            pane_id: left,
        };

        // Each action does move the focus while no popup is up, so a pass below
        // is not a miss.
        focus(middle).await;
        rig.command(RemuxCommand::PaneFocusLeft).await;
        assert_eq!(focused().await, left, "a focus key");
        focus(middle).await;
        click_left().await;
        assert_eq!(focused().await, left, "a click on a pane");
        focus(middle).await;
        rig.command(switch_to_left.clone()).await;
        assert_eq!(focused().await, left, "an agent or session-tree switch");

        focus(middle).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        for cmd in [
            RemuxCommand::PaneStackNext,
            RemuxCommand::PaneStackPrev,
            RemuxCommand::SetMaster,
            RemuxCommand::LayoutNext,
            switch_to_left.clone(),
            RemuxCommand::SessionSwitchTab {
                session: "main".to_string(),
                tab_index: 0,
            },
        ] {
            rig.command(cmd.clone()).await;
            assert_eq!(focused().await, middle, "{cmd:?} while the popup is up");
        }
        // A tab switch away and back.
        rig.new_tab(dir.path()).await;
        rig.command(RemuxCommand::TabGoto(0)).await;
        assert_eq!(focused().await, middle, "a tab switch away and back");

        rig.command(RemuxCommand::PopupToggle(None)).await;
        let st = rig.state.lock().await;
        let tab = &st.sessions["main"].tabs[0];
        assert!(!tab.popup_visible);
        assert_eq!(
            tab.input_target(),
            middle,
            "input is back on the middle pane"
        );
        drop(st);
        rig.finish().await;
    }

    /// Three panes side by side in tab 0, `(left, middle, right)`, with the
    /// middle one focused and the popup up. A click in the left pane lands at
    /// `LEFT_X` and one in the right pane at `RIGHT_X`.
    async fn three_panes_popup_up(rig: &Rig, cwd: &std::path::Path) -> (PaneId, PaneId, PaneId) {
        let left = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        let middle = rig.split(cwd).await;
        let right = rig.split(cwd).await;
        rig.state
            .lock()
            .await
            .sessions
            .get_mut("main")
            .unwrap()
            .tabs[0]
            .focus_pane(middle);
        rig.command(RemuxCommand::PopupToggle(None)).await;
        (left, middle, right)
    }

    const LEFT_X: u16 = 2;
    const RIGHT_X: u16 = 90;

    async fn press(rig: &Rig, x: u16, y: u16, popup: bool) {
        handle_mouse_click(
            CLIENT,
            x,
            y,
            false,
            popup,
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await
        .expect("click");
    }

    async fn focus_and_popup(rig: &Rig) -> (PaneId, bool) {
        let st = rig.state.lock().await;
        let tab = &st.sessions["main"].tabs[0];
        (tab.focused_pane, tab.popup_visible)
    }

    #[tokio::test]
    async fn a_click_outside_the_popup_closes_it_and_focuses_the_pane_clicked() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let (_left, middle, right) = three_panes_popup_up(&rig, dir.path()).await;
        assert_eq!(focus_and_popup(&rig).await, (middle, true));

        press(&rig, RIGHT_X, 5, false).await;

        assert_eq!(focus_and_popup(&rig).await, (right, false));
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_click_in_the_popups_interior_stays_with_the_popup() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let (_left, middle, _right) = three_panes_popup_up(&rig, dir.path()).await;

        press(&rig, 3, 3, true).await;

        assert_eq!(focus_and_popup(&rig).await, (middle, true));
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_release_outside_the_popup_does_not_close_it() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let (_left, middle, _right) = three_panes_popup_up(&rig, dir.path()).await;

        handle_mouse_click(
            CLIENT,
            LEFT_X,
            5,
            true,
            false,
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await
        .expect("release");

        assert_eq!(focus_and_popup(&rig).await, (middle, true));
        rig.finish().await;
    }

    #[tokio::test]
    async fn the_frame_of_a_server_drawn_popup_swallows_a_click_and_outside_closes_it() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, _right) = three_panes_popup_up(&rig, dir.path()).await;
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 29,
        };
        let rect = layout::popup_rect(area, legacy_popup_pct(&rig.config));

        press(&rig, rect.x, rect.y, false).await;
        assert_eq!(
            focus_and_popup(&rig).await,
            (middle, true),
            "a click on the frame"
        );

        assert!(LEFT_X < rect.x, "the left pane is outside the box");
        press(&rig, LEFT_X, 5, false).await;
        assert_eq!(
            focus_and_popup(&rig).await,
            (left, false),
            "a click outside the box"
        );
        rig.finish().await;
    }

    fn renders(msgs: &[ServerMessage]) -> usize {
        msgs.iter()
            .filter(|m| {
                matches!(
                    m,
                    ServerMessage::FullRender { .. } | ServerMessage::RenderDiff { .. }
                )
            })
            .count()
    }

    #[tokio::test]
    async fn a_press_off_every_region_only_hides_the_popup() {
        for want in [Some((50, 10)), None] {
            let dir = tempfile::tempdir().expect("dir");
            let mut rig = Rig::new(100, 30, want, dir.path()).await;
            let (_left, middle, _right) = three_panes_popup_up(&rig, dir.path()).await;

            rig.drain();
            press(&rig, u16::MAX, u16::MAX, false).await;
            assert_eq!(focus_and_popup(&rig).await, (middle, false), "{want:?}");
            assert_eq!(renders(&rig.drain()), 1, "the frame without the popup");

            // With the popup already hidden, the same press changes nothing:
            // it must never open it, and there is nothing to render.
            press(&rig, u16::MAX, u16::MAX, false).await;
            assert_eq!(focus_and_popup(&rig).await, (middle, false), "{want:?}");
            assert_eq!(renders(&rig.drain()), 0, "{want:?}");
            rig.finish().await;
        }
    }

    #[tokio::test]
    async fn a_press_off_every_region_repaints_a_client_scrolled_under_the_popup() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let (_left, middle, _right) = three_panes_popup_up(&rig, dir.path()).await;
        {
            let mut ps = rig.panes.lock().await;
            let lines: String = (0..80).map(|i| format!("line {i}\r\n")).collect();
            ps.get_mut(&middle)
                .unwrap()
                .screen
                .process_output(lines.as_bytes());
        }
        rig.clients
            .lock()
            .await
            .get_mut(&CLIENT)
            .unwrap()
            .set_scroll(middle, 3);
        rig.drain();

        press(&rig, u16::MAX, u16::MAX, false).await;

        assert!(!focus_and_popup(&rig).await.1);
        assert_eq!(
            renders(&rig.drain()),
            1,
            "the scrolled requester is sent the frame without the popup"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_click_on_a_tab_switches_to_it_and_leaves_the_popup_of_the_tab_it_left() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        rig.new_tab(dir.path()).await;
        rig.command(RemuxCommand::TabGoto(0)).await;
        assert!(rig.state.lock().await.sessions["main"].tabs[0].popup_visible);

        let tab_x = {
            let (cols, rows, mode, naming, theme) = {
                let cls = rig.clients.lock().await;
                let c = &cls[&CLIENT];
                (
                    c.cols,
                    c.rows,
                    c.mode.clone(),
                    c.naming(),
                    c.theme(&rig.config),
                )
            };
            let hit = build_composite(
                "main",
                cols,
                rows,
                &mode,
                &rig.state,
                &rig.panes,
                &rig.config,
                None,
                None,
                &HashMap::new(),
                &theme,
                &naming,
            )
            .await
            .6;
            let region = hit
                .tab_regions
                .iter()
                .find(|r| r.tab_index == 1)
                .expect("tab 1 label");
            (region.x_start, region.y)
        };

        press(&rig, tab_x.0, tab_x.1, false).await;

        let st = rig.state.lock().await;
        let sess = &st.sessions["main"];
        assert_eq!(sess.active_tab, 1, "the click switched tabs");
        assert!(
            sess.tabs[0].popup_visible,
            "the tab left keeps its popup, as a keyboard tab switch does"
        );
        drop(st);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_focus_key_with_the_popup_up_renders_once() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        three_panes_popup_up(&rig, dir.path()).await;
        rig.drain();

        rig.command(RemuxCommand::PaneFocusRight).await;

        assert_eq!(renders(&rig.drain()), 1);
        rig.finish().await;
    }

    #[tokio::test]
    async fn an_outside_click_renders_once() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        three_panes_popup_up(&rig, dir.path()).await;
        // On the pane that already has focus, and on another one.
        for x in [60, RIGHT_X] {
            rig.state
                .lock()
                .await
                .sessions
                .get_mut("main")
                .unwrap()
                .tabs[0]
                .popup_visible = true;
            rig.drain();
            press(&rig, x, 5, false).await;
            assert_eq!(renders(&rig.drain()), 1, "click at x={x}");
            assert!(!focus_and_popup(&rig).await.1);
        }
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_focus_key_closes_the_popup_and_steps_from_the_pane_focused_before_it() {
        for (cmd, expect_right) in [
            (RemuxCommand::PaneFocusRight, true),
            (RemuxCommand::PaneFocusLeft, false),
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
            let (left, _middle, right) = three_panes_popup_up(&rig, dir.path()).await;

            rig.command(cmd.clone()).await;

            let want = if expect_right { right } else { left };
            assert_eq!(focus_and_popup(&rig).await, (want, false), "{cmd:?}");
            rig.finish().await;
        }
    }

    #[tokio::test]
    async fn a_focus_key_with_no_neighbour_closes_the_popup_and_keeps_the_pane() {
        for cmd in [
            RemuxCommand::PaneFocusRight,
            RemuxCommand::PaneFocusUp,
            RemuxCommand::PaneFocusDown,
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
            let (_left, _middle, right) = three_panes_popup_up(&rig, dir.path()).await;
            rig.state
                .lock()
                .await
                .sessions
                .get_mut("main")
                .unwrap()
                .tabs[0]
                .focus_pane(right);

            rig.command(cmd.clone()).await;

            assert_eq!(focus_and_popup(&rig).await, (right, false), "{cmd:?}");
            rig.finish().await;
        }
    }

    #[tokio::test]
    async fn a_pane_closing_that_was_not_focused_leaves_the_focus_alone() {
        for popup_up in [false, true] {
            let dir = tempfile::tempdir().expect("dir");
            let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
            // The right pane's survivor in the tree is the middle one, so a close
            // that refocused the survivor would land there, not on the left.
            let left = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
            rig.split(dir.path()).await;
            let right = rig.split(dir.path()).await;
            let focus = rig.state.clone();
            focus.lock().await.sessions.get_mut("main").unwrap().tabs[0].focus_pane(left);
            if popup_up {
                rig.command(RemuxCommand::PopupToggle(None)).await;
            }
            close_pane(
                right,
                &rig.state,
                &rig.panes,
                &rig.clients,
                &rig.config,
                &rig.prev_frames,
            )
            .await;
            let st = rig.state.lock().await;
            let tab = &st.sessions["main"].tabs[0];
            assert!(!tab.pane_order.contains(&right));
            assert_eq!(tab.focused_pane, left, "popup up: {popup_up}");
            drop(st);
            rig.finish().await;
        }
    }

    #[tokio::test]
    async fn closing_a_background_tabs_unfocused_pane_leaves_its_focus_alone() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let first = rig.new_tab(dir.path()).await;
        rig.split_tab(1, dir.path()).await;
        let third = rig.split_tab(1, dir.path()).await;
        let focus = rig.state.clone();
        focus.lock().await.sessions.get_mut("main").unwrap().tabs[1].focus_pane(first);
        rig.command(RemuxCommand::TabGoto(0)).await;
        rig.command(RemuxCommand::PaneCloseById {
            session: "main".to_string(),
            pane_id: third,
        })
        .await;
        let st = rig.state.lock().await;
        let tab = &st.sessions["main"].tabs[1];
        assert!(!tab.pane_order.contains(&third));
        assert_eq!(tab.focused_pane, first);
        drop(st);
        rig.finish().await;
    }

    #[tokio::test]
    async fn pane_new_in_tab_with_its_popup_up_leaves_the_focus_alone() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let before = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        rig.command(RemuxCommand::PaneNewInTab {
            session: "main".to_string(),
            tab_index: 0,
        })
        .await;
        let st = rig.state.lock().await;
        let tab = &st.sessions["main"].tabs[0];
        assert_eq!(tab.pane_order.len(), 2, "the pane is created");
        assert_eq!(tab.focused_pane, before);
        drop(st);
        rig.finish().await;
    }

    #[tokio::test]
    async fn every_frame_lists_the_sessions_live_popups() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let first = rig.popup_of(0).await.expect("popup");
        rig.new_tab(dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let second = rig.popup_of(1).await.expect("popup");
        // Hidden: still alive, so still listed.
        rig.command(RemuxCommand::PopupToggle(None)).await;
        rig.drain();
        rig.command(RemuxCommand::TabGoto(0)).await;
        let mut live = rig
            .drain()
            .into_iter()
            .filter_map(|m| match m {
                ServerMessage::FullRender { live_popups, .. }
                | ServerMessage::RenderDiff { live_popups, .. } => Some(live_popups),
                _ => None,
            })
            .next_back()
            .expect("a frame");
        live.sort();
        assert_eq!(live, vec![first, second]);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_popup_that_fails_to_spawn_leaves_no_popup_behind() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let result = SPAWN_FAILS
            .scope(
                true,
                handle_command(
                    CLIENT,
                    RemuxCommand::PopupToggle(None),
                    &rig.state,
                    &rig.panes,
                    &rig.clients,
                    &rig.config,
                    &rig.prev_frames,
                    &rig.dormant,
                ),
            )
            .await;
        assert!(result.is_err(), "the failure is still reported");
        {
            let st = rig.state.lock().await;
            let tab = &st.sessions["main"].tabs[0];
            assert_eq!(tab.popup_pane, None);
            assert!(!tab.popup_visible);
        }
        // With the phantom gone, the layout is not locked and the next toggle
        // spawns a popup.
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let popup = rig.popup_of(0).await.expect("a fresh popup");
        assert!(rig.panes.lock().await.contains_key(&popup));
        rig.finish().await;
    }

    /// Every `Rig` command saves `state.json`, because `Config::default()` has
    /// `save_sessions = true`. The save must land in the test sandbox and never
    /// in the data directory the environment names, which is the user's own.
    ///
    /// The user's file usually exists already, so the check is that its
    /// identity is unchanged, not that it is absent. A save replaces it by a
    /// rename, which gives it a new inode and mtime. A remux server the user is
    /// running also saves to that file, so a save of its own inside this test's
    /// window makes the test fail although no test wrote the file.
    #[tokio::test]
    async fn a_command_saves_the_session_state_into_the_test_sandbox() {
        use std::os::unix::fs::MetadataExt;
        let configured = std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(dirs::data_dir)
            .expect("a data directory")
            .join("remux")
            .join("state.json");
        let identity = || {
            std::fs::symlink_metadata(&configured)
                .ok()
                .map(|m| (m.ino(), m.len(), m.modified().ok()))
        };
        let before = identity();
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        rig.command(RemuxCommand::SessionRename("sandboxed".to_string()))
            .await;
        assert!(
            crate::paths::data_home()
                .join("remux")
                .join("state.json")
                .exists(),
            "the save ran"
        );
        assert_eq!(
            identity(),
            before,
            "{} changed during the test: a test wrote it, unless a remux server \
             you are running saved it at the same moment",
            configured.display()
        );
        rig.finish().await;
    }

    /// A pane's shell inherits the test process's environment. With the
    /// user's `HOME` it reads their rc files and appends this test's commands
    /// to their real history file when it exits.
    ///
    /// The `where` file proves on every `/bin/sh` that the shell ran in the
    /// sandbox: its `HOME`, its runtime directory (the user's server socket
    /// lives in the real one) and its `TMPDIR`, and without a variable the
    /// allow-list drops. The history check can only fail where `/bin/sh` keeps
    /// history, which bash does (Fedora, macOS) and dash (Debian, Ubuntu)
    /// never does.
    #[tokio::test]
    async fn a_pane_shell_runs_in_the_sandbox_and_leaves_the_users_history_alone() {
        use std::os::unix::fs::MetadataExt;
        // A name no other code reads, so setting it process-wide cannot change
        // another test's outcome.
        std::env::set_var("REMUX_TEST_PANE_LEAK", "leaked");
        let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let mut watched: Vec<std::path::PathBuf> =
            [".bash_history", ".sh_history", ".zsh_history", ".history"]
                .iter()
                .map(|name| home.join(name))
                .collect();
        watched.extend(std::env::var_os("HISTFILE").map(std::path::PathBuf::from));
        let identity = || {
            watched
                .iter()
                .map(|p| {
                    std::fs::symlink_metadata(p)
                        .ok()
                        .map(|m| (m.ino(), m.len(), m.modified().ok()))
                })
                .collect::<Vec<_>>()
        };
        let before = identity();
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let pane = *rig.panes.lock().await.keys().next().expect("pane");
        rig.panes.lock().await[&pane]
            .pty
            .write_input(
                b"printf 'HOME:%s RUN:%s TMP:%s LEAK:%s' \"$HOME\" \"$XDG_RUNTIME_DIR\" \
                  \"$TMPDIR\" \"${REMUX_TEST_PANE_LEAK-}\" > \"$HOME/where-sandbox-test\"\nexit\n",
            )
            .expect("write");
        let mut exited = false;
        for _ in 0..100 {
            if rig.panes.lock().await[&pane]
                .pty
                .try_wait()
                .ok()
                .flatten()
                .is_some()
            {
                exited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        std::env::remove_var("REMUX_TEST_PANE_LEAK");
        assert!(exited, "the pane's shell exited");
        rig.finish().await;
        let sandbox_home = crate::paths::sandbox::home();
        let root = sandbox_home.parent().expect("sandbox root");
        assert_eq!(
            std::fs::read_to_string(sandbox_home.join("where-sandbox-test")).ok(),
            Some(format!(
                "HOME:{} RUN:{} TMP:{} LEAK:",
                sandbox_home.display(),
                root.join("run").display(),
                root.join("tmp").display()
            )),
            "the pane's shell ran with the sandbox environment"
        );
        assert_eq!(
            identity(),
            before,
            "a history file under {} changed during the test: a pane's shell \
             wrote it, unless a shell you are running saved its history at the \
             same moment",
            home.display()
        );
    }

    #[tokio::test]
    async fn a_session_rename_is_announced_to_every_client() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let mut other = rig.add_client(2, 100, 30, None).await;
        let renamed = |events: Vec<ServerMessage>| {
            events.into_iter().find_map(|m| match m {
                ServerMessage::Event(SessionEvent::SessionRenamed { old, new }) => Some((old, new)),
                _ => None,
            })
        };
        rig.command(RemuxCommand::SessionRename("work".to_string()))
            .await;
        let expected = Some(("main".to_string(), "work".to_string()));
        assert_eq!(renamed(rig.drain()), expected);
        let mut seen = Vec::new();
        while let Ok(m) = other.try_recv() {
            seen.push(m);
        }
        assert_eq!(
            renamed(seen),
            expected,
            "a client attached elsewhere hears too"
        );
        rig.command(RemuxCommand::SessionRenameByName {
            old: "work".to_string(),
            new: "play".to_string(),
        })
        .await;
        assert_eq!(
            renamed(rig.drain()),
            Some(("work".to_string(), "play".to_string()))
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn the_first_toggle_in_a_tab_opens_in_the_focused_panes_cwd() {
        let a = tempfile::tempdir().expect("dir a");
        let b = tempfile::tempdir().expect("dir b");
        let rig = Rig::new(100, 30, None, a.path()).await;
        let first = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        let second = rig.split(b.path()).await;
        for (focus, dir) in [(second, b.path()), (first, a.path())] {
            assert_eq!(cwd_once_in(&rig, focus, dir).await, Some(canonical(dir)));
            rig.state
                .lock()
                .await
                .sessions
                .get_mut("main")
                .unwrap()
                .tabs[0]
                .focus_pane(focus);
            rig.command(RemuxCommand::PopupToggle(None)).await;
            let popup = rig.popup_of(0).await.expect("popup");
            assert_eq!(
                cwd_once_in(&rig, popup, dir).await,
                Some(canonical(dir)),
                "focused pane {focus}"
            );
            // Close it and reap it, so the next toggle is a first one again.
            let taken = rig
                .state
                .lock()
                .await
                .sessions
                .get_mut("main")
                .unwrap()
                .tabs[0]
                .take_popup();
            reap_panes(
                &taken.into_iter().collect::<Vec<_>>(),
                &rig.panes,
                &rig.clients,
            )
            .await;
        }
        rig.finish().await;
    }

    #[tokio::test]
    async fn resize_commands_do_not_touch_the_layout_while_the_popup_is_up() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        {
            let mut st = rig.state.lock().await;
            let tab = &mut st.sessions.get_mut("main").unwrap().tabs[0];
            let focused = tab.focused_pane;
            tab.layout.split_vertical(focused, 77);
            tab.pane_order.push(77);
        }
        let layout_json = |rig: &Rig| {
            let state = rig.state.clone();
            async move {
                let st = state.lock().await;
                serde_json::to_string(&st.sessions["main"].tabs[0].layout).unwrap()
            }
        };
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let before = layout_json(&rig).await;
        rig.command(RemuxCommand::ResizeRight(5)).await;
        assert_eq!(
            layout_json(&rig).await,
            before,
            "no-op while the popup is up"
        );

        rig.command(RemuxCommand::PopupToggle(None)).await;
        rig.command(RemuxCommand::ResizeRight(5)).await;
        assert_ne!(
            layout_json(&rig).await,
            before,
            "the same command resizes once the popup is hidden"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn popup_size_resizes_the_pty_and_always_answers_the_sender_with_a_full_render() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let popup = rig.popup_of(0).await.expect("popup");
        rig.drain();

        let send = |rig: &Rig, cols, rows| {
            let (state, panes, clients, config, pf) = (
                rig.state.clone(),
                rig.panes.clone(),
                rig.clients.clone(),
                rig.config.clone(),
                rig.prev_frames.clone(),
            );
            async move {
                handle_popup_size(CLIENT, (cols, rows), &state, &panes, &clients, &config, &pf)
                    .await
                    .expect("popup size")
            }
        };
        send(&rig, 60, 12).await;
        assert_eq!(rig.screen_size(popup).await, (60, 12));
        let frames = rig.drain();
        let full_renders = |frames: &[ServerMessage]| {
            frames
                .iter()
                .filter(|m| matches!(m, ServerMessage::FullRender { .. }))
                .count()
        };
        assert_eq!(full_renders(&frames), 1, "one answer, not two");
        let Some(ServerMessage::FullRender {
            popup: layer,
            focused_pane_rect,
            ..
        }) = frames.into_iter().rev().find(|m| {
            matches!(
                m,
                ServerMessage::FullRender { .. } | ServerMessage::RenderDiff { .. }
            )
        })
        else {
            panic!("the sender must be answered with a full render");
        };
        let layer = layer.expect("an asking client gets the layer");
        assert_eq!((layer.cols, layer.rows), (60, 12));
        assert_eq!(focused_pane_rect, None);

        // The same size again changes nothing on the server and is still
        // answered with a full render.
        send(&rig, 60, 12).await;
        assert_eq!(rig.screen_size(popup).await, (60, 12));
        let frames = rig.drain();
        assert_eq!(full_renders(&frames), 1, "always a full render");
        assert!(!frames
            .iter()
            .any(|m| matches!(m, ServerMessage::RenderDiff { .. })));
        rig.finish().await;
    }

    #[tokio::test]
    async fn resizing_sizes_a_non_active_tabs_popup_too() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let first = rig.popup_of(0).await.expect("popup");
        rig.new_tab(dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let second = rig.popup_of(1).await.expect("popup");
        // A tab switch does not resize a popup: both are already the shared size.
        assert_eq!(rig.screen_size(first).await, rig.screen_size(second).await);

        rig.command(RemuxCommand::TabGoto(0)).await;
        {
            let mut cls = rig.clients.lock().await;
            cls.get_mut(&CLIENT).unwrap().popup_want = Some((70, 14));
        }
        // The active tab is now tab 0; tab 1's popup is on a background tab.
        resize_session_panes("main", &rig.state, &rig.panes, &rig.clients, &rig.config)
            .await
            .expect("resize");
        assert_eq!(rig.screen_size(first).await, (70, 14));
        assert_eq!(rig.screen_size(second).await, (70, 14));
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_hidden_popups_shell_exiting_clears_it_on_a_non_active_tab() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        rig.new_tab(dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let hidden = rig.popup_of(1).await.expect("tab 1 popup");
        rig.command(RemuxCommand::TabGoto(0)).await;

        close_pane(
            hidden,
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await;

        let st = rig.state.lock().await;
        assert_eq!(st.sessions["main"].tabs[1].popup_pane, None);
        assert!(!st.sessions["main"].tabs[1].popup_visible);
        assert_eq!(st.sessions["main"].tabs.len(), 2, "the tab itself stays");
        drop(st);
        assert!(!rig.panes.lock().await.contains_key(&hidden));
        // The next toggle on that tab spawns a fresh popup.
        rig.command(RemuxCommand::TabGoto(1)).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let fresh = rig.popup_of(1).await.expect("fresh popup");
        assert_ne!(fresh, hidden);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_sender_scrolled_in_its_popup_is_still_answered_once() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let popup = rig.popup_of(0).await.expect("popup");
        // Give the popup history and scroll the sender into it. The broadcast
        // skips a client whose input pane is scrolled.
        {
            let mut ps = rig.panes.lock().await;
            let lines: String = (0..40).map(|i| format!("line {i}\r\n")).collect();
            ps.get_mut(&popup)
                .unwrap()
                .screen
                .process_output(lines.as_bytes());
        }
        rig.clients
            .lock()
            .await
            .get_mut(&CLIENT)
            .unwrap()
            .set_scroll(popup, 3);
        rig.drain();
        handle_popup_size(
            CLIENT,
            (60, 12),
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await
        .expect("popup size");
        assert_eq!(rig.screen_size(popup).await, (60, 12));
        let full_renders = rig
            .drain()
            .into_iter()
            .filter(|m| matches!(m, ServerMessage::FullRender { .. }))
            .count();
        assert_eq!(full_renders, 1, "the size change is answered exactly once");
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_hidden_clients_refreshed_want_lifts_the_shared_size() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let _other = rig.add_client(2, 160, 48, Some((70, 14))).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let popup = rig.popup_of(0).await.expect("popup");
        rig.command(RemuxCommand::PopupToggle(None)).await;
        assert_eq!(rig.screen_size(popup).await, (50, 10), "capped by client 1");
        // Client 1's terminal grows while the popup is hidden, and it announces
        // the box it would now place.
        handle_popup_size(
            CLIENT,
            (90, 20),
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await
        .expect("popup size");
        assert_eq!(rig.screen_size(popup).await, (70, 14), "no longer capped");
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_popup_already_at_the_shared_size_is_not_resized_again() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let popup = rig.popup_of(0).await.expect("popup");
        let resize = |rig: &Rig| {
            let (state, panes, clients, config) = (
                rig.state.clone(),
                rig.panes.clone(),
                rig.clients.clone(),
                rig.config.clone(),
            );
            async move { resize_session_popups("main", &state, &panes, &clients, &config).await }
        };
        // A terminal resize reaches here with the want the client had before
        // its box changed: the popup is already that size.
        assert!(!resize(&rig).await, "no SIGWINCH for a size the popup has");
        rig.clients
            .lock()
            .await
            .get_mut(&CLIENT)
            .unwrap()
            .popup_want = Some((60, 12));
        assert!(resize(&rig).await);
        assert_eq!(rig.screen_size(popup).await, (60, 12));
        rig.finish().await;
    }

    #[tokio::test]
    async fn client_sizes_are_clamped_to_the_terminal_maximum() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        handle_resize(
            CLIENT,
            u16::MAX,
            0,
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await
        .expect("resize");
        handle_popup_size(
            CLIENT,
            (0, u16::MAX),
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await
        .expect("popup size");
        let cls = rig.clients.lock().await;
        let client = &cls[&CLIENT];
        assert_eq!((client.cols, client.rows), (MAX_TERMINAL_COLS, 1));
        assert_eq!(client.popup_want, Some((1, MAX_TERMINAL_ROWS)));
        drop(cls);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_pane_subscription_demand_is_clamped_to_the_terminal_maximum() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let pane_id = *rig.panes.lock().await.keys().next().expect("pane");
        let subscribe = |cols, rows, size_demand| ClientMessage::SubscribePane {
            pane_id,
            cols,
            rows,
            size_demand,
        };
        for (msg, want) in [
            (subscribe(u16::MAX, 0, true), Some((MAX_TERMINAL_COLS, 1))),
            (subscribe(0, u16::MAX, true), Some((1, MAX_TERMINAL_ROWS))),
            (subscribe(80, 24, true), Some((80, 24))),
            (subscribe(u16::MAX, u16::MAX, false), None),
        ] {
            handle_client_message(
                CLIENT,
                msg,
                &Arc::new(identity::Peer::unpinned()),
                &rig.state,
                &rig.panes,
                &rig.clients,
                &rig.config,
                &rig.prev_frames,
                &rig.dormant,
            )
            .await
            .expect("subscribe");
            let cls = rig.clients.lock().await;
            assert_eq!(cls[&CLIENT].subscribed_panes.get(&pane_id), Some(&want));
        }
        rig.finish().await;
    }

    /// The ways a client stops counting towards a session it shared.
    #[derive(Debug, Clone, Copy)]
    enum Leave {
        Detach,
        Disconnect,
        AttachAway,
        SwitchTabAway,
        SwitchPaneAway,
    }

    #[tokio::test]
    async fn a_client_leaving_resizes_the_popup_to_the_clients_left() {
        for how in [
            Leave::Detach,
            Leave::Disconnect,
            Leave::AttachAway,
            Leave::SwitchTabAway,
            Leave::SwitchPaneAway,
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
            let mut survivor = rig.add_client(2, 100, 30, Some((70, 14))).await;
            rig.new_session("other", dir.path()).await;
            rig.command(RemuxCommand::PopupToggle(None)).await;
            let popup = rig.popup_of(0).await.expect("popup");
            assert_eq!(rig.screen_size(popup).await, (50, 10), "{how:?}");
            while survivor.try_recv().is_ok() {}

            match how {
                Leave::Detach => rig.command(RemuxCommand::SessionDetach).await,
                Leave::Disconnect => {
                    handle_client_disconnect(
                        CLIENT,
                        &rig.state,
                        &rig.panes,
                        &rig.clients,
                        &rig.config,
                        &rig.prev_frames,
                    )
                    .await
                }
                Leave::AttachAway => handle_attach(
                    CLIENT,
                    "other",
                    &rig.state,
                    &rig.panes,
                    &rig.clients,
                    &rig.config,
                    &rig.prev_frames,
                    &rig.dormant,
                )
                .await
                .expect("attach"),
                Leave::SwitchTabAway => {
                    rig.command(RemuxCommand::SessionSwitchTab {
                        session: "other".to_string(),
                        tab_index: 0,
                    })
                    .await
                }
                Leave::SwitchPaneAway => {
                    let pane_id = rig.state.lock().await.sessions["other"].tabs[0].focused_pane;
                    rig.command(RemuxCommand::SessionSwitchPane {
                        session: "other".to_string(),
                        tab_index: 0,
                        pane_id,
                    })
                    .await
                }
            }

            assert_eq!(rig.screen_size(popup).await, (70, 14), "{how:?}");
            let mut layers = Vec::new();
            while let Ok(m) = survivor.try_recv() {
                if let ServerMessage::FullRender { popup: Some(l), .. } = m {
                    layers.push((l.cols, l.rows));
                }
            }
            assert_eq!(
                layers.last(),
                Some(&(70, 14)),
                "{how:?}: the client left behind is repainted at the new size"
            );
            rig.finish().await;
        }
    }

    // -- Float zoom -----------------------------------------------------------

    fn float_rig_config(mode: crate::config::ZoomMode) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.general.default_shell = Some("/bin/sh".to_string());
        cfg.zoom.mode = mode;
        Arc::new(cfg)
    }

    /// The box and interior a float zoom gets in the 100x30 rig (a 100x29 area).
    fn float_box() -> (Rect, Rect) {
        float_zoom_box(
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 29,
            },
            (90, 90),
        )
    }

    async fn sizes_of(rig: &Rig, panes: &[PaneId]) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        for &pane in panes {
            out.push(rig.screen_size(pane).await);
        }
        out
    }

    /// Three panes side by side, the middle one focused, every pane sized by the
    /// layout.
    async fn three_panes(rig: &Rig, cwd: &std::path::Path) -> (PaneId, PaneId, PaneId) {
        let left = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        let middle = rig.split(cwd).await;
        let right = rig.split(cwd).await;
        rig.state
            .lock()
            .await
            .sessions
            .get_mut("main")
            .unwrap()
            .tabs[0]
            .focus_pane(middle);
        resize_session_panes("main", &rig.state, &rig.panes, &rig.clients, &rig.config)
            .await
            .expect("resize");
        (left, middle, right)
    }

    async fn float_of(rig: &Rig) -> Option<PaneId> {
        rig.state.lock().await.sessions["main"].tabs[0].float_zoomed()
    }

    async fn composite_of(
        rig: &Rig,
    ) -> (
        Vec<Vec<RenderCell>>,
        HitRegions,
        Option<PaneRect>,
        Vec<(PaneId, Rect)>,
    ) {
        let (cells, _cx, _cy, _cv, _cs, fpr, hit_regions, pane_rects, _ack, _bp, _layer, _status) =
            build_composite(
                "main",
                100,
                30,
                "NORMAL",
                &rig.state,
                &rig.panes,
                &rig.config,
                None,
                None,
                &HashMap::new(),
                &rig.config.compositor_theme(),
                &Naming::server(),
            )
            .await;
        (cells, hit_regions, fpr, pane_rects)
    }

    #[tokio::test]
    async fn a_float_zoom_sizes_only_the_floated_pane_and_snap_back_restores_it() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, right) = three_panes(&rig, dir.path()).await;
        let before = sizes_of(&rig, &[left, middle, right]).await;
        let (_, interior) = float_box();
        assert_ne!(before[1], (interior.width, interior.height));

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        assert_eq!(float_of(&rig).await, Some(middle));
        let during = sizes_of(&rig, &[left, middle, right]).await;
        assert_eq!(during[0], before[0], "the left pane is untouched");
        assert_eq!(during[2], before[2], "the right pane is untouched");
        assert_eq!(during[1], (interior.width, interior.height));

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        assert_eq!(float_of(&rig).await, None);
        assert_eq!(sizes_of(&rig, &[left, middle, right]).await, before);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_float_zoom_draws_a_centred_box_and_leaves_the_other_panes_where_they_were() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, right) = three_panes(&rig, dir.path()).await;
        let (_, _, _, rects_before) = composite_of(&rig).await;

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        let (cells, regions, fpr, rects) = composite_of(&rig).await;

        let (rect, interior) = float_box();
        assert_eq!(regions.float, Some((middle, rect)));
        assert_eq!((rect.x, rect.y), (5, 1), "centred in the area");
        assert_eq!(
            cells[rect.y as usize][rect.x as usize].c, '\u{256d}',
            "top-left corner"
        );
        let bottom = (rect.y + rect.height - 1) as usize;
        let right_edge = (rect.x + rect.width - 1) as usize;
        assert_eq!(
            cells[bottom][right_edge].c, '\u{256f}',
            "bottom-right corner"
        );
        let fpr = fpr.expect("the floated pane is the focused one");
        assert_eq!(
            (fpr.x, fpr.y, fpr.width, fpr.height),
            (interior.x, interior.y, interior.width, interior.height)
        );
        for pane in [left, right] {
            let slot =
                |rects: &[(PaneId, Rect)]| rects.iter().find(|(id, _)| *id == pane).map(|r| r.1);
            assert_eq!(
                slot(&rects),
                slot(&rects_before),
                "pane {pane} keeps its rect"
            );
        }
        // The floated pane's own entry is its box, so a hit in it resolves there.
        assert_eq!(
            rects.iter().find(|(id, _)| *id == middle).map(|r| r.1),
            Some(rect)
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn the_floated_panes_slot_shows_an_empty_unfocused_frame() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, _middle, _right) = three_panes(&rig, dir.path()).await;
        rig.state
            .lock()
            .await
            .sessions
            .get_mut("main")
            .unwrap()
            .tabs[0]
            .focus_pane(left);
        {
            let mut ps = rig.panes.lock().await;
            let screen = &mut ps.get_mut(&left).unwrap().screen;
            screen.process_output(b"\r\n\r\n\r\nSTALE-TEXT");
        }
        let (_, _, _, rects) = composite_of(&rig).await;
        let slot = rects.iter().find(|(id, _)| *id == left).unwrap().1;
        assert_eq!((slot.x, slot.y), (0, 0));
        let (cells, _, _, _) = composite_of(&rig).await;
        let shown: String = cells[4][1..5].iter().map(|c| c.c).collect();
        assert_eq!(shown, "STAL", "the pane shows in its slot before it floats");

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        let (cells, _, _, _) = composite_of(&rig).await;

        // Columns 1..5 of the slot are left of the box (which starts at 5).
        let (rect, _) = float_box();
        assert!(rect.x >= 5);
        let shown: String = cells[4][1..5].iter().map(|c| c.c).collect();
        assert_eq!(shown, "    ", "the slot holds no copy of the floated pane");
        assert_eq!(cells[0][0].c, '\u{256d}', "the slot keeps its frame");
        let theme = rig.config.compositor_theme();
        assert_eq!(
            cells[0][0].fg,
            crate::server::compositor::border_fg(&theme, false),
            "and wears the unfocused colour"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn pane_toggle_zoom_follows_the_configured_mode() {
        for (mode, floats) in [
            (crate::config::ZoomMode::Fullscreen, false),
            (crate::config::ZoomMode::Float, true),
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let mut rig = Rig::new(100, 30, None, dir.path()).await;
            rig.config = float_rig_config(mode);
            three_panes(&rig, dir.path()).await;

            rig.command(RemuxCommand::PaneToggleZoom).await;

            let st = rig.state.lock().await;
            let tab = &st.sessions["main"].tabs[0];
            assert!(tab.zoomed_pane.is_some(), "{mode:?}");
            assert_eq!(tab.float_zoomed().is_some(), floats, "{mode:?}");
            drop(st);
            rig.finish().await;
        }
    }

    #[tokio::test]
    async fn either_zoom_command_releases_a_zoom_of_either_mode() {
        for (take, release) in [
            (
                RemuxCommand::PaneToggleZoom,
                RemuxCommand::PaneToggleZoomFloat,
            ),
            (
                RemuxCommand::PaneToggleZoomFloat,
                RemuxCommand::PaneToggleZoom,
            ),
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let rig = Rig::new(100, 30, None, dir.path()).await;
            let (left, middle, right) = three_panes(&rig, dir.path()).await;
            let before = sizes_of(&rig, &[left, middle, right]).await;

            rig.command(take.clone()).await;
            assert!(rig.state.lock().await.sessions["main"].tabs[0]
                .zoomed_pane
                .is_some());
            rig.command(release.clone()).await;

            assert_eq!(
                rig.state.lock().await.sessions["main"].tabs[0].zoomed_pane,
                None,
                "{take:?} then {release:?}"
            );
            assert_eq!(sizes_of(&rig, &[left, middle, right]).await, before);
            rig.finish().await;
        }
    }

    async fn right(rig: &Rig, x: u16, y: u16, release: bool, popup: bool) {
        handle_right_click(
            CLIENT,
            x,
            y,
            release,
            popup,
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await
        .expect("right click");
    }

    fn menus(msgs: &[ServerMessage]) -> Vec<(u16, u16, crate::protocol::ContextTarget)> {
        msgs.iter()
            .filter_map(|m| match m {
                ServerMessage::ContextMenu { x, y, target } => Some((*x, *y, target.clone())),
                _ => None,
            })
            .collect()
    }

    /// Start `cat -v` in `pane`, which prints the escape bytes it is sent, and
    /// mark the pane as an application that asked for SGR mouse reports.
    async fn track_mouse(rig: &Rig, pane: PaneId) {
        write_to_pane(&rig.panes, pane, b"cat -v\n")
            .await
            .expect("write");
        assert!(
            output_has(rig, pane, &["cat -v"], 5000).await,
            "shell echoed the command"
        );
        let mut ps = rig.panes.lock().await;
        let s = &mut ps.get_mut(&pane).expect("pane").screen;
        s.mouse_tracking = true;
        s.mouse_sgr = true;
    }

    /// Whether everything `pane`'s shell prints within `wait_ms` contains every
    /// needle. The Rig runs no forwarding loop, so this reads the PTY's output
    /// channel itself, which also drains it.
    async fn output_has(rig: &Rig, pane: PaneId, needles: &[&str], wait_ms: u64) -> bool {
        let mut seen = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
        while std::time::Instant::now() < deadline {
            {
                let mut ps = rig.panes.lock().await;
                while let Ok(bytes) = ps.get_mut(&pane).expect("pane").pty_rx.try_recv() {
                    seen.push_str(&String::from_utf8_lossy(&bytes.data));
                }
            }
            if needles.iter().all(|n| seen.contains(n)) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_right_click_on_a_shell_pane_focuses_it_and_asks_for_its_menu() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, _right) = three_panes(&rig, dir.path()).await;
        rig.drain();

        right(&rig, 5, 10, false, false).await;

        let msgs = rig.drain();
        assert_eq!(
            menus(&msgs),
            vec![(
                5,
                10,
                crate::protocol::ContextTarget::Pane {
                    pane_id: left,
                    zoomed: false,
                    zoom_mode: crate::config::ZoomMode::Fullscreen,
                    monocle: false,
                }
            )]
        );
        let focused = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        assert_eq!((focused, focused == middle), (left, false));
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_click_on_a_tracking_apps_content_goes_to_the_app_not_the_menu() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, _middle, _right) = three_panes(&rig, dir.path()).await;
        track_mouse(&rig, left).await;
        rig.drain();

        right(&rig, 5, 10, false, false).await;
        right(&rig, 5, 10, true, false).await;

        assert!(menus(&rig.drain()).is_empty());
        assert!(
            output_has(&rig, left, &["<2;5;10M", "<2;5;10m"], 5000).await,
            "the press and the release reached the app"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_click_on_a_tracking_panes_border_still_opens_the_menu() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, _middle, _right) = three_panes(&rig, dir.path()).await;
        track_mouse(&rig, left).await;
        let (_, _, _, rects) = composite_of(&rig).await;
        let rect = rects.iter().find(|(id, _)| *id == left).unwrap().1;
        rig.drain();

        right(&rig, rect.x + 3, rect.y, false, false).await;
        right(&rig, rect.x + 3, rect.y, true, false).await;

        let found = menus(&rig.drain());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(matches!(
            found[0].2,
            crate::protocol::ContextTarget::Pane { pane_id, .. } if pane_id == left
        ));
        assert!(
            !output_has(&rig, left, &["<2;"], 1000).await,
            "nothing reached the app"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_click_on_a_tab_label_selects_the_tab_and_asks_for_its_menu() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        rig.command(RemuxCommand::TabNew).await;
        let (_, regions, _, _) = composite_of(&rig).await;
        let first = regions
            .tab_regions
            .iter()
            .find(|r| r.tab_index == 0)
            .unwrap();
        let (x, y) = (first.x_start, first.y);
        rig.drain();

        right(&rig, x, y, false, false).await;

        let first_id = tab_ids(&rig).await[0];
        assert_eq!(
            menus(&rig.drain()),
            vec![(
                x,
                y,
                crate::protocol::ContextTarget::Tab {
                    tab_id: first_id,
                    index: 0,
                    count: 2
                }
            )]
        );
        assert_eq!(rig.state.lock().await.sessions["main"].active_tab, 0);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_click_outside_normal_mode_does_nothing() {
        for mode in ["VISUAL", "COMMAND", "SEARCH"] {
            let dir = tempfile::tempdir().expect("dir");
            let mut rig = Rig::new(100, 30, None, dir.path()).await;
            let (_left, middle, _right) = three_panes(&rig, dir.path()).await;
            rig.clients.lock().await.get_mut(&CLIENT).unwrap().mode = mode.to_string();
            rig.drain();

            right(&rig, 5, 10, false, false).await;

            assert!(menus(&rig.drain()).is_empty(), "{mode}");
            let focused = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
            assert_eq!(focused, middle, "{mode}: focus did not move");
            rig.finish().await;
        }
    }

    #[tokio::test]
    async fn a_right_click_inside_a_float_zoom_offers_unzoom_and_keeps_the_float() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        let (_left, middle, _right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        let (rect, _) = float_box();
        rig.drain();

        right(&rig, rect.x + 3, rect.y + 3, false, false).await;

        let found = menus(&rig.drain());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(matches!(
            found[0].2,
            crate::protocol::ContextTarget::Pane { pane_id, zoomed: true, .. } if pane_id == middle
        ));
        assert_eq!(float_of(&rig).await, Some(middle));
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_click_on_a_fullscreen_zoomed_pane_offers_unzoom() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        let (_left, middle, _right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoom).await;
        rig.drain();

        right(&rig, 40, 10, false, false).await;

        let found = menus(&rig.drain());
        assert!(
            matches!(
                found.as_slice(),
                [(_, _, crate::protocol::ContextTarget::Pane { pane_id, zoomed: true, .. })]
                    if *pane_id == middle
            ),
            "{found:?}"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_click_on_a_float_zooms_frame_opens_the_menu_even_for_a_tracking_app() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        let (_left, middle, _right) = three_panes(&rig, dir.path()).await;
        track_mouse(&rig, middle).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        let (rect, _) = float_box();
        rig.drain();

        right(&rig, rect.x + 4, rect.y, false, false).await;

        assert_eq!(menus(&rig.drain()).len(), 1);
        rig.finish().await;
    }

    fn on_pane(pane_id: PaneId, command: RemuxCommand) -> RemuxCommand {
        RemuxCommand::OnPane {
            pane_id,
            command: Box::new(command),
        }
    }

    fn on_tab(tab_id: u64, command: RemuxCommand) -> RemuxCommand {
        RemuxCommand::OnTab {
            tab_id,
            command: Box::new(command),
        }
    }

    async fn pane_ids(rig: &Rig) -> Vec<PaneId> {
        rig.state.lock().await.sessions["main"].tabs[0]
            .pane_order
            .clone()
    }

    async fn tab_ids(rig: &Rig) -> Vec<u64> {
        rig.state.lock().await.sessions["main"]
            .tabs
            .iter()
            .map(|t| t.id)
            .collect()
    }

    #[tokio::test]
    async fn on_pane_acts_on_the_named_pane_and_not_on_the_focused_one() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, right) = three_panes(&rig, dir.path()).await;

        rig.command(on_pane(left, RemuxCommand::PaneClose)).await;

        let left_alive = pane_ids(&rig).await.contains(&left);
        assert!(!left_alive, "the named pane closed");
        assert!(pane_ids(&rig).await.contains(&middle));
        assert!(pane_ids(&rig).await.contains(&right));
        rig.finish().await;
    }

    #[tokio::test]
    async fn on_pane_does_nothing_when_the_pane_is_gone() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, right) = three_panes(&rig, dir.path()).await;
        rig.command(on_pane(left, RemuxCommand::PaneClose)).await;

        rig.command(on_pane(left, RemuxCommand::PaneClose)).await;

        let mut alive = pane_ids(&rig).await;
        alive.sort_unstable();
        assert_eq!(alive, vec![middle, right], "no other pane was closed");
        rig.finish().await;
    }

    #[tokio::test]
    async fn on_pane_splits_and_zooms_the_named_pane() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, _right) = three_panes(&rig, dir.path()).await;

        rig.command(on_pane(left, RemuxCommand::PaneToggleZoom))
            .await;

        let st = rig.state.lock().await;
        assert_eq!(st.sessions["main"].tabs[0].zoomed_pane, Some(left));
        assert_eq!(st.sessions["main"].tabs[0].focused_pane, left);
        assert_ne!(left, middle);
        drop(st);
        rig.finish().await;
    }

    #[tokio::test]
    async fn on_pane_snaps_a_float_back_and_renders_even_when_the_command_does_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, _right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        assert_eq!(float_of(&rig).await, Some(middle));
        rig.drain();

        rig.command(on_pane(left, RemuxCommand::EnterNormal)).await;

        assert_eq!(float_of(&rig).await, None, "the float snapped back");
        let focused = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        assert_eq!(focused, left);
        assert!(renders(&rig.drain()) >= 1, "the focus change was painted");
        rig.finish().await;
    }

    #[tokio::test]
    async fn on_tab_renders_the_tab_switch_even_when_the_command_does_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        rig.new_tab(dir.path()).await;
        let first = tab_ids(&rig).await[0];
        rig.drain();

        rig.command(on_tab(first, RemuxCommand::EnterNormal)).await;

        assert_eq!(rig.state.lock().await.sessions["main"].active_tab, 0);
        assert!(renders(&rig.drain()) >= 1);
        rig.finish().await;
    }

    #[tokio::test]
    async fn on_pane_does_not_move_focus_under_a_visible_popup() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let (left, middle, _right) = three_panes_popup_up(&rig, dir.path()).await;

        rig.command(on_pane(left, RemuxCommand::PaneSplitVertical))
            .await;

        assert_eq!(focus_and_popup(&rig).await, (middle, true));
        assert_eq!(pane_ids(&rig).await.len(), 3, "the split was refused");
        rig.finish().await;
    }

    #[tokio::test]
    async fn ensure_zoom_zooms_once_and_never_releases() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, _middle, _right) = three_panes(&rig, dir.path()).await;

        rig.command(on_pane(left, RemuxCommand::PaneEnsureZoom))
            .await;
        rig.command(on_pane(left, RemuxCommand::PaneEnsureZoom))
            .await;

        assert_eq!(
            rig.state.lock().await.sessions["main"].tabs[0].zoomed_pane,
            Some(left)
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_tab_is_found_by_id_after_another_client_moved_it() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        rig.new_tab(dir.path()).await;
        let ids = tab_ids(&rig).await;
        let (a, b) = (ids[0], ids[1]);
        // Another client moves tab `a` behind `b` while the menu is open.
        rig.command(on_tab(a, RemuxCommand::TabMoveBy(1))).await;
        assert_eq!(tab_ids(&rig).await, vec![b, a]);

        rig.command(on_tab(a, RemuxCommand::TabClose)).await;

        assert_eq!(tab_ids(&rig).await, vec![b], "`a` closed, `b` survived");
        rig.finish().await;
    }

    #[tokio::test]
    async fn closing_or_renaming_a_tab_by_id_ignores_a_gone_tab_and_a_moved_index() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        rig.new_tab(dir.path()).await;
        let ids = tab_ids(&rig).await;
        let (a, b) = (ids[0], ids[1]);
        rig.command(on_tab(a, RemuxCommand::TabMoveBy(1))).await;

        rig.command(RemuxCommand::TabRenameById {
            session: "main".into(),
            tab_id: a,
            name: "kept".into(),
        })
        .await;
        rig.command(RemuxCommand::TabCloseById {
            session: "main".into(),
            tab_id: 9999,
        })
        .await;

        let st = rig.state.lock().await;
        let names: Vec<(u64, String)> = st.sessions["main"]
            .tabs
            .iter()
            .map(|t| (t.id, t.name.clone()))
            .collect();
        assert_eq!(names.len(), 2, "a gone tab closed nothing");
        assert_eq!(names.iter().find(|(id, _)| *id == a).unwrap().1, "kept");
        assert_ne!(names.iter().find(|(id, _)| *id == b).unwrap().1, "kept");
        drop(st);
        rig.command(RemuxCommand::TabCloseById {
            session: "main".into(),
            tab_id: a,
        })
        .await;
        assert_eq!(tab_ids(&rig).await, vec![b]);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_release_in_the_content_after_a_border_press_is_not_forwarded() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, _middle, _right) = three_panes(&rig, dir.path()).await;
        track_mouse(&rig, left).await;
        let (_, _, _, rects) = composite_of(&rig).await;
        let rect = rects.iter().find(|(id, _)| *id == left).unwrap().1;
        rig.drain();

        right(&rig, rect.x + 3, rect.y, false, false).await;
        right(&rig, rect.x + 5, rect.y + 5, true, false).await;

        assert_eq!(
            menus(&rig.drain()).len(),
            1,
            "the border press opened the menu"
        );
        assert!(
            !output_has(&rig, left, &["<2;"], 1000).await,
            "the app saw no release"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_release_on_the_border_after_a_content_press_still_reaches_the_app() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, _middle, _right) = three_panes(&rig, dir.path()).await;
        track_mouse(&rig, left).await;
        let (_, _, _, rects) = composite_of(&rig).await;
        let rect = rects.iter().find(|(id, _)| *id == left).unwrap().1;

        right(&rig, rect.x + 5, rect.y + 5, false, false).await;
        right(&rig, rect.x + 3, rect.y, true, false).await;

        assert!(
            output_has(&rig, left, &["<2;5;5M", "<2;3;1m"], 3000).await,
            "the press and the release both arrived"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_release_goes_only_to_the_pane_that_got_the_press() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, _right) = three_panes(&rig, dir.path()).await;
        track_mouse(&rig, left).await;
        track_mouse(&rig, middle).await;
        let (_, _, _, rects) = composite_of(&rig).await;
        let l = rects.iter().find(|(id, _)| *id == left).unwrap().1;
        let m = rects.iter().find(|(id, _)| *id == middle).unwrap().1;

        right(&rig, l.x + 5, l.y + 5, false, false).await;
        right(&rig, m.x + 5, m.y + 5, true, false).await;

        // The pointer is over the middle pane, so the column and row are counted
        // from the left pane's own origin.
        let release = format!("<2;{};{}m", m.x + 5 - l.x, m.y + 5 - l.y);
        assert!(
            output_has(&rig, left, &[&release], 3000).await,
            "left got its release"
        );
        assert!(
            !output_has(&rig, middle, &["<2;"], 1000).await,
            "middle got nothing"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_press_that_does_nothing_keeps_the_selection() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        three_panes_popup_up(&rig, dir.path()).await;
        {
            let mut cls = rig.clients.lock().await;
            let c = cls.get_mut(&CLIENT).unwrap();
            c.mouse_selection = Some(MouseSelection {
                pane_id: 1,
                start: (0, 0),
                end: (3, 0),
            });
        }

        // Outside the popup: refused, so nothing is cleared.
        right(&rig, RIGHT_X, 5, false, false).await;

        assert!(rig.clients.lock().await[&CLIENT].mouse_selection.is_some());
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_press_on_a_stack_label_opens_that_panes_menu() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        rig.command(RemuxCommand::PaneStackAdd).await;
        let focused = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        let (_, regions, _, _) = composite_of(&rig).await;
        let label = regions
            .stack_regions
            .iter()
            .find(|r| r.pane_id != focused)
            .expect("a label for the pane that is not focused");
        let (x, y, pane) = (label.x_start, label.y, label.pane_id);
        rig.drain();

        right(&rig, x, y, false, false).await;

        let found = menus(&rig.drain());
        assert!(
            matches!(
                found.as_slice(),
                [(_, _, crate::protocol::ContextTarget::Pane { pane_id, .. })] if *pane_id == pane
            ),
            "{found:?}"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_click_does_not_dismiss_a_float_zoom() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        let (_left, middle, _right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        rig.drain();

        right(&rig, 98, 14, false, false).await;

        assert_eq!(float_of(&rig).await, Some(middle));
        assert!(menus(&rig.drain()).is_empty());
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_right_click_inside_the_popup_never_opens_the_menu_or_closes_it() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let (_left, middle, _right) = three_panes_popup_up(&rig, dir.path()).await;
        rig.drain();

        right(&rig, 3, 3, false, true).await;
        right(&rig, RIGHT_X, 5, false, false).await;

        assert!(menus(&rig.drain()).is_empty());
        assert_eq!(focus_and_popup(&rig).await, (middle, true));
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_click_outside_the_float_snaps_it_back_and_focuses_the_pane_clicked() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        let (rect, _) = float_box();

        // Inside the box: the float stays and keeps its pane.
        press(&rig, rect.x + 3, rect.y + 3, false).await;
        assert_eq!(float_of(&rig).await, Some(middle));

        // In the margin outside the box, over the right pane's slot.
        assert!(98 >= rect.x + rect.width, "98 is outside the box");
        press(&rig, 98, 14, false).await;

        assert_eq!(float_of(&rig).await, None, "snapped back");
        let focused = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
        assert_eq!(focused, right, "the click acted");
        assert_ne!(focused, left);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_click_on_the_empty_slot_of_the_floated_pane_snaps_it_back() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (_left, middle, _right) = three_panes(&rig, dir.path()).await;
        let (_, _, _, rects) = composite_of(&rig).await;
        let slot = rects.iter().find(|(id, _)| *id == middle).unwrap().1;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        let (rect, _) = float_box();
        // A cell of the slot that the box does not cover: the last layout row.
        let (x, y) = (slot.x + 2, slot.y + slot.height - 1);
        assert!(
            y >= rect.y + rect.height,
            "the slot pokes out under the box"
        );

        press(&rig, x, y, false).await;

        assert_eq!(float_of(&rig).await, None);
        assert_eq!(
            rig.state.lock().await.sessions["main"].tabs[0].focused_pane,
            middle
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_release_outside_the_float_does_not_snap_it_back() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (_left, middle, _right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        handle_mouse_click(
            CLIENT,
            98,
            14,
            true,
            false,
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await
        .expect("release");

        assert_eq!(float_of(&rig).await, Some(middle));
        rig.finish().await;
    }

    #[tokio::test]
    async fn an_outside_click_that_snaps_a_float_back_renders_once() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        rig.drain();

        press(&rig, 98, 14, false).await;

        assert_eq!(renders(&rig.drain()), 1);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_focus_key_snaps_a_float_back_and_steps_from_the_floated_pane() {
        for (cmd, expect_right) in [
            (RemuxCommand::PaneFocusRight, true),
            (RemuxCommand::PaneFocusLeft, false),
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let rig = Rig::new(100, 30, None, dir.path()).await;
            let (left, middle, right) = three_panes(&rig, dir.path()).await;
            let before = sizes_of(&rig, &[left, middle, right]).await;
            rig.command(RemuxCommand::PaneToggleZoomFloat).await;

            rig.command(cmd.clone()).await;

            assert_eq!(float_of(&rig).await, None, "{cmd:?}");
            let focused = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;
            assert_eq!(focused, if expect_right { right } else { left }, "{cmd:?}");
            assert_eq!(
                sizes_of(&rig, &[left, middle, right]).await,
                before,
                "{cmd:?}: the floated pane is back in its slot"
            );
            rig.finish().await;
        }
    }

    #[tokio::test]
    async fn a_focus_key_with_no_neighbour_snaps_the_float_back_and_stays() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (_left, middle, _right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        rig.command(RemuxCommand::PaneFocusUp).await;

        assert_eq!(float_of(&rig).await, None);
        assert_eq!(
            rig.state.lock().await.sessions["main"].tabs[0].focused_pane,
            middle
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_float_zoom_and_the_popup_close_each_other() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let (_l, middle, _r) = three_panes(&rig, dir.path()).await;

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        assert_eq!(float_of(&rig).await, Some(middle));
        rig.command(RemuxCommand::PopupToggle(None)).await;
        let (focused, popup) = focus_and_popup(&rig).await;
        assert_eq!((focused, popup), (middle, true));
        assert_eq!(float_of(&rig).await, None, "the popup closed the float");

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        let (_, popup) = focus_and_popup(&rig).await;
        assert!(!popup, "the float closed the popup");
        assert_eq!(float_of(&rig).await, Some(middle));
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_float_zoom_closes_a_popup_that_was_already_spawned_and_is_closed_by_reopening_it() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        let (_l, middle, _r) = three_panes(&rig, dir.path()).await;
        // Spawn the popup and hide it again, so the next toggle reuses its pane.
        rig.command(RemuxCommand::PopupToggle(None)).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        assert_eq!(float_of(&rig).await, Some(middle));

        rig.command(RemuxCommand::PopupToggle(None)).await;

        assert_eq!(float_of(&rig).await, None);
        assert!(focus_and_popup(&rig).await.1);
        rig.finish().await;
    }

    #[tokio::test]
    async fn the_configured_float_mode_closes_the_popup_like_the_float_action() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        rig.config = float_rig_config(crate::config::ZoomMode::Float);
        let (_l, middle, _r) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;

        rig.command(RemuxCommand::PaneToggleZoom).await;

        assert_eq!(float_of(&rig).await, Some(middle));
        assert!(!focus_and_popup(&rig).await.1);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_float_zoom_works_in_monocle_where_a_fullscreen_zoom_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (_l, _middle, _r) = three_panes(&rig, dir.path()).await;
        for _ in 0..4 {
            let mode = rig.state.lock().await.sessions["main"].tabs[0]
                .layout_mode
                .name()
                .to_string();
            if mode == "monocle" {
                break;
            }
            rig.command(RemuxCommand::LayoutNext).await;
        }
        let tab_mode = rig.state.lock().await.sessions["main"].tabs[0]
            .layout_mode
            .name()
            .to_string();
        assert_eq!(tab_mode, "monocle");
        let focused = rig.state.lock().await.sessions["main"].tabs[0].focused_pane;

        rig.command(RemuxCommand::PaneToggleZoom).await;
        assert_eq!(
            rig.state.lock().await.sessions["main"].tabs[0].zoomed_pane,
            None,
            "fullscreen has nothing to do in Monocle"
        );

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        assert_eq!(float_of(&rig).await, Some(focused));
        let (cells, regions, _, _) = composite_of(&rig).await;
        let (rect, interior) = float_box();
        assert_eq!(regions.float, Some((focused, rect)));
        assert_eq!(cells[rect.y as usize][rect.x as usize].c, '\u{256d}');
        assert_eq!(
            rig.screen_size(focused).await,
            (interior.width, interior.height)
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_float_zoom_that_closes_the_popup_renders_once() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;
        rig.drain();

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        assert_eq!(renders(&rig.drain()), 1);
        rig.finish().await;
    }

    #[tokio::test]
    async fn releasing_a_fullscreen_zoom_with_the_float_action_leaves_the_popup_up() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoom).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        let st = rig.state.lock().await;
        let tab = &st.sessions["main"].tabs[0];
        assert_eq!(tab.zoomed_pane, None, "the toggle released the zoom");
        assert!(tab.popup_visible, "and took no float, so the popup stays");
        drop(st);
        rig.finish().await;
    }

    #[tokio::test]
    async fn the_fullscreen_action_zooms_fullscreen_even_when_the_mode_is_float() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        rig.config = float_rig_config(crate::config::ZoomMode::Float);
        let (left, middle, right) = three_panes(&rig, dir.path()).await;
        let before = sizes_of(&rig, &[left, middle, right]).await;

        rig.command(RemuxCommand::PaneToggleZoomFullscreen).await;
        {
            let st = rig.state.lock().await;
            let tab = &st.sessions["main"].tabs[0];
            assert_eq!(tab.zoomed_pane, Some(middle));
            assert_eq!(tab.float_zoomed(), None);
        }
        assert_ne!(
            rig.screen_size(middle).await,
            before[1],
            "it fills the area"
        );

        rig.command(RemuxCommand::PaneToggleZoomFullscreen).await;
        assert_eq!(
            rig.state.lock().await.sessions["main"].tabs[0].zoomed_pane,
            None
        );
        assert_eq!(sizes_of(&rig, &[left, middle, right]).await, before);
        rig.finish().await;
    }

    #[tokio::test]
    async fn the_fullscreen_action_releases_a_float_and_the_float_action_releases_a_fullscreen_zoom(
    ) {
        for (take, release) in [
            (
                RemuxCommand::PaneToggleZoomFloat,
                RemuxCommand::PaneToggleZoomFullscreen,
            ),
            (
                RemuxCommand::PaneToggleZoomFullscreen,
                RemuxCommand::PaneToggleZoomFloat,
            ),
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let rig = Rig::new(100, 30, None, dir.path()).await;
            let (left, middle, right) = three_panes(&rig, dir.path()).await;
            let before = sizes_of(&rig, &[left, middle, right]).await;

            rig.command(take.clone()).await;
            rig.command(release.clone()).await;

            assert_eq!(
                rig.state.lock().await.sessions["main"].tabs[0].zoomed_pane,
                None,
                "{take:?} then {release:?}"
            );
            assert_eq!(sizes_of(&rig, &[left, middle, right]).await, before);
            rig.finish().await;
        }
    }

    #[tokio::test]
    async fn the_fullscreen_action_is_refused_in_monocle() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        three_panes(&rig, dir.path()).await;
        for _ in 0..4 {
            let name = rig.state.lock().await.sessions["main"].tabs[0]
                .layout_mode
                .name()
                .to_string();
            if name == "monocle" {
                break;
            }
            rig.command(RemuxCommand::LayoutNext).await;
        }

        rig.command(RemuxCommand::PaneToggleZoomFullscreen).await;

        assert_eq!(
            rig.state.lock().await.sessions["main"].tabs[0].zoomed_pane,
            None
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_zoom_taken_before_the_popup_can_still_be_released_while_it_is_up() {
        for (mode, release) in [
            (
                crate::config::ZoomMode::Fullscreen,
                RemuxCommand::PaneToggleZoom,
            ),
            (
                crate::config::ZoomMode::Float,
                RemuxCommand::PaneToggleZoomFullscreen,
            ),
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
            rig.config = float_rig_config(mode);
            three_panes(&rig, dir.path()).await;
            rig.command(RemuxCommand::PaneToggleZoomFullscreen).await;
            rig.command(RemuxCommand::PopupToggle(None)).await;
            assert!(rig.state.lock().await.sessions["main"].tabs[0].popup_visible);

            rig.command(release.clone()).await;

            let st = rig.state.lock().await;
            let tab = &st.sessions["main"].tabs[0];
            assert_eq!(tab.zoomed_pane, None, "{release:?} in {mode:?} mode");
            assert!(tab.popup_visible, "the popup stays up");
            drop(st);
            rig.finish().await;
        }
    }

    #[tokio::test]
    async fn the_fullscreen_action_is_refused_while_the_popup_is_up_whatever_the_mode() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        rig.config = float_rig_config(crate::config::ZoomMode::Float);
        three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;

        rig.command(RemuxCommand::PaneToggleZoomFullscreen).await;

        let st = rig.state.lock().await;
        assert_eq!(st.sessions["main"].tabs[0].zoomed_pane, None);
        assert!(st.sessions["main"].tabs[0].popup_visible);
        drop(st);
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_fullscreen_zoom_toggle_is_still_refused_while_the_popup_is_up() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, Some((50, 10)), dir.path()).await;
        three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PopupToggle(None)).await;

        rig.command(RemuxCommand::PaneToggleZoom).await;

        let st = rig.state.lock().await;
        assert_eq!(st.sessions["main"].tabs[0].zoomed_pane, None);
        assert!(st.sessions["main"].tabs[0].popup_visible);
        drop(st);
        rig.finish().await;
    }

    #[tokio::test]
    async fn splitting_releases_a_float_zoom() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        rig.command(RemuxCommand::PaneSplitVertical).await;

        assert_eq!(
            rig.state.lock().await.sessions["main"].tabs[0].zoomed_pane,
            None
        );
        rig.finish().await;
    }

    fn full_render_float_flags(msgs: &[ServerMessage]) -> Vec<bool> {
        msgs.iter()
            .filter_map(|m| match m {
                ServerMessage::FullRender { float_zoom, .. }
                | ServerMessage::RenderDiff { float_zoom, .. } => Some(*float_zoom),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn every_frame_says_whether_the_tab_shows_a_float_zoom() {
        let dir = tempfile::tempdir().expect("dir");
        let mut rig = Rig::new(100, 30, None, dir.path()).await;
        three_panes(&rig, dir.path()).await;
        rig.drain();

        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        assert_eq!(full_render_float_flags(&rig.drain()), vec![true]);

        rig.command(RemuxCommand::PaneToggleZoom).await;
        assert_eq!(full_render_float_flags(&rig.drain()), vec![false]);

        rig.command(RemuxCommand::PaneToggleZoom).await;
        assert_eq!(
            full_render_float_flags(&rig.drain()),
            vec![false],
            "a fullscreen zoom is not a float"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn a_float_survives_leaving_its_tab_and_coming_back_with_its_sizes() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, right) = three_panes(&rig, dir.path()).await;
        let before = sizes_of(&rig, &[left, middle, right]).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        let floated = sizes_of(&rig, &[left, middle, right]).await;
        rig.new_tab(dir.path()).await;
        rig.command(RemuxCommand::SessionSwitchTab {
            session: "main".to_string(),
            tab_index: 1,
        })
        .await;

        rig.command(RemuxCommand::SessionSwitchTab {
            session: "main".to_string(),
            tab_index: 0,
        })
        .await;

        assert_eq!(float_of(&rig).await, Some(middle));
        assert_eq!(sizes_of(&rig, &[left, middle, right]).await, floated);
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        assert_eq!(sizes_of(&rig, &[left, middle, right]).await, before);
        rig.finish().await;
    }

    #[tokio::test]
    async fn closing_a_neighbour_under_a_float_keeps_the_float_and_resizes_the_rest() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        close_pane(
            left,
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await;

        assert_eq!(float_of(&rig).await, Some(middle));
        let (_, interior) = float_box();
        assert_eq!(
            rig.screen_size(middle).await,
            (interior.width, interior.height)
        );
        let (_, _, _, rects) = composite_of(&rig).await;
        let slot = rects.iter().find(|(id, _)| *id == right).unwrap().1;
        assert_eq!(
            rig.screen_size(right).await,
            (slot.width - 2, slot.height - 2),
            "the survivor fills the slot the layout gave it"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn closing_the_floated_pane_releases_the_float_and_sizes_the_survivors() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        close_pane(
            middle,
            &rig.state,
            &rig.panes,
            &rig.clients,
            &rig.config,
            &rig.prev_frames,
        )
        .await;

        assert_eq!(
            rig.state.lock().await.sessions["main"].tabs[0].zoomed_pane,
            None
        );
        let (_, regions, _, rects) = composite_of(&rig).await;
        assert_eq!(regions.float, None);
        for pane in [left, right] {
            let slot = rects.iter().find(|(id, _)| *id == pane).unwrap().1;
            assert_eq!(
                rig.screen_size(pane).await,
                (slot.width - 2, slot.height - 2),
                "pane {pane}"
            );
        }
        rig.finish().await;
    }

    #[tokio::test]
    async fn switching_to_another_pane_snaps_a_float_back_and_acts() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (left, middle, right) = three_panes(&rig, dir.path()).await;
        let before = sizes_of(&rig, &[left, middle, right]).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        rig.command(RemuxCommand::SessionSwitchPane {
            session: "main".to_string(),
            tab_index: 0,
            pane_id: left,
        })
        .await;

        assert_eq!(float_of(&rig).await, None);
        assert_eq!(
            rig.state.lock().await.sessions["main"].tabs[0].focused_pane,
            left
        );
        assert_eq!(sizes_of(&rig, &[left, middle, right]).await, before);
        rig.finish().await;
    }

    #[tokio::test]
    async fn switching_to_the_floated_pane_leaves_the_float_up() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (_left, middle, _right) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        rig.command(RemuxCommand::SessionSwitchPane {
            session: "main".to_string(),
            tab_index: 0,
            pane_id: middle,
        })
        .await;

        assert_eq!(float_of(&rig).await, Some(middle));
        rig.finish().await;
    }

    #[tokio::test]
    async fn renaming_the_floated_pane_shows_the_name_on_the_box() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;

        rig.command(RemuxCommand::PaneRename("floaty".to_string()))
            .await;

        let (cells, _, _, _) = composite_of(&rig).await;
        let (rect, _) = float_box();
        let top: String = cells[rect.y as usize].iter().map(|c| c.c).collect();
        assert!(top.contains("floaty"), "{top:?}");
        assert!(
            float_of(&rig).await.is_some(),
            "a rename does not end the float"
        );
        rig.finish().await;
    }

    #[tokio::test]
    async fn the_float_follows_a_window_resize() {
        let dir = tempfile::tempdir().expect("dir");
        let rig = Rig::new(100, 30, None, dir.path()).await;
        let (_l, middle, _r) = three_panes(&rig, dir.path()).await;
        rig.command(RemuxCommand::PaneToggleZoomFloat).await;
        {
            let mut cls = rig.clients.lock().await;
            let c = cls.get_mut(&CLIENT).unwrap();
            c.cols = 140;
            c.rows = 40;
        }

        resize_session_panes("main", &rig.state, &rig.panes, &rig.clients, &rig.config)
            .await
            .expect("resize");

        let (_, interior) = float_zoom_box(
            Rect {
                x: 0,
                y: 0,
                width: 140,
                height: 39,
            },
            (90, 90),
        );
        assert_eq!(
            rig.screen_size(middle).await,
            (interior.width, interior.height)
        );
        rig.finish().await;
    }
}
