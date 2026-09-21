//! Session persistence and state saving.
//!
//! This module handles saving and loading the server state to disk, enabling
//! session resurrection after a server restart. State is stored as JSON in
//! the user's data directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::server::session::ServerState;

/// The command string for manual session save, used by the command dispatcher.
pub const SAVE_COMMAND: &str = "session_save";

/// The state that gets persisted to disk for resurrection.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistedState {
    /// The full server state (sessions, folders, tabs, layouts).
    pub state: ServerState,
    /// Mapping from PaneId to the current working directory of the pane's
    /// child process at the time of the save.
    pub pane_cwds: HashMap<u64, String>,
}

impl PersistedState {
    /// Create a `PersistedState` from the current server state.
    ///
    /// `pane_cwds` maps PaneId to the current working directory of the child
    /// process running in that pane.
    pub fn from_server(state: &ServerState, pane_cwds: &HashMap<u64, String>) -> Result<Self> {
        let json = serde_json::to_string(state)?;
        let cloned: ServerState = serde_json::from_str(&json)?;
        Ok(Self {
            state: cloned,
            pane_cwds: pane_cwds.clone(),
        })
    }
}

/// Read the current working directory of a process.
///
/// On Linux this reads the `/proc/{pid}/cwd` symlink (zero dependencies, the
/// hot path). On macOS it queries the process table via the `sysinfo` crate,
/// which is the only portable way to read a foreign process's cwd there.
///
/// Returns `None` if the cwd cannot be determined (e.g., the process has
/// exited or permission is denied).
#[cfg(target_os = "linux")]
pub fn get_pane_cwd(pid: nix::unistd::Pid) -> Option<String> {
    std::fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
}

/// See the Linux variant for documentation. macOS reads the cwd through
/// `sysinfo` because there is no `/proc` filesystem.
///
/// The `System` is built ONCE and reused. On macOS `System::new()` is not the
/// empty struct its name suggests: it calls `mach_host_self()`, reads the host
/// clock info, enumerates CPUs and allocates a 200-slot process map, all before
/// the refresh that actually does the work. This function is called from the
/// session-tree push -- once per session, under the `state`, `clients` AND
/// `panes` guards, at up to 10 Hz while a sidebar is subscribed -- so paying all
/// of that per call put real setup work inside the daemon's hottest lock scope.
/// On Linux the same function is one `readlink` and needs nothing.
///
/// The cost of reusing it is a retained `Process` entry per pid ever queried.
/// That is bounded by the panes this server has focused, entries are dropped as
/// soon as a refresh finds their process dead, and each is far cheaper than the
/// per-call construction it replaces.
///
/// A poisoned lock is recovered from rather than propagated: a panicking caller
/// leaves the `System` merely stale, and refusing to read a cwd for the rest of
/// the process's life would be the worse failure.
#[cfg(target_os = "macos")]
pub fn get_pane_cwd(pid: nix::unistd::Pid) -> Option<String> {
    use std::sync::{Mutex, OnceLock};
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    static SYSTEM: OnceLock<Mutex<System>> = OnceLock::new();

    let spid = Pid::from_u32(pid.as_raw() as u32);
    let mut system = SYSTEM
        .get_or_init(|| Mutex::new(System::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[spid]),
        true,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
    );
    system
        .process(spid)
        .and_then(|p| p.cwd())
        .and_then(|c| c.to_str().map(String::from))
}

/// Fallback for platforms without a known mechanism to read a foreign
/// process's cwd. Always returns `None`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn get_pane_cwd(_pid: nix::unistd::Pid) -> Option<String> {
    None
}

/// Return the path to the persistence data directory (`$XDG_DATA_HOME/remux`,
/// the platform's data directory, or `/tmp/remux` as a last resort).
fn data_dir() -> PathBuf {
    crate::paths::data_home().join("remux")
}

/// Where a `state.json` written before remux honoured `$XDG_DATA_HOME` on every
/// platform would be, or `None` when there is nothing to migrate.
///
/// Split from [`legacy_state_path`] so the decision can be tested on a machine
/// where the two directories always agree. `dirs::data_dir()` reads
/// `XDG_DATA_HOME` on Linux, so on Linux `platform` is always the directory
/// `current` is already under and the real resolver can only ever answer `None`.
fn legacy_of(current: &Path, platform: Option<&Path>) -> Option<PathBuf> {
    let legacy = platform?.join("remux").join("state.json");
    (legacy != current).then_some(legacy)
}

fn legacy_state_path() -> Option<PathBuf> {
    legacy_of(
        &data_dir().join("state.json"),
        crate::paths::platform_data_home().as_deref(),
    )
}

/// Atomically save the persisted state to disk.
///
/// The state is first written to a temporary file, then renamed into place
/// to avoid corruption from partial writes.
pub fn save_state(state: &PersistedState) -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;

    let state_path = dir.join("state.json");
    log::debug!("persistence: save_state path={:?}", state_path);
    let tmp_path = dir.join("state.json.tmp");

    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, &state_path)?;
    Ok(())
}

/// Load the persisted state from disk.
///
/// Returns `Ok(None)` if no state file exists. Returns an error if the file
/// exists but cannot be read or parsed.
pub fn load_state() -> Result<Option<PersistedState>> {
    let state_path = data_dir().join("state.json");
    log::debug!("persistence: load_state path={:?}", state_path);
    load_state_from(&state_path, legacy_state_path().as_deref())
}

/// [`load_state`] against explicit paths, so the migration below is reachable from
/// a test on a platform where the two real paths are always the same one.
fn load_state_from(state_path: &Path, legacy: Option<&Path>) -> Result<Option<PersistedState>> {
    if !state_path.exists() {
        // A macOS user who exports `XDG_DATA_HOME` had their sessions saved to the
        // PLATFORM directory, because `dirs::data_dir()` ignores that variable
        // there. Honouring it moves the path, so a first start after the change
        // would find nothing and read as "all my sessions are gone". Read the old
        // file instead, once, and let the next `save_state` write the new path.
        //
        // Read only, deliberately: the old file is not moved or deleted. It costs
        // nothing to leave, and leaving it is what makes a downgrade to an earlier
        // binary land back on a state that still exists.
        if let Some(legacy) = legacy.filter(|p| p.exists()) {
            log::info!(
                "persistence: no state at {}; loading the pre-XDG state from {}",
                state_path.display(),
                legacy.display()
            );
            let json = std::fs::read_to_string(legacy)?;
            let state: PersistedState = serde_json::from_str(&json)?;
            return Ok(Some(state));
        }
        return Ok(None);
    }

    let json = std::fs::read_to_string(state_path)?;
    let state: PersistedState = serde_json::from_str(&json)?;
    Ok(Some(state))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_persisted_state_roundtrip() {
        let mut state = ServerState::new();
        state
            .create_session(
                "test",
                None,
                crate::config::BorderStyle::ZellijStyle,
                Default::default(),
                (80, 80),
            )
            .unwrap();

        let mut cwds = HashMap::new();
        cwds.insert(1u64, "/home/user".to_string());

        let persisted = PersistedState::from_server(&state, &cwds).unwrap();

        let json = serde_json::to_string(&persisted).unwrap();
        let loaded: PersistedState = serde_json::from_str(&json).unwrap();

        assert!(loaded.state.sessions.contains_key("test"));
        assert_eq!(loaded.pane_cwds.get(&1).unwrap(), "/home/user");
    }

    #[test]
    fn test_persisted_state_from_server_clones_state() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s1",
                Some("work"),
                crate::config::BorderStyle::ZellijStyle,
                Default::default(),
                (80, 80),
            )
            .unwrap();
        state.create_tab("s1", "tab2", Default::default()).unwrap();

        let cwds = HashMap::new();
        let persisted = PersistedState::from_server(&state, &cwds).unwrap();

        assert!(persisted.state.sessions.contains_key("s1"));
        assert!(persisted.state.folders.contains_key("work"));
        let sess = persisted.state.sessions.get("s1").unwrap();
        assert_eq!(sess.tabs.len(), 2);
    }

    #[test]
    fn test_save_and_load_state() {
        // Use a temp directory to avoid polluting real data dir.
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");

        let mut state = ServerState::new();
        state
            .create_session(
                "persist-test",
                None,
                crate::config::BorderStyle::ZellijStyle,
                Default::default(),
                (80, 80),
            )
            .unwrap();

        let mut cwds = HashMap::new();
        cwds.insert(1u64, "/tmp".to_string());

        let persisted = PersistedState::from_server(&state, &cwds).unwrap();
        let json = serde_json::to_string_pretty(&persisted).unwrap();
        std::fs::write(&state_path, &json).unwrap();

        let loaded_json = std::fs::read_to_string(&state_path).unwrap();
        let loaded: PersistedState = serde_json::from_str(&loaded_json).unwrap();

        assert!(loaded.state.sessions.contains_key("persist-test"));
        assert_eq!(loaded.pane_cwds.get(&1).unwrap(), "/tmp");
    }

    #[test]
    fn test_load_state_no_file() {
        // load_state returns None when the file doesn't exist.
        // We can't easily test this without mocking the path, but we verify
        // the function signature works correctly.
        let result = load_state();
        // This might return Some or None depending on the test environment,
        // but it should not error.
        assert!(result.is_ok());
    }

    /// The migration must not fire when the two directories are the same one,
    /// which is every Linux run and every macOS run with `XDG_DATA_HOME` unset.
    /// A `Some` here would make the loader read a file it had just declined.
    #[test]
    fn no_legacy_path_when_the_platform_directory_is_the_current_one() {
        let current = Path::new("/home/u/.local/share/remux/state.json");
        assert_eq!(
            legacy_of(current, Some(Path::new("/home/u/.local/share"))),
            None
        );
        assert_eq!(legacy_of(current, None), None);
    }

    /// The macOS case: `XDG_DATA_HOME` is exported, so the two disagree.
    #[test]
    fn the_platform_directory_is_the_legacy_path_when_it_differs() {
        assert_eq!(
            legacy_of(
                Path::new("/srv/xdg/remux/state.json"),
                Some(Path::new("/home/u/Library/Application Support")),
            ),
            Some(PathBuf::from(
                "/home/u/Library/Application Support/remux/state.json"
            ))
        );
    }

    fn write_state(path: &Path, session: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut state = ServerState::new();
        state
            .create_session(
                session,
                None,
                crate::config::BorderStyle::ZellijStyle,
                Default::default(),
                (80, 80),
            )
            .unwrap();
        let persisted = PersistedState::from_server(&state, &HashMap::new()).unwrap();
        std::fs::write(path, serde_json::to_string(&persisted).unwrap()).unwrap();
    }

    fn only_session(loaded: Option<PersistedState>) -> String {
        let persisted = loaded.expect("a state file should have been read");
        let mut names: Vec<String> = persisted
            .state
            .sessions
            .values()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names.len(), 1, "expected one session, got {names:?}");
        names.remove(0)
    }

    /// The whole point of the migration: sessions saved before `XDG_DATA_HOME` was
    /// honoured are still found at the platform path.
    #[test]
    fn the_legacy_state_is_read_when_the_current_path_has_none() {
        let root = std::env::temp_dir().join(format!("rmx-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let current = root.join("xdg/remux/state.json");
        let legacy = root.join("platform/remux/state.json");
        write_state(&legacy, "from_legacy");

        let loaded = load_state_from(&current, Some(&legacy)).unwrap();
        assert_eq!(only_session(loaded), "from_legacy");
        assert!(
            legacy.exists(),
            "the migration must not move or delete the old file"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ...and once the new path exists it wins, so the stale copy left behind
    /// cannot resurrect sessions the user has since closed.
    #[test]
    fn the_current_state_wins_over_a_legacy_file() {
        let root = std::env::temp_dir().join(format!("rmx-mig2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let current = root.join("xdg/remux/state.json");
        let legacy = root.join("platform/remux/state.json");
        write_state(&current, "from_current");
        write_state(&legacy, "from_legacy");

        let loaded = load_state_from(&current, Some(&legacy)).unwrap();
        assert_eq!(only_session(loaded), "from_current");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_is_read_when_neither_path_has_a_state_file() {
        let root = std::env::temp_dir().join(format!("rmx-mig3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let loaded =
            load_state_from(&root.join("a/state.json"), Some(&root.join("b/state.json"))).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn test_save_command_constant() {
        assert_eq!(SAVE_COMMAND, "session_save");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_get_pane_cwd_self() {
        // Reading our own process's cwd should match the current directory.
        let pid = nix::unistd::Pid::from_raw(std::process::id() as i32);
        let cwd = get_pane_cwd(pid).expect("should read own cwd");
        let expected = std::env::current_dir().unwrap();
        // Canonicalize both sides so symlinked temp/working dirs compare equal.
        let got = std::fs::canonicalize(&cwd).unwrap();
        let expected = std::fs::canonicalize(&expected).unwrap();
        assert_eq!(got, expected);
    }
}
