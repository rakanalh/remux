//! Client-side popup terminal: where it sits, how it is framed, and the layer
//! the server feeds it.
//!
//! The server only runs the popup's PTY. The client decides the box, draws the
//! frame itself (so the popup can sit over its own sidebars, which the server's
//! composite never contains) and tells the server the interior size it wants
//! with [`crate::protocol::ClientMessage::PopupSize`].

use std::collections::HashMap;

use crate::client::registry::ConnId;
use crate::config::{Config, PopupPosition};
use crate::protocol::{PaneId, PopupCells, RenderCell};
use crate::server::compositor::{
    border_fg, draw_zellij_border, draw_zellij_border_open, fits_zellij_border, OpenEdge,
};
use crate::server::layout::{
    clamp_popup_size, Rect, POPUP_MAX_PCT, POPUP_MIN_INTERIOR_COLS, POPUP_MIN_INTERIOR_ROWS,
    POPUP_MIN_PCT,
};

/// Which of the box's edges are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupFrame {
    /// All four edges, rounded corners: a float.
    Full,
    /// A drawer docked to the top: no top edge.
    OpenTop,
    /// A drawer docked to the bottom: no bottom edge.
    OpenBottom,
    /// A drawer docked to the left: no left edge.
    OpenLeft,
    /// A drawer docked to the right: no right edge.
    OpenRight,
}

impl PopupFrame {
    fn of(position: PopupPosition) -> Self {
        match position {
            PopupPosition::Float => PopupFrame::Full,
            PopupPosition::Top => PopupFrame::OpenTop,
            PopupPosition::Bottom => PopupFrame::OpenBottom,
            PopupPosition::Left => PopupFrame::OpenLeft,
            PopupPosition::Right => PopupFrame::OpenRight,
        }
    }

    /// The edge left open, `None` for a full box.
    fn open_edge(self) -> Option<OpenEdge> {
        match self {
            PopupFrame::Full => None,
            PopupFrame::OpenTop => Some(OpenEdge::Top),
            PopupFrame::OpenBottom => Some(OpenEdge::Bottom),
            PopupFrame::OpenLeft => Some(OpenEdge::Left),
            PopupFrame::OpenRight => Some(OpenEdge::Right),
        }
    }

    /// The cells the frame takes along each axis: `(columns, rows)`.
    fn thickness(self) -> (u16, u16) {
        match self {
            PopupFrame::Full => (2, 2),
            PopupFrame::OpenTop | PopupFrame::OpenBottom => (2, 1),
            PopupFrame::OpenLeft | PopupFrame::OpenRight => (1, 2),
        }
    }
}

/// Where the popup is, in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupBox {
    /// The whole box, frame included.
    pub rect: Rect,
    pub frame: PopupFrame,
    /// The terminal inside the frame. Equal to `rect` when the box is too small
    /// for a frame, in which case none is drawn (the rule panes follow too).
    pub interior: Rect,
}

impl PopupBox {
    /// Whether a frame is drawn around the interior.
    pub fn framed(&self) -> bool {
        self.interior != self.rect
    }
}

/// `pct` percent of `len`, rounded down. `u32` because `65535 * 100` overflows
/// `u16`.
fn pct_of(len: u16, pct: u8) -> u16 {
    ((len as u32 * pct as u32) / 100) as u16
}

/// The box for `position`.
///
/// `pct` is `(width_pct, height_pct)`, clamped to the supported range. `window`
/// is the terminal size and `content` the rect the server composites into (the
/// terminal minus the sidebars). The status bar row, the last row of the
/// window, is never covered.
///
/// * `Float`: centred on the whole window, over every sidebar.
/// * `Top` and `Bottom`: the window's full width, `height_pct` of its rows,
///   over the sidebars. `width_pct` is ignored.
/// * `Left` and `Right`: `width_pct` of the content width, the content's full
///   height, beside the sidebars. `height_pct` is ignored.
///
/// Each axis is clamped like [`crate::server::layout::popup_rect`]: the
/// interior gets at least its minimum plus the frame, and the box never
/// outgrows the area it is placed in. A float centres with the same up/left
/// bias on an odd remainder.
pub fn popup_box(
    position: PopupPosition,
    pct: (u8, u8),
    window: (u16, u16),
    content: Rect,
) -> PopupBox {
    let (width_pct, height_pct) = clamp_popup_size(pct);
    let frame = PopupFrame::of(position);
    let (frame_cols, frame_rows) = frame.thickness();
    let min_width = POPUP_MIN_INTERIOR_COLS + frame_cols;
    let min_height = POPUP_MIN_INTERIOR_ROWS + frame_rows;

    // The window and the content, each without the status bar row.
    let area = Rect {
        x: 0,
        y: 0,
        width: window.0,
        height: window.1.saturating_sub(1),
    };
    let beside_sidebars = Rect {
        x: content.x,
        y: content.y,
        width: content.width,
        height: content.height.saturating_sub(1),
    };
    let sized = |len: u16, pct: u8, min: u16| pct_of(len, pct).max(min).min(len);

    let rect = match position {
        PopupPosition::Float => {
            let width = sized(area.width, width_pct, min_width);
            let height = sized(area.height, height_pct, min_height);
            Rect {
                x: area.x + (area.width - width) / 2,
                y: area.y + (area.height - height) / 2,
                width,
                height,
            }
        }
        PopupPosition::Top => Rect {
            height: sized(area.height, height_pct, min_height),
            ..area
        },
        PopupPosition::Bottom => {
            let height = sized(area.height, height_pct, min_height);
            Rect {
                y: area.y + area.height - height,
                height,
                ..area
            }
        }
        PopupPosition::Left => Rect {
            width: sized(beside_sidebars.width, width_pct, min_width),
            ..beside_sidebars
        },
        PopupPosition::Right => {
            let width = sized(beside_sidebars.width, width_pct, min_width);
            Rect {
                x: beside_sidebars.x + beside_sidebars.width - width,
                width,
                ..beside_sidebars
            }
        }
    };

    let interior = if fits_zellij_border(rect.width, rect.height) {
        let (x, y, w, h) = (rect.x, rect.y, rect.width, rect.height);
        match frame {
            PopupFrame::Full => Rect {
                x: x + 1,
                y: y + 1,
                width: w - 2,
                height: h - 2,
            },
            PopupFrame::OpenTop => Rect {
                x: x + 1,
                y,
                width: w - 2,
                height: h - 1,
            },
            PopupFrame::OpenBottom => Rect {
                x: x + 1,
                y: y + 1,
                width: w - 2,
                height: h - 1,
            },
            PopupFrame::OpenLeft => Rect {
                x,
                y: y + 1,
                width: w - 1,
                height: h - 2,
            },
            PopupFrame::OpenRight => Rect {
                x: x + 1,
                y: y + 1,
                width: w - 1,
                height: h - 2,
            },
        }
    } else {
        rect
    };
    PopupBox {
        rect,
        frame,
        interior,
    }
}

/// The popup layer the client holds: the server's interior-local cells and
/// cursor, as of the last frame that carried them.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldLayer {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Vec<RenderCell>>,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    pub cursor_style: u8,
}

impl HeldLayer {
    /// The layer a frame's `popup` describes, given the layer held before it.
    ///
    /// A set `cells` replaces the layer. Otherwise `changes` are applied to the
    /// held layer in place, which is only meaningful when it has the frame's
    /// size. The
    /// server sends a full layer whenever the size changes or the layer
    /// appears, so a diff that does not fit is a broken frame: `None` tells the
    /// caller to drop the layer rather than show a wrong one.
    pub fn from_frame(held: Option<HeldLayer>, frame: &PopupCells) -> Option<HeldLayer> {
        let mut layer = match (&frame.cells, held) {
            (Some(cells), _) => HeldLayer {
                cols: frame.cols,
                rows: frame.rows,
                cells: cells.clone(),
                cursor_x: 0,
                cursor_y: 0,
                cursor_visible: false,
                cursor_style: 0,
            },
            (None, Some(held)) if (held.cols, held.rows) == (frame.cols, frame.rows) => held,
            (None, _) => {
                log::warn!(
                    "popup: a layer diff for {}x{} does not fit the layer held, dropping it",
                    frame.cols,
                    frame.rows
                );
                return None;
            }
        };
        for change in &frame.changes {
            if let Some(slot) = layer
                .cells
                .get_mut(change.y as usize)
                .and_then(|row| row.get_mut(change.x as usize))
            {
                *slot = change.cell.clone();
            }
        }
        layer.cursor_x = frame.cursor_x;
        layer.cursor_y = frame.cursor_y;
        layer.cursor_visible = frame.cursor_visible;
        layer.cursor_style = frame.cursor_style;
        Some(layer)
    }
}

/// The box as a grid of its own size: the frame, then the held layer anchored at
/// the interior's top-left, the rest of the interior blank.
///
/// Anchoring at the top-left is what keeps the prompt where it is when another
/// client changes the shared size: the layer only ever shrinks or grows toward
/// the bottom-right.
pub fn compose_box(
    popup: &PopupBox,
    held: Option<&HeldLayer>,
    theme: &crate::config::theme::CompositorTheme,
) -> Vec<Vec<RenderCell>> {
    let rect = popup.rect;
    let mut grid = vec![vec![RenderCell::default(); rect.width as usize]; rect.height as usize];
    let local = Rect {
        x: 0,
        y: 0,
        width: rect.width,
        height: rect.height,
    };
    if popup.framed() {
        // The popup owns input whenever it is visible, so it always wears the
        // active-frame colour.
        let fg = border_fg(theme, true);
        let title = Some((vec!["popup".to_string()], vec![0], 0usize));
        match popup.frame.open_edge() {
            None => draw_zellij_border(&mut grid, local, &fg, &title, 0, "", theme),
            Some(open) => {
                draw_zellij_border_open(&mut grid, local, open, &fg, &title, 0, "", theme)
            }
        }
    }
    if let Some(held) = held {
        let (ox, oy) = (
            (popup.interior.x - rect.x) as usize,
            (popup.interior.y - rect.y) as usize,
        );
        let rows = (held.rows as usize).min(popup.interior.height as usize);
        let cols = (held.cols as usize).min(popup.interior.width as usize);
        for (dy, row) in held.cells.iter().take(rows).enumerate() {
            for (dx, cell) in row.iter().take(cols).enumerate() {
                grid[oy + dy][ox + dx] = cell.clone();
            }
        }
    }
    grid
}

/// The client's popup settings and runtime state.
#[derive(Debug, Clone)]
pub struct PopupState {
    position: PopupPosition,
    pct: (u8, u8),
    /// Runtime resizes, per session. A session with no entry uses the config's
    /// percentages. Reset whenever a popup config key changes.
    overrides: HashMap<String, (u8, u8)>,
    /// The box last announced to the server, with the connection it went to,
    /// so an identical send is skipped.
    last_sent: Option<(ConnId, PopupBox)>,
    /// Where a popup opened by a `PopupToggle <position>` key sits, by session
    /// and popup pane. There is one popup per tab, so this is per tab, and a
    /// respawned popup has a new id, so a record never outlives its popup. A
    /// popup with no entry follows `position`.
    named: HashMap<(String, PaneId), PopupPosition>,
    /// The popup whose layer is on screen.
    on_screen: Option<PaneId>,
    /// A toggle this client sent to open a popup, until its first layer
    /// arrives: the position it named, or `None` for the configured one.
    opening: Option<Option<PopupPosition>>,
}

impl PopupState {
    pub fn from_config(config: &Config) -> Self {
        PopupState {
            position: config.appearance.popup_position,
            pct: (
                config.appearance.popup_width_pct,
                config.appearance.popup_height_pct,
            ),
            overrides: HashMap::new(),
            last_sent: None,
            named: HashMap::new(),
            on_screen: None,
            opening: None,
        }
    }

    /// Take the three popup keys from `config`. A changed value discards every
    /// runtime override, so the saved file is what the user sees. Returns
    /// whether anything changed.
    pub fn apply_config(&mut self, config: &Config) -> bool {
        let position = config.appearance.popup_position;
        let pct = (
            config.appearance.popup_width_pct,
            config.appearance.popup_height_pct,
        );
        if (position, pct) == (self.position, self.pct) {
            return false;
        }
        self.position = position;
        self.pct = pct;
        self.overrides.clear();
        true
    }

    fn pct_for(&self, session: &str) -> (u8, u8) {
        self.overrides.get(session).copied().unwrap_or(self.pct)
    }

    /// Where `session`'s popup goes: where its key put the popup on screen,
    /// or where a toggle on its way to the server is opening one, else the
    /// configured position.
    fn position_for(&self, session: &str) -> PopupPosition {
        let named = match self.on_screen {
            Some(pane) => self.named.get(&(session.to_string(), pane)).copied(),
            None => self.opening.flatten(),
        };
        named.unwrap_or(self.position)
    }

    /// The box for `session`.
    pub fn popup_box(&self, session: &str, window: (u16, u16), content: Rect) -> PopupBox {
        popup_box(
            self.position_for(session),
            self.pct_for(session),
            window,
            content,
        )
    }

    /// Record a `PopupToggle` this client is about to send for `session`.
    ///
    /// Every popup key is a plain show/hide toggle: with the popup `visible`
    /// it closes, whatever position the key names, and the popup's record
    /// goes with it. Otherwise the popup opens at `position` (`None`: the
    /// configured one), which binds to the popup when its first layer arrives.
    pub fn toggle(&mut self, session: &str, position: Option<PopupPosition>, visible: bool) {
        if visible {
            if let Some(pane) = self.on_screen {
                self.named.remove(&(session.to_string(), pane));
            }
            self.opening = None;
        } else {
            self.opening = Some(position);
        }
    }

    /// Note which popup's layer `session`'s frames now carry, if any. The
    /// first layer after an opening toggle takes that toggle's position.
    pub fn bind_layer(&mut self, session: &str, pane: Option<PaneId>) {
        if let (Some(pane), Some(position)) = (pane, self.opening) {
            self.opening = None;
            let key = (session.to_string(), pane);
            match position {
                Some(position) => self.named.insert(key, position),
                None => self.named.remove(&key),
            };
        }
        self.on_screen = pane;
    }

    /// Adjust the popup's size by `(dw, dh)` percentage points on the axis the
    /// position uses. Returns `false` when the position does not use that axis,
    /// in which case nothing changes.
    pub fn resize(&mut self, session: &str, dw: i16, dh: i16) -> bool {
        let position = self.position_for(session);
        let uses_width = matches!(
            position,
            PopupPosition::Float | PopupPosition::Left | PopupPosition::Right
        );
        let uses_height = matches!(
            position,
            PopupPosition::Float | PopupPosition::Top | PopupPosition::Bottom
        );
        if (dw != 0 && !uses_width) || (dh != 0 && !uses_height) {
            log::debug!("popup: resize ({dw}, {dh}) ignored, {position:?} does not use that axis");
            return false;
        }
        let adjust = |cur: u8, delta: i16| -> u8 {
            (cur as i16 + delta).clamp(POPUP_MIN_PCT as i16, POPUP_MAX_PCT as i16) as u8
        };
        let (w, h) = self.pct_for(session);
        self.overrides
            .insert(session.to_string(), (adjust(w, dw), adjust(h, dh)));
        true
    }

    /// Forget what was announced, so the next [`PopupState::announce`] sends
    /// again. The server answers every announcement with a full frame, which
    /// carries the whole layer: this is how a client that had to drop its layer
    /// gets it back.
    pub fn reannounce(&mut self) {
        self.last_sent = None;
    }

    /// Whether a toggle this client sent to open a popup is still waiting for
    /// that popup's first layer.
    pub fn is_opening(&self) -> bool {
        self.opening.is_some()
    }

    /// The interior to ask `conn` for, or `None` when it was already told for
    /// this box, or when nothing needs a new one.
    ///
    /// The comparison is on the whole box, not only its interior: a box that
    /// moved while keeping its size still has to be announced, because the
    /// answer to a [`crate::protocol::ClientMessage::PopupSize`] is the full
    /// frame that repaints what the old box masked away.
    ///
    /// `active` says a popup is on screen or opening. Without one, a connection
    /// already told a box is told nothing new. The popups of a session share
    /// one terminal size, so announcing the configured box for a tab with no
    /// popup would resize every other tab's popup, and resize it back when the
    /// user returns. A connection told nothing yet is still told, because until
    /// then the server draws the popup for this client itself.
    pub fn announce(
        &mut self,
        conn: &ConnId,
        popup: &PopupBox,
        active: bool,
    ) -> Option<(u16, u16)> {
        if self
            .last_sent
            .as_ref()
            .is_some_and(|(c, b)| c == conn && (b == popup || !active))
        {
            return None;
        }
        self.last_sent = Some((conn.clone(), *popup));
        Some((popup.interior.width, popup.interior.height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CellChange;

    const WINDOW: (u16, u16) = (160, 48);

    fn content(x: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y: 0,
            width: w,
            height: h,
        }
    }

    /// The reference sidebar: 34 columns on the left, so content is x=34 w=126.
    fn left_sidebar() -> Rect {
        content(34, 126, 48)
    }

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn boxed(position: PopupPosition, content: Rect) -> PopupBox {
        popup_box(position, (80, 80), WINDOW, content)
    }

    #[test]
    fn float_is_centred_on_the_whole_window_over_the_sidebar() {
        let b = boxed(PopupPosition::Float, left_sidebar());
        assert_eq!(b.rect, rect(16, 5, 128, 37));
        assert_eq!(b.interior, rect(17, 6, 126, 35));
        assert_eq!(b.frame, PopupFrame::Full);
    }

    #[test]
    fn top_spans_the_window_over_the_sidebar_and_has_no_top_edge() {
        let b = boxed(PopupPosition::Top, left_sidebar());
        assert_eq!(b.rect, rect(0, 0, 160, 37));
        assert_eq!(b.interior, rect(1, 0, 158, 36));
        assert_eq!(b.frame, PopupFrame::OpenTop);
    }

    #[test]
    fn bottom_sits_directly_above_the_status_row() {
        let b = boxed(PopupPosition::Bottom, left_sidebar());
        assert_eq!(b.rect, rect(0, 10, 160, 37));
        assert_eq!(b.interior, rect(1, 11, 158, 36));
        assert_eq!(b.frame, PopupFrame::OpenBottom);
        assert_eq!(b.rect.y + b.rect.height, WINDOW.1 - 1, "status row is free");
    }

    #[test]
    fn left_starts_just_right_of_a_left_sidebar() {
        let b = boxed(PopupPosition::Left, left_sidebar());
        assert_eq!(b.rect, rect(34, 0, 100, 47));
        assert_eq!(b.interior, rect(34, 1, 99, 45));
        assert_eq!(b.frame, PopupFrame::OpenLeft);
    }

    #[test]
    fn right_ends_where_the_content_ends() {
        let b = boxed(PopupPosition::Right, left_sidebar());
        assert_eq!(b.rect, rect(60, 0, 100, 47));
        assert_eq!(b.interior, rect(61, 1, 99, 45));
        assert_eq!(b.frame, PopupFrame::OpenRight);
    }

    #[test]
    fn right_stops_before_a_right_sidebar() {
        // A 20-column right sidebar: content is x=34 w=106.
        let b = boxed(PopupPosition::Right, content(34, 106, 48));
        assert_eq!(b.rect.x + b.rect.width, 140);
        assert_eq!(b.rect.width, 84);
    }

    #[test]
    fn a_bottom_sidebar_changes_left_and_right_but_not_float_or_bottom() {
        // A 6-row bottom sidebar leaves 42 content rows.
        let with_bottom = content(34, 126, 42);
        assert_eq!(
            boxed(PopupPosition::Float, with_bottom),
            boxed(PopupPosition::Float, left_sidebar())
        );
        assert_eq!(
            boxed(PopupPosition::Bottom, with_bottom),
            boxed(PopupPosition::Bottom, left_sidebar())
        );
        assert_eq!(
            boxed(PopupPosition::Left, with_bottom).rect,
            rect(34, 0, 100, 41)
        );
        assert_eq!(
            boxed(PopupPosition::Right, with_bottom).rect,
            rect(60, 0, 100, 41)
        );
    }

    #[test]
    fn the_axis_a_position_does_not_use_is_ignored() {
        let full = |pct| popup_box(PopupPosition::Top, pct, WINDOW, left_sidebar());
        assert_eq!(full((20, 80)), full((100, 80)), "top ignores width_pct");
        let side = |pct| popup_box(PopupPosition::Left, pct, WINDOW, left_sidebar());
        assert_eq!(side((80, 20)), side((80, 100)), "left ignores height_pct");
    }

    #[test]
    fn with_no_sidebar_float_is_what_popup_rect_gives() {
        for window in [(160, 48), (101, 31), (80, 24), (200, 60)] {
            let whole = content(0, window.0, window.1);
            let b = popup_box(PopupPosition::Float, (80, 80), window, whole);
            let area = Rect {
                x: 0,
                y: 0,
                width: window.0,
                height: window.1 - 1,
            };
            assert_eq!(
                b.rect,
                crate::server::layout::popup_rect(area, (80, 80)),
                "window {window:?}"
            );
        }
    }

    #[test]
    fn percentages_are_clamped_to_the_supported_range() {
        let low = popup_box(PopupPosition::Float, (0, 5), WINDOW, left_sidebar());
        let at_min = popup_box(PopupPosition::Float, (20, 20), WINDOW, left_sidebar());
        assert_eq!(low, at_min);
        let high = popup_box(PopupPosition::Float, (200, 255), WINDOW, left_sidebar());
        assert_eq!(high.rect, rect(0, 0, 160, 47));
    }

    #[test]
    fn each_axis_keeps_the_minimum_interior_and_never_outgrows_the_area() {
        // 20% of 40 columns is 8, below the 10 + 2 the float needs.
        let tiny = popup_box(PopupPosition::Float, (20, 20), (40, 12), content(0, 40, 12));
        assert_eq!(tiny.interior.width, POPUP_MIN_INTERIOR_COLS);
        assert_eq!(tiny.interior.height, POPUP_MIN_INTERIOR_ROWS);
        // A window smaller than the minimum is filled and gets no frame.
        let smaller = popup_box(PopupPosition::Float, (80, 80), (4, 3), content(0, 4, 3));
        assert_eq!(smaller.rect, rect(0, 0, 4, 2));
        // 4x2 does not fit a frame, so the whole box is the interior.
        assert_eq!(smaller.interior, smaller.rect);
        assert!(!smaller.framed());
        // A drawer's minimum counts only the edges it draws.
        let drawer = popup_box(PopupPosition::Left, (20, 80), (40, 30), content(0, 40, 30));
        assert_eq!(drawer.rect.width, POPUP_MIN_INTERIOR_COLS + 1);
        let top = popup_box(PopupPosition::Top, (80, 20), (60, 12), content(0, 60, 12));
        assert_eq!(top.rect.height, POPUP_MIN_INTERIOR_ROWS + 1);
    }

    #[test]
    fn a_framed_box_has_an_interior_inside_its_rect() {
        for position in [
            PopupPosition::Float,
            PopupPosition::Top,
            PopupPosition::Bottom,
            PopupPosition::Left,
            PopupPosition::Right,
        ] {
            let b = boxed(position, left_sidebar());
            assert!(b.framed());
            let (i, r) = (b.interior, b.rect);
            assert!(i.x >= r.x && i.y >= r.y);
            assert!(i.x + i.width <= r.x + r.width && i.y + i.height <= r.y + r.height);
        }
    }

    // -- State ----------------------------------------------------------------

    fn config_with(position: PopupPosition, w: u8, h: u8) -> Config {
        let mut config = Config::default();
        config.appearance.popup_position = position;
        config.appearance.popup_width_pct = w;
        config.appearance.popup_height_pct = h;
        config
    }

    #[test]
    fn a_runtime_resize_adjusts_the_axis_the_position_uses() {
        let mut s = PopupState::from_config(&config_with(PopupPosition::Left, 50, 80));
        let before = s.popup_box("main", WINDOW, left_sidebar()).rect.width;
        assert!(s.resize("main", 10, 0));
        let after = s.popup_box("main", WINDOW, left_sidebar()).rect.width;
        assert!(after > before, "{after} should exceed {before}");
        // The other axis is ignored and changes nothing.
        let held = s.popup_box("main", WINDOW, left_sidebar());
        assert!(!s.resize("main", 0, 10));
        assert_eq!(s.popup_box("main", WINDOW, left_sidebar()), held);
    }

    #[test]
    fn a_float_resizes_on_both_axes_and_a_drawer_on_one() {
        let mut float = PopupState::from_config(&config_with(PopupPosition::Float, 50, 50));
        assert!(float.resize("s", 10, 0) && float.resize("s", 0, 10));
        let mut top = PopupState::from_config(&config_with(PopupPosition::Top, 50, 50));
        assert!(top.resize("s", 0, 10));
        assert!(!top.resize("s", 10, 0));
    }

    #[test]
    fn a_resize_is_per_session_and_clamped() {
        let mut s = PopupState::from_config(&config_with(PopupPosition::Float, 80, 80));
        for _ in 0..10 {
            s.resize("a", 20, 20);
        }
        assert_eq!(s.pct_for("a"), (100, 100));
        assert_eq!(s.pct_for("b"), (80, 80), "another session is unaffected");
        for _ in 0..10 {
            s.resize("a", -20, -20);
        }
        assert_eq!(s.pct_for("a"), (20, 20));
    }

    #[test]
    fn a_changed_popup_key_resets_every_override() {
        let mut s = PopupState::from_config(&config_with(PopupPosition::Float, 80, 80));
        s.resize("a", 5, 5);
        assert!(!s.apply_config(&config_with(PopupPosition::Float, 80, 80)));
        assert_eq!(
            s.pct_for("a"),
            (85, 85),
            "an unchanged config keeps overrides"
        );
        assert!(s.apply_config(&config_with(PopupPosition::Left, 80, 80)));
        assert_eq!(s.pct_for("a"), (80, 80));
        s.resize("a", 5, 0);
        assert!(s.apply_config(&config_with(PopupPosition::Left, 50, 80)));
        assert_eq!(s.pct_for("a"), (50, 80));
    }

    #[test]
    fn an_identical_box_is_not_announced_twice_to_one_connection() {
        let mut s = PopupState::from_config(&Config::default());
        let b = boxed(PopupPosition::Float, left_sidebar());
        assert_eq!(s.announce(&ConnId::Local, &b, true), Some((126, 35)));
        assert_eq!(s.announce(&ConnId::Local, &b, true), None);
        // Another server has not been told.
        let far = ConnId::Remote("far".to_string());
        assert_eq!(s.announce(&far, &b, true), Some((126, 35)));
        assert_eq!(s.announce(&ConnId::Local, &b, true), Some((126, 35)));
    }

    #[test]
    fn no_box_is_announced_while_no_popup_is_up_or_opening() {
        let mut s = PopupState::from_config(&Config::default());
        let right = boxed(PopupPosition::Right, left_sidebar());
        let float = boxed(PopupPosition::Float, left_sidebar());
        // The first box a connection hears of is always announced: until it
        // has one, the server treats this client as one that cannot place
        // the popup, and draws a float itself.
        assert!(s.announce(&ConnId::Local, &right, false).is_some());
        // Then, with no popup on screen and none opening, another box (a tab
        // without a popup falls back to the config's) is not: it would resize
        // the popups of every other tab in the session.
        assert_eq!(s.announce(&ConnId::Local, &float, false), None);
        // A popup up or opening is announced as before.
        assert!(s.announce(&ConnId::Local, &float, true).is_some());
        // Another server has not been told anything yet.
        let far = ConnId::Remote("far".to_string());
        assert!(s.announce(&far, &right, false).is_some());
    }

    #[test]
    fn a_box_that_moved_with_the_same_interior_is_announced_again() {
        // Left and right drawers of the same width have equal interiors, but a
        // different place: the answer to the announce is the frame that
        // repaints what the old box masked.
        let mut s = PopupState::from_config(&Config::default());
        let left = boxed(PopupPosition::Left, left_sidebar());
        let right = boxed(PopupPosition::Right, left_sidebar());
        assert_eq!(
            (left.interior.width, left.interior.height),
            (right.interior.width, right.interior.height)
        );
        assert!(s.announce(&ConnId::Local, &left, true).is_some());
        assert!(s.announce(&ConnId::Local, &right, true).is_some());
    }

    fn frame_of(s: &PopupState, session: &str) -> PopupFrame {
        s.popup_box(session, WINDOW, left_sidebar()).frame
    }

    #[test]
    fn a_named_open_places_the_popup_there_and_any_key_closes_it() {
        let mut s = PopupState::from_config(&Config::default());
        assert_eq!(frame_of(&s, "a"), PopupFrame::Full);
        s.toggle("a", Some(PopupPosition::Right), false);
        // Before the server's first layer, the box is the one being opened, so
        // the PopupSize sent ahead of the toggle sizes the right drawer.
        assert_eq!(frame_of(&s, "a"), PopupFrame::OpenRight);
        s.bind_layer("a", Some(7));
        assert_eq!(frame_of(&s, "a"), PopupFrame::OpenRight);
        // A key naming another position closes it rather than moving it.
        s.toggle("a", Some(PopupPosition::Bottom), true);
        s.bind_layer("a", None);
        assert_eq!(frame_of(&s, "a"), PopupFrame::Full);
        // And the record went with it: reopened plain, it is the config's.
        s.toggle("a", None, false);
        s.bind_layer("a", Some(7));
        assert_eq!(frame_of(&s, "a"), PopupFrame::Full);
    }

    #[test]
    fn a_plain_open_follows_the_config() {
        let mut s = PopupState::from_config(&Config::default());
        s.toggle("a", None, false);
        s.bind_layer("a", Some(7));
        assert!(s.apply_config(&config_with(PopupPosition::Left, 80, 80)));
        assert_eq!(frame_of(&s, "a"), PopupFrame::OpenLeft);
    }

    #[test]
    fn each_tabs_popup_keeps_the_position_it_was_opened_at() {
        let mut s = PopupState::from_config(&Config::default());
        s.toggle("a", Some(PopupPosition::Right), false);
        s.bind_layer("a", Some(7));
        // Another tab, with no popup up.
        s.bind_layer("a", None);
        assert_eq!(frame_of(&s, "a"), PopupFrame::Full);
        s.toggle("a", Some(PopupPosition::Bottom), false);
        s.bind_layer("a", Some(9));
        assert_eq!(frame_of(&s, "a"), PopupFrame::OpenBottom);
        // Back on the first tab, whose popup is still open.
        s.bind_layer("a", Some(7));
        assert_eq!(frame_of(&s, "a"), PopupFrame::OpenRight);
        // The same popup id on another server is another popup.
        s.bind_layer("b", Some(7));
        assert_eq!(frame_of(&s, "b"), PopupFrame::Full);
    }

    #[test]
    fn a_runtime_resize_uses_the_axis_of_the_position_on_screen() {
        let mut s = PopupState::from_config(&config_with(PopupPosition::Float, 50, 50));
        s.toggle("a", Some(PopupPosition::Left), false);
        s.bind_layer("a", Some(7));
        assert!(!s.resize("a", 0, 10), "a left drawer ignores its height");
        assert!(s.resize("a", 10, 0));
    }

    // -- The layer ------------------------------------------------------------

    fn cell(c: char) -> RenderCell {
        RenderCell {
            c,
            ..RenderCell::default()
        }
    }

    fn frame(cols: u16, rows: u16, cells: Option<Vec<Vec<RenderCell>>>) -> PopupCells {
        PopupCells {
            pane_id: 7,
            cols,
            rows,
            cursor_x: 1,
            cursor_y: 0,
            cursor_visible: true,
            cursor_style: 2,
            cells,
            changes: Vec::new(),
        }
    }

    #[test]
    fn a_full_layer_replaces_what_was_held() {
        let a = HeldLayer::from_frame(None, &frame(2, 1, Some(vec![vec![cell('a'), cell('b')]])))
            .expect("layer");
        assert_eq!(a.cells[0][1].c, 'b');
        assert_eq!((a.cursor_x, a.cursor_y, a.cursor_visible), (1, 0, true));
        let b = HeldLayer::from_frame(Some(a), &frame(1, 1, Some(vec![vec![cell('z')]])))
            .expect("layer");
        assert_eq!((b.cols, b.rows), (1, 1));
    }

    #[test]
    fn a_diff_is_applied_to_the_held_layer() {
        let held =
            HeldLayer::from_frame(None, &frame(2, 1, Some(vec![vec![cell('a'), cell('b')]])))
                .expect("layer");
        let mut diff = frame(2, 1, None);
        diff.changes.push(CellChange {
            x: 1,
            y: 0,
            cell: cell('q'),
        });
        let next = HeldLayer::from_frame(Some(held), &diff).expect("layer");
        assert_eq!(next.cells[0][0].c, 'a');
        assert_eq!(next.cells[0][1].c, 'q');
    }

    #[test]
    fn a_diff_with_nothing_to_apply_it_to_drops_the_layer() {
        let diff = frame(2, 1, None);
        assert_eq!(HeldLayer::from_frame(None, &diff), None);
        let held =
            HeldLayer::from_frame(None, &frame(1, 1, Some(vec![vec![cell('a')]]))).expect("layer");
        assert_eq!(
            HeldLayer::from_frame(Some(held), &diff),
            None,
            "size differs"
        );
    }

    #[test]
    fn the_box_anchors_a_smaller_layer_top_left_and_blanks_the_rest() {
        let theme = crate::config::theme::CompositorTheme::default();
        let b = boxed(PopupPosition::Float, content(0, 160, 48));
        let held = HeldLayer::from_frame(
            None,
            &frame(
                2,
                2,
                Some(vec![vec![cell('a'), cell('b')], vec![cell('c'), cell('d')]]),
            ),
        )
        .expect("layer");
        let grid = compose_box(&b, Some(&held), &theme);
        assert_eq!(grid.len(), b.rect.height as usize);
        assert_eq!(grid[0].len(), b.rect.width as usize);
        // Interior origin is (1, 1) in the box.
        assert_eq!(grid[1][1].c, 'a');
        assert_eq!(grid[1][2].c, 'b');
        assert_eq!(grid[2][1].c, 'c');
        assert_eq!(grid[1][3].c, ' ', "beyond the layer is blank");
        assert_eq!(grid[3][1].c, ' ');
        // The frame is drawn.
        assert_eq!(grid[0][0].c, crate::server::compositor::BOX_TOP_LEFT);
    }

    #[test]
    fn an_open_drawer_box_has_its_open_side_to_the_interior() {
        let theme = crate::config::theme::CompositorTheme::default();
        let b = boxed(PopupPosition::Left, left_sidebar());
        let held =
            HeldLayer::from_frame(None, &frame(1, 1, Some(vec![vec![cell('x')]]))).expect("layer");
        let grid = compose_box(&b, Some(&held), &theme);
        // Interior is (34, 1): column 0 of the box, row 1.
        assert_eq!(grid[1][0].c, 'x');
        assert_ne!(grid[0][0].c, crate::server::compositor::BOX_TOP_LEFT);
    }
}
