//! `remux integrate <harness>`: install, report on, or remove what a coding
//! agent needs in its own files to work with remux.
//!
//! Run by a human on the machine the harness runs on, so failures exit
//! non-zero rather than following the hooks' exit-0 contract.

pub mod claude_messaging;
pub mod hooks_file;

use std::path::PathBuf;

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
