//! Session manager overlay.
//!
//! Provides a tree-view popup that shows all folders, sessions, tabs, and
//! panes. The user can navigate, expand/collapse nodes, switch sessions/tabs,
//! create/delete folders and sessions, and move sessions between folders.

use std::collections::{HashMap, HashSet};

use unicode_width::UnicodeWidthStr;

use crate::client::registry::{ConnId, RemoteState};
use crate::client::whichkey::DrawCommand;
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
}

impl NodeType {
    /// The connection this node belongs to.
    pub fn server(&self) -> ConnId {
        match self {
            NodeType::Server { id, .. } => id.clone(),
            NodeType::Folder { server, .. }
            | NodeType::Session { server, .. }
            | NodeType::Tab { server, .. }
            | NodeType::Pane { server, .. } => server.clone(),
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
    CreateFolder(String),
    CreateSession {
        name: String,
        folder: Option<String>,
    },
    MoveSession {
        session: String,
        folder: Option<String>,
    },
    DeleteSession(String),
    DeleteFolder(String),
    CloseTab {
        session: String,
        tab_index: usize,
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
    /// The name of the session the client is currently attached to.
    pub current_session: Option<String>,
    /// The foreground connection — a session row is "current" only when it is
    /// the attached session of the foreground server.
    foreground: ConnId,
    /// Ordered roster of servers: `(id, label, state)`.
    roster: Vec<(ConnId, String, RemoteState)>,
    /// Per-server raw tree data: `(folders, unfiled)`.
    trees: HashMap<ConnId, (Vec<FolderTreeEntry>, Vec<SessionTreeEntry>)>,
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

impl SessionManagerState {
    /// Create a new session manager state (initially just a local server node,
    /// expanded so local sessions show immediately as before).
    pub fn new(current_session: Option<String>) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(server_key(&ConnId::Local));
        Self {
            rows: Vec::new(),
            selected: 0,
            expanded,
            sub_mode: SubMode::Navigate,
            current_session,
            foreground: ConnId::Local,
            roster: vec![(ConnId::Local, "local".to_string(), RemoteState::Connected)],
            trees: HashMap::new(),
        }
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
    ) {
        log::debug!(
            "session_manager: update_tree server={:?} folders={} unfiled={}",
            server,
            folders.len(),
            unfiled.len()
        );
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
            NodeType::Pane { .. } => String::new(), // Panes don't expand.
        }
    }

    /// The server the currently-selected row belongs to (if any).
    fn selected_server(&self) -> Option<ConnId> {
        self.rows.get(self.selected).map(|r| r.node_type.server())
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
        }
    }

    /// Handle 'd' key -- enter delete confirmation sub-mode.
    ///
    /// Structural edits are Local-only: on a remote node this is a no-op.
    pub fn handle_delete_key(&mut self) -> SessionManagerAction {
        let row = match self.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => return SessionManagerAction::None,
        };
        // Guard: remote nodes cannot be structurally edited.
        if row.node_type.server() != ConnId::Local {
            return SessionManagerAction::None;
        }
        let description = match &row.node_type {
            NodeType::Folder { name, .. } => format!("folder '{}'", name),
            NodeType::Session { name, .. } => format!("session '{}'", name),
            NodeType::Tab {
                session, tab_index, ..
            } => format!("tab {} in '{}'", tab_index, session),
            // Cannot delete panes or server nodes.
            NodeType::Pane { .. } | NodeType::Server { .. } => {
                return SessionManagerAction::None;
            }
        };
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

        // Guard: remote nodes cannot be structurally edited.
        if row.node_type.server() != ConnId::Local {
            return SessionManagerAction::None;
        }
        match &row.node_type {
            NodeType::Folder { name, .. } => SessionManagerAction::DeleteFolder(name.clone()),
            NodeType::Session { name, .. } => SessionManagerAction::DeleteSession(name.clone()),
            NodeType::Tab {
                session, tab_index, ..
            } => SessionManagerAction::CloseTab {
                session: session.clone(),
                tab_index: *tab_index,
            },
            NodeType::Pane { .. } | NodeType::Server { .. } => SessionManagerAction::None,
        }
    }

    /// Handle 'c' key -- enter create-folder sub-mode. Local-only.
    pub fn handle_create_folder_key(&mut self) -> SessionManagerAction {
        if !self.selected_is_local() {
            return SessionManagerAction::None;
        }
        self.sub_mode = SubMode::CreateFolder(String::new());
        SessionManagerAction::None
    }

    /// Handle 'n' key -- enter create-session sub-mode. Local-only.
    pub fn handle_create_session_key(&mut self) -> SessionManagerAction {
        if !self.selected_is_local() {
            return SessionManagerAction::None;
        }
        self.sub_mode = SubMode::CreateSession {
            name: String::new(),
            phase: CreatePhase::EnterName,
        };
        SessionManagerAction::None
    }

    /// Handle 'm' key -- enter move-session sub-mode. Local-only.
    pub fn handle_move_key(&mut self) -> SessionManagerAction {
        let row = match self.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => return SessionManagerAction::None,
        };
        if let NodeType::Session { server, name } = &row.node_type {
            if server != &ConnId::Local {
                return SessionManagerAction::None;
            }
            let mut folder_names = self.local_folder_names();
            folder_names.sort();
            // Add "(none)" option for top-level.
            folder_names.insert(0, "(none)".to_string());
            self.sub_mode = SubMode::MoveSession {
                session: name.clone(),
                folders: folder_names,
                selected: 0,
            };
        }
        SessionManagerAction::None
    }

    /// Whether the currently-selected row belongs to the Local server (used to
    /// gate structural, Local-only edits).
    fn selected_is_local(&self) -> bool {
        self.selected_server()
            .map(|s| s == ConnId::Local)
            .unwrap_or(true)
    }

    /// Get the list of Local folder names (for folder selection in sub-modes).
    pub fn folder_names(&self) -> Vec<String> {
        self.local_folder_names()
    }

    /// The Local server's folder names.
    fn local_folder_names(&self) -> Vec<String> {
        self.trees
            .get(&ConnId::Local)
            .map(|(folders, _)| folders.iter().map(|f| f.name.clone()).collect())
            .unwrap_or_default()
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
                    NodeType::Pane { .. } => "  ",
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
            SubMode::Navigate => String::new(),
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
        state.update_tree(ConnId::Local, folders, unfiled);
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
        assert!(matches!(action, SessionManagerAction::DeleteSession(ref n) if n == "project-a"));
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
    fn test_structural_edit_guarded_on_remote() {
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
        state.update_tree(ConnId::Remote("pi".to_string()), folders, unfiled);
        expand_server(&mut state, &ConnId::Remote("pi".to_string()));

        // Select the remote's session and try to delete it -> no-op, no sub-mode.
        let remote_session_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Session { server: ConnId::Remote(s), name } if s == "pi" && name == "project-a"))
            .unwrap();
        state.selected = remote_session_idx;
        let action = state.handle_delete_key();
        assert!(matches!(action, SessionManagerAction::None));
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
        state.update_tree(ConnId::Remote("pi".to_string()), folders, unfiled);
        expand_server(&mut state, &ConnId::Remote("pi".to_string()));

        let remote_session = state
            .rows
            .iter()
            .find(|r| matches!(&r.node_type, NodeType::Session { server: ConnId::Remote(s), .. } if s == "pi"))
            .unwrap();
        assert!(!remote_session.is_current);
    }

    #[test]
    fn test_render_returns_commands() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        let theme = Theme::default();
        let cmds = state.render(80, 24, &theme);
        assert!(!cmds.is_empty());
    }
}
