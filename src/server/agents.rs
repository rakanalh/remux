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
//!   [`crate::server::daemon::ProcessNames`]. Deliberately scoped to agent
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
/// The log line is the only diagnostic anyone gets for "the panel is empty but
/// the agent is right there", so it says which names were read rather than just
/// that nothing matched. See [`log_detection`] for why it is deduplicated
/// instead of levelled down.
pub fn foreground_command(fd: BorrowedFd<'_>, rules: &AgentRules) -> Option<String> {
    let pgid = nix::unistd::tcgetpgrp(fd).ok()?;
    let pid = pgid.as_raw();
    let names = crate::server::daemon::get_process_names(pid);
    let matched = rules.match_command(&names.name, names.argv0.as_deref());
    log_detection(pid, &names, matched);
    matched.map(|c| c.to_string())
}

/// Say what this pane's process was called and what it was taken for -- ONCE
/// per distinct answer.
///
/// **The dedupe is what makes this loggable at all.** There is no level quiet
/// enough to hide behind: `main.rs` pins the logger at `Debug` and never reads
/// `RUST_LOG`, so `trace!` would be invisible to a user and `debug!` would fire
/// for every non-agent pane on every sample -- tens of lines a second into a log
/// that is not rotated. One line per newly-seen foreground process is what
/// somebody debugging "the panel is empty" actually wants, and it is the only
/// thing that can answer "so what DID this platform call it".
///
/// A pane matched on its own name is the ordinary case and says nothing.
///
/// The cache is CLEARED rather than evicted when it fills. It exists to suppress
/// repeats, not to remember, so the worst a clear costs is one repeated line --
/// against an eviction policy whose bound would have to be argued for.
fn log_detection(pid: i32, names: &crate::server::daemon::ProcessNames, matched: Option<&str>) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    /// One logged answer: the pid, both names it went by, and whether it was
    /// taken for an agent.
    type Answer = (i32, String, Option<String>, bool);
    /// Distinct foreground processes remembered before starting over.
    const CAP: usize = 256;
    static SEEN: OnceLock<Mutex<HashSet<Answer>>> = OnceLock::new();

    if matched == Some(names.name.as_str()) {
        return;
    }
    let key = (
        pid,
        names.name.clone(),
        names.argv0.clone(),
        matched.is_some(),
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
    match matched {
        Some(command) => log::debug!(
            "agents: pid={pid} is {command:?}, matched on argv[0] {:?} -- \
             this platform names the process {:?}",
            names.argv0,
            names.name
        ),
        None => log::debug!(
            "agents: pid={pid} is not a configured agent: name={:?} argv0={:?}",
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
    pub why: String,
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
                };
            }
        }
        if since_output <= self.working {
            return Verdict {
                state: AgentState::Working,
                why: format!("output {}ms ago", since_output.as_millis()),
            };
        }
        Verdict {
            state: AgentState::Idle,
            why: format!(
                "no pattern matched; silent for {}ms",
                since_output.as_millis()
            ),
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
        for question in CLAUDE_QUESTIONS {
            for cols in 8..=60u16 {
                let mut screen = Screen::new(cols, 12, 100);
                screen.process_output(question.as_bytes());
                assert_eq!(
                    r.classify(
                        "claude",
                        &r.visible_bottom(&screen),
                        Duration::from_secs(60)
                    )
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
}
