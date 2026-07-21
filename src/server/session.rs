//! Session model for Remux terminal multiplexer.
//!
//! This module manages the bookkeeping for sessions, folders, and tabs.
//! It is pure -- no PTY management, no I/O -- just state management.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::layout::{self, LayoutMode, LayoutNode, PaneId};
use crate::config::BorderStyle;
use crate::protocol::{FolderTreeEntry, PaneTreeEntry, SessionTreeEntry, TabTreeEntry};

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
    /// Border rendering style for this session. Initialized from
    /// `config.appearance.border_style` and toggled at runtime.
    #[serde(default = "default_border_style")]
    pub border_style: BorderStyle,
    /// Tracks an in-progress pane rename: (pane_id, original_name).
    /// Present only while a client is actively typing a new name.
    #[serde(skip)]
    pub rename_state: Option<(PaneId, String)>,
}

/// Per-tab activity state for background activity monitoring (tmux-like
/// `monitor-activity` / `monitor-silence`).
///
/// Only ever applies to *background* tabs (a tab that is not its session's
/// `active_tab`); the foreground tab is always [`TabActivity::None`] because it
/// is being viewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabActivity {
    /// No pending activity (default / cleared on focus).
    #[default]
    None,
    /// The tab produced new output while in the background ("needs attention").
    Activity,
    /// The tab emitted a terminal bell (BEL). Takes precedence over `Activity`
    /// until the tab is focused.
    Bell,
    /// The tab was active but has since gone quiet ("finished").
    Silent,
}

/// A tab holds a layout tree and tracks the focused pane.
#[derive(Debug, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub name: String,
    pub layout: LayoutNode,
    pub focused_pane: PaneId,
    #[serde(default)]
    pub layout_mode: LayoutMode,
    #[serde(default)]
    pub pane_order: Vec<PaneId>,
    #[serde(default)]
    pub zoomed_pane: Option<PaneId>,
    /// Runtime-only background activity state. Not persisted: activity is a
    /// live-session concern, meaningless once a session is dormant/restored.
    #[serde(skip)]
    pub activity: TabActivity,
    /// Runtime-only timestamp of the most recent background output, used to
    /// promote `Activity` to `Silent` after a quiet threshold. Not persisted.
    #[serde(skip)]
    pub last_output: Option<Instant>,
}

/// Return true if a tab currently in [`TabActivity::Activity`] should be
/// promoted to [`TabActivity::Silent`] given the current time `now` and the
/// silence `threshold`.
///
/// Pure and deterministic: takes an injected `now` so the promotion logic can
/// be unit-tested without real sleeps. Only `Activity` is eligible — `Bell`
/// stays `Bell`, and `None`/`Silent` are never promoted.
pub fn should_promote_to_silent(
    activity: TabActivity,
    last_output: Option<Instant>,
    now: Instant,
    threshold: Duration,
) -> bool {
    matches!(activity, TabActivity::Activity)
        && last_output
            .map(|t| now.duration_since(t) >= threshold)
            .unwrap_or(false)
}

fn default_border_style() -> BorderStyle {
    BorderStyle::ZellijStyle
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

    /// Ensure the pane and tab ID counters are higher than any existing ID.
    ///
    /// This is used after restoring persisted state to guard against
    /// corruption where the counters might be lower than the max used ID.
    pub fn ensure_id_counters(&mut self) {
        let max_pane = self
            .sessions
            .values()
            .flat_map(|s| s.tabs.iter())
            .flat_map(|t| layout::all_pane_ids(&t.layout))
            .max()
            .unwrap_or(0);
        let max_tab = self
            .sessions
            .values()
            .flat_map(|s| s.tabs.iter())
            .map(|t| t.id)
            .max()
            .unwrap_or(0);
        if self.next_pane_id <= max_pane {
            self.next_pane_id = max_pane + 1;
        }
        if self.next_tab_id <= max_tab {
            self.next_tab_id = max_tab + 1;
        }
    }

    /// Raise this state's pane/tab id counters so future allocations never
    /// collide with any id used by `other`.
    ///
    /// Used when a dormant snapshot is loaded alongside a fresh live state
    /// (`automatic_restore = false`): sessions created before a resurrect must
    /// allocate ids above the *entire* dormant id range, otherwise a
    /// resurrected pane/tab id would clash with a live one in the global pane
    /// map. Reserves above both `other`'s used ids and its own next counters.
    pub fn reserve_ids_above(&mut self, other: &ServerState) {
        let other_max_pane = other
            .sessions
            .values()
            .flat_map(|s| s.tabs.iter())
            .flat_map(|t| layout::all_pane_ids(&t.layout))
            .max()
            .unwrap_or(0)
            .max(other.next_pane_id.saturating_sub(1));
        let other_max_tab = other
            .sessions
            .values()
            .flat_map(|s| s.tabs.iter())
            .map(|t| t.id)
            .max()
            .unwrap_or(0)
            .max(other.next_tab_id.saturating_sub(1));
        if self.next_pane_id <= other_max_pane {
            self.next_pane_id = other_max_pane + 1;
        }
        if self.next_tab_id <= other_max_tab {
            self.next_tab_id = other_max_tab + 1;
        }
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
    pub fn create_session(
        &mut self,
        name: &str,
        folder: Option<&str>,
        border_style: BorderStyle,
        layout_mode: LayoutMode,
    ) -> Result<PaneId> {
        log::debug!(
            "session: create_session name={:?}, folder={:?}",
            name,
            folder
        );

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
            if !f.session_ids.contains(&name.to_string()) {
                f.session_ids.push(name.to_string());
            }
            Some(folder_name.to_string())
        } else {
            None
        };

        let tab = Tab {
            id: tab_id,
            name: "Tab 1".to_string(),
            layout: LayoutNode::new_stack(pane_id),
            focused_pane: pane_id,
            layout_mode,
            pane_order: vec![pane_id],
            zoomed_pane: None,
            activity: TabActivity::None,
            last_output: None,
        };

        let session = Session {
            name: name.to_string(),
            folder: folder_id,
            tabs: vec![tab],
            active_tab: 0,
            border_style,
            rename_state: None,
        };

        self.sessions.insert(name.to_string(), session);
        Ok(pane_id)
    }

    /// Rename a session. The new name must be unique.
    pub fn rename_session(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        log::debug!(
            "session: rename_session old={:?}, new={:?}",
            old_name,
            new_name
        );
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
        log::debug!("session: delete_session name={:?}", name);
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
        log::debug!("session: create_folder name={:?}", name);
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
        log::debug!(
            "session: rename_folder old={:?}, new={:?}",
            old_name,
            new_name
        );
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
        log::debug!("session: delete_folder name={:?}", name);
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

    /// Delete a folder and all sessions it contains (cascade).
    ///
    /// Returns a list of `(session_name, pane_ids)` for each deleted session
    /// so callers can clean up PTYs and notify clients.
    pub fn delete_folder_cascade(&mut self, name: &str) -> Result<Vec<(String, Vec<PaneId>)>> {
        log::debug!("session: delete_folder_cascade name={:?}", name);
        let folder = self
            .folders
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("folder '{}' not found", name))?;

        let mut deleted_sessions = Vec::new();
        for session_id in &folder.session_ids {
            if let Some(session) = self.sessions.remove(session_id) {
                let mut pane_ids = Vec::new();
                for tab in &session.tabs {
                    pane_ids.extend(layout::all_pane_ids(&tab.layout));
                }
                deleted_sessions.push((session_id.clone(), pane_ids));
            }
        }

        Ok(deleted_sessions)
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
    pub fn create_tab(
        &mut self,
        session: &str,
        name: &str,
        layout_mode: LayoutMode,
    ) -> Result<PaneId> {
        log::debug!("session: create_tab name={:?}, session={:?}", name, session);
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
            layout_mode,
            pane_order: vec![pane_id],
            zoomed_pane: None,
            activity: TabActivity::None,
            last_output: None,
        };

        sess.tabs.push(tab);
        sess.active_tab = sess.tabs.len() - 1;
        Ok(pane_id)
    }

    /// Close a tab by index. Returns the pane IDs that need cleanup and
    /// whether the session was deleted (if it was the last tab).
    pub fn close_tab(&mut self, session: &str, tab_idx: usize) -> Result<(Vec<PaneId>, bool)> {
        log::debug!(
            "session: close_tab index={}, session={:?}",
            tab_idx,
            session
        );
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

        // Closing the active tab moves focus to a different existing tab that
        // may carry stale background activity; clear it (harmless no-op when
        // the surviving active tab was already clean).
        if let Some(tab) = sess.tabs.get_mut(sess.active_tab) {
            tab.activity = TabActivity::None;
            tab.last_output = None;
        }

        Ok((pane_ids, false))
    }

    /// Rename a tab by index.
    pub fn rename_tab(&mut self, session: &str, tab_idx: usize, new_name: &str) -> Result<()> {
        log::debug!(
            "session: rename_tab index={}, new_name={:?}, session={:?}",
            tab_idx,
            new_name,
            session
        );
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

    /// Move a tab (identified by `tab_idx`) left/right by `delta` positions
    /// within its session's tab vector.
    ///
    /// The destination index is clamped to `[0, len - 1]`, so out-of-range
    /// deltas saturate at the ends rather than erroring. `active_tab` is
    /// preserved to keep pointing at the *same* tab it did before the move
    /// (tracked by tab id), regardless of whether the moved tab was the active
    /// one. A no-op move (destination == source) returns `Ok(())` unchanged.
    pub fn move_tab(&mut self, session: &str, tab_idx: usize, delta: i32) -> Result<()> {
        log::debug!(
            "session: move_tab index={}, delta={}, session={:?}",
            tab_idx,
            delta,
            session
        );
        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        let len = sess.tabs.len();
        if tab_idx >= len {
            bail!(
                "tab index {} out of range (session has {} tabs)",
                tab_idx,
                len
            );
        }

        // Clamp the destination to the valid range so large deltas saturate.
        let dest = (tab_idx as i32 + delta).clamp(0, len as i32 - 1) as usize;
        if dest == tab_idx {
            return Ok(());
        }

        // Remember which tab is active by identity so we can restore the index
        // after the reorder (the active tab may or may not be the moved one).
        let active_id = sess.tabs.get(sess.active_tab).map(|t| t.id);

        let tab = sess.tabs.remove(tab_idx);
        sess.tabs.insert(dest, tab);

        if let Some(active_id) = active_id {
            if let Some(pos) = sess.tabs.iter().position(|t| t.id == active_id) {
                sess.active_tab = pos;
            }
        }

        Ok(())
    }

    /// Navigate to a tab by index.
    pub fn goto_tab(&mut self, session: &str, tab_idx: usize) -> Result<()> {
        log::debug!("session: goto_tab index={}, session={:?}", tab_idx, session);
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
        // Clear activity for the newly-focused tab: it is now being viewed.
        if let Some(tab) = sess.tabs.get_mut(tab_idx) {
            tab.activity = TabActivity::None;
            tab.last_output = None;
        }
        Ok(())
    }

    /// Record background output for the tab that owns `pane_id`.
    ///
    /// If the owning tab is its session's `active_tab` (foreground / being
    /// viewed), this is a no-op — the foreground tab never accrues activity.
    /// Otherwise the tab's state is updated: `Bell` if `bell` is set (Bell wins
    /// and is never downgraded to `Activity`), else `Activity`. `last_output`
    /// is refreshed to `now` so the silence timer restarts on every new byte.
    pub fn record_pane_activity(&mut self, pane_id: PaneId, bell: bool, now: Instant) {
        for sess in self.sessions.values_mut() {
            let active = sess.active_tab;
            for (idx, tab) in sess.tabs.iter_mut().enumerate() {
                if layout::all_pane_ids(&tab.layout).contains(&pane_id) {
                    if idx == active {
                        // Foreground tab: being viewed, never accrues activity.
                        return;
                    }
                    if bell {
                        tab.activity = TabActivity::Bell;
                    } else if tab.activity != TabActivity::Bell {
                        // Don't downgrade a pending Bell to Activity. New output
                        // also revives a Silent tab back to Activity.
                        tab.activity = TabActivity::Activity;
                    }
                    tab.last_output = Some(now);
                    return;
                }
            }
        }
    }

    /// Promote any background tab that has been quietly in `Activity` past the
    /// `threshold` to `Silent` ("finished"). Returns the names of sessions that
    /// had at least one tab change, so the caller can re-render only those.
    ///
    /// `Bell` tabs are left untouched. `now` is injected for deterministic
    /// testing (see [`should_promote_to_silent`]).
    pub fn promote_silent_tabs(&mut self, now: Instant, threshold: Duration) -> Vec<String> {
        let mut affected = Vec::new();
        for (name, sess) in self.sessions.iter_mut() {
            let mut changed = false;
            for tab in sess.tabs.iter_mut() {
                if should_promote_to_silent(tab.activity, tab.last_output, now, threshold) {
                    tab.activity = TabActivity::Silent;
                    changed = true;
                }
            }
            if changed {
                affected.push(name.clone());
            }
        }
        affected
    }

    /// Clear activity on a session's currently-active tab. Used on attach/focus
    /// so a freshly-viewed tab never shows a stale marker.
    pub fn clear_active_tab_activity(&mut self, session: &str) {
        if let Some(sess) = self.sessions.get_mut(session) {
            let active = sess.active_tab;
            if let Some(tab) = sess.tabs.get_mut(active) {
                tab.activity = TabActivity::None;
                tab.last_output = None;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Session movement
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Session tree (for session manager)
    // -----------------------------------------------------------------------

    /// Build the full session tree hierarchy.
    ///
    /// Returns `(folders, unfiled)` where `folders` contains sessions grouped
    /// by folder, and `unfiled` contains sessions not in any folder.
    ///
    /// `current_session` marks which session the requesting client is attached
    /// to. `client_counts` maps session name to the number of clients attached.
    /// `pane_names` maps pane IDs to display names (e.g. process name).
    pub fn build_session_tree(
        &self,
        current_session: Option<&str>,
        client_counts: &HashMap<String, usize>,
        pane_names: &HashMap<PaneId, String>,
    ) -> (Vec<FolderTreeEntry>, Vec<SessionTreeEntry>) {
        let build_entry = |session: &Session| -> SessionTreeEntry {
            let tabs = session
                .tabs
                .iter()
                .map(|tab| {
                    let panes = layout::all_pane_ids(&tab.layout)
                        .into_iter()
                        .map(|pid| PaneTreeEntry {
                            id: pid,
                            name: pane_names
                                .get(&pid)
                                .cloned()
                                .unwrap_or_else(|| format!("pane-{}", pid)),
                            is_focused: pid == tab.focused_pane,
                        })
                        .collect();
                    TabTreeEntry {
                        id: tab.id,
                        name: tab.name.clone(),
                        panes,
                    }
                })
                .collect();
            SessionTreeEntry {
                name: session.name.clone(),
                tabs,
                client_count: client_counts.get(&session.name).copied().unwrap_or(0),
                is_current: current_session == Some(&session.name),
            }
        };

        let mut folders = Vec::new();
        for folder in self.folders.values() {
            let mut sessions = Vec::new();
            let mut seen = HashSet::new();
            for session_id in &folder.session_ids {
                if !seen.insert(session_id.clone()) {
                    continue; // skip duplicates
                }
                if let Some(session) = self.sessions.get(session_id) {
                    sessions.push(build_entry(session));
                }
            }
            sessions.sort_by(|a, b| a.name.cmp(&b.name));
            folders.push(FolderTreeEntry {
                name: folder.name.clone(),
                sessions,
            });
        }
        folders.sort_by(|a, b| a.name.cmp(&b.name));

        let mut unfiled = Vec::new();
        for session in self.sessions.values() {
            if session.folder.is_none() {
                unfiled.push(build_entry(session));
            }
        }
        unfiled.sort_by(|a, b| a.name.cmp(&b.name));

        (folders, unfiled)
    }

    // -----------------------------------------------------------------------
    // Session movement
    // -----------------------------------------------------------------------

    /// Move a session to a different folder (or to top-level if `None`).
    pub fn move_session(&mut self, session_name: &str, target_folder: Option<&str>) -> Result<()> {
        log::debug!(
            "session: move_session name={:?}, target_folder={:?}",
            session_name,
            target_folder
        );
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
                if !folder.session_ids.contains(&session_name.to_string()) {
                    folder.session_ids.push(session_name.to_string());
                }

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
    fn test_reserve_ids_above_prevents_collision() {
        // A dormant snapshot that used pane ids 1..=3.
        let mut dormant = ServerState::new();
        dormant
            .create_session(
                "alpha",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
        dormant
            .create_session(
                "beta",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();

        // A fresh live state would otherwise start allocating at pane id 1,
        // colliding with the dormant snapshot.
        let mut live = ServerState::new();
        live.reserve_ids_above(&dormant);

        // The next live pane id must exceed every id the dormant snapshot used.
        let next = live.next_pane_id();
        assert!(
            next >= dormant.next_pane_id,
            "live next_pane_id {next} should be >= dormant next {}",
            dormant.next_pane_id
        );
        assert!(next > 2);
    }

    /// Build a session named `s` with `n` tabs and return the ordered list of
    /// their tab ids. `create_tab` leaves `active_tab` pointing at the last tab.
    fn state_with_tabs(n: usize) -> (ServerState, Vec<TabId>) {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        for i in 1..n {
            state
                .create_tab("s", &format!("Tab {}", i + 1), LayoutMode::default())
                .unwrap();
        }
        let ids: Vec<TabId> = state.sessions["s"].tabs.iter().map(|t| t.id).collect();
        assert_eq!(ids.len(), n);
        (state, ids)
    }

    fn tab_order(state: &ServerState) -> Vec<TabId> {
        state.sessions["s"].tabs.iter().map(|t| t.id).collect()
    }

    #[test]
    fn test_move_tab_clamps_high_delta() {
        let (mut state, ids) = state_with_tabs(4);
        // Move the first tab far right; destination saturates at the last slot.
        state.move_tab("s", 0, 100).unwrap();
        assert_eq!(tab_order(&state), vec![ids[1], ids[2], ids[3], ids[0]]);
    }

    #[test]
    fn test_move_tab_clamps_low_delta() {
        let (mut state, ids) = state_with_tabs(4);
        // Move the last tab far left; destination saturates at the first slot.
        state.move_tab("s", 3, -100).unwrap();
        assert_eq!(tab_order(&state), vec![ids[3], ids[0], ids[1], ids[2]]);
    }

    #[test]
    fn test_move_tab_preserves_active_by_identity() {
        let (mut state, ids) = state_with_tabs(4);
        // Make tab index 1 the active one.
        state.sessions.get_mut("s").unwrap().active_tab = 1;
        // Move a *different* (non-active) tab across it: index 3 -> front.
        state.move_tab("s", 3, -3).unwrap();
        assert_eq!(tab_order(&state), vec![ids[3], ids[0], ids[1], ids[2]]);
        // active_tab must still point at the same tab (ids[1]), now at index 2.
        assert_eq!(state.sessions["s"].active_tab, 2);
        assert_eq!(state.sessions["s"].tabs[2].id, ids[1]);
    }

    #[test]
    fn test_move_tab_noop_leaves_state_unchanged() {
        let (mut state, ids) = state_with_tabs(4);
        state.sessions.get_mut("s").unwrap().active_tab = 2;
        // delta 0 is a no-op.
        state.move_tab("s", 1, 0).unwrap();
        assert_eq!(tab_order(&state), ids);
        assert_eq!(state.sessions["s"].active_tab, 2);
    }

    #[test]
    fn test_move_tab_out_of_range_index_errors() {
        let (mut state, _ids) = state_with_tabs(3);
        assert!(state.move_tab("s", 10, 1).is_err());
    }

    #[test]
    fn test_move_tab_missing_session_errors() {
        let mut state = ServerState::new();
        assert!(state.move_tab("nope", 0, 1).is_err());
    }

    #[test]
    fn test_create_session() {
        let mut state = ServerState::new();
        let pane_id = state
            .create_session(
                "test",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
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
        state
            .create_session(
                "test",
                Some("work"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();

        assert!(state.folders.contains_key("work"));
        let folder = state.folders.get("work").unwrap();
        assert_eq!(folder.session_ids, vec!["test"]);

        let sess = state.sessions.get("test").unwrap();
        assert_eq!(sess.folder, Some("work".to_string()));
    }

    #[test]
    fn test_create_session_duplicate_name() {
        let mut state = ServerState::new();
        state
            .create_session(
                "test",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
        let result = state.create_session(
            "test",
            None,
            BorderStyle::ZellijStyle,
            LayoutMode::default(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_rename_session() {
        let mut state = ServerState::new();
        state
            .create_session(
                "old",
                Some("folder"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
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
        state
            .create_session("a", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state
            .create_session("b", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();

        let result = state.rename_session("a", "b");
        assert!(result.is_err());
    }

    #[test]
    fn test_rename_session_same_name() {
        let mut state = ServerState::new();
        state
            .create_session("a", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state.rename_session("a", "a").unwrap();
    }

    #[test]
    fn test_delete_session() {
        let mut state = ServerState::new();
        state
            .create_session(
                "test",
                Some("folder"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
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
        state
            .create_session("b", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state
            .create_session(
                "a",
                Some("f"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();

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
        state
            .create_session(
                "s",
                Some("old"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
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
        state
            .create_session(
                "s",
                Some("work"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
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
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        let pane_id = state
            .create_tab("s", "new-tab", LayoutMode::default())
            .unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs.len(), 2);
        assert_eq!(sess.active_tab, 1);
        assert_eq!(sess.tabs[1].name, "new-tab");
        assert_eq!(sess.tabs[1].focused_pane, pane_id);
    }

    #[test]
    fn test_close_tab() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();

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
        state
            .create_session(
                "s",
                Some("f"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();

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
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state.rename_tab("s", 0, "renamed").unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs[0].name, "renamed");
    }

    #[test]
    fn test_goto_tab() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        state.goto_tab("s", 0).unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.active_tab, 0);
    }

    #[test]
    fn test_record_activity_background_tab_only() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        // pane 1 is in tab 0 (initially active). Create tab 1 (now active) with
        // pane 2. So tab 0 is now a background tab holding pane 1.
        let pane2 = state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        let now = Instant::now();

        // Output on the background tab's pane => Activity.
        state.record_pane_activity(1, false, now);
        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs[0].activity, TabActivity::Activity);
        assert!(sess.tabs[0].last_output.is_some());
        // The active (foreground) tab never accrues activity.
        assert_eq!(sess.tabs[1].activity, TabActivity::None);

        // Output on the active tab's pane => still None.
        state.record_pane_activity(pane2, false, now);
        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs[1].activity, TabActivity::None);
    }

    #[test]
    fn test_record_activity_bell_wins_and_no_downgrade() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        let now = Instant::now();

        // Bell on background tab 0 => Bell.
        state.record_pane_activity(1, true, now);
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Bell
        );

        // Subsequent plain output must NOT downgrade Bell to Activity.
        state.record_pane_activity(1, false, now);
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Bell
        );
    }

    #[test]
    fn test_goto_tab_clears_activity() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        // Give background tab 0 some activity.
        state.record_pane_activity(1, true, Instant::now());
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Bell
        );

        // Switching to tab 0 clears its activity.
        state.goto_tab("s", 0).unwrap();
        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs[0].activity, TabActivity::None);
        assert!(sess.tabs[0].last_output.is_none());
    }

    #[test]
    fn test_should_promote_to_silent_pure() {
        let base = Instant::now();
        let threshold = Duration::from_secs(3);
        let last = Some(base);

        // Activity older than threshold => promote.
        assert!(should_promote_to_silent(
            TabActivity::Activity,
            last,
            base + Duration::from_secs(4),
            threshold
        ));
        // Activity younger than threshold => no promote.
        assert!(!should_promote_to_silent(
            TabActivity::Activity,
            last,
            base + Duration::from_secs(1),
            threshold
        ));
        // Bell stays Bell regardless of age.
        assert!(!should_promote_to_silent(
            TabActivity::Bell,
            last,
            base + Duration::from_secs(10),
            threshold
        ));
        // None / Silent never promoted.
        assert!(!should_promote_to_silent(
            TabActivity::None,
            last,
            base + Duration::from_secs(10),
            threshold
        ));
        // No last_output => never promoted.
        assert!(!should_promote_to_silent(
            TabActivity::Activity,
            None,
            base + Duration::from_secs(10),
            threshold
        ));
    }

    #[test]
    fn test_promote_silent_tabs_transitions_activity() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        let base = Instant::now();
        // Background tab 0: Activity as of `base`.
        state.record_pane_activity(1, false, base);
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Activity
        );

        // Past the threshold, it promotes to Silent and reports the session.
        let affected =
            state.promote_silent_tabs(base + Duration::from_secs(4), Duration::from_secs(3));
        assert_eq!(affected, vec!["s".to_string()]);
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Silent
        );

        // Running again is idempotent: nothing changes (empty affected list).
        let affected2 =
            state.promote_silent_tabs(base + Duration::from_secs(8), Duration::from_secs(3));
        assert!(affected2.is_empty());
    }

    #[test]
    fn test_goto_tab_out_of_range() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        assert!(state.goto_tab("s", 5).is_err());
    }

    #[test]
    fn test_move_session_to_folder() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state.move_session("s", Some("new-folder")).unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.folder, Some("new-folder".to_string()));

        let folder = state.folders.get("new-folder").unwrap();
        assert!(folder.session_ids.contains(&"s".to_string()));
    }

    #[test]
    fn test_move_session_between_folders() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                Some("old"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
        state.move_session("s", Some("new")).unwrap();

        let old_folder = state.folders.get("old").unwrap();
        assert!(old_folder.session_ids.is_empty());

        let new_folder = state.folders.get("new").unwrap();
        assert!(new_folder.session_ids.contains(&"s".to_string()));
    }

    #[test]
    fn test_move_session_to_top_level() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                Some("folder"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
        state.move_session("s", None).unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert!(sess.folder.is_none());

        let folder = state.folders.get("folder").unwrap();
        assert!(folder.session_ids.is_empty());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s1",
                Some("work"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
        state
            .create_tab("s1", "tab2", LayoutMode::default())
            .unwrap();
        state
            .create_session("s2", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();

        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: ServerState = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.sessions.len(), 2);
        assert!(deserialized.sessions.contains_key("s1"));
        assert!(deserialized.sessions.contains_key("s2"));
        assert!(deserialized.folders.contains_key("work"));
    }

    #[test]
    fn test_build_session_tree_empty() {
        let state = ServerState::new();
        let counts = HashMap::new();
        let pane_names = HashMap::new();
        let (folders, unfiled) = state.build_session_tree(None, &counts, &pane_names);
        assert!(folders.is_empty());
        assert!(unfiled.is_empty());
    }

    #[test]
    fn test_build_session_tree_folders_and_unfiled() {
        let mut state = ServerState::new();
        state
            .create_session(
                "proj",
                Some("work"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();
        state
            .create_session(
                "scratch",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
            )
            .unwrap();

        let mut counts = HashMap::new();
        counts.insert("proj".to_string(), 2);
        let pane_names = HashMap::new();

        let (folders, unfiled) = state.build_session_tree(Some("proj"), &counts, &pane_names);
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "work");
        assert_eq!(folders[0].sessions.len(), 1);
        assert_eq!(folders[0].sessions[0].name, "proj");
        assert!(folders[0].sessions[0].is_current);
        assert_eq!(folders[0].sessions[0].client_count, 2);

        assert_eq!(unfiled.len(), 1);
        assert_eq!(unfiled[0].name, "scratch");
        assert!(!unfiled[0].is_current);
        assert_eq!(unfiled[0].client_count, 0);
    }

    #[test]
    fn test_build_session_tree_with_tabs_and_panes() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();

        let counts = HashMap::new();
        let mut pane_names = HashMap::new();
        pane_names.insert(1, "zsh".to_string());
        pane_names.insert(2, "vim".to_string());

        let (_, unfiled) = state.build_session_tree(None, &counts, &pane_names);
        assert_eq!(unfiled.len(), 1);
        assert_eq!(unfiled[0].tabs.len(), 2);
        // First tab should have pane with name "zsh"
        assert_eq!(unfiled[0].tabs[0].panes[0].name, "zsh");
    }

    #[test]
    fn test_build_session_tree_custom_pane_name_wins() {
        let mut state = ServerState::new();
        state
            .create_session("s", None, BorderStyle::ZellijStyle, LayoutMode::default())
            .unwrap();

        // Identify the first pane and give it a user-set custom name (as PaneRename does).
        let pane_id = {
            let sess = state.sessions.get_mut("s").unwrap();
            let tab = &mut sess.tabs[0];
            let pane_id = layout::all_pane_ids(&tab.layout)[0];
            assert!(layout::set_pane_custom_name(
                &mut tab.layout,
                pane_id,
                "XYZZY"
            ));
            pane_id
        };

        // Simulate the daemon: start from the auto-detected process name, then
        // apply the custom-name override (mirrors handle_list_session_tree).
        let counts = HashMap::new();
        let mut pane_names = HashMap::new();
        pane_names.insert(pane_id, "zsh".to_string());
        for sess in state.sessions.values() {
            for tab in &sess.tabs {
                for pid in layout::all_pane_ids(&tab.layout) {
                    if let Some(Some(custom)) = layout::get_pane_custom_name(&tab.layout, pid) {
                        pane_names.insert(pid, custom);
                    }
                }
            }
        }

        let (_, unfiled) = state.build_session_tree(None, &counts, &pane_names);
        assert_eq!(unfiled.len(), 1);
        // The custom name must win over the auto-detected process name.
        assert_eq!(unfiled[0].tabs[0].panes[0].name, "XYZZY");
    }
}
