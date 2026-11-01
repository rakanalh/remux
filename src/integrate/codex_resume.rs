//! Codex's session restore integration: one `SessionStart` hook in
//! `$CODEX_HOME/hooks.json` (Claude's shape), running `remux agent session
//! codex --hook` by this binary's absolute path, and `[features] hooks = true`
//! in `$CODEX_HOME/config.toml`, without which an older Codex never reads
//! `hooks.json`.
//!
//! Uninstall removes the hook and LEAVES the feature flag: other hooks may
//! rely on it, and turning a user's feature off is not remux's to do.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::claude_resume::require_dir;
use super::hooks_file::{
    effective_uid, home_and_binary, write_atomically, HookSet, Hooks, HooksFile,
};
use super::plugin_file::{read_for_edit, ForEdit};
use super::{Change, Integration, IntegrationStatus};

pub static RESUME_CODEX: HookSet = HookSet {
    events: &[("SessionStart", "codex")],
    tail: |agent| format!(" agent session {agent} --hook || true"),
    by_name: &[("codex", "remux agent session codex --hook || true")],
    marker: " agent session codex",
    timeout_secs: crate::client::hook_cli::HOOK_TIMEOUT.as_secs(),
};

const KEPT_NOTE: &str = "left `[features] hooks = true` in config.toml: other hooks may rely on it";

/// `remux integrate codex-resume`.
pub struct CodexResume {
    dir: PathBuf,
    hooks: HooksFile,
    config: PathBuf,
    euid: u32,
}

impl CodexResume {
    pub fn from_env(allow_dev_path: bool) -> Result<Self> {
        let (home, binary) = home_and_binary()?;
        Ok(Self::new(
            &super::env::codex_home(&home),
            binary,
            allow_dev_path,
            effective_uid(),
        ))
    }

    pub fn new(dir: &Path, binary: PathBuf, allow_dev_path: bool, euid: u32) -> Self {
        Self {
            dir: dir.to_path_buf(),
            hooks: HooksFile::new(
                dir.join("hooks.json"),
                Hooks::new(&RESUME_CODEX, binary),
                allow_dev_path,
                euid,
            ),
            config: dir.join("config.toml"),
            euid,
        }
    }

    fn read_config(&self) -> Result<ForEdit> {
        read_for_edit(&self.config, self.euid)
    }

    /// The config change an install makes, if any.
    fn config_change(&self) -> Result<(ForEdit, Option<String>)> {
        let config = self.read_config()?;
        let after = super::toml_file::ensure_features_hooks(config.text.as_deref().unwrap_or(""))
            .with_context(|| format!("editing {}", config.path.display()))?;
        Ok((config, after))
    }

    fn feature_on(&self) -> Result<bool> {
        super::hooks_file::check_regular(&self.config)?;
        let text = match std::fs::read_to_string(&self.config) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.config.display())),
        };
        let doc: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("{} is not valid TOML", self.config.display()))?;
        Ok(super::toml_file::features_hooks(&doc) == Some(true))
    }
}

impl Integration for CodexResume {
    fn name(&self) -> &'static str {
        "codex-resume"
    }

    fn status(&self) -> Result<IntegrationStatus> {
        let status = self.hooks.status()?;
        if matches!(status, IntegrationStatus::Installed { .. }) && !self.feature_on()? {
            return Ok(IntegrationStatus::Stale {
                why: format!(
                    "{} does not set `[features] hooks = true`",
                    self.config.display()
                ),
            });
        }
        Ok(status)
    }

    fn plan(&self) -> Result<(Vec<Change>, Vec<String>)> {
        require_dir(&self.dir, "codex", "CODEX_HOME")?;
        let (config, after) = self.config_change()?;
        let (mut changes, notes) = self.hooks.plan()?;
        if after.is_some() {
            changes.push(Change {
                path: config.path,
                before: config.text,
                after,
            });
        }
        Ok((changes, notes))
    }

    fn install(&self) -> Result<Vec<String>> {
        require_dir(&self.dir, "codex", "CODEX_HOME")?;
        self.hooks.check_binary()?;
        let (config, after) = self.config_change()?;
        if let Some(text) = after {
            write_atomically(&config.path, &text, config.fingerprint, Some(0o644))?;
        }
        self.hooks.install()
    }

    fn uninstall(&self) -> Result<Vec<String>> {
        let mut notes = self.hooks.uninstall()?;
        if self.feature_on().unwrap_or(false) {
            notes.push(KEPT_NOTE.to_string());
        }
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BINARY: &str = "/usr/local/bin/remux";

    #[test]
    fn the_command_is_the_quoted_binary_and_the_hook_verb() {
        let hooks = Hooks::new(&RESUME_CODEX, PathBuf::from(BINARY));
        assert_eq!(
            hooks.command("codex"),
            "'/usr/local/bin/remux' agent session codex --hook || true"
        );
    }

    #[test]
    fn a_missing_codex_home_is_refused_and_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("codex");
        let resume = CodexResume::new(&gone, PathBuf::from(BINARY), true, effective_uid());
        assert!(resume.install().is_err());
        assert!(!gone.exists());
    }
}
