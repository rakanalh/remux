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
//! `commands` ships with the agents this project knows about (`claude`,
//! `codex`, `aider`, `gemini`, `omp`, `opencode`), and blocked-prompt patterns
//! ship for `claude` and `codex`, so the panel works with nothing configured.
//! They are best-effort by nature: if an agent changes its prompt, the fix is
//! `[[agents.pattern]]`, not a patch.
//!
//! The server reads this at startup, so an edit needs `remux restart`.

use serde::Deserialize;

/// The commands that count as an agent out of the box.
///
/// These are NAMES of agents, never of the launchers that start them. `node`,
/// `bun` and `npx` are deliberately absent: an npm-shipped agent is a script
/// run by an interpreter, and listing the interpreter would put every Node REPL
/// in the panel. The foreground JOB is walked instead, so a shim is looked
/// through to the agent it started -- see
/// [`crate::server::agents::foreground_command`].
fn default_commands() -> Vec<String> {
    ["claude", "codex", "aider", "gemini", "omp", "opencode"]
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

/// The shipped patterns: enough for `claude`, `codex`, `omp` and `opencode` to
/// work unconfigured.
///
/// **Two grades of evidence sit in this list, and the split does NOT run along
/// agent lines.** `claude-select`, `omp-allow` and the two `opencode` patterns
/// were matched against a real pane, blocked and then answered, so both that
/// they FIRE and that they STOP are observed. `claude-proceed` and the two
/// `codex` patterns come from `strings` over the shipped binary: the wordings
/// are real, but no one here has watched one of those prompts appear or clear.
///
/// `claude-proceed` is on the weaker side of that line and it is easy to miss,
/// because it sits next to `claude-select` and they name the same agent. Do not
/// level the comments up to each other.
///
/// **Deliberately no menu-ROW pattern -- and that is not in tension with the
/// question-menu FOOTER pattern that IS shipped.** An earlier default matched
/// the selected row of a numbered menu (`^\s*>\s*\d+\.`), as a second chance
/// for a question that had scrolled out of the window. It is not shipped, for
/// two reasons pointing the same way:
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
/// **`claude-select` is exempt from the first reason, and the exemption is
/// EVIDENCE rather than an argument.** It matches the selector's footer, and
/// the same pane was dumped before and after the question was answered: the
/// footer is present while the selector is live and gone once the block
/// collapses to `User answered Claude's questions: ...`. It is a keybinding
/// hint bound to an active overlay, not content that lands in the transcript,
/// so the "an answered menu is indistinguishable from a waiting one" objection
/// simply does not arise for it. Whoever is tempted to delete this pattern on
/// staleness grounds is re-deriving an objection that was already tested.
///
/// A user who wants the menu row back adds it in one config entry; the sample
/// config shows it with this caveat attached.
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
        // Claude Code's QUESTION menu -- its `AskUserQuestion` selector, which
        // asks something rather than asking permission and so matches none of
        // the wordings above. A user's pane sat blocked on "Which color do you
        // like?" and the panel showed grey.
        //
        // Keyed on the selector's FOOTER (`Enter to select . arrows to
        // navigate . Esc to cancel`), NOT on the `> 1.` menu row -- and the
        // difference is the entire staleness argument this module's other
        // comment makes. A left-behind menu row is indistinguishable in a
        // snapshot from a waiting one; the footer is a live keybinding hint
        // tied to an ACTIVE selector. Verified rather than assumed: the same
        // pane was dumped again after the question was answered, and the whole
        // block had collapsed to
        //
        //     User answered Claude's questions:
        //       . Which color do you like? -> Blue
        //
        // with no footer anywhere on screen. So this cannot hold the panel red
        // for ever the way the menu row could.
        //
        // Two phrases, not the whole line: it carries a middot, two arrow
        // glyphs and spacing nobody should be hand-transcribing, and `.*`
        // spans all of that. Requiring BOTH ends is what keeps the specificity
        // -- the same crying-wolf argument that stops `claude-proceed` from
        // matching the bare "Do you want to " prefix.
        //
        // It generalises on purpose: any Claude Code overlay drawing this
        // footer is by definition waiting on the user, so matching another
        // selector is the right answer and not a false positive.
        p(
            "claude-select",
            "claude",
            r"(?i)enter to select.*esc to cancel",
        ),
        // Codex's approval dialogs. MEASURED the way the claude ones were --
        // `strings` over the installed 0.156.0 native binary
        // (`@openai/codex-linux-x64/.../bin/codex`) -- and NOT, unlike the two
        // below, watched on a live screen. codex could not be driven to an
        // approval on either machine available: this Linux box is refused by
        // the API for every model (`The 'gpt-5-codex' model is not supported
        // when using Codex with a ChatGPT account.`) and the Mac hangs. So the
        // titles are real; that they DISAPPEAR once answered is inferred from
        // how the other agents behave, not observed. `NeedsInput` never decays,
        // so if one of these turns out to linger in the transcript it pins the
        // pane red for the session and must be narrowed.
        //
        // The binary's four dialog titles are `Would you like to run the
        // following command?`, `...make the following edits?`, `...grant these
        // permissions?` and `...send input to terminal`.
        //
        // `codex-decline` matches an OPTION of that same modal, which is the
        // point rather than a duplicate of the titles: the window scanned is
        // the bottom of the screen, and a tall dialog can push its title out of
        // it while the options are still drawn. The one string NOT made a
        // pattern is `" needs your approval."`, which is ordinary enough to
        // appear in the agent's own prose -- and a false `NeedsInput` does not
        // decay. (`Yes, and don't ask again...` would serve as well as the
        // `No, ...` row; one option row is enough.)
        //
        // These REPLACE `codex-allow` and `codex-yn`, which matched nothing in
        // the binary. `codex-yn` (`\[y(es)?/n(o)?\]`) was worse than useless:
        // codex prints no such prompt, so it could only fire on something
        // else's output passing through the pane -- a script, a diff, a test
        // log -- and a false `NeedsInput` pins the pane red for the rest of the
        // session because it never decays.
        p(
            "codex-approve",
            "codex",
            r"(?i)would you like to (run|make|grant|send)",
        ),
        p(
            "codex-decline",
            "codex",
            r"(?i)no, and tell codex what to do differently",
        ),
        // omp's approval box, CAPTURED LIVE from 18.1.19 -- blocked and then
        // answered, the same evidence `claude-select` rests on. The box title
        // names the tool being approved (`bash`, `edit`, `write`), so keying on
        // the title covers every tool kind with one rule instead of one per
        // kind. Answering it removes the whole box: the pane was re-snapshotted
        // at +4s, +8s, +12s and +16s and no part of the title survives in any
        // of them, which is what stops this holding a pane red for ever.
        p("omp-allow", "omp", r"(?i)allow tool:"),
        // opencode's permission prompt, CAPTURED LIVE from 1.18.32, also
        // blocked and then answered. Two patterns because the header and the
        // options row are several lines apart and either can leave the
        // `scan_rows` window on its own. Both are gone from the +4s snapshot
        // onwards.
        // This is the widest false-positive surface of the five: `permission
        // required` is ordinary text in an HTTP 403, a docker error and an npm
        // failure, so an opencode pane that merely PRINTS one reads NeedsInput
        // until it scrolls out of the window. Shipped anyway, because the
        // alternative is guessing a tighter anchor from a single capture and
        // missing the real prompt, which is the failure that has no symptom. If
        // it is reported, narrow it -- against a new capture -- rather than
        // removing it.
        p(
            "opencode-permission",
            "opencode",
            r"(?i)permission required",
        ),
        // `\s+` between the options, never literal runs of spaces: the run
        // between them is LAYOUT, not content -- one capture at one width
        // cannot show how opencode re-lays it out, and a pattern has no reason
        // to depend on a gap it is not matching.
        //
        // The width sweep is NOT what guards this, and assuming it was would
        // have shipped a literal-space pattern with a green suite: rewrapping a
        // line changes where the ROWS break and never its characters, so
        // `visible_bottom` hands the pattern back the original spacing at every
        // width. The guard is the collapsed-spacing assertion in
        // `the_shipped_patterns_recognise_opencodes_real_permission_prompt`,
        // which is the only test in the suite a literal-space version fails.
        p(
            "opencode-allow",
            "opencode",
            r"(?i)allow once\s+allow always\s+reject",
        ),
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

    /// The shipped list is what makes the panel work with zero configuration,
    /// so every agent we claim to support has to be in it.
    #[test]
    fn the_defaults_list_every_agent_we_ship_for() {
        let cfg = parse("");
        for command in ["claude", "codex", "aider", "gemini", "omp", "opencode"] {
            assert!(
                cfg.commands.contains(&command.to_string()),
                "{command:?} is missing from the shipped commands"
            );
        }
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
