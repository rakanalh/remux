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

use crossterm::event::{KeyCode, KeyEvent, MouseEventKind};

use super::nav::{self, Hit, NavKey, HEADER_ROWS};
use super::{blank_grid, draw_text, PluginAction, PluginEvent, SidebarPlugin};
use crate::client::registry::{ConnId, RemoteState};
use crate::client::tree_model::{ConnTrees, NodeType, TreeModel};
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, RenderCell};

pub struct SessionsPlugin {
    /// Per-connection tree data, in arrival order. Order is stable across
    /// pushes so a refresh on one server does not reshuffle the list under the
    /// user's selection.
    trees: Vec<(ConnId, ConnTrees)>,
    model: TreeModel,
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
    pub fn new() -> Self {
        Self {
            trees: Vec::new(),
            model: TreeModel::new(),
            last_top: Cell::new(0),
        }
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

    /// Whether the selected node has children to show or hide.
    fn selected_is_expandable(&self) -> bool {
        !matches!(
            self.model.selected_row().map(|r| &r.node_type),
            None | Some(NodeType::Pane { .. } | NodeType::DormantSession { .. })
        )
    }

    /// Enter / a second click: toggle an expandable node, jump from a leaf.
    ///
    /// A session and a tab are BOTH: they expand and they are jump targets. The
    /// spec settles the tie -- "Enter on any node jumps" -- so Enter jumps and
    /// `Space`/`h`/`l` do the expanding.
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
        Self::new()
    }
}

impl SidebarPlugin for SessionsPlugin {
    fn title(&self) -> &str {
        "Sessions"
    }

    fn min_size(&self) -> (u16, u16) {
        (12, 3)
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
        nav::draw_header(&mut grid, self.title(), focused, theme, &bg);

        let top = nav::scroll_offset(self.model.selected, rows);
        self.last_top.set(top);
        for i in 0..(rows as usize).saturating_sub(HEADER_ROWS) {
            let Some(row) = self.model.rows.get(top + i) else {
                break;
            };
            let y = (HEADER_ROWS + i) as u16;
            let selected = top + i == self.model.selected;
            let (fg, row_bg) = nav::row_colors(theme, focused, selected, &bg);
            if selected {
                nav::fill_row(&mut grid, y, cols, &fg, &row_bg);
            }
            // The same ▼/▶ the session-manager overlay draws, so one tree
            // reads the same on both surfaces. An expandable node with nothing
            // under it still carries a marker.
            let marker = match &row.node_type {
                NodeType::Pane { .. } | NodeType::DormantSession { .. } => "  ".to_string(),
                _ if row.is_expanded => "\u{25BC} ".to_string(),
                _ => "\u{25B6} ".to_string(),
            };
            let current = if row.is_current { "* " } else { "" };
            let text = format!(
                "{}{}{}{}",
                "  ".repeat(row.indent),
                marker,
                current,
                row.display_name
            );
            draw_text(&mut grid, 0, y, &text, fg, row_bg);
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
            _ => {}
        }
        match nav::nav_key(&key) {
            Some(NavKey::Activate) => self.activate(),
            Some(cmd) => {
                // Through the model's own cursor rather than a second one: it
                // is shared with the session-manager overlay, and two owners of
                // one selection is how the two surfaces would drift apart.
                nav::move_selection(&mut self.model.selected, self.model.rows.len(), cmd);
                PluginAction::Redraw
            }
            None => PluginAction::None,
        }
    }

    fn on_mouse(&mut self, _x: u16, y: u16, kind: MouseEventKind) -> PluginAction {
        if !nav::is_select_click(kind) {
            return PluginAction::None;
        }
        match nav::hit_test(
            y,
            self.last_top.get(),
            self.model.selected,
            self.model.rows.len(),
        ) {
            Hit::Nothing => PluginAction::None,
            Hit::Select(idx) => {
                self.model.selected = idx;
                PluginAction::Redraw
            }
            Hit::Activate(_) => self.activate(),
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
            // The aux-pane events are panel-targeted and belong to `files`; the
            // pre-resolved focused cwd is already in the tree this panel holds.
            PluginEvent::FocusedCwd { .. }
            | PluginEvent::AuxPaneReady
            | PluginEvent::AuxPaneContent { .. }
            | PluginEvent::AuxPaneExited => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tree_model::JumpTarget;
    use crate::protocol::{FolderTreeEntry, PaneTreeEntry, SessionTreeEntry, TabTreeEntry};
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
        let mut p = SessionsPlugin::new();
        p.on_event(&local_tree());
        let rows = painted(&p, 24, 8);
        assert_eq!(rows[0], "Sessions");
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
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
        p.on_event(&remote_tree());
        let rows = painted(&p, 24, 8);
        assert!(
            rows.iter().any(|r| r.contains("beta")),
            "the remote's sessions stayed hidden: {rows:?}"
        );
    }

    #[test]
    fn connection_lost_drops_only_that_connections_subtree() {
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
        p.on_event(&local_tree());
        p.on_event(&remote_tree());
        let before = painted(&p, 24, 12);
        // The remote pushes again; the local row must stay above it.
        p.on_event(&remote_tree());
        assert_eq!(painted(&p, 24, 12), before);
    }

    #[test]
    fn j_and_k_move_the_selection_and_ask_for_a_redraw() {
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
        p.on_event(&local_tree());
        p.on_key(key(KeyCode::Char('G')));
        assert_eq!(p.model.selected, p.model.rows.len() - 1);
        p.on_key(key(KeyCode::Char('g')));
        assert_eq!(p.model.selected, 0);
    }

    #[test]
    fn enter_on_a_pane_row_jumps_to_that_pane() {
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
        p.on_event(&local_tree());
        let open = painted(&p, 24, 12);
        assert!(
            open.iter().any(|r| r.starts_with("\u{25BC} local")),
            "an expanded server is not marked open: {open:?}"
        );
        select(&mut p, "local");
        p.on_key(key(KeyCode::Char('h')));
        let shut = painted(&p, 24, 12);
        assert!(
            shut.iter().any(|r| r.starts_with("\u{25B6} local")),
            "a collapsed server is not marked shut: {shut:?}"
        );
    }

    #[test]
    fn a_pane_row_is_indented_under_its_tab() {
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
        p.on_event(&local_tree());
        let theme = CompositorTheme::default();
        let grid = p.render(14, 5, false, &theme);
        assert_eq!(grid.len(), 5);
        assert!(grid.iter().all(|r| r.len() == 14));
        assert!(p.render(0, 0, false, &theme).is_empty());
    }

    #[test]
    fn the_selected_row_is_highlighted_only_while_focused() {
        let mut p = SessionsPlugin::new();
        p.on_event(&local_tree());
        let theme = CompositorTheme::default();
        let focused = p.render(24, 6, true, &theme);
        let idle = p.render(24, 6, false, &theme);
        assert_eq!(focused[1][0].bg, theme.tab_active_bg);
        assert_eq!(focused[1][0].fg, theme.tab_active_fg);
        assert_eq!(idle[1][0].bg, theme.tab_inactive_bg);
        assert_ne!(
            focused[2][0].bg, theme.tab_active_bg,
            "an unselected row is highlighted too"
        );
    }

    #[test]
    fn a_click_selects_a_row_and_a_second_click_activates_it() {
        let mut p = SessionsPlugin::new();
        p.on_event(&local_tree());
        // Paint first: the click's row is resolved against what was drawn.
        let _ = painted(&p, 24, 12);
        let alpha = p
            .model
            .rows
            .iter()
            .position(|r| r.display_name == "alpha")
            .unwrap();
        let y = (alpha + HEADER_ROWS) as u16;
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

    #[test]
    fn a_click_on_the_header_or_past_the_last_row_does_nothing() {
        let mut p = SessionsPlugin::new();
        p.on_event(&local_tree());
        let _ = painted(&p, 24, 12);
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(p.on_mouse(0, 0, down), PluginAction::None);
        assert_eq!(p.on_mouse(0, 200, down), PluginAction::None);
    }

    #[test]
    fn a_click_lands_on_the_row_under_the_cursor_after_scrolling() {
        let mut p = SessionsPlugin::new();
        p.on_event(&local_tree());
        select(&mut p, "editor");
        p.on_key(key(KeyCode::Char('l')));
        p.on_key(key(KeyCode::Char('G')));
        // A 3-row panel shows the header plus the last two tree rows.
        let _ = painted(&p, 24, 3);
        let down = MouseEventKind::Down(MouseButton::Left);
        p.on_mouse(0, 1, down);
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
        let mut p = SessionsPlugin::new();
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
        let mut p = SessionsPlugin::new();
        p.on_event(&remote_tree());
        let rows = painted(&p, 40, 12);
        assert!(rows.iter().any(|r| r.trim() == "\u{25BC} gpu"), "{rows:?}");
    }

    #[test]
    fn a_folder_the_user_collapsed_survives_a_tree_that_empties_and_refills() {
        // The panel refreshes on EVERY push, so this boundary is its problem in
        // a way it never was for an overlay opened on demand.
        let mut p = SessionsPlugin::new();
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
}
