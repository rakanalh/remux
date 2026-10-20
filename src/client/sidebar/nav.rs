//! The navigable-list behaviour every list-shaped sidebar panel shares.
//!
//! Three panels want the same thing: `sessions` (a tree), `agents` (a flat list
//! of panes running an AI agent) and the `files` panel after it. "The same
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
//!   panel: its selection is [`TreeModel::selected`], and giving it a [`NavList`]
//!   as well would put **two reconcilers on one cursor**.
//!
//!   `TreeModel` already does [`NavList`]'s job. It captures the selected row's
//!   `row_key` before a rebuild and re-points by identity, clamping if the row
//!   is gone -- the very thing [`NavList::reselect`] exists for -- and
//!   `expand_selected`, `collapse_selected`, `toggle_expand` and the query
//!   filter all move the cursor too. A second reconciler would disagree with
//!   that one the first time a push rebuilt the tree, and the panel would jump.
//!
//!   (The session-manager overlay uses the same TYPE with its own instance;
//!   there is no shared cursor between the two surfaces. An earlier version of
//!   this comment claimed there was, which sent readers looking for a handle
//!   that does not exist.)
//!
//!   [`TreeModel::selected`]: crate::client::tree_model::TreeModel::selected
//! * **[`NavList`]** is those functions with a cursor and a viewport attached,
//!   for a panel whose rows are a plain `Vec` and which therefore has no model
//!   to keep the cursor in. Both `agents` and `files` use it.
//!
//! Movement WRAPS, because `TreeModel::select_next` wraps and the two panels
//! must not disagree about what `j` on the last row does.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};

use super::{draw_text, draw_text_with};
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, RenderCell};

/// Rows of chrome above a panel's list: the header.
pub const HEADER_ROWS: usize = 1;

/// Where a list's rows start and how tall each one is.
///
/// Every sidebar style shares one layout: a header row plus a blank row, one
/// column of left padding, and rows whose height belongs to the panel (the
/// agents panel's are two lines). [`ListLayout::PLAIN`] is the older shape the
/// plain functions below assume (a one-row header, no padding, one-line rows),
/// and a bare [`NavList`] starts with it. The selection, the scroll offset and
/// every count in the `NavList` are in ITEMS, never in screen rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListLayout {
    pub header_rows: usize,
    pub row_height: usize,
    pad: u16,
}

impl ListLayout {
    pub const PLAIN: Self = Self {
        header_rows: HEADER_ROWS,
        row_height: 1,
        pad: 0,
    };

    /// The layout the sidebar panels draw with, for rows `row_height` lines
    /// tall.
    pub fn new(row_height: usize) -> Self {
        Self {
            header_rows: LIST_HEADER_ROWS,
            row_height: row_height.max(1),
            pad: 1,
        }
    }

    /// Columns of left padding before any content.
    pub fn pad(&self) -> u16 {
        self.pad
    }

    /// How many whole items fit in `rows` rows below the header.
    pub fn visible(&self, rows: u16) -> usize {
        (rows as usize).saturating_sub(self.header_rows) / self.row_height
    }

    /// The panel row an item `i` places in the window (`i` counted from the
    /// window's first item) starts on.
    pub fn row_of(&self, i: usize) -> u16 {
        (self.header_rows + i * self.row_height) as u16
    }
}

/// A panel's header: the title row and one blank row.
pub const LIST_HEADER_ROWS: usize = 2;

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
#[cfg(test)]
pub fn scroll_offset(selected: usize, rows: u16) -> usize {
    scroll_offset_in(ListLayout::PLAIN, selected, rows)
}

/// [`scroll_offset`] for a given layout. `selected` and the result are item
/// indices.
pub fn scroll_offset_in(layout: ListLayout, selected: usize, rows: u16) -> usize {
    let visible = layout.visible(rows);
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
#[cfg(test)]
pub fn hit_test(y: u16, top: usize, selected: usize, len: usize) -> Hit {
    hit_test_in(ListLayout::PLAIN, y, top, selected, len)
}

/// [`hit_test`] for a given layout: any line of a multi-line row hits that row.
pub fn hit_test_in(layout: ListLayout, y: u16, top: usize, selected: usize, len: usize) -> Hit {
    let Some(offset) = (y as usize).checked_sub(layout.header_rows) else {
        return Hit::Nothing;
    };
    let idx = top + offset / layout.row_height;
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

/// `(fg, bg, bold)` for one list row.
///
/// The selection keeps its focused/unfocused distinction: the band is always
/// painted, and the name takes the bright selected colour only while the panel
/// holds the keyboard.
pub fn row_colors(
    theme: &CompositorTheme,
    focused: bool,
    selected: bool,
    bg: &CellColor,
) -> (CellColor, CellColor, bool) {
    if !selected {
        return (theme.status_bar_fg.clone(), bg.clone(), false);
    }
    let fg = if focused {
        theme.sidebar_name_selected_fg.clone()
    } else {
        theme.status_bar_fg.clone()
    };
    (fg, theme.sidebar_selection_bg.clone(), true)
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

/// The colour of a panel's header title: its own accent while the panel holds
/// the keyboard, so the user can tell which of several stacked panels takes
/// their keys, and the dim colour otherwise.
fn header_fg(focused: bool, theme: &CompositorTheme) -> CellColor {
    if focused {
        theme.sidebar_header_active_fg.clone()
    } else {
        theme.sidebar_dim_fg.clone()
    }
}

/// The background of a panel's header bar.
fn header_bg(focused: bool, theme: &CompositorTheme) -> CellColor {
    if focused {
        theme.sidebar_header_active_bg.clone()
    } else {
        theme.sidebar_header_bg.clone()
    }
}

/// Paint a panel's header bar: row 0 filled edge to edge in [`header_bg`], with
/// `text` bold from column `x`.
///
/// The bar spans the same extent as the selection band, and `text` is drawn as
/// given, so a panel whose header is not a fixed name (the files panel's path)
/// sits on the same bar.
pub fn draw_header_bar(
    grid: &mut [Vec<RenderCell>],
    x: u16,
    text: &str,
    focused: bool,
    theme: &CompositorTheme,
) {
    let Some(cols) = grid.first().map(Vec::len) else {
        return;
    };
    let fg = header_fg(focused, theme);
    let bg = header_bg(focused, theme);
    fill_row(grid, 0, cols as u16, &fg, &bg);
    draw_text_with(grid, x, 0, text, fg, bg, true);
}

/// Paint a panel's header: the title in lowercase and bold, one column in, on
/// the header bar.
pub fn draw_header(
    grid: &mut [Vec<RenderCell>],
    title: &str,
    focused: bool,
    theme: &CompositorTheme,
) {
    draw_header_bar(grid, 1, &title.to_lowercase(), focused, theme);
}

/// A cursor and a viewport over a panel's rows.
///
/// For panels whose rows are a plain `Vec` and so have no model to keep the
/// cursor in. It stores neither the rows nor their length: the panel owns those
/// and passes the length in, which is what keeps this usable by a panel whose
/// rows are rebuilt wholesale on every push.
#[derive(Debug)]
pub struct NavList {
    selected: usize,
    /// The row index the last `render` started its window at.
    ///
    /// A `Cell` rather than a `&mut self` render: this is a record of what was
    /// drawn, not state the panel reasons with, and `render` takes `&self`.
    last_top: Cell<usize>,
    /// How many rows that render actually PAINTED.
    ///
    /// Recorded because "is there a row under this click?" is not answerable
    /// from the list's length: a panel may paint fewer rows than it holds (it
    /// ran out of height) and may put something else -- a note, a footer -- in
    /// the space below them. Bounding the hit test by the length alone would
    /// resolve a click on that something else to whatever row index happened to
    /// sit at the same offset.
    last_shown: Cell<usize>,
    layout: ListLayout,
}

impl Default for NavList {
    fn default() -> Self {
        Self {
            selected: 0,
            last_top: Cell::new(0),
            last_shown: Cell::new(0),
            layout: ListLayout::PLAIN,
        }
    }
}

impl NavList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_layout(&mut self, layout: ListLayout) {
        self.layout = layout;
    }

    pub fn layout(&self) -> ListLayout {
        self.layout
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

    /// The window start for a panel `rows` tall holding `len` rows, recording
    /// what this paint will show for [`NavList::hit`].
    pub fn top_for(&self, rows: u16, len: usize) -> usize {
        let top = scroll_offset_in(self.layout, self.selected, rows);
        let capacity = self.layout.visible(rows);
        self.last_top.set(top);
        self.last_shown.set(len.saturating_sub(top).min(capacity));
        top
    }

    /// Resolve a click at panel-local row `y` against the last painted window.
    ///
    /// Bounded by BOTH what was painted and what still exists, and it needs both:
    ///
    /// * by the paint, because a panel may draw something else below its rows
    ///   (the agents panel draws a "this server cannot detect" note there) and a
    ///   click on that must select nothing;
    /// * by `len`, because a click can arrive between a rebuild that SHRANK the
    ///   list and the next paint. The recorded window is then stale and describes
    ///   rows that no longer exist -- selecting one leaves the cursor past the
    ///   end, where the next paint scrolls to a window with nothing in it and the
    ///   panel goes blank. It self-heals on any later push or keypress, except
    ///   that a list of entirely idle agents produces no pushes, so nothing is
    ///   coming.
    pub fn hit(&self, y: u16, len: usize) -> Hit {
        let painted = self.last_top.get() + self.last_shown.get();
        hit_test_in(
            self.layout,
            y,
            self.last_top.get(),
            self.selected,
            painted.min(len),
        )
    }

    /// Resolve a click at panel-local row `y`, move the cursor, and say what is
    /// left to do. [`NavList::hit`] paired with [`action_for_hit`].
    pub fn click(&mut self, y: u16, len: usize) -> HitOutcome {
        let hit = self.hit(y, len);
        action_for_hit(hit, &mut self.selected)
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

/// What a panel still has to do about a click, once the cursor has been moved.
///
/// An INTENT rather than a `PluginAction`, and that is the whole reason this
/// pair works at all. The obvious shape -- one function taking a `select`
/// closure and an `activate` closure -- does not compile in any real caller:
/// both closures need unique access to `*self`, so the second borrow is
/// rejected (`E0524`). Handing the caller back a verdict and letting it call its
/// own `self.activate()` after the borrow of the cursor has ended is not a
/// workaround for that; it is the arrangement that has no borrow conflict to
/// work around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitOutcome {
    /// The click landed on no row. The panel does nothing.
    Ignore,
    /// The cursor moved to the clicked row. The panel repaints.
    Moved,
    /// The click was a second one on the already-selected row: activate it.
    Activate,
}

/// Apply a [`Hit`] to a panel's cursor and say what is left to do.
///
/// Takes `&mut usize` rather than a `NavList` so it serves the `sessions` panel
/// too, whose cursor lives in its `TreeModel` -- the same split the rest of this
/// module is built on. [`NavList::click`] is the one-liner for panels that do
/// own a `NavList`.
pub fn action_for_hit(hit: Hit, selected: &mut usize) -> HitOutcome {
    match hit {
        Hit::Nothing => HitOutcome::Ignore,
        Hit::Select(idx) => {
            *selected = idx;
            HitOutcome::Moved
        }
        Hit::Activate(_) => HitOutcome::Activate,
    }
}

/// Test helper shared by every panel: row 0 is one bar edge to edge in the
/// state's bar colour with the title in the state's fg, row 1 has no bar.
#[cfg(test)]
pub fn assert_header_bar(grid: &[Vec<RenderCell>], focused: bool, theme: &CompositorTheme) {
    let (want_bg, want_fg) = if focused {
        (
            &theme.sidebar_header_active_bg,
            &theme.sidebar_header_active_fg,
        )
    } else {
        (&theme.sidebar_header_bg, &theme.sidebar_dim_fg)
    };
    for (x, cell) in grid[0].iter().enumerate() {
        assert_eq!(&cell.bg, want_bg, "header cell {x} (focused={focused})");
    }
    let title = grid[0].iter().position(|c| c.c != ' ').expect("a title");
    assert_eq!(&grid[0][title].fg, want_fg);
    assert!(grid[0][title].bold);
    for (x, cell) in grid[1].iter().enumerate() {
        assert_ne!(&cell.bg, want_bg, "blank row cell {x} carries the bar");
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

    /// The click primitive all three list panels now share.
    ///
    /// It was dead for a whole phase, and not by accident: its previous
    /// signature took two closures, both of which needed unique access to
    /// `*self` in any real caller, so it did not COMPILE when used (`E0524`).
    /// `#![allow(dead_code)]` on the crate root kept the unused-function warning
    /// from ever saying so. Returning a verdict instead is what made it usable,
    /// so this test is as much about the shape as the behaviour.
    #[test]
    fn a_click_moves_the_cursor_and_hands_activation_back_to_the_caller() {
        let mut selected = 3usize;
        assert_eq!(
            action_for_hit(Hit::Nothing, &mut selected),
            HitOutcome::Ignore
        );
        assert_eq!(selected, 3, "a click on nothing must not move the cursor");

        assert_eq!(
            action_for_hit(Hit::Select(1), &mut selected),
            HitOutcome::Moved
        );
        assert_eq!(selected, 1, "a click on a row selects it");

        assert_eq!(
            action_for_hit(Hit::Activate(1), &mut selected),
            HitOutcome::Activate
        );
        assert_eq!(
            selected, 1,
            "activation leaves the cursor alone; the caller acts on it"
        );
    }

    /// `NavList::click` is the same thing, bounded by what was painted.
    #[test]
    fn navlist_click_selects_within_the_painted_window_and_ignores_below_it() {
        let mut nav = NavList::new();
        // Paint a 6-row panel holding 3 rows, so rows occupy the window and the
        // space below them belongs to whatever else the panel draws there.
        nav.top_for(6, 3);
        assert_eq!(nav.click(1 + HEADER_ROWS as u16, 3), HitOutcome::Moved);
        assert_eq!(nav.selected(), 1);
        // A second click on the SAME row activates rather than re-selecting.
        assert_eq!(nav.click(1 + HEADER_ROWS as u16, 3), HitOutcome::Activate);
        assert_eq!(nav.selected(), 1);
        // Below the painted rows: nothing.
        assert_eq!(nav.click(5 + HEADER_ROWS as u16, 3), HitOutcome::Ignore);
        assert_eq!(nav.selected(), 1);
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
        assert_eq!(nav.top_for(4, 20), 0);
        assert_eq!(nav.hit(2, 20), Hit::Select(1));

        let mut nav = NavList::new();
        nav.set_selected(10);
        assert_eq!(nav.top_for(4, 20), 8, "scrolled to show row 10");
        assert_eq!(
            nav.hit(1, 20),
            Hit::Select(8),
            "the top visible row is row 8"
        );
    }

    /// A click below the last PAINTED row is not a row, even when the list has
    /// more rows that a taller panel would have shown there. A panel may put
    /// something else in that space -- the agents panel puts its "this server
    /// cannot detect" note there -- and a click on it must select nothing.
    #[test]
    fn a_click_past_the_painted_rows_hits_nothing() {
        let nav = NavList::new();
        // A 3-row panel: header + 2 rows, out of a 20-row list.
        nav.top_for(3, 20);
        assert_eq!(nav.hit(1, 20), Hit::Activate(0));
        assert_eq!(nav.hit(2, 20), Hit::Select(1));
        assert_eq!(
            nav.hit(3, 20),
            Hit::Nothing,
            "row 2 was never painted; whatever is drawn there is not this list"
        );
    }

    #[test]
    fn a_panel_shorter_than_its_list_reports_only_what_it_painted() {
        let nav = NavList::new();
        nav.top_for(5, 2); // room for 4 rows, only 2 exist
        assert_eq!(nav.hit(2, 2), Hit::Select(1));
        assert_eq!(nav.hit(3, 2), Hit::Nothing);
    }

    /// A click landing between a shrinking rebuild and the next paint. The
    /// recorded window still describes twenty rows; five remain.
    #[test]
    fn a_click_against_a_stale_window_cannot_select_past_the_end() {
        let mut nav = NavList::new();
        nav.set_selected(19);
        assert_eq!(
            nav.top_for(5, 20),
            16,
            "scrolled to the end of a 20-row list"
        );
        // The list shrinks to five. No repaint yet, so the window is stale.
        assert_eq!(
            nav.hit(4, 5),
            Hit::Nothing,
            "row 19 is gone; selecting it would park the cursor past the end and \
             paint an empty panel"
        );
        // The rows that DO still exist are unaffected by the staleness.
        nav.set_selected(0);
        assert_eq!(nav.top_for(5, 5), 0);
        assert_eq!(nav.hit(2, 5), Hit::Select(1));
    }

    #[test]
    fn the_header_bar_spans_the_row_and_follows_focus() {
        let theme = CompositorTheme::default();
        for focused in [false, true] {
            let mut grid = vec![vec![RenderCell::default(); 12]; 3];
            draw_header(&mut grid, "Agents", focused, &theme);
            assert_header_bar(&grid, focused, &theme);
            assert_eq!(grid[0][1].c, 'a', "lowercased, one column in");
            assert!(
                grid[2].iter().all(|c| c.bg == RenderCell::default().bg),
                "rows below the blank row are untouched"
            );
        }
        assert_ne!(
            header_bg(true, &theme),
            header_bg(false, &theme),
            "focus must change the bar"
        );
    }
}
