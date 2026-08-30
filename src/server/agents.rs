//! Detecting AI coding agents in panes, and classifying what they are doing.
//!
//! Two jobs, both pure enough to test on their own:
//!
//! * **Detection** ([`foreground_command`]) reads the PTY's foreground process
//!   group and asks `/proc` what it is. `Pty::child_pid` is the login SHELL, so
//!   the pane's own name is `zsh` however long `claude` has been running in it;
//!   `tcgetpgrp` is what sees past that. Deliberately scoped to agent detection
//!   -- pane NAMES are untouched, because fixing them the same way would rename
//!   every pane border in the product and that needs its own decision.
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

/// The command running in the foreground of this PTY, e.g. `"claude"`.
///
/// `None` when the PTY has no foreground group to report -- a child that is
/// exiting, or already gone. NEVER a panic: this runs on a timer against panes
/// that close underneath it, which is the exact boundary at which Phase C found
/// two latent PTY bugs.
pub fn foreground_command(fd: BorrowedFd<'_>) -> Option<String> {
    let pgid = nix::unistd::tcgetpgrp(fd).ok()?;
    // The same `/proc/<pid>/comm` reader the session tree uses for pane names,
    // handed the process GROUP leader instead of the shell. One reader.
    Some(crate::server::daemon::get_process_name(pgid.as_raw()))
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

    /// Whether a pane whose foreground command is `comm` belongs in the list.
    pub fn is_agent(&self, comm: &str) -> bool {
        self.commands.iter().any(|c| c == comm)
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
                lines.push(std::mem::take(&mut current));
            }
        }
        if continuing {
            lines.push(current);
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

/// The text of one screen row, with trailing blanks removed.
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

    #[test]
    fn only_configured_commands_are_agents() {
        let r = shipped();
        assert!(r.is_agent("claude"));
        assert!(r.is_agent("codex"));
        assert!(!r.is_agent("zsh"));
        assert!(!r.is_agent("vim"));
        // `get_process_name`'s unreadable-process fallback must not be an agent.
        assert!(!r.is_agent("shell"));
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

    #[test]
    fn the_shipped_patterns_recognise_claudes_permission_prompt() {
        let r = shipped();
        let screen = vec![
            "Do you want to proceed?".to_string(),
            "❯ 1. Yes".to_string(),
            "  2. No".to_string(),
        ];
        assert_eq!(
            r.classify("claude", &screen, Duration::from_secs(30)).state,
            AgentState::NeedsInput,
            "the panel has to work with zero configuration"
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

    /// The second bug a probe found: a prompt that soft-wraps in a narrow pane.
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
