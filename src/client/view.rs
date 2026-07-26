//! Client-side "View": a virtual tab whose cells alias real panes.
//!
//! A [`ClientView`] is a purely client-side construct. It does not exist on any
//! server; instead it references a set of real panes (each identified by the
//! connection it lives on plus its [`PaneId`]) and composites the per-pane
//! [`PaneContent`](crate::protocol::ServerMessage::PaneContent) snapshots the
//! server streams for those panes into a single grid. The event loop owns the
//! list of views and feeds fresh snapshots in as they arrive; everything in
//! this module is pure geometry + buffer composition so it can be unit-tested
//! headlessly (no terminal, no sockets, no `Theme`).
//!
//! Sizing note: cells render a pane's *current-size* snapshot, clipped and
//! letterboxed into the cell rect and bottom-anchored (so the latest output is
//! visible). True "smallest-viewer-wins" pane sizing (Model A) is deferred, so
//! a cell does not yet demand a size from its source pane.

use crate::client::registry::ConnId;
use crate::protocol::{CellColor, PaneId, RenderCell};
use crate::server::layout::{FocusDirection, Rect};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A snapshot of one pane's rendered screen, as delivered by `PaneContent`.
/// Already a finished cell grid — the client never sees the server's `Screen`.
#[derive(Debug, Clone)]
pub struct PaneSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Vec<RenderCell>>,
}

/// One cell of a view: a reference to a real pane on a specific connection,
/// plus the most recent snapshot received for it (`None` until the first
/// `PaneContent` arrives).
#[derive(Debug, Clone)]
pub struct ViewCell {
    pub conn: ConnId,
    pub pane_id: PaneId,
    pub snapshot: Option<PaneSnapshot>,
}

/// How a view arranges its cells.
///
/// The MVP supports two arrangements. Full parity with the server's
/// [`LayoutMode`](crate::server::layout::LayoutMode) (Bsp / Master) is
/// deferred; only `Grid` and `Monocle` exist here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewLayout {
    /// All cells in a balanced grid of (roughly) equal-size cells.
    Grid,
    /// The focused cell fills the whole area; the others are hidden.
    Monocle,
}

impl ViewLayout {
    /// Cycle to the next layout: Grid -> Monocle -> Grid.
    pub fn next(self) -> Self {
        match self {
            ViewLayout::Grid => ViewLayout::Monocle,
            ViewLayout::Monocle => ViewLayout::Grid,
        }
    }
}

/// A client-side virtual tab compositing several panes.
#[derive(Debug, Clone)]
pub struct ClientView {
    pub name: String,
    pub cells: Vec<ViewCell>,
    pub layout: ViewLayout,
    /// Index into `cells` of the focused cell. Always clamped to a valid index
    /// (or 0 when there are no cells) by the mutators below.
    pub focused: usize,
}

impl ClientView {
    /// Create an empty view with the given name (Grid layout, no cells).
    pub fn new(name: String) -> Self {
        Self {
            name,
            cells: Vec::new(),
            layout: ViewLayout::Grid,
            focused: 0,
        }
    }

    /// Clamp `focused` into range after the cell list changed.
    pub fn clamp_focus(&mut self) {
        if self.cells.is_empty() {
            self.focused = 0;
        } else if self.focused >= self.cells.len() {
            self.focused = self.cells.len() - 1;
        }
    }

    /// Move focus in the given direction using the same `cols = ceil(sqrt(n))`
    /// geometry as [`cell_rects`], so focus tracks the visible grid. Returns
    /// `true` if the focused cell actually changed.
    ///
    /// Monocle has only one visible cell, but focus still moves through the
    /// underlying cell list (left/up = previous, right/down = next) so the user
    /// can page between panes; that keeps the two layouts consistent.
    pub fn move_focus(&mut self, dir: FocusDirection) -> bool {
        let n = self.cells.len();
        if n == 0 {
            return false;
        }
        if self.layout == ViewLayout::Monocle {
            let new = match dir {
                FocusDirection::Left | FocusDirection::Up => self.focused.saturating_sub(1),
                FocusDirection::Right | FocusDirection::Down => (self.focused + 1).min(n - 1),
            };
            let changed = new != self.focused;
            self.focused = new;
            return changed;
        }
        let cols = grid_cols(n);
        let row = self.focused / cols;
        let col = self.focused % cols;
        let new = match dir {
            FocusDirection::Left => {
                if col > 0 {
                    self.focused - 1
                } else {
                    self.focused
                }
            }
            FocusDirection::Right => {
                if col + 1 < cols && self.focused + 1 < n {
                    self.focused + 1
                } else {
                    self.focused
                }
            }
            FocusDirection::Up => {
                if row > 0 {
                    self.focused - cols
                } else {
                    self.focused
                }
            }
            FocusDirection::Down => {
                if self.focused + cols < n {
                    self.focused + cols
                } else {
                    self.focused
                }
            }
        };
        let changed = new != self.focused;
        self.focused = new;
        changed
    }
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Number of columns per full grid row for `n` cells: `ceil(sqrt(n))`.
fn grid_cols(n: usize) -> usize {
    if n <= 1 {
        1
    } else {
        (n as f64).sqrt().ceil() as usize
    }
}

/// Compute the outer rectangle for each of `n` cells within `area`.
///
/// - `Grid`: rows stacked top-to-bottom, each row spanning the full width; the
///   cells within a row partition that width equally. `cols = ceil(sqrt(n))`
///   cells per row, with the last (possibly short) row's cells spread across
///   the full width. Remainder pixels go to the first rows / first columns, so
///   the rects fully tile `area` with no gaps and no overlap.
/// - `Monocle`: every rect equals `area` (only the focused one is drawn).
///
/// Returns exactly `n` rects (empty when `n == 0`).
pub fn cell_rects(layout: ViewLayout, n: usize, area: Rect) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    match layout {
        ViewLayout::Monocle => vec![area; n],
        ViewLayout::Grid => grid_rects(n, area),
    }
}

/// Grid tiling: see [`cell_rects`].
fn grid_rects(n: usize, area: Rect) -> Vec<Rect> {
    if n == 1 {
        return vec![area];
    }
    let cols = grid_cols(n);
    let rows = n.div_ceil(cols);

    let base_h = area.height / rows as u16;
    let rem_h = area.height % rows as u16;

    let mut rects = Vec::with_capacity(n);
    let mut placed = 0usize;
    let mut y = area.y;
    for r in 0..rows {
        let h = base_h + if (r as u16) < rem_h { 1 } else { 0 };
        // Cells in this row: `cols` for every full row, whatever remains for
        // the last row (always between 1 and `cols`).
        let cells_in_row = if r == rows - 1 {
            n - cols * (rows - 1)
        } else {
            cols
        };
        let base_w = area.width / cells_in_row as u16;
        let rem_w = area.width % cells_in_row as u16;
        let mut x = area.x;
        for c in 0..cells_in_row {
            let w = base_w + if (c as u16) < rem_w { 1 } else { 0 };
            rects.push(Rect {
                x,
                y,
                width: w,
                height: h,
            });
            x += w;
            placed += 1;
            if placed == n {
                break;
            }
        }
        y += h;
    }
    rects
}

// ---------------------------------------------------------------------------
// Compositing
// ---------------------------------------------------------------------------

/// SGR indexed color for the focused cell's border (bright green). Chosen over
/// threading a `Theme` through so the compositor stays trivially testable; the
/// focused cell must be visually unmistakable, which this + bold achieves.
const FOCUSED_BORDER: CellColor = CellColor::Indexed(10);
/// Indexed color for an unfocused cell's border (bright black / grey).
const UNFOCUSED_BORDER: CellColor = CellColor::Indexed(8);

/// Composite a view into an `area.height` x `area.width` buffer of
/// [`RenderCell`]s, ready to hand to
/// [`Renderer::render_full`](crate::client::renderer::Renderer::render_full).
///
/// Each cell gets a box border (the focused cell's border is drawn bold in a
/// distinct color) and its snapshot is blitted bottom-anchored into the box's
/// interior, clipped to the interior's width/height. Cells with no snapshot yet
/// show a centered placeholder label. In `Monocle` only the focused cell is
/// drawn, filling `area`.
///
/// `area` is expected to have its origin at the buffer origin in normal use
/// (the full terminal, `x = y = 0`); rect coordinates are translated back to
/// buffer-local space so a non-zero origin still composites correctly.
pub fn composite(view: &ClientView, area: Rect) -> Vec<Vec<RenderCell>> {
    let w = area.width as usize;
    let h = area.height as usize;
    let mut buf = vec![vec![RenderCell::default(); w]; h];
    if w == 0 || h == 0 || view.cells.is_empty() {
        return buf;
    }

    match view.layout {
        ViewLayout::Grid => {
            let rects = cell_rects(ViewLayout::Grid, view.cells.len(), area);
            for (i, cell) in view.cells.iter().enumerate() {
                if let Some(rect) = rects.get(i) {
                    draw_cell(&mut buf, area, *rect, cell, i == view.focused);
                }
            }
        }
        ViewLayout::Monocle => {
            if let Some(cell) = view.cells.get(view.focused) {
                draw_cell(&mut buf, area, area, cell, true);
            }
        }
    }
    buf
}

/// Write a single cell into the buffer if the coordinates are in range.
fn put(buf: &mut [Vec<RenderCell>], y: usize, x: usize, cell: RenderCell) {
    if let Some(row) = buf.get_mut(y) {
        if let Some(slot) = row.get_mut(x) {
            *slot = cell;
        }
    }
}

/// Make a border cell with the given glyph and focus styling.
fn border_cell(ch: char, focused: bool) -> RenderCell {
    RenderCell {
        c: ch,
        fg: if focused {
            FOCUSED_BORDER
        } else {
            UNFOCUSED_BORDER
        },
        bold: focused,
        ..RenderCell::default()
    }
}

/// Draw one cell (border + snapshot) into `buf`. `rect` is in `area`-absolute
/// coordinates; it is translated to buffer-local space using `area`'s origin.
fn draw_cell(buf: &mut [Vec<RenderCell>], area: Rect, rect: Rect, cell: &ViewCell, focused: bool) {
    let ox = area.x as usize;
    let oy = area.y as usize;
    let rx = (rect.x as usize).saturating_sub(ox);
    let ry = (rect.y as usize).saturating_sub(oy);
    let rw = rect.width as usize;
    let rh = rect.height as usize;
    if rw == 0 || rh == 0 {
        return;
    }

    // A border is only drawn when there is room for it plus at least one
    // interior cell in each axis; otherwise the snapshot fills the whole rect.
    let draw_border = rw >= 2 && rh >= 2;
    let (ix, iy, iw, ih) = if draw_border {
        (rx + 1, ry + 1, rw - 2, rh - 2)
    } else {
        (rx, ry, rw, rh)
    };

    if draw_border {
        let last_x = rx + rw - 1;
        let last_y = ry + rh - 1;
        // Corners.
        put(buf, ry, rx, border_cell('┌', focused));
        put(buf, ry, last_x, border_cell('┐', focused));
        put(buf, last_y, rx, border_cell('└', focused));
        put(buf, last_y, last_x, border_cell('┘', focused));
        // Horizontal edges.
        for x in (rx + 1)..last_x {
            put(buf, ry, x, border_cell('─', focused));
            put(buf, last_y, x, border_cell('─', focused));
        }
        // Vertical edges.
        for y in (ry + 1)..last_y {
            put(buf, y, rx, border_cell('│', focused));
            put(buf, y, last_x, border_cell('│', focused));
        }
        // Label the top border with the pane identity, clipped to fit.
        let label = format!(" pane {} ", cell.pane_id);
        let max = rw.saturating_sub(2);
        for (i, ch) in label.chars().take(max).enumerate() {
            put(buf, ry, rx + 1 + i, border_cell(ch, focused));
        }
    }

    if iw == 0 || ih == 0 {
        return;
    }

    match &cell.snapshot {
        Some(snap) => {
            // Bottom-anchor: when the snapshot is taller than the interior show
            // its LAST `ih` rows; when shorter, top-align from row 0.
            let sr = snap.cells.len();
            let start = sr.saturating_sub(ih);
            for r in 0..ih {
                let src = start + r;
                if src >= sr {
                    break;
                }
                let row = &snap.cells[src];
                for c in 0..iw {
                    match row.get(c) {
                        Some(rc) => put(buf, iy + r, ix + c, rc.clone()),
                        None => break,
                    }
                }
            }
        }
        None => {
            // No snapshot yet: centered placeholder so the cell isn't blank.
            let label = format!("… pane {} …", cell.pane_id);
            let text: Vec<char> = label.chars().take(iw).collect();
            let start_x = ix + (iw - text.len()) / 2;
            let mid_y = iy + ih / 2;
            for (i, ch) in text.into_iter().enumerate() {
                put(
                    buf,
                    mid_y,
                    start_x + i,
                    RenderCell {
                        c: ch,
                        ..RenderCell::default()
                    },
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    /// A snapshot filled with a single marker char.
    fn snap_filled(cols: u16, rows: u16, marker: char) -> PaneSnapshot {
        let cell = RenderCell {
            c: marker,
            ..RenderCell::default()
        };
        PaneSnapshot {
            cols,
            rows,
            cells: vec![vec![cell; cols as usize]; rows as usize],
        }
    }

    fn cell_with(pane_id: PaneId, snapshot: Option<PaneSnapshot>) -> ViewCell {
        ViewCell {
            conn: ConnId::Local,
            pane_id,
            snapshot,
        }
    }

    #[test]
    fn cell_rects_returns_n_rects() {
        for n in 0..=5 {
            assert_eq!(cell_rects(ViewLayout::Grid, n, area(80, 24)).len(), n);
            assert_eq!(cell_rects(ViewLayout::Monocle, n, area(80, 24)).len(), n);
        }
    }

    #[test]
    fn cell_rects_within_area() {
        let a = area(80, 24);
        for n in 1..=5 {
            for r in cell_rects(ViewLayout::Grid, n, a) {
                assert!(r.x >= a.x && r.y >= a.y);
                assert!(r.x + r.width <= a.x + a.width);
                assert!(r.y + r.height <= a.y + a.height);
                assert!(r.width > 0 && r.height > 0);
            }
        }
    }

    #[test]
    fn cell_rects_grid_tiles_without_gaps_or_overlap() {
        // Paint a coverage grid; every pixel must be covered exactly once.
        let a = area(37, 19); // deliberately not divisible, exercises remainder
        for n in 1..=5 {
            let mut cover = vec![vec![0u8; a.width as usize]; a.height as usize];
            for r in cell_rects(ViewLayout::Grid, n, a) {
                for y in r.y..r.y + r.height {
                    for x in r.x..r.x + r.width {
                        cover[y as usize][x as usize] += 1;
                    }
                }
            }
            for (y, row) in cover.iter().enumerate() {
                for (x, &c) in row.iter().enumerate() {
                    assert_eq!(c, 1, "n={n} pixel ({x},{y}) covered {c} times");
                }
            }
        }
    }

    #[test]
    fn monocle_rects_all_equal_area() {
        let a = area(80, 24);
        for r in cell_rects(ViewLayout::Monocle, 4, a) {
            assert_eq!(r, a);
        }
    }

    #[test]
    fn composite_places_each_snapshot_in_its_cell() {
        // Two side-by-side cells; each snapshot filled with a distinct marker.
        let view = ClientView {
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(40, 24, 'A'))),
                cell_with(2, Some(snap_filled(40, 24, 'B'))),
            ],
            layout: ViewLayout::Grid,
            focused: 0,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);
        let rects = cell_rects(ViewLayout::Grid, 2, a);

        // Interior center of cell 0 must be 'A', cell 1 must be 'B'.
        for (rect, marker) in [(rects[0], 'A'), (rects[1], 'B')] {
            let cx = (rect.x + rect.width / 2) as usize;
            let cy = (rect.y + rect.height / 2) as usize;
            assert_eq!(buf[cy][cx].c, marker, "cell marker mismatch at ({cx},{cy})");
        }
    }

    #[test]
    fn composite_draws_distinct_focused_border() {
        let view = ClientView {
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(40, 24, 'A'))),
                cell_with(2, Some(snap_filled(40, 24, 'B'))),
            ],
            layout: ViewLayout::Grid,
            focused: 1,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);
        let rects = cell_rects(ViewLayout::Grid, 2, a);

        // Focused cell (index 1) top-left corner is bold + focused color.
        let f = rects[1];
        let fc = &buf[f.y as usize][f.x as usize];
        assert_eq!(fc.fg, FOCUSED_BORDER);
        assert!(fc.bold);

        // Unfocused cell (index 0) corner is not.
        let u = rects[0];
        let uc = &buf[u.y as usize][u.x as usize];
        assert_eq!(uc.fg, UNFOCUSED_BORDER);
        assert!(!uc.bold);
    }

    #[test]
    fn composite_bottom_anchors_tall_snapshot() {
        // Snapshot taller than the cell interior: first rows carry 'T', last
        // row carries 'L'. Bottom-anchoring must show 'L', never 'T'.
        let cols = 78u16;
        let rows = 100u16;
        let mut cells = vec![
            vec![
                RenderCell {
                    c: 'T',
                    ..RenderCell::default()
                };
                cols as usize
            ];
            rows as usize
        ];
        // Mark the very last row distinctly (latest output) and the very first
        // row (oldest, must scroll off).
        for cell in cells.last_mut().unwrap() {
            cell.c = 'L';
        }
        for cell in cells.first_mut().unwrap() {
            cell.c = 'X';
        }
        let snap = PaneSnapshot { cols, rows, cells };
        let view = ClientView {
            name: "v".into(),
            cells: vec![cell_with(1, Some(snap))],
            layout: ViewLayout::Grid,
            focused: 0,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);

        // Interior bottom row (just above the box border) must be 'L'.
        let inner_bottom = a.height as usize - 2;
        assert_eq!(buf[inner_bottom][2].c, 'L');
        // 'X' (the snapshot's very top row) must have scrolled off the top.
        let has_x = buf.iter().any(|row| row.iter().any(|c| c.c == 'X'));
        assert!(
            !has_x,
            "tall snapshot should be bottom-anchored, top row 'X' leaked in"
        );
    }

    #[test]
    fn composite_short_snapshot_top_aligns() {
        // Snapshot shorter than the interior: it should sit at the top of the
        // interior (row iy), not be pushed to the bottom.
        let snap = snap_filled(78, 3, 'S');
        let view = ClientView {
            name: "v".into(),
            cells: vec![cell_with(1, Some(snap))],
            layout: ViewLayout::Grid,
            focused: 0,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);
        // Interior starts at row 1 (under the top border).
        assert_eq!(buf[1][2].c, 'S');
    }

    #[test]
    fn composite_empty_snapshot_shows_placeholder() {
        let view = ClientView {
            name: "v".into(),
            cells: vec![cell_with(42, None)],
            layout: ViewLayout::Grid,
            focused: 0,
        };
        let buf = composite(&view, area(80, 24));
        // The pane id appears somewhere in the placeholder label.
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains("42"));
    }

    #[test]
    fn monocle_draws_only_focused_cell() {
        let view = ClientView {
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(78, 24, 'A'))),
                cell_with(2, Some(snap_filled(78, 24, 'B'))),
            ],
            layout: ViewLayout::Monocle,
            focused: 1,
        };
        let buf = composite(&view, area(80, 24));
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains('B'));
        assert!(
            !joined.contains('A'),
            "monocle must hide the unfocused cell"
        );
    }

    #[test]
    fn layout_next_cycles() {
        assert_eq!(ViewLayout::Grid.next(), ViewLayout::Monocle);
        assert_eq!(ViewLayout::Monocle.next(), ViewLayout::Grid);
    }

    #[test]
    fn move_focus_grid_navigation() {
        // 4 cells => 2x2 grid (cols=2). Focus starts at 0 (top-left).
        let mut view = ClientView {
            name: "v".into(),
            cells: (1..=4).map(|id| cell_with(id, None)).collect(),
            layout: ViewLayout::Grid,
            focused: 0,
        };
        assert!(view.move_focus(FocusDirection::Right));
        assert_eq!(view.focused, 1);
        assert!(view.move_focus(FocusDirection::Down));
        assert_eq!(view.focused, 3);
        assert!(view.move_focus(FocusDirection::Left));
        assert_eq!(view.focused, 2);
        assert!(view.move_focus(FocusDirection::Up));
        assert_eq!(view.focused, 0);
        // At an edge: no movement, returns false.
        assert!(!view.move_focus(FocusDirection::Left));
        assert_eq!(view.focused, 0);
        assert!(!view.move_focus(FocusDirection::Up));
        assert_eq!(view.focused, 0);
    }

    #[test]
    fn move_focus_empty_is_noop() {
        let mut view = ClientView::new("v".into());
        assert!(!view.move_focus(FocusDirection::Right));
        assert_eq!(view.focused, 0);
    }

    #[test]
    fn clamp_focus_after_removal() {
        let mut view = ClientView {
            name: "v".into(),
            cells: (1..=3).map(|id| cell_with(id, None)).collect(),
            layout: ViewLayout::Grid,
            focused: 2,
        };
        view.cells.pop();
        view.clamp_focus();
        assert_eq!(view.focused, 1);
        view.cells.clear();
        view.clamp_focus();
        assert_eq!(view.focused, 0);
    }
}
