//! The navigable-list behaviour every list-shaped sidebar panel shares.
//!
//! Three panels want the same thing: `sessions` (a tree), `agents` (a flat list
//! of panes running an AI agent) and the browser panel after it. "The same
//! thing" is `j`/`k` to move, `g`/`G` to jump to the ends, `Enter` to act, a
//! header row above a window scrolled just far enough to keep the selection on
//! screen, a click that selects and a second click that activates -- and, the
//! part that is easy to get wrong, a selection that survives a refresh by
//! IDENTITY rather than by index.
//!
//! The seam is deliberately split in two, because the two callers are not the
//! same shape and forcing them into one type would distort `sessions`:
//!
//! * **Free functions** ([`nav_key`], [`scroll_offset`], [`hit_test`],
//!   [`row_colors`], [`fill_row`], [`draw_header`]) carry the behaviour. They
//!   take the selection as a parameter, so a panel whose selection lives
//!   somewhere else can still use every one of them. `sessions` is exactly that
//!   panel: its selection is [`TreeModel::selected`], shared with the
//!   session-manager overlay, and moving it out here to satisfy a helper would
//!   split one cursor across two owners.
//!
//!   [`TreeModel::selected`]: crate::client::tree_model::TreeModel::selected
//! * **[`NavList`]** is those functions with a cursor and a viewport attached,
//!   for a panel whose rows are a plain `Vec` and which therefore has no model
//!   to keep the cursor in. `agents` uses it; the browser panel will.
//!
//! Movement WRAPS, because `TreeModel::select_next` wraps and the two panels
//! must not disagree about what `j` on the last row does.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};

use super::{draw_text, PluginAction};
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, RenderCell};

/// Rows of chrome above a panel's list: the header.
pub const HEADER_ROWS: usize = 1;

/// A list command, independent of what the rows mean.
///
/// The mapping from keys to these lives in one place so the panels cannot drift
/// apart; what a row DOES on [`NavKey::Activate`] is each panel's own business,
/// and so is any key this enum has no variant for (`sessions` handles `h`, `l`
/// and `Space` itself, and must, since only a tree can expand).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKey {
    Down,
    Up,
    /// `g`: the first row.
    First,
    /// `G`: the last row.
    Last,
    /// `Enter`: act on the selected row.
    Activate,
}

/// The list command `key` means, or `None` if the list has no use for it.
pub fn nav_key(key: &KeyEvent) -> Option<NavKey> {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => Some(NavKey::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(NavKey::Up),
        KeyCode::Char('g') => Some(NavKey::First),
        KeyCode::Char('G') => Some(NavKey::Last),
        KeyCode::Enter => Some(NavKey::Activate),
        _ => None,
    }
}

/// Move `selected` within a list of `len` rows.
///
/// Wrapping, matching `TreeModel::select_next`/`select_prev`. An empty list
/// parks the cursor at 0, which is where every other path here leaves it.
/// [`NavKey::Activate`] is not movement and is ignored here -- a panel matches
/// on it before calling this.
pub fn move_selection(selected: &mut usize, len: usize, cmd: NavKey) {
    if len == 0 {
        *selected = 0;
        return;
    }
    match cmd {
        NavKey::Down => *selected = (*selected + 1) % len,
        NavKey::Up => {
            *selected = if *selected == 0 {
                len - 1
            } else {
                *selected - 1
            }
        }
        NavKey::First => *selected = 0,
        NavKey::Last => *selected = len - 1,
        NavKey::Activate => {}
    }
}

/// Row index the list window starts at, in a panel `rows` tall.
///
/// Scroll only far enough to keep the selection on screen -- the same formula
/// the session-manager overlay uses (`session_manager.rs`, the `scroll_offset`
/// above its content loop). A pure function of `(selected, rows)`, which is
/// what lets `render(&self)` compute it: a panel is not told its height until
/// it is asked to paint.
pub fn scroll_offset(selected: usize, rows: u16) -> usize {
    let visible = (rows as usize).saturating_sub(HEADER_ROWS);
    if visible == 0 {
        return 0;
    }
    if selected >= visible {
        selected + 1 - visible
    } else {
        0
    }
}

/// What a click at a panel-local row means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// The header, or past the last row: nothing to do.
    Nothing,
    /// Move the selection to this index.
    Select(usize),
    /// A second click on the already-selected row: act on it, the way a double
    /// click does in a file tree.
    Activate(usize),
}

/// Resolve a click at panel-local row `y` against the window the LAST render
/// drew (`top`).
///
/// `on_mouse` is not told the panel's height, and the selection cannot have
/// moved since that paint -- a click is what moves it.
pub fn hit_test(y: u16, top: usize, selected: usize, len: usize) -> Hit {
    let Some(offset) = (y as usize).checked_sub(HEADER_ROWS) else {
        return Hit::Nothing;
    };
    let idx = top + offset;
    if idx >= len {
        Hit::Nothing
    } else if idx == selected {
        Hit::Activate(idx)
    } else {
        Hit::Select(idx)
    }
}

/// Whether `kind` is the press a list acts on.
pub fn is_select_click(kind: MouseEventKind) -> bool {
    matches!(kind, MouseEventKind::Down(MouseButton::Left))
}

/// `(fg, bg)` for one list row.
///
/// An UNFOCUSED panel still marks its selection, dimmer: the row is where the
/// keyboard would land on re-entry, so losing it entirely would make focusing
/// the panel feel like it moved.
pub fn row_colors(
    theme: &CompositorTheme,
    focused: bool,
    selected: bool,
    bg: &CellColor,
) -> (CellColor, CellColor) {
    if !selected {
        return (theme.status_bar_fg.clone(), bg.clone());
    }
    if focused {
        (theme.tab_active_fg.clone(), theme.tab_active_bg.clone())
    } else {
        (theme.tab_inactive_fg.clone(), theme.tab_inactive_bg.clone())
    }
}

/// Paint row `y` edge to edge in `bg`.
///
/// The selection is a full-width bar, not just a highlight behind the text: one
/// that stopped at the label would read as a smear in a column this narrow.
pub fn fill_row(grid: &mut [Vec<RenderCell>], y: u16, cols: u16, fg: &CellColor, bg: &CellColor) {
    draw_text(
        grid,
        0,
        y,
        &" ".repeat(cols as usize),
        fg.clone(),
        bg.clone(),
    );
}

/// Paint a panel's header row.
///
/// A panel's header tracks focus with the SAME theme roles its frame does --
/// that is why they match on screen -- so it asks the same rule rather than
/// restating the choice.
pub fn draw_header(
    grid: &mut [Vec<RenderCell>],
    title: &str,
    focused: bool,
    theme: &CompositorTheme,
    bg: &CellColor,
) {
    let header_fg = crate::server::compositor::border_fg(theme, focused);
    draw_text(grid, 0, 0, title, header_fg, bg.clone());
}

/// A cursor and a viewport over a panel's rows.
///
/// For panels whose rows are a plain `Vec` and so have no model to keep the
/// cursor in. It stores neither the rows nor their length: the panel owns those
/// and passes the length in, which is what keeps this usable by a panel whose
/// rows are rebuilt wholesale on every push.
#[derive(Debug, Default)]
pub struct NavList {
    selected: usize,
    /// The row index the last `render` started its window at.
    ///
    /// A `Cell` rather than a `&mut self` render: this is a record of what was
    /// drawn, not state the panel reasons with, and `render` takes `&self`.
    last_top: Cell<usize>,
}

impl NavList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Point the cursor at `idx`. Clamped by the caller's `len` on the next
    /// [`NavList::reselect`]; nothing here can read past the end because every
    /// reader takes `len`.
    pub fn set_selected(&mut self, idx: usize) {
        self.selected = idx;
    }

    /// Apply a movement command. Returns `false` for [`NavKey::Activate`],
    /// which is not movement, so a caller can tell the two apart.
    pub fn apply(&mut self, cmd: NavKey, len: usize) -> bool {
        if cmd == NavKey::Activate {
            return false;
        }
        move_selection(&mut self.selected, len, cmd);
        true
    }

    /// The window start for a panel `rows` tall, recording it for
    /// [`NavList::hit`].
    pub fn top_for(&self, rows: u16) -> usize {
        let top = scroll_offset(self.selected, rows);
        self.last_top.set(top);
        top
    }

    /// Resolve a click at panel-local row `y` against the last painted window.
    pub fn hit(&self, y: u16, len: usize) -> Hit {
        hit_test(y, self.last_top.get(), self.selected, len)
    }

    /// Re-point the cursor at the row it was on, by IDENTITY, after the rows
    /// were rebuilt.
    ///
    /// `previous` is the key read out BEFORE the rebuild; `keys` are the new
    /// rows' keys in render order. A row that is gone leaves the cursor where it
    /// was, clamped into the new list -- the nearest thing to "where the user
    /// was looking" that still exists.
    ///
    /// Index preservation is the bug this exists to prevent: these panels
    /// refresh on every server push, and a list that kept the INDEX moved the
    /// selection onto a different row whenever one above it appeared or went
    /// away, so `Enter` jumped somewhere the user had not chosen.
    pub fn reselect<K: PartialEq>(&mut self, keys: &[K], previous: Option<&K>) {
        if keys.is_empty() {
            self.selected = 0;
            return;
        }
        if let Some(prev) = previous {
            if let Some(idx) = keys.iter().position(|k| k == prev) {
                self.selected = idx;
                return;
            }
        }
        if self.selected >= keys.len() {
            self.selected = keys.len() - 1;
        }
    }
}

/// The action a panel returns for a click, given the [`Hit`] and its own
/// activation.
///
/// A one-line convenience so the two panels' `on_mouse` bodies stay identical
/// rather than merely similar.
pub fn action_for_hit(
    hit: Hit,
    select: impl FnOnce(usize),
    activate: impl FnOnce() -> PluginAction,
) -> PluginAction {
    match hit {
        Hit::Nothing => PluginAction::None,
        Hit::Select(idx) => {
            select(idx);
            PluginAction::Redraw
        }
        Hit::Activate(_) => activate(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn the_key_vocabulary_is_the_one_both_panels_advertise() {
        assert_eq!(nav_key(&key(KeyCode::Char('j'))), Some(NavKey::Down));
        assert_eq!(nav_key(&key(KeyCode::Down)), Some(NavKey::Down));
        assert_eq!(nav_key(&key(KeyCode::Char('k'))), Some(NavKey::Up));
        assert_eq!(nav_key(&key(KeyCode::Up)), Some(NavKey::Up));
        assert_eq!(nav_key(&key(KeyCode::Char('g'))), Some(NavKey::First));
        assert_eq!(nav_key(&key(KeyCode::Char('G'))), Some(NavKey::Last));
        assert_eq!(nav_key(&key(KeyCode::Enter)), Some(NavKey::Activate));
        assert_eq!(nav_key(&key(KeyCode::Char('q'))), None);
    }

    #[test]
    fn movement_wraps_like_the_tree_model_does() {
        let mut sel = 2;
        move_selection(&mut sel, 3, NavKey::Down);
        assert_eq!(sel, 0, "past the last row wraps to the first");
        move_selection(&mut sel, 3, NavKey::Up);
        assert_eq!(sel, 2, "before the first row wraps to the last");
    }

    #[test]
    fn movement_on_an_empty_list_parks_at_zero() {
        let mut sel = 4;
        move_selection(&mut sel, 0, NavKey::Down);
        assert_eq!(sel, 0);
        move_selection(&mut sel, 0, NavKey::Last);
        assert_eq!(sel, 0);
    }

    #[test]
    fn the_window_scrolls_only_far_enough_to_show_the_selection() {
        // 5 rows tall => 4 list rows under the header.
        assert_eq!(scroll_offset(0, 5), 0);
        assert_eq!(
            scroll_offset(3, 5),
            0,
            "the last visible row needs no scroll"
        );
        assert_eq!(scroll_offset(4, 5), 1);
        assert_eq!(scroll_offset(9, 5), 6);
        // A panel with no room for rows at all still answers.
        assert_eq!(scroll_offset(9, 1), 0);
        assert_eq!(scroll_offset(9, 0), 0);
    }

    #[test]
    fn a_click_selects_and_a_second_click_activates() {
        assert_eq!(
            hit_test(0, 0, 0, 3),
            Hit::Nothing,
            "the header is not a row"
        );
        assert_eq!(hit_test(1, 0, 2, 3), Hit::Select(0));
        assert_eq!(hit_test(3, 0, 2, 3), Hit::Activate(2));
        assert_eq!(hit_test(9, 0, 0, 3), Hit::Nothing, "past the last row");
        assert_eq!(
            hit_test(1, 5, 0, 9),
            Hit::Select(5),
            "resolved against the window"
        );
    }

    #[test]
    fn the_selection_follows_its_row_when_one_above_it_goes_away() {
        let mut nav = NavList::new();
        let before = ["a", "b", "c"];
        nav.set_selected(2);
        // "a" disappears: the row the user was on is now index 1.
        nav.reselect(&["b", "c"], Some(&before[2]));
        assert_eq!(nav.selected(), 1);
    }

    #[test]
    fn a_selection_whose_row_is_gone_clamps_rather_than_jumping() {
        let mut nav = NavList::new();
        nav.set_selected(2);
        nav.reselect(&["x", "y"], Some(&"c"));
        assert_eq!(nav.selected(), 1, "clamped into the shorter list");

        let mut nav = NavList::new();
        nav.set_selected(0);
        nav.reselect(&["x", "y"], Some(&"c"));
        assert_eq!(nav.selected(), 0, "still in range: left alone");
    }

    #[test]
    fn an_emptied_list_parks_the_cursor_at_zero() {
        let mut nav = NavList::new();
        nav.set_selected(4);
        nav.reselect::<&str>(&[], None);
        assert_eq!(nav.selected(), 0);
    }

    #[test]
    fn a_click_is_resolved_against_the_window_the_last_paint_used() {
        let nav = NavList::new();
        // A 4-row panel showing rows 0..3 of a 20-row list, selection at 0.
        assert_eq!(nav.top_for(4), 0);
        assert_eq!(nav.hit(2, 20), Hit::Select(1));

        let mut nav = NavList::new();
        nav.set_selected(10);
        assert_eq!(nav.top_for(4), 8, "scrolled to show row 10");
        assert_eq!(
            nav.hit(1, 20),
            Hit::Select(8),
            "the top visible row is row 8"
        );
    }
}
