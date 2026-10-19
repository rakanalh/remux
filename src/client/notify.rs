//! Desktop notifications for agents: when one needs input, finishes, or exits.
//!
//! Sent by the CLIENT, from the agent lists every connected server already
//! pushes, so a remote agent notifies on the machine the user is sitting at and
//! nothing fires while no client is running.
//!
//! Two halves, kept apart so the rules can be tested without spawning
//! anything. [`AgentNotifier`] turns successive lists into events and does no
//! I/O and reads no clock: the caller passes the time in. [`deliver`] hands an
//! event to the platform's notifier.
//!
//! The identity of an agent is `(ConnId, PaneId)`, never the pane id alone,
//! because pane ids are per-server counters and two connected machines both
//! have a pane 1.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::client::registry::ConnId;
use crate::client::sidebar::agents::AgentRow;
use crate::config::notifications::NotificationsConfig;
use crate::protocol::{AgentEntry, AgentState, PaneId};

/// How long a run of work must last before its end counts as `finished`.
///
/// The server calls a pane `Working` on any output, so a shell prompt redraw or
/// a short reply would otherwise end in a notification.
pub const MIN_WORKING: Duration = Duration::from_secs(3);

/// How long an end of work or a disappearance must hold before it is reported.
///
/// The server marks a pane `Idle` after `[agents] working_ms` of silence, 500 ms
/// by default, and an agent thinking between two tool calls is silent for
/// longer than that. A pane can also drop out of one list when its process
/// group cannot be read for a moment. Waiting this long turns both into
/// non-events.
pub const SETTLE: Duration = Duration::from_secs(3);

/// How long a dedupe marker keeps another client from sending the same event.
const DEDUPE_WINDOW: Duration = Duration::from_secs(10);

/// The longest title or body handed to the notifier, in characters. A window
/// title has no length limit, and a notification banner shows little of one.
const MAX_TEXT_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifyKind {
    NeedsInput,
    Finished,
    Exited,
}

impl NotifyKind {
    fn verb(self) -> &'static str {
        match self {
            NotifyKind::NeedsInput => "needs input",
            NotifyKind::Finished => "finished",
            NotifyKind::Exited => "exited",
        }
    }
}

/// One event to show, already named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub kind: NotifyKind,
    pub conn: ConnId,
    pub pane_id: PaneId,
    pub title: String,
    pub body: String,
}

/// What the notifier remembers about one agent pane.
#[derive(Debug, Clone)]
struct Tracked {
    /// The newest entry seen. While an `exited` is pending this is the last one
    /// seen, which is what that event is named and suppressed by.
    entry: AgentEntry,
    /// When the current run of work began. A run spans `Idle` gaps shorter
    /// than [`SETTLE`], so an agent that pauses between steps is one task.
    run_start: Option<Instant>,
    /// When the pane last went from `Working` to anything else.
    idle_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    kind: NotifyKind,
    due: Instant,
}

/// Turns each connection's successive agent lists into notifications.
#[derive(Debug, Default)]
pub struct AgentNotifier {
    config: NotificationsConfig,
    /// The `[appearance] pane_title` template, so an agent is named exactly as
    /// the agents panel names it.
    pane_title: Option<String>,
    /// The pane this client is showing the user, as far as the client knows.
    /// It covers what the server's `focused` flag cannot: an older server that
    /// sends no flag, and a pane aliased in a View cell, which is not the
    /// focused pane of any session.
    local_focus: Option<(ConnId, PaneId)>,
    tracked: HashMap<(ConnId, PaneId), Tracked>,
    /// Connections whose first list has been recorded. Until then a list is a
    /// baseline: the agents in it were already in their state when this client
    /// connected, and announcing them would report the past.
    baselined: HashSet<ConnId>,
    /// At most one per pane: a `finished` or an `exited` waiting out [`SETTLE`].
    pending: HashMap<(ConnId, PaneId), Pending>,
}

impl AgentNotifier {
    pub fn new(config: NotificationsConfig, pane_title: Option<String>) -> Self {
        Self {
            config,
            pane_title,
            ..Self::default()
        }
    }

    pub fn config(&self) -> &NotificationsConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: NotificationsConfig) {
        self.config = config;
    }

    pub fn set_pane_title(&mut self, template: Option<String>) {
        self.pane_title = template;
    }

    pub fn set_local_focus(&mut self, focus: Option<(ConnId, PaneId)>) {
        self.local_focus = focus;
    }

    /// Take `conn`'s newest agent list. Returns the `needs_input` events it
    /// causes; `finished` and `exited` are scheduled and come out of
    /// [`AgentNotifier::tick`].
    pub fn observe(
        &mut self,
        conn: &ConnId,
        agents: &[AgentEntry],
        now: Instant,
    ) -> Vec<Notification> {
        if self.baselined.insert(conn.clone()) {
            for entry in agents {
                let run_start = (entry.state == AgentState::Working).then_some(now);
                self.tracked.insert(
                    (conn.clone(), entry.pane_id),
                    Tracked {
                        entry: entry.clone(),
                        run_start,
                        idle_since: None,
                    },
                );
            }
            return Vec::new();
        }

        let mut fired = Vec::new();
        let mut present = HashSet::new();
        for entry in agents {
            let key = (conn.clone(), entry.pane_id);
            present.insert(entry.pane_id);
            if self
                .pending
                .get(&key)
                .is_some_and(|p| p.kind == NotifyKind::Exited)
            {
                self.pending.remove(&key);
            }
            let Some(t) = self.tracked.get_mut(&key) else {
                let run_start = (entry.state == AgentState::Working).then_some(now);
                self.tracked.insert(
                    key.clone(),
                    Tracked {
                        entry: entry.clone(),
                        run_start,
                        idle_since: None,
                    },
                );
                if entry.state == AgentState::NeedsInput {
                    fired.extend(self.emit(NotifyKind::NeedsInput, conn, entry));
                }
                continue;
            };
            let before = t.entry.state;
            t.entry = entry.clone();
            match (before, entry.state) {
                (AgentState::NeedsInput, AgentState::NeedsInput) => {}
                (_, AgentState::NeedsInput) => {
                    t.run_start = None;
                    t.idle_since = None;
                    self.pending.remove(&key);
                    fired.extend(self.emit(NotifyKind::NeedsInput, conn, entry));
                }
                (AgentState::Working, AgentState::Working) => {}
                (_, AgentState::Working) => {
                    let continues = t.run_start.is_some()
                        && t.idle_since
                            .is_some_and(|idle| now.saturating_duration_since(idle) < SETTLE);
                    if !continues {
                        t.run_start = Some(now);
                    }
                    t.idle_since = None;
                    self.pending.remove(&key);
                }
                (AgentState::Working, AgentState::Idle) => {
                    t.idle_since = Some(now);
                    let long_enough = t
                        .run_start
                        .is_some_and(|start| now.saturating_duration_since(start) >= MIN_WORKING);
                    if long_enough {
                        self.pending.insert(
                            key,
                            Pending {
                                kind: NotifyKind::Finished,
                                due: now + SETTLE,
                            },
                        );
                    }
                }
                (_, AgentState::Idle) => {}
            }
        }

        let gone: Vec<(ConnId, PaneId)> = self
            .tracked
            .keys()
            .filter(|(c, pane)| c == conn && !present.contains(pane))
            .cloned()
            .collect();
        for key in gone {
            let already = self
                .pending
                .get(&key)
                .is_some_and(|p| p.kind == NotifyKind::Exited);
            if !already {
                self.pending.insert(
                    key,
                    Pending {
                        kind: NotifyKind::Exited,
                        due: now + SETTLE,
                    },
                );
            }
        }
        fired
    }

    /// Fire every scheduled event that is due at `now`, in the order they fell
    /// due.
    pub fn tick(&mut self, now: Instant) -> Vec<Notification> {
        let mut due: Vec<((ConnId, PaneId), Pending)> = self
            .pending
            .iter()
            .filter(|(_, p)| p.due <= now)
            .map(|(k, p)| (k.clone(), *p))
            .collect();
        due.sort_by_key(|(_, p)| p.due);

        let mut fired = Vec::new();
        for (key, p) in due {
            self.pending.remove(&key);
            match p.kind {
                NotifyKind::Exited => {
                    if let Some(t) = self.tracked.remove(&key) {
                        fired.extend(self.emit(NotifyKind::Exited, &key.0, &t.entry));
                    }
                }
                NotifyKind::NeedsInput => {}
                NotifyKind::Finished => {
                    let Some(t) = self.tracked.get_mut(&key) else {
                        continue;
                    };
                    t.run_start = None;
                    t.idle_since = None;
                    if t.entry.state == AgentState::Idle {
                        let entry = t.entry.clone();
                        fired.extend(self.emit(p.kind, &key.0, &entry));
                    }
                }
            }
        }
        fired
    }

    /// When [`AgentNotifier::tick`] next has something to fire, if ever.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.due).min()
    }

    /// Forget every connection `keep` rejects, without firing anything for it.
    /// A connection that drops takes its agents out of view, which is not the
    /// same as those agents exiting, and its next list is a baseline again.
    pub fn retain(&mut self, mut keep: impl FnMut(&ConnId) -> bool) {
        self.baselined.retain(|c| keep(c));
        self.tracked.retain(|(c, _), _| self.baselined.contains(c));
        self.pending.retain(|(c, _), _| self.baselined.contains(c));
    }

    fn emit(&self, kind: NotifyKind, conn: &ConnId, entry: &AgentEntry) -> Option<Notification> {
        let wanted = self.config.enabled
            && match kind {
                NotifyKind::NeedsInput => self.config.needs_input,
                NotifyKind::Finished => self.config.finished,
                NotifyKind::Exited => self.config.exited,
            };
        if !wanted {
            return None;
        }
        let pane = entry.pane_id;
        if entry.focused || self.local_focus.as_ref() == Some(&(conn.clone(), pane)) {
            log::debug!(
                "notify: {kind:?} for {conn:?} pane_id={pane} skipped, the pane is focused"
            );
            return None;
        }
        let row = AgentRow {
            conn: conn.clone(),
            entry: entry.clone(),
            pane_title: self.pane_title.clone(),
        };
        let host = match conn {
            ConnId::Local => String::new(),
            ConnId::Remote(name) => format!("{name}:"),
        };
        Some(Notification {
            kind,
            conn: conn.clone(),
            pane_id: pane,
            title: format!("{} {}", fit(&row.label(), kind.verb()), kind.verb()),
            body: format!("{host}{} · tab {}", entry.session, entry.tab_index + 1),
        })
    }
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Other
        }
    }
}

/// The program and arguments that show `title` and `body`, or `None` when
/// there is nothing to run.
///
/// The text is only ever a separate argument. A title comes from an `OSC 0`/`2`
/// sequence that any program in a pane can emit, so text spliced into the
/// AppleScript source or a shell line would let that program run commands on
/// this machine. The macOS script is a constant that reads its text from
/// `argv`, and `--` stops both notifiers from reading text that starts with
/// `-` as an option.
pub fn notifier_argv(
    platform: Platform,
    command: Option<&[String]>,
    title: &str,
    body: &str,
) -> Option<Vec<String>> {
    if let Some(command) = command {
        if command.is_empty() {
            return None;
        }
        return Some(
            command
                .iter()
                .map(|arg| match arg.as_str() {
                    "{title}" => title.to_string(),
                    "{body}" => body.to_string(),
                    _ => arg.clone(),
                })
                .collect(),
        );
    }
    let argv: &[&str] = match platform {
        Platform::MacOs => &[
            "osascript",
            "-e",
            "on run argv",
            "-e",
            "display notification (item 1 of argv) with title (item 2 of argv)",
            "-e",
            "end run",
            "--",
            body,
            title,
        ],
        Platform::Linux => &["notify-send", "--app-name=Remux", "--", title, body],
        Platform::Other => return None,
    };
    Some(argv.iter().map(|a| a.to_string()).collect())
}

/// `label` sanitised and cut short enough that `"{label} {verb}"` is within
/// [`MAX_TEXT_CHARS`], so that a long name never pushes the event out of the
/// title.
fn fit(label: &str, verb: &str) -> String {
    let room = MAX_TEXT_CHARS.saturating_sub(verb.chars().count() + 1);
    sanitize(label).chars().take(room).collect()
}

/// `text` without control or invisible format characters, cut to
/// [`MAX_TEXT_CHARS`].
pub fn sanitize(text: &str) -> String {
    crate::config::pane_title::strip_unprintable(text)
        .chars()
        .take(MAX_TEXT_CHARS)
        .collect()
}

/// The file name two clients both race to create for the same event.
///
/// Hashed with `DefaultHasher::new()`, whose keys are fixed, so every client
/// process built from the same source computes the same name. `RandomState` is
/// seeded per process and would give each client a different name.
fn marker_name(server: &str, pane_id: PaneId, kind: NotifyKind) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    (server, pane_id, kind).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Whether the client with process id `pid` should send the event `name`
/// stands for: no other client has claimed it within [`DEDUPE_WINDOW`].
///
/// Every client attached to a server sees the same agent list and would
/// otherwise each show the same notification. The marker holds the pid of the
/// client that wrote it, so that a fresh marker stops only OTHER clients. The
/// same client sending the same event again inside the window, such as a second
/// `needs_input` from the same pane, is a new event and is delivered.
///
/// Best-effort by design: two clients that race past each other both deliver,
/// and an I/O failure delivers, because a duplicate is a smaller fault than a
/// lost notification.
fn claim(dir: &Path, name: &str, now: SystemTime, pid: u32) -> bool {
    use std::os::unix::fs::DirBuilderExt;

    if let Err(e) = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        log::debug!("notify: cannot make {}: {e}", dir.display());
        return true;
    }
    prune(dir, now);
    let path = dir.join(name);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(_) => {
            take(&path, now, pid);
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified());
            // A marker written after `now` was read was written by a client
            // that is delivering this event right now.
            let fresh = modified.is_ok_and(|m| {
                now.duration_since(m)
                    .map_or(true, |age| age < DEDUPE_WINDOW)
            });
            // An empty or unreadable owner is another client between creating
            // the marker and writing its pid, so it counts as someone else's.
            let owner = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            if fresh && owner != Some(pid) {
                return false;
            }
            take(&path, now, pid);
            true
        }
        Err(e) => {
            log::debug!("notify: cannot create {}: {e}", path.display());
            true
        }
    }
}

/// Record `pid` as the owner of the marker at `path`, as of `now`.
fn take(path: &Path, now: SystemTime, pid: u32) {
    let written = std::fs::write(path, pid.to_string()).and_then(|()| {
        std::fs::File::options()
            .write(true)
            .open(path)?
            .set_modified(now)
    });
    if let Err(e) = written {
        log::debug!("notify: cannot write {}: {e}", path.display());
    }
}

/// Remove the markers whose window has passed. Pane ids only grow, so without
/// this the directory would gain a file for every agent pane that ever
/// notified.
fn prune(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let expired = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|m| now.duration_since(m).is_ok_and(|age| age >= DEDUPE_WINDOW));
        if expired {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Where the markers live: a per-user directory beside the local server's
/// socket. The user id is in the name because the socket directory falls back
/// to `/tmp`, which every user shares.
fn marker_dir() -> std::path::PathBuf {
    let socket = crate::server::daemon::socket_path();
    let parent = socket.parent().unwrap_or(Path::new("/tmp"));
    // SAFETY: `getuid` takes no arguments, cannot fail and touches no memory.
    let uid = unsafe { libc::getuid() };
    parent.join(format!("remux-notify-{uid}"))
}

/// The server half of the dedupe key. The local server is named by its socket
/// so that two local servers on different sockets are told apart.
fn server_identity(conn: &ConnId) -> String {
    match conn {
        ConnId::Local => crate::server::daemon::socket_path().display().to_string(),
        ConnId::Remote(_) => conn.key(),
    }
}

static SPAWN_WARNED: AtomicBool = AtomicBool::new(false);
static UNSUPPORTED_WARNED: AtomicBool = AtomicBool::new(false);

/// Log at `warn` the first time `flag` is raised in this process, and at
/// `debug` after that, so a missing notifier is reported once rather than on
/// every agent event.
fn warn_once(flag: &AtomicBool, message: &str) {
    if flag.swap(true, Ordering::Relaxed) {
        log::debug!("{message}");
    } else {
        log::warn!("{message}");
    }
}

/// Show `n` on this machine, without blocking the caller.
///
/// The dedupe claim, the spawn and the wait run on a detached thread, so
/// neither file I/O nor a slow notifier can stall the render loop. Every
/// standard stream is `/dev/null`: the client's terminal is in raw mode, and a
/// child that read from it or wrote to it would corrupt the screen or steal
/// keystrokes.
pub fn deliver(n: &Notification, config: &NotificationsConfig) {
    let title = sanitize(&n.title);
    let body = sanitize(&n.body);
    let Some(argv) = notifier_argv(
        Platform::current(),
        config.command.as_deref(),
        &title,
        &body,
    ) else {
        warn_once(
            &UNSUPPORTED_WARNED,
            "notify: no desktop notifier on this platform; set [notifications] command",
        );
        return;
    };
    let marker = marker_name(&server_identity(&n.conn), n.pane_id, n.kind);
    let spawned = std::thread::Builder::new()
        .name("remux-notify".to_string())
        .spawn(move || {
            if !claim(
                &marker_dir(),
                &marker,
                SystemTime::now(),
                std::process::id(),
            ) {
                log::debug!("notify: another client already sent {marker}");
                return;
            }
            let child = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match child {
                Ok(mut child) => {
                    if let Err(e) = child.wait() {
                        log::debug!("notify: waiting for {} failed: {e}", argv[0]);
                    }
                }
                Err(e) => warn_once(
                    &SPAWN_WARNED,
                    &format!("notify: cannot run {}: {e}", argv[0]),
                ),
            }
        });
    if let Err(e) = spawned {
        log::debug!("notify: cannot start the delivery thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AgentState;
    use std::time::Duration;

    const REMOTE: &str = "far";

    fn remote() -> ConnId {
        ConnId::Remote(REMOTE.to_string())
    }

    fn agent(pane_id: PaneId, state: AgentState) -> AgentEntry {
        AgentEntry {
            pane_id,
            session: "main".to_string(),
            tab_index: 0,
            command: "claude".to_string(),
            state,
            title: None,
            custom_name: None,
            cwd: None,
            focused: false,
        }
    }

    fn focused(mut entry: AgentEntry) -> AgentEntry {
        entry.focused = true;
        entry
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn kinds(fired: &[Notification]) -> Vec<NotifyKind> {
        fired.iter().map(|n| n.kind).collect()
    }

    /// A notifier that has already seen `agents` as the baseline on `conn`.
    fn baselined(conn: &ConnId, agents: &[AgentEntry], t0: Instant) -> AgentNotifier {
        let mut n = AgentNotifier::new(NotificationsConfig::default(), None);
        assert!(n.observe(conn, agents, t0).is_empty());
        n
    }

    /// Drive pane 1 through `Working` for `run`, then `Idle`, and return the
    /// notifier and the instant it went idle.
    fn worked_then_idle(run: Duration) -> (AgentNotifier, Instant) {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Idle)], t0);
        assert!(n
            .observe(&ConnId::Local, &[agent(1, AgentState::Working)], t0)
            .is_empty());
        let idle_at = t0 + run;
        assert!(n
            .observe(&ConnId::Local, &[agent(1, AgentState::Idle)], idle_at)
            .is_empty());
        (n, idle_at)
    }

    #[test]
    fn the_first_list_from_a_connection_is_a_baseline_and_fires_nothing() {
        let t0 = Instant::now();
        let mut n = AgentNotifier::new(NotificationsConfig::default(), None);
        let fired = n.observe(&ConnId::Local, &[agent(1, AgentState::NeedsInput)], t0);
        assert!(fired.is_empty());
        assert!(n.tick(t0 + secs(60)).is_empty());
    }

    #[test]
    fn a_pane_that_starts_waiting_fires_needs_input_once() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        let fired = n.observe(&ConnId::Local, &[agent(1, AgentState::NeedsInput)], t0);
        assert_eq!(kinds(&fired), vec![NotifyKind::NeedsInput]);
        let again = n.observe(
            &ConnId::Local,
            &[agent(1, AgentState::NeedsInput)],
            t0 + secs(1),
        );
        assert!(again.is_empty(), "still blocked is not a new event");
    }

    #[test]
    fn a_new_agent_that_appears_waiting_fires_needs_input() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[], t0);
        let fired = n.observe(&ConnId::Local, &[agent(7, AgentState::NeedsInput)], t0);
        assert_eq!(kinds(&fired), vec![NotifyKind::NeedsInput]);
        assert_eq!(fired[0].pane_id, 7);
    }

    #[test]
    fn a_short_run_of_work_fires_nothing() {
        let (mut n, idle_at) = worked_then_idle(secs(2));
        assert_eq!(n.next_deadline(), None);
        assert!(n.tick(idle_at + secs(60)).is_empty());
    }

    #[test]
    fn a_long_run_fires_finished_only_after_the_settle() {
        let (mut n, idle_at) = worked_then_idle(secs(10));
        assert_eq!(n.next_deadline(), Some(idle_at + SETTLE));
        assert!(n
            .tick(idle_at + SETTLE - Duration::from_millis(1))
            .is_empty());
        let fired = n.tick(idle_at + SETTLE);
        assert_eq!(kinds(&fired), vec![NotifyKind::Finished]);
        assert!(n.tick(idle_at + secs(60)).is_empty(), "fires once");
        assert_eq!(n.next_deadline(), None);
    }

    #[test]
    fn work_resumed_within_the_settle_cancels_the_finish() {
        let (mut n, idle_at) = worked_then_idle(secs(10));
        n.observe(
            &ConnId::Local,
            &[agent(1, AgentState::Working)],
            idle_at + secs(1),
        );
        assert!(n.tick(idle_at + secs(60)).is_empty());
    }

    #[test]
    fn a_pause_inside_a_run_does_not_split_it_into_short_runs() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Idle)], t0);
        let mut t = t0;
        for _ in 0..3 {
            n.observe(&ConnId::Local, &[agent(1, AgentState::Working)], t);
            t += Duration::from_millis(1500);
            n.observe(&ConnId::Local, &[agent(1, AgentState::Idle)], t);
            t += Duration::from_millis(600);
        }
        let fired = n.tick(t + secs(60));
        assert_eq!(
            kinds(&fired),
            vec![NotifyKind::Finished],
            "three 1.5 s bursts 0.6 s apart are one 5 s task"
        );
    }

    #[test]
    fn work_that_resumes_after_a_settled_finish_starts_a_new_run() {
        let (mut n, idle_at) = worked_then_idle(secs(10));
        assert_eq!(kinds(&n.tick(idle_at + SETTLE)), vec![NotifyKind::Finished]);
        let t = idle_at + secs(30);
        n.observe(&ConnId::Local, &[agent(1, AgentState::Working)], t);
        n.observe(&ConnId::Local, &[agent(1, AgentState::Idle)], t + secs(1));
        assert!(n.tick(t + secs(60)).is_empty(), "a 1 s run is not a task");
    }

    #[test]
    fn needs_input_cancels_a_pending_finish() {
        let (mut n, idle_at) = worked_then_idle(secs(10));
        let fired = n.observe(
            &ConnId::Local,
            &[agent(1, AgentState::NeedsInput)],
            idle_at + secs(1),
        );
        assert_eq!(kinds(&fired), vec![NotifyKind::NeedsInput]);
        assert!(n.tick(idle_at + secs(60)).is_empty());
    }

    #[test]
    fn a_pane_that_leaves_the_list_fires_exited_after_the_settle() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Idle)], t0);
        assert!(n.observe(&ConnId::Local, &[], t0).is_empty());
        assert!(n.tick(t0 + SETTLE - Duration::from_millis(1)).is_empty());
        let fired = n.tick(t0 + SETTLE);
        assert_eq!(kinds(&fired), vec![NotifyKind::Exited]);
        assert_eq!(fired[0].pane_id, 1);
        assert!(n.tick(t0 + secs(60)).is_empty());
    }

    #[test]
    fn a_pane_that_comes_back_within_the_settle_has_not_exited() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Idle)], t0);
        n.observe(&ConnId::Local, &[], t0);
        let fired = n.observe(&ConnId::Local, &[agent(1, AgentState::Idle)], t0 + secs(1));
        assert!(fired.is_empty());
        assert!(n.tick(t0 + secs(60)).is_empty());
    }

    #[test]
    fn a_waiting_pane_that_blinks_out_and_back_is_not_a_new_needs_input() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::NeedsInput)], t0);
        n.observe(&ConnId::Local, &[], t0);
        let fired = n.observe(
            &ConnId::Local,
            &[agent(1, AgentState::NeedsInput)],
            t0 + secs(1),
        );
        assert!(fired.is_empty());
    }

    #[test]
    fn a_dropped_connection_fires_nothing() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Idle)], t0);
        n.observe(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        n.observe(&ConnId::Local, &[agent(1, AgentState::Idle)], t0 + secs(10));
        n.retain(|_| false);
        assert_eq!(n.next_deadline(), None);
        assert!(n.tick(t0 + secs(60)).is_empty());
    }

    #[test]
    fn a_connection_that_comes_back_is_baselined_again() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Idle)], t0);
        n.retain(|_| false);
        let fired = n.observe(&ConnId::Local, &[agent(2, AgentState::NeedsInput)], t0);
        assert!(fired.is_empty());
        assert!(n.tick(t0 + secs(60)).is_empty(), "pane 1 did not exit");
    }

    #[test]
    fn retain_forgets_only_the_connections_it_rejects() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Idle)], t0);
        n.observe(&remote(), &[agent(1, AgentState::Idle)], t0);
        n.retain(|c| *c == ConnId::Local);
        n.observe(&ConnId::Local, &[], t0);
        let fired = n.tick(t0 + secs(60));
        assert_eq!(kinds(&fired), vec![NotifyKind::Exited]);
        assert_eq!(fired[0].conn, ConnId::Local);
    }

    #[test]
    fn a_pane_focused_in_any_client_suppresses_needs_input() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        let fired = n.observe(
            &ConnId::Local,
            &[focused(agent(1, AgentState::NeedsInput))],
            t0,
        );
        assert!(fired.is_empty());
    }

    #[test]
    fn a_pane_focused_at_the_end_of_the_settle_suppresses_finished() {
        let (mut n, idle_at) = worked_then_idle(secs(10));
        n.observe(
            &ConnId::Local,
            &[focused(agent(1, AgentState::Idle))],
            idle_at + secs(1),
        );
        assert!(n.tick(idle_at + SETTLE).is_empty());
    }

    #[test]
    fn a_pane_last_seen_focused_suppresses_exited() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[focused(agent(1, AgentState::Idle))], t0);
        n.observe(&ConnId::Local, &[], t0);
        assert!(n.tick(t0 + SETTLE).is_empty());
    }

    #[test]
    fn the_clients_own_focus_suppresses_an_event() {
        let t0 = Instant::now();
        let mut n = baselined(&remote(), &[agent(1, AgentState::Working)], t0);
        n.set_local_focus(Some((remote(), 1)));
        assert!(n
            .observe(&remote(), &[agent(1, AgentState::NeedsInput)], t0)
            .is_empty());
    }

    #[test]
    fn the_clients_own_focus_on_another_server_suppresses_nothing() {
        let t0 = Instant::now();
        let mut n = baselined(&remote(), &[agent(1, AgentState::Working)], t0);
        n.set_local_focus(Some((ConnId::Local, 1)));
        let fired = n.observe(&remote(), &[agent(1, AgentState::NeedsInput)], t0);
        assert_eq!(kinds(&fired), vec![NotifyKind::NeedsInput]);
    }

    #[test]
    fn a_disabled_event_does_not_fire() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        n.set_config(NotificationsConfig {
            needs_input: false,
            ..NotificationsConfig::default()
        });
        assert!(n
            .observe(&ConnId::Local, &[agent(1, AgentState::NeedsInput)], t0)
            .is_empty());

        let (mut n, idle_at) = worked_then_idle(secs(10));
        n.set_config(NotificationsConfig {
            finished: false,
            ..NotificationsConfig::default()
        });
        assert!(n.tick(idle_at + SETTLE).is_empty());

        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Idle)], t0);
        n.set_config(NotificationsConfig {
            exited: false,
            ..NotificationsConfig::default()
        });
        n.observe(&ConnId::Local, &[], t0);
        assert!(n.tick(t0 + SETTLE).is_empty());
    }

    #[test]
    fn the_master_switch_silences_every_event() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        n.set_config(NotificationsConfig {
            enabled: false,
            ..NotificationsConfig::default()
        });
        assert!(n
            .observe(&ConnId::Local, &[agent(1, AgentState::NeedsInput)], t0)
            .is_empty());
    }

    #[test]
    fn two_servers_with_the_same_pane_id_do_not_collide() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        n.observe(&remote(), &[agent(1, AgentState::Working)], t0);
        let fired = n.observe(&remote(), &[agent(1, AgentState::NeedsInput)], t0);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].conn, remote());
        // The local pane 1 did not leave because the remote one changed.
        assert!(n
            .observe(&ConnId::Local, &[agent(1, AgentState::Working)], t0)
            .is_empty());
        assert!(n.tick(t0 + secs(60)).is_empty());
    }

    #[test]
    fn the_title_is_the_agents_panel_label_and_the_body_says_where() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        let mut entry = agent(1, AgentState::NeedsInput);
        entry.tab_index = 2;
        let fired = n.observe(&ConnId::Local, &[entry], t0);
        assert_eq!(fired[0].title, "claude main/2 needs input");
        assert_eq!(fired[0].body, "main · tab 3");
    }

    #[test]
    fn a_remote_agent_is_named_with_its_host() {
        let t0 = Instant::now();
        let mut n = baselined(&remote(), &[agent(1, AgentState::Idle)], t0);
        n.observe(&remote(), &[], t0);
        let fired = n.tick(t0 + SETTLE);
        assert_eq!(fired[0].title, "claude far:main/0 exited");
        assert_eq!(fired[0].body, "far:main · tab 1");
    }

    #[test]
    fn the_title_follows_the_pane_title_template() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        n.set_pane_title(Some("{host}{command}@{session}".to_string()));
        let fired = n.observe(&ConnId::Local, &[agent(1, AgentState::NeedsInput)], t0);
        assert_eq!(fired[0].title, "claude@main needs input");
    }

    #[test]
    fn a_name_too_long_to_show_keeps_its_event() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        let mut entry = agent(1, AgentState::NeedsInput);
        entry.custom_name = Some("n".repeat(500));
        let fired = n.observe(&ConnId::Local, &[entry], t0);
        assert!(
            fired[0].title.ends_with("n needs input"),
            "{}",
            fired[0].title
        );
        assert_eq!(fired[0].title.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn a_name_the_user_gave_the_pane_names_the_agent() {
        let t0 = Instant::now();
        let mut n = baselined(&ConnId::Local, &[agent(1, AgentState::Working)], t0);
        let mut entry = agent(1, AgentState::NeedsInput);
        entry.custom_name = Some("reviewer".to_string());
        let fired = n.observe(&ConnId::Local, &[entry], t0);
        assert_eq!(fired[0].title, "reviewer needs input");
    }

    // -- Delivery -------------------------------------------------------------

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn macos_passes_the_text_as_arguments_to_a_fixed_script() {
        let argv = notifier_argv(Platform::MacOs, None, "a \"title\"", "$(rm -rf ~)").unwrap();
        assert_eq!(
            argv,
            strings(&[
                "osascript",
                "-e",
                "on run argv",
                "-e",
                "display notification (item 1 of argv) with title (item 2 of argv)",
                "-e",
                "end run",
                "--",
                "$(rm -rf ~)",
                "a \"title\"",
            ])
        );
    }

    #[test]
    fn linux_ends_the_options_before_the_text() {
        let argv = notifier_argv(Platform::Linux, None, "-t", "--help").unwrap();
        assert_eq!(
            argv,
            strings(&["notify-send", "--app-name=Remux", "--", "-t", "--help"])
        );
    }

    #[test]
    fn another_platform_has_no_notifier_of_its_own() {
        assert_eq!(notifier_argv(Platform::Other, None, "t", "b"), None);
    }

    #[test]
    fn a_custom_command_substitutes_whole_arguments_only() {
        let command = strings(&["notifier", "-t", "{title}", "-m", "{body}", "x{title}"]);
        let argv = notifier_argv(Platform::Other, Some(&command), "T; ls", "B").unwrap();
        assert_eq!(
            argv,
            strings(&["notifier", "-t", "T; ls", "-m", "B", "x{title}"])
        );
    }

    #[test]
    fn a_custom_command_wins_on_every_platform() {
        let command = strings(&["mine", "{title}"]);
        for platform in [Platform::MacOs, Platform::Linux] {
            assert_eq!(
                notifier_argv(platform, Some(&command), "T", "B"),
                Some(strings(&["mine", "T"]))
            );
        }
    }

    #[test]
    fn an_empty_custom_command_runs_nothing() {
        assert_eq!(notifier_argv(Platform::MacOs, Some(&[]), "T", "B"), None);
    }

    #[test]
    fn sanitising_strips_control_characters_and_bounds_the_length() {
        assert_eq!(sanitize("ok\x1b]0;evil\x07\nline\u{202e}"), "ok]0;evilline");
        assert_eq!(sanitize(&"x".repeat(500)).chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn the_marker_name_does_not_depend_on_a_random_seed() {
        // Each `RandomState::new()` is seeded differently even within one
        // process, so a hasher built from one would fail this.
        assert_eq!(
            marker_name("/tmp/remux.sock", 4, NotifyKind::Finished),
            marker_name("/tmp/remux.sock", 4, NotifyKind::Finished)
        );
    }

    #[test]
    fn the_marker_name_tells_servers_panes_and_events_apart() {
        let base = marker_name("local", 1, NotifyKind::Finished);
        assert_ne!(base, marker_name("remote:far", 1, NotifyKind::Finished));
        assert_ne!(base, marker_name("local", 2, NotifyKind::Finished));
        assert_ne!(base, marker_name("local", 1, NotifyKind::Exited));
    }

    const A: u32 = 1001;
    const B: u32 = 1002;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rmx-notify-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_first_client_to_claim_an_event_sends_it_and_the_second_does_not() {
        let dir = scratch_dir("claim");
        let now = SystemTime::now();
        assert!(claim(&dir, "m", now, A));
        assert!(!claim(&dir, "m", now + Duration::from_secs(1), B));
        assert!(claim(&dir, "other", now, B));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_client_repeating_its_own_event_inside_the_window_sends_it_again() {
        let dir = scratch_dir("repeat");
        let now = SystemTime::now();
        assert!(claim(&dir, "m", now, A));
        assert!(claim(&dir, "m", now + Duration::from_secs(6), A));
        assert!(
            !claim(&dir, "m", now + Duration::from_secs(7), B),
            "the repeat still belongs to the first client"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_marker_with_no_owner_is_left_to_whoever_is_writing_it() {
        let dir = scratch_dir("owner");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m"), b"").unwrap();
        assert!(!claim(&dir, "m", SystemTime::now(), A));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_marker_older_than_the_window_is_claimed_again() {
        let dir = scratch_dir("stale");
        let then = SystemTime::now() - Duration::from_secs(60);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("m"))
            .unwrap()
            .set_modified(then)
            .unwrap();
        assert!(claim(&dir, "m", SystemTime::now(), A));
        assert!(!claim(&dir, "m", SystemTime::now(), B));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_that_cannot_be_made_still_delivers() {
        let file = scratch_dir("blocked");
        std::fs::write(&file, b"").unwrap();
        assert!(claim(&file.join("sub"), "m", SystemTime::now(), A));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn claiming_prunes_markers_past_the_window() {
        let dir = scratch_dir("prune");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("old"))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        assert!(claim(&dir, "new", SystemTime::now(), A));
        assert!(!dir.join("old").exists());
        assert!(dir.join("new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
