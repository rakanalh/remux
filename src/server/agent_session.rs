//! The session an agent in a pane reports it is running, kept so that a
//! restore can type the agent's resume command into the respawned shell.
//!
//! Pure: validation and the replacement rule, with no I/O, so that the daemon
//! applies them under the `panes` lock and the restore path can apply them
//! again to a record read back from an edited `state.json`.

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

fn valid_id(id: &str) -> bool {
    (1..=MAX_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'@' | b'-'))
}

fn valid_path(path: &str) -> bool {
    path.len() <= MAX_PATH && path.starts_with('/') && !path.chars().any(char::is_control)
}

/// Whether a report of `reference` from `agent` may be kept at all. The id
/// charset admits UUIDs and OpenCode's `ses_...` ids and nothing a shell would
/// treat specially, because the restore types the value into a shell.
pub fn validate(agent: &str, reference: &AgentSessionRef) -> Result<(), Refusal> {
    let takes_paths = takes_paths(agent).ok_or(Refusal::BadAgent)?;
    match reference {
        AgentSessionRef::Id(id) if valid_id(id) => Ok(()),
        AgentSessionRef::Id(_) => Err(Refusal::BadReference(
            "an id is 1 to 128 bytes of [A-Za-z0-9._:@-]",
        )),
        AgentSessionRef::Path(_) if !takes_paths => Err(Refusal::BadReference(
            "this agent reports an id, not a path",
        )),
        AgentSessionRef::Path(path) if valid_path(path) => Ok(()),
        AgentSessionRef::Path(_) => Err(Refusal::BadReference(
            "a path is absolute, at most 4096 bytes, with no control characters",
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

/// The command that resumes `record`'s session, one argv word each. `None`
/// for an agent this server does not know, which [`validate`] refuses.
pub fn plan(record: &AgentSessionRecord) -> Option<Vec<String>> {
    let value = record.value.clone();
    let words = match record.agent.as_str() {
        "claude" => vec!["claude".into(), "--resume".into(), value],
        "codex" => vec!["codex".into(), "resume".into(), value],
        // omp has no `--session`, and takes the value only joined to the flag.
        "omp" => vec!["omp".into(), format!("--resume={value}")],
        "pi" => vec!["pi".into(), "--session".into(), value],
        "opencode" => vec!["opencode".into(), "--session".into(), value],
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
        .map(|w| crate::integrate::hooks_file::sh_word(w))
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
            ("claude", id(UUID), vec!["claude", "--resume", UUID]),
            ("codex", id(UUID), vec!["codex", "resume", UUID]),
            (
                "opencode",
                id("ses_3f9a"),
                vec!["opencode", "--session", "ses_3f9a"],
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
            Some(format!(" claude --resume {UUID}\r").as_str())
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
}
