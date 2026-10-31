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
    SessionHeld,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::BadAgent => "bad_agent",
            Refusal::BadReference(_) => "bad_reference",
            Refusal::SessionHeld => "session_held",
        }
    }

    pub fn message(self) -> String {
        match self {
            Refusal::BadAgent => {
                "unknown agent: expected claude, codex, opencode, pi or omp".into()
            }
            Refusal::BadReference(why) => format!("bad session reference: {why}"),
            Refusal::SessionHeld => "the pane already holds a different agent session".into(),
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

/// What a valid report does to a pane that holds `held`.
pub fn decide(
    held: Option<&AgentSessionRecord>,
    report: &AgentSessionRecord,
) -> Result<AgentSessionOutcome, Refusal> {
    match held {
        None => Ok(AgentSessionOutcome::Stored),
        Some(held) if held == report => Ok(AgentSessionOutcome::Unchanged),
        Some(_) => Err(Refusal::SessionHeld),
    }
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
    fn decide_stores_on_an_empty_pane_and_refuses_any_other_held_record() {
        let a = AgentSessionRecord::new("claude", &id(UUID));
        let b = AgentSessionRecord::new("claude", &id("other"));
        let c = AgentSessionRecord::new("codex", &id(UUID));
        assert_eq!(decide(None, &a), Ok(AgentSessionOutcome::Stored));
        assert_eq!(decide(Some(&a), &a), Ok(AgentSessionOutcome::Unchanged));
        assert_eq!(decide(Some(&a), &b), Err(Refusal::SessionHeld));
        assert_eq!(decide(Some(&a), &c), Err(Refusal::SessionHeld));
    }
}
