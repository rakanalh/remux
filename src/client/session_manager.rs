//! Session manager overlay.
//!
//! Provides a tree-view popup that shows all folders, sessions, tabs, and
//! panes. The user can navigate, expand/collapse nodes, switch sessions/tabs,
//! create/delete folders and sessions, and move sessions between folders.

use std::collections::HashSet;

use crossterm::style::Color;

use crate::client::whichkey::DrawCommand;
use crate::config::theme::Theme;
use crate::protocol::{FolderTreeEntry, SessionTreeEntry};

// ---------------------------------------------------------------------------
// NodeType / TreeRow
// ---------------------------------------------------------------------------

/// The type of a node in the flattened tree view.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeType {
    Folder(String),
    Session(String),
    Tab {
        session: String,
        tab_index: usize,
    },
    Pane {
        session: String,
        tab_index: usize,
        pane_id: u64,
    },
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
    SwitchSession(String),
    SwitchTab {
        session: String,
        tab_index: usize,
    },
    SwitchPane {
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
    /// Set of expanded node keys (e.g. "folder:work", "session:proj").
    pub expanded: HashSet<String>,
    /// Current sub-mode.
    pub sub_mode: SubMode,
    /// The name of the session the client is currently attached to.
    pub current_session: Option<String>,
    /// Raw tree data from the server.
    folders: Vec<FolderTreeEntry>,
    unfiled: Vec<SessionTreeEntry>,
}

impl SessionManagerState {
    /// Create a new session manager state (initially empty).
    pub fn new(current_session: Option<String>) -> Self {
        // By default, expand all folders and sessions.
        Self {
            rows: Vec::new(),
            selected: 0,
            expanded: HashSet::new(),
            sub_mode: SubMode::Navigate,
            current_session,
            folders: Vec::new(),
            unfiled: Vec::new(),
        }
    }

    /// Update with new tree data from the server.
    pub fn update_tree(&mut self, folders: Vec<FolderTreeEntry>, unfiled: Vec<SessionTreeEntry>) {
        // On first load, auto-expand everything.
        if self.expanded.is_empty() {
            for f in &folders {
                self.expanded.insert(format!("folder:{}", f.name));
                for s in &f.sessions {
                    self.expanded.insert(format!("session:{}", s.name));
                }
            }
            for s in &unfiled {
                self.expanded.insert(format!("session:{}", s.name));
            }
        }
        self.folders = folders;
        self.unfiled = unfiled;
        self.rebuild_rows();
    }

    /// Rebuild the flat row list from the tree data, respecting expanded state.
    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();

        for folder in &self.folders {
            let folder_key = format!("folder:{}", folder.name);
            let folder_expanded = self.expanded.contains(&folder_key);
            rows.push(TreeRow {
                indent: 0,
                node_type: NodeType::Folder(folder.name.clone()),
                display_name: folder.name.clone(),
                is_expanded: folder_expanded,
                is_current: false,
            });

            if folder_expanded {
                for session in &folder.sessions {
                    self.add_session_rows(&mut rows, session, 1);
                }
            }
        }

        for session in &self.unfiled {
            self.add_session_rows(&mut rows, session, 0);
        }

        self.rows = rows;
        // Clamp selection.
        if !self.rows.is_empty() && self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
    }

    fn add_session_rows(&self, rows: &mut Vec<TreeRow>, session: &SessionTreeEntry, indent: usize) {
        let session_key = format!("session:{}", session.name);
        let session_expanded = self.expanded.contains(&session_key);
        let client_suffix = if session.client_count > 0 {
            format!(" ({})", session.client_count)
        } else {
            String::new()
        };
        rows.push(TreeRow {
            indent,
            node_type: NodeType::Session(session.name.clone()),
            display_name: format!("{}{}", session.name, client_suffix),
            is_expanded: session_expanded,
            is_current: session.is_current,
        });

        if session_expanded {
            for (tab_idx, tab) in session.tabs.iter().enumerate() {
                let tab_key = format!("tab:{}:{}", session.name, tab_idx);
                let tab_expanded = self.expanded.contains(&tab_key);
                rows.push(TreeRow {
                    indent: indent + 1,
                    node_type: NodeType::Tab {
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
            NodeType::Folder(name) => format!("folder:{}", name),
            NodeType::Session(name) => format!("session:{}", name),
            NodeType::Tab {
                session, tab_index, ..
            } => format!("tab:{}:{}", session, tab_index),
            NodeType::Pane { .. } => String::new(), // Panes don't expand.
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
            NodeType::Folder(_) => {
                self.toggle_expand();
                SessionManagerAction::None
            }
            NodeType::Session(name) => SessionManagerAction::SwitchSession(name.clone()),
            NodeType::Tab { session, tab_index } => SessionManagerAction::SwitchTab {
                session: session.clone(),
                tab_index: *tab_index,
            },
            NodeType::Pane {
                session,
                tab_index,
                pane_id,
            } => SessionManagerAction::SwitchPane {
                session: session.clone(),
                tab_index: *tab_index,
                pane_id: *pane_id,
            },
        }
    }

    /// Handle 'd' key -- enter delete confirmation sub-mode.
    pub fn handle_delete_key(&mut self) -> SessionManagerAction {
        let row = match self.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => return SessionManagerAction::None,
        };
        let description = match &row.node_type {
            NodeType::Folder(name) => format!("folder '{}'", name),
            NodeType::Session(name) => format!("session '{}'", name),
            NodeType::Tab { session, tab_index } => format!("tab {} in '{}'", tab_index, session),
            NodeType::Pane { .. } => return SessionManagerAction::None, // Cannot delete individual panes.
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

        match &row.node_type {
            NodeType::Folder(name) => SessionManagerAction::DeleteFolder(name.clone()),
            NodeType::Session(name) => SessionManagerAction::DeleteSession(name.clone()),
            NodeType::Tab { session, tab_index } => SessionManagerAction::CloseTab {
                session: session.clone(),
                tab_index: *tab_index,
            },
            NodeType::Pane { .. } => SessionManagerAction::None,
        }
    }

    /// Handle 'c' key -- enter create-folder sub-mode.
    pub fn handle_create_folder_key(&mut self) -> SessionManagerAction {
        self.sub_mode = SubMode::CreateFolder(String::new());
        SessionManagerAction::None
    }

    /// Handle 'n' key -- enter create-session sub-mode.
    pub fn handle_create_session_key(&mut self) -> SessionManagerAction {
        self.sub_mode = SubMode::CreateSession {
            name: String::new(),
            phase: CreatePhase::EnterName,
        };
        SessionManagerAction::None
    }

    /// Handle 'm' key -- enter move-session sub-mode.
    pub fn handle_move_key(&mut self) -> SessionManagerAction {
        let row = match self.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => return SessionManagerAction::None,
        };
        if let NodeType::Session(name) = &row.node_type {
            let mut folder_names: Vec<String> =
                self.folders.iter().map(|f| f.name.clone()).collect();
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

    /// Get the list of folder names (for folder selection in sub-modes).
    pub fn folder_names(&self) -> Vec<String> {
        self.folders.iter().map(|f| f.name.clone()).collect()
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    /// Render the session manager overlay as a list of draw commands.
    pub fn render(&self, screen_cols: u16, screen_rows: u16, _theme: &Theme) -> Vec<DrawCommand> {
        let mut commands = Vec::new();

        // Popup dimensions: take up ~80% of the screen, min 40x12.
        let popup_width = (screen_cols * 4 / 5).max(40).min(screen_cols);
        let popup_height = (screen_rows * 4 / 5).max(12).min(screen_rows);

        if popup_width < 20 || popup_height < 6 {
            return commands;
        }

        let start_x = (screen_cols.saturating_sub(popup_width)) / 2;
        let start_y = (screen_rows.saturating_sub(popup_height)) / 2;

        let fg = Color::AnsiValue(252);
        let bg = Color::AnsiValue(235);
        let sel_fg = Color::AnsiValue(235);
        let sel_bg = Color::AnsiValue(252);
        let current_fg = Color::AnsiValue(10);
        let border_fg = Color::AnsiValue(244);

        let inner_width = (popup_width - 2) as usize;

        // Top border with title.
        let title = " Session Manager ";
        let border_len = inner_width.saturating_sub(title.len());
        let left_border = border_len / 2;
        let right_border = border_len - left_border;
        let top_line = format!(
            "\u{256D}{}\u{2500}{}{}\u{256E}",
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
                let padded = format!("\u{2502}{:<width$}\u{2502}", text, width = inner_width);
                // Truncate if needed.
                let display: String = padded.chars().take(popup_width as usize).collect();

                let (row_fg, row_bg) = if is_selected {
                    (sel_fg, sel_bg)
                } else if row.is_current {
                    (current_fg, bg)
                } else {
                    (fg, bg)
                };

                commands.push(DrawCommand {
                    x: start_x,
                    y,
                    text: display,
                    fg: row_fg,
                    bg: row_bg,
                });
            } else {
                // Empty row.
                let empty_line = format!("\u{2502}{}\u{2502}", " ".repeat(inner_width));
                commands.push(DrawCommand {
                    x: start_x,
                    y,
                    text: empty_line,
                    fg,
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
        let sep_line = if !prompt_line.is_empty() {
            let padded = format!(
                "\u{251C}{:<width$}\u{2524}",
                prompt_line,
                width = inner_width
            );
            padded.chars().take(popup_width as usize).collect()
        } else {
            format!("\u{251C}{}\u{2524}", "\u{2500}".repeat(inner_width))
        };
        commands.push(DrawCommand {
            x: start_x,
            y: sep_y,
            text: sep_line,
            fg: border_fg,
            bg,
        });

        // Help line.
        let help_y = sep_y + 1;
        let help_text = " j/k:nav  Enter:select  d:delete  c:folder  n:session  m:move  q:quit ";
        let help_padded = format!("\u{2502}{:<width$}\u{2502}", help_text, width = inner_width);
        let help_display: String = help_padded.chars().take(popup_width as usize).collect();
        commands.push(DrawCommand {
            x: start_x,
            y: help_y,
            text: help_display,
            fg: Color::AnsiValue(244),
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
                    name: "tab-1".to_string(),
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
                name: "tab-1".to_string(),
                panes: vec![],
            }],
            client_count: 0,
            is_current: false,
        }];
        (folders, unfiled)
    }

    #[test]
    fn test_new_state_is_empty() {
        let state = SessionManagerState::new(None);
        assert!(state.rows.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_update_tree_builds_rows() {
        let mut state = SessionManagerState::new(Some("project-a".to_string()));
        let (folders, unfiled) = sample_tree();
        state.update_tree(folders, unfiled);

        // With all expanded: folder, session, tab, pane, unfiled session, unfiled tab
        assert!(state.rows.len() >= 4);
        // First row should be the folder.
        assert!(matches!(state.rows[0].node_type, NodeType::Folder(ref n) if n == "work"));
    }

    #[test]
    fn test_navigation_wraps() {
        let mut state = SessionManagerState::new(None);
        let (folders, unfiled) = sample_tree();
        state.update_tree(folders, unfiled);

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
        let (folders, unfiled) = sample_tree();
        state.update_tree(folders, unfiled);

        let initial_count = state.rows.len();
        // Collapse the folder (first row).
        state.selected = 0;
        state.collapse_selected();
        assert!(state.rows.len() < initial_count);

        // Expand it back.
        state.expand_selected();
        assert_eq!(state.rows.len(), initial_count);
    }

    #[test]
    fn test_enter_on_session_returns_switch() {
        let mut state = SessionManagerState::new(None);
        let (folders, unfiled) = sample_tree();
        state.update_tree(folders, unfiled);

        // Find the session row.
        let session_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Session(n) if n == "project-a"))
            .unwrap();
        state.selected = session_idx;
        let action = state.handle_enter();
        assert!(matches!(action, SessionManagerAction::SwitchSession(ref n) if n == "project-a"));
    }

    #[test]
    fn test_enter_on_folder_toggles_expand() {
        let mut state = SessionManagerState::new(None);
        let (folders, unfiled) = sample_tree();
        state.update_tree(folders, unfiled);

        state.selected = 0; // Folder
        let initial_count = state.rows.len();
        let action = state.handle_enter();
        assert!(matches!(action, SessionManagerAction::None));
        // Folder was expanded, now collapsed.
        assert!(state.rows.len() < initial_count);
    }

    #[test]
    fn test_delete_confirmation_flow() {
        let mut state = SessionManagerState::new(None);
        let (folders, unfiled) = sample_tree();
        state.update_tree(folders, unfiled);

        // Select the session.
        let session_idx = state
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Session(n) if n == "project-a"))
            .unwrap();
        state.selected = session_idx;

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
    fn test_render_returns_commands() {
        let mut state = SessionManagerState::new(None);
        let (folders, unfiled) = sample_tree();
        state.update_tree(folders, unfiled);

        let theme = Theme::default();
        let cmds = state.render(80, 24, &theme);
        assert!(!cmds.is_empty());
    }
}
