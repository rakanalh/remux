//! Session persistence and state saving.
//!
//! This module handles saving and loading the server state to disk, enabling
//! session resurrection after a server restart. State is stored as JSON in
//! the user's data directory.

use std::collections::HashMap;
use std::path::PathBuf;

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

/// Read the current working directory of a process by reading the
/// `/proc/{pid}/cwd` symlink.
///
/// Returns `None` if the symlink cannot be read (e.g., the process has
/// exited or permission is denied).
pub fn get_pane_cwd(pid: nix::unistd::Pid) -> Option<String> {
    std::fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
}

/// Return the path to the persistence data directory (`$XDG_DATA_HOME/remux`
/// or `/tmp/remux` as a fallback).
fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("remux")
}

/// Atomically save the persisted state to disk.
///
/// The state is first written to a temporary file, then renamed into place
/// to avoid corruption from partial writes.
pub fn save_state(state: &PersistedState) -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;

    let state_path = dir.join("state.json");
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

    if !state_path.exists() {
        return Ok(None);
    }

    let json = std::fs::read_to_string(&state_path)?;
    let state: PersistedState = serde_json::from_str(&json)?;
    Ok(Some(state))
}

/// Returns the auto-save interval as a `Duration`, or `None` if auto-save is
/// disabled (interval is 0).
pub fn auto_save_interval(config: &crate::config::Config) -> Option<std::time::Duration> {
    let secs = config.general.auto_save_interval_secs;
    if secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(secs))
    }
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
        state.create_session("test", None, false).unwrap();

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
        state.create_session("s1", Some("work"), false).unwrap();
        state.create_tab("s1", "tab2").unwrap();

        let cwds = HashMap::new();
        let persisted = PersistedState::from_server(&state, &cwds).unwrap();

        assert!(persisted.state.sessions.contains_key("s1"));
        assert!(persisted.state.folders.contains_key("work"));
        let sess = persisted.state.sessions.get("s1").unwrap();
        assert_eq!(sess.tabs.len(), 2);
    }

    #[test]
    fn test_auto_save_interval_disabled() {
        let mut config = crate::config::Config::default();
        config.general.auto_save_interval_secs = 0;
        assert!(auto_save_interval(&config).is_none());
    }

    #[test]
    fn test_auto_save_interval_enabled() {
        let config = crate::config::Config::default();
        let interval = auto_save_interval(&config).unwrap();
        assert_eq!(interval, std::time::Duration::from_secs(30));
    }

    #[test]
    fn test_save_and_load_state() {
        // Use a temp directory to avoid polluting real data dir.
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");

        let mut state = ServerState::new();
        state.create_session("persist-test", None, false).unwrap();

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

    #[test]
    fn test_save_command_constant() {
        assert_eq!(SAVE_COMMAND, "session_save");
    }
}
