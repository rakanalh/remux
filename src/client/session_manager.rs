//! Session manager overlay.
//!
//! Provides a tree-view popup that shows all folders, sessions, tabs, and
//! panes. The user can navigate, expand/collapse nodes, switch sessions/tabs,
//! create/delete folders and sessions, and move sessions between folders.

use unicode_width::UnicodeWidthStr;

use crate::client::registry::{ConnId, RemoteState};
use crate::client::tree_model::TreeModel;
use crate::client::whichkey::DrawCommand;
use crate::config::keybindings::{SessionManagerBinding, SessionManagerBindings};
use crate::config::theme::Theme;
use crate::protocol::{FolderTreeEntry, SessionTreeEntry};

/// The tree node type lives in [`crate::client::tree_model`] now; re-exported
/// here so the overlay's long-standing import path keeps working.
pub use crate::client::tree_model::NodeType;

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
    /// Waiting for delete confirmation.
    ///
    /// Carries the TARGET NODE, not just its description. The tree rebuilds on
    /// every server push, so between `d` and `y` the row under the selection
    /// index can become a different node -- and when the selected key is gone
    /// `rebuild_rows` deliberately leaves the index where it was, so the
    /// selection silently slides onto a neighbour. Re-reading the selection at
    /// confirm time would then delete something the prompt never named.
    ConfirmDelete {
        target: NodeType,
        /// What the prompt says, captured with the target so the two agree.
        description: String,
    },
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
    /// Add one or more existing panes (marked, or the highlighted pane) to a
    /// client-only view. Handled entirely client-side (opens the view picker);
    /// never forwarded to the server.
    AddToView {
        panes: Vec<(ConnId, u64)>,
    },
    Close,
    None,
}

// ---------------------------------------------------------------------------
// SessionManagerState
// ---------------------------------------------------------------------------

/// State for the session manager overlay.
#[derive(Debug, Clone)]
pub struct SessionManagerState {
    /// The tree itself: rows, expansion state, selection, the search query,
    /// the roster and the per-server data behind them. Shared with the
    /// sidebar's session-tree panel (see [`crate::client::tree_model`]).
    pub model: TreeModel,
    /// Current sub-mode.
    pub sub_mode: SubMode,
    /// The server a structural sub-mode (create/delete/move) targets. Set from
    /// the selected node's server when entering the sub-mode, and read when the
    /// completed action is emitted so it is routed to the right connection.
    sub_mode_server: ConnId,
    /// The name of the session the client is currently attached to.
    pub current_session: Option<String>,
    /// Configured chord bindings for the overlay (defaults unless injected from
    /// config via `set_bindings`).
    bindings: SessionManagerBindings,
    /// The first char of an in-progress 2-char chord, awaiting completion.
    pending_chord: Option<char>,
    /// Panes marked (via Space) for a multi-select "add to view" action, keyed
    /// by `(server, pane_id)` so a mark survives a rebuild (row indices are
    /// not stable across tree refreshes). Insertion order is preserved so the
    /// resulting view cell order is deterministic; inserts dedupe.
    marked: Vec<(ConnId, u64)>,
    /// Whether keystrokes are typed into the search bar rather than routed to
    /// the tree.
    ///
    /// Invariant: `search_focused == true` implies `sub_mode == SubMode::Navigate`.
    /// The sub-modes are only ever entered by a binding or a hardcoded key, and
    /// neither can fire while focus is on the search bar.
    pub search_focused: bool,
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

/// Short human label for a session-manager chord binding, shown in the popup
/// help footer. Kept terse so several fit per line.
fn binding_label(b: SessionManagerBinding) -> &'static str {
    use SessionManagerBinding::*;
    match b {
        TabNew => "tab new",
        TabClose => "tab close",
        TabRename => "tab rename",
        TabMoveLeft => "tab \u{2190}",
        TabMoveRight => "tab \u{2192}",
        PaneNew => "pane new",
        PaneClose => "pane close",
        PaneRename => "pane rename",
        SessionNew => "session new",
        SessionClose => "session close",
        SessionRename => "session rename",
        SessionMove => "session move",
        FolderNew => "folder new",
        FolderDelete => "folder del",
        FolderRename => "folder rename",
        AddToView => "add to view",
    }
}

/// Group ordering for footer chords: session (s*), folder (f*), tab (t*),
/// pane (p*), then anything else. Gives a stable, readable grouping.
fn chord_group_rank(chord: &str) -> u8 {
    match chord.chars().next() {
        Some('s') => 0,
        Some('f') => 1,
        Some('t') => 2,
        Some('p') => 3,
        _ => 4,
    }
}

/// Separator between footer cells (matches the box-drawing style used across
/// the popups).
const FOOTER_SEP: &str = " \u{2502} ";

/// Pack `(key, label)` cells into up to `max_lines` footer lines that each fit
/// within `inner_width` display columns. Cells are placed greedily; any that do
/// not fit within `max_lines` are dropped (clean truncation).
fn pack_footer_lines(
    cells: &[(String, String)],
    inner_width: usize,
    max_lines: usize,
) -> Vec<Vec<(String, String)>> {
    let sep_w = UnicodeWidthStr::width(FOOTER_SEP);
    let mut lines: Vec<Vec<(String, String)>> = Vec::new();
    let mut cur: Vec<(String, String)> = Vec::new();
    let mut cur_w = 0usize;
    for cell in cells {
        let cell_w = UnicodeWidthStr::width(format!("{} {}", cell.0, cell.1).as_str());
        if cur.is_empty() {
            cur.push(cell.clone());
            cur_w = cell_w;
        } else if cur_w + sep_w + cell_w <= inner_width {
            cur.push(cell.clone());
            cur_w += sep_w + cell_w;
        } else {
            lines.push(std::mem::take(&mut cur));
            if lines.len() >= max_lines {
                return lines;
            }
            cur.push(cell.clone());
            cur_w = cell_w;
        }
    }
    if !cur.is_empty() && lines.len() < max_lines {
        lines.push(cur);
    }
    lines.truncate(max_lines);
    lines
}

impl SessionManagerState {
    /// Create a new session manager state (initially just a local server node,
    /// expanded so local sessions show immediately as before).
    pub fn new(current_session: Option<String>) -> Self {
        Self {
            model: TreeModel::new(),
            sub_mode: SubMode::Navigate,
            sub_mode_server: ConnId::Local,
            current_session,
            bindings: SessionManagerBindings::default(),
            pending_chord: None,
            marked: Vec::new(),
            // The overlay opens with the search bar focused so the user can
            // just start typing; Tab/Down/Enter hands focus to the tree.
            search_focused: true,
        }
    }

    // -----------------------------------------------------------------------
    // Tree model delegation
    //
    // The tree lives in `TreeModel`; these keep the overlay's own API (and its
    // callers in `input.rs` / `main.rs`) exactly as it was.
    // -----------------------------------------------------------------------

    /// Append a char to the search query and refilter.
    pub fn push_query_char(&mut self, c: char) {
        self.model.push_query_char(c);
    }

    /// Remove the last char of the search query and refilter.
    pub fn pop_query_char(&mut self) {
        self.model.pop_query_char();
    }

    /// Clear the search query and refilter (Ctrl-U).
    pub fn clear_query(&mut self) {
        self.model.clear_query();
    }

    /// Set the foreground connection (drives which server's sessions render as
    /// "current"). Does not rebuild rows on its own; callers pair this with
    /// `set_roster`/`update_tree`.
    pub fn set_foreground(&mut self, foreground: ConnId) {
        self.model.set_foreground(foreground);
    }

    /// Replace the server roster (order + labels + states) and rebuild rows.
    pub fn set_roster(&mut self, roster: Vec<(ConnId, String, RemoteState, Option<String>)>) {
        self.model.set_roster(roster);
    }

    /// Update a single server's slice of the tree and rebuild rows.
    pub fn update_tree(
        &mut self,
        server: ConnId,
        folders: Vec<FolderTreeEntry>,
        unfiled: Vec<SessionTreeEntry>,
        dormant: Vec<String>,
    ) {
        self.model.update_tree(server, folders, unfiled, dormant);
    }

    /// Move selection down, wrapping to the top.
    pub fn select_next(&mut self) {
        self.model.select_next();
    }

    /// Move selection up, wrapping to the bottom.
    pub fn select_prev(&mut self) {
        self.model.select_prev();
    }

    /// Toggle the expand/collapse state of the selected node.
    pub fn toggle_expand(&mut self) {
        self.model.toggle_expand();
    }

    /// Expand the selected node.
    pub fn expand_selected(&mut self) {
        self.model.expand_selected();
    }

    /// Collapse the selected node.
    pub fn collapse_selected(&mut self) {
        self.model.collapse_selected();
    }

    /// Move focus to the search bar. Any half-typed chord prefix is dropped so
    /// it cannot lurk and complete after the user comes back to the tree.
    pub fn focus_search(&mut self) {
        self.search_focused = true;
        self.pending_chord = None;
    }

    /// Move focus to the tree (Tab / Down / Enter from the search bar).
    pub fn focus_tree(&mut self) {
        self.search_focused = false;
    }

    /// Toggle the mark on the selected row when it is a pane. Marks are keyed by
    /// `(server, pane_id)` so they survive tree rebuilds. Returns true when a
    /// pane row was toggled (marked or unmarked), false for non-pane rows.
    pub fn toggle_mark(&mut self) -> bool {
        let key = match self
            .model
            .rows
            .get(self.model.selected)
            .map(|r| &r.node_type)
        {
            Some(NodeType::Pane {
                server, pane_id, ..
            }) => (server.clone(), *pane_id),
            _ => return false,
        };
        if let Some(pos) = self.marked.iter().position(|m| *m == key) {
            self.marked.remove(pos);
        } else {
            self.marked.push(key);
        }
        true
    }

    /// Number of currently marked panes.
    pub fn marked_count(&self) -> usize {
        self.marked.len()
    }

    /// Take the panes to add to a view: the marked set (drained) if non-empty,
    /// else the single highlighted pane (if the selected row is a pane), else
    /// empty. Draining clears the marks so a subsequent action starts fresh.
    pub fn take_marked_or_highlighted_panes(&mut self) -> Vec<(ConnId, u64)> {
        if !self.marked.is_empty() {
            return std::mem::take(&mut self.marked);
        }
        match self
            .model
            .rows
            .get(self.model.selected)
            .map(|r| &r.node_type)
        {
            Some(NodeType::Pane {
                server, pane_id, ..
            }) => vec![(server.clone(), *pane_id)],
            _ => Vec::new(),
        }
    }

    /// Inject the effective chord bindings (built from config). Called by the
    /// input layer when it constructs the overlay so user overrides apply.
    pub fn set_bindings(&mut self, bindings: SessionManagerBindings) {
        self.bindings = bindings;
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    /// Handle Enter key on the selected row.
    pub fn handle_enter(&mut self) -> SessionManagerAction {
        let row = match self.model.rows.get(self.model.selected) {
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
                        self.model.force_expand_server(id);
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
        let row = match self.model.rows.get(self.model.selected) {
            Some(r) => r.clone(),
            None => return SessionManagerAction::None,
        };

        match &row.node_type {
            NodeType::Server { id, state } => match id {
                ConnId::Remote(name) if *state != RemoteState::Connected => {
                    // Force-expand so children appear once the tree arrives,
                    // and kick off the lazy connect.
                    self.model.force_expand_server(id);
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
        let row = match self.model.rows.get(self.model.selected) {
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
        self.sub_mode = SubMode::ConfirmDelete {
            target: row.node_type.clone(),
            description,
        };
        SessionManagerAction::None
    }

    /// Handle confirmation response in ConfirmDelete sub-mode.
    pub fn handle_confirm_delete(&mut self, confirmed: bool) -> SessionManagerAction {
        if !confirmed {
            self.sub_mode = SubMode::Navigate;
            return SessionManagerAction::None;
        }

        // Act on the node captured when `d` was pressed, NEVER on whatever the
        // selection index points at now. A `SubscribeSessionTree` push can
        // rebuild the tree between the two keystrokes, and a rebuild that loses
        // the selected key leaves the index parked on a neighbour -- so
        // re-reading the selection here would delete a session the prompt did
        // not name, with no second confirmation.
        let target = match &self.sub_mode {
            SubMode::ConfirmDelete { target, .. } => target.clone(),
            // Not in the prompt: nothing was confirmed.
            _ => {
                self.sub_mode = SubMode::Navigate;
                return SessionManagerAction::None;
            }
        };
        self.sub_mode = SubMode::Navigate;

        // The captured node may itself have disappeared while the prompt was
        // up. Deleting by name regardless would resurrect a race the capture
        // exists to close, so abort instead.
        let key = self.model.node_key(&target);
        if !self
            .model
            .rows
            .iter()
            .any(|r| self.model.node_key(&r.node_type) == key)
        {
            log::warn!(
                "session_manager: delete aborted -- {key:?} disappeared while the \
                 confirmation prompt was open"
            );
            return SessionManagerAction::None;
        }

        // Route the delete to the server captured when the sub-mode was entered.
        let server = self.sub_mode_server.clone();
        match &target {
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
        let row = match self.model.rows.get(self.model.selected) {
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
        self.model.is_connected(server)
    }

    /// The server a structural edit (create folder/session) should target:
    /// the selected node's server, but only if it is connected. Returns `None`
    /// for the saved group, dormant sessions, and not-connected servers.
    fn structural_target_server(&self) -> Option<ConnId> {
        let row = self.model.rows.get(self.model.selected)?;
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
        self.model.folder_names_for(server)
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
        let node = match self.model.rows.get(self.model.selected) {
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
            AddToView => {
                // Marked panes (or the highlighted pane) are added to a view.
                // Drop any whose server is no longer connected so a stale mark
                // from a since-disconnected remote can't be added.
                let mut panes = self.take_marked_or_highlighted_panes();
                panes.retain(|(server, _)| self.is_connected(server));
                if panes.is_empty() {
                    SessionManagerAction::None
                } else {
                    SessionManagerAction::AddToView { panes }
                }
            }
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

    /// Build the ordered `(key, label)` cells for the help footer. When a chord
    /// prefix is pending, only chords beginning with that prefix are shown (a
    /// mini which-key); otherwise the full chord list (built from the effective
    /// bindings, so user overrides are reflected) plus the fixed navigation keys
    /// are shown.
    fn footer_cells(&self) -> Vec<(String, String)> {
        let pending = self.pending_chord;
        let mut chords: Vec<(String, SessionManagerBinding)> = self
            .bindings
            .iter()
            .map(|(c, b)| (c.to_string(), b))
            .collect();
        if let Some(p) = pending {
            chords.retain(|(c, _)| c.starts_with(p));
        }
        chords.sort_by(|(a, _), (b, _)| {
            chord_group_rank(a)
                .cmp(&chord_group_rank(b))
                .then_with(|| a.cmp(b))
        });
        let mut cells: Vec<(String, String)> = chords
            .into_iter()
            .map(|(c, b)| (c, binding_label(b).to_string()))
            .collect();
        // Fixed navigation keys are only shown in the full (non-pending) view.
        if pending.is_none() {
            for (k, l) in [
                ("Enter", "open"),
                ("d", "delete"),
                ("Esc", "close"),
                ("j/k", "nav"),
            ] {
                cells.push((k.to_string(), l.to_string()));
            }
        }
        cells
    }

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

        // Top border with title. When panes are marked, surface the count so it
        // is visible even if the marked rows are scrolled off or in a collapsed
        // tab (also a stable string for the PTY harness to assert).
        let marked_n = self.marked_count();
        let title = if marked_n > 0 {
            format!(" Session Manager ({marked_n} marked) ")
        } else {
            " Session Manager ".to_string()
        };
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

        // Search row, directly under the top border. Focused, it carries a
        // block cursor and the accent color so there is no doubt where the
        // keystrokes are going.
        let search_y = start_y + 1;
        let search_text = if self.search_focused {
            format!(" / {}\u{2588}", self.model.query)
        } else if self.model.query.is_empty() {
            " / (search)".to_string()
        } else {
            format!(" / {}", self.model.query)
        };
        let search_fg = if self.search_focused { current_fg } else { fg };
        commands.push(DrawCommand {
            x: start_x,
            y: search_y,
            text: "\u{2502}".to_string(),
            fg: border_fg,
            bg,
        });
        commands.push(DrawCommand {
            x: start_x + 1,
            y: search_y,
            text: pad_to_display_width(&search_text, inner_width),
            fg: search_fg,
            bg,
        });
        commands.push(DrawCommand {
            x: start_x + 1 + inner_width as u16,
            y: search_y,
            text: "\u{2502}".to_string(),
            fg: border_fg,
            bg,
        });
        // Separator between the search row and the tree.
        commands.push(DrawCommand {
            x: start_x,
            y: search_y + 1,
            text: format!("\u{251C}{}\u{2524}", "\u{2500}".repeat(inner_width)),
            fg: border_fg,
            bg,
        });

        // Help footer: the dynamic chord list (or, when a chord prefix is
        // pending, a mini which-key of that prefix's chords), packed into up to
        // 3 lines that fit the popup width. On a very short popup the footer is
        // capped so the fixed chrome (5 rows: two borders, the search row and
        // the two separators) still fits.
        let footer_budget = (popup_height as usize).saturating_sub(5).clamp(1, 3);
        let footer_cells = self.footer_cells();
        let footer_lines = pack_footer_lines(&footer_cells, inner_width, footer_budget);
        let footer_n = footer_lines.len().max(1);

        // Content area (rows): popup height minus the top border (1), the
        // search row (1), its separator (1), the prompt/separator line (1), the
        // footer lines, and the bottom border (1).
        let content_height = (popup_height as usize).saturating_sub(5 + footer_n);
        let scroll_offset = if self.model.selected >= content_height {
            self.model.selected - content_height + 1
        } else {
            0
        };

        for row_idx in 0..content_height {
            let tree_idx = scroll_offset + row_idx;
            // +3: top border, search row, search separator.
            let y = start_y + 3 + row_idx as u16;

            if let Some(row) = self.model.rows.get(tree_idx) {
                let is_selected = tree_idx == self.model.selected;
                let indent = "  ".repeat(row.indent);

                // Is this a pane marked for "add to view"? Keyed by identity so
                // the highlight survives tree rebuilds.
                let is_marked = matches!(
                    &row.node_type,
                    NodeType::Pane { server, pane_id, .. }
                        if self.marked.iter().any(|(s, p)| s == server && p == pane_id)
                );

                // A marked pane gets a distinct leading glyph (a filled circle,
                // NOT the '*' focus/current marker) in place of its blank indent.
                let expand_marker = match &row.node_type {
                    NodeType::Pane { .. } if is_marked => "\u{25CF} ",
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
                } else if is_marked || row.is_current {
                    // Marked (and current) rows use the accent color so they
                    // stand out even when not the highlighted row.
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
            SubMode::ConfirmDelete { description, .. } => {
                format!(" Delete {}? (y/n) ", description)
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
        let sep_y = start_y + 3 + content_height as u16;
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

        // Help footer lines (bound chord list, or a pending-prefix mini
        // which-key). Each line's chord keys are overlaid in the highlight
        // color, mirroring the which-key popup.
        let footer_y0 = sep_y + 1;
        for i in 0..footer_n {
            let y = footer_y0 + i as u16;

            // Left border.
            commands.push(DrawCommand {
                x: start_x,
                y,
                text: "\u{2502}".to_string(),
                fg: border_fg,
                bg,
            });

            // Build the line text and record each chord key's column offset so
            // it can be re-drawn in the highlight color afterwards.
            let mut line_text = String::new();
            let mut w = 0usize;
            let mut key_cols: Vec<(usize, String)> = Vec::new();
            if let Some(line) = footer_lines.get(i) {
                for (idx, (key, label)) in line.iter().enumerate() {
                    if idx > 0 {
                        line_text.push_str(FOOTER_SEP);
                        w += UnicodeWidthStr::width(FOOTER_SEP);
                    }
                    key_cols.push((w, key.clone()));
                    let cell = format!("{key} {label}");
                    w += UnicodeWidthStr::width(cell.as_str());
                    line_text.push_str(&cell);
                }
            }
            let content = pad_to_display_width(&line_text, inner_width);
            commands.push(DrawCommand {
                x: start_x + 1,
                y,
                text: content,
                fg: theme.separator_fg,
                bg,
            });

            // Overlay each chord key in the highlight color (clipped to width).
            for (col, key) in key_cols {
                if col >= inner_width {
                    continue;
                }
                let avail = inner_width - col;
                let key_disp: String = key.chars().take(avail).collect();
                if key_disp.is_empty() {
                    continue;
                }
                commands.push(DrawCommand {
                    x: start_x + 1 + col as u16,
                    y,
                    text: key_disp,
                    fg: current_fg,
                    bg,
                });
            }

            // Right border.
            commands.push(DrawCommand {
                x: start_x + 1 + inner_width as u16,
                y,
                text: "\u{2502}".to_string(),
                fg: border_fg,
                bg,
            });
        }

        // Bottom border.
        let bottom_y = footer_y0 + footer_n as u16;
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
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Server { id, .. } if id == server))
            .unwrap();
        state.model.selected = idx;
        state.expand_selected();
    }

    /// Index of the first row matching the named local session.
    fn session_row(state: &SessionManagerState, name: &str) -> usize {
        state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Session { name: n, .. } if n == name))
            .unwrap()
    }

    #[test]
    fn test_new_state_is_empty() {
        let state = SessionManagerState::new(None);
        // No tree data yet, so no rows built.
        assert!(state.model.rows.is_empty());
        assert_eq!(state.model.selected, 0);
    }

    #[test]
    fn test_update_tree_builds_rows() {
        let mut state = SessionManagerState::new(Some("project-a".to_string()));
        local_tree(&mut state);

        // Row 0 is the local server node; the folder nests beneath it.
        assert!(matches!(
            state.model.rows[0].node_type,
            NodeType::Server {
                id: ConnId::Local,
                ..
            }
        ));
        assert!(state
            .model
            .rows
            .iter()
            .any(|r| matches!(&r.node_type, NodeType::Folder { name, .. } if name == "work")));
    }

    #[test]
    fn test_navigation_wraps() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        let total = state.model.rows.len();
        state.model.selected = total - 1;
        state.select_next();
        assert_eq!(state.model.selected, 0);

        state.select_prev();
        assert_eq!(state.model.selected, total - 1);
    }

    #[test]
    fn test_toggle_expand_collapse() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        // Find the folder row and collapse it.
        let folder_idx = state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Folder { name, .. } if name == "work"))
            .unwrap();
        let initial_count = state.model.rows.len();
        state.model.selected = folder_idx;
        state.collapse_selected();
        assert!(state.model.rows.len() < initial_count);

        // Expand it back.
        state.expand_selected();
        assert_eq!(state.model.rows.len(), initial_count);
    }

    #[test]
    fn test_enter_on_session_returns_switch() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        state.model.selected = session_row(&state, "project-a");
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

        state.model.selected = 0; // local server node
        let initial_count = state.model.rows.len();
        let action = state.handle_enter();
        assert!(matches!(action, SessionManagerAction::None));
        // Server was expanded, now collapsed -> only the server row remains.
        assert!(state.model.rows.len() < initial_count);
        assert_eq!(state.model.rows.len(), 1);
    }

    /// A confirmed delete must act on the node the PROMPT named, even if the
    /// tree was rebuilt underneath the prompt.
    ///
    /// The window is two keystrokes wide (`d` then `y`) and a sidebar holding a
    /// standing `SubscribeSessionTree` rebuilds the tree on ANY structural
    /// change on ANY connected server. When the selected key disappears,
    /// `rebuild_rows` deliberately leaves the index where it was -- so the
    /// selection slides onto the next row, and a confirm that re-read the
    /// selection would delete a session the user never saw named, with no
    /// second confirmation.
    #[test]
    fn a_delete_never_retargets_when_the_tree_rebuilds_under_the_prompt() {
        let unfiled = |names: &[&str]| -> Vec<SessionTreeEntry> {
            names
                .iter()
                .map(|n| SessionTreeEntry {
                    name: (*n).to_string(),
                    tabs: vec![],
                    client_count: 0,
                    is_current: false,
                })
                .collect()
        };

        let mut state = SessionManagerState::new(None);
        state.update_tree(
            ConnId::Local,
            Vec::new(),
            unfiled(&["beta", "gamma"]),
            Vec::new(),
        );

        let beta_idx = session_row(&state, "beta");
        state.model.selected = beta_idx;

        // Arm the prompt on `beta`.
        state.handle_delete_key();
        assert!(
            matches!(&state.sub_mode, SubMode::ConfirmDelete { description, .. }
                     if description == "session 'beta'"),
            "the prompt must name beta, got {:?}",
            state.sub_mode
        );

        // Another client kills `beta`; the push rebuilds the tree. `gamma` now
        // occupies the index the selection is parked on.
        state.update_tree(ConnId::Local, Vec::new(), unfiled(&["gamma"]), Vec::new());
        assert_eq!(
            state.model.selected, beta_idx,
            "precondition: the stale index is what makes this dangerous"
        );
        assert!(
            matches!(&state.model.rows[beta_idx].node_type,
                     NodeType::Session { name, .. } if name == "gamma"),
            "precondition: the selection must now sit on gamma"
        );

        // Confirming must delete NOTHING -- beta is gone, and gamma was never
        // named by the prompt.
        let action = state.handle_confirm_delete(true);
        assert!(
            matches!(action, SessionManagerAction::None),
            "a vanished target must abort, not retarget -- got {action:?}"
        );
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    /// The companion case: the target still exists but the rows moved around it.
    ///
    /// This one is a GUARD, not a bug-catcher -- it passes against the old
    /// re-read-the-selection code too, because identity-preserving
    /// `rebuild_rows` already moved the index onto `beta`. It is here so the
    /// capture cannot regress the ordinary path while fixing the dangerous
    /// one.
    #[test]
    fn a_delete_follows_its_target_when_rows_shift_under_the_prompt() {
        let entry = |n: &str| SessionTreeEntry {
            name: n.to_string(),
            tabs: vec![],
            client_count: 0,
            is_current: false,
        };

        let mut state = SessionManagerState::new(None);
        state.update_tree(
            ConnId::Local,
            Vec::new(),
            vec![entry("alpha"), entry("beta")],
            Vec::new(),
        );
        state.model.selected = session_row(&state, "beta");
        state.handle_delete_key();

        // `alpha` disappears: every row below it shifts up, so the index that
        // named `beta` now names something else -- but `beta` is still there.
        state.update_tree(ConnId::Local, Vec::new(), vec![entry("beta")], Vec::new());

        let action = state.handle_confirm_delete(true);
        assert!(
            matches!(&action, SessionManagerAction::DeleteSession { name, .. } if name == "beta"),
            "the delete must follow beta, not the index -- got {action:?}"
        );
    }

    #[test]
    fn test_delete_confirmation_flow() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        state.model.selected = session_row(&state, "project-a");

        // Press 'd'.
        let action = state.handle_delete_key();
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::ConfirmDelete { .. }));

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
            (
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            ),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::NotConnected,
                None,
            ),
        ]);

        // Find the remote server row.
        let remote_idx = state
            .model.rows
            .iter()
            .position(|r| {
                matches!(&r.node_type, NodeType::Server { id: ConnId::Remote(n), .. } if n == "pi")
            })
            .unwrap();
        state.model.selected = remote_idx;
        let action = state.handle_enter();
        assert!(matches!(action, SessionManagerAction::ConnectRemote(ref n) if n == "pi"));
    }

    #[test]
    fn test_structural_edit_guarded_on_disconnected_remote() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            ),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::NotConnected,
                None,
            ),
        ]);

        // Select the not-connected remote server node and try to create a
        // folder -> no-op, no sub-mode (structural edits require a connection).
        let remote_idx = state
            .model.rows
            .iter()
            .position(|r| {
                matches!(&r.node_type, NodeType::Server { id: ConnId::Remote(n), .. } if n == "pi")
            })
            .unwrap();
        state.model.selected = remote_idx;
        let action = state.handle_create_folder_key();
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_create_session_key_on_connected_remote_targets_remote() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            ),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
                None,
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
            .model.rows
            .iter()
            .position(|r| {
                matches!(&r.node_type, NodeType::Server { id: ConnId::Remote(n), .. } if n == "pi")
            })
            .unwrap();
        state.model.selected = remote_idx;
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
            (
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            ),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
                None,
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
            .model.rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Session { server: ConnId::Remote(s), name } if s == "pi" && name == "project-a"))
            .unwrap();
        state.model.selected = remote_session_idx;
        let action = state.handle_delete_key();
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::ConfirmDelete { .. }));

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
            (
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            ),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
                None,
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
            .model.rows
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
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Folder { name, .. } if name == "work"))
            .unwrap();
        state.model.selected = folder_idx;
        state.expand_selected();
        state.model.selected = session_row(&state, "project-a");
        state.expand_selected();

        // Select the tab row.
        let tab_idx = state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { .. }))
            .unwrap();
        let tab_key = state
            .model
            .node_key(&state.model.rows[tab_idx].node_type.clone());

        // handle_enter on the tab still SWITCHES (returns SwitchTab).
        state.model.selected = tab_idx;
        let enter_action = state.handle_enter();
        assert!(matches!(
            enter_action,
            SessionManagerAction::SwitchTab { .. }
        ));

        // handle_expand on the tab EXPANDS: inserts the tab key, no switch.
        // Re-find the tab row (rebuilds may have shifted indices).
        let tab_idx = state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { .. }))
            .unwrap();
        state.model.selected = tab_idx;
        let expand_action = state.handle_expand();
        assert!(matches!(expand_action, SessionManagerAction::None));
        assert!(state.model.expanded.contains(&tab_key));
        // The tab's pane is now visible.
        assert!(state
            .model
            .rows
            .iter()
            .any(|r| matches!(&r.node_type, NodeType::Pane { .. })));
    }

    #[test]
    fn test_handle_expand_remote_server_triggers_lazy_connect() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            ),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::NotConnected,
                None,
            ),
        ]);

        let remote_idx = state
            .model.rows
            .iter()
            .position(|r| {
                matches!(&r.node_type, NodeType::Server { id: ConnId::Remote(n), .. } if n == "pi")
            })
            .unwrap();
        state.model.selected = remote_idx;
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
        assert!(state.model.rows.iter().any(|r| matches!(
            &r.node_type,
            NodeType::SavedGroup {
                server: ConnId::Local
            }
        )));
        // Both dormant sessions are shown as DormantSession rows (expanded by
        // default).
        let dormant_names: Vec<&str> = state
            .model
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
            .model
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
            .model.rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::DormantSession { name, .. } if name == "saved-a"))
            .unwrap();
        state.model.selected = idx;
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
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::SavedGroup { .. }))
            .unwrap();
        state.model.selected = group_idx;
        // Group starts expanded (dormant row visible); Enter collapses it.
        let action = state.handle_enter();
        assert!(matches!(action, SessionManagerAction::None));
        assert!(!state
            .model
            .rows
            .iter()
            .any(|r| matches!(&r.node_type, NodeType::DormantSession { .. })));
    }

    #[test]
    fn test_dormant_only_for_local_server() {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            ),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
                None,
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
            .model
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
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { session: s, .. } if s == session))
            .unwrap()
    }

    /// A connected-remote ("pi") state with the sample tree loaded + expanded.
    fn remote_state_with_tree() -> SessionManagerState {
        let mut state = SessionManagerState::new(None);
        state.set_roster(vec![
            (
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            ),
            (
                ConnId::Remote("pi".to_string()),
                "pi".to_string(),
                RemoteState::Connected,
                None,
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
        state.model.selected = tab_row(&state, "project-a");

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
        state.model.selected = session_row(&state, "project-a");
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
        state.model.selected = tab_row(&state, "project-a");
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
        state.model.selected = tab_row(&state, "project-a");
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
        state.model.selected = tab_row(&state, "project-a");
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
        state.model.selected = tab_row(&state, "project-a");
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
        state.model.selected = tab_row(&state, "project-a");
        state.expand_selected();
        let pane_idx = state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Pane { .. }))
            .unwrap();
        state.model.selected = pane_idx;
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
        state.model.selected = session_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::PaneClose);
        assert!(matches!(action, SessionManagerAction::None));
    }

    #[test]
    fn test_session_close_on_session_enters_confirm() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.model.selected = session_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::SessionClose);
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::ConfirmDelete { .. }));
    }

    #[test]
    fn test_session_close_on_folder_is_noop() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        let folder_idx = state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Folder { .. }))
            .unwrap();
        state.model.selected = folder_idx;
        let action = state.apply_binding(SessionManagerBinding::SessionClose);
        assert!(matches!(action, SessionManagerAction::None));
        // No confirm sub-mode was entered — the folder is untouched.
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_folder_delete_on_session_is_noop() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.model.selected = session_row(&state, "project-a");
        let action = state.apply_binding(SessionManagerBinding::FolderDelete);
        assert!(matches!(action, SessionManagerAction::None));
        assert!(matches!(state.sub_mode, SubMode::Navigate));
    }

    #[test]
    fn test_tab_rename_on_remote_tab_carries_remote() {
        let mut state = remote_state_with_tree();
        let idx = state
            .model.rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { server: ConnId::Remote(s), session, .. } if s == "pi" && session == "project-a"))
            .unwrap();
        state.model.selected = idx;

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
            .model.rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { server: ConnId::Remote(s), .. } if s == "pi"))
            .unwrap();
        state.model.selected = tab_idx;
        state.expand_selected();

        let pane_idx = state
            .model.rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Pane { server: ConnId::Remote(s), .. } if s == "pi"))
            .unwrap();
        state.model.selected = pane_idx;

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

    // -----------------------------------------------------------------------
    // Help footer (bound-chord hints)
    // -----------------------------------------------------------------------

    /// (a) The footer lists a configured chord alongside its short label.
    #[test]
    fn test_footer_lists_configured_chord_and_label() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        let cells = state.footer_cells();
        // Default binding tn -> TabNew, labelled "tab new".
        assert!(cells.contains(&("tn".to_string(), "tab new".to_string())));
        // Fixed navigation keys are present in the full (non-pending) view.
        assert!(cells.iter().any(|(k, l)| k == "Enter" && l == "open"));
    }

    /// (a') End-to-end: a surviving chord's label and highlighted key both make
    /// it into the rendered draw commands. `sn` sorts into the first footer line
    /// even at the default 80x24 popup width.
    #[test]
    fn test_footer_chord_rendered() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        let theme = Theme::default();
        let cmds = state.render(80, 24, &theme);
        // The label appears in a footer line's text...
        assert!(cmds.iter().any(|c| c.text.contains("session new")));
        // ...and the chord key is overlaid as its own (highlighted) command.
        assert!(cmds.iter().any(|c| c.text == "sn"));
    }

    /// (b) A user override is reflected: the overridden chord (and not the
    /// replaced default) appears for the action.
    #[test]
    fn test_footer_reflects_user_override() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);

        // Rebind TabNew to "zt" and unbind the default "tn".
        let toml_str = "zt = \"TabNew\"\ntn = \"\"\n";
        let value: toml::Value = toml_str.parse().unwrap();
        state.set_bindings(SessionManagerBindings::from_toml(&value));

        let cells = state.footer_cells();
        // The overridden chord carries the "tab new" label.
        assert!(cells.contains(&("zt".to_string(), "tab new".to_string())));
        // The default "tn" chord is gone.
        assert!(!cells.iter().any(|(k, _)| k == "tn"));
    }

    /// (c) With a pending chord prefix the footer is a mini which-key: only
    /// chords beginning with that prefix (e.g. "tab new", never "pane new").
    #[test]
    fn test_footer_pending_prefix_filters() {
        let mut state = SessionManagerState::new(None);
        local_tree(&mut state);
        state.feed_chord('t');

        let cells = state.footer_cells();
        // Only t-chords remain (fixed keys are suppressed too).
        assert!(cells.iter().all(|(k, _)| k.starts_with('t')));
        assert!(cells.iter().any(|(_, l)| l == "tab new"));
        assert!(!cells.iter().any(|(_, l)| l == "pane new"));
    }

    // -- Pane multi-select marking ("add to view") ---------------------------

    /// A single-session, two-pane local state with the tab expanded so both
    /// pane rows are present. Pane ids are 10 and 11.
    fn two_pane_state() -> SessionManagerState {
        let mut state = SessionManagerState::new(None);
        let folders = vec![FolderTreeEntry {
            name: "work".to_string(),
            sessions: vec![SessionTreeEntry {
                name: "multi".to_string(),
                tabs: vec![TabTreeEntry {
                    id: 1,
                    name: "Tab 1".to_string(),
                    panes: vec![
                        PaneTreeEntry {
                            id: 10,
                            name: "p10".to_string(),
                            is_focused: true,
                        },
                        PaneTreeEntry {
                            id: 11,
                            name: "p11".to_string(),
                            is_focused: false,
                        },
                    ],
                }],
                client_count: 1,
                is_current: false,
            }],
        }];
        state.update_tree(ConnId::Local, folders, Vec::new(), Vec::new());
        let tab_idx = state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { .. }))
            .unwrap();
        state.model.selected = tab_idx;
        state.expand_selected();
        state
    }

    fn pane_row_by_id(state: &SessionManagerState, id: u64) -> usize {
        state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Pane { pane_id, .. } if *pane_id == id))
            .unwrap()
    }

    #[test]
    fn test_toggle_mark_only_on_pane() {
        let mut state = two_pane_state();
        // On a non-pane row (the tab), toggle is a no-op.
        assert!(!state.toggle_mark());
        assert_eq!(state.marked_count(), 0);

        // On a pane row, toggle marks it; toggling again unmarks.
        state.model.selected = pane_row_by_id(&state, 10);
        assert!(state.toggle_mark());
        assert_eq!(state.marked_count(), 1);
        assert!(state.toggle_mark());
        assert_eq!(state.marked_count(), 0);
    }

    #[test]
    fn test_take_marked_returns_drained_set() {
        let mut state = two_pane_state();
        state.model.selected = pane_row_by_id(&state, 10);
        state.toggle_mark();
        state.model.selected = pane_row_by_id(&state, 11);
        state.toggle_mark();
        assert_eq!(state.marked_count(), 2);

        let panes = state.take_marked_or_highlighted_panes();
        assert_eq!(panes, vec![(ConnId::Local, 10u64), (ConnId::Local, 11u64)]);
        // Draining clears the marks.
        assert_eq!(state.marked_count(), 0);
    }

    #[test]
    fn test_take_falls_back_to_highlighted_pane() {
        let mut state = two_pane_state();
        state.model.selected = pane_row_by_id(&state, 11);
        // No marks: the single highlighted pane is returned.
        let panes = state.take_marked_or_highlighted_panes();
        assert_eq!(panes, vec![(ConnId::Local, 11u64)]);
    }

    #[test]
    fn test_take_empty_on_non_pane_with_no_marks() {
        let mut state = two_pane_state();
        // Highlight the tab (not a pane), no marks -> empty.
        state.model.selected = state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Tab { .. }))
            .unwrap();
        assert!(state.take_marked_or_highlighted_panes().is_empty());
    }

    #[test]
    fn test_mark_survives_rebuild_rows() {
        let mut state = two_pane_state();
        state.model.selected = pane_row_by_id(&state, 10);
        state.toggle_mark();
        assert_eq!(state.marked_count(), 1);

        // A fresh tree (e.g. an async refresh) rebuilds all rows; the mark is
        // keyed by (server, pane_id), so it must survive.
        let folders = vec![FolderTreeEntry {
            name: "work".to_string(),
            sessions: vec![SessionTreeEntry {
                name: "multi".to_string(),
                tabs: vec![TabTreeEntry {
                    id: 1,
                    name: "Tab 1".to_string(),
                    panes: vec![PaneTreeEntry {
                        id: 10,
                        name: "p10".to_string(),
                        is_focused: true,
                    }],
                }],
                client_count: 1,
                is_current: false,
            }],
        }];
        state.update_tree(ConnId::Local, folders, Vec::new(), Vec::new());
        assert_eq!(state.marked_count(), 1);
    }

    #[test]
    fn test_add_to_view_binding_emits_marked_panes() {
        let mut state = two_pane_state();
        state.model.selected = pane_row_by_id(&state, 10);
        state.toggle_mark();
        state.model.selected = pane_row_by_id(&state, 11);
        state.toggle_mark();

        let action = state.apply_binding(SessionManagerBinding::AddToView);
        match action {
            SessionManagerAction::AddToView { panes } => {
                assert_eq!(panes, vec![(ConnId::Local, 10u64), (ConnId::Local, 11u64)]);
            }
            other => panic!("expected AddToView, got {other:?}"),
        }
        // Marks consumed.
        assert_eq!(state.marked_count(), 0);
    }

    #[test]
    fn test_add_to_view_binding_noop_on_non_pane_without_marks() {
        let mut state = two_pane_state();
        state.model.selected = state
            .model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Session { .. }))
            .unwrap();
        let action = state.apply_binding(SessionManagerBinding::AddToView);
        assert!(matches!(action, SessionManagerAction::None));
    }

    // -----------------------------------------------------------------------
    // Search / filtering
    // -----------------------------------------------------------------------

    /// A tree with distinct folder / session / tab / pane names at every level,
    /// so a query can be aimed at exactly one depth.
    fn search_tree() -> (Vec<FolderTreeEntry>, Vec<SessionTreeEntry>) {
        let pane = |id: u64, name: &str| PaneTreeEntry {
            id,
            name: name.to_string(),
            is_focused: false,
        };
        let folders = vec![
            FolderTreeEntry {
                name: "clients".to_string(),
                sessions: vec![SessionTreeEntry {
                    name: "alpha".to_string(),
                    tabs: vec![
                        TabTreeEntry {
                            id: 1,
                            name: "editor".to_string(),
                            panes: vec![pane(10, "vim"), pane(11, "logtail")],
                        },
                        TabTreeEntry {
                            id: 2,
                            name: "shell".to_string(),
                            panes: vec![pane(12, "bash")],
                        },
                    ],
                    client_count: 0,
                    is_current: false,
                }],
            },
            FolderTreeEntry {
                name: "personal".to_string(),
                sessions: vec![SessionTreeEntry {
                    name: "beta".to_string(),
                    tabs: vec![TabTreeEntry {
                        id: 3,
                        name: "notes".to_string(),
                        panes: vec![pane(13, "nvim")],
                    }],
                    client_count: 0,
                    is_current: false,
                }],
            },
        ];
        let unfiled = vec![SessionTreeEntry {
            name: "gamma".to_string(),
            tabs: vec![TabTreeEntry {
                id: 4,
                name: "build".to_string(),
                panes: vec![pane(14, "cargo")],
            }],
            client_count: 0,
            is_current: false,
        }];
        (folders, unfiled)
    }

    fn search_state(dormant: Vec<String>) -> SessionManagerState {
        let mut state = SessionManagerState::new(None);
        let (folders, unfiled) = search_tree();
        state.update_tree(ConnId::Local, folders, unfiled, dormant);
        state
    }

    /// Compact `kind:name` label per visible row, for exact assertions.
    fn row_labels(state: &SessionManagerState) -> Vec<String> {
        state
            .model
            .rows
            .iter()
            .map(|r| match &r.node_type {
                NodeType::Server { id, .. } => format!("server:{}", id.key()),
                NodeType::Folder { name, .. } => format!("folder:{name}"),
                NodeType::Session { name, .. } => format!("session:{name}"),
                NodeType::Tab { .. } => format!("tab:{}", r.display_name),
                NodeType::Pane { .. } => {
                    format!("pane:{}", r.display_name.trim_end_matches('*'))
                }
                NodeType::SavedGroup { .. } => "saved".to_string(),
                NodeType::DormantSession { name, .. } => format!("dormant:{name}"),
            })
            .collect()
    }

    /// Type `q` into the search bar one char at a time (the same path the key
    /// handler drives).
    fn type_query(state: &mut SessionManagerState, q: &str) {
        for c in q.chars() {
            state.push_query_char(c);
        }
    }

    /// Index of the first row whose label matches.
    fn label_row(state: &SessionManagerState, label: &str) -> usize {
        row_labels(state).iter().position(|l| l == label).unwrap()
    }

    #[test]
    fn search_empty_query_changes_nothing() {
        let mut state = search_state(Vec::new());
        let before = row_labels(&state);
        // Type then delete: back to an empty query, and the tree is identical.
        type_query(&mut state, "al");
        assert_ne!(row_labels(&state), before);
        state.pop_query_char();
        state.pop_query_char();
        assert!(state.model.query.is_empty());
        assert_eq!(row_labels(&state), before);
    }

    #[test]
    fn search_tab_name_drags_session_and_folder_into_view() {
        let mut state = search_state(Vec::new());
        type_query(&mut state, "editor");
        let labels = row_labels(&state);
        // The match's full ancestor chain is shown...
        assert_eq!(
            labels,
            vec![
                "server:local".to_string(),
                "folder:clients".to_string(),
                "session:alpha".to_string(),
                "tab:editor".to_string(),
            ],
            "tab match must show its session and folder and nothing else"
        );
        // ...and the matching tab is not force-opened, so its panes stay under
        // the user's own expansion state (the tab was never expanded).
        assert!(!labels.iter().any(|l| l.starts_with("pane:")));
    }

    #[test]
    fn search_pane_name_shows_the_whole_chain() {
        let mut state = search_state(Vec::new());
        type_query(&mut state, "logtail");
        assert_eq!(
            row_labels(&state),
            vec![
                "server:local".to_string(),
                "folder:clients".to_string(),
                "session:alpha".to_string(),
                "tab:editor".to_string(),
                "pane:logtail".to_string(),
            ],
            "pane match must force its tab open and show the chain above it"
        );
    }

    #[test]
    fn search_session_name_keeps_its_subtree_browsable() {
        let mut state = search_state(Vec::new());
        type_query(&mut state, "ALPHA"); // case-insensitive
        let labels = row_labels(&state);
        assert!(labels.contains(&"session:alpha".to_string()));
        assert!(labels.contains(&"folder:clients".to_string()));
        // A matching session is visible with its tabs (the session is expanded),
        // but the sibling folder and the unfiled session are filtered out.
        assert!(labels.contains(&"tab:editor".to_string()));
        assert!(!labels.contains(&"folder:personal".to_string()));
        assert!(!labels.contains(&"session:gamma".to_string()));
    }

    #[test]
    fn search_no_match_leaves_only_server_rows() {
        let mut state = search_state(Vec::new());
        type_query(&mut state, "zzzznope");
        assert_eq!(row_labels(&state), vec!["server:local".to_string()]);
        // Nothing panics with a stale selection against the shrunken list.
        state.select_next();
        state.select_prev();
        assert!(matches!(state.handle_enter(), SessionManagerAction::None));
        assert!(matches!(
            state.handle_delete_key(),
            SessionManagerAction::None
        ));
    }

    #[test]
    fn search_matches_dormant_sessions() {
        let mut state = search_state(vec!["saved-one".to_string(), "other".to_string()]);
        type_query(&mut state, "saved");
        assert_eq!(
            row_labels(&state),
            vec![
                "server:local".to_string(),
                "saved".to_string(),
                "dormant:saved-one".to_string(),
            ],
        );
        // The "Saved (resurrect)" header is an ancestor, not the match: the
        // selection belongs on the dormant session itself.
        assert_eq!(
            row_labels(&state)[state.model.selected],
            "dormant:saved-one"
        );
    }

    #[test]
    fn search_does_not_permanently_expand_the_tree() {
        let mut state = search_state(Vec::new());
        // Collapse a folder FIRST, so "restored" is a state the auto-expansion
        // in `update_tree` did not already produce.
        state.model.selected = label_row(&state, "folder:personal");
        state.collapse_selected();
        let rows_before = row_labels(&state);
        let expanded_before = state.model.expanded.clone();
        assert!(!rows_before.contains(&"session:beta".to_string()));

        // A query that only matches inside the collapsed folder must reveal it.
        type_query(&mut state, "notes");
        assert_eq!(
            row_labels(&state),
            vec![
                "server:local".to_string(),
                "folder:personal".to_string(),
                "session:beta".to_string(),
                "tab:notes".to_string(),
            ],
        );

        // Clearing the query restores the user's expansion state exactly.
        state.clear_query();
        assert_eq!(
            state.model.expanded, expanded_before,
            "expanded set was mutated"
        );
        assert_eq!(row_labels(&state), rows_before);
    }

    #[test]
    fn search_query_change_replaces_a_stale_selection() {
        let mut state = search_state(Vec::new());
        state.model.selected = 3;
        type_query(&mut state, "a");
        // The stale index is gone, and the selection is NOT parked on the server
        // row at 0 -- nor on `folder:clients`, which only renders because a
        // session beneath it matched. `alpha` is the topmost direct hit.
        assert_ne!(state.model.selected, 3);
        assert_eq!(row_labels(&state)[state.model.selected], "session:alpha");
    }

    #[test]
    fn search_selection_lands_on_a_foldered_session_not_its_folder() {
        let mut state = search_state(Vec::new());
        // `alpha` lives inside the `clients` folder, so the filtered tree is
        // server > folder > session: the first non-server row is the FOLDER, and
        // Enter there would merely toggle it open.
        type_query(&mut state, "alpha");
        let labels = row_labels(&state);
        assert_eq!(labels[1], "folder:clients", "the ancestor folder must show");
        assert_eq!(
            labels[state.model.selected], "session:alpha",
            "the selection landed on an ancestor instead of the direct hit"
        );
        assert!(matches!(
            state.handle_enter(),
            SessionManagerAction::SwitchSession { server: ConnId::Local, session }
                if session == "alpha"
        ));
    }

    #[test]
    fn search_selection_lands_on_a_tab_not_its_ancestors() {
        let mut state = search_state(Vec::new());
        // Two ancestor rows sit above this match, and neither is what was typed.
        type_query(&mut state, "editor");
        let labels = row_labels(&state);
        assert_eq!(
            labels,
            vec![
                "server:local".to_string(),
                "folder:clients".to_string(),
                "session:alpha".to_string(),
                "tab:editor".to_string(),
            ],
            "the ancestors must still be visible above the match"
        );
        assert_eq!(labels[state.model.selected], "tab:editor");
    }

    #[test]
    fn search_selection_lands_on_a_matching_folder() {
        let mut state = search_state(Vec::new());
        // A folder is a legitimate direct hit: nothing beneath `personal`
        // matches, so the folder row itself is what the user searched for.
        type_query(&mut state, "personal");
        assert_eq!(row_labels(&state)[state.model.selected], "folder:personal");
    }

    #[test]
    fn search_selection_lands_on_the_match_not_the_server_row() {
        let mut state = search_state(Vec::new());
        // `gamma` is an unfiled session, so the filtered tree is the server row
        // followed by the session the user actually searched for.
        type_query(&mut state, "gamma");
        assert!(
            !matches!(
                state.model.rows[state.model.selected].node_type,
                NodeType::Server { .. }
            ),
            "the selection sat on a server row after a query"
        );
        assert_eq!(row_labels(&state)[state.model.selected], "session:gamma");
        // ...so Enter activates the match instead of toggling the server open.
        assert!(matches!(
            state.handle_enter(),
            SessionManagerAction::SwitchSession { server: ConnId::Local, session }
                if session == "gamma"
        ));

        // A query that matches nothing leaves only server rows: the selection
        // falls back to the top rather than pointing past the end.
        state.clear_query();
        type_query(&mut state, "zzzznope");
        assert_eq!(row_labels(&state), vec!["server:local".to_string()]);
        assert_eq!(state.model.selected, 0);

        // Clearing the query restores the plain "top of the tree" selection.
        state.clear_query();
        assert_eq!(state.model.selected, 0);
    }

    #[test]
    fn search_bar_renders_focused_then_unfocused() {
        let theme = Theme::default();
        let mut state = search_state(Vec::new());

        // The search row is the popup's own content command on the row directly
        // under the top border; pull it out rather than asserting against every
        // draw command concatenated together.
        let search_row = |state: &SessionManagerState| -> DrawCommand {
            let (cols, rows) = (100u16, 30u16);
            let popup_width = (cols / 2).max(40).min(cols);
            let popup_height = (rows / 2).max(12).min(rows);
            let start_x = (cols.saturating_sub(popup_width)) / 2;
            let start_y = (rows.saturating_sub(popup_height)) / 2;
            state
                .render(cols, rows, &theme)
                .into_iter()
                .find(|c| c.y == start_y + 1 && c.x == start_x + 1)
                .expect("search row content command")
        };

        // Opens focused: the block cursor, painted in the accent color.
        assert!(state.search_focused);
        let row = search_row(&state);
        assert_eq!(row.text.trim(), "/ \u{2588}");
        assert_eq!(row.fg, theme.whichkey_key_fg);

        // Unfocused with an empty query: the placeholder, no cursor, normal fg.
        state.focus_tree();
        let row = search_row(&state);
        assert_eq!(row.text.trim(), "/ (search)");
        assert_eq!(row.fg, theme.whichkey_fg);

        // Unfocused with a query: the query, still no cursor.
        type_query(&mut state, "alpha");
        let row = search_row(&state);
        assert_eq!(row.text.trim(), "/ alpha");
        assert_eq!(row.fg, theme.whichkey_fg);

        // Refocused: cursor back, accent color back.
        state.focus_search();
        let row = search_row(&state);
        assert_eq!(row.text.trim(), "/ alpha\u{2588}");
        assert_eq!(row.fg, theme.whichkey_key_fg);
    }

    #[test]
    fn search_focus_drops_a_pending_chord() {
        let mut state = search_state(Vec::new());
        state.focus_tree();
        assert_eq!(state.feed_chord('t'), ChordOutcome::Pending);
        state.focus_search();
        assert_eq!(state.pending_chord(), None);
    }

    #[test]
    fn render_rows_do_not_overflow_a_tiny_popup() {
        // The popup floor (`popup_height < 6` returns early) must still lay out
        // inside its own box now that the search row and its separator exist.
        let theme = Theme::default();
        let state = search_state(Vec::new());
        for rows in 6u16..14 {
            let cmds = state.render(40, rows, &theme);
            let popup_height = (rows / 2).max(12).min(rows);
            let start_y = (rows.saturating_sub(popup_height)) / 2;
            let max_y = cmds.iter().map(|c| c.y).max().unwrap_or(0);
            assert!(
                max_y < start_y + popup_height,
                "rows={rows}: drew at y={max_y}, popup ends at {}",
                start_y + popup_height
            );
        }
    }

    #[test]
    fn test_marked_count_shows_in_title() {
        let mut state = two_pane_state();
        state.model.selected = pane_row_by_id(&state, 10);
        state.toggle_mark();
        let theme = Theme::default();
        let cmds = state.render(100, 30, &theme);
        let text: String = cmds.iter().map(|c| c.text.as_str()).collect();
        assert!(
            text.contains("(1 marked)"),
            "title should show marked count: {text:?}"
        );
    }
}
