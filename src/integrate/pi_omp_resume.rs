//! pi's and omp's session restore integration: one extension,
//! `extensions/remux-agent-session.ts` in the agent's dir, rendered from
//! `integrations/remux-agent-session.ts` with this binary's path and the
//! agent's name baked in.
//!
//! pi's dir is `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`; omp's is the same
//! variable, else `~/$PI_CONFIG_DIR/agent`. With the variable set both agents
//! resolve to one dir, and one extension there would report under one agent's
//! name for both: an install for either is refused then.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::claude_resume::require_dir;
use super::hooks_file::{effective_uid, home_and_binary};
use super::plugin_file::{js_string, render, PluginFile};
use super::{Change, Integration, IntegrationStatus};

pub const TEMPLATE: &str = include_str!("../../integrations/remux-agent-session.ts");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Pi,
    Omp,
}

impl Agent {
    fn name(self) -> &'static str {
        match self {
            Agent::Pi => "pi",
            Agent::Omp => "omp",
        }
    }
}

/// `remux integrate pi-resume` and `remux integrate omp-resume`.
pub struct PiOmpResume {
    agent: Agent,
    dir: PathBuf,
    other: PathBuf,
    file: PluginFile,
}

/// The extension as this build writes it for `agent`, running `binary`.
pub fn rendered(agent: Agent, binary: &Path) -> String {
    render(
        TEMPLATE,
        &[
            ("\"__REMUX_BIN__\"", &js_string(&binary.to_string_lossy())),
            ("__REMUX_AGENT__", agent.name()),
        ],
    )
}

impl PiOmpResume {
    pub fn from_env(agent: Agent, allow_dev_path: bool) -> Result<Self> {
        let (home, binary) = home_and_binary()?;
        let (pi, omp) = (
            super::env::pi_agent_dir(&home),
            super::env::omp_agent_dir(&home),
        );
        let (dir, other) = match agent {
            Agent::Pi => (pi, omp),
            Agent::Omp => (omp, pi),
        };
        Ok(Self::new(
            agent,
            &dir,
            &other,
            binary,
            allow_dev_path,
            effective_uid(),
        ))
    }

    pub fn new(
        agent: Agent,
        dir: &Path,
        other: &Path,
        binary: PathBuf,
        allow_dev_path: bool,
        euid: u32,
    ) -> Self {
        Self {
            agent,
            dir: dir.to_path_buf(),
            other: other.to_path_buf(),
            file: PluginFile::new(
                dir.join("extensions").join("remux-agent-session.ts"),
                "pi-omp-resume",
                rendered(agent, &binary),
                binary,
                allow_dev_path,
                euid,
            ),
        }
    }

    fn check(&self) -> Result<()> {
        let variable = match self.agent {
            Agent::Pi => "PI_CODING_AGENT_DIR",
            Agent::Omp => "PI_CONFIG_DIR",
        };
        require_dir(&self.dir, self.agent.name(), variable)?;
        let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => a == b,
        };
        anyhow::ensure!(
            !same(&self.dir, &self.other),
            "pi and omp both resolve to {}, so one extension there would report under one \
             agent's name for both: unset $PI_CODING_AGENT_DIR, or point one of them elsewhere",
            self.dir.display()
        );
        Ok(())
    }
}

impl Integration for PiOmpResume {
    fn name(&self) -> &'static str {
        match self.agent {
            Agent::Pi => "pi-resume",
            Agent::Omp => "omp-resume",
        }
    }

    fn status(&self) -> Result<IntegrationStatus> {
        self.file.status()
    }

    fn plan(&self) -> Result<(Vec<Change>, Vec<String>)> {
        self.check()?;
        Ok((self.file.plan()?.into_iter().collect(), Vec::new()))
    }

    fn install(&self) -> Result<Vec<String>> {
        self.check()?;
        self.file.install()?;
        Ok(Vec::new())
    }

    fn uninstall(&self) -> Result<Vec<String>> {
        self.file.uninstall()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rendering_bakes_the_binary_and_the_agent_in() {
        let text = rendered(Agent::Omp, Path::new("/opt/remux"));
        assert!(text.starts_with(
            "// managed by remux; REMUX_INTEGRATION_ID=pi-omp-resume REMUX_INTEGRATION_VERSION=1\n"
        ));
        assert!(text.contains("const REMUX_BIN = \"/opt/remux\";"));
        assert!(text.contains("const AGENT = \"omp\";"));
        assert!(text.contains("remux integrate omp-resume --uninstall"));
        assert!(!text.contains("__REMUX_"));
    }

    #[test]
    fn one_directory_for_both_agents_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let resume = PiOmpResume::new(
            Agent::Pi,
            dir.path(),
            dir.path(),
            std::env::current_exe().unwrap(),
            true,
            effective_uid(),
        );
        assert!(resume.install().is_err());
        assert!(!dir.path().join("extensions").exists());
    }
}
