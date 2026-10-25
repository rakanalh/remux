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

use std::collections::HashMap;

use crossterm::event::{KeyEvent, MouseEventKind};

use super::nav::{self, ListLayout, NavKey, NavList};
use super::{blank_grid, draw_text, draw_text_with, PluginAction, PluginEvent, SidebarPlugin};
use crate::client::context_menu::{MenuAction, MenuItem};
use crate::client::registry::ConnId;
use crate::client::tree_model::JumpTarget;
use crate::config::theme::CompositorTheme;
use crate::protocol::{AgentEntry, AgentState, CellColor, PaneId, RemuxCommand, RenderCell};

/// Lines per agent row, in every sidebar style.
const ROW_HEIGHT: usize = 2;

/// One listed agent, and the server it lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub conn: ConnId,
    pub entry: AgentEntry,
    /// The client's `[appearance] pane_title` template.
    pub pane_title: Option<String>,
}

impl AgentRow {
    /// The row's identity across refreshes. See the module docs for why the
    /// connection is half of it.
    pub fn key(&self) -> (ConnId, PaneId) {
        (self.conn.clone(), self.entry.pane_id)
    }

    /// The label beside the marker, e.g. `claude main/1` -- host-prefixed for a
    /// remote, since two machines routinely have a session of the same name.
    ///
    /// A name the user gave the pane wins outright. Otherwise a `pane_title`
    /// template decides; with none, a titled agent is shown by its title alone
    /// and an untitled one by this command-and-location form.
    pub fn label(&self) -> String {
        if let Some(custom) = &self.entry.custom_name {
            return custom.clone();
        }
        let host = match &self.conn {
            ConnId::Local => "",
            ConnId::Remote(name) => name.as_str(),
        };
        let title = self.entry.title.as_deref().filter(|t| !t.is_empty());
        if let Some(template) = self.pane_title.as_deref() {
            return crate::config::pane_title::display_name(
                Some(template),
                &crate::config::pane_title::NameParts {
                    title,
                    command: &self.entry.command,
                    session: &self.entry.session,
                    tab: self.entry.tab_index,
                    cwd: self.entry.cwd.as_deref(),
                    host,
                },
            );
        }
        if let Some(title) = title {
            return title.to_string();
        }
        let where_ = match &self.conn {
            ConnId::Local => format!("{}/{}", self.entry.session, self.entry.tab_index),
            ConnId::Remote(name) => {
                format!("{name}:{}/{}", self.entry.session, self.entry.tab_index)
            }
        };
        format!("{} {}", self.entry.command, where_)
    }

    /// What a row shows on its first line: a name, and an optional dimmer
    /// tail.
    ///
    /// A name the user gave the pane, a `pane_title` template result or the
    /// pane's own title is shown whole, as [`AgentRow::label`] shows it. With
    /// none of those, the location is the name: the session, then ` · ` and the
    /// tab's name, or its index when `tab_name` is not known (a tree not yet
    /// received). The command is left out because the second line carries it.
    pub fn row_name(&self, tab_name: Option<&str>) -> (String, Option<String>) {
        let titled = self.entry.custom_name.is_some()
            || self.pane_title.is_some()
            || self.entry.title.as_deref().is_some_and(|t| !t.is_empty());
        if titled {
            return (self.label(), None);
        }
        (
            self.entry.session.clone(),
            Some(match tab_name.filter(|n| !n.is_empty()) {
                Some(name) => format!(" \u{b7} {name}"),
                None => format!(" \u{b7} {}", self.entry.tab_index),
            }),
        )
    }

    /// A row's second line: the agent, and the host when it runs on a remote.
    pub fn row_detail(&self) -> String {
        match &self.conn {
            ConnId::Local => self.entry.command.clone(),
            ConnId::Remote(host) => format!("{} \u{b7} {host}", self.entry.command),
        }
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
    /// The `[appearance] pane_title` template every row is labelled with.
    pane_title: Option<String>,
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

    /// Label rows with `template` from now on.
    pub fn set_pane_title(&mut self, template: Option<String>) {
        self.pane_title = template;
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
                    pane_title: self.pane_title.clone(),
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

/// How one row is painted: where it sits and in which colours.
struct RowLook {
    /// Width of the panel, which the selection band spans.
    cols: u16,
    /// Columns of left padding.
    pad: u16,
    /// The band behind both lines, when the row is selected or current.
    band: Option<CellColor>,
    name_fg: CellColor,
    bold: bool,
    /// The panel background, behind a row without a band.
    base_bg: CellColor,
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
    /// Tab names from the session tree: per connection, a session's tab names in
    /// tab order. A row names the tab the agent runs in; the agent list itself
    /// carries only the index. Nested so a lookup borrows the row's own strings.
    tab_names: HashMap<ConnId, HashMap<String, Vec<String>>>,
}

impl AgentsPlugin {
    pub fn new() -> Self {
        Self {
            roster: AgentRoster::new(),
            rows: Vec::new(),
            current: None,
            nav: {
                let mut nav = NavList::new();
                nav.set_layout(ListLayout::new(ROW_HEIGHT));
                nav
            },
            tab_names: HashMap::new(),
        }
    }

    fn tab_name(&self, row: &AgentRow) -> Option<&str> {
        self.tab_names
            .get(&row.conn)?
            .get(row.entry.session.as_str())?
            .get(row.entry.tab_index)
            .map(String::as_str)
    }

    /// Paint one two-line row at panel row `y`.
    ///
    /// Line one is the state dot at the padding column and the name two columns
    /// after it. Line two sits under the name. The band, when there is one,
    /// covers both lines edge to edge. The secondary text is `sidebar_dim_fg`.
    fn paint_row(
        grid: &mut [Vec<RenderCell>],
        y: u16,
        row: &AgentRow,
        tab_name: Option<&str>,
        look: &RowLook,
        theme: &CompositorTheme,
    ) {
        let RowLook {
            cols,
            pad,
            band,
            name_fg,
            bold,
            base_bg,
        } = look;
        let (cols, pad, bold) = (*cols, *pad, *bold);
        let name_fg = name_fg.clone();
        let dim_fg = theme.sidebar_dim_fg.clone();
        let bg = band.as_ref().unwrap_or(base_bg).clone();
        if band.is_some() {
            for line in [y, y + 1] {
                nav::fill_row(grid, line, cols, &name_fg, &bg);
            }
        }
        // The marker keeps its STATE colour even on the selected row: the
        // colour is the whole point of the panel, and a selection that
        // swallowed it would hide the one thing the user came to see.
        let dot = if row.entry.state == AgentState::Idle {
            "\u{25CB}"
        } else {
            "\u{25CF}"
        };
        draw_text(
            grid,
            pad,
            y,
            dot,
            state_fg(row.entry.state, theme),
            bg.clone(),
        );
        let text_x = pad + 2;
        let (name, tail) = row.row_name(tab_name);
        draw_text_with(grid, text_x, y, &name, name_fg, bg.clone(), bold);
        if let Some(tail) = tail {
            let name_width = unicode_width::UnicodeWidthStr::width(name.as_str());
            let x = text_x.saturating_add(u16::try_from(name_width).unwrap_or(u16::MAX));
            draw_text(grid, x, y, &tail, dim_fg.clone(), bg.clone());
        }
        draw_text(grid, text_x, y + 1, &row.row_detail(), dim_fg, bg);
    }

    fn layout(&self) -> ListLayout {
        self.nav.layout()
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
        (12, (self.layout().header_rows + ROW_HEIGHT) as u16)
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
        let layout = self.layout();
        nav::draw_header(&mut grid, self.title(), focused, theme);

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
        let capacity = (rows as usize).saturating_sub(layout.header_rows);
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
                layout.pad(),
                layout.header_rows as u16,
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
        let list_capacity = layout.visible(list_height);

        let top = self.nav.top_for(list_height, self.rows.len());
        for i in 0..list_capacity {
            let Some(row) = self.rows.get(top + i) else {
                break;
            };
            let y = layout.row_of(i);
            let selected = top + i == self.nav.selected();
            let current = self.current.as_ref() == Some(&row.key());
            let tab_name = self.tab_name(row);
            let (fg, sel_bg, bold) = nav::row_colors(theme, focused, selected, &bg);
            // The selection band wins on a row that is both, because the j/k
            // cursor must never be hidden. The current-pane band is drawn even
            // while this panel has focus: seeing where you are while scrolling
            // the list is the reason it exists.
            let band = if selected {
                Some(sel_bg)
            } else if current {
                Some(theme.sidebar_current_bg.clone())
            } else {
                None
            };
            let look = RowLook {
                cols,
                pad: layout.pad(),
                band,
                name_fg: fg,
                bold,
                base_bg: bg.clone(),
            };
            Self::paint_row(&mut grid, y, row, tab_name, &look, theme);
        }

        // Directly under the last row when the list is short, and at the bottom
        // of the panel when it is not -- either way, on screen.
        let painted = self.rows.len().saturating_sub(top).min(list_capacity);
        for (n, note) in notes.iter().take(note_rows).enumerate() {
            let y = (layout.header_rows + painted * layout.row_height + n) as u16;
            draw_text(
                &mut grid,
                layout.pad(),
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

    fn context_menu(&mut self, _x: u16, y: u16) -> Vec<MenuItem> {
        let Some(row) = self
            .nav
            .pick(y, self.rows.len())
            .and_then(|i| self.rows.get(i))
        else {
            return Vec::new();
        };
        let target = row.jump_target();
        vec![
            MenuItem::new("Go to pane", MenuAction::Jump(target.clone()))
                .hint(Some("Enter".into())),
            MenuItem::new(
                "Zoom",
                MenuAction::JumpThen(
                    target,
                    Box::new(MenuAction::Command(RemuxCommand::OnPane {
                        pane_id: row.entry.pane_id,
                        command: Box::new(RemuxCommand::PaneEnsureZoom),
                    })),
                ),
            ),
            MenuItem::new(
                "Close pane",
                MenuAction::ClosePane {
                    conn: row.conn.clone(),
                    session: row.entry.session.clone(),
                    pane_id: row.entry.pane_id,
                },
            ),
        ]
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
                self.tab_names.remove(conn);
                if self.roster.drop_conn(conn) {
                    self.rebuild();
                }
            }
            // The tree is the sessions panel's, but its tab names are what a
            // row shows beside the session. The cwd is the `files`
            // panel's half of the focused-pane walk, and the directory events
            // are the `files` panel's too.
            PluginEvent::SessionTree {
                conn,
                folders,
                unfiled,
                ..
            } => {
                let sessions = folders
                    .iter()
                    .flat_map(|f| f.sessions.iter())
                    .chain(unfiled.iter());
                let names = sessions
                    .map(|session| {
                        let tabs = session.tabs.iter().map(|t| t.name.clone()).collect();
                        (session.name.clone(), tabs)
                    })
                    .collect();
                self.tab_names.insert(conn.clone(), names);
            }
            PluginEvent::FocusedCwd { .. } | PluginEvent::DirectoryListing { .. } => {}
            PluginEvent::PaneTitle { template } => {
                self.roster.set_pane_title(template.clone());
                self.rebuild();
            }
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
            title: None,
            custom_name: None,
            cwd: None,
            focused: false,
        }
    }

    fn row(conn: ConnId, entry: AgentEntry, pane_title: Option<&str>) -> AgentRow {
        AgentRow {
            conn,
            entry,
            pane_title: pane_title.map(str::to_string),
        }
    }

    #[test]
    fn an_untitled_row_keeps_the_command_and_location() {
        let e = agent(1, "claude", "main", AgentState::Idle);
        assert_eq!(row(ConnId::Local, e.clone(), None).label(), "claude main/0");
        assert_eq!(row(remote("mini"), e, None).label(), "claude mini:main/0");
    }

    #[test]
    fn a_titled_row_is_the_title_alone() {
        let mut e = agent(1, "claude", "main", AgentState::Idle);
        e.title = Some("Fix the bug".into());
        assert_eq!(row(remote("mini"), e, None).label(), "Fix the bug");
    }

    #[test]
    fn a_new_template_relabels_the_rows_at_once() {
        let mut p = AgentsPlugin::new();
        let mut e = agent(1, "claude", "main", AgentState::Idle);
        e.title = Some("Fix the bug".into());
        p.on_event(&push(ConnId::Local, vec![e]));
        assert_eq!(p.rows[0].label(), "Fix the bug");
        p.on_event(&PluginEvent::PaneTitle {
            template: Some("{command}: {title}".into()),
        });
        assert_eq!(p.rows[0].label(), "claude: Fix the bug");
    }

    #[test]
    fn a_template_labels_the_row_and_a_custom_name_beats_it() {
        let mut e = agent(1, "claude", "main", AgentState::Idle);
        e.title = Some("Fix the bug".into());
        e.cwd = Some("remux".into());
        assert_eq!(
            row(
                remote("mini"),
                e.clone(),
                Some("{command}: {title} @{host} {cwd}")
            )
            .label(),
            "claude: Fix the bug @mini remux"
        );
        e.custom_name = Some("api".into());
        assert_eq!(row(remote("mini"), e, Some("{title}")).label(), "api");
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

    /// The first panel row painted edge to edge in `sidebar_current_bg`, and
    /// how many ITEMS are (two panel rows each). At most one item may ever be
    /// marked, and a count is the only way to see a second one.
    fn current_bar(
        p: &AgentsPlugin,
        cols: u16,
        rows: u16,
        focused: bool,
    ) -> (Option<usize>, usize) {
        let theme = CompositorTheme::default();
        let grid = p.render(cols, rows + 1, focused, &theme);
        let marked: Vec<usize> = grid
            .iter()
            .enumerate()
            .filter(|(_, r)| r.iter().all(|c| c.bg == theme.sidebar_current_bg))
            .map(|(y, _)| y)
            .collect();
        // A band is two adjacent panel rows: a half-painted one must fail here
        // rather than pass as a smaller count.
        assert!(
            marked.len().is_multiple_of(2)
                && marked.windows(2).step_by(2).all(|w| w[1] == w[0] + 1),
            "the marked rows are not whole two-line bands: {marked:?}"
        );
        // Reported in `painted`'s numbering: the blank header row is not counted.
        (marked.first().map(|y| y - 1), marked.len() / 2)
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

    /// Every row of a real render of the FOCUSED panel, exactly as drawn.
    fn painted_raw(p: &AgentsPlugin, cols: u16, rows: u16) -> Vec<String> {
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

    /// The panel as a list of lines: the header at index 0 and the list from
    /// index 1, each without its padding column.
    ///
    /// A render of a panel one row taller than `rows`, because the header is
    /// followed by a blank row that these tests do not index. That row is
    /// dropped, so `rows` is still the number of lines the caller sees.
    fn painted(p: &AgentsPlugin, cols: u16, rows: u16) -> Vec<String> {
        let mut lines: Vec<String> = painted_raw(p, cols, rows + 1)
            .into_iter()
            .map(|l| l.strip_prefix(' ').unwrap_or(&l).to_string())
            .collect();
        lines.remove(1);
        lines
    }

    #[test]
    fn an_empty_panel_says_so() {
        let p = AgentsPlugin::new();
        let rows = painted(&p, 20, 4);
        assert_eq!(rows[0], "agents");
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
        let rows = painted(&p, 24, 6);
        assert_eq!(rows[1], "\u{25CF} work \u{b7} 0");
        assert_eq!(rows[2], "  claude");
        assert_eq!(rows[3], "\u{25CB} play \u{b7} 0");
        assert_eq!(rows[4], "  codex");
    }

    #[test]
    fn a_remote_agent_is_host_prefixed() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            remote("pi"),
            vec![agent(1, "claude", "main", AgentState::Idle)],
        ));
        let rows = painted(&p, 24, 4);
        assert_eq!(rows[1], "\u{25CB} main \u{b7} 0");
        assert_eq!(rows[2], "  claude \u{b7} pi");
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
        let grid = p.render(20, 4, true, &theme);
        assert_eq!(
            grid[2][1].fg, theme.tab_bell_fg,
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
        let rows = painted(&p, 24, 6);
        assert!(rows[1].contains("here"), "got {:?}", rows[1]);
        assert!(rows[2].contains("claude"), "got {:?}", rows[2]);
        assert!(rows[3].contains("far"), "got {:?}", rows[3]);
        assert!(rows[4].contains("codex \u{b7} pi"), "got {:?}", rows[4]);
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
        let rows = painted(&p, 24, 6);
        assert!(rows[1].contains("here"));
        assert_eq!(rows[3], "", "the remote's row is gone");
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
        let _ = painted(&p, 24, 6);
        let down = MouseEventKind::Down(MouseButton::Left);
        // The second agent's row is two lines; its second line counts too.
        assert_eq!(p.on_mouse(0, 5, down), PluginAction::Redraw);
        assert!(matches!(p.on_mouse(0, 4, down), PluginAction::JumpTo(_)));
        // The header is not a row.
        assert_eq!(p.on_mouse(0, 0, down), PluginAction::None);
    }

    #[test]
    fn an_agent_row_offers_go_to_zoom_and_close_for_that_pane() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            ConnId::Local,
            vec![
                agent(1, "claude", "a", AgentState::Idle),
                agent(2, "claude", "b", AgentState::Idle),
            ],
        ));
        let _ = painted(&p, 24, 6);
        // The second agent's row: rows are two lines tall under a two-row header.
        let items = p.context_menu(0, 5);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Go to pane", "Zoom", "Close pane"]);
        let target = p.rows[1].jump_target();
        assert_eq!(items[0].action, MenuAction::Jump(target.clone()));
        assert_eq!(
            items[1].action,
            MenuAction::JumpThen(
                target,
                Box::new(MenuAction::Command(RemuxCommand::OnPane {
                    pane_id: 2,
                    command: Box::new(RemuxCommand::PaneEnsureZoom),
                }))
            )
        );
        assert!(matches!(
            items[2].action,
            MenuAction::ClosePane { pane_id: 2, .. }
        ));
        assert_eq!(p.nav.selected(), 1, "the press moved the cursor");
        assert!(p.context_menu(0, 0).is_empty(), "the header has no menu");
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
        assert!(rows[1].contains("here"), "got {:?}", rows[1]);
        assert_eq!(rows[3], "mac: no detection", "got {:?}", rows[3]);
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
        assert_eq!(p.on_mouse(0, 4, down), PluginAction::None);
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
        // Header + one two-line row + the note: the list alone would fill this
        // and more.
        let rows = painted(&p, 24, 4);
        assert_eq!(
            rows[3], "local: no detection",
            "a full list must not push the explanation off the panel, got {rows:?}"
        );
        assert!(
            rows[1].contains("s1") && rows[2].contains("claude"),
            "and the row still paints: {rows:?}"
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
        let _ = painted(&p, 24, 6);
        let down = MouseEventKind::Down(MouseButton::Left);
        // Two rows are painted at y=2..5; the note is at y=6.
        assert_eq!(p.on_mouse(0, 4, down), PluginAction::Redraw);
        assert_eq!(
            p.on_mouse(0, 6, down),
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
        // Four blocked agents and three unsupported remotes in a six-row panel.
        let rows = painted(&p, 24, 6);
        let agents_shown = rows[1..].iter().filter(|r| r.contains(" \u{b7} 0")).count();
        assert!(
            agents_shown >= 2,
            "the notes took the panel from the agents: {rows:?}"
        );
        assert_eq!(
            rows[5], "3 servers: no detection",
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
            current_bar(&p, 24, 8, false),
            (Some(3), 1),
            "pane 2 is the second agent, at panel rows 3 and 4, and it is the only mark"
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
        let grid = p.render(24, 7, false, &theme);
        for line in [4, 5] {
            for (x, cell) in grid[line].iter().enumerate() {
                assert_eq!(cell.bg, theme.sidebar_current_bg, "row {line} column {x}");
            }
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
        assert_eq!(current_bar(&p, 24, 8, false).0, Some(3));
        p.on_event(&focus(ConnId::Local, Some(3)));
        assert_eq!(
            current_bar(&p, 24, 8, false),
            (Some(5), 1),
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
        assert_eq!(current_bar(&p, 24, 5, false), (Some(3), 1), "unfocused");
        assert_eq!(current_bar(&p, 24, 5, true), (Some(3), 1), "focused");
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
        let grid = p.render(24, 6, true, &theme);
        for line in [4, 5] {
            for (x, cell) in grid[line].iter().enumerate() {
                assert_eq!(
                    cell.bg, theme.sidebar_selection_bg,
                    "focused selection, row {line} column {x}"
                );
            }
        }
        let grid = p.render(24, 6, false, &theme);
        for line in [4, 5] {
            for (x, cell) in grid[line].iter().enumerate() {
                assert_eq!(
                    cell.bg, theme.sidebar_selection_bg,
                    "unfocused selection, row {line} column {x}"
                );
            }
        }
        assert_eq!(current_bar(&p, 24, 5, true).1, 0);
        p.on_key(key(KeyCode::Char('k')));
        assert_eq!(current_bar(&p, 24, 5, true), (Some(3), 1));
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
        let unmarked = painted(&p, 24, 5)[3].clone();
        p.on_event(&focus(ConnId::Local, Some(2)));
        assert_eq!(current_bar(&p, 24, 5, true).0, Some(3));
        let marked = painted(&p, 24, 5)[3].clone();
        assert_eq!(marked, unmarked, "the bar changes colour, never text");
        assert!(
            marked.starts_with("\u{25CB} work \u{b7} 0"),
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
        assert_eq!(current_bar(&p, 24, 5, false), (Some(3), 1));
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
        let grid = p.render(20, 6, false, &theme);
        assert_eq!(grid[4][1].fg, theme.tab_bell_fg);
        assert_eq!(grid[4][0].bg, theme.sidebar_current_bg);
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
        let grid = p.render(20, 4, false, &theme);
        assert_eq!(grid[2][1].fg, theme.tab_bell_fg);
        assert_eq!(
            grid[2][4].bg, theme.sidebar_selection_bg,
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
        // local pane 1 sits at row 3, off the selection, so a mark that landed
        // on it would be visible.
        p.on_event(&focus(remote("pi"), Some(1)));
        assert_eq!(
            current_bar(&p, 24, 8, false),
            (Some(5), 1),
            "row 5 is the remote's pane 1, not the local one at row 3"
        );
        p.on_event(&PluginEvent::ConnectionLost { conn: remote("pi") });
        assert_eq!(
            current_bar(&p, 24, 8, false).1,
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
        assert_eq!(current_bar(&p, 24, 8, false), (Some(3), 1));
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
        assert_eq!(current_bar(&p, 24, 8, false), (Some(3), 1));
        p.on_event(&focus(remote("pi"), Some(1)));
        assert_eq!(
            current_bar(&p, 24, 8, false),
            (Some(5), 1),
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
        // 6 rows tall: header + two two-line items, showing s9 and s10.
        let rows = painted(&p, 24, 6);
        assert!(
            rows[3].contains("s10"),
            "the selection must be on screen, got {rows:?}"
        );
    }

    fn plugin_with(n: u64) -> AgentsPlugin {
        let mut p = AgentsPlugin::new();
        let agents = (1..=n)
            .map(|i| {
                let state = match i % 3 {
                    0 => AgentState::NeedsInput,
                    1 => AgentState::Working,
                    _ => AgentState::Idle,
                };
                agent(i, "claude", &format!("s{i}"), state)
            })
            .collect();
        p.on_event(&push(ConnId::Local, agents));
        p
    }

    /// The panel row each rendered row index of a flat render band, by
    /// whether every cell carries `sidebar_selection_bg`.
    fn banded_rows(p: &AgentsPlugin, focused: bool, rows: u16) -> Vec<usize> {
        let theme = CompositorTheme::default();
        p.render(24, rows, focused, &theme)
            .iter()
            .enumerate()
            .filter(|(_, r)| r.iter().all(|c| c.bg == theme.sidebar_selection_bg))
            .map(|(y, _)| y)
            .collect()
    }

    #[test]
    fn an_item_takes_two_rows_below_a_two_row_header() {
        let p = plugin_with(3);
        let out = painted_raw(&p, 24, 10);
        assert_eq!(out[0], " agents");
        assert_eq!(out[1], "");
        assert_eq!(out[2], " \u{25CF} s1 \u{b7} 0");
        assert_eq!(out[3], "   claude");
        assert_eq!(out[4], " \u{25CB} s2 \u{b7} 0");
        assert_eq!(out[5], "   claude");
        assert_eq!(out[6], " \u{25CF} s3 \u{b7} 0");
        assert_eq!(out[7], "   claude");
        assert_eq!(out.len(), 10);
    }

    #[test]
    fn the_header_is_lowercase_bold_and_dim() {
        let p = plugin_with(1);
        let theme = CompositorTheme::default();
        let grid = p.render(24, 6, false, &theme);
        assert_eq!(grid[0][1].c, 'a');
        assert!(grid[0][1].bold);
        assert_eq!(grid[0][1].fg, theme.sidebar_dim_fg);
    }

    #[test]
    fn the_selection_band_covers_both_lines_of_the_item() {
        let mut p = plugin_with(3);
        assert_eq!(banded_rows(&p, true, 10), vec![2, 3]);
        p.on_key(key(KeyCode::Char('j')));
        assert_eq!(
            banded_rows(&p, true, 10),
            vec![4, 5],
            "one j moves the band by one item, which is two rows"
        );
        // An unfocused panel still shows where its cursor is.
        assert_eq!(banded_rows(&p, false, 10), vec![4, 5]);
    }

    #[test]
    fn g_and_big_g_move_by_items() {
        let mut p = plugin_with(3);
        p.on_key(key(KeyCode::Char('G')));
        assert_eq!(banded_rows(&p, true, 10), vec![6, 7]);
        p.on_key(key(KeyCode::Char('g')));
        assert_eq!(banded_rows(&p, true, 10), vec![2, 3]);
        p.on_key(key(KeyCode::Char('k')));
        assert_eq!(banded_rows(&p, true, 10), vec![6, 7], "k wraps by one item");
    }

    #[test]
    fn the_selected_name_is_bold_and_bright_only_with_focus() {
        let p = plugin_with(2);
        let theme = CompositorTheme::default();
        let focused = p.render(24, 10, true, &theme);
        assert_eq!(focused[2][3].fg, theme.sidebar_name_selected_fg);
        assert!(focused[2][3].bold);
        assert_eq!(focused[4][3].fg, theme.status_bar_fg);
        assert!(!focused[4][3].bold);
        let idle = p.render(24, 10, false, &theme);
        assert_eq!(idle[2][3].fg, theme.status_bar_fg);
    }

    #[test]
    fn state_dots_keep_the_state_colours_and_idle_is_hollow() {
        let p = plugin_with(3);
        let theme = CompositorTheme::default();
        let g = p.render(24, 10, true, &theme);
        assert_eq!(g[2][1].c, '\u{25CF}');
        assert_eq!(g[2][1].fg, state_fg(AgentState::Working, &theme));
        assert_eq!(g[4][1].c, '\u{25CB}');
        assert_eq!(g[4][1].fg, state_fg(AgentState::Idle, &theme));
        assert_eq!(g[6][1].fg, state_fg(AgentState::NeedsInput, &theme));
    }

    #[test]
    fn a_remote_agent_carries_its_host_on_the_second_line() {
        let mut p = AgentsPlugin::new();
        p.on_event(&push(
            remote("mini"),
            vec![agent(1, "codex", "main", AgentState::Idle)],
        ));
        let out = painted_raw(&p, 24, 6);
        assert_eq!(out[2], " \u{25CB} main \u{b7} 0");
        assert_eq!(out[3], "   codex \u{b7} mini");
    }

    #[test]
    fn scrolling_is_in_items_and_a_partial_item_is_not_drawn() {
        let mut p = plugin_with(5);
        // 2 header rows + 2 items (4 rows) + 1 spare row that cannot hold a
        // third item.
        let rows = 7;
        for _ in 0..3 {
            p.on_key(key(KeyCode::Char('j')));
        }
        let out = painted_raw(&p, 24, rows);
        assert_eq!(out.len(), 7);
        assert!(out[2].contains("s3"), "{out:?}");
        assert!(out[4].contains("s4"), "{out:?}");
        assert_eq!(out[6], "");
        assert_eq!(banded_rows(&p, true, rows), vec![4, 5]);
    }

    #[test]
    fn a_click_on_either_line_of_an_item_selects_it() {
        let down = MouseEventKind::Down(MouseButton::Left);
        for (item, lines) in [(0usize, [2u16, 3]), (1, [4, 5]), (2, [6, 7])] {
            for y in lines {
                let mut p = plugin_with(3);
                let _ = painted(&p, 24, 10);
                // Item 0 starts selected, so a first click on it activates.
                let expect = if item == 0 {
                    PluginAction::JumpTo(p.rows[0].jump_target())
                } else {
                    PluginAction::Redraw
                };
                assert_eq!(p.on_mouse(1, y, down), expect, "item {item} line y={y}");
                assert_eq!(p.nav.selected(), item);
            }
        }
        let mut p = plugin_with(3);
        let _ = painted(&p, 24, 10);
        assert_eq!(p.on_mouse(1, 0, down), PluginAction::None);
        assert_eq!(p.on_mouse(1, 1, down), PluginAction::None, "the blank row");
        assert_eq!(p.on_mouse(1, 8, down), PluginAction::None, "below the list");
    }

    #[test]
    fn enter_on_an_item_jumps_to_its_pane() {
        let mut p = plugin_with(3);
        p.on_key(key(KeyCode::Char('j')));
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            PluginAction::JumpTo(p.rows[1].jump_target())
        );
    }

    fn tree_with_tab(conn: ConnId, session: &str, tab: &str) -> PluginEvent {
        PluginEvent::SessionTree {
            conn,
            folders: Vec::new(),
            dormant: Vec::new(),
            unfiled: vec![crate::protocol::SessionTreeEntry {
                name: session.to_string(),
                tabs: vec![crate::protocol::TabTreeEntry {
                    id: 1,
                    name: tab.to_string(),
                    panes: Vec::new(),
                    is_active: true,
                }],
                client_count: 0,
                is_current: false,
            }],
        }
    }

    #[test]
    fn a_row_names_the_tab_from_the_session_tree() {
        let mut p = plugin_with(1);
        assert_eq!(
            painted_raw(&p, 24, 6)[2],
            " \u{25CF} s1 \u{b7} 0",
            "no tree yet"
        );
        p.on_event(&tree_with_tab(ConnId::Local, "s1", "editor"));
        assert_eq!(painted_raw(&p, 24, 6)[2], " \u{25CF} s1 \u{b7} editor");
        // Another machine's tab of the same session name is not this agent's.
        p.on_event(&tree_with_tab(remote("mini"), "s1", "other"));
        assert_eq!(painted_raw(&p, 24, 6)[2], " \u{25CF} s1 \u{b7} editor");
        p.on_event(&PluginEvent::ConnectionLost {
            conn: ConnId::Local,
        });
        p.on_event(&push(
            ConnId::Local,
            vec![agent(1, "claude", "s1", AgentState::Working)],
        ));
        assert_eq!(
            painted_raw(&p, 24, 6)[2],
            " \u{25CF} s1 \u{b7} 0",
            "a lost connection's tab names are dropped"
        );
    }

    #[test]
    fn the_header_takes_the_active_colour_only_while_the_panel_has_focus() {
        let p = plugin_with(1);
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
        let p = plugin_with(1);
        let theme = CompositorTheme::default();
        for focused in [false, true] {
            let grid = p.render(24, 6, focused, &theme);
            nav::assert_header_bar(&grid, focused, &theme);
        }
    }
}
