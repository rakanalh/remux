//! Claude Code's messaging hooks: `Stop` and `UserPromptSubmit` in
//! `$HOME/.claude/settings.json`, running `remux msg hook claude-stop` and
//! `remux msg hook claude-prompt` by this binary's absolute path.

use anyhow::Result;

use super::hooks_file::{effective_uid, home_and_binary, HookSet, Hooks, HooksFile};
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

/// `remux integrate claude-messaging`.
pub struct ClaudeMessaging {
    hooks: HooksFile,
}

impl ClaudeMessaging {
    /// For `$HOME/.claude/settings.json`, writing hooks that run THIS binary
    /// by its canonical path, as this process's user.
    pub fn from_env(allow_dev_path: bool) -> Result<Self> {
        let (home, binary) = home_and_binary()?;
        Ok(Self {
            hooks: HooksFile::new(
                home.join(".claude").join("settings.json"),
                Hooks::new(&MESSAGING, binary),
                allow_dev_path,
                effective_uid(),
            ),
        })
    }
}

impl Integration for ClaudeMessaging {
    fn name(&self) -> &'static str {
        "claude-messaging"
    }

    fn status(&self) -> Result<IntegrationStatus> {
        self.hooks.status()
    }

    fn plan(&self) -> Result<(Vec<Change>, Vec<String>)> {
        self.hooks.plan()
    }

    fn install(&self) -> Result<Vec<String>> {
        self.hooks.install()
    }

    fn uninstall(&self) -> Result<Vec<String>> {
        self.hooks.uninstall()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrate::hooks_file::{install_hooks, status_of};
    use serde_json::json;
    use std::path::PathBuf;

    fn hooks() -> Hooks {
        Hooks::new(&MESSAGING, PathBuf::from("/opt/remux/bin/remux"))
    }

    /// The installed spelling is exactly what tests/pty/msg_hooks.py writes
    /// by hand: the hook command and its timeout.
    #[test]
    fn the_messaging_hooks_are_spelled_as_the_hook_command_expects() {
        let h = hooks();
        assert_eq!(
            h.command("claude-stop"),
            "'/opt/remux/bin/remux' msg hook claude-stop || true"
        );
        assert_eq!(
            h.command("claude-prompt"),
            "'/opt/remux/bin/remux' msg hook claude-prompt || true"
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
}
