//! The agents panel: every pane running an AI coding agent, on every connected
//! server, with what it is doing.
//!
//! A flat list rather than a tree. The panel answers one question -- "which of
//! my agents needs me?" -- and a hierarchy would put the answer two keystrokes
//! away behind expanders.
//!
//! The list behaviour (`j`/`k`, `g`/`G`, `Enter`, the scrolled window, the
//! click-then-click-again activation, and the selection that survives a refresh
//! by IDENTITY) is [`super::nav`], shared with the sessions panel rather than
//! rewritten here. `Enter` produces a [`JumpTarget::Pane`], the same value the
//! sessions panel produces and the same client-side jump path -- there is
//! deliberately no second "go to that pane" implementation.
//!
//! One row may carry a full-width bar in `sidebar_current_bg` marking the pane
//! the user is currently in. It is fed by [`super::PluginEvent::FocusedPane`]
//! -- the client's walk of the foreground server's tree, the same call that
//! feeds the `files` panel's directory -- rather than derived here, for the
//! reason that event documents: this panel gets an agent LIST, which says
//! nothing about where the user is.
//!
//! The identity is `(ConnId, PaneId)`, never the pane id alone. Pane ids are
//! per-SERVER counters, so two connected machines both have a pane 1; keying on
//! the id would let a refresh on one server point the selection at the other's
//! pane, which is the same class of bug the identity-preserving selection
//! exists to prevent.

use crossterm::event::{KeyEvent, MouseEventKind};

use super::nav::{self, NavKey, NavList, HEADER_ROWS};
use super::{blank_grid, draw_text, PluginAction, PluginEvent, SidebarPlugin};
use crate::client::registry::ConnId;
use crate::client::tree_model::JumpTarget;
use crate::config::theme::CompositorTheme;
use crate::protocol::{AgentEntry, AgentState, CellColor, PaneId, RenderCell};

/// One listed agent, and the server it lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub conn: ConnId,
    pub entry: AgentEntry,
}

impl AgentRow {
    /// The row's identity across refreshes. See the module docs for why the
    /// connection is half of it.
    pub fn key(&self) -> (ConnId, PaneId) {
        (self.conn.clone(), self.entry.pane_id)
    }

    /// The label beside the marker, e.g. `claude main/1` -- host-prefixed for a
    /// remote, since two machines routinely have a session of the same name.
    pub fn label(&self) -> String {
        let where_ = match &self.conn {
            ConnId::Local => format!("{}/{}", self.entry.session, self.entry.tab_index),
            ConnId::Remote(name) => {
                format!("{name}:{}/{}", self.entry.session, self.entry.tab_index)
            }
        };
        format!("{} {}", self.entry.command, where_)
    }

    /// Where Enter on this row goes: the client's one jump path, keyed by
    /// connection and pane.
    pub fn jump_target(&self) -> JumpTarget {
        JumpTarget::Pane {
            conn: self.conn.clone(),
            session: self.entry.session.clone(),
            tab_index: self.entry.tab_index,
            pane_id: self.entry.pane_id,
        }
    }
}

/// The colour a state's marker is drawn in.
///
/// The theme's existing activity roles rather than new ones: a panel that
/// invented its own red would disagree with the status bar's bell marker on
/// the same terminal.
pub fn state_fg(state: AgentState, theme: &CompositorTheme) -> CellColor {
    match state {
        AgentState::NeedsInput => theme.tab_bell_fg.clone(),
        AgentState::Working => theme.tab_activity_fg.clone(),
        AgentState::Idle => theme.status_bar_fg.clone(),
    }
}

/// What one connection last reported.
#[derive(Debug, Clone)]
struct ConnAgents {
    agents: Vec<AgentEntry>,
    /// Whether that server can detect agents at all. `false` means "cannot
    /// know", which is not the same as "none".
    supported: bool,
}

/// Every connection's last `AgentList`, and the order agents are listed in.
///
/// Shared by the agents panel and the agent switcher, so the two list the same
/// agents in the same order with the same notes.
#[derive(Debug, Clone, Default)]
pub struct AgentRoster {
    /// Per-connection lists, in arrival order. Order is stable across pushes so
    /// a refresh on one server does not reshuffle the list under the user.
    lists: Vec<(ConnId, ConnAgents)>,
}

impl AgentRoster {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace what `conn` reported, keeping its place in the order.
    pub fn apply(&mut self, conn: &ConnId, agents: &[AgentEntry], supported: bool) {
        let listed = ConnAgents {
            agents: agents.to_vec(),
            supported,
        };
        match self.lists.iter_mut().find(|(id, _)| id == conn) {
            Some(slot) => slot.1 = listed,
            None => self.lists.push((conn.clone(), listed)),
        }
    }

    /// Forget `conn`. Returns whether it had reported anything.
    pub fn drop_conn(&mut self, conn: &ConnId) -> bool {
        let before = self.lists.len();
        self.lists.retain(|(id, _)| id != conn);
        self.lists.len() != before
    }

    /// Whether any connection has reported, even an empty list.
    pub fn has_reports(&self) -> bool {
        !self.lists.is_empty()
    }

    /// Keep only the connections `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&ConnId) -> bool) {
        self.lists.retain(|(id, _)| keep(id));
    }

    /// Local first, then remotes in arrival order: the same fixed head the
    /// sessions panel gives its roster, so the two agree about where "this
    /// machine" is.
    fn ordered(&self) -> impl Iterator<Item = &(ConnId, ConnAgents)> {
        self.lists
            .iter()
            .filter(|(c, _)| *c == ConnId::Local)
            .chain(self.lists.iter().filter(|(c, _)| *c != ConnId::Local))
    }

    /// Every agent, in listing order.
    pub fn rows(&self) -> Vec<AgentRow> {
        self.ordered()
            .flat_map(|(conn, listed)| {
                listed.agents.iter().map(|entry| AgentRow {
                    conn: conn.clone(),
                    entry: entry.clone(),
                })
            })
            .collect()
    }

    /// One line per server that cannot detect agents at all, in listing order.
    ///
    /// An explanation, not a destination: callers render these apart from the
    /// rows so they can never be selected or jumped to.
    pub fn notes(&self) -> Vec<String> {
        self.ordered()
            .filter(|(_, listed)| !listed.supported)
            .map(|(conn, _)| {
                let host = match conn {
                    ConnId::Local => "local".to_string(),
                    ConnId::Remote(name) => name.clone(),
                };
                // Short, because this panel is often twenty columns wide, and
                // specific, because "unavailable" invites a bug report where
                // naming the missing capability answers the question.
                //
                // NOT "needs Linux" any more: macOS detects agents too (the
                // server names a pid through `sysinfo` where Linux reads
                // `/proc/<pid>/comm`), so the old note would have sent a macOS
                // user hunting for a Linux box to fix a panel that works. What
                // is left unsupported is every OTHER platform, and there is no
                // short true name for that set -- so the note says what is
                // missing rather than what to go and install.
                format!("{host}: no detection")
            })
            .collect()
    }
}

pub struct AgentsPlugin {
    roster: AgentRoster,
    /// The flattened rows, in render order.
    rows: Vec<AgentRow>,
    /// The pane the user is currently in, if the panel has been told of one and
    /// it is on a connection the panel is listing. A row IDENTITY, never a row
    /// index: the list is rebuilt on every push, and an index would follow
    /// whichever agent slid into that slot.
    current: Option<(ConnId, PaneId)>,
    nav: NavList,
}

impl AgentsPlugin {
    pub fn new() -> Self {
        Self {
            roster: AgentRoster::new(),
            rows: Vec::new(),
            current: None,
            nav: NavList::new(),
        }
    }

    /// Rebuild [`AgentsPlugin::rows`], keeping the selection on the row it was
    /// on.
    fn rebuild(&mut self) {
        let previous = self.rows.get(self.nav.selected()).map(|r| r.key());
        self.rows = self.roster.rows();
        let keys: Vec<(ConnId, PaneId)> = self.rows.iter().map(|r| r.key()).collect();
        self.nav.reselect(&keys, previous.as_ref());
    }

    /// Enter, or a second click: go to that pane.
    fn activate(&self) -> PluginAction {
        match self.rows.get(self.nav.selected()) {
            Some(row) => PluginAction::JumpTo(row.jump_target()),
            None => PluginAction::None,
        }
    }
}

impl Default for AgentsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SidebarPlugin for AgentsPlugin {
    fn title(&self) -> &str {
        "Agents"
    }

    fn min_size(&self) -> (u16, u16) {
        (12, 3)
    }

    fn wants_agents(&self) -> bool {
        true
    }

    /// Yes -- indirectly, exactly as the `files` panel's does. This panel never
    /// reads a `SessionTree`, but [`PluginEvent::FocusedPane`] is derived from
    /// one, so a client whose only panel is this one would otherwise subscribe
    /// to no tree, receive no focus event, and never mark the current pane's
    /// row at all. The agent list itself rides its own subscription and is
    /// unaffected either way.
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

        // The notes are CAPPED against the list, not just against the panel.
        // Uncapped, three unsupported remotes beside four blocked agents took
        // the whole four-row panel and showed zero agents -- the explanation
        // outranking the thing it explains, on a panel whose entire job is the
        // list. Three macOS remotes is not exotic.
        //
        // At most half the rows when there are agents to show, all of them when
        // there are none (there is nothing to crowd out), and one summarising
        // line rather than a truncated set when they do not fit: partial lists
        // invite "which servers?" with no way to ask.
        let capacity = (rows as usize).saturating_sub(HEADER_ROWS);
        let all_notes = self.roster.notes();
        let budget = if self.rows.is_empty() {
            capacity
        } else {
            capacity / 2
        };
        let notes: Vec<String> = if all_notes.len() <= budget {
            all_notes
        } else if budget >= 1 {
            vec![format!("{} servers: no detection", all_notes.len())]
        } else {
            // A panel with one usable row and agents in it: the agent wins.
            Vec::new()
        };

        if self.rows.is_empty() && notes.is_empty() {
            // Said out loud rather than left blank: an empty panel and a broken
            // one look identical, and this one is empty most of the time.
            draw_text(
                &mut grid,
                0,
                HEADER_ROWS as u16,
                "no agents",
                theme.status_bar_fg.clone(),
                bg,
            );
            return grid;
        }

        // Space for the notes is RESERVED, not left over. Painting them only
        // where the rows ran out dropped the explanation entirely whenever the
        // list filled the panel -- which is exactly the case where a whole
        // server is missing from a list that otherwise looks complete. The list
        // gets the shorter height, so its scrolling accounts for the
        // reservation too.
        let note_rows = notes.len();
        let list_height = rows.saturating_sub(note_rows as u16);
        let list_capacity = (list_height as usize).saturating_sub(HEADER_ROWS);

        let top = self.nav.top_for(list_height, self.rows.len());
        for i in 0..list_capacity {
            let Some(row) = self.rows.get(top + i) else {
                break;
            };
            let y = (HEADER_ROWS + i) as u16;
            let selected = top + i == self.nav.selected();
            let current = self.current.as_ref() == Some(&row.key());
            let (fg, mut row_bg) = nav::row_colors(theme, focused, selected, &bg);
            // The selection bar wins on a row that is both, because the j/k
            // cursor must never be hidden. The current-pane bar is drawn even
            // while this panel has focus: seeing where you are while scrolling
            // the list is the reason it exists.
            if current && !selected {
                row_bg = theme.sidebar_current_bg.clone();
            }
            if selected || current {
                nav::fill_row(&mut grid, y, cols, &fg, &row_bg);
            }
            // The marker keeps its STATE colour even on the selected row: the
            // colour is the whole point of the panel, and a selection that
            // swallowed it would hide the one thing the user came to see.
            draw_text(
                &mut grid,
                0,
                y,
                "\u{25CF}",
                state_fg(row.entry.state, theme),
                row_bg.clone(),
            );
            draw_text(&mut grid, 2, y, &row.label(), fg, row_bg);
        }

        // Directly under the last row when the list is short, and at the bottom
        // of the panel when it is not -- either way, on screen.
        let painted = self.rows.len().saturating_sub(top).min(list_capacity);
        for (n, note) in notes.iter().take(note_rows).enumerate() {
            let y = (HEADER_ROWS + painted + n) as u16;
            draw_text(
                &mut grid,
                0,
                y,
                note,
                theme.status_bar_fg.clone(),
                bg.clone(),
            );
        }
        grid
    }

    fn on_key(&mut self, key: KeyEvent) -> PluginAction {
        match nav::nav_key(&key) {
            Some(NavKey::Activate) => self.activate(),
            Some(cmd) => {
                self.nav.apply(cmd, self.rows.len());
                PluginAction::Redraw
            }
            None => PluginAction::None,
        }
    }

    fn on_mouse(&mut self, _x: u16, y: u16, kind: MouseEventKind) -> PluginAction {
        if !nav::is_select_click(kind) {
            return PluginAction::None;
        }
        match self.nav.click(y, self.rows.len()) {
            nav::HitOutcome::Ignore => PluginAction::None,
            nav::HitOutcome::Moved => PluginAction::Redraw,
            nav::HitOutcome::Activate => self.activate(),
        }
    }

    fn on_event(&mut self, ev: &PluginEvent) {
        match ev {
            PluginEvent::Agents {
                conn,
                agents,
                supported,
            } => {
                self.roster.apply(conn, agents, *supported);
                self.rebuild();
            }
            PluginEvent::FocusedPane { conn, pane_id } => {
                // Replaced outright rather than merged. The event always names
                // the FOREGROUND connection, so the foreground moving to another
                // machine drops the old machine's mark by construction -- no
                // conn-changed branch to get wrong, and no way for two marks to
                // exist at once. `None` clears it: there is a foreground, and it
                // names no pane (detached, or a session with no active tab).
                self.current = pane_id.map(|id| (conn.clone(), id));
            }
            PluginEvent::ConnectionLost { conn } => {
                // The mark named a pane on THAT machine. Pane ids are
                // per-server counters, so leaving it standing would let it land
                // on another machine's pane of the same id -- a different pane
                // entirely, and the reason the connection is half of the key.
                if self.current.as_ref().is_some_and(|(c, _)| c == conn) {
                    self.current = None;
                }
                if self.roster.drop_conn(conn) {
                    self.rebuild();
                }
            }
            // The tree itself is the sessions panel's -- this panel subscribes
            // to it only so the focused-pane event above is derived at all --
            // the cwd is the `files` panel's half of that same walk, and the
            // directory events are the `files` panel's too.
            PluginEvent::SessionTree { .. }
            | PluginEvent::FocusedCwd { .. }
            | PluginEvent::DirectoryListing { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers, MouseButton};

    fn remote(name: &str) -> ConnId {
        ConnId::Remote(name.to_string())
    }

    fn agent(pane_id: PaneId, command: &str, session: &str, state: AgentState) -> AgentEntry {
        AgentEntry {
            pane_id,
            session: session.to_string(),
            tab_index: 0,
            command: command.to_string(),
            state,
        }
    }

    fn push(conn: ConnId, agents: Vec<AgentEntry>) -> PluginEvent {
        PluginEvent::Agents {
            conn,
            agents,
            supported: true,
        }
    }

    fn focus(conn: ConnId, pane_id: Option<PaneId>) -> PluginEvent {
        PluginEvent::FocusedPane { conn, pane_id }
    }

    /// The panel-row index painted edge to edge in `sidebar_current_bg`, and
    /// how many rows are. At most one row may ever be marked, and a count is
    /// the only way to see a second one.
    fn current_bar(
        p: &AgentsPlugin,
        cols: u16,
        rows: u16,
        focused: bool,
    ) -> (Option<usize>, usize) {
        let theme = CompositorTheme::default();
        let grid = p.render(cols, rows, focused, &theme);
        let marked: Vec<usize> = grid
            .iter()
            .enumerate()
            .filter(|(_, r)| r.iter().all(|c| c.bg == theme.sidebar_current_bg))
            .map(|(y, _)| y)
            .collect();
        (marked.first().copied(), marked.len())
    }

    fn unsupported(conn: ConnId) -> PluginEvent {
        PluginEvent::Agents {
            conn,
            agents: Vec::new(),
            supported: false,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// The panel's visible rows, from a real render of the FOCUSED panel.
    fn painted(p: &AgentsPlugin, cols: u16, rows: u16) -> Vec<String> {
        let theme = CompositorTheme::default();
        p.render(cols, rows, true, &theme)
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn an_empty_panel_says_so() {
        let p = AgentsPlugin::new();
        let rows = painted(&p, 20, 4);
        assert_eq!(rows[0], "Agents");
        assert_eq!(rows[1], "no agents");
    }

    #[test]
    fn each_agent_is_a_row_naming_its_command_and_place() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "work", AgentState::NeedsInput),
                agent(2, "codex", "play", AgentState::Idle),
            ],
        ));
        let rows = painted(&p, 24, 5);
        assert!(rows[1].contains("claude work/0"), "got {:?}", rows[1]);
        assert!(rows[2].contains("codex play/0"), "got {:?}", rows[2]);
    }

    #[test]
    fn a_remote_agent_is_host_prefixed() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            remote("pi"),
            vec![agent(1, "claude", "main", AgentState::Idle)],
        ));
        assert!(painted(&p, 24, 4)[1].contains("pi:main/0"));
    }

    #[test]
    fn the_state_marker_takes_the_themes_activity_colours() {
        let theme = CompositorTheme::default();
        assert_eq!(
            state_fg(AgentState::NeedsInput, &theme),
            theme.tab_bell_fg,
            "urgent shares the bell role"
        );
        assert_eq!(state_fg(AgentState::Working, &theme), theme.tab_activity_fg);
        assert_eq!(state_fg(AgentState::Idle, &theme), theme.status_bar_fg);
    }

    #[test]
    fn the_marker_keeps_its_state_colour_on_the_selected_row() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![agent(1, "claude", "work", AgentState::NeedsInput)],
        ));
        let theme = CompositorTheme::default();
        let grid = p.render(20, 3, true, &theme);
        assert_eq!(
            grid[1][0].fg, theme.tab_bell_fg,
            "the selection must not swallow the state colour"
        );
    }

    #[test]
    fn enter_jumps_to_that_pane_on_that_server() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![agent(7, "claude", "work", AgentState::Idle)],
        ));
        p.on_event(&push(
            remote("pi"),
            vec![agent(3, "codex", "far", AgentState::Working)],
        ));
        p.on_key(key(KeyCode::Char('j')));
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            PluginAction::JumpTo(JumpTarget::Pane {
                conn: remote("pi"),
                session: "far".to_string(),
                tab_index: 0,
                pane_id: 3,
            })
        );
    }

    #[test]
    fn local_is_listed_before_the_remotes_whatever_order_they_arrived_in() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            remote("pi"),
            vec![agent(1, "codex", "far", AgentState::Idle)],
        ));
        p.on_event(&push(
            ConnId::Local,
            vec![agent(1, "claude", "here", AgentState::Idle)],
        ));
        let rows = painted(&p, 24, 5);
        assert!(rows[1].contains("claude here/0"), "got {:?}", rows[1]);
        assert!(rows[2].contains("codex pi:far/0"), "got {:?}", rows[2]);
    }

    #[test]
    fn the_selection_follows_its_agent_when_one_above_it_goes_away() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
                agent(3, "claude", "c", AgentState::Idle),
            ],
        ));
        p.on_key(key(KeyCode::Char('j')));
        p.on_key(key(KeyCode::Char('j')));
        // Pane 1 exits: the row the user was on moves up an index.
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(2, "claude", "b", AgentState::Idle),
                agent(3, "claude", "c", AgentState::Idle),
            ],
        ));
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            PluginAction::JumpTo(JumpTarget::Pane {
                conn: ConnId::Local,
                session: "c".to_string(),
                tab_index: 0,
                pane_id: 3,
            }),
            "Enter must still go where the user was looking"
        );
    }

    /// Pane ids are per-server counters, so identity must carry the connection.
    #[test]
    fn two_servers_with_the_same_pane_id_are_two_different_rows() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![agent(1, "claude", "here", AgentState::Idle)],
        ));
        p.on_event(&push(
            remote("pi"),
            vec![agent(1, "claude", "far", AgentState::Idle)],
        ));
        // Select the REMOTE pane 1.
        p.on_key(key(KeyCode::Char('G')));
        // The local server refreshes, unchanged. A selection keyed on the pane
        // id alone would now match the LOCAL pane 1 and move the cursor.
        p.on_event(&push(
            ConnId::Local,
            vec![agent(1, "claude", "here", AgentState::Idle)],
        ));
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            PluginAction::JumpTo(JumpTarget::Pane {
                conn: remote("pi"),
                session: "far".to_string(),
                tab_index: 0,
                pane_id: 1,
            })
        );
    }

    #[test]
    fn a_state_change_alone_does_not_move_the_selection() {
        let mut p = AgentsPlugin::new();
        let listed = |state| {
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", state),
            ]
        };
        p.on_event(&push(ConnId::Local, listed(AgentState::Idle)));
        p.on_key(key(KeyCode::Char('j')));
        p.on_event(&push(ConnId::Local, listed(AgentState::NeedsInput)));
        assert_eq!(p.nav.selected(), 1);
    }

    #[test]
    fn a_dropped_connection_takes_its_agents_with_it() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![agent(1, "claude", "here", AgentState::Idle)],
        ));
        p.on_event(&push(
            remote("pi"),
            vec![agent(1, "codex", "far", AgentState::Idle)],
        ));
        p.on_event(&PluginEvent::ConnectionLost { conn: remote("pi") });
        let rows = painted(&p, 24, 5);
        assert!(rows[1].contains("claude here/0"));
        assert_eq!(rows[2], "", "the remote's row is gone");
    }

    #[test]
    fn a_click_selects_and_a_second_click_jumps() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
            ],
        ));
        // A render is what tells the panel which window a click lands in.
        let _ = painted(&p, 24, 5);
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(p.on_mouse(0, 2, down), PluginAction::Redraw);
        assert!(matches!(p.on_mouse(0, 2, down), PluginAction::JumpTo(_)));
        // The header is not a row.
        assert_eq!(p.on_mouse(0, 0, down), PluginAction::None);
    }

    #[test]
    fn a_server_that_cannot_detect_agents_says_so_instead_of_looking_empty() {
        let mut p = AgentsPlugin::new();
        p.on_event(&unsupported(ConnId::Local));
        let rows = painted(&p, 24, 5);
        assert_eq!(
            rows[1], "local: no detection",
            "an empty list there is indistinguishable from having no agents"
        );
        assert_ne!(rows[1], "no agents");
    }

    #[test]
    fn the_note_names_the_server_that_cannot_detect_and_lists_the_ones_that_can() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![agent(1, "claude", "here", AgentState::Idle)],
        ));
        p.on_event(&unsupported(remote("mac")));
        let rows = painted(&p, 24, 6);
        assert!(rows[1].contains("claude here/0"), "got {:?}", rows[1]);
        assert_eq!(rows[2], "mac: no detection", "got {:?}", rows[2]);
    }

    /// The note is an explanation, not a destination.
    #[test]
    fn the_note_is_not_selectable_and_enter_cannot_land_on_it() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![agent(1, "claude", "here", AgentState::Idle)],
        ));
        p.on_event(&unsupported(remote("mac")));
        let _ = painted(&p, 24, 6);
        // `G` goes to the last ROW; the note is not one.
        p.on_key(key(KeyCode::Char('G')));
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            PluginAction::JumpTo(JumpTarget::Pane {
                conn: ConnId::Local,
                session: "here".to_string(),
                tab_index: 0,
                pane_id: 1,
            })
        );
        // And a click on the note's row selects nothing.
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(p.on_mouse(0, 2, down), PluginAction::None);
    }

    /// The note must survive a panel with NO slack. The three tests above all
    /// use panels roomy enough that the note landed in leftover space, so none
    /// of them could fail for the reason they were written.
    #[test]
    fn the_note_survives_a_panel_the_agent_list_already_fills() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            remote("linux"),
            (1..=6)
                .map(|i| agent(i, "claude", &format!("s{i}"), AgentState::Idle))
                .collect(),
        ));
        p.on_event(&unsupported(ConnId::Local));
        // Header + 2 rows + the note: the list alone would fill this and more.
        let rows = painted(&p, 24, 4);
        assert_eq!(
            rows[3], "local: no detection",
            "a full list must not push the explanation off the panel, got {rows:?}"
        );
        assert!(
            rows[1].contains("claude"),
            "and the rows still paint: {rows:?}"
        );
    }

    /// With the note pinned at the bottom, a click there no longer lines up
    /// with a row index by accident.
    #[test]
    fn a_click_on_the_note_selects_nothing_even_with_rows_scrolled_under_it() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            remote("linux"),
            (1..=6)
                .map(|i| agent(i, "claude", &format!("s{i}"), AgentState::Idle))
                .collect(),
        ));
        p.on_event(&unsupported(ConnId::Local));
        let _ = painted(&p, 24, 4);
        let down = MouseEventKind::Down(MouseButton::Left);
        // Rows are painted at y=1..2; the note is at y=3.
        assert_eq!(p.on_mouse(0, 2, down), PluginAction::Redraw);
        assert_eq!(
            p.on_mouse(0, 3, down),
            PluginAction::None,
            "the note is not a row, however many rows exist below the fold"
        );
    }

    /// The explanation must never outrank the thing it explains.
    #[test]
    fn notes_cannot_crowd_the_agents_out_of_the_panel() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            (1..=4)
                .map(|i| agent(i, "claude", &format!("s{i}"), AgentState::NeedsInput))
                .collect(),
        ));
        for host in ["mac1", "mac2", "mac3"] {
            p.on_event(&unsupported(remote(host)));
        }
        // Four blocked agents and three unsupported remotes in a four-row panel.
        let rows = painted(&p, 24, 4);
        let agents_shown = rows[1..].iter().filter(|r| r.contains("claude")).count();
        assert!(
            agents_shown >= 2,
            "the notes took the panel from the agents: {rows:?}"
        );
        assert_eq!(
            rows[3], "3 servers: no detection",
            "and the three collapse to one line rather than being truncated: {rows:?}"
        );
    }

    #[test]
    fn with_no_agents_at_all_every_note_is_listed_individually() {
        let mut p = AgentsPlugin::new();
        for host in ["mac1", "mac2", "mac3"] {
            p.on_event(&unsupported(remote(host)));
        }
        let rows = painted(&p, 24, 5);
        assert_eq!(rows[1], "mac1: no detection");
        assert_eq!(rows[2], "mac2: no detection");
        assert_eq!(
            rows[3], "mac3: no detection",
            "nothing to crowd out: {rows:?}"
        );
    }

    #[test]
    fn a_supported_server_with_no_agents_still_says_no_agents() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(ConnId::Local, Vec::new()));
        assert_eq!(painted(&p, 24, 4)[1], "no agents");
    }

    // -- the current-pane bar ---------------------------------------------
    //
    // A fresh panel selects its first row, and the selection bar wins on a row
    // that is both. So every fixture below puts the current pane on a row
    // OTHER than the selected one: a current row under the selection shows no
    // current bar, and a "nothing is marked" assertion there would pass whatever
    // the code did.

    #[test]
    fn the_bar_marks_the_focused_pane_and_no_other_row() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
                agent(3, "claude", "c", AgentState::Idle),
            ],
        ));
        p.on_event(&focus(ConnId::Local, Some(2)));
        assert_eq!(
            current_bar(&p, 24, 5, false),
            (Some(2), 1),
            "pane 2 is the second agent row, at panel row 2, and it is the only mark"
        );
    }

    #[test]
    fn the_bar_spans_the_full_width_of_the_row() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
            ],
        ));
        p.on_event(&focus(ConnId::Local, Some(2)));
        let theme = CompositorTheme::default();
        let grid = p.render(24, 4, false, &theme);
        for (x, cell) in grid[2].iter().enumerate() {
            assert_eq!(cell.bg, theme.sidebar_current_bg, "column {x}");
        }
    }

    #[test]
    fn the_bar_moves_when_the_focused_pane_does() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
                agent(3, "claude", "c", AgentState::Idle),
            ],
        ));
        p.on_event(&focus(ConnId::Local, Some(2)));
        assert_eq!(current_bar(&p, 24, 5, false).0, Some(2));
        p.on_event(&focus(ConnId::Local, Some(3)));
        assert_eq!(
            current_bar(&p, 24, 5, false),
            (Some(3), 1),
            "the old mark must go, not accumulate"
        );
    }

    /// The focused pane is very often NOT an agent pane -- an editor, a shell --
    /// and no mark beats a mark on whichever agent happens to sit nearby.
    #[test]
    fn a_focused_pane_that_is_not_an_agent_marks_nothing() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
            ],
        ));
        p.on_event(&focus(ConnId::Local, Some(99)));
        assert_eq!(current_bar(&p, 24, 5, false).1, 0);
    }

    /// `None` is "there is a foreground, and it names no pane" -- detached, or a
    /// session with no active tab. It must CLEAR the mark, not leave the last
    /// one standing.
    #[test]
    fn a_focus_event_carrying_no_pane_clears_the_bar() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
            ],
        ));
        p.on_event(&focus(ConnId::Local, Some(2)));
        assert_eq!(
            current_bar(&p, 24, 5, false).1,
            1,
            "the bar was there to clear"
        );
        p.on_event(&focus(ConnId::Local, None));
        assert_eq!(current_bar(&p, 24, 5, false).1, 0);
    }

    #[test]
    fn a_panel_that_has_heard_no_focus_marks_nothing() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
            ],
        ));
        assert_eq!(current_bar(&p, 24, 5, false).1, 0);
    }

    /// The user reads the bar WHILE scrolling this list with j/k, so it must
    /// not depend on where focus is. The two renders are the same setup, so
    /// the focus flag is the only variable.
    #[test]
    fn the_bar_shows_whether_or_not_this_panel_has_focus() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
            ],
        ));
        p.on_event(&focus(ConnId::Local, Some(2)));
        assert_eq!(current_bar(&p, 24, 5, false), (Some(2), 1), "unfocused");
        assert_eq!(current_bar(&p, 24, 5, true), (Some(2), 1), "focused");
    }

    /// Scrolling onto the current row hides its bar under the selection, and
    /// scrolling off again brings it back.
    #[test]
    fn the_selection_bar_wins_on_the_current_row_and_the_bar_returns_after() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
            ],
        ));
        p.on_event(&focus(ConnId::Local, Some(2)));
        let theme = CompositorTheme::default();
        p.on_key(key(KeyCode::Char('j')));
        let grid = p.render(24, 4, true, &theme);
        for (x, cell) in grid[2].iter().enumerate() {
            assert_eq!(
                cell.bg, theme.tab_active_bg,
                "focused selection, column {x}"
            );
        }
        let grid = p.render(24, 4, false, &theme);
        for (x, cell) in grid[2].iter().enumerate() {
            assert_eq!(
                cell.bg, theme.tab_inactive_bg,
                "unfocused selection, column {x}"
            );
        }
        assert_eq!(current_bar(&p, 24, 4, true).1, 0);
        p.on_key(key(KeyCode::Char('k')));
        assert_eq!(current_bar(&p, 24, 4, true), (Some(2), 1));
    }

    #[test]
    fn the_bar_displaces_neither_the_state_marker_nor_the_label() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "other", AgentState::Idle),
                agent(2, "claude", "work", AgentState::Idle),
            ],
        ));
        let unmarked = painted(&p, 24, 4)[2].clone();
        p.on_event(&focus(ConnId::Local, Some(2)));
        assert_eq!(current_bar(&p, 24, 4, true).0, Some(2));
        let marked = painted(&p, 24, 4)[2].clone();
        assert_eq!(marked, unmarked, "the bar changes colour, never text");
        assert!(
            marked.starts_with("\u{25CF} claude work/0"),
            "marker at column 0, label at column 2, got {marked:?}"
        );
    }

    /// The tree push and the agent push are separate subscriptions, so either
    /// can arrive first. A mark resolved when the event ARRIVED would be lost
    /// by the arrival that has not happened yet; matched at render time, the
    /// order cannot matter.
    #[test]
    fn a_focus_that_arrives_before_the_agent_list_still_marks_its_row() {
        let mut p = AgentsPlugin::new();
        p.on_event(&focus(ConnId::Local, Some(2)));
        assert_eq!(current_bar(&p, 24, 5, false).1, 0, "nothing to mark yet");
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
            ],
        ));
        assert_eq!(current_bar(&p, 24, 5, false), (Some(2), 1));
    }

    /// The panel's standing doctrine: the state colour is the whole point, and
    /// no bar painted on the row may swallow it.
    #[test]
    fn the_state_colour_survives_on_the_current_row() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "work", AgentState::NeedsInput),
            ],
        ));
        p.on_event(&focus(ConnId::Local, Some(2)));
        let theme = CompositorTheme::default();
        let grid = p.render(20, 4, false, &theme);
        assert_eq!(grid[2][0].fg, theme.tab_bell_fg);
        assert_eq!(grid[2][0].bg, theme.sidebar_current_bg);
    }

    #[test]
    fn the_state_colour_survives_on_a_row_that_is_both_current_and_selected() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![agent(1, "claude", "work", AgentState::NeedsInput)],
        ));
        p.on_event(&focus(ConnId::Local, Some(1)));
        let theme = CompositorTheme::default();
        // Row 0 is both the selection (a fresh panel selects it) and the current
        // pane.
        let grid = p.render(20, 3, false, &theme);
        assert_eq!(grid[1][0].fg, theme.tab_bell_fg);
        assert_eq!(
            grid[1][3].bg, theme.tab_inactive_bg,
            "the selection bar wins over the current-pane bar"
        );
    }

    /// Pane ids are per-server counters. A mark held for a machine that went
    /// away must not reappear on another machine's pane of the same id -- which
    /// is the whole reason the connection is half of the key.
    #[test]
    fn a_lost_connection_takes_its_bar_and_does_not_hand_it_to_the_same_id_elsewhere() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(5, "claude", "top", AgentState::Idle),
                agent(1, "claude", "here", AgentState::Idle),
            ],
        ));
        p.on_event(&push(
            remote("pi"),
            vec![agent(1, "claude", "far", AgentState::Idle)],
        ));
        // Two machines, both with a pane 1. The mark is on the REMOTE's. The
        // local pane 1 sits on row 2, off the selection, so a mark that landed
        // on it would be visible.
        p.on_event(&focus(remote("pi"), Some(1)));
        assert_eq!(
            current_bar(&p, 24, 6, false),
            (Some(3), 1),
            "row 3 is the remote's pane 1, not the local one at row 2"
        );
        p.on_event(&PluginEvent::ConnectionLost { conn: remote("pi") });
        assert_eq!(
            current_bar(&p, 24, 6, false).1,
            0,
            "the local pane 1 is still listed; a mark keyed on the id alone would land on it"
        );
    }

    #[test]
    fn another_connection_going_away_leaves_the_bar_where_it_is() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(5, "claude", "top", AgentState::Idle),
                agent(1, "claude", "here", AgentState::Idle),
            ],
        ));
        p.on_event(&push(
            remote("pi"),
            vec![agent(1, "claude", "far", AgentState::Idle)],
        ));
        p.on_event(&focus(ConnId::Local, Some(1)));
        p.on_event(&PluginEvent::ConnectionLost { conn: remote("pi") });
        assert_eq!(current_bar(&p, 24, 6, false), (Some(2), 1));
    }

    /// The foreground moving to another machine replaces the mark rather than
    /// adding one: the event names the foreground, so there is only ever one.
    #[test]
    fn the_foreground_moving_to_another_machine_moves_the_bar_with_it() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(5, "claude", "top", AgentState::Idle),
                agent(1, "claude", "here", AgentState::Idle),
            ],
        ));
        p.on_event(&push(
            remote("pi"),
            vec![agent(1, "claude", "far", AgentState::Idle)],
        ));
        p.on_event(&focus(ConnId::Local, Some(1)));
        assert_eq!(current_bar(&p, 24, 6, false), (Some(2), 1));
        p.on_event(&focus(remote("pi"), Some(1)));
        assert_eq!(
            current_bar(&p, 24, 6, false),
            (Some(3), 1),
            "the previous machine's mark must not survive alongside the new one"
        );
    }

    /// Indirectly, exactly as the `files` panel does: this panel reads no tree,
    /// but the focus event it marks with is derived from one.
    #[test]
    fn the_panel_subscribes_to_the_tree_the_focus_event_is_derived_from() {
        let p = AgentsPlugin::new();
        assert!(p.wants_agents());
        assert!(
            p.wants_session_tree(),
            "no subscription, no tree, no focus event, no current-pane bar -- ever"
        );
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_selection_visible() {
        let mut p = AgentsPlugin::new();
        let many: Vec<AgentEntry> = (1..=10)
            .map(|i| agent(i, "claude", &format!("s{i}"), AgentState::Idle))
            .collect();
        p.on_event(&push(ConnId::Local, many));
        p.on_key(key(KeyCode::Char('G')));
        // 4 rows tall: header + 3 list rows, showing s8, s9, s10.
        let rows = painted(&p, 24, 4);
        assert!(
            rows[3].contains("s10"),
            "the selection must be on screen, got {rows:?}"
        );
    }
}
