//! Claude Code's session restore integration: one `SessionStart` hook in
//! `settings.json` in Claude's config dir (`$CLAUDE_CONFIG_DIR`, else
//! `~/.claude`), running `remux agent session claude --hook` by this binary's
//! absolute path. It shares the file with `claude-messaging`, each owning
//! only its own events.
//!
//! The group has no `matcher`: the CLI reads the payload's `source` itself
//! and the server's allowlist decides what it may replace.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::hooks_file::{effective_uid, home_and_binary, HookSet, Hooks, HooksFile};
use super::{Change, Integration, IntegrationStatus};

pub static RESUME_CLAUDE: HookSet = HookSet {
    events: &[("SessionStart", "claude")],
    tail: |agent| format!(" agent session {agent} --hook || true"),
    by_name: &[("claude", "remux agent session claude --hook || true")],
    marker: " agent session claude",
    timeout_secs: crate::client::hook_cli::HOOK_TIMEOUT.as_secs(),
};

/// `remux integrate claude-resume`.
pub struct ClaudeResume {
    dir: PathBuf,
    hooks: HooksFile,
}

impl ClaudeResume {
    pub fn from_env(allow_dev_path: bool) -> Result<Self> {
        let (home, binary) = home_and_binary()?;
        Ok(Self::new(
            &super::env::claude_config_dir(&home),
            binary,
            allow_dev_path,
            effective_uid(),
        ))
    }

    pub fn new(dir: &Path, binary: PathBuf, allow_dev_path: bool, euid: u32) -> Self {
        Self {
            dir: dir.to_path_buf(),
            hooks: HooksFile::new(
                dir.join("settings.json"),
                Hooks::new(&RESUME_CLAUDE, binary),
                allow_dev_path,
                euid,
            ),
        }
    }
}

/// Refuse an install into an agent's config dir that is not there: the agent
/// creates it on its first run, and a dir remux made would say the agent is
/// set up when it is not.
pub fn require_dir(dir: &Path, agent: &str, variable: &str) -> Result<()> {
    anyhow::ensure!(
        dir.is_dir(),
        "{} does not exist: run {agent} once first, or point ${variable} at its config dir",
        dir.display()
    );
    Ok(())
}

impl Integration for ClaudeResume {
    fn name(&self) -> &'static str {
        "claude-resume"
    }

    fn status(&self) -> Result<IntegrationStatus> {
        self.hooks.status()
    }

    fn plan(&self) -> Result<(Vec<Change>, Vec<String>)> {
        require_dir(&self.dir, "claude", "CLAUDE_CONFIG_DIR")?;
        self.hooks.plan()
    }

    fn install(&self) -> Result<Vec<String>> {
        require_dir(&self.dir, "claude", "CLAUDE_CONFIG_DIR")?;
        self.hooks.install()
    }

    fn uninstall(&self) -> Result<Vec<String>> {
        self.hooks.uninstall()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BINARY: &str = "/usr/local/bin/remux";

    fn hooks() -> Hooks {
        Hooks::new(&RESUME_CLAUDE, PathBuf::from(BINARY))
    }

    #[test]
    fn the_command_is_the_quoted_binary_and_the_hook_verb() {
        assert_eq!(
            hooks().command("claude"),
            "'/usr/local/bin/remux' agent session claude --hook || true"
        );
    }

    #[test]
    fn install_beside_messaging_touches_only_session_start() {
        let mut settings = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "'/usr/local/bin/remux' msg hook claude-stop || true"}
        ]}]}});
        let before = settings["hooks"]["Stop"].clone();
        super::super::hooks_file::install_hooks(&mut settings, &hooks());
        assert_eq!(settings["hooks"]["Stop"], before);
        assert_eq!(
            settings["hooks"]["SessionStart"],
            json!([{"hooks": [{"type": "command",
                "command": "'/usr/local/bin/remux' agent session claude --hook || true",
                "timeout": 10}]}])
        );
        super::super::hooks_file::uninstall_hooks(&mut settings, &hooks());
        assert_eq!(settings, json!({"hooks": {"Stop": before}}));
    }

    #[test]
    fn a_wrapper_around_the_command_is_foreign_and_survives_uninstall() {
        let wrapper = "my-wrapper remux agent session claude --hook";
        let mut settings = json!({"hooks": {"SessionStart": [{"hooks": [
            {"type": "command", "command": wrapper}
        ]}]}});
        let before = settings.clone();
        super::super::hooks_file::uninstall_hooks(&mut settings, &hooks());
        assert_eq!(settings, before);
    }

    #[test]
    fn a_missing_config_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("claude");
        let resume = ClaudeResume::new(&gone, PathBuf::from(BINARY), true, effective_uid());
        assert!(resume.install().is_err());
        assert!(!gone.exists());
    }
}
