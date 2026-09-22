//! Detecting AI coding agents in panes, and classifying what they are doing.
//!
//! Two jobs, both pure enough to test on their own:
//!
//! * **Detection** ([`foreground_command`]) reads the PTY's foreground process
//!   group and asks the OS what it is. `Pty::child_pid` is the login SHELL, so
//!   the pane's own name is `zsh` however long `claude` has been running in it;
//!   `tcgetpgrp` is what sees past that. It then matches on TWO names -- the
//!   process name and its `argv[0]` -- because the platforms answer differently
//!   and Claude Code falls in the gap; see
//!   [`crate::server::daemon::ProcessNames`]. And when the leader is a LAUNCHER
//!   rather than the agent -- an npm shim, `npx`, `bunx` -- the job underneath
//!   it is walked too, because the shim stays alive as the agent's parent and
//!   neither of ITS names can ever match. Deliberately scoped to agent
//!   detection -- pane NAMES are untouched, because fixing them the same way
//!   would rename every pane border in the product and that needs its own
//!   decision.
//! * **Classification** ([`AgentRules::classify`]) is a pure function of the
//!   visible screen and how long ago output arrived.
//!
//! ## The classifier is stateless, and that is the whole design
//!
//! Precedence, per sample:
//!
//! 1. a configured pattern matches the bottom of the LIVE screen -> `NeedsInput`
//! 2. else output within the working window -> `Working`
//! 3. else -> `Idle`
//!
//! `NeedsInput` outranks `Working` on purpose: a spinner underneath an approval
//! prompt is still blocked.
//!
//! The property that matters is that `NeedsInput` never decays on silence, and
//! it holds here for free rather than being implemented: a blocked agent's
//! prompt is STILL ON SCREEN, so every later sample matches the same pattern
//! again. This is the bug the original spec would have shipped -- "`Working` if
//! output was recent, `Idle` otherwise" decays a blocked agent to `Idle` at
//! exactly the moment the user needs to see it, because it produces no output
//! precisely BECAUSE it is waiting. Getting there without a sticky flag also
//! means there is no flag to leak, and no drain to coordinate with anyone.
//!
//! The terminal bell is deliberately not an input. A bare bell may not set
//! `NeedsInput` (it is user-configurable in Claude Code and other agents never
//! ring it), and with the rule above there is nothing else left for it to do.

use std::os::fd::BorrowedFd;
use std::time::Duration;

use regex::Regex;

use crate::config::agents::AgentsConfig;
use crate::protocol::AgentState;
use crate::screen::Screen;
use crate::server::daemon::ProcessNames;

/// Whether this build can detect agents at all.
///
/// [`foreground_command`] resolves the foreground process group through
/// `get_process_name`, which knows Linux (`/proc/<pgid>/comm`) and macOS
/// (`sysinfo`) and falls back to `"shell"` on anything else -- and a server that
/// always answers `"shell"` can never match a configured command, so no pane
/// could ever be classified. Reported to the client
/// (`ServerMessage::AgentList`) so the panel can say why it is empty rather than
/// looking broken.
///
/// `tcgetpgrp` needs no split, so the platform question is only ever "can this
/// build NAME a pid" -- and this mirrors `get_process_name`'s cfg arms exactly
/// rather than inventing a second platform split, because if the two ever
/// disagree the panel is lying in one direction or the other.
///
/// **The reason `tcgetpgrp` needs no split is NOT that it is POSIX**, which is
/// what this comment used to say and is what sent a later reader hunting a
/// non-existent macOS bug here. POSIX says nothing about calling it on a PTY
/// MASTER, which is not a controlling terminal -- so portability is a matter of
/// what each kernel chose. Both of ours chose to answer. XNU special-cases it in
/// `ptyioctl` on the controller side, precisely to avoid the controlling-
/// terminal requirement (`bsd/kern/tty_dev.c`, and `tty_ptmx.c` wires the ptmx
/// master's `d_open` to the `ptcopen` this branch tests for):
///
/// ```c
/// } else if (cdevsw[major(dev)].d_open == ptcopen) {
///     switch (cmd) {
///     case TIOCGPGRP:
///         /*
///          * We aviod calling ttioctl on the controller since,
///          * in that case, tp must be the controlling terminal.
///          */
///         *(int *)data = tp->t_pgrp ? tp->t_pgrp->pg_id : 0;
/// ```
///
/// Linux's `ptmx` implements `TIOCGPGRP` on the master directly; FreeBSD reaches
/// the same place by redirecting unhandled master ioctls to the slave tty. Note
/// the `: 0` above: a master with no session attached reports pgid **0**, not an
/// error, and pid 0 names nothing -- so that reads as "no agent", not a panic.
///
/// COMPILE-TIME, so it reports what this BUILD could do, never what a given
/// sample actually did: a Linux server with no `/proc` mounted, or one whose
/// `commands` list is empty, still reports `true` with nothing listed. See
/// `ServerMessage::AgentList`'s field docs.
pub const DETECTION_SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// The CONFIGURED agent command running in the foreground of this PTY, e.g.
/// `"claude"`, or `None` if this pane is not running one.
///
/// `None` also when the PTY has no foreground group to report -- a child that is
/// exiting, or already gone. NEVER a panic: this runs on a timer against panes
/// that close underneath it, which is the exact boundary at which Phase C found
/// two latent PTY bugs.
///
/// The `rules` are taken here rather than the raw name being returned for the
/// caller to test, because the OS offers SEVERAL names for a process and which
/// one matched is not the caller's business -- see [`crate::server::daemon::ProcessNames`] for why
/// there is more than one, and [`AgentRules::match_command`] for the order they
/// are tried in. What comes back is always a string from `[agents] commands`,
/// which is what `AgentEntry::command` promises and what `classify`'s
/// per-command patterns are scoped against.
///
/// The whole foreground JOB is asked, not just its leader. An agent installed
/// through npm is a launcher -- `codex` is a Node shim that `spawn()`s the real
/// binary and stays ALIVE as its parent, so the process group's leader is node
/// (whose `comm` is `MainThread` from Node 22 on) and the agent is a child
/// inside the same group. Neither of the leader's names can ever match, and
/// that pane was invisible. `npx` and `bunx` have the same shape.
///
/// The leader is still tried FIRST and on its own, which is load-bearing twice
/// over: it leaves every pane the old rule got right decided by the old rule,
/// and it means a pane that matches costs no walk at all. Only a leader that
/// MISSES pays for [`job_candidates`], and an idle shell pays one empty
/// `children` read for it.
///
/// Listing the launchers in `[agents] commands` instead would be the wrong fix
/// in the obvious way: `node` names every REPL on the machine.
///
/// The walk is confined to the foreground process GROUP. A child that has been
/// parked -- `Ctrl-Z`, or `&` under an interactive shell -- is in a group of its
/// own, and the pane it left behind is showing a shell prompt: classifying that
/// screen would report the SHELL's silence as the agent's state. Leader-only
/// detection never listed a parked job, so this is what keeps that true.
///
/// The log line is the only diagnostic anyone gets for "the panel is empty but
/// the agent is right there", so it says which names were read rather than just
/// that nothing matched -- and, when a launcher was looked through, which pid
/// the agent was actually found at. See [`log_detection`] for why it is
/// deduplicated instead of levelled down.
pub fn foreground_command(fd: BorrowedFd<'_>, rules: &AgentRules) -> Option<String> {
    let pgid = nix::unistd::tcgetpgrp(fd).ok()?;
    let leader_pid = pgid.as_raw();
    let leader = crate::server::daemon::get_process_names(leader_pid);
    if let Some(command) = rules.match_command(&leader.name, leader.argv0.as_deref()) {
        let command = command.to_string();
        log_detection(leader_pid, &leader, Detected::Leader(&command));
        return Some(command);
    }
    // Only the processes of THIS job. `child_pids` lists every child the
    // leader has, and a job the user PARKED -- `Ctrl-Z`, or `&` under an
    // interactive shell -- is in another process group while the pane's screen
    // shows a shell prompt. Listing it would report the shell's silence as the
    // agent's state, and leader-only detection never listed a parked job, so
    // keeping it unlisted is the non-regression rather than a narrowing. A
    // candidate outside the group is not descended through either: its own
    // children are in the parked job too.
    //
    // `tcgetpgrp` returns the group's ID, so `leader_pid` IS the pgid to match.
    let in_job = |pid: i32| {
        crate::server::daemon::child_pids(pid)
            .into_iter()
            .filter(|&child| crate::server::daemon::process_pgid(child) == Some(leader_pid))
            .collect::<Vec<_>>()
    };
    // The leader has already been asked, so its own entry is dropped rather
    // than its names being read a second time.
    let descendants = job_candidates(leader_pid, in_job).skip(1);
    let hit = first_match(
        rules,
        descendants.map(|pid| (pid, crate::server::daemon::get_process_names(pid))),
    );
    match hit {
        Some((pid, names, command)) => {
            let command = command.to_string();
            log_detection(
                leader_pid,
                &leader,
                Detected::Descendant {
                    pid,
                    names: &names,
                    command: &command,
                },
            );
            Some(command)
        }
        None => {
            log_detection(leader_pid, &leader, Detected::Nothing);
            None
        }
    }
}

/// How many generations past the foreground leader the job walk looks.
///
/// Three, because a launcher chain is short and a process tree is not: `npx`
/// -> the shim -> the agent is the deepest real case, and everything below
/// that is the agent's OWN children (a `git`, a `rg`, a build) which no
/// configured command should ever be found among.
const MAX_DEPTH: u32 = 3;

/// How many processes the job walk will name before giving up, the leader
/// included.
///
/// This is a COST bound, not a correctness one. `collect_agents`
/// (`server/daemon.rs`) runs the walk for every pane at up to ten samples a
/// second, and a pane whose
/// leader is a build tool has a wide tree under it -- so the walk is capped
/// rather than allowed to become a `/proc` sweep driven by whatever the user is
/// running. A launcher starts ONE agent, so a real hit is in the first few
/// candidates or it is not there.
const MAX_CANDIDATES: usize = 32;

/// The foreground job's processes in the order they should be tried: the
/// leader, then its descendants breadth-first, bounded by [`MAX_DEPTH`] and
/// [`MAX_CANDIDATES`].
///
/// `children` is a parameter rather than a direct `/proc` read so the ORDER and
/// the BOUNDS can be tested without a process tree to arrange.
fn job_candidates<F: FnMut(i32) -> Vec<i32>>(leader: i32, children: F) -> JobWalk<F> {
    JobWalk {
        frontier: vec![leader],
        next: 0,
        depth: 0,
        yielded: 0,
        done: false,
        children,
    }
}

/// [`job_candidates`]' iterator. A generation is expanded only when the
/// previous one has been exhausted, so a caller that stops at the first hit
/// never pays for the generation below it.
struct JobWalk<F> {
    frontier: Vec<i32>,
    next: usize,
    depth: u32,
    yielded: usize,
    /// Set by every route out, and checked before anything else.
    ///
    /// **An explicit flag rather than a state the three exits happen to leave
    /// behind.** Two of them did leave a self-consistent state and one did not:
    /// the generation that finds no children has already emptied `frontier`,
    /// and returning from there left the cursor pointing past the end of an
    /// empty vec, so the NEXT poll walked straight past the "is this
    /// generation finished" test and indexed it. `collect()` and a `for` loop
    /// both stop at the first `None` and never saw it; a fused iterator is
    /// contractually pollable for ever, and this one runs against panes that
    /// are closing underneath it.
    done: bool,
    children: F,
}

impl<F> JobWalk<F> {
    /// End the walk for good, and say so.
    fn finish(&mut self) -> Option<i32> {
        self.done = true;
        None
    }
}

/// Declared, not merely true: `done` is what makes it hold, and a caller is
/// entitled to rely on it.
impl<F: FnMut(i32) -> Vec<i32>> std::iter::FusedIterator for JobWalk<F> {}

impl<F: FnMut(i32) -> Vec<i32>> Iterator for JobWalk<F> {
    type Item = i32;

    fn next(&mut self) -> Option<i32> {
        if self.done {
            return None;
        }
        if self.yielded >= MAX_CANDIDATES {
            return self.finish();
        }
        if self.next == self.frontier.len() {
            if self.depth >= MAX_DEPTH {
                return self.finish();
            }
            let parents = std::mem::take(&mut self.frontier);
            let mut deeper = Vec::new();
            for parent in parents {
                deeper.extend((self.children)(parent));
                if deeper.len() >= MAX_CANDIDATES {
                    break;
                }
            }
            if deeper.is_empty() {
                return self.finish();
            }
            self.frontier = deeper;
            self.next = 0;
            self.depth += 1;
        }
        let pid = self.frontier[self.next];
        self.next += 1;
        self.yielded += 1;
        Some(pid)
    }
}

/// The first candidate that is a configured agent, with the pid it was found
/// at and the names it went by.
///
/// The pid comes back because the CALLER has to know whether the leader or
/// something under it matched -- that is the whole of the "why is my wrapper
/// listed" diagnostic. The names come back so the log can say what the matched
/// process called itself without reading it a second time.
///
/// `candidates` is consumed lazily and abandoned at the first hit, which is
/// what keeps a pane whose leader matches from costing a walk at all.
fn first_match<I>(rules: &AgentRules, candidates: I) -> Option<(i32, ProcessNames, &str)>
where
    I: IntoIterator<Item = (i32, ProcessNames)>,
{
    for (pid, names) in candidates {
        if let Some(command) = rules.match_command(&names.name, names.argv0.as_deref()) {
            return Some((pid, names, command));
        }
    }
    None
}

/// What a sample concluded about one pane's foreground job.
#[derive(Clone, Copy)]
enum Detected<'a> {
    /// The foreground leader is itself a configured agent.
    Leader(&'a str),
    /// Something the leader started is.
    Descendant {
        pid: i32,
        names: &'a ProcessNames,
        command: &'a str,
    },
    /// Nothing in the job is.
    Nothing,
}

/// Say what this pane's foreground job was called and what it was taken for --
/// ONCE per distinct answer.
///
/// **The dedupe is what makes this loggable at all.** There is no level quiet
/// enough to hide behind: `main.rs` pins the logger at `Debug` and never reads
/// `RUST_LOG`, so `trace!` would be invisible to a user and `debug!` would fire
/// for every non-agent pane on every sample -- tens of lines a second into a log
/// that is not rotated. One line per newly-seen foreground process is what
/// somebody debugging "the panel is empty" actually wants, and it is the only
/// thing that can answer "so what DID this platform call it".
///
/// A pane matched on the LEADER's own name is the ordinary case and says
/// nothing.
///
/// A descendant that did NOT match is never logged either, and that is the
/// difference between a diagnostic and a firehose: a pane running a build
/// cycles through dozens of short-lived pids, each of which would be a new key
/// and would churn the cache below to uselessness. Only the pane's ANSWER is
/// logged, once.
///
/// The cache is CLEARED rather than evicted when it fills. It exists to suppress
/// repeats, not to remember, so the worst a clear costs is one repeated line --
/// against an eviction policy whose bound would have to be argued for.
fn log_detection(pid: i32, names: &ProcessNames, outcome: Detected<'_>) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    /// One logged answer: the leader's pid and both its names, the pid of the
    /// descendant that rescued it if any, and what the job was taken for.
    type Answer = (i32, String, Option<String>, Option<i32>, Option<String>);
    /// Distinct answers remembered before starting over.
    const CAP: usize = 256;
    static SEEN: OnceLock<Mutex<HashSet<Answer>>> = OnceLock::new();

    let (child, command) = match outcome {
        Detected::Leader(command) => (None, Some(command)),
        Detected::Descendant { pid, command, .. } => (Some(pid), Some(command)),
        Detected::Nothing => (None, None),
    };
    if child.is_none() && command == Some(names.name.as_str()) {
        return;
    }
    let key = (
        pid,
        names.name.clone(),
        names.argv0.clone(),
        child,
        command.map(|c| c.to_string()),
    );
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.len() >= CAP {
        seen.clear();
    }
    if !seen.insert(key) {
        return;
    }
    drop(seen);
    match outcome {
        Detected::Leader(command) => log::debug!(
            "agents: pid={pid} is {command:?}, matched on argv[0] {:?} -- \
             this platform names the process {:?}",
            names.argv0,
            names.name
        ),
        Detected::Descendant {
            pid: child_pid,
            names: child,
            command,
        } => log::debug!(
            "agents: pid={pid} (name={:?} argv0={:?}) is a launcher for {command:?}, \
             found at pid={child_pid} (name={:?} argv0={:?})",
            names.name,
            names.argv0,
            child.name,
            child.argv0
        ),
        Detected::Nothing => log::debug!(
            "agents: pid={pid} is not a configured agent, and nor is anything in its job: \
             name={:?} argv0={:?}",
            names.name,
            names.argv0
        ),
    }
}

/// The name part of an `argv[0]`.
///
/// Verbatim apart from the path: a login shell's `-zsh` stays `-zsh` and does
/// NOT match a configured `zsh`, because the dash is how the OS distinguishes
/// the two and inventing an equivalence here would be a new false positive in
/// the name of fixing a false negative.
fn argv0_name(argv0: &str) -> Option<&str> {
    let name = argv0.rsplit('/').next().unwrap_or(argv0);
    (!name.is_empty()).then_some(name)
}

/// A configured pattern, compiled.
#[derive(Debug)]
struct CompiledPattern {
    name: String,
    command: Option<String>,
    re: Regex,
}

/// Why a pane was classified as it was.
///
/// Carried so the decision can be logged. A heuristic classifier that cannot say
/// why it decided something is very hard to tune, and this one will misclassify
/// eventually -- the first question will be "why does it say idle", and without
/// this the honest answer would be "read the source and guess".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub state: AgentState,
    /// The one-line explanation, for the log. Carries SAMPLE-SPECIFIC detail --
    /// the elapsed milliseconds -- so it is not what a caller may dedupe on.
    pub why: String,
    /// The same reason with that detail removed, so two consecutive samples of
    /// an unchanged pane compare EQUAL. See [`Reason`].
    pub reason: Reason,
}

/// Why a pane is in the state it is in, stripped of anything that changes from
/// one sample to the next.
///
/// This exists because [`Verdict::why`] cannot serve as a dedup key and quietly
/// failed to for a whole phase. The caller logged the classification only when
/// `why` changed -- which reads like a transition check and is not one, because
/// `why` embeds the elapsed milliseconds. Every sample produced a new string, so
/// every sample logged, at up to ten lines a second per agent pane, for ever:
/// one user's `server.log` reached **586 MB in about a day**. Both the `Working`
/// and the `Idle` wordings carry that number, so it was not one state
/// misbehaving.
///
/// The pattern NAME is part of the key on purpose. Two different patterns both
/// mean `NeedsInput`, and a pane moving from one to the other is a change of
/// REASON worth a line even though the state did not move -- which is exactly
/// what the log is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A configured pattern matched; carries its `name`.
    Pattern(String),
    /// Output arrived inside the working window.
    RecentOutput,
    /// Neither: nothing matched and nothing arrived.
    Silent,
}

/// The agent commands and blocked-prompt patterns, ready to match.
#[derive(Debug)]
pub struct AgentRules {
    commands: Vec<String>,
    patterns: Vec<CompiledPattern>,
    working: Duration,
    scan_rows: usize,
}

impl AgentRules {
    /// Compile `cfg`. A pattern whose regex is invalid is logged and dropped --
    /// a typo in one rule must not cost the user the other rules, and must
    /// certainly not take the server down.
    pub fn from_config(cfg: &AgentsConfig) -> Self {
        let mut patterns = Vec::new();
        for p in &cfg.pattern {
            match Regex::new(&p.regex) {
                Ok(re) => patterns.push(CompiledPattern {
                    name: p.name.clone(),
                    command: p.command.clone(),
                    re,
                }),
                Err(e) => log::warn!(
                    "agents: pattern {:?} has an invalid regex ({e}); skipping it",
                    p.name
                ),
            }
        }
        log::info!(
            "agents: watching for {:?} with {} pattern(s), working window {}ms, scanning {} rows",
            cfg.commands,
            patterns.len(),
            cfg.working_ms,
            cfg.scan_rows
        );
        Self {
            commands: cfg.commands.clone(),
            patterns,
            working: Duration::from_millis(cfg.working_ms),
            scan_rows: cfg.scan_rows as usize,
        }
    }

    /// The configured command this process is running, if any.
    ///
    /// `name` is what the OS calls the process and `argv0` is its `argv[0]`, and
    /// the two disagree for real programs --
    /// [`crate::server::daemon::ProcessNames`] has the case that
    /// prompted this. Both are candidates; the first that IS a configured
    /// command wins.
    ///
    /// **`name` is tried first, and the order is load-bearing rather than
    /// arbitrary.** It is the answer Linux has always given and every existing
    /// harness pins, so trying it first leaves that path's outcome untouched and
    /// makes `argv0` purely additive -- a rescue for the panes the old rule
    /// missed, never a re-decision of one it already got right.
    ///
    /// The return is borrowed from `commands`, so a caller cannot accidentally
    /// report the raw candidate: what comes back is the CONFIGURED spelling.
    pub fn match_command(&self, name: &str, argv0: Option<&str>) -> Option<&str> {
        if let Some(c) = self.commands.iter().find(|c| c.as_str() == name) {
            return Some(c.as_str());
        }
        let base = argv0.and_then(argv0_name)?;
        self.commands
            .iter()
            .find(|c| c.as_str() == base)
            .map(|c| c.as_str())
    }

    /// Whether anything is configured at all. With no commands there is nothing
    /// to detect, and the sampler can skip its work entirely.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Classify one agent pane. See the module docs for the precedence.
    ///
    /// `bottom` is the visible bottom of the pane's live screen, oldest first;
    /// `since_output` is how long ago bytes last reached it.
    pub fn classify(&self, command: &str, bottom: &[String], since_output: Duration) -> Verdict {
        for p in &self.patterns {
            if let Some(want) = &p.command {
                if want != command {
                    continue;
                }
            }
            if bottom.iter().any(|line| p.re.is_match(line)) {
                return Verdict {
                    state: AgentState::NeedsInput,
                    why: format!("matched pattern {:?}", p.name),
                    reason: Reason::Pattern(p.name.clone()),
                };
            }
        }
        if since_output <= self.working {
            return Verdict {
                state: AgentState::Working,
                why: format!("output {}ms ago", since_output.as_millis()),
                reason: Reason::RecentOutput,
            };
        }
        Verdict {
            state: AgentState::Idle,
            why: format!(
                "no pattern matched; silent for {}ms",
                since_output.as_millis()
            ),
            reason: Reason::Silent,
        }
    }

    /// The lines [`AgentRules::classify`] should be given for `screen`: the last
    /// `scan_rows` LOGICAL lines of the LIVE grid that have anything on them.
    ///
    /// The live grid, never the scrollback -- an approval prompt the user has
    /// scrolled past is not what the agent is showing now. It is also what makes
    /// the foreground tab work: this reads the pane's own screen, so it is
    /// entirely independent of `record_pane_activity`, which returns early for
    /// the tab being viewed and would have made the classifier blind to exactly
    /// the pane the user is looking at.
    ///
    /// Two things are done to the raw grid first, and both were found by
    /// probing rather than reasoned out in advance:
    ///
    /// * **Soft-wrapped rows are joined into one line.** A pattern is matched
    ///   against a line, and in a narrow pane `Do you want to proceed?` is two
    ///   grid rows -- `Do you want to pro` and `ceed?` -- which no sensible
    ///   pattern matches. Splitting one pane three ways was enough to hide a
    ///   prompt that was plainly on screen. `Row::wrapped` is exactly the
    ///   "this line continues" flag needed, so this costs nothing and makes
    ///   matching independent of the pane's width.
    /// * **Trailing BLANK rows are skipped** before the window is taken. The
    ///   bottom of the grid is not the bottom of the output: an agent that has
    ///   printed five lines into a thirty-row pane leaves the last twelve rows
    ///   empty, so a window anchored to the grid's last row would scan twelve
    ///   blank lines and see nothing. A full-screen TUI, where the grid really
    ///   is full, is unaffected by either.
    ///
    /// **`scan_rows` is a COST bound as well as a correctness one, and widening
    /// it is not a free "improvement".** Every configured pattern is run against
    /// every returned line, for every agent pane, on every sample -- up to ten
    /// samples a second. Whoever wants a bigger window should raise `scan_rows`
    /// in their own config, which is exactly why it is configurable; do not
    /// widen the default, and do not turn this into a whole-screen scan.
    ///
    /// Note what the number actually bounds: **logical LINES, not rows of
    /// text.** Soft-wrapped rows are joined first, so on a narrow pane twelve
    /// lines can be many more than twelve rows, and each line can be several
    /// hundred characters. Twelve lines of a 200-column pane is a haystack of a
    /// couple of thousand characters per pattern per sample -- still small, and
    /// still the quantity that grows if this is widened.
    ///
    /// (The line-BUILDING pass below does walk the whole grid, because a
    /// soft-wrapped line has to be assembled from its start. That is character
    /// copying, not regex; the bounded quantity is the matching.)
    pub fn visible_bottom(&self, screen: &Screen) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut continuing = false;
        for row in &screen.grid {
            if continuing {
                current.push_str(&row_text(row));
            } else {
                current = row_text(row);
            }
            continuing = row.wrapped;
            if !continuing {
                // Trimmed HERE, once, and never per row: a wrapped row's
                // trailing space is content (see `row_text`).
                lines.push(trim_line_end(std::mem::take(&mut current)));
            }
        }
        if continuing {
            lines.push(trim_line_end(current));
        }
        let end = match lines.iter().rposition(|line| !line.is_empty()) {
            Some(last) => last + 1,
            // A blank screen: nothing to match, and no lines worth handing on.
            None => return Vec::new(),
        };
        let start = end.saturating_sub(self.scan_rows);
        lines[start..end].to_vec()
    }
}

/// The text of one screen row, VERBATIM -- trailing blanks included.
///
/// Trimming belongs to the logical LINE, not to a row, and doing it here was a
/// real bug: a space written in the last column fills that cell and sets
/// `pending_wrap` (`screen.rs`), so the row is flagged `wrapped` and its final
/// cell is a genuine space that is INTERIOR to the line. Stripping it rejoined
/// `Do you want to proceed?` as `Do you want toproceed?`, which no pattern
/// matches -- a visible blocked prompt reading `Idle`, at roughly one pane width
/// in ten (four of the prompt's own spaces, each with its own unlucky width).
/// [`AgentRules::visible_bottom`] trims once, after the line is assembled.
///
/// Continuation cells of a wide glyph (`width == 0` after a `width == 2` lead)
/// are skipped so a CJK character contributes one char and not a spurious space
/// in the middle of a word a pattern is trying to match.
fn row_text(row: &crate::screen::Row) -> String {
    let mut s = String::new();
    for cell in &row.cells {
        if cell.width == 0 {
            continue;
        }
        s.push(cell.c);
        for m in &cell.combining {
            s.push(*m);
        }
    }
    s
}

/// Drop trailing spaces from a finished logical line.
fn trim_line_end(mut s: String) -> String {
    while s.ends_with(' ') {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::agents::AgentPattern;

    fn rules(patterns: Vec<AgentPattern>, working_ms: u64) -> AgentRules {
        AgentRules::from_config(&AgentsConfig {
            commands: vec!["claude".to_string(), "codex".to_string()],
            working_ms,
            scan_rows: 12,
            pattern: patterns,
        })
    }

    fn pattern(name: &str, command: Option<&str>, regex: &str) -> AgentPattern {
        AgentPattern {
            name: name.to_string(),
            command: command.map(|c| c.to_string()),
            regex: regex.to_string(),
        }
    }

    fn shipped() -> AgentRules {
        AgentRules::from_config(&AgentsConfig::default())
    }

    /// The approval questions present in the installed Claude Code 2.1.251
    /// binary, read with `strings`. The shipped patterns exist to match these.
    const CLAUDE_QUESTIONS: [&str; 5] = [
        "Do you want to proceed?",
        "Do you want to continue?",
        "Do you want to allow this connection?",
        "Do you want to allow Claude to fetch this content?",
        "Do you want to use this API key?",
    ];

    #[test]
    fn only_configured_commands_are_agents() {
        let r = shipped();
        assert_eq!(r.match_command("claude", None), Some("claude"));
        assert_eq!(r.match_command("codex", None), Some("codex"));
        assert_eq!(r.match_command("zsh", None), None);
        assert_eq!(r.match_command("vim", None), None);
        // `get_process_name`'s unreadable-process fallback must not be an agent.
        assert_eq!(r.match_command("shell", None), None);
    }

    /// THE macOS BUG, as a unit test.
    ///
    /// Claude Code's installer execs `~/.local/share/claude/versions/<version>`
    /// with `argv[0] = "claude"`. Linux `comm` reports the title the binary sets
    /// (`claude`); macOS `sysinfo` reports the executable's basename
    /// (`2.1.251`), which matches nothing in `commands` -- an agents panel that
    /// is empty on a Mac while detection is running perfectly.
    #[test]
    fn a_process_named_after_its_version_is_still_matched_by_argv0() {
        let r = shipped();
        assert_eq!(r.match_command("2.1.251", Some("claude")), Some("claude"));
    }

    /// `argv[0]` is often a path, and only its last component is a name.
    #[test]
    fn an_argv0_that_is_a_path_matches_on_its_basename() {
        let r = shipped();
        assert_eq!(
            r.match_command("node", Some("/Users/rakan/.local/bin/claude")),
            Some("claude")
        );
        // ...and a path with nothing after the last slash names nothing.
        assert_eq!(r.match_command("node", Some("/usr/local/bin/")), None);
        assert_eq!(r.match_command("node", Some("")), None);
    }

    /// Each candidate must be able to match ON ITS OWN.
    ///
    /// Pinned separately because the fallback can MASK the primary: if `name`
    /// matching broke, every real agent would still be found through `argv[0]`
    /// and the end-to-end harnesses would stay green over a dead code path.
    #[test]
    fn each_candidate_matches_without_help_from_the_other() {
        let r = shipped();
        // Name alone, with argv[0] absent and then actively wrong.
        assert_eq!(r.match_command("claude", None), Some("claude"));
        assert_eq!(r.match_command("claude", Some("/bin/zsh")), Some("claude"));
        // argv[0] alone, with the name actively wrong.
        assert_eq!(r.match_command("bun", Some("codex")), Some("codex"));
    }

    /// The NAME is tried first. A pane whose name already matches is decided by
    /// the rule that has always decided it, whatever its `argv[0]` says.
    #[test]
    fn the_process_name_outranks_argv0() {
        let r = AgentRules::from_config(&AgentsConfig {
            commands: vec!["claude".to_string(), "codex".to_string()],
            working_ms: 500,
            scan_rows: 12,
            pattern: Vec::new(),
        });
        assert_eq!(r.match_command("claude", Some("codex")), Some("claude"));
    }

    #[test]
    fn a_pane_matching_on_neither_name_is_not_an_agent() {
        let r = shipped();
        assert_eq!(r.match_command("2.1.251", Some("/usr/bin/vim")), None);
        assert_eq!(r.match_command("zsh", Some("-zsh")), None);
    }

    /// A login shell's leading dash is part of its `argv[0]`, and is left there.
    /// Stripping it would make `-zsh` match a configured `zsh` -- a new false
    /// positive introduced while fixing a false negative.
    #[test]
    fn a_login_shells_leading_dash_is_not_stripped() {
        let r = AgentRules::from_config(&AgentsConfig {
            commands: vec!["zsh".to_string()],
            working_ms: 500,
            scan_rows: 12,
            pattern: Vec::new(),
        });
        assert_eq!(r.match_command("bash", Some("-zsh")), None);
        assert_eq!(r.match_command("bash", Some("/bin/zsh")), Some("zsh"));
    }

    /// What comes back is the CONFIGURED spelling, which is what
    /// `AgentEntry::command` promises and what `classify` scopes patterns
    /// against -- so a pattern with `command = "claude"` still fires for a pane
    /// that was only recognisable through its `argv[0]`.
    #[test]
    fn a_pane_matched_by_argv0_is_still_scoped_to_its_configured_patterns() {
        let r = rules(vec![pattern("approval", Some("claude"), r"proceed\?")], 500);
        let command = r
            .match_command("2.1.251", Some("/opt/claude"))
            .expect("argv[0] names a configured command");
        assert_eq!(command, "claude");
        assert_eq!(
            r.classify(command, &["proceed?".to_string()], Duration::from_secs(60))
                .state,
            AgentState::NeedsInput
        );
    }

    // -- the other shipped agents, from captured screens --------------------

    const OMP_BLOCKED: [&str; 11] = [
        "╭─ Allow tool: bash ──────╮",
        "│                                                                                                                                          │",
        "│ Command: touch /tmp/omp_probe_z                                                                                                          │",
        "│                                                                                                                                          │",
        "│   Approve                                                                                                                               │",
        "│    Deny                                                                                                                                  │",
        "│                                                                                                                                          │",
        "│ up/down navigate  enter select  esc cancel                                                                                               │",
        "│                                                                                                                                          │",
        "╰──────╯",
        " ⠴ 47s · 󰪣 qwen3.8:27b ·  ~/Work/Personal/Remux ·  master *6 ·  11.9%/262K 󰁨",
    ];

    const OMP_ANSWERED: [&str; 10] = [
        " Tip: Say `orchestrate` in your message to drive a multi-phase task with parallel subagents — watch",
        "      it glow as you type",
        "──────",
        " Update Available",
        " New version 18.2.9 is available. Run: omp update",
        "──────",
        " Run this exact shell command and nothing else, do not explain: touch /tmp/omp_probe_z",
        "╭──────╮",
        "│ $ touch /tmp/omp_probe_z                                                                                                                 │",
        "├─── Output ──────┤",
    ];

    const OPENCODE_BLOCKED: [&str; 11] = [
        "  ┃",
        "  ┃  △ Permission required                                                                              Connect from 75+ providers to",
        "  ┃    ← Access external directory /tmp                                                                 use other models, including",
        "  ┃                                                                                                     Claude, GPT, Gemini etc",
        "  ┃  Patterns",
        "  ┃                                                                                                     Connect provider        /connect",
        "  ┃  - /tmp/*",
        "  ┃",
        "  ┃                                                                                                 ~/Work/Personal/Remux:master",
        "  ┃   Allow once   Allow always   Reject           ctrl+f fullscreen  ⇆ select  enter confirm",
        "  ┃                                                                                                 • OpenCode 1.18.32",
    ];

    const OPENCODE_ANSWERED: [&str; 10] = [
        "  ┃",
        "  ┃  (no output)                                                                                    LSP",
        "  ┃                                                                                                 LSPs are disabled",
        "     Done.",
        "     ▣  Build · Big Pickle · 9.4s",
        "                                                                                                      ⬖ Getting started                ✕",
        "                                                                                                        OpenCode includes free models",
        "                                                                                                        so you can start immediately.",
        "                                                                                                        Connect from 75+ providers to",
        "                                                                                                        use other models, including",
    ];

    /// The four approval dialog titles present in the installed codex 0.156.0
    /// native binary, read with `strings`, plus the decline option its modal
    /// draws.
    ///
    /// **These are BINARY strings, not a captured screen, and the difference
    /// matters.** codex could not be driven to a real approval on either
    /// machine available: this Linux box is refused by the API for every model
    /// (`The 'gpt-5-codex' model is not supported when using Codex with a
    /// ChatGPT account.`) and the Mac hangs. So unlike `omp` and `opencode`
    /// below, nobody has watched one of these prompts appear OR clear. See
    /// `the_shipped_codex_patterns_are_binary_strings_never_a_watched_prompt`.
    ///
    /// Two further strings were extracted and deliberately NOT made patterns:
    /// `" needs your approval."` and ``"Yes, and don't ask again for commands
    /// that start with `"``. The first is ordinary enough to appear in an
    /// agent's own prose, and a false `NeedsInput` never decays; the second is
    /// an option of the same modal the titles already catch.
    const CODEX_APPROVALS: [&str; 5] = [
        "Would you like to run the following command?",
        "Would you like to make the following edits?",
        "Would you like to grant these permissions?",
        "Would you like to send input to terminal",
        "No, and tell Codex what to do differently",
    ];

    /// omp's approval box is a blocked agent, with zero configuration.
    ///
    /// Captured live from omp 18.1.19. The pattern keys on the box TITLE, which
    /// names the tool being approved (`bash`, `edit`, `write`), so one rule
    /// covers every tool kind rather than one per kind.
    #[test]
    fn the_shipped_patterns_recognise_omps_real_approval_box() {
        let r = shipped();
        let screen: Vec<String> = OMP_BLOCKED.iter().map(|s| s.to_string()).collect();
        let v = r.classify("omp", &screen, Duration::from_secs(30));
        assert_eq!(v.state, AgentState::NeedsInput, "{v:?}");
        assert_eq!(v.reason, Reason::Pattern("omp-allow".to_string()));
    }

    /// ...and the ANSWERED screen is not. MEASURED, not argued: the same pane
    /// was re-snapshotted at +4s, +8s, +12s and +16s after Enter on `Approve`,
    /// and the whole box is gone from every one of them. This is what stops the
    /// pattern pinning a pane red for the rest of the session.
    #[test]
    fn the_answered_omp_box_is_not_a_blocked_signal() {
        let r = shipped();
        let screen: Vec<String> = OMP_ANSWERED.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            r.classify("omp", &screen, Duration::from_secs(30)).state,
            AgentState::Idle
        );
    }

    /// opencode's permission prompt, captured live from opencode 1.18.32.
    ///
    /// Two patterns, and each is asserted ON ITS OWN below, because either can
    /// scroll out of the window without the other: the header sits several
    /// lines above the options row.
    #[test]
    fn the_shipped_patterns_recognise_opencodes_real_permission_prompt() {
        let r = shipped();
        let screen: Vec<String> = OPENCODE_BLOCKED.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            r.classify("opencode", &screen, Duration::from_secs(30))
                .state,
            AgentState::NeedsInput
        );
        for (line, want) in [
            (OPENCODE_BLOCKED[1], "opencode-permission"),
            (OPENCODE_BLOCKED[9], "opencode-allow"),
        ] {
            let v = r.classify("opencode", &[line.to_string()], Duration::from_secs(30));
            assert_eq!(v.state, AgentState::NeedsInput, "{line:?}");
            assert_eq!(v.reason, Reason::Pattern(want.to_string()), "{line:?}");
        }
        // The options row COLLAPSED, which is what buys `\s+` in that pattern.
        //
        // The width sweep does NOT cover this and cannot: rewrapping a line
        // changes where the rows break, never the characters, so
        // `visible_bottom` hands the pattern back the original run of spaces at
        // every width and a literal-space pattern sweeps green. The run between
        // the options is LAYOUT rather than content, and only a differently
        // spaced line can tell the two pattern styles apart. (One capture at
        // one width cannot show what opencode does to that gap; the pattern
        // simply has no reason to depend on it.) Verified by mutation: a
        // literal-space `opencode-allow` fails here and nowhere else in the
        // suite.
        let collapsed = "  \u{2503}  Allow once Allow always Reject  enter confirm";
        assert_eq!(
            r.classify(
                "opencode",
                &[collapsed.to_string()],
                Duration::from_secs(30)
            )
            .state,
            AgentState::NeedsInput,
            "the options row must match however the agent spaced it"
        );
    }

    /// ...and the ANSWERED screen is not. MEASURED the same way: re-snapshotted
    /// at +4s through +16s, and both the header and the options row are gone.
    #[test]
    fn the_answered_opencode_permission_is_not_a_blocked_signal() {
        let r = shipped();
        let screen: Vec<String> = OPENCODE_ANSWERED.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            r.classify("opencode", &screen, Duration::from_secs(30))
                .state,
            AgentState::Idle
        );
    }

    /// The codex patterns, against the strings in the shipped binary.
    ///
    /// **There is no answered-screen counterpart to this test, and there is not
    /// supposed to be.** Every other agent here has one because its prompt was
    /// watched appearing and then clearing. codex could not be driven to an
    /// approval at all (see [`CODEX_APPROVALS`]), so that these strings
    /// DISAPPEAR once answered is INFERRED from how the other agents behave,
    /// not observed. If codex ever becomes runnable here, the missing test is
    /// the answered screen -- and if it turns out a title lingers, these
    /// patterns pin the pane red for the session and must be narrowed.
    #[test]
    fn the_shipped_codex_patterns_are_binary_strings_never_a_watched_prompt() {
        let r = shipped();
        for line in CODEX_APPROVALS {
            let v = r.classify("codex", &[line.to_string()], Duration::from_secs(30));
            assert_eq!(v.state, AgentState::NeedsInput, "{line:?} -> {v:?}");
        }
    }

    /// The old `codex-yn` pattern (`\[y(es)?/n(o)?\]`) is GONE, and this pins
    /// it out.
    ///
    /// codex prints no such prompt -- nothing of the shape is in its binary --
    /// so it could only ever have fired on something else's output passing
    /// through the pane. That is not a harmless false positive: `NeedsInput`
    /// never decays on silence, so one `[y/n]` from a script, a diff or a test
    /// log pinned that pane red for the rest of the session.
    #[test]
    fn a_yes_no_prompt_no_longer_pins_a_codex_pane_red() {
        let r = shipped();
        for line in ["Continue? [y/N]", "overwrite? [yes/no]"] {
            assert_eq!(
                r.classify("codex", &[line.to_string()], Duration::from_secs(30))
                    .state,
                AgentState::Idle,
                "{line:?}"
            );
        }
    }

    /// One agent's prompt must never decide another agent's pane. The patterns
    /// are command-scoped, and these two look nothing alike, so a cross hit
    /// would mean a pattern had been shipped unscoped.
    #[test]
    fn one_agents_prompt_does_not_decide_another_agents_pane() {
        let r = shipped();
        let omp: Vec<String> = OMP_BLOCKED.iter().map(|s| s.to_string()).collect();
        let oc: Vec<String> = OPENCODE_BLOCKED.iter().map(|s| s.to_string()).collect();
        for command in ["opencode", "claude", "codex"] {
            assert_eq!(
                r.classify(command, &omp, Duration::from_secs(30)).state,
                AgentState::Idle,
                "omp's box decided a {command} pane"
            );
        }
        for command in ["omp", "claude", "codex"] {
            assert_eq!(
                r.classify(command, &oc, Duration::from_secs(30)).state,
                AgentState::Idle,
                "opencode's prompt decided a {command} pane"
            );
        }
    }

    #[test]
    fn recent_output_reads_as_working() {
        let r = rules(Vec::new(), 500);
        let v = r.classify(
            "claude",
            &["thinking...".to_string()],
            Duration::from_millis(100),
        );
        assert_eq!(v.state, AgentState::Working);
    }

    #[test]
    fn silence_past_the_window_decays_working_to_idle() {
        let r = rules(Vec::new(), 500);
        let v = r.classify("claude", &["done".to_string()], Duration::from_millis(900));
        assert_eq!(v.state, AgentState::Idle);
    }

    /// The §11 bug, as a unit test: the state that must NOT decay.
    #[test]
    fn needs_input_survives_silence_far_beyond_the_working_window() {
        let r = rules(
            vec![pattern(
                "approval",
                Some("claude"),
                r"Do you want to proceed",
            )],
            500,
        );
        let screen = vec![
            "  Edit file.rs".to_string(),
            "Do you want to proceed?".to_string(),
            "> 1. Yes".to_string(),
        ];
        // A blocked agent produces no output PRECISELY BECAUSE it is waiting.
        for silent_ms in [0, 600, 5_000, 3_600_000] {
            let v = r.classify("claude", &screen, Duration::from_millis(silent_ms));
            assert_eq!(
                v.state,
                AgentState::NeedsInput,
                "silence of {silent_ms}ms must not clear NeedsInput"
            );
        }
    }

    #[test]
    fn needs_input_outranks_working() {
        let r = rules(
            vec![pattern(
                "approval",
                Some("claude"),
                r"Do you want to proceed",
            )],
            500,
        );
        // A spinner under an approval prompt is still blocked.
        let v = r.classify(
            "claude",
            &[
                "Do you want to proceed?".to_string(),
                "* thinking".to_string(),
            ],
            Duration::from_millis(1),
        );
        assert_eq!(v.state, AgentState::NeedsInput);
    }

    #[test]
    fn resumed_output_clears_needs_input_because_the_prompt_is_gone() {
        let r = rules(
            vec![pattern(
                "approval",
                Some("claude"),
                r"Do you want to proceed",
            )],
            500,
        );
        let after = vec!["Editing file.rs".to_string()];
        assert_eq!(
            r.classify("claude", &after, Duration::from_millis(10))
                .state,
            AgentState::Working
        );
    }

    #[test]
    fn a_pattern_is_scoped_to_its_command() {
        let r = rules(vec![pattern("approval", Some("claude"), r"proceed\?")], 500);
        let screen = vec!["proceed?".to_string()];
        assert_eq!(
            r.classify("claude", &screen, Duration::from_secs(60)).state,
            AgentState::NeedsInput
        );
        assert_eq!(
            r.classify("codex", &screen, Duration::from_secs(60)).state,
            AgentState::Idle,
            "another agent's pattern must not decide this one"
        );
    }

    #[test]
    fn a_pattern_without_a_command_applies_to_every_agent() {
        let r = rules(vec![pattern("any", None, r"waiting for you")], 500);
        for cmd in ["claude", "codex"] {
            assert_eq!(
                r.classify(
                    cmd,
                    &["waiting for you".to_string()],
                    Duration::from_secs(60)
                )
                .state,
                AgentState::NeedsInput
            );
        }
    }

    #[test]
    fn an_invalid_regex_is_skipped_and_the_others_still_work() {
        let r = rules(
            vec![
                pattern("broken", None, r"("),
                pattern("good", None, r"blocked here"),
            ],
            500,
        );
        assert_eq!(r.patterns.len(), 1, "the broken one was dropped, not fatal");
        assert_eq!(
            r.classify(
                "claude",
                &["blocked here".to_string()],
                Duration::from_secs(60)
            )
            .state,
            AgentState::NeedsInput
        );
    }

    #[test]
    fn the_verdict_names_the_rule_that_fired() {
        let r = rules(vec![pattern("claude-proceed", None, r"proceed")], 500);
        let v = r.classify("claude", &["proceed?".to_string()], Duration::from_secs(9));
        assert!(
            v.why.contains("claude-proceed"),
            "the explain path must name the pattern, got {:?}",
            v.why
        );
        let v = r.classify("claude", &["nothing".to_string()], Duration::from_secs(9));
        assert!(
            v.why.contains("no pattern matched"),
            "and must say when none did, got {:?}",
            v.why
        );
    }

    /// The shipped patterns against the prompt wordings actually present in the
    /// installed Claude Code binary (2.1.251, read with `strings`).
    #[test]
    fn the_shipped_patterns_recognise_claudes_real_permission_prompts() {
        let r = shipped();
        for question in CLAUDE_QUESTIONS {
            let screen = vec![question.to_string(), "❯ 1. Yes".to_string()];
            assert_eq!(
                r.classify("claude", &screen, Duration::from_secs(30)).state,
                AgentState::NeedsInput,
                "the panel has to work with zero configuration: {question:?}"
            );
        }
    }

    /// The real question menu, glyph for glyph, from a pane the user reported
    /// as showing grey while it was blocked.
    ///
    /// The footer is the last line, and it is what the shipped pattern matches;
    /// the `\u{276f} 1.` rows deliberately do NOT match anything (see
    /// `a_bare_menu_row_is_not_a_shipped_blocked_signal`), so a green result
    /// here can only have come from the footer.
    const CLAUDE_QUESTION_MENU: [&str; 8] = [
        "Which color do you like?",
        "",
        "\u{276f} 1. Blue",
        "  2. Red",
        "  3. Yellow",
        "  4. Type something.",
        "  5. Chat about this",
        "Enter to select \u{b7} \u{2191}/\u{2193} to navigate \u{b7} Esc to cancel",
    ];

    /// What the SAME pane showed once the question had been answered: the whole
    /// block collapsed, and no footer anywhere.
    const CLAUDE_ANSWERED: [&str; 2] = [
        "\u{23fa} User answered Claude's questions:",
        "  \u{23bf}  \u{b7} Which color do you like? \u{2192} Blue",
    ];

    /// Claude Code's QUESTION menu is a blocked agent, with zero configuration.
    ///
    /// It is its `AskUserQuestion` UI, which asks something rather than asking
    /// permission, so it matches none of the approval wordings above -- and a
    /// user's blocked pane read `Idle` for exactly that reason.
    #[test]
    fn the_question_menu_is_a_shipped_blocked_signal() {
        let r = shipped();
        let screen: Vec<String> = CLAUDE_QUESTION_MENU.iter().map(|s| s.to_string()).collect();
        let v = r.classify("claude", &screen, Duration::from_secs(30));
        assert_eq!(v.state, AgentState::NeedsInput, "{v:?}");
        assert_eq!(v.reason, Reason::Pattern("claude-select".to_string()));
    }

    /// **The staleness objection, tested rather than argued.**
    ///
    /// The reason no menu-ROW pattern ships is that an answered menu left on
    /// screen cannot be told from a waiting one, so the panel would stay red for
    /// ever. The footer does not have that problem, and this is the evidence:
    /// once the question is answered the block collapses to two lines with the
    /// footer nowhere in them, so nothing keeps matching.
    ///
    /// If a future change makes this fail, the shipped `claude-select` pattern
    /// has become exactly the thing its own doc comment says it is not.
    #[test]
    fn the_answered_question_menu_is_not_a_blocked_signal() {
        let r = shipped();
        let screen: Vec<String> = CLAUDE_ANSWERED.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            r.classify("claude", &screen, Duration::from_secs(30)).state,
            AgentState::Idle
        );
    }

    /// Half the footer is not the footer. Both ends are required, which is what
    /// keeps the pattern from firing on ordinary agent prose -- the same
    /// crying-wolf argument that stops `claude-proceed` matching the bare
    /// "Do you want to " prefix.
    #[test]
    fn half_the_selector_footer_does_not_match() {
        let r = shipped();
        for half in ["press Enter to select a file", "Esc to cancel"] {
            assert_eq!(
                r.classify("claude", &[half.to_string()], Duration::from_secs(30))
                    .state,
                AgentState::Idle,
                "{half:?}"
            );
        }
    }

    /// A menu row on its own is NOT a shipped blocked signal. See
    /// `config::agents::default_patterns`: an answered menu left on screen is
    /// indistinguishable from a waiting one, and a panel stuck red for ever is
    /// worse than one that is slow.
    #[test]
    fn a_bare_menu_row_is_not_a_shipped_blocked_signal() {
        let r = shipped();
        let screen = vec!["❯ 2. Yes, and don't ask again".to_string()];
        assert_eq!(
            r.classify("claude", &screen, Duration::from_secs(30)).state,
            AgentState::Idle
        );
    }

    /// ...and a user who wants it can still have it, in one config entry.
    #[test]
    fn a_user_can_add_a_menu_pattern_back() {
        let r = rules(vec![pattern("choice", Some("claude"), r"❯\s*\d+\.")], 500);
        let screen = vec!["❯ 2. Yes, and don't ask again".to_string()];
        assert_eq!(
            r.classify("claude", &screen, Duration::from_secs(30)).state,
            AgentState::NeedsInput
        );
    }

    #[test]
    fn ordinary_agent_output_is_not_mistaken_for_a_prompt() {
        let r = shipped();
        let screen = vec![
            "● Read(src/main.rs)".to_string(),
            "  Read 120 lines".to_string(),
            "· Thinking… (12s)".to_string(),
        ];
        assert_eq!(
            r.classify("claude", &screen, Duration::from_secs(30)).state,
            AgentState::Idle,
            "a false NeedsInput is worse than a missed one: it cries wolf"
        );
    }

    #[test]
    fn the_scan_window_is_the_bottom_of_the_live_screen() {
        let r = AgentRules::from_config(&AgentsConfig {
            commands: vec!["claude".to_string()],
            working_ms: 500,
            scan_rows: 2,
            pattern: Vec::new(),
        });
        let mut screen = Screen::new(20, 4, 100);
        screen.process_output(b"top row\r\nsecond\r\nthird\r\nlast");
        let bottom = r.visible_bottom(&screen);
        assert_eq!(bottom.len(), 2, "only the configured number of rows");
        assert_eq!(bottom, vec!["third".to_string(), "last".to_string()]);
    }

    /// The bug a frame probe found: the bottom of the GRID is not the bottom of
    /// the OUTPUT.
    #[test]
    fn output_at_the_top_of_a_mostly_empty_screen_is_still_scanned() {
        let r = AgentRules::from_config(&AgentsConfig {
            commands: vec!["claude".to_string()],
            working_ms: 500,
            scan_rows: 4,
            pattern: vec![pattern("approval", None, r"Do you want to proceed")],
        });
        // Three lines of output at the top of a thirty-row pane: the prompt is
        // twenty-six rows above the grid's last row.
        let mut screen = Screen::new(40, 30, 100);
        screen.process_output(b"agent ready\r\nDo you want to proceed?\r\n> 1. Yes");
        let bottom = r.visible_bottom(&screen);
        assert!(
            bottom.iter().any(|l| l.contains("Do you want to proceed")),
            "the prompt must be in the scanned window, got {bottom:?}"
        );
        assert_eq!(
            r.classify("claude", &bottom, Duration::from_secs(60)).state,
            AgentState::NeedsInput
        );
    }

    /// The THIRD bug a probe found, and the one the test below could not catch.
    ///
    /// At 15 columns `Do you want to proceed?` breaks on the SPACE at index 14:
    /// the space fills the last cell, sets `pending_wrap`, and the row is
    /// flagged `wrapped` with a trailing space that is interior to the line.
    /// Trimming per row ate it and rejoined the line as `Do you want toproceed?`.
    ///
    /// The prompt has four spaces, so four widths per screen size are unlucky --
    /// about one pane width in ten, and MORE for a real agent's longer prompt,
    /// not fewer.
    #[test]
    fn a_prompt_that_wraps_on_a_space_keeps_the_space() {
        let r = AgentRules::from_config(&AgentsConfig {
            commands: vec!["claude".to_string()],
            working_ms: 500,
            scan_rows: 12,
            pattern: vec![pattern("approval", None, r"Do you want to proceed")],
        });
        let mut screen = Screen::new(15, 10, 100);
        screen.process_output(b"Do you want to proceed?");
        let bottom = r.visible_bottom(&screen);
        assert!(
            bottom.iter().any(|l| l.contains("Do you want to proceed")),
            "the wrap point's space is content, not padding; got {bottom:?}"
        );
        assert_eq!(
            r.classify("claude", &bottom, Duration::from_secs(60)).state,
            AgentState::NeedsInput
        );
    }

    /// Every width, for every question the real binary asks.
    ///
    /// One question was not enough, and the reason is arithmetic: a space lands
    /// in the last column only at widths that DIVIDE (space index + 1), so a
    /// single prompt exercises the bug at just a couple of the 53 widths -- for
    /// `Do you want to proceed?` exactly two, 12 and 15. Every question has its
    /// spaces elsewhere and so its own unlucky widths, which a one-string loop
    /// would never visit: across the five shipped questions the bug is reachable
    /// at 24 (question, width) pairs rather than 2, including widths 8, 13, 14,
    /// 19, 21, 24, 26, 28, 31, 37 and 42 that the single-question loop missed
    /// entirely.
    #[test]
    fn every_shipped_question_matches_at_every_pane_width() {
        let r = shipped();
        // Every agent's captured blocked line, not just claude's. A pattern
        // written against a wide capture and matched with literal runs of
        // SPACES passes at the width it was captured at and fails everywhere
        // else, because a narrow pane rewraps the row -- which is exactly why
        // `opencode-allow` joins its three options with `\s+`.
        let mut lines: Vec<(&str, &str)> =
            CLAUDE_QUESTIONS.iter().map(|q| ("claude", *q)).collect();
        lines.push(("omp", OMP_BLOCKED[0]));
        lines.push(("opencode", OPENCODE_BLOCKED[1]));
        lines.push(("opencode", OPENCODE_BLOCKED[9]));
        for line in CODEX_APPROVALS {
            lines.push(("codex", line));
        }
        for (command, question) in lines {
            for cols in 8..=60u16 {
                // 40 rows, not 12. `visible_bottom` reads the LIVE grid and
                // never the scrollback, and a 130-column capture rewrapped to
                // eight columns is seventeen rows -- so on a short screen the
                // head of the line scrolls into history and this test measures
                // scrollback rather than the rewrapping it is about. (A pattern
                // genuinely cannot match a prompt that has scrolled away; that
                // is the design, and it is not what is under test here.)
                let mut screen = Screen::new(cols, 40, 100);
                screen.process_output(question.as_bytes());
                assert_eq!(
                    r.classify(command, &r.visible_bottom(&screen), Duration::from_secs(60))
                        .state,
                    AgentState::NeedsInput,
                    "{question:?} read as not-blocked at {cols} columns: {:?}",
                    r.visible_bottom(&screen)
                );
            }
        }
    }

    /// A prompt that soft-wraps MID-WORD. Kept beside the space case above
    /// because it is a different alignment, and because on its own it claimed
    /// to cover "wrapping" while covering only the lucky half of it.
    #[test]
    fn a_prompt_wrapped_across_two_rows_is_matched_as_one_line() {
        let r = AgentRules::from_config(&AgentsConfig {
            commands: vec!["claude".to_string()],
            working_ms: 500,
            scan_rows: 12,
            pattern: vec![pattern("approval", None, r"Do you want to proceed")],
        });
        // 18 columns: the prompt does not fit on one row.
        let mut screen = Screen::new(18, 10, 100);
        screen.process_output(b"Do you want to proceed?");
        assert!(
            r.visible_bottom(&screen)
                .iter()
                .any(|l| l.contains("Do you want to proceed")),
            "soft-wrapped rows must rejoin, got {:?}",
            r.visible_bottom(&screen)
        );
        assert_eq!(
            r.classify(
                "claude",
                &r.visible_bottom(&screen),
                Duration::from_secs(60)
            )
            .state,
            AgentState::NeedsInput
        );
    }

    #[test]
    fn a_hard_newline_is_not_joined_to_the_next_line() {
        let r = AgentRules::from_config(&AgentsConfig {
            commands: vec!["claude".to_string()],
            working_ms: 500,
            scan_rows: 12,
            // Only matches if two SEPARATE lines were wrongly joined.
            pattern: vec![pattern("joined", None, r"firstsecond")],
        });
        let mut screen = Screen::new(40, 10, 100);
        screen.process_output(b"first\r\nsecond");
        assert_eq!(
            r.classify(
                "claude",
                &r.visible_bottom(&screen),
                Duration::from_secs(60)
            )
            .state,
            AgentState::Idle
        );
    }

    /// Trailing padding on a line that does NOT continue is still dropped --
    /// the fix for the wrapped case must not stop trimming the ordinary one.
    #[test]
    fn an_unwrapped_line_still_loses_its_padding() {
        let r = shipped();
        let mut screen = Screen::new(40, 4, 100);
        screen.process_output(b"hi");
        assert_eq!(r.visible_bottom(&screen), vec!["hi".to_string()]);
    }

    #[test]
    fn a_blank_screen_scans_to_nothing() {
        let r = shipped();
        let screen = Screen::new(20, 10, 100);
        assert!(r.visible_bottom(&screen).is_empty());
    }

    #[test]
    fn a_scan_window_larger_than_the_screen_is_the_whole_screen() {
        let r = shipped();
        let mut screen = Screen::new(20, 3, 100);
        screen.process_output(b"a\r\nb\r\nc");
        assert_eq!(r.visible_bottom(&screen).len(), 3);
    }

    // -- the job walk -----------------------------------------------------

    /// A synthetic process tree, so the walk can be tested without a `/proc`.
    fn tree<'a>(edges: &'a [(i32, &'a [i32])]) -> impl Fn(i32) -> Vec<i32> + 'a {
        move |pid| {
            edges
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, kids)| kids.to_vec())
                .unwrap_or_default()
        }
    }

    fn names(name: &str, argv0: Option<&str>) -> ProcessNames {
        ProcessNames {
            name: name.to_string(),
            argv0: argv0.map(|a| a.to_string()),
        }
    }

    #[test]
    fn the_leader_is_the_first_candidate_and_the_rest_are_breadth_first() {
        let kids = tree(&[(1, &[2, 3]), (2, &[4]), (3, &[5])]);
        let got: Vec<i32> = job_candidates(1, kids).collect();
        assert_eq!(got, vec![1, 2, 3, 4, 5]);
    }

    /// The walk is bounded because it runs for EVERY pane at up to 10 Hz. A
    /// pane whose leader is a build tool has a wide, deep tree underneath it.
    #[test]
    fn the_walk_stops_at_the_depth_bound() {
        // A chain, one child each: 1 -> 2 -> 3 -> 4 -> 5.
        let kids = tree(&[(1, &[2]), (2, &[3]), (3, &[4]), (4, &[5])]);
        let got: Vec<i32> = job_candidates(1, kids).collect();
        assert_eq!(got, vec![1, 2, 3, 4], "depth 4 is past the bound");
    }

    #[test]
    fn the_walk_stops_at_the_candidate_bound() {
        let wide: Vec<i32> = (100..200).collect();
        let kids = |pid: i32| if pid == 1 { wide.clone() } else { Vec::new() };
        let got: Vec<i32> = job_candidates(1, kids).collect();
        assert_eq!(got.len(), MAX_CANDIDATES);
        assert_eq!(got[0], 1, "the leader is still first");
    }

    /// A leader with nothing under it costs ONE children lookup, not a sweep.
    #[test]
    fn a_childless_leader_ends_the_walk() {
        let got: Vec<i32> = job_candidates(1, |_| Vec::new()).collect();
        assert_eq!(got, vec![1]);
    }

    /// **Polled PAST exhaustion, on every route out.**
    ///
    /// `collect()` stops at the first `None` and so cannot see this, which is
    /// why `a_childless_leader_ends_the_walk` passed over a walk that PANICKED
    /// on its second `None`: the generation that found no children had already
    /// emptied `frontier` while leaving the cursor where it was, so the poll
    /// after it indexed an empty vec. The childless route is the ordinary one
    /// -- every pane whose leader misses and has no children takes it, on every
    /// sample -- and this runs on a timer against panes that are closing, where
    /// a panic is the one thing the module promises not to do.
    #[test]
    fn the_walk_is_fused_on_every_route_out() {
        /// One way for the walk to run out.
        type Route = (&'static str, Box<dyn Fn(i32) -> Vec<i32>>);
        let routes: [Route; 3] = [
            // Ran out of children.
            ("childless", Box::new(|_| Vec::new())),
            // Ran out of depth.
            ("deep", Box::new(|pid| vec![pid + 1])),
            // Ran out of candidates.
            (
                "wide",
                Box::new(|pid| {
                    if pid == 1 {
                        (100..300).collect()
                    } else {
                        Vec::new()
                    }
                }),
            ),
        ];
        for (name, children) in routes {
            let mut walk = job_candidates(1, children);
            while walk.next().is_some() {}
            for poll in 0..3 {
                assert_eq!(walk.next(), None, "{name}: poll {poll} past the end");
            }
        }
    }

    /// THE npm-shim BUG, as a unit test.
    ///
    /// `codex` installs as a Node shim that `spawn()`s the real binary and
    /// stays alive as its parent, so the foreground process GROUP's leader is
    /// node -- whose `comm` is `MainThread` on Node 22+ and whose `argv[0]` is
    /// `node`. Neither can ever match, and the pane was invisible.
    #[test]
    fn a_wrapper_is_recognised_by_the_job_it_started() {
        let r = shipped();
        let candidates = vec![
            (10, names("MainThread", Some("node"))),
            (11, names("codex", Some("/opt/codex/bin/codex"))),
        ];
        let (pid, _, command) =
            first_match(&r, candidates).expect("the child is a configured agent");
        assert_eq!(pid, 11);
        assert_eq!(command, "codex");
    }

    /// The leader is tried first, and a match there ends it -- the walk is a
    /// rescue for panes the old rule missed, never a re-decision of one it
    /// already got right.
    #[test]
    fn a_configured_leader_outranks_a_configured_descendant() {
        let r = shipped();
        let candidates = vec![(10, names("claude", None)), (11, names("codex", None))];
        let (pid, _, command) = first_match(&r, candidates).expect("the leader is an agent");
        assert_eq!((pid, command), (10, "claude"));
    }

    #[test]
    fn a_job_of_unconfigured_processes_is_not_an_agent() {
        let r = shipped();
        let candidates = vec![
            (10, names("python3", Some("python3"))),
            (11, names("notanagent", Some("notanagent"))),
        ];
        assert!(first_match(&r, candidates).is_none());
    }

    /// Reading a process's names is two `/proc` reads, so the candidates must
    /// be consumed LAZILY: a hit on the leader must not cost a walk.
    #[test]
    fn no_candidate_past_the_hit_is_ever_read() {
        use std::cell::Cell;
        let r = shipped();
        let reads = Cell::new(0);
        let candidates = [10, 11, 12].into_iter().map(|pid| {
            reads.set(reads.get() + 1);
            (pid, names(if pid == 10 { "claude" } else { "codex" }, None))
        });
        assert!(first_match(&r, candidates).is_some());
        assert_eq!(reads.get(), 1, "the walk stopped at the first hit");
    }
}
