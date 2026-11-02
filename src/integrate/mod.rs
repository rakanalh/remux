//! `remux integrate <harness>`: install, report on, or remove what a coding
//! agent needs in its own files to work with remux.
//!
//! Run by a human on the machine the harness runs on, so failures exit
//! non-zero rather than following the hooks' exit-0 contract.

pub mod claude_messaging;
pub mod claude_resume;
pub mod codex_resume;
pub mod env;
pub mod hooks_file;
pub mod jsonc_file;
pub mod opencode_resume;
pub mod pi_omp_resume;
pub mod plugin_file;
pub mod toml_file;

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Whether a harness's hooks are in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationStatus {
    /// Exactly what this build installs; `runs` says what that runs.
    Installed {
        runs: String,
    },
    /// Remux's is there, but not what this build installs; or something of
    /// the user's is in its place.
    Stale {
        why: String,
    },
    Absent,
}

/// One file an install or uninstall would rewrite, before and after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
}

pub trait Integration {
    fn name(&self) -> &'static str;
    fn status(&self) -> Result<IntegrationStatus>;
    /// What [`Integration::install`] would change, without changing it, and
    /// its notes.
    fn plan(&self) -> Result<(Vec<Change>, Vec<String>)>;
    /// Install, returning a note for each thing deliberately left alone.
    fn install(&self) -> Result<Vec<String>>;
    /// Uninstall, returning a note for each thing deliberately left alone.
    fn uninstall(&self) -> Result<Vec<String>>;
    /// What the user must do by hand, printed on `--status` and after an
    /// install: remux never edits a shell's own files.
    fn instructions(&self) -> Option<String> {
        None
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
