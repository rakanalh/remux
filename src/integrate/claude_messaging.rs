//! Claude Code's messaging integration: the `Stop` and `UserPromptSubmit`
//! hooks in `$HOME/.claude/settings.json`, running `remux msg hook
//! claude-stop` and `remux msg hook claude-prompt` by this binary's absolute
//! path, and the remux-messaging skill at
//! `$HOME/.claude/skills/remux-messaging/SKILL.md`.
//!
//! The binary knows only its own build's skill text. A skill file that
//! differs from it may be an older build's or the user's edit, and the two
//! cannot be told apart, so either is left alone and named, never replaced.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::hooks_file::{
    check_directory, check_owner, effective_uid, home_and_binary, shown, write_atomically, HookSet,
    Hooks, HooksFile,
};
use super::{Change, Integration, IntegrationStatus};

/// The hooks the messaging integration owns. The timeout is the one the hook
/// command itself is built to finish inside, so the two cannot drift apart.
pub static MESSAGING: HookSet = HookSet {
    events: &[
        ("Stop", "claude-stop"),
        ("UserPromptSubmit", "claude-prompt"),
    ],
    tail: |verb| format!(" msg hook {verb} || true"),
    // The skill's by-hand snippet is one skipped `realpath` away from these.
    by_name: &[
        ("claude-stop", "remux msg hook claude-stop || true"),
        ("claude-prompt", "remux msg hook claude-prompt || true"),
    ],
    // As tight as a remux spelling allows, so an unrelated hook that happens
    // to say "msg hook" is the user's and does not block an install.
    marker: " msg hook claude-",
    timeout_secs: crate::client::hook_cli::HOOK_TIMEOUT.as_secs(),
};

/// The skill this build installs, byte for byte.
pub const SKILL: &str = include_str!("../../skills/remux-messaging/SKILL.md");

/// The skill file as it is on disk, against [`SKILL`].
#[derive(Debug, PartialEq, Eq)]
enum Skill {
    Installed,
    Differs,
    Absent,
}

/// The skill path as `--status` reads it. A symlink to nothing reads as
/// absent, but writing the skill would replace the link with a file, so it is
/// not a [`Skill`]: the commands that write or remove the skill refuse it.
#[derive(Debug, PartialEq, Eq)]
enum OnDisk {
    Skill(Skill),
    Dangling { to: PathBuf },
}

/// `remux integrate claude-messaging`.
pub struct ClaudeMessaging {
    hooks: HooksFile,
    skill: PathBuf,
    euid: u32,
}

impl ClaudeMessaging {
    /// For `$HOME/.claude`, writing hooks that run THIS binary by its
    /// canonical path, as this process's user.
    pub fn from_env(allow_dev_path: bool) -> Result<Self> {
        let (home, binary) = home_and_binary()?;
        Ok(Self::new(&home, binary, allow_dev_path, effective_uid()))
    }

    /// For `home/.claude`, writing hooks that run `binary`, as the user
    /// `euid`.
    pub fn new(home: &Path, binary: PathBuf, allow_dev_path: bool, euid: u32) -> Self {
        let claude = home.join(".claude");
        Self {
            hooks: HooksFile::new(
                claude.join("settings.json"),
                Hooks::new(&MESSAGING, binary),
                allow_dev_path,
                euid,
            ),
            skill: claude
                .join("skills")
                .join("remux-messaging")
                .join("SKILL.md"),
            euid,
        }
    }

    fn on_disk(&self) -> Result<OnDisk> {
        match std::fs::read(&self.skill) {
            Ok(bytes) if bytes == SKILL.as_bytes() => Ok(OnDisk::Skill(Skill::Installed)),
            Ok(_) => Ok(OnDisk::Skill(Skill::Differs)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(match std::fs::read_link(&self.skill) {
                    Ok(to) => OnDisk::Dangling { to },
                    Err(_) => OnDisk::Skill(Skill::Absent),
                })
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", self.skill.display())),
        }
    }

    /// The skill's state for a command that writes or removes it.
    fn skill(&self) -> Result<Skill> {
        match self.on_disk()? {
            OnDisk::Skill(skill) => Ok(skill),
            OnDisk::Dangling { to } => anyhow::bail!("{}", self.dangling_why(&to)),
        }
    }

    fn dangling_why(&self, to: &Path) -> String {
        format!(
            "{} is a symlink to {}, which does not exist; replacing it would destroy the \
             link: create the file it names, or remove the link, and re-run",
            shown(&self.skill),
            shown(to)
        )
    }

    /// Before the skill is written or removed: the same ownership rules as
    /// the settings file, checked BEFORE the hooks are touched, so a refusal
    /// leaves neither half changed.
    fn check_skill_path(&self) -> Result<()> {
        check_owner(&self.skill, self.euid)?;
        check_directory(&self.skill, self.euid)
    }

    fn differs_note(&self) -> String {
        format!(
            "the skill at {} differs from this build's; left alone: remove it and re-run \
             install to replace it",
            shown(&self.skill)
        )
    }
}

impl Integration for ClaudeMessaging {
    fn name(&self) -> &'static str {
        "claude-messaging"
    }

    /// One line for both halves: the hooks' state, and the skill's beside it.
    fn status(&self) -> Result<IntegrationStatus> {
        let hooks = self.hooks.status()?;
        let skill = self.on_disk()?;
        let path = shown(&self.skill);
        let skill_why = match &skill {
            OnDisk::Skill(Skill::Installed) => None,
            OnDisk::Dangling { to } => Some(self.dangling_why(to)),
            OnDisk::Skill(Skill::Absent) => Some(format!("the skill is absent from {path}")),
            OnDisk::Skill(Skill::Differs) => Some(format!(
                "the skill at {path} differs from this build; remove it and re-run install \
                 to replace it, or keep your edit"
            )),
        };
        let joined = |why: String| match &skill_why {
            Some(skill) => format!("{why}; {skill}"),
            None => why,
        };
        Ok(match hooks {
            IntegrationStatus::Installed { runs } => match skill_why {
                None => IntegrationStatus::Installed {
                    runs: format!("{runs}; the skill is at {path}"),
                },
                Some(why) => IntegrationStatus::Stale { why },
            },
            IntegrationStatus::Stale { why } => IntegrationStatus::Stale { why: joined(why) },
            IntegrationStatus::Absent if skill == OnDisk::Skill(Skill::Absent) => {
                IntegrationStatus::Absent
            }
            IntegrationStatus::Absent => IntegrationStatus::Stale {
                why: joined(
                    MESSAGING
                        .events
                        .iter()
                        .map(|(event, _)| format!("{event} has no remux hook"))
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
            },
        })
    }

    /// The hooks' change, and the skill file written in full when it is
    /// absent: what will be written, since a summary of it is the one thing a
    /// dry run cannot show.
    fn plan(&self) -> Result<(Vec<Change>, Vec<String>)> {
        self.check_skill_path()?;
        let (mut changes, mut notes) = self.hooks.plan()?;
        match self.skill()? {
            Skill::Absent => changes.push(Change {
                path: self.skill.clone(),
                before: None,
                after: Some(SKILL.to_string()),
            }),
            Skill::Differs => notes.push(self.differs_note()),
            Skill::Installed => {}
        }
        Ok((changes, notes))
    }

    fn install(&self) -> Result<Vec<String>> {
        self.check_skill_path()?;
        let skill = self.skill()?;
        let mut notes = self.hooks.install()?;
        match skill {
            Skill::Absent => write_atomically(&self.skill, SKILL, None, Some(0o644))?,
            Skill::Differs => notes.push(self.differs_note()),
            Skill::Installed => {}
        }
        Ok(notes)
    }

    /// Removes the hooks first, because they are what runs code, and a skill
    /// path that is refused must not keep them in place. Then removes this
    /// build's skill, and its directory once that is empty; a skill that
    /// differs is left, and `skills/` is never touched.
    fn uninstall(&self) -> Result<Vec<String>> {
        let mut notes = self.hooks.uninstall()?;
        let skill = match self.check_skill_path().and_then(|()| self.skill()) {
            Ok(skill) => skill,
            Err(e) => {
                let also: String = notes.iter().map(|n| format!("; {n}")).collect();
                return Err(e.context(format!(
                    "the hooks were removed, but the skill was left alone{also}"
                )));
            }
        };
        match skill {
            Skill::Installed => {
                std::fs::remove_file(&self.skill)
                    .with_context(|| format!("removing {}", self.skill.display()))?;
                if let Some(dir) = self.skill.parent() {
                    // Only an empty directory goes; anything left in it is the
                    // user's.
                    let _ = std::fs::remove_dir(dir);
                }
            }
            Skill::Differs => notes.push(format!(
                "the skill at {} is not this build's; left alone",
                shown(&self.skill)
            )),
            Skill::Absent => {}
        }
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrate::hooks_file::{install_hooks, status_of};
    use serde_json::json;
    use std::path::PathBuf;

    /// Absent, under a first component nobody creates, so `/` is the only
    /// directory the install's binary check reads and the tests do not
    /// depend on the host's other directories (GitHub's runner has a 777
    /// `/opt`). See `hooks_file`'s test `hooks()`.
    const BINARY: &str = "/remux-test-absent/remux/bin/remux";

    fn hooks() -> Hooks {
        Hooks::new(&MESSAGING, PathBuf::from(BINARY))
    }

    /// The installed spelling is exactly what tests/pty/msg_hooks.py writes
    /// by hand: the hook command and its timeout.
    #[test]
    fn the_messaging_hooks_are_spelled_as_the_hook_command_expects() {
        let h = hooks();
        assert_eq!(
            h.command("claude-stop"),
            "'/remux-test-absent/remux/bin/remux' msg hook claude-stop || true"
        );
        assert_eq!(
            h.command("claude-prompt"),
            "'/remux-test-absent/remux/bin/remux' msg hook claude-prompt || true"
        );
        assert_eq!(MESSAGING.timeout_secs, 10);
    }

    /// A user's hook that merely says "msg hook" is the user's: it does not
    /// stop remux's being added beside it.
    #[test]
    fn an_unrelated_msg_hook_is_the_users_and_blocks_nothing() {
        let mut v = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "slack-msg hook-runner"}
        ]}]}});
        let notes = install_hooks(&mut v, &hooks());
        assert!(notes.is_empty(), "{notes:?}");
        let stops = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stops.len(), 2, "theirs, then ours");
        assert!(matches!(
            status_of(&v, &hooks()),
            IntegrationStatus::Installed { .. }
        ));
    }

    /// The integration under a throwaway `home`, never the real `$HOME`.
    fn in_home(home: &Path) -> ClaudeMessaging {
        ClaudeMessaging::new(home, PathBuf::from(BINARY), true, effective_uid())
    }

    fn skill_path(home: &Path) -> PathBuf {
        home.join(".claude/skills/remux-messaging/SKILL.md")
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The same length as the shipped skill, one byte different: only a
    /// byte comparison tells it apart.
    fn edited_skill() -> String {
        let mut bytes = SKILL.as_bytes().to_vec();
        let at = bytes.iter().position(|&b| b == b'a').unwrap();
        bytes[at] = b'b';
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn install_writes_the_skill_readable_and_status_names_both() {
        let home = tempfile::tempdir().unwrap();
        let it = in_home(home.path());
        assert_eq!(it.status().unwrap(), IntegrationStatus::Absent);
        assert!(it.install().unwrap().is_empty());
        let skill = skill_path(home.path());
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), SKILL);
        assert_eq!(mode(&skill), 0o644);
        match it.status().unwrap() {
            IntegrationStatus::Installed { runs } => assert!(
                runs.contains(BINARY)
                    && runs.contains(&format!("the skill is at {}", skill.display())),
                "{runs}"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_differing_skill_is_named_and_left_alone_by_install_and_uninstall() {
        let home = tempfile::tempdir().unwrap();
        let it = in_home(home.path());
        let skill = skill_path(home.path());
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        let edited = edited_skill();
        assert_eq!(edited.len(), SKILL.len());
        std::fs::write(&skill, &edited).unwrap();

        let notes = it.install().unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("differs from this build's")),
            "{notes:?}"
        );
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), edited);
        match it.status().unwrap() {
            IntegrationStatus::Stale { why } => assert!(
                why.contains("differs from this build") && why.contains("remove it"),
                "{why}"
            ),
            other => panic!("{other:?}"),
        }
        let notes = it.uninstall().unwrap();
        assert!(
            notes.iter().any(|n| n.contains("not this build's")),
            "{notes:?}"
        );
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), edited);
    }

    #[test]
    fn uninstall_removes_this_builds_skill_and_its_empty_directory_only() {
        let home = tempfile::tempdir().unwrap();
        let it = in_home(home.path());
        it.install().unwrap();
        let skill = skill_path(home.path());
        it.uninstall().unwrap();
        assert!(!skill.exists());
        assert!(!skill.parent().unwrap().exists());
        assert!(home.path().join(".claude/skills").is_dir(), "skills/ stays");
        assert_eq!(it.status().unwrap(), IntegrationStatus::Absent);
    }

    #[test]
    fn a_skill_with_no_hooks_beside_it_is_stale() {
        let home = tempfile::tempdir().unwrap();
        let it = in_home(home.path());
        it.install().unwrap();
        std::fs::remove_file(home.path().join(".claude/settings.json")).unwrap();
        match it.status().unwrap() {
            IntegrationStatus::Stale { why } => assert!(
                why.contains("Stop has no remux hook")
                    && why.contains("UserPromptSubmit has no remux hook"),
                "{why}"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// A `skills` directory that is a link into a directory another user owns
    /// is refused, and neither the skill nor the hooks are written.
    #[test]
    fn a_skills_directory_linked_to_another_users_is_refused() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".claude")).unwrap();
        std::os::unix::fs::symlink("/etc", home.path().join(".claude/skills")).unwrap();
        let err = in_home(home.path()).install().unwrap_err();
        assert!(format!("{err:#}").contains("belongs to uid 0"), "{err:#}");
        assert!(!home.path().join(".claude/settings.json").exists());
        let err = in_home(home.path()).plan().unwrap_err();
        assert!(
            format!("{err:#}").contains("belongs to uid 0"),
            "a dry run fails as the install would: {err:#}"
        );
    }

    /// The hooks are what run code: a skill path that cannot be used must not
    /// stop them being removed. The refusal is still an error, naming both.
    #[test]
    fn uninstall_removes_the_hooks_even_when_the_skill_path_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let it = in_home(home.path());
        it.install().unwrap();
        std::fs::remove_dir_all(home.path().join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink("/etc", home.path().join(".claude/skills")).unwrap();
        let err = it.uninstall().unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("hooks were removed") && text.contains("belongs to uid 0"),
            "{text}"
        );
        let settings = std::fs::read_to_string(home.path().join(".claude/settings.json")).unwrap();
        assert!(!settings.contains("msg hook"), "{settings}");
    }

    /// A SKILL.md that is a link to nothing is refused, as settings.json is:
    /// writing "through" it would create a file wherever it points, and
    /// replacing it would destroy the link.
    #[test]
    fn a_dangling_skill_symlink_is_refused_and_kept() {
        let home = tempfile::tempdir().unwrap();
        let skill = skill_path(home.path());
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        let gone = home.path().join("dotfiles/SKILL.md");
        std::os::unix::fs::symlink(&gone, &skill).unwrap();
        let err = in_home(home.path()).install().unwrap_err();
        assert!(
            format!("{err:#}").contains("which does not exist"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_link(&skill).unwrap(), gone);
        assert!(!gone.exists());
        assert!(!home.path().join(".claude/settings.json").exists());
    }

    /// Status reports the dangling link rather than failing: it is the
    /// command a user runs to find out what is wrong.
    #[test]
    fn status_reports_a_dangling_skill_symlink_as_stale() {
        let home = tempfile::tempdir().unwrap();
        let it = in_home(home.path());
        it.install().unwrap();
        let skill = skill_path(home.path());
        std::fs::remove_file(&skill).unwrap();
        let gone = home.path().join("dotfiles/SKILL.md");
        std::os::unix::fs::symlink(&gone, &skill).unwrap();
        match it.status().unwrap() {
            IntegrationStatus::Stale { why } => assert!(
                why.contains(&format!("is a symlink to {}", gone.display()))
                    && why.contains("which does not exist"),
                "{why}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(it.uninstall().is_err(), "uninstall still refuses");
        assert_eq!(std::fs::read_link(&skill).unwrap(), gone);
    }
}
