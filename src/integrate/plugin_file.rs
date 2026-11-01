//! A whole file remux owns inside an agent's config dir: a pi/omp extension or
//! an OpenCode plugin, rendered from a template this build carries.
//!
//! The file is compared BYTE FOR BYTE with this build's rendering. A file at
//! the path whose first line is not remux's header is someone else's: it is
//! refused and left alone, because it cannot be told apart from the user's
//! own plugin of the same name. Remux's header with different bytes is an
//! older build's file, or one the user edited: Stale, rewritten by install
//! and removed by uninstall, since the header says remux rewrites it.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::hooks_file::{
    check_binary_safe, check_directory, check_hook_binary, check_owner, check_plain_file,
    check_regular, shown, write_atomically, Fingerprint,
};
use super::{Change, IntegrationStatus};

/// A file remux edits but does not own (a config file), as read for an edit:
/// the path written to, its text, and the fingerprint the write checks.
pub struct ForEdit {
    pub path: PathBuf,
    pub text: Option<String>,
    pub fingerprint: Option<Fingerprint>,
}

/// `path`, or the file it links to, so a link is written through rather than
/// replaced; a link to nothing is refused.
fn resolve(path: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::ensure!(
                std::fs::symlink_metadata(path).is_err(),
                "{} is a symlink to a file that does not exist; create the file it names, \
                 or remove the link, and re-run",
                path.display()
            );
            Ok(path.to_path_buf())
        }
        Err(e) => Err(e).with_context(|| format!("resolving {}", path.display())),
    }
}

/// Read `path` for an edit, through the ownership and plain-file checks.
pub fn read_for_edit(path: &Path, euid: u32) -> Result<ForEdit> {
    let path = resolve(path)?;
    check_owner(&path, euid)?;
    check_directory(&path, euid)?;
    check_plain_file(&path)?;
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ForEdit {
                path,
                text: None,
                fingerprint: None,
            })
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let fingerprint = Fingerprint::of(&file.metadata()?);
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(ForEdit {
        path,
        text: Some(text),
        fingerprint: Some(fingerprint),
    })
}

/// What every remux-managed plugin file starts with.
pub const HEADER: &str = "// managed by remux; REMUX_INTEGRATION_ID=";

/// `template` with each `(placeholder, value)` replaced.
pub fn render(template: &str, values: &[(&str, &str)]) -> String {
    values
        .iter()
        .fold(template.to_string(), |text, (placeholder, value)| {
            text.replace(placeholder, value)
        })
}

/// `value` as a JavaScript string literal, quotes included.
pub fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// Whether `text` carries remux's header for the integration `id`.
pub fn is_ours(text: &str, id: &str) -> bool {
    text.lines()
        .next()
        .and_then(|line| line.strip_prefix(HEADER))
        .and_then(|rest| rest.split_whitespace().next())
        .is_some_and(|found| found == id)
}

#[derive(Debug, PartialEq, Eq)]
enum OnDisk {
    Installed,
    Stale,
    Foreign,
    Absent,
}

/// One remux-owned plugin file.
pub struct PluginFile {
    path: PathBuf,
    id: &'static str,
    rendered: String,
    binary: PathBuf,
    allow_dev_path: bool,
    euid: u32,
}

struct OnDiskFile {
    path: PathBuf,
    text: Option<String>,
    fingerprint: Option<Fingerprint>,
}

impl PluginFile {
    pub fn new(
        path: PathBuf,
        id: &'static str,
        rendered: String,
        binary: PathBuf,
        allow_dev_path: bool,
        euid: u32,
    ) -> Self {
        Self {
            path,
            id,
            rendered,
            binary,
            allow_dev_path,
            euid,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn target(&self) -> Result<PathBuf> {
        resolve(&self.path)
    }

    fn read(&self) -> Result<OnDiskFile> {
        let path = self.target()?;
        check_regular(&path)?;
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(OnDiskFile {
                    path,
                    text: None,
                    fingerprint: None,
                })
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let fingerprint = Fingerprint::of(&file.metadata()?);
        let mut text = String::new();
        file.read_to_string(&mut text)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(OnDiskFile {
            path,
            text: Some(text),
            fingerprint: Some(fingerprint),
        })
    }

    fn on_disk(&self, text: Option<&str>) -> OnDisk {
        match text {
            None => OnDisk::Absent,
            Some(text) if text == self.rendered => OnDisk::Installed,
            Some(text) if is_ours(text, self.id) => OnDisk::Stale,
            Some(_) => OnDisk::Foreign,
        }
    }

    fn foreign(&self) -> String {
        format!(
            "{} is not a file remux wrote (its first line is not remux's header); \
             it is left alone: move it aside and re-run",
            shown(&self.path)
        )
    }

    pub fn status(&self) -> Result<IntegrationStatus> {
        let read = self.read()?;
        Ok(match self.on_disk(read.text.as_deref()) {
            OnDisk::Installed => match check_binary_safe(&self.binary, self.euid) {
                Ok(()) => IntegrationStatus::Installed {
                    runs: format!("{} runs {}", shown(&self.path), shown(&self.binary)),
                },
                Err(e) => IntegrationStatus::Stale {
                    why: format!("{e:#}"),
                },
            },
            OnDisk::Stale => IntegrationStatus::Stale {
                why: format!("{} differs from this build's", shown(&self.path)),
            },
            OnDisk::Foreign => IntegrationStatus::Stale {
                why: self.foreign(),
            },
            OnDisk::Absent => IntegrationStatus::Absent,
        })
    }

    /// What [`Self::install`] would change.
    pub fn plan(&self) -> Result<Option<Change>> {
        check_hook_binary(&self.binary, self.allow_dev_path, self.euid)?;
        let read = self.checked_read()?;
        Ok(match self.on_disk(read.text.as_deref()) {
            OnDisk::Installed => None,
            OnDisk::Foreign => anyhow::bail!("{}", self.foreign()),
            OnDisk::Stale | OnDisk::Absent => Some(Change {
                path: read.path,
                before: read.text,
                after: Some(self.rendered.clone()),
            }),
        })
    }

    fn checked_read(&self) -> Result<OnDiskFile> {
        let path = self.target()?;
        check_owner(&path, self.euid)?;
        check_directory(&path, self.euid)?;
        check_plain_file(&path)?;
        self.read()
    }

    pub fn install(&self) -> Result<()> {
        check_hook_binary(&self.binary, self.allow_dev_path, self.euid)?;
        let read = self.checked_read()?;
        match self.on_disk(read.text.as_deref()) {
            OnDisk::Installed => Ok(()),
            OnDisk::Foreign => anyhow::bail!("{}", self.foreign()),
            OnDisk::Stale | OnDisk::Absent => {
                write_atomically(&read.path, &self.rendered, read.fingerprint, Some(0o644))
            }
        }
    }

    /// Remove remux's file. A file that is not remux's is left, with a note.
    pub fn uninstall(&self) -> Result<Vec<String>> {
        let read = self.checked_read()?;
        match self.on_disk(read.text.as_deref()) {
            OnDisk::Absent => Ok(Vec::new()),
            OnDisk::Foreign => Ok(vec![self.foreign()]),
            // The path itself, which may be a link into the user's dotfiles:
            // the file it names is theirs.
            OnDisk::Installed | OnDisk::Stale => {
                std::fs::remove_file(&self.path)
                    .with_context(|| format!("removing {}", self.path.display()))?;
                Ok(Vec::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrate::hooks_file::effective_uid;

    const TEMPLATE: &str =
        "// managed by remux; REMUX_INTEGRATION_ID=demo REMUX_INTEGRATION_VERSION=1\nconst BIN = \"__BIN__\";\n";

    fn plugin(dir: &Path) -> PluginFile {
        // A binary under the tempdir is not under a world-writable directory,
        // which the binary check refuses.
        let binary = std::env::current_exe().unwrap();
        PluginFile::new(
            dir.join("ext").join("demo.ts"),
            "demo",
            render(TEMPLATE, &[("\"__BIN__\"", &js_string("/opt/re\"mux"))]),
            binary,
            true,
            effective_uid(),
        )
    }

    #[test]
    fn render_substitutes_a_js_string_literal() {
        assert_eq!(
            render(TEMPLATE, &[("\"__BIN__\"", &js_string("/opt/re\"mux"))])
                .lines()
                .nth(1),
            Some(r#"const BIN = "/opt/re\"mux";"#)
        );
    }

    #[test]
    fn the_header_names_the_integration() {
        assert!(is_ours(TEMPLATE, "demo"));
        assert!(!is_ours(TEMPLATE, "other"));
        assert!(!is_ours("// my own plugin\n", "demo"));
        assert!(!is_ours(&format!("\n{TEMPLATE}"), "demo"));
    }

    #[test]
    fn absent_installed_stale_and_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin(dir.path());
        assert_eq!(p.status().unwrap(), IntegrationStatus::Absent);

        p.install().unwrap();
        assert!(matches!(
            p.status().unwrap(),
            IntegrationStatus::Installed { .. }
        ));
        let installed = std::fs::read_to_string(p.path()).unwrap();
        assert!(installed.starts_with(HEADER));

        std::fs::write(p.path(), installed.replace("re\\\"mux", "older")).unwrap();
        assert!(matches!(
            p.status().unwrap(),
            IntegrationStatus::Stale { .. }
        ));
        p.install().unwrap();
        assert_eq!(std::fs::read_to_string(p.path()).unwrap(), installed);

        std::fs::write(p.path(), "// my own plugin\n").unwrap();
        assert!(p.install().is_err());
        assert_eq!(p.uninstall().unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(p.path()).unwrap(),
            "// my own plugin\n"
        );
    }

    /// A plugin path that is a symlink (into a dotfiles repo, say) is
    /// written through, but uninstall removes the LINK: the file it names is
    /// the user's, and deleting it would leave a dangling link behind.
    #[test]
    fn uninstall_through_a_symlink_removes_the_link_and_keeps_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin(dir.path());
        let target = dir.path().join("dotfiles").join("demo.ts");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, TEMPLATE).unwrap();
        std::fs::create_dir_all(p.path().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, p.path()).unwrap();
        p.install().unwrap();
        assert!(target.exists(), "install writes through the link");
        p.uninstall().unwrap();
        assert!(
            std::fs::symlink_metadata(p.path()).is_err(),
            "the link is gone"
        );
        assert!(target.exists(), "and the file it named is kept");
    }

    #[test]
    fn uninstall_removes_ours_stale_or_not() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin(dir.path());
        p.install().unwrap();
        p.uninstall().unwrap();
        assert!(!p.path().exists());
        std::fs::write(p.path(), TEMPLATE.replace("const", "let")).unwrap();
        p.uninstall().unwrap();
        assert!(!p.path().exists());
    }
}
