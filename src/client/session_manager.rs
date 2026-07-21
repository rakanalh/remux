//! Session manager overlay.
//!
//! Provides a tree-view popup that shows all folders, sessions, tabs, and
//! panes. The user can navigate, expand/collapse nodes, switch sessions/tabs,
//! create/delete folders and sessions, and move sessions between folders.

use std::collections::{HashMap, HashSet};

use unicode_width::UnicodeWidthStr;

use crate::client::registry::{ConnId, RemoteState};
use crate::client::whichkey::DrawCommand;
use crate::config::keybindings::{SessionManagerBinding, SessionManagerBindings};
use crate::config::theme::Theme;
use crate::protocol::{FolderTreeEntry, SessionTreeEntry};

// ---------------------------------------------------------------------------
// NodeType / TreeRow
// ---------------------------------------------------------------------------

/// The type of a node in the flattened tree view.
///
/// The tree is now two-level at the top: a `Server` node per connection, whose
/// folders/sessions/tabs/panes nest beneath it. Every non-server node carries
/// the `ConnId` of the server it belongs to so actions can be routed and
/// remote-only guards applied.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeType {
    Server {
        id: ConnId,
        state: RemoteState,
    },
    Folder {
        server: ConnId,
        name: String,
    },
    Session {
        server: ConnId,
        name: String,
    },
    Tab {
        server: ConnId,
        session: String,
        tab_index: usize,
    },
    Pane {
        server: ConnId,
        session: String,
        tab_index: usize,
        pane_id: u64,
    },
    /// Header row for the "Saved (resurrect)" group of dormant sessions. Local
    /// server only for now.
    SavedGroup {
        server: ConnId,
    },
    /// A dormant (saved-but-not-live) session that can be resurrected. Pressing
    /// Enter on it materializes the session on the server.
    DormantSession {
        server: ConnId,
        name: String,
    },
}

impl NodeType {
    /// The connection this node belongs to.
    pub fn server(&self) -> ConnId {
        match self {
            NodeType::Server { id, .. } => id.clone(),
            NodeType::Folder { server, .. }
            | NodeType::Session { server, .. }
            | NodeType::Tab { server, .. }
            | NodeType::Pane { server, .. }
            | NodeType::SavedGroup { server }
            | NodeType::DormantSession { server, .. } => server.clone(),
        }
    }
}

/// A single row in the flattened session manager tree.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub indent: usize,
    pub node_type: NodeType,
    pub display_name: String,
    pub is_expanded: bool,
    pub is_current: bool,
}

// ---------------------------------------------------------------------------
// SubMode / CreatePhase
// ---------------------------------------------------------------------------

/// The target of a rename sub-mode. Captures which structural entity (and the
/// data needed to address it) is being renamed. The server it lives on is
/// recorded separately in `sub_mode_server`.
#[derive(Debug, Clone, PartialEq)]
pub enum RenameKind {
    Session { name: String },
    Folder { name: String },
    Tab { session: String, tab_index: usize },
    Pane { session: String, pane_id: u64 },
}

/// Sub-modes within the session manager for multi-step actions.
#[derive(Debug, Clone, PartialEq)]
pub enum SubMode {
    /// Normal navigation.
    Navigate,
    /// Waiting for delete confirmation. String describes the item.
    ConfirmDelete(String),
    /// Creating a new folder -- text buffer for the name.
    CreateFolder(String),
    /// Creating a new session.
    CreateSession { name: String, phase: CreatePhase },
    /// Moving a session to a different folder.
    MoveSession {
        session: String,
        folders: Vec<String>,
        selected: usize,
    },
    /// Renaming a structural entity -- text buffer for the new name. The target
    /// is captured in `kind`; the server in `sub_mode_server`.
    Rename { kind: RenameKind, buffer: String },
}

/// Outcome of feeding a key char to the chord engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordOutcome {
    /// A prefix char was consumed; waiting for the second key.
    Pending,
    /// A chord (single- or two-key) resolved to a binding.
    Binding(SessionManagerBinding),
    /// A pending prefix was cleared by an unmatched second key (consumed).
    Cleared,
    /// The char matched neither a prefix nor a binding; the caller should fall
    /// through to legacy (hardcoded) key handling.
    NoMatch,
}

/// Phase of the create-session flow.
#[derive(Debug, Clone, PartialEq)]
pub enum CreatePhase {
    EnterName,
    SelectFolder {
        folders: Vec<String>,
        selected: usize,
    },
}

// ---------------------------------------------------------------------------
// SessionManagerAction
// ---------------------------------------------------------------------------

/// Actions that the session manager produces in response to key input.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionManagerAction {
    /// Expand a not-yet-connected remote server node (triggers a lazy connect).
    ConnectRemote(String),
    SwitchSession {
        server: ConnId,
        session: String,
    },
    SwitchTab {
        server: ConnId,
        session: String,
        tab_index: usize,
    },
    SwitchPane {
        server: ConnId,
        session: String,
        tab_index: usize,
        pane_id: u64,
    },
    CreateFolder {
        server: ConnId,
        name: String,
    },
    CreateSession {
        server: ConnId,
        name: String,
        folder: Option<String>,
    },
    MoveSession {
        server: ConnId,
        session: String,
        folder: Option<String>,
    },
    DeleteSession {
        server: ConnId,
        name: String,
    },
    DeleteFolder {
        server: ConnId,
        name: String,
    },
    /// Resurrect a dormant (saved) session by name (Local server only).
    ResurrectSession(String),
    CloseTab {
        server: ConnId,
        session: String,
        tab_index: usize,
    },
    /// Create a new tab (with its default pane) in the target session.
    TabNew {
        server: ConnId,
        session: String,
    },
    /// Move a tab left/right within its session (delta -1 / +1).
    TabMove {
        server: ConnId,
        session: String,
        tab_index: usize,
        delta: i32,
    },
    /// Add a pane to the given tab of the target session.
    PaneNew {
        server: ConnId,
        session: String,
        tab_index: usize,
    },
    /// Close a pane by id in the target session.
    PaneClose {
        server: ConnId,
        session: String,
        pane_id: u64,
    },
    /// Rename a structural entity (session/folder/tab/pane) on `server`.
    Rename {
        server: ConnId,
        kind: RenameKind,
        new_name: String,
    },
    RefreshTree,
    Close,
    None,
}

// ---------------------------------------------------------------------------
// SessionManagerState
// ---------------------------------------------------------------------------

/// State for the session manager overlay.
#[derive(Debug, Clone)]
pub struct SessionManagerState {
    /// Flattened tree rows currently displayed.
    pub rows: Vec<TreeRow>,
    /// Index of the selected row.
    pub selected: usize,
    /// Set of expanded node keys (namespaced by server, e.g.
    /// "server:local", "folder:local:work", "session:remote:pi:proj").
    pub expanded: HashSet<String>,
    /// Current sub-mode.
    pub sub_mode: SubMode,
    /// The server a structural sub-mode (create/delete/move) targets. Set from
    /// the selected node's server when entering the sub-mode, and read when the
    /// completed action is emitted so it is routed to the right connection.
    sub_mode_server: ConnId,
    /// The name of the session the client is currently attached to.
    pub current_session: Option<String>,
    /// The foreground connection — a session row is "current" only when it is
    /// the attached session of the foreground server.
    foreground: ConnId,
    /// Ordered roster of servers: `(id, label, state)`.
    roster: Vec<(ConnId, String, RemoteState)>,
    /// Per-server raw tree data: `(folders, unfiled)`.
    trees: HashMap<ConnId, (Vec<FolderTreeEntry>, Vec<SessionTreeEntry>)>,
    /// Names of the Local server's dormant (saved-but-not-live) sessions,
    /// rendered as a "Saved (resurrect)" group. Dormant sessions are a
    /// Local-server concept for now.
    dormant: Vec<String>,
    /// Configured chord bindings for the overlay (defaults unless injected from
    /// config via `set_bindings`).
    bindings: SessionManagerBindings,
    /// The first char of an in-progress 2-char chord, awaiting completion.
    pending_chord: Option<char>,
}

/// Pad or truncate a string to exactly `target_width` display columns,
/// using `unicode-width` to account for ambiguous/wide characters.
fn pad_to_display_width(text: &str, target_width: usize) -> String {
    let display_w = UnicodeWidthStr::width(text);
    if display_w >= target_width {
        // Truncate: take chars until we reach target_width display columns.
        let mut result = String::new();
        let mut w = 0;
        for c in text.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > target_width {
                break;
            }
            result.push(c);
            w += cw;
        }
        // Pad remaining if truncation left us short (due to a wide char).
        while w < target_width {
            result.push(' ');
            w += 1;
        }
        result
    } else {
        let mut s = text.to_string();
        let padding = target_width - display_w;
        s.extend(std::iter::repeat_n(' ', padding));
        s
    }
}

/// Expansion key for a server node.
fn server_key(id: &ConnId) -> String {
    format!("server:{}", id.key())
}

/// Expansion key for a folder node (namespaced by server).
fn folder_key(server: &ConnId, name: &str) -> String {
    format!("folder:{}:{}", server.key(), name)
}

/// Expansion key for a session node (namespaced by server).
fn session_key(server: &ConnId, name: &str) -> String {
    format!("session:{}:{}", server.key(), name)
}

/// Expansion key for a tab node (namespaced by server).
fn tab_key(server: &ConnId, session: &str, tab_index: usize) -> String {
    format!("tab:{}:{}:{}", server.key(), session, tab_index)
}

/// Expansion key for the "Saved (resurrect)" group node (namespaced by server).
fn saved_key(server: &ConnId) -> String {
    format!("saved:{}", server.key())
}

impl SessionManagerState {
    /// Create a new session manager state (initially just a local server node,
    /// expanded so local sessions show immediately as before).
    pub fn new(current_session: Option<String>) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(server_key(&ConnId::Local));
        // Expand the Saved group by default so dormant sessions are discoverable.
        expanded.insert(saved_key(&ConnId::Local));
        Self {
            rows: Vec::new(),
            selected: 0,
            expanded,
            sub_mode: SubMode::Navigate,
            sub_mode_server: ConnId::Local,
            current_session,
            foreground: ConnId::Local,
            roster: vec![(ConnId::Local, "local".to_string(), RemoteState::Connected)],
            trees: HashMap::new(),
            dormant: Vec::new(),
            bindings: SessionManagerBindings::default(),
            pending_chord: None,
        }
    }

    /// Inject the effective chord bindings (built from config). Called by the
    /// input layer when it constructs the overlay so user overrides apply.
    pub fn set_bindings(&mut self, bindings: SessionManagerBindings) {
        self.bindings = bindings;
    }

    /// Set the foreground connection (drives which server's sessions render as
    /// "current"). Does not rebuild rows on its own; callers pair this with
    /// `set_roster`/`update_tree`.
    pub fn set_foreground(&mut self, foreground: ConnId) {
        self.foreground = foreground;
    }

    /// Replace the server roster (order + labels + states) and rebuild rows.
    pub fn set_roster(&mut self, roster: Vec<(ConnId, String, RemoteState)>) {
        // Ensure Local is always expanded by default the first time we see it.
        for (id, _, _) in &roster {
            if matches!(id, ConnId::Local) {
                self.expanded.insert(server_key(id));
            }
            self.trees.entry(id.clone()).or_default();
        }
        self.roster = roster;
        self.rebuild_rows();
    }

    /// Update a single server's slice of the tree and rebuild rows.
    pub fn update_tree(
        &mut self,
        server: ConnId,
        folders: Vec<FolderTreeEntry>,
        unfiled: Vec<SessionTreeEntry>,
        dormant: Vec<String>,
    ) {
        log::debug!(
            "session_manager: update_tree server={:?} folders={} unfiled={} dormant={}",
            server,
            folders.len(),
            unfiled.len(),
            dormant.len()
        );
        // Dormant sessions are a Local-server concept for now.
        if server == ConnId::Local {
            self.dormant = dormant;
        }
        // Determine whether this is the first data we've seen for this server.
        let is_first_load = self
            .trees
            .get(&server)
            .map(|(f, u)| f.is_empty() && u.is_empty())
            .unwrap_or(true);

        // Collect previously known keys so we auto-expand only new entries.
        let mut known_keys: HashSet<String> = HashSet::new();
        if let Some((pf, pu)) = self.trees.get(&server) {
            for f in pf {
                known_keys.insert(folder_key(&server, &f.name));
                for s in &f.sessions {
                    known_keys.insert(session_key(&server, &s.name));
                }
            }
            for s in pu {
                known_keys.insert(session_key(&server, &s.name));
            }
        }

        for f in &folders {
            let key = folder_key(&server, &f.name);
            if is_first_load || !known_keys.contains(&key) {
                self.expanded.insert(key);
            }
            for s in &f.sessions {
                let key = session_key(&server, &s.name);
                if is_first_load || !known_keys.contains(&key) {
                    self.expanded.insert(key);
                }
            }
        }
        for s in &unfiled {
            let key = session_key(&server, &s.name);
            if is_first_load || !known_keys.contains(&key) {
                self.expanded.insert(key);
            }
        }

        self.trees.insert(server, (folders, unfiled));
        self.rebuild_rows();
    }

    /// Rebuild the flat row list from the roster + per-server tree data.
    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();

        for (id, label, state) in &self.roster {
            let skey = server_key(id);
            let server_expanded = self.expanded.contains(&skey);
            let connected = matches!(state, RemoteState::Connected);
            let suffix = match state {
                RemoteState::Connected => String::new(),
                RemoteState::NotConnected => " (offline)".to_string(),
                RemoteState::Connecting => " (connecting…)".to_string(),
                RemoteState::Failed(msg) => format!(" (failed: {msg})"),
            };
            rows.push(TreeRow {
                indent: 0,
                node_type: NodeType::Server {
                    id: id.clone(),
                    state: state.clone(),
                },
                display_name: format!("{label}{suffix}"),
                is_expanded: server_expanded,
                is_current: false,
            });

            if server_expanded && connected {
                if let Some((folders, unfiled)) = self.trees.get(id) {
                    for folder in folders {
                        let fkey = folder_key(id, &folder.name);
                        let folder_expanded = self.expanded.contains(&fkey);
                        rows.push(TreeRow {
                            indent: 1,
                            node_type: NodeType::Folder {
                                server: id.clone(),
                                name: folder.name.clone(),
                            },
                            display_name: folder.name.clone(),
                            is_expanded: folder_expanded,
                            is_current: false,
                        });

                        if folder_expanded {
                            for session in &folder.sessions {
                                self.add_session_rows(&mut rows, id, session, 2);
                            }
                        }
                    }

                    for session in unfiled {
                        self.add_session_rows(&mut rows, id, session, 1);
                    }
                }

                // Render the "Saved (resurrect)" group at the bottom of the
                // Local server's children. Dormant sessions are Local-only.
                if *id == ConnId::Local && !self.dormant.is_empty() {
                    let gkey = saved_key(id);
                    let group_expanded = self.expanded.contains(&gkey);
                    rows.push(TreeRow {
                        indent: 1,
                        node_type: NodeType::SavedGroup { server: id.clone() },
                        display_name: "Saved (resurrect)".to_string(),
                        is_expanded: group_expanded,
                        is_current: false,
                    });
                    if group_expanded {
                        for name in &self.dormant {
                            rows.push(TreeRow {
                                indent: 2,
                                node_type: NodeType::DormantSession {
                                    server: id.clone(),
                                    name: name.clone(),
                                },
                                display_name: format!("\u{1F4A4} {}", name),
                                is_expanded: false,
                                is_current: false,
                            });
                        }
                    }
                }
            }
        }

        self.rows = rows;
        // Clamp selection.
        if !self.rows.is_empty() && self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
    }

    fn add_session_rows(
        &self,
        rows: &mut Vec<TreeRow>,
        server: &ConnId,
        session: &SessionTreeEntry,
        indent: usize,
    ) {
        let skey = session_key(server, &session.name);
        let session_expanded = self.expanded.contains(&skey);
        let client_suffix = if session.client_count > 0 {
            format!(" ({})", session.client_count)
        } else {
            String::new()
        };
        // "Current" only for the foreground server's attached session.
        let is_current = server == &self.foreground && session.is_current;
        rows.push(TreeRow {
            indent,
            node_type: NodeType::Session {
                server: server.clone(),
                name: session.name.clone(),
            },
            display_name: format!("{}{}", session.name, client_suffix),
            is_expanded: session_expanded,
            is_current,
        });

        if session_expanded {
            for (tab_idx, tab) in session.tabs.iter().enumerate() {
                let tkey = tab_key(server, &session.name, tab_idx);
                let tab_expanded = self.expanded.contains(&tkey);
                rows.push(TreeRow {
                    indent: indent + 1,
                    node_type: NodeType::Tab {
                        server: server.clone(),
                        session: session.name.clone(),
                        tab_index: tab_idx,
                    },
                    display_name: tab.name.clone(),
                    is_expanded: tab_expanded,
                    is_current: false,
                });

                if tab_expanded {
                    for pane in &tab.panes {
                        let focus_marker = if pane.is_focused { "*" } else { "" };
                        rows.push(TreeRow {
                            indent: indent + 2,
                            node_type: NodeType::Pane {
                                server: server.clone(),
                                session: session.name.clone(),
                                tab_index: tab_idx,
                                pane_id: pane.id,
                            },
                            display_name: format!("{}{}", pane.name, focus_marker),
                            is_expanded: false,
                            is_current: false,
                        });
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    /// Move selection down, wrapping to the top.
    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.rows.len();
        log::debug!("session_manager: select_next selected={}", self.selected);
    }

    /// Move selection up, wrapping to the bottom.
    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.rows.len() - 1;
        } else {
            self.selected -= 1;
        }
        log::debug!("session_manager: select_prev selected={}", self.selected);
    }

    /// Toggle the expand/collapse state of the selected node.
    pub fn toggle_expand(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            let key = self.node_key(&row.node_type);
            if self.expanded.contains(&key) {
                self.expanded.remove(&key);
            } else {
                self.expanded.insert(key);
            }
            self.rebuild_rows();
        }
    }

    /// Expand the selected node.
    pub fn expand_selected(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            let key = self.node_key(&row.node_type);
            if !self.expanded.contains(&key) {
                self.expanded.insert(key);
                self.rebuild_rows();
            }
        }
    }

    /// Collapse the selected node.
    pub fn collapse_selected(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            let key = self.node_key(&row.node_type);
            if self.expanded.contains(&key) {
                self.expanded.remove(&key);
                self.rebuild_rows();
            }
        }
    }

    fn node_key(&self, node_type: &NodeType) -> String {
        match node_type {
            NodeType::Server { id, .. } => server_key(id),
            NodeType::Folder { server, name } => folder_key(server, name),
            NodeType::Session { server, name } => session_key(server, name),
            NodeType::Tab {
                server,
                session,
                tab_index,
            } => tab_key(server, session, *tab_index),
            NodeType::SavedGroup { server } => saved_key(server),
            // Panes and dormant sessions don't expand.
            NodeType::Pane { .. } | NodeType::DormantSession { .. } => String::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    /// Handle Enter key on the selected row.
    pub fn handle_enter(&mut self) -> SessionManagerAction {
        let row = match self.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => return SessionManagerAction::None,
        };

        match &row.node_type {
            NodeType::Server { id, state } => match id {
                ConnId::Local => {
                    self.toggle_expand();
                    SessionManagerAction::None
                }
                ConnId::Remote(name) => match state {
                    RemoteState::Connected => {
                        self.toggle_expand();
                        SessionManagerAction::None
                    }
                    RemoteState::Connecting => SessionManagerAction::None,
                    RemoteState::NotConnected | RemoteState::Failed(_) => {
                        // Force-expand so children appear once the tree arrives,
                        // and kick off the lazy connect.
                        self.expanded.insert(server_key(id));
                        self.rebuild_rows();
                        SessionManagerAction::ConnectRemote(name.clone())
                    }
                },
            },
            NodeType::Folder { .. } => {
                self.toggle_expand();
                SessionManagerAction::None
            }
            NodeType::Session { server, name } => SessionManagerAction::SwitchSession {
                server: server.clone(),
                session: name.clone(),
            },
            NodeType::Tab {
                server,
                session,
                tab_index,
            } => SessionManagerAction::SwitchTab {
                server: server.clone(),
                session: session.clone(),
                tab_index: *tab_index,
            },
            NodeType::Pane {
                server,
                session,
                tab_index,
                pane_id,
            } => SessionManagerAction::SwitchPane {
                server: server.clone(),
                session: session.clone(),
                tab_index: *tab_index,
                pane_id: *pane_id,
            },
            // Enter on the Saved group toggles it; on a dormant session it
            // resurrects that session.
            NodeType::SavedGroup { .. } => {
                self.toggle_expand();
                SessionManagerAction::None
            }
            NodeType::DormantSession { name, .. } => {
                SessionManagerAction::ResurrectSession(name.clone())
            }
        }
    }

    /// Expand the selected node (Right / `l`). Unlike [`handle_enter`], this
    /// never switches/activates a Session/Tab/Pane -- it only reveals children.
    ///
    /// For a Pane (leaf) it does nothing. For a not-yet-connected/failed remote
    /// Server it force-expands and returns [`SessionManagerAction::ConnectRemote`]
    /// so the connection is established lazily (mirroring `handle_enter`).
    pub fn handle_expand(&mut self) -> SessionManagerAction {
        let row = match self.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => return SessionManagerAction::None,
        };

        match &row.node_type {
            NodeType::Server { id, state } => match id {
                ConnId::Remote(name) if *state != RemoteState::Connected => {
                    // Force-expand so children appear once the tree arrives,
                    // and kick off the lazy connect.
                    self.expanded.insert(server_key(id));
                    self.rebuild_rows();
                    SessionManagerAction::ConnectRemote(name.clone())
                }
                _ => {
                    self.expand_selected();
                    SessionManagerAction::None
                }
            },
            // Leaf: nothing to expand.
            NodeType::Pane { .. } | NodeType::DormantSession { .. } => SessionManagerAction::None,
            // Folder / Session / Tab / SavedGroup: reveal children without switching.
            _ => {
                self.expand_selected();
                SessionManagerAction::None
            }
        }
    }

    /// Handle 'd' key -- enter delete confirmation sub-mode.
    ///
    /// Works on any connected server (Local or a connected remote). Panes,
    /// server nodes, the saved group, and dormant sessions are never deletable.
    pub fn handle_delete_key(&mut self) -> SessionManagerAction {
        let row = match self.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => return SessionManagerAction::None,
        };
        let description = match &row.node_type {
            NodeType::Folder { name, .. } => format!("folder '{}'", name),
            NodeType::Session { name, .. } => format!("session '{}'", name),
            NodeType::Tab {
                session, tab_index, ..
            } => format!("tab {} in '{}'", tab_index, session),
            // Cannot delete panes, server nodes, or the saved group / dormant
            // sessions.
            NodeType::Pane { .. }
            | NodeType::Server { .. }
            | NodeType::SavedGroup { .. }
            | NodeType::DormantSession { .. } => {
                return SessionManagerAction::None;
            }
        };
        // Guard: only connected servers can be structurally edited.
        let server = row.node_type.server();
        if !self.is_connected(&server) {
            return SessionManagerAction::None;
        }
        self.sub_mode_server = server;
        self.sub_mode = SubMode::ConfirmDelete(description);
        SessionManagerAction::None
    }

    /// Handle confirmation response in ConfirmDelete sub-mode.
    pub fn handle_confirm_delete(&mut self, confirmed: bool) -> SessionManagerAction {
        if !confirmed {
            self.sub_mode = SubMode::Navigate;
            return SessionManagerAction::None;
        }

        let row = match self.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => {
                self.sub_mode = SubMode::Navigate;
                return SessionManagerAction::None;
            }
        };
        self.sub_mode = SubMode::Navigate;

        // Route the delete to the server captured when the sub-mode was entered.
        let server = self.sub_mode_server.clone();
        match &row.node_type {
            NodeType::Folder { name, .. } => SessionManagerAction::DeleteFolder {
                server,
                name: name.clone(),
            },
            NodeType::Session { name, .. } => SessionManagerAction::DeleteSession {
                server,
                name: name.clone(),
            },
            NodeType::Tab {
                session, tab_index, ..
            } => SessionManagerAction::CloseTab {
                server,
                session: session.clone(),
                tab_index: *tab_index,
            },
            NodeType::Pane { .. }
            | NodeType::Server { .. }
            | NodeType::SavedGroup { .. }
            | NodeType::DormantSession { .. } => SessionManagerAction::None,
        }
    }

    /// Handle 'c' key -- enter create-folder sub-mode on the selected node's
    /// (connected) server.
    pub fn handle_create_folder_key(&mut self) -> SessionManagerAction {
        let server = match self.structural_target_server() {
            Some(s) => s,
            None => return SessionManagerAction::None,
        };
        self.sub_mode_server = server;
        self.sub_mode = SubMode::CreateFolder(String::new());
        SessionManagerAction::None
    }

    /// Handle 'n' key -- enter create-session sub-mode on the selected node's
    /// (connected) server.
    pub fn handle_create_session_key(&mut self) -> SessionManagerAction {
        let server = match self.structural_target_server() {
            Some(s) => s,
            None => return SessionManagerAction::None,
        };
        self.sub_mode_server = server;
        self.sub_mode = SubMode::CreateSession {
            name: String::new(),
            phase: CreatePhase::EnterName,
        };
        SessionManagerAction::None
    }

    /// Handle 'm' key -- enter move-session sub-mode. Works on a session on any
    /// connected server; the folder list is drawn from that server's tree.
    pub fn handle_move_key(&mut self) -> SessionManagerAction {
        let row = match self.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => return SessionManagerAction::None,
        };
        if let NodeType::Session { server, name } = &row.node_type {
            if !self.is_connected(server) {
                return SessionManagerAction::None;
            }
            let mut folder_names = self.folder_names_for(server);
            folder_names.sort();
            // Add "(none)" option for top-level.
            folder_names.insert(0, "(none)".to_string());
            self.sub_mode_server = server.clone();
            self.sub_mode = SubMode::MoveSession {
                session: name.clone(),
                folders: folder_names,
                selected: 0,
            };
        }
        SessionManagerAction::None
    }

    /// Whether `server` is present in the roster and currently connected.
    fn is_connected(&self, server: &ConnId) -> bool {
        self.roster
            .iter()
            .any(|(id, _, state)| id == server && matches!(state, RemoteState::Connected))
    }

    /// The server a structural edit (create folder/session) should target:
    /// the selected node's server, but only if it is connected. Returns `None`
    /// for the saved group, dormant sessions, and not-connected servers.
    fn structural_target_server(&self) -> Option<ConnId> {
        let row = self.rows.get(self.selected)?;
        match &row.node_type {
            NodeType::SavedGroup { .. } | NodeType::DormantSession { .. } => None,
            _ => {
                let server = row.node_type.server();
                if self.is_connected(&server) {
                    Some(server)
                } else {
                    None
                }
            }
        }
    }

    /// The server the current structural sub-mode targets. Read by the input
    /// layer when it emits the completed create/move action so it is routed to
    /// the right connection.
    pub fn sub_mode_server(&self) -> ConnId {
        self.sub_mode_server.clone()
    }

    /// Get the folder names of the sub-mode's target server (for folder
    /// selection in the create-session flow).
    pub fn folder_names(&self) -> Vec<String> {
        self.folder_names_for(&self.sub_mode_server)
    }

    /// A given server's folder names.
    fn folder_names_for(&self, server: &ConnId) -> Vec<String> {
        self.trees
            .get(server)
            .map(|(folders, _)| folders.iter().map(|f| f.name.clone()).collect())
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Chord engine
    // -----------------------------------------------------------------------

    /// The in-progress chord prefix, if any (for the render's pending hint).
    pub fn pending_chord(&self) -> Option<char> {
        self.pending_chord
    }

    /// Cancel any in-progress chord prefix.
    pub fn clear_pending_chord(&mut self) {
        self.pending_chord = None;
    }

    /// Feed a single key char to the chord engine.
    ///
    /// - If a prefix is pending, this is the completing key: resolves to a
    ///   [`ChordOutcome::Binding`] on match, or [`ChordOutcome::Cleared`] on an
    ///   unmatched second key (the pending prefix is always cleared here).
    /// - Otherwise, if `c` begins a 2-char chord it becomes pending
    ///   ([`ChordOutcome::Pending`]); if `c` is a lone single-char binding it
    ///   fires immediately; else [`ChordOutcome::NoMatch`] (fall through to the
    ///   legacy hardcoded keys).
    pub fn feed_chord(&mut self, c: char) -> ChordOutcome {
        if let Some(first) = self.pending_chord.take() {
            return match self.bindings.chord(first, c) {
                Some(b) => ChordOutcome::Binding(b),
                None => ChordOutcome::Cleared,
            };
        }
        if self.bindings.is_prefix(c) {
            self.pending_chord = Some(c);
            return ChordOutcome::Pending;
        }
        if let Some(b) = self.bindings.single(c) {
            return ChordOutcome::Binding(b);
        }
        ChordOutcome::NoMatch
    }

    /// Apply a resolved chord binding against the currently selected row.
    ///
    /// The selected node decides the target; every emitted action carries the
    /// node's `server` and is gated on that server being connected. Bindings
    /// whose node type does not match the selected row are no-ops. Rename
    /// bindings enter a text-input sub-mode and return [`SessionManagerAction::None`].
    pub fn apply_binding(&mut self, binding: SessionManagerBinding) -> SessionManagerAction {
        use SessionManagerBinding::*;
        let node = match self.rows.get(self.selected) {
            Some(r) => r.node_type.clone(),
            None => return SessionManagerAction::None,
        };
        match binding {
            TabNew => {
                // Session node -> its own session; Tab node -> its session.
                let (server, session) = match &node {
                    NodeType::Session { server, name } => (server.clone(), name.clone()),
                    NodeType::Tab {
                        server, session, ..
                    } => (server.clone(), session.clone()),
                    _ => return SessionManagerAction::None,
                };
                if !self.is_connected(&server) {
                    return SessionManagerAction::None;
                }
                SessionManagerAction::TabNew { server, session }
            }
            TabClose => match &node {
                NodeType::Tab {
                    server,
                    session,
                    tab_index,
                } if self.is_connected(server) => SessionManagerAction::CloseTab {
                    server: server.clone(),
                    session: session.clone(),
                    tab_index: *tab_index,
                },
                _ => SessionManagerAction::None,
            },
            TabRename => match &node {
                NodeType::Tab {
                    server,
                    session,
                    tab_index,
                } if self.is_connected(server) => {
                    self.enter_rename(
                        server.clone(),
                        RenameKind::Tab {
                            session: session.clone(),
                            tab_index: *tab_index,
                        },
                    );
                    SessionManagerAction::None
                }
                _ => SessionManagerAction::None,
            },
            TabMoveLeft | TabMoveRight => match &node {
                NodeType::Tab {
                    server,
                    session,
                    tab_index,
                } if self.is_connected(server) => {
                    let delta = if matches!(binding, TabMoveLeft) {
                        -1
                    } else {
                        1
                    };
                    SessionManagerAction::TabMove {
                        server: server.clone(),
                        session: session.clone(),
                        tab_index: *tab_index,
                        delta,
                    }
                }
                _ => SessionManagerAction::None,
            },
            PaneNew => {
                // Tab node -> its tab; Pane node -> its containing tab.
                let (server, session, tab_index) = match &node {
                    NodeType::Tab {
                        server,
                        session,
                        tab_index,
                    } => (server.clone(), session.clone(), *tab_index),
                    NodeType::Pane {
                        server,
                        session,
                        tab_index,
                        ..
                    } => (server.clone(), session.clone(), *tab_index),
                    _ => return SessionManagerAction::None,
                };
                if !self.is_connected(&server) {
                    return SessionManagerAction::None;
                }
                SessionManagerAction::PaneNew {
                    server,
                    session,
                    tab_index,
                }
            }
            PaneClose => match &node {
                NodeType::Pane {
                    server,
                    session,
                    pane_id,
                    ..
                } if self.is_connected(server) => SessionManagerAction::PaneClose {
                    server: server.clone(),
                    session: session.clone(),
                    pane_id: *pane_id,
                },
                _ => SessionManagerAction::None,
            },
            PaneRename => match &node {
                NodeType::Pane {
                    server,
                    session,
                    pane_id,
                    ..
                } if self.is_connected(server) => {
                    self.enter_rename(
                        server.clone(),
                        RenameKind::Pane {
                            session: session.clone(),
                            pane_id: *pane_id,
                        },
                    );
                    SessionManagerAction::None
                }
                _ => SessionManagerAction::None,
            },
            SessionNew => self.handle_create_session_key(),
            SessionClose => match &node {
                // Reuse the delete-confirmation flow, but only for Session nodes.
                NodeType::Session { .. } => self.handle_delete_key(),
                _ => SessionManagerAction::None,
            },
            SessionRename => match &node {
                NodeType::Session { server, name } if self.is_connected(server) => {
                    self.enter_rename(server.clone(), RenameKind::Session { name: name.clone() });
                    SessionManagerAction::None
                }
                _ => SessionManagerAction::None,
            },
            SessionMove => match &node {
                NodeType::Session { .. } => self.handle_move_key(),
                _ => SessionManagerAction::None,
            },
            FolderNew => self.handle_create_folder_key(),
            FolderDelete => match &node {
                // Reuse the delete-confirmation flow, but only for Folder nodes.
                NodeType::Folder { .. } => self.handle_delete_key(),
                _ => SessionManagerAction::None,
            },
            FolderRename => match &node {
                NodeType::Folder { server, name } if self.is_connected(server) => {
                    self.enter_rename(server.clone(), RenameKind::Folder { name: name.clone() });
                    SessionManagerAction::None
                }
                _ => SessionManagerAction::None,
            },
        }
    }

    /// Enter a rename sub-mode targeting `kind` on `server`.
    fn enter_rename(&mut self, server: ConnId, kind: RenameKind) {
        self.sub_mode_server = server;
        self.sub_mode = SubMode::Rename {
            kind,
            buffer: String::new(),
        };
    }

    /// Confirm the current rename sub-mode, emitting a [`SessionManagerAction::Rename`]
    /// carrying the recorded server + target. An empty buffer is a no-op.
    /// Always returns to Navigate.
    pub fn confirm_rename(&mut self) -> SessionManagerAction {
        let server = self.sub_mode_server.clone();
        let action = if let SubMode::Rename { kind, buffer } = &self.sub_mode {
            if buffer.is_empty() {
                SessionManagerAction::None
            } else {
                SessionManagerAction::Rename {
                    server,
                    kind: kind.clone(),
                    new_name: buffer.clone(),
                }
            }
        } else {
            SessionManagerAction::None
        };
        self.sub_mode = SubMode::Navigate;
        action
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    /// Render the session manager overlay as a list of draw commands.
    pub fn render(&self, screen_cols: u16, screen_rows: u16, theme: &Theme) -> Vec<DrawCommand> {
        let mut commands = Vec::new();

        // Popup dimensions: 50% of the screen, min 40x12.
        let popup_width = (screen_cols / 2).max(40).min(screen_cols);
        let popup_height = (screen_rows / 2).max(12).min(screen_rows);

        if popup_width < 20 || popup_height < 6 {
            return commands;
        }

        let start_x = (screen_cols.saturating_sub(popup_width)) / 2;
        let start_y = (screen_rows.saturating_sub(popup_height)) / 2;

        let fg = theme.whichkey_fg;
        let bg = theme.whichkey_bg;
        let sel_fg = theme.whichkey_bg;
        let sel_bg = theme.whichkey_fg;
        let current_fg = theme.whichkey_key_fg;
        let border_fg = theme.separator_fg;

        let inner_width = (popup_width - 2) as usize;

        // Fill the entire popup area with background to prevent bleed-through.
        for row in 0..popup_height {
            commands.push(DrawCommand {
                x: start_x,
                y: start_y + row,
                text: " ".repeat(popup_width as usize),
                fg,
                bg,
            });
        }

        // Top border with title.
        let title = " Session Manager ";
        let border_len = inner_width.saturating_sub(title.len());
        let left_border = border_len / 2;
        let right_border = border_len - left_border;
        let top_line = format!(
            "\u{256D}{}{}{}\u{256E}",
            "\u{2500}".repeat(left_border),
            title,
            "\u{2500}".repeat(right_border),
        );
        commands.push(DrawCommand {
            x: start_x,
            y: start_y,
            text: top_line,
            fg: border_fg,
            bg,
        });

        // Content area (rows).
        let content_height = (popup_height - 4) as usize; // -2 for borders, -2 for help
        let scroll_offset = if self.selected >= content_height {
            self.selected - content_height + 1
        } else {
            0
        };

        for row_idx in 0..content_height {
            let tree_idx = scroll_offset + row_idx;
            let y = start_y + 1 + row_idx as u16;

            if let Some(row) = self.rows.get(tree_idx) {
                let is_selected = tree_idx == self.selected;
                let indent = "  ".repeat(row.indent);

                let expand_marker = match &row.node_type {
                    NodeType::Pane { .. } | NodeType::DormantSession { .. } => "  ",
                    _ => {
                        if row.is_expanded {
                            "\u{25BC} "
                        } else {
                            "\u{25B6} "
                        }
                    }
                };

                let current_marker = if row.is_current { "* " } else { "" };

                let text = format!(
                    "{}{}{}{}",
                    indent, expand_marker, current_marker, row.display_name
                );
                let content = pad_to_display_width(&text, inner_width);

                let (row_fg, row_bg) = if is_selected {
                    (sel_fg, sel_bg)
                } else if row.is_current {
                    (current_fg, bg)
                } else {
                    (fg, bg)
                };

                // Left border (always border color).
                commands.push(DrawCommand {
                    x: start_x,
                    y,
                    text: "\u{2502}".to_string(),
                    fg: border_fg,
                    bg,
                });
                // Content (selection/current/normal color).
                commands.push(DrawCommand {
                    x: start_x + 1,
                    y,
                    text: content,
                    fg: row_fg,
                    bg: row_bg,
                });
                // Right border (always border color).
                commands.push(DrawCommand {
                    x: start_x + 1 + inner_width as u16,
                    y,
                    text: "\u{2502}".to_string(),
                    fg: border_fg,
                    bg,
                });
            } else {
                // Left border
                commands.push(DrawCommand {
                    x: start_x,
                    y,
                    text: "\u{2502}".to_string(),
                    fg: border_fg,
                    bg,
                });
                // Empty content
                commands.push(DrawCommand {
                    x: start_x + 1,
                    y,
                    text: " ".repeat(inner_width),
                    fg,
                    bg,
                });
                // Right border
                commands.push(DrawCommand {
                    x: start_x + 1 + inner_width as u16,
                    y,
                    text: "\u{2502}".to_string(),
                    fg: border_fg,
                    bg,
                });
            }
        }

        // Sub-mode prompt (if applicable).
        let prompt_line = match &self.sub_mode {
            SubMode::Navigate => match self.pending_chord {
                // Subtle pending-chord hint, e.g. " [t-] ".
                Some(c) => format!(" [{c}-] "),
                None => String::new(),
            },
            SubMode::Rename { kind, buffer } => {
                let label = match kind {
                    RenameKind::Session { name } => format!("session '{name}'"),
                    RenameKind::Folder { name } => format!("folder '{name}'"),
                    RenameKind::Tab { .. } => "tab".to_string(),
                    RenameKind::Pane { .. } => "pane".to_string(),
                };
                format!(" Rename {label}: {buffer}_ ")
            }
            SubMode::ConfirmDelete(desc) => {
                format!(" Delete {}? (y/n) ", desc)
            }
            SubMode::CreateFolder(buf) => {
                format!(" Folder name: {}_ ", buf)
            }
            SubMode::CreateSession { name, phase } => match phase {
                CreatePhase::EnterName => format!(" Session name: {}_ ", name),
                CreatePhase::SelectFolder { folders, selected } => {
                    let folder = folders.get(*selected).map(|s| s.as_str()).unwrap_or("");
                    format!(" Folder (j/k, Enter): {} ", folder)
                }
            },
            SubMode::MoveSession {
                session,
                folders,
                selected,
            } => {
                let folder = folders.get(*selected).map(|s| s.as_str()).unwrap_or("");
                format!(" Move '{}' to (j/k, Enter): {} ", session, folder)
            }
        };

        // Separator line.
        let sep_y = start_y + 1 + content_height as u16;
        if !prompt_line.is_empty() {
            let prompt_content = pad_to_display_width(&prompt_line, inner_width);
            commands.push(DrawCommand {
                x: start_x,
                y: sep_y,
                text: "\u{251C}".to_string(),
                fg: border_fg,
                bg,
            });
            commands.push(DrawCommand {
                x: start_x + 1,
                y: sep_y,
                text: prompt_content,
                fg,
                bg,
            });
            commands.push(DrawCommand {
                x: start_x + 1 + inner_width as u16,
                y: sep_y,
                text: "\u{2524}".to_string(),
                fg: border_fg,
                bg,
            });
        } else {
            commands.push(DrawCommand {
                x: start_x,
                y: sep_y,
                text: format!("\u{251C}{}\u{2524}", "\u{2500}".repeat(inner_width)),
                fg: border_fg,
                bg,
            });
        }

        // Help line.
        let help_y = sep_y + 1;
        let help_text = " j/k:nav  Enter:select  d:delete  c:folder  n:session  m:move  q:quit ";
        let help_content = pad_to_display_width(help_text, inner_width);
        commands.push(DrawCommand {
            x: start_x,
            y: help_y,
            text: "\u{2502}".to_string(),
            fg: border_fg,
            bg,
        });
        commands.push(DrawCommand {
            x: start_x + 1,
            y: help_y,
            text: help_content,
            fg: theme.separator_fg,
            bg,
        });
        commands.push(DrawCommand {
            x: start_x + 1 + inner_width as u16,
            y: help_y,
            text: "\u{2502}".to_string(),
            fg: border_fg,
            bg,
        });

        // Bottom border.
        let bottom_y = help_y + 1;
        let bottom_line = format!("\u{2570}{}\u{256F}", "\u{2500}".repeat(inner_width));
        commands.push(DrawCommand {
            x: start_x,
            y: bottom_y,
            text: bottom_line,
            fg: border_fg,
            bg,
        });

        commands
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PaneTreeEntry, TabTreeEntry};

    fn sample_tree() -> (Vec<FolderTreeEntry>, Vec<SessionTreeEntry>) {
        let folders = vec![FolderTreeEntry {
            name: "work".to_string(),
            sessions: vec![SessionTreeEntry {
                name: "project-a".to_string(),
                tabs: vec![TabTreeEntry {
                    id: 1,
                    name: "Tab 1".to_string(),
                    panes: vec![PaneTreeEntry {
                        id: 10,
                        name: "zsh".to_string(),
                        is_focused: true,
                    }],
                }],
                client_count: 1,
                is_current: true,
            }],
        }];
        let unfiled = vec![SessionTreeEntry {
            name: "scratch".to_string(),
            tabs: vec![TabTreeEntry {
                id: 2,
                name: "Tab 1".to_string(),
                panes: vec![],
            }],
            client_count: 0,
            is_current: false,
        }];
        (folders, unfiled)
    }

    fn local_tree(state: &mut SessionManagerState) {
        let (folders, unfiled) = sample_tree();
        state.update_tree(ConnId::Local, folders, unfiled, Vec::new());
    }

    /// Expand a server node by selecting its row and expanding it.
    fn expand_server(state: &mut SessionManagerState, server: &ConnId) {
        let idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Server { id, .. } if id == server))
            .unwrap();
        state.selected = idx;
        state.expand_selected();
    }

    /// Index of the first row matching the named local session.
    fn session_row(state: &SessionManagerState, name: &str) -> usize {
        state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Session { name: n, .. } if n == name))
            .unwrap()
    }

    #[test]
    fn test_new_state_is_empty() {
        let state = SessionManagerState::new(None);
        // No tree data yet, so no rows built.
        assert!(state.rows.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_update_tree_builds_rows() {
        let mut state = SessionManagerState::new(Some("project-a".to_string()));
        local_tree(&mut state);

        // Row 0 is the local server node; the folder nests beneath it.
        assert!(matches!(
            state.rows[0].node_type,
            NodeType::Server {
                id: ConnId::Local,
                ..
            }
        ));
        assert!(state
            .rows
            .iter()
            .any(|r| matches!(&r.node_type, NodeType::Folder { name, .. } if name == "work")));
    }

    #[test]
    fn test_navigation_wraps() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        let total = state.rows.len();
        state.selected = total - 1;
        state.select_next();
        assert_eq!(state.selected, 0);

        state.select_prev();
        assert_eq!(state.selected, total - 1);
    }

    #[test]
    fn test_toggle_expand_collapse() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        // Find the folder row and collapse it.
        let folder_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Folder { name, .. } if name == "work"))
            .unwrap();
        let initial_count = state.rows.len();
        state.selected = folder_idx;
        state.collapse_selected();
        assert!(state.rows.len() < initial_count);

        // Expand it back.
        state.expand_selected();
        assert_eq!(state.rows.len(), initial_count);
    }

    #[test]
    fn test_enter_on_session_returns_switch() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        state.selected = session_row(&state, "project-a");
        let action = state.handle_enter();
        assert!(matches!(
            action,
            SessionManagerAction::SwitchSession { server: ConnId::Local, session } if session == "project-a"
        ));
    }

    #[test]
    fn test_enter_on_server_toggles_expand() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        state.selected = 0; // local server node
        let initial_count = state.rows.len();
        let action = state.handle_enter();
        assert!(matches!(action, SessionManagerAction::None));
        // Server was expanded, now collapsed -> only the server row remains.
        assert!(state.rows.len() < initial_count);
        assert_eq!(state.rows.len(), 1);
    }

    #[test]
    fn test_delete_confirmation_flow() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        state.selected = session_row(&state, "project-a");

        // Press 'd'.
        let action = state.handle_delete_key();
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::ConfirmDelete(_)));

        // Confirm with 'y'.
        let action = state.handle_confirm_delete(true);
        assert!(matches!(
            action,
            SessionManagerAction::DeleteSession { server: ConnId::Local, ref name } if name == "project-a"
        ));
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_remote_server_node_lazy_connect() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (ConnId::Local, "local".to_string(), RemoteState::Connected),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::NotConnected,
            ),
        ]);

        // Find the remote server row.
        let remote_idx = state
            .rows
            .iter()
            .position(|r| {
                matches!(&r.node_type, NodeType::Server { id: ConnId::Remote(n), .. } if n == "pi")
            })
            .unwrap();
        state.selected = remote_idx;
        let action = state.handle_enter();
        assert!(matches!(action, SessionManagerAction::ConnectRemote(ref n) if n == "pi"));
    }

    #[test]
    fn test_structural_edit_guarded_on_disconnected_remote() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (ConnId::Local, "local".to_string(), RemoteState::Connected),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::NotConnected,
            ),
        ]);

        // Select the not-connected remote server node and try to create a
        // folder -> no-op, no sub-mode (structural edits require a connection).
        let remote_idx = state
            .rows
            .iter()
            .position(|r| {
                matches!(&r.node_type, NodeType::Server { id: ConnId::Remote(n), .. } if n == "pi")
            })
            .unwrap();
        state.selected = remote_idx;
        let action = state.handle_create_folder_key();
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_create_session_key_on_connected_remote_targets_remote() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (ConnId::Local, "local".to_string(), RemoteState::Connected),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
            ),
        ]);
        let (folders, unfiled) = sample_tree();
        state.update_tree(
            ConnId::Remote("pi".to_string()),
            folders,
            unfiled,
            Vec::new(),
        );
        expand_server(&mut state, &ConnId::Remote("pi".to_string()));

        // Select the remote server node and press 'n' -> enters create-session
        // sub-mode, and the target server is the remote connection. This
        // sub_mode_server value is exactly what the input layer reads to route
        // the completed CreateSession action.
        let remote_idx = state
            .rows
            .iter()
            .position(|r| {
                matches!(&r.node_type, NodeType::Server { id: ConnId::Remote(n), .. } if n == "pi")
            })
            .unwrap();
        state.selected = remote_idx;
        let action = state.handle_create_session_key();
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(
            state.sub_mode,
            SubMode::CreateSession {
                phase: CreatePhase::EnterName,
                ..
            }
        ));
        assert_eq!(state.sub_mode_server(), ConnId::Remote("pi".to_string()));
    }

    #[test]
    fn test_delete_on_connected_remote_session_targets_remote() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (ConnId::Local, "local".to_string(), RemoteState::Connected),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
            ),
        ]);
        let (folders, unfiled) = sample_tree();
        state.update_tree(
            ConnId::Remote("pi".to_string()),
            folders,
            unfiled,
            Vec::new(),
        );
        expand_server(&mut state, &ConnId::Remote("pi".to_string()));

        // Select the remote's session and delete it -> enters confirm, then the
        // confirmed action carries the remote ConnId.
        let remote_session_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Session { server: ConnId::Remote(s), name } if s == "pi" && name == "project-a"))
            .unwrap();
        state.selected = remote_session_idx;
        let action = state.handle_delete_key();
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::ConfirmDelete(_)));

        let action = state.handle_confirm_delete(true);
        assert!(matches!(
            action,
            SessionManagerAction::DeleteSession { server: ConnId::Remote(ref s), ref name }
                if s == "pi" && name == "project-a"
        ));
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_remote_session_not_current_when_local_foreground() {
        let mut state = SessionManagerState::new(None);
        state.set_foreground(ConnId::Local);
        state.set_roster(vec![
            (ConnId::Local, "local".to_string(), RemoteState::Connected),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
            ),
        ]);
        // Remote reports an is_current session, but it is not the foreground.
        let (folders, unfiled) = sample_tree();
        state.update_tree(
            ConnId::Remote("pi".to_string()),
            folders,
            unfiled,
            Vec::new(),
        );
        expand_server(&mut state, &ConnId::Remote("pi".to_string()));

        let remote_session = state
            .rows
            .iter()
            .find(|r| matches!(&r.node_type, NodeType::Session { server: ConnId::Remote(s), .. } if s == "pi"))
            .unwrap();
        assert!(!remote_session.is_current);
    }

    #[test]
    fn test_handle_expand_reveals_tab_panes_without_switching() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        // Reveal the tab: expand server -> folder -> session.
        expand_server(&mut state, &ConnId::Local);
        let folder_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Folder { name, .. } if name == "work"))
            .unwrap();
        state.selected = folder_idx;
        state.expand_selected();
        state.selected = session_row(&state, "project-a");
        state.expand_selected();

        // Select the tab row.
        let tab_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { .. }))
            .unwrap();
        let tab_key = state.node_key(&state.rows[tab_idx].node_type.clone());

        // handle_enter on the tab still SWITCHES (returns SwitchTab).
        state.selected = tab_idx;
        let enter_action = state.handle_enter();
        assert!(matches!(
            enter_action,
            SessionManagerAction::SwitchTab { .. }
        ));

        // handle_expand on the tab EXPANDS: inserts the tab key, no switch.
        // Re-find the tab row (rebuilds may have shifted indices).
        let tab_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { .. }))
            .unwrap();
        state.selected = tab_idx;
        let expand_action = state.handle_expand();
        assert!(matches!(expand_action, SessionManagerAction::None));
        assert!(state.expanded.contains(&tab_key));
        // The tab's pane is now visible.
        assert!(state
            .rows
            .iter()
            .any(|r| matches!(&r.node_type, NodeType::Pane { .. })));
    }

    #[test]
    fn test_handle_expand_remote_server_triggers_lazy_connect() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (ConnId::Local, "local".to_string(), RemoteState::Connected),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::NotConnected,
            ),
        ]);

        let remote_idx = state
            .rows
            .iter()
            .position(|r| {
                matches!(&r.node_type, NodeType::Server { id: ConnId::Remote(n), .. } if n == "pi")
            })
            .unwrap();
        state.selected = remote_idx;
        let action = state.handle_expand();
        assert!(matches!(action, SessionManagerAction::ConnectRemote(ref n) if n == "pi"));
    }

    #[test]
    fn test_render_returns_commands() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        let theme = Theme::default();
        let cmds = state.render(80, 24, &theme);
        assert!(!cmds.is_empty());
    }

    #[test]
    fn test_saved_group_built_from_dormant_sessions() {
        let mut state = SessionManagerState::new(None);
        let (folders, unfiled) = sample_tree();
        state.update_tree(
            ConnId::Local,
            folders,
            unfiled,
            vec!["saved-a".to_string(), "saved-b".to_string()],
        );

        // A SavedGroup header row exists under the Local server.
        assert!(state.rows.iter().any(|r| matches!(
            &r.node_type,
            NodeType::SavedGroup {
                server: ConnId::Local
            }
        )));
        // Both dormant sessions are shown as DormantSession rows (expanded by
        // default).
        let dormant_names: Vec<&str> = state
            .rows
            .iter()
            .filter_map(|r| match &r.node_type {
                NodeType::DormantSession { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(dormant_names, vec!["saved-a", "saved-b"]);
    }

    #[test]
    fn test_no_saved_group_when_no_dormant_sessions() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state); // dormant is empty
        assert!(!state
            .rows
            .iter()
            .any(|r| matches!(&r.node_type, NodeType::SavedGroup { .. })));
    }

    #[test]
    fn test_enter_on_dormant_session_returns_resurrect() {
        let mut state = SessionManagerState::new(None);
        let (folders, unfiled) = sample_tree();
        state.update_tree(ConnId::Local, folders, unfiled, vec!["saved-a".to_string()]);

        let idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::DormantSession { name, .. } if name == "saved-a"))
            .unwrap();
        state.selected = idx;
        let action = state.handle_enter();
        assert!(matches!(
            action,
            SessionManagerAction::ResurrectSession(ref n) if n == "saved-a"
        ));
    }

    #[test]
    fn test_enter_on_saved_group_toggles_expand() {
        let mut state = SessionManagerState::new(None);
        let (folders, unfiled) = sample_tree();
        state.update_tree(ConnId::Local, folders, unfiled, vec!["saved-a".to_string()]);

        let group_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::SavedGroup { .. }))
            .unwrap();
        state.selected = group_idx;
        // Group starts expanded (dormant row visible); Enter collapses it.
        let action = state.handle_enter();
        assert!(matches!(action, SessionManagerAction::None));
        assert!(!state
            .rows
            .iter()
            .any(|r| matches!(&r.node_type, NodeType::DormantSession { .. })));
    }

    #[test]
    fn test_dormant_only_for_local_server() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (ConnId::Local, "local".to_string(), RemoteState::Connected),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
            ),
        ]);
        // A remote server reporting dormant names must not create a Saved group.
        let (folders, unfiled) = sample_tree();
        state.update_tree(
            ConnId::Remote("pi".to_string()),
            folders,
            unfiled,
            vec!["remote-saved".to_string()],
        );
        assert!(!state
            .rows
            .iter()
            .any(|r| matches!(&r.node_type, NodeType::SavedGroup { .. })));
    }

    // -----------------------------------------------------------------------
    // Chord engine + binding dispatch
    // -----------------------------------------------------------------------

    /// Index of the first Tab row for the named local session.
    fn tab_row(state: &SessionManagerState, session: &str) -> usize {
        state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { session: s, .. } if s == session))
            .unwrap()
    }

    /// A connected-remote ("pi") state with the sample tree loaded + expanded.
    fn remote_state_with_tree() -> SessionManagerState {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (ConnId::Local, "local".to_string(), RemoteState::Connected),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
            ),
        ]);
        let (folders, unfiled) = sample_tree();
        state.update_tree(
            ConnId::Remote("pi".to_string()),
            folders,
            unfiled,
            Vec::new(),
        );
        expand_server(&mut state, &ConnId::Remote("pi".to_string()));
        state
    }

    #[test]
    fn test_feed_chord_pending_then_complete() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.selected = tab_row(&state, "project-a");

        // 't' begins the default 2-char chords -> pending.
        assert_eq!(state.feed_chord('t'), ChordOutcome::Pending);
        assert_eq!(state.pending_chord(), Some('t'));

        // 'r' completes -> TabRename binding, pending cleared.
        assert_eq!(
            state.feed_chord('r'),
            ChordOutcome::Binding(SessionManagerBinding::TabRename)
        );
        assert_eq!(state.pending_chord(), None);

        // Applying enters a Rename sub-mode targeting the tab.
        let action = state.apply_binding(SessionManagerBinding::TabRename);
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(
            state.sub_mode,
            SubMode::Rename {
                kind: RenameKind::Tab { tab_index: 0, .. },
                ..
            }
        ));
    }

    #[test]
    fn test_feed_chord_unmatched_second_key_clears() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        assert_eq!(state.feed_chord('t'), ChordOutcome::Pending);
        // 'z' completes no 't' chord -> Cleared, pending reset.
        assert_eq!(state.feed_chord('z'), ChordOutcome::Cleared);
        assert_eq!(state.pending_chord(), None);
    }

    #[test]
    fn test_feed_chord_nomatch_falls_through() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        // 'j' is neither a prefix nor a binding -> NoMatch (legacy nav survives).
        assert_eq!(state.feed_chord('j'), ChordOutcome::NoMatch);
        assert_eq!(state.pending_chord(), None);
    }

    #[test]
    fn test_clear_pending_chord() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.feed_chord('t');
        assert_eq!(state.pending_chord(), Some('t'));
        state.clear_pending_chord();
        assert_eq!(state.pending_chord(), None);
    }

    #[test]
    fn test_tab_new_on_session_node() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.selected = session_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::TabNew);
        assert!(matches!(
            action,
            SessionManagerAction::TabNew { server: ConnId::Local, ref session }
                if session == "project-a"
        ));
    }

    #[test]
    fn test_tab_close_is_immediate_no_confirm() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.selected = tab_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::TabClose);
        assert!(matches!(
            action,
            SessionManagerAction::CloseTab {
                server: ConnId::Local,
                tab_index: 0,
                ..
            }
        ));
        // TabClose does NOT route through a confirm sub-mode.
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_tab_move_left_and_right() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.selected = tab_row(&state, "project-a");
        let right = state.apply_binding(SessionManagerBinding::TabMoveRight);
        assert!(matches!(
            right,
            SessionManagerAction::TabMove {
                delta: 1,
                tab_index: 0,
                ..
            }
        ));
        let left = state.apply_binding(SessionManagerBinding::TabMoveLeft);
        assert!(matches!(
            left,
            SessionManagerAction::TabMove {
                delta: -1,
                tab_index: 0,
                ..
            }
        ));
    }

    #[test]
    fn test_pane_new_on_tab_node() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.selected = tab_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::PaneNew);
        assert!(matches!(
            action,
            SessionManagerAction::PaneNew {
                server: ConnId::Local,
                tab_index: 0,
                ..
            }
        ));
    }

    #[test]
    fn test_tab_new_on_tab_node_uses_its_session() {
        // TabNew on a Tab node targets that tab's session.
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.selected = tab_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::TabNew);
        assert!(matches!(
            action,
            SessionManagerAction::TabNew { server: ConnId::Local, ref session }
                if session == "project-a"
        ));
    }

    #[test]
    fn test_pane_new_on_pane_node_uses_its_tab() {
        // PaneNew on a Pane node targets that pane's containing tab.
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        // Expand the tab to reveal its pane, then select the pane.
        state.selected = tab_row(&state, "project-a");
        state.expand_selected();
        let pane_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Pane { .. }))
            .unwrap();
        state.selected = pane_idx;
        let action = state.apply_binding(SessionManagerBinding::PaneNew);
        assert!(matches!(
            action,
            SessionManagerAction::PaneNew {
                server: ConnId::Local,
                tab_index: 0,
                ref session,
            } if session == "project-a"
        ));
    }

    #[test]
    fn test_pane_close_on_session_node_is_noop() {
        // A direct action on the wrong node type is a no-op.
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.selected = session_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::PaneClose);
        assert!(matches!(action, SessionManagerAction::None));
    }

    #[test]
    fn test_session_close_on_session_enters_confirm() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.selected = session_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::SessionClose);
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::ConfirmDelete(_)));
    }

    #[test]
    fn test_session_close_on_folder_is_noop() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        let folder_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Folder { .. }))
            .unwrap();
        state.selected = folder_idx;
        let action = state.apply_binding(SessionManagerBinding::SessionClose);
        assert!(matches!(action, SessionManagerAction::None));
        // No confirm sub-mode was entered — the folder is untouched.
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_folder_delete_on_session_is_noop() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.selected = session_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::FolderDelete);
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_tab_rename_on_remote_tab_carries_remote() {
        let mut state = remote_state_with_tree();
        let idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { server: ConnId::Remote(s), session, .. } if s == "pi" && session == "project-a"))
            .unwrap();
        state.selected = idx;

        // Chord: t, r.
        assert_eq!(state.feed_chord('t'), ChordOutcome::Pending);
        assert_eq!(
            state.feed_chord('r'),
            ChordOutcome::Binding(SessionManagerBinding::TabRename)
        );
        let action = state.apply_binding(SessionManagerBinding::TabRename);
        assert!(matches!(action, SessionManagerAction::None));

        // Type a new name and confirm -> Rename carrying the remote server.
        if let SubMode::Rename { ref mut buffer, .. } = state.sub_mode {
            buffer.push_str("newtab");
        }
        let action = state.confirm_rename();
        assert!(matches!(
            action,
            SessionManagerAction::Rename {
                server: ConnId::Remote(ref s),
                kind: RenameKind::Tab { ref session, tab_index: 0 },
                ref new_name,
            } if s == "pi" && session == "project-a" && new_name == "newtab"
        ));
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_pane_close_on_remote_pane_carries_remote() {
        let mut state = remote_state_with_tree();
        // Reveal the pane by expanding its (remote) tab.
        let tab_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { server: ConnId::Remote(s), .. } if s == "pi"))
            .unwrap();
        state.selected = tab_idx;
        state.expand_selected();

        let pane_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Pane { server: ConnId::Remote(s), .. } if s == "pi"))
            .unwrap();
        state.selected = pane_idx;

        // Chord: p, x.
        assert_eq!(state.feed_chord('p'), ChordOutcome::Pending);
        assert_eq!(
            state.feed_chord('x'),
            ChordOutcome::Binding(SessionManagerBinding::PaneClose)
        );
        let action = state.apply_binding(SessionManagerBinding::PaneClose);
        assert!(matches!(
            action,
            SessionManagerAction::PaneClose {
                server: ConnId::Remote(ref s),
                ref session,
                pane_id: 10,
            } if s == "pi" && session == "project-a"
        ));
    }

    #[test]
    fn test_rename_pending_chord_shown_in_render() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        // A pending prefix should surface a subtle hint in the rendered popup.
        state.feed_chord('t');
        let theme = Theme::default();
        let cmds = state.render(80, 24, &theme);
        assert!(cmds.iter().any(|c| c.text.contains("[t-]")));
    }
}
