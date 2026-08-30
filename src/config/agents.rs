//! `[agents]` configuration: which commands count as an AI coding agent, and
//! what a BLOCKED one looks like on screen.
//!
//! The patterns are data on purpose. Screen-scraping an agent's UI is the only
//! zero-setup signal available -- the agents do not yet emit lifecycle events we
//! could subscribe to -- and its known weakness is that a pattern rots on the
//! next release of the agent, or under a theme that draws the prompt
//! differently. Keeping them in `config.toml` makes that a config edit instead
//! of a rebuild, which is the same call `herdr` makes with its TOML manifests.
//!
//! Defaults ship for `claude` and `codex` so the panel works with nothing
//! configured. They are best-effort by nature: if an agent changes its prompt,
//! the fix is `[[agents.pattern]]`, not a patch.
//!
//! The server reads this at startup, so an edit needs `remux restart`.

use serde::Deserialize;

fn default_commands() -> Vec<String> {
    ["claude", "codex", "aider", "gemini"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_working_ms() -> u64 {
    500
}

fn default_scan_rows() -> u16 {
    // 24, not 12. A real agent's blocked prompt is not one line: Claude Code
    // renders the question, three or four options, a box border, a blank, a
    // three-row input box and a hint line, which puts the QUESTION about a
    // dozen rows above the bottom of the screen -- so a twelve-line window sat
    // exactly on the boundary, and a window one line too short fails precisely
    // when the user needs it. The option count is measured (the binary carries
    // the labels); the surrounding box geometry is inferred, so the margin is
    // deliberate rather than tuned. Doubling it is cheap: the cost is linear,
    // and the failure it prevents is silent.
    24
}

/// A pattern that, when it matches the visible bottom of an agent pane's
/// screen, means the agent is blocked on the user.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AgentPattern {
    /// Short label, reported in the explain log when this pattern decides a
    /// pane's state. Naming them is what makes the classifier tunable: "why is
    /// this one idle" is answerable only if a match can say which rule fired.
    pub name: String,
    /// The agent command this applies to, e.g. `"claude"`. Omit to apply it to
    /// every agent.
    #[serde(default)]
    pub command: Option<String>,
    /// A Rust `regex` pattern, matched against the last `scan_rows` LOGICAL
    /// LINES of the live screen, one line at a time.
    ///
    /// **Lines, not rows.** Soft-wrapped rows are rejoined before matching, so
    /// `^` and `$` anchor to the start and end of a wrapped-out line, not of a
    /// screen row. That is what lets a pattern survive a narrow pane -- and it
    /// also means an anchored marker pattern matches only where the marker
    /// begins a LINE, never where it happens to begin a continuation row.
    ///
    /// A pattern that fails to compile is logged and skipped; it never takes
    /// the server down.
    pub regex: String,
}

/// `[agents]`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentsConfig {
    /// The foreground commands that count as an agent. A pane running anything
    /// else is not listed at all.
    pub commands: Vec<String>,
    /// How recently output must have reached a pane for it to read as
    /// `Working`.
    pub working_ms: u64,
    /// How many LOGICAL LINES up from the bottom of the live screen the
    /// patterns are matched against (soft-wrapped rows are joined first, so on
    /// a narrow pane this is more rows than it is lines).
    ///
    /// The bottom, not the scrollback: an approval prompt the user has scrolled
    /// past is not what the agent is showing now. Bounded rather than
    /// whole-screen so a pattern cannot match something that scrolled up into
    /// the transcript and sit there for ever.
    pub scan_rows: u16,
    /// `[[agents.pattern]]` entries. Replacing this list REPLACES the defaults
    /// -- there is no merge, so a user who writes their own owns the whole set
    /// and cannot be surprised by a shipped pattern they cannot see.
    pub pattern: Vec<AgentPattern>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            commands: default_commands(),
            working_ms: default_working_ms(),
            scan_rows: default_scan_rows(),
            pattern: default_patterns(),
        }
    }
}

/// The shipped patterns: enough for `claude` and `codex` to work unconfigured.
///
/// **Deliberately no menu-shaped pattern.** An earlier default matched the
/// selected row of a numbered menu, as a second chance for a question that had
/// scrolled out of the window. It is not shipped, for two reasons pointing the
/// same way:
///
/// * **It is unverifiable in the way that matters.** If an agent leaves an
///   ANSWERED menu on screen with its chosen row still marked, no pattern
///   evaluated against a snapshot can tell it from one still waiting -- and a
///   panel stuck red for ever is worse than one that is slow, because it trains
///   the user to ignore the colour. Whether Claude Code clears that row after an
///   answer is not something this project has established: a session running
///   with permission prompts disabled never renders one, and the binary's
///   strings do not settle it.
/// * **Its specificity is far below the question patterns'.** A marker, a digit
///   and a dot appear in plenty of ordinary agent output; `Do you want to
///   proceed?` essentially does not.
///
/// The same staleness argument applies in principle to the question patterns,
/// and is much weaker there: a resumed agent redraws, and its output pushes the
/// question out of the `scan_rows` window within a screenful. A menu row is the
/// LAST thing on screen, so it would survive longest.
///
/// A user who wants the second chance adds it back in one config entry; the
/// sample config shows it with this caveat attached.
fn default_patterns() -> Vec<AgentPattern> {
    let p = |name: &str, command: &str, regex: &str| AgentPattern {
        name: name.to_string(),
        command: Some(command.to_string()),
        regex: regex.to_string(),
    };
    vec![
        // Claude Code's permission/approval prompts. The alternation is
        // MEASURED, not guessed: `strings` over the installed 2.1.251 binary
        // reports "Do you want to proceed?" (x6), "...to continue?",
        // "...to allow this connection?", "...to allow Claude to fetch this
        // content?" and "...to use this API key?". An earlier version of this
        // pattern listed `create` and `make`, neither of which appears anywhere
        // in the binary.
        //
        // The binary also carries a bare "Do you want to " prefix, completed at
        // render time. It is deliberately NOT matched on the prefix alone: an
        // agent's own prose can contain that phrase, and a false `NeedsInput`
        // cries wolf on a panel whose whole value is that red means red.
        p(
            "claude-proceed",
            "claude",
            r"(?i)do you want to (proceed|continue|allow|use this api key)",
        ),
        // Codex's command-approval prompt.
        p(
            "codex-allow",
            "codex",
            r"(?i)allow (this )?command|approve this (command|action)",
        ),
        p("codex-yn", "codex", r"(?i)\[y(es)?/n(o)?\]"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        agents: AgentsConfig,
    }

    fn parse(s: &str) -> AgentsConfig {
        toml::from_str::<Wrapper>(s)
            .expect("config should parse")
            .agents
    }

    #[test]
    fn an_absent_table_ships_the_defaults() {
        let cfg = parse("");
        assert!(cfg.commands.contains(&"claude".to_string()));
        assert!(cfg.commands.contains(&"codex".to_string()));
        assert_eq!(cfg.working_ms, 500);
        assert!(
            cfg.pattern
                .iter()
                .any(|p| p.command.as_deref() == Some("claude")),
            "claude works with zero setup or it does not work"
        );
    }

    #[test]
    fn a_partial_table_keeps_the_other_defaults() {
        let cfg = parse("[agents]\nworking_ms = 200\n");
        assert_eq!(cfg.working_ms, 200);
        assert!(!cfg.commands.is_empty(), "commands kept its default");
        assert!(!cfg.pattern.is_empty(), "patterns kept theirs");
    }

    #[test]
    fn user_patterns_replace_the_shipped_ones_rather_than_merging() {
        let cfg = parse(
            r#"
[agents]
commands = ["mine"]

  [[agents.pattern]]
  name = "mine-blocked"
  command = "mine"
  regex = "press enter"
"#,
        );
        assert_eq!(cfg.commands, vec!["mine".to_string()]);
        assert_eq!(cfg.pattern.len(), 1, "no merge with the defaults");
        assert_eq!(cfg.pattern[0].name, "mine-blocked");
    }

    #[test]
    fn a_pattern_with_no_command_is_allowed_and_means_every_agent() {
        let cfg = parse(
            r#"
[agents]

  [[agents.pattern]]
  name = "any"
  regex = "waiting"
"#,
        );
        assert_eq!(cfg.pattern[0].command, None);
    }
}
