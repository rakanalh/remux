//! The session-tree panel: every connected server's sessions, tabs and panes,
//! live, in a sidebar.
//!
//! The tree itself is [`TreeModel`] -- the same model the session-manager
//! overlay drives -- so what a node *is*, what is expanded, and where a row
//! jumps to are one implementation shared by both surfaces. This file is only
//! the panel's own chrome (narrow, headed, scrolled to the selection) and its
//! key/mouse handling.
//!
//! Three deliberate narrowings of what the model can render, each chosen so the
//! panel never shows a row that its own key handling cannot act on:
//!
//! * **Only servers we hold a tree for.** The roster is synthesized from the
//!   connections that have pushed data, all `Connected`, so no node ever wears
//!   the model's `(offline)` / `(connecting…)` / `(failed: …)` chrome. Dialling
//!   a remote is the session manager's job -- the panel has no action for it,
//!   and an offline row in twenty columns would be a dead key with a suffix.
//! * **No dormant sessions.** Resurrecting one is a server command the panel
//!   cannot issue, so a `💤` row would be exactly the visibly dead Enter the
//!   spec is trying to avoid.
//! * **No search query.** The model's filter stays off; the overlay is where a
//!   search bar has room to live.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};

#[cfg(test)]
use super::nav::LIST_HEADER_ROWS;
use super::nav::{self, ListLayout, NavKey};
use super::{blank_grid, draw_text_with, PluginAction, PluginEvent, SidebarPlugin};
use crate::client::context_menu::{self, MenuAction, MenuItem, RenameFor};
use crate::client::registry::{ConnId, RemoteState};
use crate::client::tree_model::{ConnTrees, JumpTarget, NodeType, TreeModel};
use crate::config::sidebar::IconStyle;
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, RenderCell};

/// One icon per row type, as `[server, folder, session, tab, pane]`.
///
/// These are emoji-presentation glyphs because `unicode-width` and terminals
/// agree that those take two columns. A text-presentation pictograph such as
/// U+1F5A5 measures one column where many terminals draw two, which shifts the
/// rest of the row.
const EMOJI_ICONS: [char; 5] = [
    '\u{1F4BB}',
    '\u{1F4C1}',
    '\u{1F4FA}',
    '\u{1F4D1}',
    '\u{1F532}',
];

/// Font Awesome glyphs from the Nerd Font Private Use Area: desktop, folder,
/// terminal, clone and square-o. Each measures one column, and a font can draw one wider
/// than its cell, so the space after the icon keeps it off the name.
const NERD_ICONS: [char; 5] = ['\u{F108}', '\u{F07B}', '\u{F120}', '\u{F24D}', '\u{F096}'];

/// The columns the `▼`/`▶` marker and the space after it take.
const MARKER_WIDTH: u16 = 2;

/// The columns one level of the tree indents by.
const INDENT_WIDTH: u16 = 2;

/// One line of the `?` list. `keys` are the keys it stands for, which is what
/// lets a test hold this table to what `on_key` handles.
struct Shortcut {
    keys: &'static [KeyCode],
    shown: &'static str,
    does: &'static str,
}

const SHORTCUTS: &[Shortcut] = &[
    Shortcut {
        keys: &[KeyCode::Char('j'), KeyCode::Down],
        shown: "j/Down",
        does: "Next row",
    },
    Shortcut {
        keys: &[KeyCode::Char('k'), KeyCode::Up],
        shown: "k/Up",
        does: "Previous row",
    },
    Shortcut {
        keys: &[KeyCode::Char('g')],
        shown: "g",
        does: "First row",
    },
    Shortcut {
        keys: &[KeyCode::Char('G')],
        shown: "G",
        does: "Last row",
    },
    Shortcut {
        keys: &[KeyCode::Enter],
        shown: "Enter",
        does: "Switch to row",
    },
    Shortcut {
        keys: &[KeyCode::Char(' ')],
        shown: "Space",
        does: "Expand or collapse",
    },
    Shortcut {
        keys: &[KeyCode::Char('h'), KeyCode::Left],
        shown: "h/Left",
        does: "Collapse",
    },
    Shortcut {
        keys: &[KeyCode::Char('l'), KeyCode::Right],
        shown: "l/Right",
        does: "Expand",
    },
    Shortcut {
        keys: &[KeyCode::Char('d')],
        shown: "d",
        does: "Delete folder, session or tab; disconnect remote",
    },
    Shortcut {
        keys: &[KeyCode::Char('m')],
        shown: "m",
        does: "Move session or tab",
    },
    Shortcut {
        keys: &[KeyCode::Char('?')],
        shown: "?",
        does: "Show or hide this list",
    },
];

/// The client handles Esc before a panel sees a key.
const LEAVE_SHORTCUT: Shortcut = Shortcut {
    keys: &[],
    shown: "Esc",
    does: "Leave the panel",
};

/// What a confirmation says will close with `name`.
fn closes_under(name: &str, count: usize, noun: &str) -> String {
    match count {
        0 => format!("'{name}' is empty"),
        1 => format!("Closes everything under '{name}': 1 {noun}"),
        n => format!("Closes everything under '{name}': {n} {noun}s"),
    }
}

fn icon_of(node: &NodeType, style: IconStyle) -> Option<char> {
    let [server, folder, session, tab, pane] = match style {
        IconStyle::Emoji => EMOJI_ICONS,
        IconStyle::Nerd => NERD_ICONS,
        IconStyle::None => return None,
    };
    match node {
        NodeType::Server { .. } => Some(server),
        NodeType::Folder { .. } => Some(folder),
        NodeType::Session { .. } => Some(session),
        NodeType::Tab { .. } => Some(tab),
        NodeType::Pane { .. } => Some(pane),
        _ => None,
    }
}

/// Whether a node has children to show or hide, and so wears a marker.
fn is_expandable(node: &NodeType) -> bool {
    !matches!(
        node,
        NodeType::Pane { .. } | NodeType::DormantSession { .. }
    )
}

pub struct SessionsPlugin {
    /// Per-connection tree data, in arrival order. Order is stable across
    /// pushes so a refresh on one server does not reshuffle the list under the
    /// user's selection.
    trees: Vec<(ConnId, ConnTrees)>,
    model: TreeModel,
    icons: IconStyle,
    /// The tree index the last `render` started its window at.
    ///
    /// `render` takes `&self` and `on_mouse` is never told the panel's height,
    /// so the one place the scroll offset can be computed is the paint that
    /// produced the rows the user is clicking on. A `Cell` rather than a
    /// `&mut self` render: this is a record of what was drawn, not state the
    /// panel reasons with.
    last_top: Cell<usize>,
}

impl SessionsPlugin {
    pub fn new(icons: IconStyle) -> Self {
        Self {
            trees: Vec::new(),
            model: TreeModel::new(),
            icons,
            last_top: Cell::new(0),
        }
    }

    fn layout(&self) -> ListLayout {
        ListLayout::new(1)
    }

    /// Push `trees` into the model: roster first (it is what makes server rows
    /// exist), then each connection's slice.
    ///
    /// Local is pinned first so the list order is arrival order *below* a fixed
    /// head, rather than "whichever server answered the handshake first".
    fn refresh_model(&mut self) {
        let mut roster: Vec<(ConnId, String, RemoteState, Option<String>)> = Vec::new();
        let mut push = |id: &ConnId| {
            let label = match id {
                ConnId::Local => "local".to_string(),
                ConnId::Remote(name) => name.clone(),
            };
            roster.push((id.clone(), label, RemoteState::Connected, None));
        };
        for (id, _) in self.trees.iter().filter(|(id, _)| *id == ConnId::Local) {
            push(id);
        }
        for (id, _) in self.trees.iter().filter(|(id, _)| *id != ConnId::Local) {
            push(id);
        }
        self.model.set_roster(roster);
        for (id, t) in &self.trees {
            self.model.update_tree(
                id.clone(),
                t.folders.clone(),
                t.unfiled.clone(),
                // Dormant sessions are deliberately not rendered here; see the
                // module docs.
                Vec::new(),
            );
        }
    }

    /// The id of tab `tab_index` of `session` on `conn`, from the tree the panel
    /// holds.
    fn tab_id(&self, conn: &ConnId, session: &str, tab_index: usize) -> Option<u64> {
        let (_, trees) = self.trees.iter().find(|(id, _)| id == conn)?;
        trees
            .unfiled
            .iter()
            .chain(trees.folders.iter().flat_map(|f| f.sessions.iter()))
            .find(|s| s.name == session)?
            .tabs
            .get(tab_index)
            .map(|t| t.id)
    }

    /// How many sessions the panel holds in `folder` on `conn`.
    fn folder_session_count(&self, conn: &ConnId, folder: &str) -> usize {
        self.trees
            .iter()
            .find(|(id, _)| id == conn)
            .and_then(|(_, trees)| trees.folders.iter().find(|f| f.name == folder))
            .map_or(0, |f| f.sessions.len())
    }

    /// The row that deletes `folder`, for its menu and for `d`.
    fn delete_folder_item(&self, server: &ConnId, folder: &str) -> MenuItem {
        // The server kills a deleted folder's sessions, so the row says how
        // many go with it.
        let count = self.folder_session_count(server, folder);
        let label = match count {
            0 => "Delete folder".to_string(),
            1 => "Delete folder and 1 session".to_string(),
            n => format!("Delete folder and {n} sessions"),
        };
        MenuItem::new(
            label,
            MenuAction::DeleteFolder {
                conn: server.clone(),
                name: folder.to_string(),
            },
        )
        .confirm()
        .warning(closes_under(folder, count, "session"))
    }

    /// The row that kills `session`, for its menu and for `d`.
    fn kill_session_item(&self, server: &ConnId, session: &str) -> MenuItem {
        let tabs = self
            .model
            .sessions_for(server)
            .iter()
            .find(|s| s.name == session)
            .map_or(0, |s| s.tabs.len());
        MenuItem::new(
            "Kill session",
            MenuAction::KillSession {
                conn: server.clone(),
                name: session.to_string(),
            },
        )
        .confirm()
        .warning(closes_under(session, tabs, "tab"))
    }

    /// The panel row the selection is drawn on.
    fn selected_y(&self) -> u16 {
        self.layout()
            .row_of(self.model.selected.saturating_sub(self.last_top.get()))
    }

    fn open_menu(&self, items: Vec<MenuItem>, confirm: bool) -> PluginAction {
        PluginAction::OpenMenu {
            items,
            row: self.selected_y(),
            confirm,
        }
    }

    fn is_only_tab(&self, server: &ConnId, session: &str) -> bool {
        self.model
            .sessions_for(server)
            .iter()
            .any(|s| s.name == session && s.tabs.len() == 1)
    }

    /// `d`: a folder or a session asks first, because the rest of the tree goes
    /// with it, and so does a session's only tab, because closing it ends the
    /// session. Any other tab closes at once. A remote server disconnects at
    /// once, because its sessions keep running and it can be reconnected. Other
    /// rows have nothing to delete.
    fn delete_selected(&self) -> PluginAction {
        let Some(row) = self.model.selected_row() else {
            return PluginAction::None;
        };
        match &row.node_type {
            NodeType::Server {
                id: ConnId::Remote(name),
                ..
            } => PluginAction::RunMenuAction(MenuAction::DisconnectRemote(name.clone())),
            NodeType::Folder { server, name } => {
                self.open_menu(vec![self.delete_folder_item(server, name)], true)
            }
            NodeType::Session { server, name } => {
                self.open_menu(vec![self.kill_session_item(server, name)], true)
            }
            NodeType::Tab {
                server,
                session,
                tab_index,
            } if self.is_only_tab(server, session) => {
                self.open_menu(vec![self.kill_session_item(server, session)], true)
            }
            NodeType::Tab {
                server,
                session,
                tab_index,
            } => match self.tab_id(server, session, *tab_index) {
                Some(tab_id) => PluginAction::RunMenuAction(MenuAction::CloseTab {
                    conn: server.clone(),
                    session: session.clone(),
                    tab_id,
                }),
                None => PluginAction::None,
            },
            _ => PluginAction::None,
        }
    }

    /// `m`: pick where a session or a tab goes. Other rows do not move.
    fn move_selected(&self) -> PluginAction {
        let items = match self.model.selected_row().map(|r| &r.node_type) {
            Some(NodeType::Session { server, name }) => {
                context_menu::move_session_items(&self.model, server, name)
            }
            Some(NodeType::Tab {
                server,
                session,
                tab_index,
            }) => context_menu::move_tab_items(&self.model, server, session, *tab_index),
            _ => return PluginAction::None,
        };
        self.open_menu(items, false)
    }

    fn show_shortcuts(&self) -> PluginAction {
        let items = SHORTCUTS
            .iter()
            .chain(std::iter::once(&LEAVE_SHORTCUT))
            .map(|s| MenuItem::new(s.does, MenuAction::Cancel).hint(Some(s.shown.to_string())))
            .collect();
        self.open_menu(items, false)
    }

    /// Whether the selected node has children to show or hide.
    fn selected_is_expandable(&self) -> bool {
        self.model
            .selected_row()
            .is_some_and(|r| is_expandable(&r.node_type))
    }

    /// Whether panel column `x` is on the marker of tree row `index`.
    fn is_on_marker(&self, x: u16, index: usize) -> bool {
        self.model.rows.get(index).is_some_and(|row| {
            let start = self.layout().pad() + INDENT_WIDTH * row.indent as u16;
            is_expandable(&row.node_type) && (start..start + MARKER_WIDTH).contains(&x)
        })
    }

    /// Enter / a second click: toggle an expandable node, jump from a leaf.
    ///
    /// A session and a tab are BOTH: they expand and they are jump targets. The
    /// spec settles the tie -- "Enter on any node jumps" -- so Enter jumps and
    /// `Space`/`h`/`l` and a click on the marker do the expanding.
    fn activate(&mut self) -> PluginAction {
        match self.model.jump_target() {
            Some(target) => PluginAction::JumpTo(target),
            None => {
                self.model.toggle_expand();
                PluginAction::Redraw
            }
        }
    }
}

impl Default for SessionsPlugin {
    fn default() -> Self {
        Self::new(IconStyle::default())
    }
}

impl SidebarPlugin for SessionsPlugin {
    fn title(&self) -> &str {
        "Sessions"
    }

    fn min_size(&self) -> (u16, u16) {
        (12, self.layout().header_rows as u16 + 2)
    }

    fn wants_session_tree(&self) -> bool {
        true
    }

    fn render(
        &self,
        cols: u16,
        rows: u16,
        focused: bool,
        theme: &CompositorTheme,
    ) -> Vec<Vec<RenderCell>> {
        let bg = theme.frame_bg.clone().unwrap_or(CellColor::Default);
        let mut grid = blank_grid(cols, rows, bg.clone());
        if grid.is_empty() {
            return grid;
        }
        let layout = self.layout();
        nav::draw_header(&mut grid, self.title(), focused, theme);

        let top = nav::scroll_offset_in(layout, self.model.selected, rows);
        self.last_top.set(top);
        for i in 0..layout.visible(rows) {
            let Some(row) = self.model.rows.get(top + i) else {
                break;
            };
            let y = layout.row_of(i);
            let selected = top + i == self.model.selected;
            let (fg, row_bg, bold) = nav::row_colors(theme, focused, selected, &bg);
            if selected {
                nav::fill_row(&mut grid, y, cols, &fg, &row_bg);
            }
            // The same ▼/▶ the session-manager overlay draws, so one tree
            // reads the same on both surfaces. An expandable node with nothing
            // under it still carries a marker.
            let marker = match &row.node_type {
                node if !is_expandable(node) => "  ",
                _ if row.is_expanded => "\u{25BC} ",
                _ => "\u{25B6} ",
            };
            let icon = icon_of(&row.node_type, self.icons)
                .map(|icon| format!("{icon} "))
                .unwrap_or_default();
            let current = if row.is_current { "* " } else { "" };
            let text = format!(
                "{}{}{}{}{}",
                " ".repeat(usize::from(INDENT_WIDTH) * row.indent),
                marker,
                icon,
                current,
                row.display_name
            );
            draw_text_with(&mut grid, layout.pad(), y, &text, fg, row_bg, bold);
        }
        grid
    }

    fn on_key(&mut self, key: KeyEvent) -> PluginAction {
        // The tree's OWN keys first -- only a tree can expand, so these have no
        // place in the shared vocabulary and are matched before it.
        match key.code {
            // `h`/`l` collapse and expand rather than toggling, so holding a
            // direction cannot flap a node open and shut.
            KeyCode::Char('h') | KeyCode::Left => {
                self.model.collapse_selected();
                return PluginAction::Redraw;
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.model.expand_selected();
                return PluginAction::Redraw;
            }
            KeyCode::Char(' ') => {
                if self.selected_is_expandable() {
                    self.model.toggle_expand();
                }
                return PluginAction::Redraw;
            }
            // A chord such as Ctrl-d reaches the panel too, and must not delete.
            KeyCode::Char('d' | 'm' | '?')
                if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() =>
            {
                return PluginAction::None;
            }
            KeyCode::Char('d') => return self.delete_selected(),
            KeyCode::Char('m') => return self.move_selected(),
            KeyCode::Char('?') => return self.show_shortcuts(),
            _ => {}
        }
        match nav::nav_key(&key) {
            Some(NavKey::Activate) => self.activate(),
            Some(cmd) => {
                // Through the model's OWN cursor rather than a second one.
                // `TreeModel` already reconciles it -- by identity, on every
                // rebuild -- so a `NavList` beside it would be a second
                // reconciler for the same selection, and the two would disagree
                // the first time a push rebuilt the tree. See `nav`'s docs.
                nav::move_selection(&mut self.model.selected, self.model.rows.len(), cmd);
                PluginAction::Redraw
            }
            None => PluginAction::None,
        }
    }

    fn on_mouse(&mut self, x: u16, y: u16, kind: MouseEventKind) -> PluginAction {
        if !nav::is_select_click(kind) {
            return PluginAction::None;
        }
        let hit = nav::hit_test_in(
            self.layout(),
            y,
            self.last_top.get(),
            self.model.selected,
            self.model.rows.len(),
        );
        // The marker is decided before `action_for_hit`, so a press on it never
        // counts as the second click that jumps from a session or a tab.
        if let nav::Hit::Select(index) | nav::Hit::Activate(index) = hit {
            if self.is_on_marker(x, index) {
                self.model.selected = index;
                self.model.toggle_expand();
                return PluginAction::Redraw;
            }
        }
        // The free function, not `NavList::click`: this panel's cursor lives in
        // its `TreeModel`, which re-points itself by identity on every refresh,
        // so a `NavList` here would be a second reconciler on one cursor.
        match nav::action_for_hit(hit, &mut self.model.selected) {
            nav::HitOutcome::Ignore => PluginAction::None,
            nav::HitOutcome::Moved => PluginAction::Redraw,
            nav::HitOutcome::Activate => self.activate(),
        }
    }

    fn context_menu(&mut self, _x: u16, y: u16) -> Vec<MenuItem> {
        let hit = nav::hit_test_in(
            self.layout(),
            y,
            self.last_top.get(),
            self.model.selected,
            self.model.rows.len(),
        );
        if nav::pick_row(hit, &mut self.model.selected).is_none() {
            return Vec::new();
        }
        let Some(row) = self.model.selected_row() else {
            return Vec::new();
        };
        match row.node_type.clone() {
            NodeType::Server {
                id: ConnId::Remote(name),
                ..
            } => vec![
                MenuItem::new("Disconnect", MenuAction::DisconnectRemote(name))
                    .hint(Some("d".into())),
            ],
            NodeType::Folder { server, name } => {
                let delete = self.delete_folder_item(&server, &name);
                vec![
                    MenuItem::new(
                        "Rename",
                        MenuAction::Rename(RenameFor::Folder { conn: server, name }),
                    ),
                    delete,
                ]
            }
            NodeType::Session { server, name } => {
                let target = JumpTarget::Session {
                    conn: server.clone(),
                    session: name.clone(),
                };
                vec![
                    MenuItem::new("Switch", MenuAction::Jump(target)).hint(Some("Enter".into())),
                    MenuItem::new(
                        "Rename",
                        MenuAction::Rename(RenameFor::Session {
                            conn: server.clone(),
                            name: name.clone(),
                        }),
                    ),
                    self.kill_session_item(&server, &name),
                ]
            }
            NodeType::Tab {
                server,
                session,
                tab_index,
            } => {
                let target = JumpTarget::Tab {
                    conn: server.clone(),
                    session: session.clone(),
                    tab_index,
                };
                let mut items =
                    vec![MenuItem::new("Switch", MenuAction::Jump(target))
                        .hint(Some("Enter".into()))];
                // The id is read now, so a tab moved before a row is chosen is
                // still the one renamed or closed.
                if let Some(tab_id) = self.tab_id(&server, &session, tab_index) {
                    items.push(MenuItem::new(
                        "Rename",
                        MenuAction::Rename(RenameFor::SessionTab {
                            conn: server.clone(),
                            session: session.clone(),
                            tab_id,
                        }),
                    ));
                    items.push(
                        MenuItem::new(
                            "Close tab",
                            MenuAction::CloseTab {
                                conn: server,
                                session,
                                tab_id,
                            },
                        )
                        .confirm(),
                    );
                }
                items
            }
            NodeType::Pane {
                server,
                session,
                tab_index,
                pane_id,
            } => {
                let target = JumpTarget::Pane {
                    conn: server.clone(),
                    session: session.clone(),
                    tab_index,
                    pane_id,
                };
                vec![
                    MenuItem::new("Switch", MenuAction::Jump(target)).hint(Some("Enter".into())),
                    MenuItem::new(
                        "Close pane",
                        MenuAction::ClosePane {
                            conn: server,
                            session,
                            pane_id,
                        },
                    )
                    .confirm(),
                ]
            }
            _ => Vec::new(),
        }
    }

    fn on_event(&mut self, ev: &PluginEvent) {
        match ev {
            PluginEvent::SessionTree {
                conn,
                folders,
                unfiled,
                dormant,
            } => {
                let trees = ConnTrees {
                    folders: folders.clone(),
                    unfiled: unfiled.clone(),
                    dormant: dormant.clone(),
                };
                match self.trees.iter_mut().find(|(id, _)| id == conn) {
                    // Replace in place: the list order is what the panel
                    // renders, and a refresh must not move a server.
                    Some(slot) => slot.1 = trees,
                    None => {
                        self.trees.push((conn.clone(), trees));
                        // A server's own node is not auto-expanded by
                        // `update_tree` -- only `Local` starts open -- so a
                        // newly seen remote would arrive as one collapsed row.
                        // Done once, on first sight, so a later push cannot
                        // re-open a server the user collapsed.
                        self.model.force_expand_server(conn);
                    }
                }
                self.refresh_model();
            }
            PluginEvent::ConnectionLost { conn } => {
                let before = self.trees.len();
                self.trees.retain(|(id, _)| id != conn);
                if self.trees.len() != before {
                    self.refresh_model();
                }
            }
            // The agent list is the `agents` panel's and the directory events
            // are the `files` panel's; the pre-resolved focused cwd and focused
            // pane are both already in the tree this panel holds, which is why
            // it derives its own highlight rather than following either.
            PluginEvent::Agents { .. }
            | PluginEvent::FocusedCwd { .. }
            | PluginEvent::FocusedPane { .. }
            | PluginEvent::DirectoryListing { .. }
            | PluginEvent::PaneTitle { .. }
            | PluginEvent::Views { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tree_model::JumpTarget;
    use crate::protocol::{
        FolderTreeEntry, PaneTreeEntry, RemuxCommand, SessionTreeEntry, TabTreeEntry,
    };
    use crossterm::event::MouseButton;
    use crossterm::event::{KeyEventKind, KeyModifiers};

    fn remote(name: &str) -> ConnId {
        ConnId::Remote(name.to_string())
    }

    fn pane(id: u64, name: &str) -> PaneTreeEntry {
        PaneTreeEntry {
            cwd: None,
            id,
            name: name.to_string(),
            is_focused: false,
        }
    }

    fn tab(id: u64, name: &str, panes: Vec<PaneTreeEntry>) -> TabTreeEntry {
        TabTreeEntry {
            is_active: true,
            id,
            name: name.to_string(),
            panes,
        }
    }

    fn session(name: &str, tabs: Vec<TabTreeEntry>) -> SessionTreeEntry {
        SessionTreeEntry {
            name: name.to_string(),
            tabs,
            is_current: false,
            client_count: 0,
        }
    }

    fn tree(conn: ConnId, unfiled: Vec<SessionTreeEntry>) -> PluginEvent {
        PluginEvent::SessionTree {
            conn,
            folders: Vec::new(),
            unfiled,
            dormant: Vec::new(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    /// The panel's visible rows, as `(indent-less) text`, from a real render.
    fn painted(p: &SessionsPlugin, cols: u16, rows: u16) -> Vec<String> {
        let theme = CompositorTheme::default();
        p.render(cols, rows, true, &theme)
            .into_iter()
            .map(|r| {
                r.iter()
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn local_tree() -> PluginEvent {
        tree(
            ConnId::Local,
            vec![session(
                "alpha",
                vec![tab(1, "editor", vec![pane(10, "sh"), pane(11, "top")])],
            )],
        )
    }

    fn remote_tree() -> PluginEvent {
        tree(
            remote("gpu"),
            vec![session(
                "beta",
                vec![tab(2, "shell", vec![pane(20, "zsh")])],
            )],
        )
    }

    /// Select the row whose label ends with `name`.
    fn select(p: &mut SessionsPlugin, name: &str) {
        let idx = p
            .model
            .rows
            .iter()
            .position(|r| r.display_name == name)
            .unwrap_or_else(|| {
                panic!(
                    "no row {name:?} in {:?}",
                    p.model
                        .rows
                        .iter()
                        .map(|r| r.display_name.clone())
                        .collect::<Vec<_>>()
                )
            });
        p.model.selected = idx;
    }

    #[test]
    fn registry_resolves_the_sessions_plugin() {
        assert!(
            crate::client::sidebar::make_plugin(&crate::config::sidebar::PanelConfig::named(
                "sessions"
            ))
            .is_some()
        );
    }

    #[test]
    fn a_local_tree_populates_the_model() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let rows = painted(&p, 24, 8);
        assert_eq!(rows[0], " sessions");
        assert!(
            rows.iter().any(|r| r.contains("local")),
            "no local server row: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("alpha")),
            "no session row: {rows:?}"
        );
        // The session auto-expanded on first load, so its tab shows too.
        assert!(
            rows.iter().any(|r| r.contains("editor")),
            "no tab row: {rows:?}"
        );
    }

    #[test]
    fn a_second_connections_tree_adds_a_subtree_without_dropping_the_first() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        p.on_event(&remote_tree());
        let rows = painted(&p, 24, 12);
        for want in ["local", "alpha", "gpu", "beta"] {
            assert!(
                rows.iter().any(|r| r.contains(want)),
                "{want:?} missing: {rows:?}"
            );
        }
    }

    #[test]
    fn a_newly_seen_remote_is_expanded_so_its_sessions_show() {
        // `TreeModel` opens only the LOCAL server node by itself, so without
        // this the remote would arrive as one collapsed row.
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&remote_tree());
        let rows = painted(&p, 24, 8);
        assert!(
            rows.iter().any(|r| r.contains("beta")),
            "the remote's sessions stayed hidden: {rows:?}"
        );
    }

    #[test]
    fn connection_lost_drops_only_that_connections_subtree() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        p.on_event(&remote_tree());
        p.on_event(&PluginEvent::ConnectionLost {
            conn: remote("gpu"),
        });
        let rows = painted(&p, 24, 12);
        assert!(
            rows.iter().any(|r| r.contains("alpha")),
            "the local subtree went with it: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("gpu") || r.contains("beta")),
            "the dropped connection is still listed: {rows:?}"
        );
    }

    #[test]
    fn a_refresh_does_not_move_a_server_in_the_list() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        p.on_event(&remote_tree());
        let before = painted(&p, 24, 12);
        // The remote pushes again; the local row must stay above it.
        p.on_event(&remote_tree());
        assert_eq!(painted(&p, 24, 12), before);
    }

    #[test]
    fn j_and_k_move_the_selection_and_ask_for_a_redraw() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let start = p.model.selected;
        assert_eq!(p.on_key(key(KeyCode::Char('j'))), PluginAction::Redraw);
        assert_ne!(p.model.selected, start, "j did not move the selection");
        assert_eq!(p.on_key(key(KeyCode::Char('k'))), PluginAction::Redraw);
        assert_eq!(p.model.selected, start, "k did not come back");
        // Arrows are the same movement.
        assert_eq!(p.on_key(key(KeyCode::Down)), PluginAction::Redraw);
        assert_ne!(p.model.selected, start);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        p.on_key(key(KeyCode::Char('G')));
        assert_eq!(p.model.selected, p.model.rows.len() - 1);
        p.on_key(key(KeyCode::Char('g')));
        assert_eq!(p.model.selected, 0);
    }

    #[test]
    fn enter_on_a_pane_row_jumps_to_that_pane() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l'))); // expand the tab
        select(&mut p, "top");
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            PluginAction::JumpTo(JumpTarget::Pane {
                conn: ConnId::Local,
                session: "alpha".to_string(),
                tab_index: 0,
                pane_id: 11,
            })
        );
    }

    #[test]
    fn enter_on_a_session_or_tab_row_jumps_rather_than_expanding() {
        // The spec's "Enter on any node jumps": above pane level the target is
        // the node, and the server resolves its current focus.
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());

        select(&mut p, "alpha");
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            PluginAction::JumpTo(JumpTarget::Session {
                conn: ConnId::Local,
                session: "alpha".to_string(),
            })
        );
        select(&mut p, "editor");
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            PluginAction::JumpTo(JumpTarget::Tab {
                conn: ConnId::Local,
                session: "alpha".to_string(),
                tab_index: 0,
            })
        );
    }

    #[test]
    fn enter_on_a_server_row_expands_rather_than_jumping() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "local");
        assert_eq!(p.on_key(key(KeyCode::Enter)), PluginAction::Redraw);
        let rows = painted(&p, 24, 12);
        assert!(
            !rows.iter().any(|r| r.contains("alpha")),
            "Enter on the server row did not collapse it: {rows:?}"
        );
    }

    #[test]
    fn space_expands_a_session_instead_of_jumping() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "alpha");
        assert_eq!(p.on_key(key(KeyCode::Char(' '))), PluginAction::Redraw);
        let rows = painted(&p, 24, 12);
        assert!(
            !rows.iter().any(|r| r.contains("editor")),
            "Space did not collapse the session: {rows:?}"
        );
    }

    #[test]
    fn h_collapses_and_l_expands_without_flapping() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "alpha");
        p.on_key(key(KeyCode::Char('h')));
        p.on_key(key(KeyCode::Char('h')));
        assert!(
            !painted(&p, 24, 12).iter().any(|r| r.contains("editor")),
            "a second h re-opened the node"
        );
        p.on_key(key(KeyCode::Char('l')));
        p.on_key(key(KeyCode::Char('l')));
        assert!(
            painted(&p, 24, 12).iter().any(|r| r.contains("editor")),
            "a second l re-closed the node"
        );
    }

    #[test]
    fn the_expansion_marker_tracks_the_node_state() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let open = painted(&p, 24, 12);
        assert!(
            open.iter()
                .any(|r| r.starts_with(&format!(" \u{25BC} {}  local", EMOJI_ICONS[0]))),
            "an expanded server is not marked open: {open:?}"
        );
        select(&mut p, "local");
        p.on_key(key(KeyCode::Char('h')));
        let shut = painted(&p, 24, 12);
        assert!(
            shut.iter()
                .any(|r| r.starts_with(&format!(" \u{25B6} {}  local", EMOJI_ICONS[0]))),
            "a collapsed server is not marked shut: {shut:?}"
        );
    }

    #[test]
    fn a_pane_row_is_indented_under_its_tab() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l')));
        let rows = painted(&p, 24, 12);
        let server = rows.iter().find(|r| r.contains("local")).unwrap();
        let pane_row = rows.iter().find(|r| r.contains("top")).unwrap();
        let lead = |s: &str| s.len() - s.trim_start().len();
        assert!(
            lead(pane_row) > lead(server),
            "the pane is not indented under its server: {rows:?}"
        );
    }

    #[test]
    fn the_selection_stays_visible_when_it_runs_past_the_panel() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l')));
        p.on_key(key(KeyCode::Char('G')));
        let last = p.model.rows.last().unwrap().display_name.clone();
        // Three rows: a header and two tree rows -- far fewer than the tree.
        let rows = painted(&p, 24, 3);
        assert!(
            rows.iter().any(|r| r.contains(&last)),
            "the selection scrolled off the panel: {rows:?}"
        );
    }

    #[test]
    fn render_returns_exactly_the_requested_dimensions() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let theme = CompositorTheme::default();
        let grid = p.render(14, 5, false, &theme);
        assert_eq!(grid.len(), 5);
        assert!(grid.iter().all(|r| r.len() == 14));
        assert!(p.render(0, 0, false, &theme).is_empty());
    }

    #[test]
    fn the_selected_row_is_highlighted_only_while_focused() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let theme = CompositorTheme::default();
        let focused = p.render(24, 6, true, &theme);
        let idle = p.render(24, 6, false, &theme);
        assert_eq!(focused[2][0].bg, theme.sidebar_selection_bg);
        assert_eq!(focused[2][1].fg, theme.sidebar_name_selected_fg);
        assert!(focused[2][1].bold);
        // Unfocused, the band stays and the name loses its bright colour.
        assert_eq!(idle[2][0].bg, theme.sidebar_selection_bg);
        assert_eq!(idle[2][1].fg, theme.status_bar_fg);
        assert_ne!(
            focused[3][0].bg, theme.sidebar_selection_bg,
            "an unselected row is highlighted too"
        );
    }

    #[test]
    fn a_click_selects_a_row_and_a_second_click_activates_it() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        // Paint first: the click's row is resolved against what was drawn.
        let _ = painted(&p, 24, 12);
        let alpha = p
            .model
            .rows
            .iter()
            .position(|r| r.display_name == "alpha")
            .unwrap();
        let y = (alpha + LIST_HEADER_ROWS) as u16;
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(p.on_mouse(0, y, down), PluginAction::Redraw);
        assert_eq!(p.model.selected, alpha);
        assert_eq!(
            p.on_mouse(0, y, down),
            PluginAction::JumpTo(JumpTarget::Session {
                conn: ConnId::Local,
                session: "alpha".to_string(),
            })
        );
    }

    fn menu_at(p: &mut SessionsPlugin, label: &str) -> Vec<MenuItem> {
        let idx = p
            .model
            .rows
            .iter()
            .position(|r| r.display_name == label)
            .unwrap();
        p.context_menu(0, (idx + LIST_HEADER_ROWS) as u16)
    }

    #[test]
    fn a_session_row_offers_switch_rename_and_a_confirmed_kill() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let _ = painted(&p, 24, 12);
        let items = menu_at(&mut p, "alpha");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Switch", "Rename", "Kill session"]);
        let target = JumpTarget::Session {
            conn: ConnId::Local,
            session: "alpha".to_string(),
        };
        assert_eq!(items[0].action, MenuAction::Jump(target));
        assert_eq!(
            items[1].action,
            MenuAction::Rename(RenameFor::Session {
                conn: ConnId::Local,
                name: "alpha".to_string()
            })
        );
        assert_eq!(
            items[2].action,
            MenuAction::KillSession {
                conn: ConnId::Local,
                name: "alpha".to_string()
            }
        );
        assert!(items[2].confirm && !items[0].confirm);
        assert_eq!(p.model.selected_row().unwrap().display_name, "alpha");
    }

    #[test]
    fn a_tab_row_offers_switch_rename_and_a_confirmed_close() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let _ = painted(&p, 24, 12);
        let items = menu_at(&mut p, "editor");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Switch", "Rename", "Close tab"]);
        assert!(matches!(
            items[2].action,
            MenuAction::CloseTab { tab_id: 1, .. }
        ));
        assert_eq!(
            items[1].action,
            MenuAction::Rename(RenameFor::SessionTab {
                conn: ConnId::Local,
                session: "alpha".to_string(),
                tab_id: 1
            })
        );
        assert!(items[2].confirm);
    }

    fn folder_tree(sessions: Vec<SessionTreeEntry>) -> PluginEvent {
        PluginEvent::SessionTree {
            conn: remote("gpu"),
            folders: vec![FolderTreeEntry {
                name: "work".to_string(),
                sessions,
            }],
            unfiled: Vec::new(),
            dormant: Vec::new(),
        }
    }

    #[test]
    fn a_folder_row_offers_rename_and_a_confirmed_delete_on_its_own_connection() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        p.on_event(&folder_tree(vec![
            session("a", vec![]),
            session("b", vec![]),
        ]));
        let _ = painted(&p, 40, 20);
        let items = menu_at(&mut p, "work");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Rename", "Delete folder and 2 sessions"]);
        assert_eq!(
            items[0].action,
            MenuAction::Rename(RenameFor::Folder {
                conn: remote("gpu"),
                name: "work".to_string()
            })
        );
        assert_eq!(
            items[1].action,
            MenuAction::DeleteFolder {
                conn: remote("gpu"),
                name: "work".to_string()
            }
        );
        assert!(items[1].confirm && !items[0].confirm);
    }

    #[test]
    fn a_folder_delete_counts_only_the_sessions_it_takes() {
        for (sessions, label) in [
            (vec![], "Delete folder"),
            (vec![session("a", vec![])], "Delete folder and 1 session"),
        ] {
            let mut p = SessionsPlugin::new(IconStyle::Emoji);
            p.on_event(&folder_tree(sessions));
            let _ = painted(&p, 40, 20);
            assert_eq!(menu_at(&mut p, "work")[1].label, label);
        }
    }

    #[test]
    fn a_folder_rename_goes_to_the_folders_connection() {
        let rename = RenameFor::Folder {
            conn: remote("gpu"),
            name: "work".to_string(),
        };
        assert_eq!(rename.label(), "Folder");
        assert_eq!(
            rename.command("play".to_string()),
            (
                Some(remote("gpu")),
                RemuxCommand::FolderRename {
                    old: "work".to_string(),
                    new: "play".to_string()
                }
            )
        );
    }

    #[test]
    fn a_pane_row_offers_switch_and_a_confirmed_close_but_no_rename() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l')));
        let _ = painted(&p, 24, 12);
        let items = menu_at(&mut p, "top");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Switch", "Close pane"]);
        assert_eq!(
            items[0].action,
            MenuAction::Jump(JumpTarget::Pane {
                conn: ConnId::Local,
                session: "alpha".to_string(),
                tab_index: 0,
                pane_id: 11,
            })
        );
        assert_eq!(
            items[1].action,
            MenuAction::ClosePane {
                conn: ConnId::Local,
                session: "alpha".to_string(),
                pane_id: 11,
            }
        );
        assert!(items[1].confirm && !items[0].confirm);
    }

    /// The `(x, y)` of the marker on `name`'s row, found on the painted grid.
    fn marker_cell(p: &SessionsPlugin, name: &str) -> (u16, u16) {
        let rows = painted(p, 40, 20);
        let y = rows
            .iter()
            .position(|r| r.contains(name))
            .unwrap_or_else(|| panic!("no row {name:?} in {rows:?}"));
        let x = rows[y]
            .chars()
            .position(|c| c == '\u{25B6}' || c == '\u{25BC}')
            .unwrap_or_else(|| panic!("no marker on {:?}", rows[y]));
        (x as u16, y as u16)
    }

    fn shows(p: &SessionsPlugin, name: &str) -> bool {
        painted(p, 40, 20).iter().any(|r| r.contains(name))
    }

    #[test]
    fn a_click_on_either_marker_column_toggles_and_selects_without_jumping() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let down = MouseEventKind::Down(MouseButton::Left);
        let (x, y) = marker_cell(&p, "editor");

        assert_eq!(p.on_mouse(x, y, down), PluginAction::Redraw);
        assert!(shows(&p, "top"), "the first click did not open the tab");
        assert_eq!(p.model.selected_row().unwrap().display_name, "editor");

        assert_eq!(p.on_mouse(x + 1, y, down), PluginAction::Redraw);
        assert!(!shows(&p, "top"), "the second click did not close the tab");
    }

    #[test]
    fn a_click_just_past_the_marker_is_a_click_on_the_name() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let down = MouseEventKind::Down(MouseButton::Left);
        let (x, y) = marker_cell(&p, "editor");
        assert_eq!(p.on_mouse(x + 2, y, down), PluginAction::Redraw);
        assert!(!shows(&p, "top"));
        assert!(matches!(
            p.on_mouse(x + 2, y, down),
            PluginAction::JumpTo(JumpTarget::Tab { .. })
        ));
        assert!(!shows(&p, "top"));
    }

    #[test]
    fn a_click_left_of_the_marker_is_not_a_click_on_it() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let down = MouseEventKind::Down(MouseButton::Left);
        let (x, y) = marker_cell(&p, "editor");
        p.on_mouse(x - 1, y, down);
        assert!(!shows(&p, "top"));
    }

    #[test]
    fn a_pane_row_has_no_marker_to_click() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l')));
        let rows = painted(&p, 40, 20);
        let y = rows.iter().position(|r| r.contains("top")).unwrap() as u16;
        let (tab_x, _) = marker_cell(&p, "editor");
        let down = MouseEventKind::Down(MouseButton::Left);
        // Where a marker would sit: the row one level below the tab's.
        let x = tab_x + INDENT_WIDTH;
        assert_eq!(p.on_mouse(x, y, down), PluginAction::Redraw);
        assert!(matches!(
            p.on_mouse(x, y, down),
            PluginAction::JumpTo(JumpTarget::Pane { pane_id: 11, .. })
        ));
    }

    #[test]
    fn a_tab_toggled_by_click_keeps_its_state_across_a_tree_push() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let down = MouseEventKind::Down(MouseButton::Left);
        let (x, y) = marker_cell(&p, "editor");
        p.on_mouse(x, y, down);
        p.on_event(&local_tree());
        assert!(shows(&p, "top"), "a push closed the opened tab");
        let (x, y) = marker_cell(&p, "editor");
        p.on_mouse(x, y, down);
        p.on_event(&local_tree());
        assert!(!shows(&p, "top"), "a push reopened the closed tab");
    }

    #[test]
    fn a_collapsed_local_server_stays_collapsed_across_a_tree_push() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let down = MouseEventKind::Down(MouseButton::Left);
        let (x, y) = marker_cell(&p, "local");
        p.on_mouse(x, y, down);
        assert!(!shows(&p, "alpha"));
        p.on_event(&local_tree());
        assert!(!shows(&p, "alpha"), "a push reopened the local server");
    }

    fn labels_of(action: &PluginAction) -> Vec<String> {
        match action {
            PluginAction::OpenMenu { items, .. } => items.iter().map(|i| i.label.clone()).collect(),
            other => panic!("no menu: {other:?}"),
        }
    }

    #[test]
    fn d_on_a_folder_or_session_opens_a_named_confirmation_on_the_rows_connection() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        p.on_event(&folder_tree(vec![session("a", vec![tab(5, "t", vec![])])]));
        let _ = painted(&p, 40, 20);

        select(&mut p, "work");
        let PluginAction::OpenMenu {
            items,
            confirm: true,
            ..
        } = p.on_key(key(KeyCode::Char('d')))
        else {
            panic!("d on a folder opened no confirmation");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].warning.as_deref(),
            Some("Closes everything under 'work': 1 session")
        );
        assert_eq!(
            items[0].action,
            MenuAction::DeleteFolder {
                conn: remote("gpu"),
                name: "work".to_string()
            }
        );

        select(&mut p, "a");
        let PluginAction::OpenMenu {
            items,
            confirm: true,
            ..
        } = p.on_key(key(KeyCode::Char('d')))
        else {
            panic!("d on a session opened no confirmation");
        };
        assert_eq!(
            items[0].warning.as_deref(),
            Some("Closes everything under 'a': 1 tab")
        );
        assert_eq!(
            items[0].action,
            MenuAction::KillSession {
                conn: remote("gpu"),
                name: "a".to_string()
            }
        );
    }

    #[test]
    fn d_on_a_tab_with_a_sibling_closes_it_with_no_confirmation() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&tree(
            ConnId::Local,
            vec![session(
                "alpha",
                vec![tab(1, "editor", vec![]), tab(2, "logs", vec![])],
            )],
        ));
        select(&mut p, "editor");
        assert_eq!(
            p.on_key(key(KeyCode::Char('d'))),
            PluginAction::RunMenuAction(MenuAction::CloseTab {
                conn: ConnId::Local,
                session: "alpha".to_string(),
                tab_id: 1,
            })
        );
    }

    #[test]
    fn d_on_a_sessions_only_tab_asks_as_d_on_the_session_does() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "editor");
        let PluginAction::OpenMenu {
            items,
            confirm: true,
            ..
        } = p.on_key(key(KeyCode::Char('d')))
        else {
            panic!("d on the only tab did not ask");
        };
        assert_eq!(
            items[0].warning.as_deref(),
            Some("Closes everything under 'alpha': 1 tab")
        );
        assert_eq!(
            items[0].action,
            MenuAction::KillSession {
                conn: ConnId::Local,
                name: "alpha".to_string()
            }
        );
    }

    #[test]
    fn d_and_m_do_nothing_on_the_local_server_or_a_pane_row() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l')));
        for row in ["local", "top"] {
            select(&mut p, row);
            assert_eq!(p.on_key(key(KeyCode::Char('d'))), PluginAction::None);
            assert_eq!(p.on_key(key(KeyCode::Char('m'))), PluginAction::None);
        }
    }

    #[test]
    fn d_on_a_remote_server_row_disconnects_that_remote() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        p.on_event(&remote_tree());
        select(&mut p, "gpu");
        assert_eq!(
            p.on_key(key(KeyCode::Char('d'))),
            PluginAction::RunMenuAction(MenuAction::DisconnectRemote("gpu".to_string()))
        );
        assert_eq!(p.on_key(key(KeyCode::Char('m'))), PluginAction::None);
        let mut chord = key(KeyCode::Char('d'));
        chord.modifiers = KeyModifiers::CONTROL;
        assert_eq!(p.on_key(chord), PluginAction::None);
    }

    #[test]
    fn only_a_remote_server_row_offers_disconnect() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        p.on_event(&remote_tree());
        let _ = painted(&p, 24, 12);
        let items = menu_at(&mut p, "gpu");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "Disconnect");
        assert_eq!(
            items[0].action,
            MenuAction::DisconnectRemote("gpu".to_string())
        );
        assert!(menu_at(&mut p, "local").is_empty());
    }

    #[test]
    fn a_chord_on_d_or_m_is_not_the_key() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "editor");
        for c in ['d', 'm', '?'] {
            for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
                let mut chord = key(KeyCode::Char(c));
                chord.modifiers = modifiers;
                assert_eq!(p.on_key(chord), PluginAction::None, "{c} {modifiers:?}");
            }
        }
    }

    #[test]
    fn m_on_a_session_offers_every_other_folder_and_the_way_out() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&PluginEvent::SessionTree {
            conn: ConnId::Local,
            folders: vec![
                FolderTreeEntry {
                    name: "work".to_string(),
                    sessions: vec![session("a", vec![])],
                },
                FolderTreeEntry {
                    name: "play".to_string(),
                    sessions: vec![],
                },
            ],
            unfiled: vec![session("b", vec![])],
            dormant: Vec::new(),
        });
        select(&mut p, "a");
        assert_eq!(
            labels_of(&p.on_key(key(KeyCode::Char('m')))),
            ["Move session 'a' to:", "play", "(no folder)"]
        );
        select(&mut p, "b");
        let action = p.on_key(key(KeyCode::Char('m')));
        assert_eq!(labels_of(&action), ["Move session 'b' to:", "work", "play"]);
        let PluginAction::OpenMenu { items, .. } = action else {
            unreachable!()
        };
        assert_eq!(
            items[1].action,
            MenuAction::MoveSession {
                conn: ConnId::Local,
                session: "b".to_string(),
                folder: Some("work".to_string()),
            }
        );
    }

    #[test]
    fn m_says_so_when_there_is_nowhere_to_go() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "alpha");
        assert_eq!(
            labels_of(&p.on_key(key(KeyCode::Char('m')))),
            ["No folders to move to"]
        );
        select(&mut p, "editor");
        assert_eq!(
            labels_of(&p.on_key(key(KeyCode::Char('m')))),
            ["No other session on this server"]
        );
    }

    #[test]
    fn m_on_a_tab_offers_only_its_own_servers_other_sessions() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&tree(
            ConnId::Local,
            vec![
                session("alpha", vec![tab(1, "editor", vec![pane(10, "sh")])]),
                session("gamma", vec![]),
            ],
        ));
        p.on_event(&remote_tree());
        select(&mut p, "editor");
        let action = p.on_key(key(KeyCode::Char('m')));
        assert_eq!(labels_of(&action), ["Move tab 'editor' to:", "gamma"]);
        let PluginAction::OpenMenu { items, .. } = action else {
            unreachable!()
        };
        assert_eq!(
            items[1].action,
            MenuAction::MoveTab {
                conn: ConnId::Local,
                tab_id: 1,
                from: "alpha".to_string(),
                to: "gamma".to_string(),
                follows: false,
            }
        );
    }

    /// Every key `on_key` acts on, against the table `?` lists.
    #[test]
    fn the_shortcut_list_is_exactly_what_on_key_handles() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&tree(
            ConnId::Local,
            vec![
                session("alpha", vec![tab(1, "editor", vec![pane(10, "sh")])]),
                session("gamma", vec![]),
            ],
        ));
        let mut candidates: Vec<KeyCode> = (0x20u8..0x7f)
            .map(|b| KeyCode::Char(char::from(b)))
            .collect();
        candidates.extend([
            KeyCode::Enter,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Delete,
        ]);
        let handled: Vec<KeyCode> = candidates
            .into_iter()
            .filter(|&code| {
                select(&mut p, "alpha");
                p.on_key(key(code)) != PluginAction::None
            })
            .collect();
        let listed: Vec<KeyCode> = SHORTCUTS.iter().flat_map(|s| s.keys).copied().collect();
        for code in &handled {
            assert!(listed.contains(code), "{code:?} is handled but not listed");
        }
        for code in &listed {
            assert!(handled.contains(code), "{code:?} is listed but not handled");
        }
    }

    #[test]
    fn question_mark_lists_every_shortcut_with_its_key() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let PluginAction::OpenMenu { items, .. } = p.on_key(key(KeyCode::Char('?'))) else {
            panic!("? opened nothing");
        };
        assert_eq!(items.len(), SHORTCUTS.len() + 1);
        for (item, shortcut) in items.iter().zip(SHORTCUTS) {
            assert_eq!(item.label, shortcut.does);
            assert_eq!(item.hint.as_deref(), Some(shortcut.shown));
        }
        assert_eq!(items.last().unwrap().hint.as_deref(), Some("Esc"));
    }

    #[test]
    fn each_icon_set_has_the_width_its_style_expects_and_is_distinct() {
        use unicode_width::UnicodeWidthChar;
        for (icons, width) in [(EMOJI_ICONS, 2), (NERD_ICONS, 1)] {
            for icon in icons {
                assert_eq!(icon.width(), Some(width), "{icon:?}");
            }
            let distinct: std::collections::HashSet<char> = icons.into_iter().collect();
            assert_eq!(distinct.len(), icons.len());
        }
    }

    #[test]
    fn every_emoji_icon_defaults_to_emoji_presentation() {
        let emoji = regex::Regex::new(r"^\p{Emoji_Presentation}$").unwrap();
        for icon in EMOJI_ICONS {
            assert!(emoji.is_match(&icon.to_string()), "{icon:?}");
        }
    }

    #[test]
    fn a_server_row_wears_a_computer_under_each_style_but_none() {
        for (style, icon) in [
            (IconStyle::Emoji, format!("{}  ", EMOJI_ICONS[0])),
            (IconStyle::Nerd, format!("{} ", NERD_ICONS[0])),
            (IconStyle::None, String::new()),
        ] {
            let mut p = SessionsPlugin::new(style);
            p.on_event(&local_tree());
            p.on_event(&remote_tree());
            let rows = painted(&p, 40, 20);
            for server in ["local", "gpu"] {
                let want = format!(" \u{25BC} {icon}{server}");
                assert!(
                    rows.iter().any(|r| r.starts_with(&want)),
                    "{style:?} {server}: {rows:?}"
                );
            }
        }
    }

    #[test]
    fn a_marker_click_toggles_under_every_icon_style() {
        for style in [IconStyle::Emoji, IconStyle::Nerd, IconStyle::None] {
            let mut p = SessionsPlugin::new(style);
            p.on_event(&filed_tree());
            let (x, y) = marker_cell(&p, "editor");
            assert!(!shows(&p, " sh"), "{style:?}");
            let down = MouseEventKind::Down(crossterm::event::MouseButton::Left);
            p.on_mouse(x, y, down);
            assert!(
                shows(&p, " sh"),
                "{style:?}: the marker click opened nothing"
            );
        }
    }

    #[test]
    fn each_style_draws_its_own_icon_between_marker_and_name() {
        for (style, want) in [
            (IconStyle::Emoji, format!("{}  alpha", EMOJI_ICONS[2])),
            (IconStyle::Nerd, format!("{} alpha", NERD_ICONS[2])),
            (IconStyle::None, "alpha".to_string()),
        ] {
            let mut p = SessionsPlugin::new(style);
            p.on_event(&filed_tree());
            let rows = painted(&p, 40, 20);
            let want = format!("\u{25BC} {want}");
            assert!(
                rows.iter().any(|r| r.ends_with(&want)),
                "{style:?}: {rows:?}"
            );
        }
    }

    fn filed_tree() -> PluginEvent {
        PluginEvent::SessionTree {
            conn: ConnId::Local,
            folders: vec![FolderTreeEntry {
                name: "work".to_string(),
                sessions: vec![session(
                    "alpha",
                    vec![tab(1, "editor", vec![pane(10, "sh")])],
                )],
            }],
            unfiled: Vec::new(),
            dormant: Vec::new(),
        }
    }

    #[test]
    fn each_row_type_wears_its_icon_after_the_marker() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&filed_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l')));
        let rows = painted(&p, 40, 20);
        for (name, icon) in [
            ("work", EMOJI_ICONS[1]),
            ("alpha", EMOJI_ICONS[2]),
            ("editor", EMOJI_ICONS[3]),
        ] {
            let (x, y) = marker_cell(&p, name);
            let row: Vec<char> = rows[y as usize].chars().collect();
            assert_eq!(row[x as usize + 2], icon, "{name}: {row:?}");
        }
        let pane_row = rows.iter().find(|r| r.ends_with(" sh")).unwrap();
        assert!(pane_row.contains(EMOJI_ICONS[4]), "{pane_row:?}");
        let server = rows.iter().find(|r| r.contains("local")).unwrap();
        assert_eq!(server, &format!(" \u{25BC} {}  local", EMOJI_ICONS[0]));
    }

    #[test]
    fn a_current_row_reads_marker_icon_star_name() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        let mut current = session("alpha", Vec::new());
        current.is_current = true;
        p.on_event(&tree(ConnId::Local, vec![current]));
        let rows = painted(&p, 40, 20);
        let want = format!("   \u{25BC} {}  * alpha", EMOJI_ICONS[2]);
        assert!(rows.contains(&want), "{rows:?}");
    }

    #[test]
    fn a_narrow_panel_never_cuts_a_wide_glyph_in_half() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&filed_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l')));
        let theme = CompositorTheme::default();
        let min_cols = p.min_size().0;
        for cols in min_cols..min_cols + 4 {
            for selected in 0..p.model.rows.len() {
                p.model.selected = selected;
                let grid = p.render(cols, 12, true, &theme);
                for row in &grid {
                    assert_eq!(row.len(), usize::from(cols));
                    for (x, cell) in row.iter().enumerate() {
                        if cell.width == 2 {
                            assert_eq!(row[x + 1].width, 0, "no second half at {cols} cols");
                        }
                        if cell.width == 0 {
                            assert_eq!(row[x - 1].width, 2, "an orphan half at {cols} cols");
                        }
                    }
                }
                let band = &grid[selected + LIST_HEADER_ROWS];
                assert!(
                    band.iter().all(|c| c.bg == theme.sidebar_selection_bg),
                    "the band has a gap at {cols} cols"
                );
            }
        }
    }

    #[test]
    fn the_header_and_the_rows_below_the_list_have_no_menu() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let _ = painted(&p, 24, 12);
        assert!(p.context_menu(0, 0).is_empty());
        assert!(p.context_menu(0, 200).is_empty());
    }

    #[test]
    fn a_click_on_the_header_or_past_the_last_row_does_nothing() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let _ = painted(&p, 24, 12);
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(p.on_mouse(0, 0, down), PluginAction::None);
        assert_eq!(p.on_mouse(0, 200, down), PluginAction::None);
    }

    #[test]
    fn a_click_lands_on_the_row_under_the_cursor_after_scrolling() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l')));
        p.on_key(key(KeyCode::Char('G')));
        // A 4-row panel shows the two-row header plus the last two tree rows.
        let _ = painted(&p, 24, 4);
        let down = MouseEventKind::Down(MouseButton::Left);
        p.on_mouse(0, 2, down);
        assert_eq!(
            p.model.selected,
            p.model.rows.len() - 2,
            "the click was resolved against an unscrolled window"
        );
    }

    #[test]
    fn a_dormant_session_is_not_listed() {
        // The panel has no way to resurrect one, so an Enter on it would be a
        // visibly dead key.
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&PluginEvent::SessionTree {
            conn: ConnId::Local,
            folders: Vec::new(),
            unfiled: vec![session("alpha", Vec::new())],
            dormant: vec!["archived".to_string()],
        });
        let rows = painted(&p, 30, 12);
        assert!(rows.iter().any(|r| r.contains("alpha")));
        assert!(
            !rows.iter().any(|r| r.contains("archived")),
            "a dormant session is listed: {rows:?}"
        );
    }

    #[test]
    fn a_server_row_never_wears_a_connection_state_suffix() {
        // The panel synthesizes its roster from the trees it holds, so a node
        // is only ever listed while it is connected.
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&remote_tree());
        let rows = painted(&p, 40, 12);
        let want = format!("\u{25BC} {}  gpu", EMOJI_ICONS[0]);
        assert!(rows.iter().any(|r| r.trim() == want), "{rows:?}");
    }

    #[test]
    fn a_folder_the_user_collapsed_survives_a_tree_that_empties_and_refills() {
        // The panel refreshes on EVERY push, so this boundary is its problem in
        // a way it never was for an overlay opened on demand.
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        let full = PluginEvent::SessionTree {
            conn: ConnId::Local,
            folders: vec![FolderTreeEntry {
                name: "work".to_string(),
                sessions: vec![session(
                    "alpha",
                    vec![tab(1, "editor", vec![pane(10, "sh")])],
                )],
            }],
            unfiled: Vec::new(),
            dormant: Vec::new(),
        };
        p.on_event(&full);
        select(&mut p, "work");
        p.on_key(key(KeyCode::Char('h')));
        assert!(!painted(&p, 24, 12).iter().any(|r| r.contains("alpha")));

        p.on_event(&tree(ConnId::Local, Vec::new()));
        p.on_event(&full);
        let rows = painted(&p, 24, 12);
        assert!(rows.iter().any(|r| r.contains("work")));
        assert!(
            !rows.iter().any(|r| r.contains("alpha")),
            "the collapsed folder sprang back open: {rows:?}"
        );
    }

    #[test]
    fn only_the_sessions_plugin_asks_for_the_session_tree_push() {
        assert!(
            crate::client::sidebar::make_plugin(&crate::config::sidebar::PanelConfig::named(
                "sessions"
            ))
            .unwrap()
            .wants_session_tree()
        );
        assert!(
            !crate::client::sidebar::make_plugin(&crate::config::sidebar::PanelConfig::named(
                "placeholder"
            ))
            .unwrap()
            .wants_session_tree()
        );
    }

    #[test]
    fn the_tree_has_a_lowercase_header_a_blank_row_and_one_column_of_padding() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let out = painted(&p, 24, 8);
        assert_eq!(out[0], " sessions");
        assert_eq!(out[1], "");
        assert!(out[2].starts_with(" \u{25BC}"), "{out:?}");
    }

    #[test]
    fn the_band_spans_the_row_and_a_click_resolves_below_the_two_row_header() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let theme = CompositorTheme::default();
        let grid = p.render(24, 8, true, &theme);
        assert!(grid[2].iter().all(|c| c.bg == theme.sidebar_selection_bg));
        assert!(grid[3].iter().all(|c| c.bg != theme.sidebar_selection_bg));
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(p.on_mouse(0, 1, down), PluginAction::None, "the blank row");
        assert_eq!(p.on_mouse(0, 3, down), PluginAction::Redraw);
        assert_eq!(p.model.selected, 1);
    }

    #[test]
    fn the_header_takes_the_active_colour_only_while_the_panel_has_focus() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let theme = CompositorTheme::default();
        assert_eq!(
            p.render(24, 6, true, &theme)[0][1].fg,
            theme.sidebar_header_active_fg
        );
        assert_eq!(
            p.render(24, 6, false, &theme)[0][1].fg,
            theme.sidebar_dim_fg
        );
    }

    #[test]
    fn the_header_is_a_bar_whose_colours_follow_focus() {
        let mut p = SessionsPlugin::new(IconStyle::Emoji);
        p.on_event(&local_tree());
        let theme = CompositorTheme::default();
        for focused in [false, true] {
            let grid = p.render(24, 6, focused, &theme);
            nav::assert_header_bar(&grid, focused, &theme);
            assert!(
                grid[2].iter().all(|c| c.bg == theme.sidebar_selection_bg),
                "the selection band keeps its own colour (focused={focused})"
            );
        }
    }
}
