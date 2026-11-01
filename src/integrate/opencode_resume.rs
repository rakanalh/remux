//! OpenCode's session restore integration, for OpenCode 1.18, in its config
//! dir (`$OPENCODE_CONFIG_DIR`, else `$XDG_CONFIG_HOME/opencode`, else
//! `~/.config/opencode`):
//!
//! - `plugins/remux-agent-session.js`, a server plugin OpenCode discovers by
//!   itself, which reports for `opencode run` and `opencode --mini`;
//! - `remux-tui-session.js`, a TUI plugin, listed by absolute path in the
//!   `plugin` array of `tui.jsonc` if there is one, else `tui.json`, created
//!   when neither exists. It is kept out of `plugins/`: OpenCode loads every
//!   file there as a SERVER plugin and logs a load failure for one without
//!   `server()`, on every start.
//!
//! OpenCode writes its own files into the dir (`package.json`,
//! `node_modules/`, `.gitignore`); none of them is touched.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::hooks_file::{effective_uid, home_and_binary, shown, write_atomically};
use super::jsonc_file::{add_plugin, has_plugin, new_tui_json, remove_plugin, TUI_KEY};
use super::plugin_file::{js_string, read_for_edit, render, PluginFile};
use super::require_dir;
use super::{Change, Integration, IntegrationStatus};

pub const SERVER_PLUGIN: &str = include_str!("../../integrations/opencode/remux-agent-session.js");
pub const TUI_PLUGIN: &str = include_str!("../../integrations/opencode/remux-tui-session.js");

/// `remux integrate opencode-resume`.
pub struct OpencodeResume {
    dir: PathBuf,
    server: PluginFile,
    tui: PluginFile,
    euid: u32,
}

fn rendered(template: &str, binary: &Path) -> String {
    render(
        template,
        &[("\"__REMUX_BIN__\"", &js_string(&binary.to_string_lossy()))],
    )
}

impl OpencodeResume {
    pub fn from_env(allow_dev_path: bool) -> Result<Self> {
        let (home, binary) = home_and_binary()?;
        Ok(Self::new(
            &super::env::opencode_config_dir(&home),
            binary,
            allow_dev_path,
            effective_uid(),
        ))
    }

    pub fn new(dir: &Path, binary: PathBuf, allow_dev_path: bool, euid: u32) -> Self {
        let file = |path: PathBuf, id, template| {
            PluginFile::new(
                path,
                id,
                rendered(template, &binary),
                binary.clone(),
                allow_dev_path,
                euid,
            )
        };
        Self {
            dir: dir.to_path_buf(),
            server: file(
                dir.join("plugins").join("remux-agent-session.js"),
                "opencode-resume-server",
                SERVER_PLUGIN,
            ),
            tui: file(
                dir.join("remux-tui-session.js"),
                "opencode-resume-tui",
                TUI_PLUGIN,
            ),
            euid,
        }
    }

    /// The TUI config this install edits: `tui.jsonc` when it exists, else
    /// `tui.json`, which is created when it does not.
    fn tui_config(&self) -> PathBuf {
        let jsonc = self.dir.join("tui.jsonc");
        if jsonc.exists() {
            jsonc
        } else {
            self.dir.join("tui.json")
        }
    }

    fn entry(&self) -> String {
        self.tui.path().to_string_lossy().into_owned()
    }

    /// The TUI config after the install, if it changes.
    fn registration(&self) -> Result<Option<Change>> {
        let read = read_for_edit(&self.tui_config(), self.euid)?;
        let after = match &read.text {
            Some(text) => add_plugin(text, TUI_KEY, &self.entry())
                .with_context(|| format!("editing {}", shown(&read.path)))?,
            None => Some(new_tui_json(&self.entry())),
        };
        Ok(after.map(|after| Change {
            path: read.path,
            before: read.text,
            after: Some(after),
        }))
    }

    fn registered(&self) -> Result<bool> {
        let path = self.tui_config();
        super::hooks_file::check_regular(&path)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => has_plugin(&text, TUI_KEY, &self.entry())
                .with_context(|| format!("reading {}", shown(&path))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

impl Integration for OpencodeResume {
    fn name(&self) -> &'static str {
        "opencode-resume"
    }

    fn status(&self) -> Result<IntegrationStatus> {
        let server = self.server.status()?;
        let tui = self.tui.status()?;
        let registered = self.registered()?;
        Ok(match (&server, &tui, registered) {
            (IntegrationStatus::Absent, IntegrationStatus::Absent, false) => {
                IntegrationStatus::Absent
            }
            (IntegrationStatus::Installed { runs }, IntegrationStatus::Installed { .. }, true) => {
                IntegrationStatus::Installed { runs: runs.clone() }
            }
            _ => IntegrationStatus::Stale {
                why: [
                    match server {
                        IntegrationStatus::Installed { .. } => None,
                        IntegrationStatus::Stale { why } => Some(why),
                        IntegrationStatus::Absent => {
                            Some(format!("{} is missing", shown(self.server.path())))
                        }
                    },
                    match tui {
                        IntegrationStatus::Installed { .. } => None,
                        IntegrationStatus::Stale { why } => Some(why),
                        IntegrationStatus::Absent => {
                            Some(format!("{} is missing", shown(self.tui.path())))
                        }
                    },
                    (!registered).then(|| {
                        format!("{} does not list the TUI plugin", shown(&self.tui_config()))
                    }),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; "),
            },
        })
    }

    fn plan(&self) -> Result<(Vec<Change>, Vec<String>)> {
        require_dir(&self.dir, "opencode", "OPENCODE_CONFIG_DIR")?;
        let changes = [self.server.plan()?, self.tui.plan()?, self.registration()?]
            .into_iter()
            .flatten()
            .collect();
        Ok((changes, Vec::new()))
    }

    fn install(&self) -> Result<Vec<String>> {
        require_dir(&self.dir, "opencode", "OPENCODE_CONFIG_DIR")?;
        // Every refusal is found before the first write: a plugin file that is
        // not remux's, or a TUI config that is not JSON.
        self.server.plan()?;
        self.tui.plan()?;
        let registration = self.registration()?;
        self.server.install()?;
        self.tui.install()?;
        if let Some(change) = registration {
            let read = read_for_edit(&change.path, self.euid)?;
            let text = change.after.unwrap_or_default();
            write_atomically(&read.path, &text, read.fingerprint, Some(0o644))?;
        }
        Ok(Vec::new())
    }

    /// The TUI config's entry goes first: a failure there leaves both files
    /// in place, never a config listing a plugin that is gone.
    fn uninstall(&self) -> Result<Vec<String>> {
        let read = read_for_edit(&self.tui_config(), self.euid)?;
        if let Some(text) = &read.text {
            if let Some(after) = remove_plugin(text, TUI_KEY, &self.entry())
                .with_context(|| format!("editing {}", shown(&read.path)))?
            {
                write_atomically(&read.path, &after, read.fingerprint, None)?;
            }
        }
        let mut notes = self.server.uninstall()?;
        notes.extend(self.tui.uninstall()?);
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_templates_render_the_binary_and_carry_their_own_header() {
        for (template, id) in [
            (SERVER_PLUGIN, "opencode-resume-server"),
            (TUI_PLUGIN, "opencode-resume-tui"),
        ] {
            let text = rendered(template, Path::new("/opt/remux"));
            assert!(super::super::plugin_file::is_ours(&text, id), "{id}");
            assert!(text.contains("const REMUX_BIN = \"/opt/remux\";"), "{id}");
            assert!(!text.contains("__REMUX_"), "{id}");
        }
    }

    #[test]
    fn a_missing_config_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("opencode");
        let resume = OpencodeResume::new(
            &gone,
            std::env::current_exe().unwrap(),
            true,
            effective_uid(),
        );
        assert!(resume.install().is_err());
        assert!(!gone.exists());
    }
}
