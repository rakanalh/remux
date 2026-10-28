//! A Claude-shaped hooks file (`settings.json`'s `hooks` key): install,
//! report on, and remove one set of remux hooks in it, leaving everything of
//! the user's as it was, byte for byte where nothing of remux's changed.
//!
//! The hooks themselves are data, a [`HookSet`]: which events, which verbs,
//! how remux spells the command, and the spellings older builds wrote. The
//! machinery here knows nothing of what the hooks do.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use super::{Change, IntegrationStatus};

/// One integration's hooks, as data.
#[derive(Debug)]
pub struct HookSet {
    /// (event, verb): each event remux hooks, and the verb its command runs.
    pub events: &'static [(&'static str, &'static str)],
    /// What follows the quoted binary for a verb. Every spelling remux writes
    /// ends in it, which is how an older build's spelling is recognised.
    pub tail: fn(&str) -> String,
    /// (verb, command): spellings by name that remux itself wrote. Ours, and
    /// stale: a command found by PATH lookup runs whatever `remux` the hook's
    /// shell finds first, which a project's own PATH entry can decide.
    pub by_name: &'static [(&'static str, &'static str)],
    /// A command containing this is remux-shaped: reported, never touched.
    pub marker: &'static str,
    /// The `timeout` written beside each command, in seconds.
    pub timeout_secs: u64,
}

/// The hook commands for one remux binary: its absolute, canonical path,
/// quoted for `sh`, never a name for PATH to resolve.
#[derive(Debug, Clone)]
pub struct Hooks {
    binary: PathBuf,
    set: &'static HookSet,
}

impl Hooks {
    pub fn new(set: &'static HookSet, binary: PathBuf) -> Self {
        Self { binary, set }
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn set(&self) -> &'static HookSet {
        self.set
    }

    /// The command this build writes for `verb`.
    pub fn command(&self, verb: &str) -> String {
        format!(
            "{}{}",
            sh_quote(&self.binary.to_string_lossy()),
            (self.set.tail)(verb)
        )
    }

    /// Whose `command` is, for the event running `verb`. Only a command EQUAL
    /// to one remux writes, or wrote, is remux's own: anything else, however
    /// much it looks like remux, is the user's, and never rewritten or removed.
    fn owner(&self, command: &str, verb: &str) -> Owner {
        if command == self.command(verb) {
            Owner::Current
        } else if let Some(binary) = self.written_binary(command, verb) {
            Owner::Older(Some(binary))
        } else if self
            .set
            .by_name
            .iter()
            .any(|(v, c)| *v == verb && *c == command)
        {
            Owner::Older(None)
        } else if command.contains(self.set.marker) {
            Owner::Foreign
        } else {
            Owner::User
        }
    }

    /// The binary an absolute-path spelling of remux's own names, if `command`
    /// is one: this build's shape, for `verb`, around any absolute path.
    pub fn written_binary(&self, command: &str, verb: &str) -> Option<PathBuf> {
        let word = command.strip_suffix(&(self.set.tail)(verb))?;
        let path = PathBuf::from(sh_unquote(word)?);
        path.is_absolute().then_some(path)
    }
}

/// `text` as one `sh` word: single quotes, a quote inside as `'\''`.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The inverse of [`sh_quote`], for exactly what it produces.
fn sh_unquote(word: &str) -> Option<String> {
    let inner = word.strip_prefix('\'')?.strip_suffix('\'')?;
    let text = inner.replace(r"'\''", "'");
    (sh_quote(&text) == word).then_some(text)
}

/// Whether `binary` lives in a cargo build directory: an ancestor named
/// `target` that cargo made (it holds `.rustc_info.json` or `CACHEDIR.TAG`) or
/// that sits beside a `Cargo.toml`. A hook pointing there runs whatever the
/// next `cargo build` leaves, or nothing after a `cargo clean`.
pub fn is_dev_build(binary: &Path) -> bool {
    binary.ancestors().skip(1).any(|dir| {
        dir.file_name().is_some_and(|n| n == "target")
            && (dir.join(".rustc_info.json").exists()
                || dir.join("CACHEDIR.TAG").exists()
                || dir.parent().is_some_and(|p| p.join("Cargo.toml").exists()))
    })
}

/// A Claude-shaped hooks file, and the hooks remux keeps in it.
pub struct HooksFile {
    settings: PathBuf,
    hooks: Hooks,
    allow_dev_path: bool,
    /// Whose files these must be. The process's effective uid, held here so a
    /// test can stand in for another user.
    euid: u32,
}

/// What a file WAS when it was read: a write goes ahead only onto the same
/// file, unchanged, so a concurrent edit is never silently overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    dev: u64,
    ino: u64,
    mtime: i64,
    mtime_nsec: i64,
    len: u64,
}

impl Fingerprint {
    pub fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            len: meta.len(),
        }
    }
}

/// The settings file, read: its resolved path, its text and value, and its
/// fingerprint (all None / empty when it does not exist yet).
struct Loaded {
    path: PathBuf,
    text: Option<String>,
    value: Value,
    fingerprint: Option<Fingerprint>,
}

/// This process's effective uid: whose files an integration may write.
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// `$HOME` (read directly, so a throwaway one is honoured on every platform)
/// and this binary's canonical path.
pub fn home_and_binary() -> Result<(PathBuf, PathBuf)> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .context("HOME is not set to an absolute path")?;
    let binary = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("naming this remux binary")?;
    anyhow::ensure!(
        binary.to_str().is_some(),
        "this remux binary's path is not UTF-8: {}",
        binary.display()
    );
    Ok((home, binary))
}

/// Before any write: never as root, and never onto a path that belongs to
/// someone else. Root running `integrate` would plant hooks in a home that is
/// not its own, and a file owned by another user is not this user's to
/// rewrite. For a path not there yet, the nearest ancestor that exists is the
/// one checked, however deep: a file two levels under a directory another
/// user owns would otherwise be created there unchecked.
pub fn check_owner(path: &Path, euid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    anyhow::ensure!(
        euid != 0,
        "refusing to run as root: run `remux integrate` as the user Claude runs as"
    );
    for what in path.ancestors() {
        let meta = match std::fs::metadata(what) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", what.display())),
        };
        anyhow::ensure!(
            meta.uid() == euid,
            "{} belongs to uid {}, not to you (uid {euid}); refusing to edit it",
            what.display(),
            meta.uid()
        );
        return Ok(());
    }
    Ok(())
}

/// The hooks run whatever file is at `binary`, as this user, each time Claude
/// stops or takes a prompt. Another user who can replace that file, or any
/// directory above it, chooses what runs. So the binary and every ancestor
/// that exists must belong to this user or root and be writable by nobody
/// else; a sticky world-writable directory such as `/tmp` counts as writable,
/// since its owner's files can still be created there under a new name.
pub fn check_binary_safe(binary: &Path, euid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    const FIX: &str = "install remux somewhere only you or root can write, and run that \
                       binary's `integrate`";
    for path in binary.ancestors() {
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        anyhow::ensure!(
            meta.uid() == euid || meta.uid() == 0,
            "{} belongs to uid {}: another user could replace the remux these hooks run; {FIX}",
            shown(path),
            meta.uid()
        );
        anyhow::ensure!(
            meta.mode() & 0o022 == 0,
            "{} is writable by others (mode {:o}): another user could replace the remux these \
             hooks run; {FIX}",
            shown(path),
            meta.mode() & 0o7777
        );
    }
    Ok(())
}

/// The nearest existing directory above `path` must be the user's and
/// writable by nobody else: another user who can write there can swap the
/// file between the read and the rename, plant one where the new file lands,
/// or create the missing directories under it before the install does.
pub fn check_directory(path: &Path, euid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut found = None;
    for dir in path.ancestors().skip(1) {
        match std::fs::metadata(dir) {
            Ok(meta) => {
                found = Some((dir, meta));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        }
    }
    let Some((dir, meta)) = found else {
        return Ok(());
    };
    anyhow::ensure!(
        meta.uid() == euid,
        "{} belongs to uid {}, not to you (uid {euid}); refusing to write into it",
        dir.display(),
        meta.uid()
    );
    anyhow::ensure!(
        meta.mode() & 0o022 == 0,
        "{} is writable by others (mode {:o}); refusing to write into it: \
         chmod go-w it first",
        dir.display(),
        meta.mode() & 0o7777
    );
    Ok(())
}

impl HooksFile {
    /// The hooks file at `settings`, writing `hooks` as the user `euid`.
    pub fn new(settings: PathBuf, hooks: Hooks, allow_dev_path: bool, euid: u32) -> Self {
        Self {
            settings,
            hooks,
            allow_dev_path,
            euid,
        }
    }

    /// A hook must not be baked onto a build directory by accident.
    pub fn check_binary(&self) -> Result<()> {
        anyhow::ensure!(
            self.allow_dev_path || !is_dev_build(self.hooks.binary()),
            "{} is a cargo build output; hooks pointing there break on the next \
             build or clean. Install remux (cargo install --path .) and run that \
             binary's `integrate`, or pass --allow-dev-path to use this one anyway",
            self.hooks.binary().display()
        );
        check_binary_safe(self.hooks.binary(), self.euid)
    }

    /// The file to read and write, resolved ONCE per operation: the settings
    /// file, or what it links to, so a dotfiles symlink is written through
    /// rather than replaced by a file. A link to nothing is refused: writing
    /// "through" it would create a file wherever it happens to point, and
    /// replacing it would destroy the link.
    fn target(&self) -> Result<PathBuf> {
        let meta = match std::fs::symlink_metadata(&self.settings) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Not there yet: under its directory as it resolves, if it exists.
                let dir = self
                    .settings
                    .parent()
                    .context("settings has no directory")?;
                return Ok(match std::fs::canonicalize(dir) {
                    Ok(dir) => match self.settings.file_name() {
                        Some(name) => dir.join(name),
                        None => self.settings.clone(),
                    },
                    Err(_) => self.settings.clone(),
                });
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", self.settings.display()))
            }
        };
        match std::fs::canonicalize(&self.settings) {
            Ok(path) => Ok(path),
            Err(e) if meta.file_type().is_symlink() && e.kind() == std::io::ErrorKind::NotFound => {
                let to = std::fs::read_link(&self.settings)
                    .map(|p| shown(&p))
                    .unwrap_or_else(|_| "?".to_string());
                anyhow::bail!(
                    "{} is a symlink to {to}, which does not exist; create the file it \
                     names, or remove the link, and re-run",
                    self.settings.display()
                )
            }
            Err(e) => Err(e).with_context(|| format!("resolving {}", self.settings.display())),
        }
    }

    fn read(&self, path: PathBuf) -> Result<Loaded> {
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Loaded {
                    path,
                    text: None,
                    value: Value::Object(Map::new()),
                    fingerprint: None,
                })
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let fingerprint = Fingerprint::of(&file.metadata()?);
        let mut text = String::new();
        file.read_to_string(&mut text)
            .with_context(|| format!("reading {}", path.display()))?;
        let value = parse_settings(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?;
        Ok(Loaded {
            path,
            text: Some(text),
            value,
            fingerprint: Some(fingerprint),
        })
    }

    /// The settings file after `edit`, if that changes what it MEANS: text that
    /// parses to the same value is not rewritten, whatever its layout. With
    /// `write`, the change is made, onto the same file it was read from.
    fn edit(
        &self,
        edit: fn(&mut Value, &Hooks) -> Vec<String>,
        write: bool,
    ) -> Result<(Change, Vec<String>)> {
        let path = self.target()?;
        check_owner(&path, self.euid)?;
        check_directory(&path, self.euid)?;
        let read = self.read(path)?;
        let mut after = read.value.clone();
        let notes = edit(&mut after, &self.hooks);
        let after = if after == read.value {
            None
        } else {
            Some(render(&after)?)
        };
        if let (true, Some(text)) = (write, &after) {
            write_atomically(&read.path, text, read.fingerprint, None)?;
        }
        Ok((
            Change {
                path: read.path,
                before: read.text,
                after,
            },
            notes,
        ))
    }

    /// The hooks' status in the file as it is now.
    pub fn status(&self) -> Result<IntegrationStatus> {
        let read = self.read(self.target()?)?;
        Ok(status_of(&read.value, &self.hooks))
    }

    /// What [`Self::install`] would change, shown as the `hooks` subtree: the
    /// rest of the file is the user's and unchanged.
    pub fn plan(&self) -> Result<(Vec<Change>, Vec<String>)> {
        self.check_binary()?;
        let (change, notes) = self.edit(install_hooks, false)?;
        let changes = if change.after.is_some() {
            vec![Change {
                before: Some(hooks_subtree(change.before.as_deref())),
                after: Some(hooks_subtree(change.after.as_deref())),
                path: change.path,
            }]
        } else {
            Vec::new()
        };
        Ok((changes, notes))
    }

    pub fn install(&self) -> Result<Vec<String>> {
        self.check_binary()?;
        Ok(self.edit(install_hooks, true)?.1)
    }

    pub fn uninstall(&self) -> Result<Vec<String>> {
        Ok(self.edit(uninstall_hooks, true)?.1)
    }
}

/// An empty file is an empty object, as Claude treats it.
fn parse_settings(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value = serde_json::from_str(text)?;
    anyhow::ensure!(value.is_object(), "the top level is not a JSON object");
    Ok(value)
}

fn render(value: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)? + "\n")
}

fn commands(group: &Value) -> impl Iterator<Item = &str> {
    group["hooks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| h["command"].as_str())
}

/// Whose a hook command is.
#[derive(Debug, PartialEq, Eq)]
enum Owner {
    /// This build's spelling.
    Current,
    /// A spelling remux wrote before: another binary's path, or by name.
    Older(Option<PathBuf>),
    /// Remux-shaped, but not a spelling remux wrote.
    Foreign,
    User,
}

fn foreign_note(event: &str, command: &str) -> String {
    format!("{event} runs {command:?}, a remux hook remux did not write; left alone")
}

/// A path as a note shows it: the settings file wrote it, so a control
/// character in it is escaped rather than sent to the terminal.
pub(super) fn shown(path: &Path) -> String {
    crate::client::cli_common::escape(&path.to_string_lossy())
}

fn older_note(event: &str, binary: &Option<PathBuf>, hooks: &Hooks) -> String {
    match binary {
        None => format!("{event} runs remux by name, which PATH resolves"),
        Some(binary) if !binary.exists() => {
            format!("{event} runs {}, which no longer exists", shown(binary))
        }
        Some(binary) => format!(
            "{event} runs {}, not this binary ({})",
            shown(binary),
            shown(hooks.binary())
        ),
    }
}

/// [`IntegrationStatus`] of a parsed settings value, against `hooks`. Only
/// the set's own events are looked at: every other key under `hooks` is
/// another integration's or the user's.
pub fn status_of(settings: &Value, hooks: &Hooks) -> IntegrationStatus {
    let events = hooks.set().events;
    let mut current = 0;
    let mut stale = Vec::new();
    for &(event, verb) in events {
        let found: Vec<(&str, Owner)> = settings["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(commands)
            .map(|c| (c, hooks.owner(c, verb)))
            .collect();
        if found.iter().any(|(_, o)| *o == Owner::Current) {
            current += 1;
        } else if let Some((_, Owner::Older(binary))) =
            found.iter().find(|(_, o)| matches!(o, Owner::Older(_)))
        {
            stale.push(older_note(event, binary, hooks));
        } else if let Some((c, _)) = found.iter().find(|(_, o)| *o == Owner::Foreign) {
            stale.push(foreign_note(event, c));
        } else {
            stale.push(format!("{event} has no remux hook"));
        }
    }
    match current {
        n if n == events.len() => IntegrationStatus::Installed {
            runs: format!("the hooks run {}", shown(hooks.binary())),
        },
        0 if stale.iter().all(|s| s.ends_with("no remux hook")) => IntegrationStatus::Absent,
        _ => IntegrationStatus::Stale {
            why: stale.join("; "),
        },
    }
}

/// Add remux's hooks, returning a note for each event left alone. An older
/// spelling of remux's own is rewritten to this build's in place. An event
/// with a remux-shaped hook remux did not write gets nothing added beside it,
/// since both would run. The user's own groups and every other key are left
/// as they are.
pub fn install_hooks(settings: &mut Value, hooks_for: &Hooks) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(root) = settings.as_object_mut() else {
        return notes;
    };
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        notes.push("hooks is not an object; left alone".to_string());
        return notes;
    };
    for &(event, verb) in hooks_for.set().events {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(groups) = groups.as_array_mut() else {
            notes.push(format!("{event} is not a list; left alone"));
            continue;
        };
        let want = hooks_for.command(verb);
        let mut ours = false;
        let mut foreign = None;
        for hook in groups
            .iter_mut()
            .filter_map(|g| g.get_mut("hooks").and_then(Value::as_array_mut))
            .flatten()
        {
            let Some(command) = hook["command"].as_str() else {
                continue;
            };
            match hooks_for.owner(command, verb) {
                Owner::Current => ours = true,
                Owner::Older(_) => {
                    hook["command"] = Value::String(want.clone());
                    ours = true;
                }
                Owner::Foreign => foreign = foreign.or(Some(command.to_string())),
                Owner::User => {}
            }
        }
        if let (false, Some(command)) = (ours, &foreign) {
            notes.push(foreign_note(event, command));
        } else if !ours {
            groups.push(json!({
                "hooks": [{
                    "type": "command",
                    "command": want,
                    "timeout": hooks_for.set().timeout_secs
                }]
            }));
        }
    }
    notes
}

/// Remove every hook entry that is remux's own spelling, then a group and an
/// event list only if that removal is what emptied it. `hooks` itself stays,
/// and so does everything of the user's, a remux-shaped hook remux did not
/// write and an empty list they left there included.
pub fn uninstall_hooks(settings: &mut Value, hooks_for: &Hooks) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return notes;
    };
    for &(event, verb) in hooks_for.set().events {
        let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
            continue;
        };
        let mut emptied = vec![false; groups.len()];
        for (group, emptied) in groups.iter_mut().zip(emptied.iter_mut()) {
            let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            let had = list.len();
            list.retain(|h| {
                let command = h["command"].as_str().unwrap_or_default();
                match hooks_for.owner(command, verb) {
                    Owner::Current | Owner::Older(_) => false,
                    Owner::Foreign => {
                        notes.push(foreign_note(event, command));
                        true
                    }
                    Owner::User => true,
                }
            });
            *emptied = had > 0 && list.is_empty();
        }
        let before = groups.len();
        let mut flags = emptied.into_iter();
        groups.retain(|_| !flags.next().unwrap_or(false));
        if before > 0 && groups.is_empty() {
            hooks.remove(event);
        }
    }
    notes
}

/// Replace `path` with `text` in one rename, keeping its permissions, so a
/// crash leaves either the old file or the new one, never half of either. Only
/// if `path` is still what was read (`read`, or absent for None) just before
/// the rename: a file edited meanwhile, by the user or by Claude, is left as
/// it is and the edit is refused. A file created here gets `mode_if_new`, or
/// the temporary file's 0600 without one.
pub fn write_atomically(
    path: &Path,
    text: &str,
    read: Option<Fingerprint>,
    mode_if_new: Option<u32>,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path
        .parent()
        .with_context(|| format!("{} has no directory", path.display()))?;
    if !dir.exists() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().sync_all()?;
    let now = match std::fs::metadata(path) {
        Ok(meta) => {
            let bits = meta.permissions().mode() & 0o777;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(bits))?;
            Some(Fingerprint::of(&meta))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(mode) = mode_if_new {
                tmp.as_file()
                    .set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            None
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    anyhow::ensure!(
        now == read,
        "{} changed while it was being edited; nothing was written, re-run",
        path.display()
    );
    tmp.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// The `hooks` subtree of a settings file's text, for `--dry-run`, printed to
/// a terminal: DEL and C1 characters in its strings are escaped, since the
/// file's author wrote them and a terminal can read C1 as control sequences.
pub fn hooks_subtree(text: Option<&str>) -> String {
    let hooks = text
        .and_then(|t| parse_settings(t).ok())
        .map(|v| v["hooks"].clone())
        .unwrap_or(Value::Null);
    crate::client::cli_common::escaped_json(
        &serde_json::to_string_pretty(&hooks).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The messaging set, defined here so the machinery is tested as
    /// machinery: nothing below depends on what the integration ships.
    static SET: HookSet = HookSet {
        events: &[
            ("Stop", "claude-stop"),
            ("UserPromptSubmit", "claude-prompt"),
        ],
        tail: |verb| format!(" msg hook {verb} || true"),
        by_name: &[
            ("claude-stop", "remux msg hook claude-stop || true"),
            ("claude-prompt", "remux msg hook claude-prompt || true"),
        ],
        marker: "msg hook",
        timeout_secs: 10,
    };

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/settings/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// A binary path with a quote in it, so every spelling test exercises the
    /// `sh` quoting too.
    fn hooks() -> Hooks {
        Hooks::new(&SET, PathBuf::from("/opt/re'mux/bin/remux"))
    }

    fn installed(text: &str) -> String {
        let mut v = parse_settings(text).unwrap();
        install_hooks(&mut v, &hooks());
        render(&v).unwrap()
    }

    fn value(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    /// `msg_by_name.json` as this build would have written it.
    fn current() -> String {
        let text = fixture("msg_by_name.json");
        let h = hooks();
        let mut out = text.clone();
        for (verb, by_name) in SET.by_name {
            assert_eq!(out.matches(by_name).count(), 1);
            let json_escaped = serde_json::to_string(&h.command(verb)).unwrap();
            out = out.replace(&format!("\"{by_name}\""), &json_escaped);
        }
        out
    }

    #[test]
    fn the_command_runs_the_binary_by_its_quoted_absolute_path() {
        let h = hooks();
        assert_eq!(
            h.command("claude-stop"),
            r"'/opt/re'\''mux/bin/remux' msg hook claude-stop || true"
        );
        assert_eq!(
            h.written_binary(&h.command("claude-prompt"), "claude-prompt"),
            Some(PathBuf::from("/opt/re'mux/bin/remux"))
        );
        assert_eq!(
            h.written_binary(&h.command("claude-prompt"), "claude-stop"),
            None
        );
        assert_eq!(
            h.written_binary(
                "'relative/remux' msg hook claude-stop || true",
                "claude-stop"
            ),
            None
        );
        assert_eq!(
            h.written_binary("'/a'b' msg hook claude-stop || true", "claude-stop"),
            None,
            "not a word sh_quote produces"
        );
    }

    #[test]
    fn an_empty_file_is_an_empty_object() {
        let after = installed(&fixture("empty.json"));
        assert!(matches!(
            status_of(&value(&after), &hooks()),
            IntegrationStatus::Installed { .. }
        ));
    }

    #[test]
    fn install_adds_hooks_and_leaves_every_other_key() {
        let before = value(&fixture("no_hooks.json"));
        let after = value(&installed(&fixture("no_hooks.json")));
        for (k, v) in before.as_object().unwrap() {
            assert_eq!(&after[k], v, "{k} untouched");
        }
        let h = hooks();
        assert_eq!(
            after["hooks"]["Stop"][0]["hooks"][0]["command"],
            h.command("claude-stop")
        );
        assert_eq!(
            after["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            h.command("claude-prompt")
        );
        assert_eq!(after["hooks"]["Stop"][0]["hooks"][0]["timeout"], 10);
    }

    /// The user's key order survives, their own groups too, and uninstalling
    /// gives back the file they had, byte for byte.
    #[test]
    fn install_keeps_the_users_order_and_uninstall_restores_it() {
        let original = fixture("msg_other_hooks_unsorted.json");
        let after = installed(&original);
        let keys = |text: &str| -> Vec<String> {
            value(text).as_object().unwrap().keys().cloned().collect()
        };
        assert_eq!(keys(&after), keys(&original));
        let v = value(&after);
        let stops = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stops.len(), 2, "the user's group, then ours");
        assert_eq!(stops[0]["hooks"][0]["command"], "notify-send stopped");
        assert!(v["hooks"]["PreToolUse"].is_array());

        let mut back = value(&after);
        uninstall_hooks(&mut back, &hooks());
        assert_eq!(render(&back).unwrap(), original);
    }

    #[test]
    fn installed_is_installed_and_a_second_install_changes_nothing() {
        let text = current();
        assert!(matches!(
            status_of(&value(&text), &hooks()),
            IntegrationStatus::Installed { .. }
        ));
        assert_eq!(installed(&text), text);
    }

    /// A hook that merely contains `msg hook` is the user's: reported, never
    /// rewritten, and nothing of remux's is added beside it.
    #[test]
    fn a_remux_hook_remux_did_not_write_is_reported_and_left_alone() {
        for name in ["msg_stale.json", "msg_foreign_hooks.json"] {
            let text = fixture(name);
            match status_of(&value(&text), &hooks()) {
                IntegrationStatus::Stale { why } => {
                    assert!(why.contains("did not write"), "{name}: {why}")
                }
                other => panic!("{name}: {other:?}"),
            }
        }
        let text = fixture("msg_foreign_hooks.json");
        let mut v = value(&text);
        let notes = install_hooks(&mut v, &hooks());
        assert_eq!(v, value(&text), "install changes nothing");
        assert_eq!(notes.len(), 2, "{notes:?}");
        let notes = uninstall_hooks(&mut v, &hooks());
        assert_eq!(v, value(&text), "uninstall changes nothing");
        assert_eq!(notes.len(), 2, "{notes:?}");
    }

    /// The by-name spelling is remux's own and stale: PATH picks the binary.
    /// Install rewrites it in place; uninstall removes it.
    #[test]
    fn the_by_name_spelling_is_ours_stale_and_repaired_in_place() {
        let text = fixture("msg_by_name.json");
        match status_of(&value(&text), &hooks()) {
            IntegrationStatus::Stale { why } => assert!(why.contains("by name"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(value(&installed(&text)), value(&current()));
        assert_eq!(installed(&text), fixture("messaging_installed.json"));
        let mut v = value(&text);
        uninstall_hooks(&mut v, &hooks());
        assert_eq!(v, value(r#"{"model":"sonnet","hooks":{}}"#));
    }

    /// Another binary's path is ours and stale, named, whether it still
    /// exists or not; install points it at this one.
    #[test]
    fn another_binary_s_path_is_stale_and_named() {
        let text = current();
        for (old, says) in [
            ("/nonexistent/remux", "no longer exists"),
            ("/bin/sh", "not this binary"),
        ] {
            let other = Hooks::new(&SET, PathBuf::from(old));
            let then = text.replace(
                &serde_json::to_string(&hooks().command("claude-stop")).unwrap(),
                &serde_json::to_string(&other.command("claude-stop")).unwrap(),
            );
            assert_ne!(then, text);
            match status_of(&value(&then), &hooks()) {
                IntegrationStatus::Stale { why } => {
                    assert!(why.contains(old) && why.contains(says), "{why}")
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(value(&installed(&then)), value(&text));
        }
    }

    /// Only a list the uninstall emptied goes; a user's empty one stays.
    #[test]
    fn uninstall_keeps_an_empty_list_it_did_not_empty() {
        let mut v = value(&current());
        v["hooks"]["PreToolUse"] = json!([]);
        v["hooks"]["UserPromptSubmit"]
            .as_array_mut()
            .unwrap()
            .push(json!({"hooks": []}));
        uninstall_hooks(&mut v, &hooks());
        assert_eq!(
            v["hooks"],
            json!({"UserPromptSubmit": [{"hooks": []}], "PreToolUse": []})
        );
        let mut v = value(&fixture("msg_no_remux_4space.json"));
        let before = v.clone();
        uninstall_hooks(&mut v, &hooks());
        assert_eq!(v, before, "the user's own empty Stop list stays");
    }

    /// The other integration's events are not this set's: invisible to its
    /// status, untouched by its install and uninstall.
    #[test]
    fn another_integrations_events_in_the_same_file_are_left_alone() {
        let text = fixture("both_integrations.json");
        let theirs = |v: &Value| {
            (
                v["hooks"]["SubagentStart"].clone(),
                v["hooks"]["SubagentStop"].clone(),
            )
        };
        let before = value(&text);
        match status_of(&before, &hooks()) {
            IntegrationStatus::Stale { why } => assert!(
                why.contains("Stop runs remux by name")
                    && why.contains("UserPromptSubmit runs remux by name")
                    && !why.contains("Subagent"),
                "{why}"
            ),
            other => panic!("{other:?}"),
        }
        let mut v = before.clone();
        install_hooks(&mut v, &hooks());
        assert_eq!(theirs(&v), theirs(&before));
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            hooks().command("claude-stop")
        );
        uninstall_hooks(&mut v, &hooks());
        assert_eq!(theirs(&v), theirs(&before));
        assert!(v["hooks"].get("Stop").is_none() && v["hooks"].get("UserPromptSubmit").is_none());
    }

    fn me() -> u32 {
        effective_uid()
    }

    /// The hooks file under a throwaway `home`, as the user `euid`, through
    /// the real install and uninstall paths. Never the real `$HOME`.
    fn file_in(home: &Path, euid: u32) -> HooksFile {
        HooksFile::new(
            home.join(".claude").join("settings.json"),
            hooks(),
            true,
            euid,
        )
    }

    #[test]
    fn root_is_refused_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let err = file_in(home.path(), 0).install().unwrap_err();
        assert!(format!("{err:#}").contains("root"), "{err:#}");
        assert!(!home.path().join(".claude").exists());
        assert!(file_in(home.path(), 0).uninstall().is_err());
    }

    /// Another user's settings file, or another user's ~/.claude for a file
    /// not there yet, is not this user's to rewrite.
    #[test]
    fn a_file_or_directory_owned_by_another_user_is_refused() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".claude")).unwrap();
        let someone_else = me() + 1;
        let err = file_in(home.path(), someone_else).install().unwrap_err();
        assert!(format!("{err:#}").contains("belongs to uid"), "{err:#}");
        assert!(!home.path().join(".claude/settings.json").exists());

        let settings = home.path().join(".claude/settings.json");
        std::fs::write(&settings, "{}\n").unwrap();
        for result in [
            file_in(home.path(), someone_else).install(),
            file_in(home.path(), someone_else).uninstall(),
        ] {
            assert!(format!("{:#}", result.unwrap_err()).contains("belongs to uid"));
        }
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{}\n");
        assert!(
            file_in(home.path(), me()).install().is_ok(),
            "the owner may"
        );
    }

    /// A path whose parent is not there yet is judged by the nearest ancestor
    /// that is: here the grandparent, which is another user's.
    #[test]
    fn an_ancestor_owned_by_another_user_is_refused_two_levels_down() {
        let home = tempfile::tempdir().unwrap();
        let deep = home
            .path()
            .join("skills")
            .join("remux-messaging")
            .join("SKILL.md");
        let err = check_owner(&deep, me() + 1).unwrap_err();
        assert!(
            format!("{err:#}").contains(&format!("{} belongs to uid", home.path().display())),
            "{err:#}"
        );
        assert!(check_owner(&deep, me()).is_ok());
    }

    /// A settings file edited between the read and the rename is left as it
    /// is: the edit is refused, not silently overwritten.
    #[test]
    fn a_file_that_changed_since_it_was_read_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let read = Fingerprint::of(&std::fs::metadata(&path).unwrap());
        std::fs::write(&path, "{\"theirs\": 1}\n").unwrap();
        let err = write_atomically(&path, "{}\n", Some(read), None).unwrap_err();
        assert!(format!("{err:#}").contains("changed while"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"theirs\": 1}\n");

        let born = dir.path().join("new.json");
        std::fs::write(&born, "{\"theirs\": 2}\n").unwrap();
        assert!(
            write_atomically(&born, "{}\n", None, None).is_err(),
            "absent when read"
        );
        assert_eq!(std::fs::read_to_string(&born).unwrap(), "{\"theirs\": 2}\n");

        let now = Fingerprint::of(&std::fs::metadata(&path).unwrap());
        write_atomically(&path, "{}\n", Some(now), None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_new_file_takes_mode_if_new() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("SKILL.md");
        write_atomically(&shared, "x\n", None, Some(0o644)).unwrap();
        assert_eq!(mode(&shared), 0o644);
        let private = dir.path().join("settings.json");
        write_atomically(&private, "{}\n", None, None).unwrap();
        assert_eq!(mode(&private), 0o600);
    }

    #[test]
    fn an_existing_files_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let read = Fingerprint::of(&std::fs::metadata(&path).unwrap());
        write_atomically(&path, "{\"a\": 1}\n", Some(read), Some(0o644)).unwrap();
        assert_eq!(mode(&path), 0o640);
    }

    /// A settings.json that is a link to nothing is refused, and stays a link.
    #[test]
    fn a_dangling_settings_symlink_is_refused_and_kept() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".claude")).unwrap();
        let link = home.path().join(".claude/settings.json");
        let gone = home.path().join("dotfiles/settings.json");
        std::os::unix::fs::symlink(&gone, &link).unwrap();
        let err = file_in(home.path(), me()).install().unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
        assert_eq!(std::fs::read_link(&link).unwrap(), gone);
        assert!(!gone.exists());
    }

    #[test]
    fn a_cargo_build_output_is_a_dev_build() {
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert!(is_dev_build(&exe), "{}", exe.display());
        assert!(!is_dev_build(Path::new("/usr/local/bin/remux")));
    }

    #[test]
    fn invalid_json_is_refused() {
        assert!(parse_settings("{ not json").is_err());
        assert!(parse_settings("[1, 2]").is_err());
    }

    /// A group that is not an object with a `hooks` list is the user's, and
    /// install leaves it exactly as it is: no key added to it.
    #[test]
    fn install_adds_no_key_to_a_group_without_hooks() {
        let before = json!({"hooks": {"Stop": [{"matcher": "x"}, null]}});
        let mut v = before.clone();
        install_hooks(&mut v, &hooks());
        let stops = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            &stops[..2],
            before["hooks"]["Stop"].as_array().unwrap().as_slice()
        );
        assert_eq!(
            stops[2]["hooks"][0]["command"],
            hooks().command("claude-stop")
        );
    }

    /// ...nor panics on one that is not an object at all.
    #[test]
    fn install_leaves_a_group_that_is_not_an_object() {
        let before = json!({"hooks": {"Stop": [5, "s", [1]]}});
        let mut v = before.clone();
        install_hooks(&mut v, &hooks());
        let stops = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            &stops[..3],
            before["hooks"]["Stop"].as_array().unwrap().as_slice()
        );
        assert_eq!(stops.len(), 4, "ours added after them");
    }

    /// A `hooks` that is not an object, or an event that is not a list, is
    /// left alone and said so, never silently skipped.
    #[test]
    fn a_hooks_value_of_the_wrong_shape_is_noted_and_left_alone() {
        let mut v = json!({"hooks": 5});
        let notes = install_hooks(&mut v, &hooks());
        assert_eq!(v, json!({"hooks": 5}));
        assert!(
            notes.iter().any(|n| n.contains("hooks is not an object")),
            "{notes:?}"
        );
        let mut v = json!({"hooks": {"Stop": "no"}});
        let notes = install_hooks(&mut v, &hooks());
        assert_eq!(v["hooks"]["Stop"], "no");
        assert!(
            notes.iter().any(|n| n.contains("Stop is not a list")),
            "{notes:?}"
        );
    }

    /// A path in the settings file is the file's to write: shown escaped.
    #[test]
    fn a_stale_hooks_binary_path_is_shown_escaped() {
        let evil = Hooks::new(&SET, PathBuf::from("/tmp/x\u{1b}[31m\nremux"));
        let mut v = json!({});
        install_hooks(&mut v, &evil);
        match status_of(&v, &hooks()) {
            IntegrationStatus::Stale { why } => assert!(
                !why.contains('\u{1b}') && !why.contains('\n') && why.contains("\\x1b[31m"),
                "{why:?}"
            ),
            other => panic!("{other:?}"),
        }
        match status_of(&v, &evil) {
            IntegrationStatus::Installed { runs } => {
                assert!(!runs.contains('\u{1b}') && !runs.contains('\n'), "{runs:?}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// A directory others can write is not a safe place to rename into.
    #[test]
    fn a_settings_directory_others_can_write_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = file_in(home.path(), me()).install().unwrap_err();
        assert!(format!("{err:#}").contains("writable by others"), "{err:#}");
        assert!(!dir.join("settings.json").exists());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(file_in(home.path(), me()).install().is_ok());
    }

    /// For a file whose directory is not there yet, the nearest directory
    /// that is decides: here a 0777 one, two levels up.
    #[test]
    fn a_missing_directory_under_one_others_can_write_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let skills = home.path().join("skills");
        std::fs::create_dir(&skills).unwrap();
        std::fs::set_permissions(&skills, std::fs::Permissions::from_mode(0o777)).unwrap();
        let deep = skills.join("remux-messaging").join("SKILL.md");
        let err = check_directory(&deep, me()).unwrap_err();
        assert!(
            format!("{err:#}").contains(&format!("{} is writable by others", skills.display())),
            "{err:#}"
        );
        std::fs::set_permissions(&skills, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_directory(&deep, me()).is_ok());
    }

    /// Only permission bits are carried over to the new file.
    #[test]
    fn a_kept_mode_is_masked_to_its_permission_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o1640)).unwrap();
        let read = Fingerprint::of(&std::fs::metadata(&path).unwrap());
        write_atomically(&path, "{\"a\": 1}\n", Some(read), None).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o640
        );
    }

    /// A binary another user could replace is refused: one in a directory
    /// writable by others, or under one (a temporary directory is under
    /// `/tmp`, which is). A root-owned system binary is not.
    #[test]
    fn a_binary_another_user_could_replace_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        assert!(check_binary_safe(Path::new("/bin/sh"), me()).is_ok());
        let dir = tempfile::tempdir().unwrap();
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let binary = open.join("remux");
        std::fs::write(&binary, "").unwrap();
        let err = check_binary_safe(&binary, me()).unwrap_err();
        assert!(format!("{err:#}").contains("writable by others"), "{err:#}");
        assert!(
            check_binary_safe(Path::new("/bin/sh"), me() + 1).is_ok(),
            "root's is fine for anyone"
        );
    }

    /// `--dry-run` prints the hooks subtree to a terminal: a C1 control or
    /// DEL in a command is escaped, never sent raw.
    #[test]
    fn the_dry_run_subtree_escapes_c1_and_del() {
        let text = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "echo \u{9b}31mred\u{7f}"}
        ]}]}})
        .to_string();
        let shown = hooks_subtree(Some(&text));
        assert!(
            !shown.contains('\u{9b}') && !shown.contains('\u{7f}'),
            "{shown:?}"
        );
        assert!(shown.contains("\\u009b31mred\\u007f"), "{shown}");
        let back: Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(
            back["Stop"][0]["hooks"][0]["command"], "echo \u{9b}31mred\u{7f}",
            "the escape decodes to the same text"
        );
    }
}
