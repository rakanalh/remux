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
    12
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
    /// A Rust `regex` pattern. Matched against each of the last `scan_rows`
    /// rows of the LIVE screen, one row at a time -- so anchors are row
    /// anchors, not screen anchors.
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
    /// How many rows up from the bottom of the live screen the patterns are
    /// matched against.
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
fn default_patterns() -> Vec<AgentPattern> {
    let p = |name: &str, command: &str, regex: &str| AgentPattern {
        name: name.to_string(),
        command: Some(command.to_string()),
        regex: regex.to_string(),
    };
    vec![
        // Claude Code's permission/approval prompt: a question followed by a
        // numbered menu whose first entry is the affirmative.
        p(
            "claude-proceed",
            "claude",
            r"(?i)do you want to (proceed|continue|create|make)",
        ),
        p("claude-choice", "claude", r"❯\s*1\.\s*Yes"),
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
