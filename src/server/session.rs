//! Session model for Remux terminal multiplexer.
//!
//! This module manages the bookkeeping for sessions, folders, and tabs.
//! It is pure -- no PTY management, no I/O -- just state management.

use std::collections::HashMap;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::layout::{self, LayoutNode, PaneId};

/// Unique identifier for a session (its name).
pub type SessionId = String;

/// Unique identifier for a folder (its name).
pub type FolderId = String;

/// Unique identifier for a tab.
pub type TabId = u64;

/// Summary information about a session, returned by listing operations.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    pub folder: Option<String>,
    pub tab_count: usize,
    pub pane_count: usize,
}

/// Summary information about a folder, returned by listing operations.
#[derive(Debug, Clone)]
pub struct FolderInfo {
    pub name: String,
    pub session_count: usize,
}

/// Top-level server state containing all sessions and folders.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServerState {
    pub folders: HashMap<FolderId, Folder>,
    pub sessions: HashMap<SessionId, Session>,
    next_pane_id: u64,
    next_tab_id: u64,
}

/// A folder groups related sessions together.
#[derive(Debug, Serialize, Deserialize)]
pub struct Folder {
    pub name: String,
    pub session_ids: Vec<SessionId>,
}

/// A session contains one or more tabs.
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub folder: Option<FolderId>,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

/// A tab holds a layout tree and tracks the focused pane.
#[derive(Debug, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub name: String,
    pub layout: LayoutNode,
    pub focused_pane: PaneId,
}

impl ServerState {
    /// Create a new empty server state.
    pub fn new() -> Self {
        ServerState {
            folders: HashMap::new(),
            sessions: HashMap::new(),
            next_pane_id: 1,
            next_tab_id: 1,
        }
    }

    /// Allocate the next pane ID (monotonically increasing).
    pub fn next_pane_id(&mut self) -> PaneId {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        id
    }

    /// Allocate the next tab ID (monotonically increasing).
    fn next_tab_id(&mut self) -> TabId {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    // -----------------------------------------------------------------------
    // Session CRUD
    // -----------------------------------------------------------------------

    /// Create a new session, optionally in a folder.
    ///
    /// The session starts with one tab containing one pane. If a folder is
    /// specified and does not exist, it is created automatically.
    ///
    /// Returns the initial pane ID.
    pub fn create_session(&mut self, name: &str, folder: Option<&str>) -> Result<PaneId> {
        if self.sessions.contains_key(name) {
            bail!("session '{}' already exists", name);
        }

        let pane_id = self.next_pane_id();
        let tab_id = self.next_tab_id();

        let folder_id = if let Some(folder_name) = folder {
            // Create folder if it doesn't exist.
            if !self.folders.contains_key(folder_name) {
                self.folders.insert(
                    folder_name.to_string(),
                    Folder {
                        name: folder_name.to_string(),
                        session_ids: Vec::new(),
                    },
                );
            }
            let f = self
                .folders
                .get_mut(folder_name)
                .expect("folder was just created or already exists");
            f.session_ids.push(name.to_string());
            Some(folder_name.to_string())
        } else {
            None
        };

        let tab = Tab {
            id: tab_id,
            name: format!("tab-{}", tab_id),
            layout: LayoutNode::new_stack(pane_id),
            focused_pane: pane_id,
        };

        let session = Session {
            name: name.to_string(),
            folder: folder_id,
            tabs: vec![tab],
            active_tab: 0,
        };

        self.sessions.insert(name.to_string(), session);
        Ok(pane_id)
    }

    /// Rename a session. The new name must be unique.
    pub fn rename_session(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        if old_name == new_name {
            return Ok(());
        }
        if self.sessions.contains_key(new_name) {
            bail!("session '{}' already exists", new_name);
        }
        let mut session = self
            .sessions
            .remove(old_name)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", old_name))?;

        // Update folder reference.
        if let Some(ref folder_id) = session.folder {
            if let Some(folder) = self.folders.get_mut(folder_id) {
                if let Some(pos) = folder.session_ids.iter().position(|s| s == old_name) {
                    folder.session_ids[pos] = new_name.to_string();
                }
            }
        }

        session.name = new_name.to_string();
        self.sessions.insert(new_name.to_string(), session);
        Ok(())
    }

    /// Delete a session. Returns all pane IDs that need cleanup (e.g., PTY
    /// teardown).
    pub fn delete_session(&mut self, name: &str) -> Result<Vec<PaneId>> {
        let session = self
            .sessions
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", name))?;

        // Remove from folder.
        if let Some(ref folder_id) = session.folder {
            if let Some(folder) = self.folders.get_mut(folder_id) {
                folder.session_ids.retain(|s| s != name);
            }
        }

        // Collect all pane IDs across all tabs.
        let mut pane_ids = Vec::new();
        for tab in &session.tabs {
            pane_ids.extend(layout::all_pane_ids(&tab.layout));
        }

        Ok(pane_ids)
    }

    /// List all sessions with summary information.
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self
            .sessions
            .values()
            .map(|s| {
                let pane_count: usize = s
                    .tabs
                    .iter()
                    .map(|t| layout::all_pane_ids(&t.layout).len())
                    .sum();
                SessionInfo {
                    name: s.name.clone(),
                    folder: s.folder.clone(),
                    tab_count: s.tabs.len(),
                    pane_count,
                }
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    // -----------------------------------------------------------------------
    // Folder CRUD
    // -----------------------------------------------------------------------

    /// Create a new folder.
    pub fn create_folder(&mut self, name: &str) -> Result<()> {
        if self.folders.contains_key(name) {
            bail!("folder '{}' already exists", name);
        }
        self.folders.insert(
            name.to_string(),
            Folder {
                name: name.to_string(),
                session_ids: Vec::new(),
            },
        );
        Ok(())
    }

    /// Rename a folder.
    pub fn rename_folder(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        if old_name == new_name {
            return Ok(());
        }
        if self.folders.contains_key(new_name) {
            bail!("folder '{}' already exists", new_name);
        }
        let mut folder = self
            .folders
            .remove(old_name)
            .ok_or_else(|| anyhow::anyhow!("folder '{}' not found", old_name))?;

        // Update all sessions that reference this folder.
        for session_id in &folder.session_ids {
            if let Some(session) = self.sessions.get_mut(session_id) {
                session.folder = Some(new_name.to_string());
            }
        }

        folder.name = new_name.to_string();
        self.folders.insert(new_name.to_string(), folder);
        Ok(())
    }

    /// Delete a folder. The folder must be empty (no sessions).
    pub fn delete_folder(&mut self, name: &str) -> Result<()> {
        let folder = self
            .folders
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("folder '{}' not found", name))?;

        if !folder.session_ids.is_empty() {
            bail!(
                "folder '{}' is not empty (contains {} sessions)",
                name,
                folder.session_ids.len()
            );
        }

        self.folders.remove(name);
        Ok(())
    }

    /// List all folders with summary information.
    pub fn list_folders(&self) -> Vec<FolderInfo> {
        let mut infos: Vec<FolderInfo> = self
            .folders
            .values()
            .map(|f| FolderInfo {
                name: f.name.clone(),
                session_count: f.session_ids.len(),
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    // -----------------------------------------------------------------------
    // Tab CRUD
    // -----------------------------------------------------------------------

    /// Create a new tab in the given session. Returns the initial pane ID.
    pub fn create_tab(&mut self, session: &str, name: &str) -> Result<PaneId> {
        let pane_id = self.next_pane_id();
        let tab_id = self.next_tab_id();

        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        let tab = Tab {
            id: tab_id,
            name: name.to_string(),
            layout: LayoutNode::new_stack(pane_id),
            focused_pane: pane_id,
        };

        sess.tabs.push(tab);
        sess.active_tab = sess.tabs.len() - 1;
        Ok(pane_id)
    }

    /// Close a tab by index. Returns the pane IDs that need cleanup and
    /// whether the session was deleted (if it was the last tab).
    pub fn close_tab(&mut self, session: &str, tab_idx: usize) -> Result<(Vec<PaneId>, bool)> {
        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        if tab_idx >= sess.tabs.len() {
            bail!(
                "tab index {} out of range (session has {} tabs)",
                tab_idx,
                sess.tabs.len()
            );
        }

        let tab = sess.tabs.remove(tab_idx);
        let pane_ids = layout::all_pane_ids(&tab.layout);

        if sess.tabs.is_empty() {
            // Last tab -- delete the session.
            // We need to remove the session from its folder too.
            let session_name = session.to_string();
            let folder_id = sess.folder.clone();

            self.sessions.remove(&session_name);

            if let Some(ref fid) = folder_id {
                if let Some(folder) = self.folders.get_mut(fid) {
                    folder.session_ids.retain(|s| s != &session_name);
                }
            }

            return Ok((pane_ids, true));
        }

        // Adjust active_tab if needed.
        if sess.active_tab >= sess.tabs.len() {
            sess.active_tab = sess.tabs.len() - 1;
        } else if sess.active_tab > tab_idx {
            sess.active_tab -= 1;
        }

        Ok((pane_ids, false))
    }

    /// Rename a tab by index.
    pub fn rename_tab(&mut self, session: &str, tab_idx: usize, new_name: &str) -> Result<()> {
        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        let tab = sess
            .tabs
            .get_mut(tab_idx)
            .ok_or_else(|| anyhow::anyhow!("tab index {} out of range", tab_idx))?;

        tab.name = new_name.to_string();
        Ok(())
    }

    /// Navigate to a tab by index.
    pub fn goto_tab(&mut self, session: &str, tab_idx: usize) -> Result<()> {
        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        if tab_idx >= sess.tabs.len() {
            bail!(
                "tab index {} out of range (session has {} tabs)",
                tab_idx,
                sess.tabs.len()
            );
        }

        sess.active_tab = tab_idx;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Session movement
    // -----------------------------------------------------------------------

    /// Move a session to a different folder (or to top-level if `None`).
    pub fn move_session(&mut self, session_name: &str, target_folder: Option<&str>) -> Result<()> {
        let sess = self
            .sessions
            .get_mut(session_name)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session_name))?;

        let old_folder = sess.folder.clone();

        // Remove from old folder.
        if let Some(ref old_fid) = old_folder {
            if let Some(folder) = self.folders.get_mut(old_fid) {
                folder.session_ids.retain(|s| s != session_name);
            }
        }

        // Add to new folder.
        match target_folder {
            Some(folder_name) => {
                // Create folder if it doesn't exist.
                if !self.folders.contains_key(folder_name) {
                    self.folders.insert(
                        folder_name.to_string(),
                        Folder {
                            name: folder_name.to_string(),
                            session_ids: Vec::new(),
                        },
                    );
                }
                let folder = self
                    .folders
                    .get_mut(folder_name)
                    .expect("folder was just created or already exists");
                folder.session_ids.push(session_name.to_string());

                // Re-borrow session mutably.
                let sess = self.sessions.get_mut(session_name).expect("session exists");
                sess.folder = Some(folder_name.to_string());
            }
            None => {
                let sess = self.sessions.get_mut(session_name).expect("session exists");
                sess.folder = None;
            }
        }

        Ok(())
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_server_state() {
        let state = ServerState::new();
        assert!(state.sessions.is_empty());
        assert!(state.folders.is_empty());
    }

    #[test]
    fn test_next_pane_id() {
        let mut state = ServerState::new();
        assert_eq!(state.next_pane_id(), 1);
        assert_eq!(state.next_pane_id(), 2);
        assert_eq!(state.next_pane_id(), 3);
    }

    #[test]
    fn test_create_session() {
        let mut state = ServerState::new();
        let pane_id = state.create_session("test", None).unwrap();
        assert_eq!(pane_id, 1);

        let sess = state.sessions.get("test").unwrap();
        assert_eq!(sess.name, "test");
        assert!(sess.folder.is_none());
        assert_eq!(sess.tabs.len(), 1);
        assert_eq!(sess.active_tab, 0);
    }

    #[test]
    fn test_create_session_with_folder() {
        let mut state = ServerState::new();
        state.create_session("test", Some("work")).unwrap();

        assert!(state.folders.contains_key("work"));
        let folder = state.folders.get("work").unwrap();
        assert_eq!(folder.session_ids, vec!["test"]);

        let sess = state.sessions.get("test").unwrap();
        assert_eq!(sess.folder, Some("work".to_string()));
    }

    #[test]
    fn test_create_session_duplicate_name() {
        let mut state = ServerState::new();
        state.create_session("test", None).unwrap();
        let result = state.create_session("test", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_rename_session() {
        let mut state = ServerState::new();
        state.create_session("old", Some("folder")).unwrap();
        state.rename_session("old", "new").unwrap();

        assert!(!state.sessions.contains_key("old"));
        assert!(state.sessions.contains_key("new"));

        let folder = state.folders.get("folder").unwrap();
        assert!(folder.session_ids.contains(&"new".to_string()));
        assert!(!folder.session_ids.contains(&"old".to_string()));
    }

    #[test]
    fn test_rename_session_duplicate() {
        let mut state = ServerState::new();
        state.create_session("a", None).unwrap();
        state.create_session("b", None).unwrap();

        let result = state.rename_session("a", "b");
        assert!(result.is_err());
    }

    #[test]
    fn test_rename_session_same_name() {
        let mut state = ServerState::new();
        state.create_session("a", None).unwrap();
        state.rename_session("a", "a").unwrap();
    }

    #[test]
    fn test_delete_session() {
        let mut state = ServerState::new();
        state.create_session("test", Some("folder")).unwrap();
        let pane_ids = state.delete_session("test").unwrap();

        assert_eq!(pane_ids, vec![1]);
        assert!(!state.sessions.contains_key("test"));

        let folder = state.folders.get("folder").unwrap();
        assert!(folder.session_ids.is_empty());
    }

    #[test]
    fn test_delete_session_not_found() {
        let mut state = ServerState::new();
        assert!(state.delete_session("nope").is_err());
    }

    #[test]
    fn test_list_sessions() {
        let mut state = ServerState::new();
        state.create_session("b", None).unwrap();
        state.create_session("a", Some("f")).unwrap();

        let list = state.list_sessions();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "a");
        assert_eq!(list[0].folder, Some("f".to_string()));
        assert_eq!(list[1].name, "b");
        assert!(list[1].folder.is_none());
    }

    #[test]
    fn test_create_folder() {
        let mut state = ServerState::new();
        state.create_folder("work").unwrap();
        assert!(state.folders.contains_key("work"));
    }

    #[test]
    fn test_create_folder_duplicate() {
        let mut state = ServerState::new();
        state.create_folder("work").unwrap();
        assert!(state.create_folder("work").is_err());
    }

    #[test]
    fn test_rename_folder() {
        let mut state = ServerState::new();
        state.create_folder("old").unwrap();
        state.create_session("s", Some("old")).unwrap();
        state.rename_folder("old", "new").unwrap();

        assert!(!state.folders.contains_key("old"));
        assert!(state.folders.contains_key("new"));

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.folder, Some("new".to_string()));
    }

    #[test]
    fn test_delete_folder_empty() {
        let mut state = ServerState::new();
        state.create_folder("work").unwrap();
        state.delete_folder("work").unwrap();
        assert!(!state.folders.contains_key("work"));
    }

    #[test]
    fn test_delete_folder_not_empty() {
        let mut state = ServerState::new();
        state.create_session("s", Some("work")).unwrap();
        assert!(state.delete_folder("work").is_err());
    }

    #[test]
    fn test_list_folders() {
        let mut state = ServerState::new();
        state.create_folder("b").unwrap();
        state.create_folder("a").unwrap();

        let list = state.list_folders();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "a");
        assert_eq!(list[1].name, "b");
    }

    #[test]
    fn test_create_tab() {
        let mut state = ServerState::new();
        state.create_session("s", None).unwrap();
        let pane_id = state.create_tab("s", "new-tab").unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs.len(), 2);
        assert_eq!(sess.active_tab, 1);
        assert_eq!(sess.tabs[1].name, "new-tab");
        assert_eq!(sess.tabs[1].focused_pane, pane_id);
    }

    #[test]
    fn test_close_tab() {
        let mut state = ServerState::new();
        state.create_session("s", None).unwrap();
        state.create_tab("s", "tab2").unwrap();

        let (pane_ids, deleted) = state.close_tab("s", 0).unwrap();
        assert!(!deleted);
        assert_eq!(pane_ids.len(), 1);

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs.len(), 1);
        assert_eq!(sess.active_tab, 0);
    }

    #[test]
    fn test_close_last_tab_deletes_session() {
        let mut state = ServerState::new();
        state.create_session("s", Some("f")).unwrap();

        let (pane_ids, deleted) = state.close_tab("s", 0).unwrap();
        assert!(deleted);
        assert_eq!(pane_ids.len(), 1);
        assert!(!state.sessions.contains_key("s"));

        // Session should be removed from folder too.
        let folder = state.folders.get("f").unwrap();
        assert!(folder.session_ids.is_empty());
    }

    #[test]
    fn test_rename_tab() {
        let mut state = ServerState::new();
        state.create_session("s", None).unwrap();
        state.rename_tab("s", 0, "renamed").unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs[0].name, "renamed");
    }

    #[test]
    fn test_goto_tab() {
        let mut state = ServerState::new();
        state.create_session("s", None).unwrap();
        state.create_tab("s", "tab2").unwrap();
        state.goto_tab("s", 0).unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.active_tab, 0);
    }

    #[test]
    fn test_goto_tab_out_of_range() {
        let mut state = ServerState::new();
        state.create_session("s", None).unwrap();
        assert!(state.goto_tab("s", 5).is_err());
    }

    #[test]
    fn test_move_session_to_folder() {
        let mut state = ServerState::new();
        state.create_session("s", None).unwrap();
        state.move_session("s", Some("new-folder")).unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.folder, Some("new-folder".to_string()));

        let folder = state.folders.get("new-folder").unwrap();
        assert!(folder.session_ids.contains(&"s".to_string()));
    }

    #[test]
    fn test_move_session_between_folders() {
        let mut state = ServerState::new();
        state.create_session("s", Some("old")).unwrap();
        state.move_session("s", Some("new")).unwrap();

        let old_folder = state.folders.get("old").unwrap();
        assert!(old_folder.session_ids.is_empty());

        let new_folder = state.folders.get("new").unwrap();
        assert!(new_folder.session_ids.contains(&"s".to_string()));
    }

    #[test]
    fn test_move_session_to_top_level() {
        let mut state = ServerState::new();
        state.create_session("s", Some("folder")).unwrap();
        state.move_session("s", None).unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert!(sess.folder.is_none());

        let folder = state.folders.get("folder").unwrap();
        assert!(folder.session_ids.is_empty());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut state = ServerState::new();
        state.create_session("s1", Some("work")).unwrap();
        state.create_tab("s1", "tab2").unwrap();
        state.create_session("s2", None).unwrap();

        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: ServerState = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.sessions.len(), 2);
        assert!(deserialized.sessions.contains_key("s1"));
        assert!(deserialized.sessions.contains_key("s2"));
        assert!(deserialized.folders.contains_key("work"));
    }
}
