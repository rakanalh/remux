//! The views panel: the local server's shared Views, with the one this client
//! has on screen marked.
//!
//! The list behaviour (`j`/`k`, `g`/`G`, `Enter`, the scrolled window, the
//! click-then-click-again activation, and the selection kept by IDENTITY across
//! a refresh) is [`super::nav`], as in the agents panel. `Enter` asks the client
//! for [`PluginAction::EnterView`], which goes through the client's one entry
//! path, so there is no second "enter a view" implementation here.
//!
//! The mark is fed by [`PluginEvent::Views`], never derived from the list's
//! timing: entering, leaving and switching views are client-side and no
//! `ViewList` arrives for them, so the client re-sends the event with the new
//! `active` at every change of the view it displays.

use crossterm::event::{KeyEvent, MouseEventKind};

use super::nav::{self, ListLayout, NavKey, NavList};
use super::{
    blank_grid, draw_text, draw_text_with, fit, PluginAction, PluginEvent, SidebarPlugin,
    ViewRowInfo,
};
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, RenderCell, ViewId};

const ROW_HEIGHT: usize = 2;

/// The second line of a row: the cell count and the layout.
fn row_detail(row: &ViewRowInfo) -> String {
    let cells = match row.cells {
        1 => "1 cell".to_string(),
        n => format!("{n} cells"),
    };
    let detail = format!("{cells} \u{b7} {}", row.layout);
    if row.zoomed {
        format!("{detail} \u{b7} zoomed")
    } else {
        detail
    }
}

pub struct ViewsPlugin {
    rows: Vec<ViewRowInfo>,
    /// The view this client has on screen, by id. The rows are replaced on every
    /// event, so an index would follow whichever view slid into that slot.
    current: Option<ViewId>,
    nav: NavList,
}

impl ViewsPlugin {
    pub fn new() -> Self {
        let mut nav = NavList::new();
        nav.set_layout(ListLayout::new(ROW_HEIGHT));
        Self {
            rows: Vec::new(),
            current: None,
            nav,
        }
    }

    fn layout(&self) -> ListLayout {
        self.nav.layout()
    }

    fn replace(&mut self, rows: &[ViewRowInfo], active: Option<ViewId>) {
        let previous = self.rows.get(self.nav.selected()).map(|r| r.id);
        self.rows = rows.to_vec();
        self.current = active;
        let keys: Vec<ViewId> = self.rows.iter().map(|r| r.id).collect();
        self.nav.reselect(&keys, previous.as_ref());
    }

    fn activate(&self) -> PluginAction {
        match self.rows.get(self.nav.selected()) {
            Some(row) => PluginAction::EnterView(row.id),
            None => PluginAction::None,
        }
    }

    fn paint_row(
        grid: &mut [Vec<RenderCell>],
        y: u16,
        row: &ViewRowInfo,
        style: &RowStyle,
        theme: &CompositorTheme,
    ) {
        let RowStyle {
            cols,
            pad,
            band,
            name_fg,
            bold,
            current,
            base_bg,
        } = style;
        let (cols, pad, bold) = (*cols, *pad, *bold);
        let bg = band.as_ref().unwrap_or(base_bg).clone();
        if band.is_some() {
            for line in [y, y + 1] {
                nav::fill_row(grid, line, cols, name_fg, &bg);
            }
        }
        // Every row reserves the marker's two columns, so the names stay in one
        // column whether or not a row is marked.
        if *current {
            draw_text_with(grid, pad, y, "*", name_fg.clone(), bg.clone(), true);
        }
        let text_x = pad + 2;
        draw_text_with(
            grid,
            text_x,
            y,
            &row.name,
            name_fg.clone(),
            bg.clone(),
            bold,
        );
        let room = usize::from(cols.saturating_sub(text_x));
        draw_text(
            grid,
            text_x,
            y + 1,
            &fit(&row_detail(row), room),
            theme.sidebar_dim_fg.clone(),
            bg,
        );
    }
}

/// How one row is painted.
struct RowStyle {
    cols: u16,
    pad: u16,
    /// The band behind both lines, when the row is selected or current.
    band: Option<CellColor>,
    name_fg: CellColor,
    bold: bool,
    /// Whether this is the view on screen, which draws the `*`.
    current: bool,
    /// The panel background, behind a row without a band.
    base_bg: CellColor,
}

impl Default for ViewsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SidebarPlugin for ViewsPlugin {
    fn title(&self) -> &str {
        "Views"
    }

    fn min_size(&self) -> (u16, u16) {
        (12, (self.layout().header_rows + ROW_HEIGHT) as u16)
    }

    fn wants_views(&self) -> bool {
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
        if self.rows.is_empty() {
            draw_text(
                &mut grid,
                layout.pad(),
                layout.header_rows as u16,
                "no views",
                theme.status_bar_fg.clone(),
                bg,
            );
            return grid;
        }
        let top = self.nav.top_for(rows, self.rows.len());
        for i in 0..layout.visible(rows) {
            let Some(row) = self.rows.get(top + i) else {
                break;
            };
            let selected = top + i == self.nav.selected();
            let current = self.current == Some(row.id);
            let (fg, sel_bg, bold) = nav::row_colors(theme, focused, selected, &bg);
            // The selection band wins on a row that is both, so that the j/k
            // cursor is never hidden. The `*` still marks the current view there.
            let band = if selected {
                Some(sel_bg)
            } else if current {
                Some(theme.sidebar_current_bg.clone())
            } else {
                None
            };
            let style = RowStyle {
                cols,
                pad: layout.pad(),
                band,
                name_fg: fg,
                bold,
                current,
                base_bg: bg.clone(),
            };
            Self::paint_row(&mut grid, layout.row_of(i), row, &style, theme);
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
            PluginEvent::Views { views, active } => self.replace(views, *active),
            PluginEvent::SessionTree { .. }
            | PluginEvent::Agents { .. }
            | PluginEvent::PaneTitle { .. }
            | PluginEvent::ConnectionLost { .. }
            | PluginEvent::FocusedCwd { .. }
            | PluginEvent::FocusedPane { .. }
            | PluginEvent::DirectoryListing { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers, MouseButton};

    fn view(id: ViewId, name: &str, cells: usize) -> ViewRowInfo {
        ViewRowInfo {
            id,
            name: name.to_string(),
            cells,
            layout: "grid".to_string(),
            zoomed: false,
        }
    }

    fn push(p: &mut ViewsPlugin, views: Vec<ViewRowInfo>, active: Option<ViewId>) {
        p.on_event(&PluginEvent::Views { views, active });
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn click(p: &mut ViewsPlugin, y: u16) -> PluginAction {
        p.on_mouse(3, y, MouseEventKind::Down(MouseButton::Left))
    }

    /// The panel's lines with the trailing blanks trimmed, header at index 0.
    fn painted(p: &ViewsPlugin, cols: u16, rows: u16) -> Vec<String> {
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

    /// The panel rows painted edge to edge in `bg`.
    fn rows_in(p: &ViewsPlugin, focused: bool, bg: &CellColor) -> Vec<usize> {
        let theme = CompositorTheme::default();
        p.render(24, 10, focused, &theme)
            .iter()
            .enumerate()
            .filter(|(_, r)| r.iter().all(|c| &c.bg == bg))
            .map(|(y, _)| y)
            .collect()
    }

    fn current_rows(p: &ViewsPlugin) -> Vec<usize> {
        rows_in(p, false, &CompositorTheme::default().sidebar_current_bg)
    }

    fn selected_rows(p: &ViewsPlugin) -> Vec<usize> {
        rows_in(p, false, &CompositorTheme::default().sidebar_selection_bg)
    }

    #[test]
    fn an_empty_panel_says_so() {
        let p = ViewsPlugin::new();
        let lines = painted(&p, 20, 4);
        assert_eq!(lines[0], " views");
        assert_eq!(lines[2], " no views");
    }

    #[test]
    fn each_view_is_a_two_line_row_with_its_count_and_layout() {
        let mut p = ViewsPlugin::new();
        let mut zoomed = view(3, "gamma", 0);
        zoomed.zoomed = true;
        zoomed.layout = "custom".into();
        push(
            &mut p,
            vec![view(1, "alpha", 2), view(2, "beta", 1), zoomed],
            None,
        );
        let lines = painted(&p, 30, 10);
        assert_eq!(lines[2], "   alpha");
        assert_eq!(lines[3], "   2 cells \u{b7} grid");
        assert_eq!(lines[4], "   beta");
        assert_eq!(lines[5], "   1 cell \u{b7} grid");
        assert_eq!(lines[6], "   gamma");
        assert_eq!(lines[7], "   0 cells \u{b7} custom \u{b7} zoomed");
    }

    #[test]
    fn the_active_view_is_starred_and_banded_and_no_other_is() {
        let mut p = ViewsPlugin::new();
        push(
            &mut p,
            vec![view(1, "alpha", 2), view(2, "beta", 2)],
            Some(2),
        );
        let lines = painted(&p, 24, 8);
        assert_eq!(lines[2], "   alpha");
        assert_eq!(lines[4], " * beta");
        assert_eq!(current_rows(&p), vec![4, 5]);
    }

    #[test]
    fn no_active_view_marks_nothing() {
        let mut p = ViewsPlugin::new();
        push(
            &mut p,
            vec![view(1, "alpha", 2), view(2, "beta", 2)],
            Some(2),
        );
        push(&mut p, vec![view(1, "alpha", 2), view(2, "beta", 2)], None);
        let lines = painted(&p, 24, 8);
        assert!(lines.iter().all(|l| !l.contains('*')), "{lines:?}");
        assert!(current_rows(&p).is_empty());
    }

    #[test]
    fn an_active_id_with_no_row_marks_nothing() {
        let mut p = ViewsPlugin::new();
        push(&mut p, vec![view(1, "alpha", 2)], Some(9));
        assert!(painted(&p, 24, 6).iter().all(|l| !l.contains('*')));
        assert!(current_rows(&p).is_empty());
    }

    #[test]
    fn the_selection_band_wins_on_the_active_row_but_the_star_stays() {
        let mut p = ViewsPlugin::new();
        push(
            &mut p,
            vec![view(1, "alpha", 2), view(2, "beta", 2)],
            Some(1),
        );
        assert_eq!(selected_rows(&p), vec![2, 3]);
        assert!(current_rows(&p).is_empty());
        assert_eq!(painted(&p, 24, 8)[2], " * alpha");
    }

    #[test]
    fn the_selection_follows_its_view_when_one_above_is_deleted() {
        let mut p = ViewsPlugin::new();
        push(
            &mut p,
            vec![view(1, "a", 0), view(2, "b", 0), view(3, "c", 0)],
            None,
        );
        p.on_key(key(KeyCode::Char('j')));
        push(&mut p, vec![view(2, "b", 0), view(3, "c", 0)], None);
        // Keeping index 1 would land on `c`: only the identity names `b`.
        assert_eq!(p.on_key(key(KeyCode::Enter)), PluginAction::EnterView(2));
    }

    #[test]
    fn a_rename_alone_does_not_move_the_selection() {
        let mut p = ViewsPlugin::new();
        push(&mut p, vec![view(1, "a", 0), view(2, "b", 0)], None);
        p.on_key(key(KeyCode::Char('j')));
        push(&mut p, vec![view(1, "a", 0), view(2, "renamed", 0)], None);
        assert_eq!(p.on_key(key(KeyCode::Enter)), PluginAction::EnterView(2));
        assert_eq!(painted(&p, 24, 8)[4], "   renamed");
    }

    #[test]
    fn enter_asks_for_the_selected_view_and_nothing_on_an_empty_list() {
        let mut p = ViewsPlugin::new();
        assert_eq!(p.on_key(key(KeyCode::Enter)), PluginAction::None);
        push(&mut p, vec![view(4, "a", 0), view(7, "b", 0)], None);
        assert_eq!(p.on_key(key(KeyCode::Enter)), PluginAction::EnterView(4));
        p.on_key(key(KeyCode::Char('j')));
        assert_eq!(p.on_key(key(KeyCode::Enter)), PluginAction::EnterView(7));
    }

    #[test]
    fn a_second_click_on_a_row_enters_it() {
        let mut p = ViewsPlugin::new();
        push(&mut p, vec![view(4, "a", 0), view(7, "b", 0)], None);
        painted(&p, 24, 10);
        assert_eq!(click(&mut p, 4), PluginAction::Redraw);
        assert_eq!(click(&mut p, 5), PluginAction::EnterView(7));
    }

    #[test]
    fn it_wants_views_and_nothing_else() {
        let p = ViewsPlugin::new();
        assert!(p.wants_views());
        assert!(!p.wants_agents());
        assert!(!p.wants_session_tree());
    }

    #[test]
    fn the_header_is_a_bar_whose_colours_follow_focus() {
        let mut p = ViewsPlugin::new();
        push(&mut p, vec![view(1, "a", 0)], None);
        let theme = CompositorTheme::default();
        for focused in [false, true] {
            let grid = p.render(24, 6, focused, &theme);
            nav::assert_header_bar(&grid, focused, &theme);
        }
    }
}
