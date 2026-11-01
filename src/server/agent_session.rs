//! The session an agent in a pane reports it is running, kept so that a
//! restore can type the agent's resume command into the respawned shell.
//!
//! Pure: validation and the replacement rule, with no I/O, so that the daemon
//! applies them under the `panes` lock and the restore path can apply them
//! again to a record read back from an edited `state.json`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::protocol::{AgentSessionOutcome, AgentSessionRef, RefKind};

/// One pane's agent session, as held live on the pane and persisted beside
/// its cwd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionRecord {
    pub agent: String,
    pub kind: RefKind,
    pub value: String,
}

impl AgentSessionRecord {
    pub fn new(agent: &str, reference: &AgentSessionRef) -> Self {
        AgentSessionRecord {
            agent: agent.to_string(),
            kind: reference.kind(),
            value: reference.value().to_string(),
        }
    }

    pub fn reference(&self) -> AgentSessionRef {
        match self.kind {
            RefKind::Id => AgentSessionRef::Id(self.value.clone()),
            RefKind::Path => AgentSessionRef::Path(self.value.clone()),
        }
    }

    /// [`validate`] for a record read back from disk, which may have been
    /// edited since it was stored.
    pub fn validate(&self) -> Result<(), Refusal> {
        validate(&self.agent, &self.reference())
    }
}

/// `agent` as it may be logged: one of the agents remux knows, else
/// `<invalid>`. The field is whatever the sender put there, and arguments
/// given in the wrong order put the session id in it.
pub fn agent_label(agent: &str) -> &'static str {
    ["claude", "codex", "opencode", "pi", "omp"]
        .into_iter()
        .find(|known| *known == agent)
        .unwrap_or("<invalid>")
}

/// Why a report is refused, as the `code` of a `ServerMessage::Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    BadAgent,
    BadReference(&'static str),
    SessionHeld(Held),
}

/// What a pane holds that a `session_held` report could not replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// A different session of the same agent, and the report's source is not
    /// one that agent starts a new session in this pane with.
    DifferentId,
    /// Another agent's session.
    OtherAgent,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::BadAgent => "bad_agent",
            Refusal::BadReference(_) => "bad_reference",
            Refusal::SessionHeld(_) => "session_held",
        }
    }

    pub fn message(self) -> String {
        match self {
            Refusal::BadAgent => {
                "unknown agent: expected claude, codex, opencode, pi or omp".into()
            }
            Refusal::BadReference(why) => format!("bad session reference: {why}"),
            Refusal::SessionHeld(Held::DifferentId) => {
                "the pane already holds a different session of this agent".into()
            }
            Refusal::SessionHeld(Held::OtherAgent) => {
                "the pane already holds another agent's session".into()
            }
        }
    }

    /// Why, for the log: which of two refusals with one code it was.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Refusal::SessionHeld(Held::DifferentId) => Some("holds a different id"),
            Refusal::SessionHeld(Held::OtherAgent) => Some("held by another agent"),
            _ => None,
        }
    }
}

const MAX_ID: usize = 128;
const MAX_PATH: usize = 4096;

/// Whether `agent` names its sessions by file. Such an agent may still send an
/// id when it has no file yet.
fn takes_paths(agent: &str) -> Option<bool> {
    match agent {
        "claude" | "codex" | "opencode" => Some(false),
        "pi" | "omp" => Some(true),
        _ => None,
    }
}

/// The first byte is alphanumeric because the id is typed after the agent's
/// flag: `--yolo` as an id would otherwise become `codex resume --yolo`.
fn valid_id(id: &str) -> bool {
    (1..=MAX_ID).contains(&id.len())
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'@' | b'-'))
}

/// Absolute, so it can never start with `-`. No backslash: the typed line is
/// quoted for POSIX shells, and fish reads `\'` inside single quotes as an
/// escaped quote, which would end the quoting early in a fish pane.
fn valid_path(path: &str) -> bool {
    path.len() <= MAX_PATH
        && path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
}

/// Whether a report of `reference` from `agent` may be kept at all. The id
/// charset admits UUIDs and OpenCode's `ses_...` ids and nothing a shell would
/// treat specially, because the restore types the value into a shell.
pub fn validate(agent: &str, reference: &AgentSessionRef) -> Result<(), Refusal> {
    let takes_paths = takes_paths(agent).ok_or(Refusal::BadAgent)?;
    match reference {
        AgentSessionRef::Id(id) if valid_id(id) => Ok(()),
        AgentSessionRef::Id(_) => Err(Refusal::BadReference(
            "an id is 1 to 128 bytes of [A-Za-z0-9._:@-], starting with a letter or digit",
        )),
        AgentSessionRef::Path(_) if !takes_paths => Err(Refusal::BadReference(
            "this agent reports an id, not a path",
        )),
        AgentSessionRef::Path(path) if valid_path(path) => Ok(()),
        AgentSessionRef::Path(_) => Err(Refusal::BadReference(
            "a path is absolute, at most 4096 bytes, with no backslash or control characters",
        )),
    }
}

/// Whether `agent` starts a NEW session in the same pane with `source`, so a
/// report of it replaces the session the pane holds. Anything else, `startup`
/// for claude above all, may be a nested run: Claude Code's Bash tool running
/// `claude -p` reports `startup` from the same pane, with an id of its own.
pub fn allows_replace(agent: &str, source: Option<&str>) -> bool {
    let allowed: &[&str] = match agent {
        "claude" => &["clear", "resume", "compact", "fork"],
        "codex" => &["startup", "clear", "resume", "compact"],
        "opencode" => &["select"],
        "pi" => &["new", "resume", "fork"],
        "omp" => &["startup", "new", "resume", "fork"],
        _ => &[],
    };
    source.is_some_and(|source| allowed.contains(&source))
}

/// What a valid report does to a pane that holds `held`. Another agent's
/// record is never replaced, whatever the source: claude's Bash tool running
/// `codex exec` reports codex's `startup`, which codex allows. That pane's
/// record goes when claude exits and the shell takes the foreground back.
pub fn decide(
    held: Option<&AgentSessionRecord>,
    report: &AgentSessionRecord,
    source: Option<&str>,
) -> Result<AgentSessionOutcome, Refusal> {
    match held {
        None => Ok(AgentSessionOutcome::Stored),
        Some(held) if held == report => Ok(AgentSessionOutcome::Unchanged),
        Some(held) if held.agent != report.agent => Err(Refusal::SessionHeld(Held::OtherAgent)),
        Some(_) if allows_replace(&report.agent, source) => Ok(AgentSessionOutcome::Replaced),
        Some(_) => Err(Refusal::SessionHeld(Held::DifferentId)),
    }
}

/// The records a restore may resume: one pane per session, the lowest pane
/// id, so that two panes holding the same session never run it twice. A
/// session one of the `live` panes already holds is resumed by none of
/// `records`, because that pane is running it.
pub fn dedupe(
    records: &HashMap<u64, AgentSessionRecord>,
    live: &[&AgentSessionRecord],
) -> HashMap<u64, AgentSessionRecord> {
    let mut ids: Vec<u64> = records.keys().copied().collect();
    ids.sort_unstable();
    let mut seen: Vec<&AgentSessionRecord> = live.to_vec();
    let mut winners = HashMap::new();
    for id in ids {
        let record = &records[&id];
        if !seen.contains(&record) {
            seen.push(record);
            winners.insert(id, record.clone());
        }
    }
    winners
}

/// The command that resumes `record`'s session, one argv word each. `None`
/// for an agent this server does not know, which [`validate`] refuses.
pub fn plan(record: &AgentSessionRecord) -> Option<Vec<String>> {
    let value = record.value.clone();
    // The value is joined to its flag, or follows `--`, wherever the agent's
    // parser takes that form, so that even a valid value is never read as an
    // option. pi's parser could not be checked, so it keeps `--session <v>`;
    // its values cannot start with `-` (see `validate`).
    let words = match record.agent.as_str() {
        "claude" => vec!["claude".into(), format!("--resume={value}")],
        "codex" => vec!["codex".into(), "resume".into(), "--".into(), value],
        // omp has no `--session`, and takes the value only joined to the flag.
        "omp" => vec!["omp".into(), format!("--resume={value}")],
        "pi" => vec!["pi".into(), "--session".into(), value],
        "opencode" => vec!["opencode".into(), format!("--session={value}")],
        _ => return None,
    };
    Some(words)
}

/// What a restore types into the respawned shell to resume `record`. The
/// leading space keeps the line out of the history of a shell that sets
/// `ignorespace` or `HIST_IGNORE_SPACE`, and the `\r` submits it.
pub fn typed_line(record: &AgentSessionRecord) -> Option<String> {
    let words: Vec<String> = plan(record)?
        .iter()
        .map(|w| crate::shell_word::sh_word(w))
        .collect();
    Some(format!(" {}\r", words.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "7f3c2a9e-51d4-4b8e-9a61-0c2f5d8e4b17";

    fn id(v: &str) -> AgentSessionRef {
        AgentSessionRef::Id(v.to_string())
    }

    fn path(v: &str) -> AgentSessionRef {
        AgentSessionRef::Path(v.to_string())
    }

    #[test]
    fn every_agent_accepts_a_uuid() {
        for agent in ["claude", "codex", "opencode", "pi", "omp"] {
            assert_eq!(validate(agent, &id(UUID)), Ok(()), "{agent}");
        }
        assert_eq!(validate("opencode", &id("ses_3f9a1bC2d")), Ok(()));
    }

    #[test]
    fn an_unknown_agent_is_bad_agent_whatever_the_reference() {
        assert_eq!(validate("vim", &id(UUID)), Err(Refusal::BadAgent));
        assert_eq!(validate("", &id(UUID)), Err(Refusal::BadAgent));
        assert_eq!(validate("Claude", &id(UUID)), Err(Refusal::BadAgent));
    }

    #[test]
    fn an_id_is_one_to_128_bytes_of_the_charset() {
        assert_eq!(validate("claude", &id(&"a".repeat(128))), Ok(()));
        assert_eq!(
            validate("claude", &id(&"a".repeat(129))).map_err(Refusal::code),
            Err("bad_reference")
        );
        assert!(validate("claude", &id("")).is_err());
        for bad in ["abc\x07def", "a b", "a;b", "a'b", "a$b", "a/b", "é"] {
            assert!(validate("claude", &id(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_pi_and_omp_take_a_path() {
        for agent in ["claude", "codex", "opencode"] {
            assert!(
                validate(agent, &path("/home/u/s.jsonl")).is_err(),
                "{agent}"
            );
        }
        for agent in ["pi", "omp"] {
            assert_eq!(validate(agent, &path("/home/u/s.jsonl")), Ok(()), "{agent}");
            assert_eq!(validate(agent, &path("/home/u/with space.jsonl")), Ok(()));
        }
    }

    #[test]
    fn a_path_is_absolute_bounded_and_free_of_control_chars() {
        assert!(validate("pi", &path("relative/s.jsonl")).is_err());
        assert!(validate("pi", &path("/home/u/s\n.jsonl")).is_err());
        assert!(validate("pi", &path(&format!("/{}", "a".repeat(4096)))).is_err());
        assert_eq!(
            validate("pi", &path(&format!("/{}", "a".repeat(4095)))),
            Ok(())
        );
    }

    #[test]
    fn decide_stores_replaces_only_an_allowlisted_same_agent_report() {
        let a = AgentSessionRecord::new("claude", &id(UUID));
        let b = AgentSessionRecord::new("claude", &id("other"));
        assert_eq!(decide(None, &a, None), Ok(AgentSessionOutcome::Stored));
        assert_eq!(
            decide(Some(&a), &a, None),
            Ok(AgentSessionOutcome::Unchanged)
        );
        assert_eq!(
            decide(Some(&a), &b, Some("startup")),
            Err(Refusal::SessionHeld(Held::DifferentId))
        );
        assert_eq!(
            decide(Some(&a), &b, Some("clear")),
            Ok(AgentSessionOutcome::Replaced)
        );
        assert_eq!(
            decide(Some(&a), &b, None),
            Err(Refusal::SessionHeld(Held::DifferentId))
        );
    }

    #[test]
    fn another_agents_record_is_never_replaced_whatever_the_source() {
        let claude = AgentSessionRecord::new("claude", &id(UUID));
        let codex = AgentSessionRecord::new("codex", &id("other"));
        for source in [
            None,
            Some("startup"),
            Some("clear"),
            Some("resume"),
            Some("compact"),
        ] {
            assert_eq!(
                decide(Some(&claude), &codex, source),
                Err(Refusal::SessionHeld(Held::OtherAgent)),
                "{source:?}"
            );
        }
    }

    #[test]
    fn allows_replace_matches_the_table_row_by_row() {
        let sources = [
            "startup", "resume", "clear", "compact", "fork", "new", "select", "bogus",
        ];
        let table = [
            ("claude", &["resume", "clear", "compact", "fork"][..]),
            ("codex", &["startup", "resume", "clear", "compact"][..]),
            ("opencode", &["select"][..]),
            ("pi", &["resume", "fork", "new"][..]),
            ("omp", &["startup", "resume", "fork", "new"][..]),
            ("vim", &[][..]),
        ];
        for (agent, allowed) in table {
            for source in sources {
                assert_eq!(
                    allows_replace(agent, Some(source)),
                    allowed.contains(&source),
                    "({agent}, {source})"
                );
            }
            assert!(!allows_replace(agent, None), "({agent}, None)");
        }
    }

    fn record(agent: &str, reference: AgentSessionRef) -> AgentSessionRecord {
        AgentSessionRecord::new(agent, &reference)
    }

    #[test]
    fn plan_builds_each_agents_resume_command() {
        let cases = [
            (
                "claude",
                id(UUID),
                vec!["claude", "--resume=7f3c2a9e-51d4-4b8e-9a61-0c2f5d8e4b17"],
            ),
            ("codex", id(UUID), vec!["codex", "resume", "--", UUID]),
            (
                "opencode",
                id("ses_3f9a"),
                vec!["opencode", "--session=ses_3f9a"],
            ),
            (
                "pi",
                path("/home/u/s.jsonl"),
                vec!["pi", "--session", "/home/u/s.jsonl"],
            ),
            ("pi", id(UUID), vec!["pi", "--session", UUID]),
            (
                "omp",
                path("/home/u/s.jsonl"),
                vec!["omp", "--resume=/home/u/s.jsonl"],
            ),
            (
                "omp",
                id(UUID),
                vec!["omp", "--resume=7f3c2a9e-51d4-4b8e-9a61-0c2f5d8e4b17"],
            ),
        ];
        for (agent, reference, want) in cases {
            assert_eq!(
                plan(&record(agent, reference)),
                Some(want.iter().map(|w| w.to_string()).collect()),
                "{agent}"
            );
        }
        assert_eq!(plan(&record("vim", id(UUID))), None);
    }

    #[test]
    fn the_typed_line_is_space_prefixed_quoted_and_submitted() {
        assert_eq!(
            typed_line(&record("claude", id(UUID))).as_deref(),
            Some(format!(" claude --resume={UUID}\r").as_str())
        );
        assert_eq!(
            typed_line(&record("pi", path("/home/u/my sessions/it's.jsonl"))).as_deref(),
            Some(" pi --session '/home/u/my sessions/it'\\''s.jsonl'\r")
        );
        assert_eq!(
            typed_line(&record("omp", path("/home/u/a b.jsonl"))).as_deref(),
            Some(" omp '--resume=/home/u/a b.jsonl'\r")
        );
    }

    /// An id is typed after the agent's flag, so one that starts with `-`
    /// would be parsed by the agent as an option of its own:
    /// `claude --resume --dangerously-skip-permissions`.
    #[test]
    fn an_id_that_starts_with_a_dash_is_refused_for_every_agent() {
        for agent in ["claude", "codex", "opencode", "pi", "omp"] {
            for bad in [
                "--dangerously-skip-permissions",
                "--yolo",
                "-x",
                "-",
                ".hidden",
                "_x",
            ] {
                assert_eq!(
                    validate(agent, &id(bad)).map_err(Refusal::code),
                    Err("bad_reference"),
                    "({agent}, {bad:?})"
                );
            }
        }
    }

    /// A path must be absolute, so it starts with `/` and can never start
    /// with `-`; one with a backslash is refused because fish reads `\'`
    /// inside single quotes as an escape, which breaks the POSIX quoting.
    #[test]
    fn a_path_that_starts_with_a_dash_or_holds_a_backslash_is_refused() {
        for agent in ["pi", "omp"] {
            for bad in [
                "-rf/x.jsonl",
                "--session=/x",
                "/x\\' ; echo pwned ; echo \\'",
            ] {
                assert_eq!(
                    validate(agent, &path(bad)).map_err(Refusal::code),
                    Err("bad_reference"),
                    "({agent}, {bad:?})"
                );
            }
        }
    }

    /// Every argv `plan` builds keeps the value out of option position: joined
    /// to its flag, or after `--`. pi takes `--session <value>` because pi's
    /// own parser could not be checked here; its values cannot start with `-`.
    #[test]
    fn the_value_is_never_a_word_of_its_own_after_a_bare_flag() {
        let value = "abc";
        for agent in ["claude", "codex", "opencode", "omp"] {
            let argv = plan(&record(agent, id(value))).unwrap();
            let at = argv.iter().position(|w| w.ends_with(value)).unwrap();
            assert!(
                argv[at].contains('=') || argv[at - 1] == "--",
                "{agent}: {argv:?}"
            );
        }
    }

    #[test]
    fn dedupe_keeps_the_lowest_pane_per_session() {
        let a = AgentSessionRecord::new("claude", &id(UUID));
        let b = AgentSessionRecord::new("claude", &id("other"));
        let a_by_codex = AgentSessionRecord::new("codex", &id(UUID));
        let records = HashMap::from([
            (9, a.clone()),
            (3, a.clone()),
            (5, b.clone()),
            (7, a_by_codex.clone()),
        ]);
        let winners = dedupe(&records, &[]);
        let mut kept: Vec<u64> = winners.keys().copied().collect();
        kept.sort_unstable();
        assert_eq!(kept, vec![3, 5, 7]);
        assert_eq!(winners[&3], a);
    }

    #[test]
    fn dedupe_drops_what_a_live_pane_already_holds() {
        let a = AgentSessionRecord::new("claude", &id(UUID));
        let b = AgentSessionRecord::new("claude", &id("other"));
        let records = HashMap::from([(2, a.clone()), (4, b.clone())]);
        let winners = dedupe(&records, &[&a]);
        assert_eq!(winners, HashMap::from([(4, b)]));
    }
}
