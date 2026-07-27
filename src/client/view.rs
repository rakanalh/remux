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
//! Sizing note: cells render a pane's snapshot clipped and letterboxed into the
//! cell rect, bottom-anchored so the latest output is visible. Under Model B
//! (focus-to-zoom) only the FOCUSED cell demands a size from its source pane
//! (the pane reflows to fit it, via the server's min-across-viewers sizing);
//! unfocused cells watch read-only and impose no size demand, so merely watching
//! never reflows the shared pane.

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
    /// Source pane's cursor position (clamped into `cols`/`rows`) and
    /// visibility. Only the focused cell renders it.
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    /// The source pane's DECCKM state, used to encode input to a focused cell.
    pub application_cursor_keys: bool,
}

/// One cell of a view: a reference to a real pane on a specific connection,
/// plus the most recent snapshot received for it (`None` until the first
/// `PaneContent` arrives).
///
/// A cell has three observable states, distinguished without a separate enum:
/// - **waiting**: `snapshot == None && !disconnected` — subscribed but no
///   `PaneContent` has arrived yet (shows `waiting for <title>…`).
/// - **live**: `snapshot == Some(_)` — compositing the latest snapshot.
/// - **disconnected**: `disconnected == true` — the source connection dropped
///   (or a send to it failed); shows `disconnected` and takes no more input.
#[derive(Debug, Clone)]
pub struct ViewCell {
    pub conn: ConnId,
    pub pane_id: PaneId,
    pub snapshot: Option<PaneSnapshot>,
    /// Set when the cell's source connection is gone (a send failed or the
    /// connection closed). A disconnected cell renders a `disconnected` label
    /// and silently drops keystrokes instead of crashing the client.
    pub disconnected: bool,
    /// `session / tab` title for the cell's source pane, learned from
    /// `PaneContent`. `None` until the first snapshot; kept live so a rename on
    /// the source updates the border label. Remote cells are host-prefixed by
    /// the compositor, not here.
    pub title: Option<String>,
}

impl ViewCell {
    /// A fresh cell aliasing `(conn, pane_id)`, waiting for its first snapshot.
    pub fn new(conn: ConnId, pane_id: PaneId) -> Self {
        Self {
            conn,
            pane_id,
            snapshot: None,
            disconnected: false,
            title: None,
        }
    }
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
    if w == 0 || h == 0 {
        return buf;
    }
    // A view with no cells (freshly created, or all removed) shows a centered
    // hint rather than a blank screen. `draw_centered` clamps the label to the
    // available width, so it degrades gracefully on a tiny terminal.
    if view.cells.is_empty() {
        draw_centered(&mut buf, 0, 0, w, h, "Add panes to this view");
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

/// Index of the cell whose rect contains the point `(x, y)`, for mouse-click
/// focus. `None` when the view is empty or the click lands outside every cell.
/// In `Monocle` only the focused cell is visible, so any in-bounds click keeps
/// the current focus.
pub fn cell_at(view: &ClientView, area: Rect, x: u16, y: u16) -> Option<usize> {
    if view.cells.is_empty() {
        return None;
    }
    match view.layout {
        ViewLayout::Monocle => Some(view.focused),
        ViewLayout::Grid => cell_rects(ViewLayout::Grid, view.cells.len(), area)
            .iter()
            .position(|r| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height),
    }
}

/// Buffer position `(x, y)` of the FOCUSED cell's terminal cursor, if it
/// should be shown. Only the focused cell shows a cursor, and only when its
/// snapshot's cursor is visible and falls within the (clipped, bottom-anchored)
/// interior. Returns `None` (cursor hidden) otherwise -- unfocused cells,
/// disconnected cells, no snapshot, a hidden source cursor, or a cursor
/// scrolled/clipped out of view. Mirrors [`draw_cell`]'s geometry exactly so
/// the cursor lands on the character it addresses.
pub fn focused_cursor(view: &ClientView, area: Rect) -> Option<(u16, u16)> {
    let n = view.cells.len();
    if n == 0 {
        return None;
    }
    let cell = view.cells.get(view.focused)?;
    if cell.disconnected {
        return None;
    }
    let snap = cell.snapshot.as_ref()?;
    if !snap.cursor_visible {
        return None;
    }
    let rect = match view.layout {
        ViewLayout::Monocle => area,
        ViewLayout::Grid => *cell_rects(ViewLayout::Grid, n, area).get(view.focused)?,
    };
    // Rect and interior, in buffer-local coordinates (area origin subtracted),
    // matching draw_cell.
    let rx = (rect.x as usize).saturating_sub(area.x as usize);
    let ry = (rect.y as usize).saturating_sub(area.y as usize);
    let rw = rect.width as usize;
    let rh = rect.height as usize;
    if rw == 0 || rh == 0 {
        return None;
    }
    let draw_border = rw >= 2 && rh >= 2;
    let (ix, iy, iw, ih) = if draw_border {
        (rx + 1, ry + 1, rw - 2, rh - 2)
    } else {
        (rx, ry, rw, rh)
    };
    if iw == 0 || ih == 0 {
        return None;
    }
    // Bottom-anchoring: the shown window is rows `start..sr` of the snapshot.
    let sr = snap.cells.len();
    let start = sr.saturating_sub(ih);
    let cy = snap.cursor_y as usize;
    let cx = snap.cursor_x as usize;
    if cy < start || cy >= sr || cx >= iw {
        return None; // scrolled above, off the bottom, or clipped right
    }
    let buf_x = ix + cx;
    let buf_y = iy + (cy - start);
    Some((buf_x as u16, buf_y as u16))
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
        // Label the top border with the cell's title (session / tab, learned
        // from PaneContent), or `waiting…` until the first snapshot arrives.
        let label = format!(" {} ", cell_title(cell));
        let max = rw.saturating_sub(2);
        for (i, ch) in label.chars().take(max).enumerate() {
            put(buf, ry, rx + 1 + i, border_cell(ch, focused));
        }
    }

    if iw == 0 || ih == 0 {
        return;
    }

    // A disconnected cell shows a centered `disconnected` label instead of a
    // (now stale) snapshot -- its source is gone.
    if cell.disconnected {
        draw_centered(buf, ix, iy, iw, ih, "disconnected");
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
            // No snapshot yet: centered `waiting for <title>…` placeholder.
            let label = format!("waiting for {}…", cell_title(cell));
            draw_centered(buf, ix, iy, iw, ih, &label);
        }
    }
}

/// The cell's border/label title: its `session / tab` (host-prefixed for
/// remotes) once known, else `waiting…` before the first snapshot.
fn cell_title(cell: &ViewCell) -> String {
    cell.title.clone().unwrap_or_else(|| "waiting…".to_string())
}

/// Draw `text` centered (horizontally and vertically) inside the interior
/// rect `(ix, iy, iw, ih)`, clipped to the interior width. Used for the
/// waiting / disconnected / empty-view placeholders.
fn draw_centered(
    buf: &mut [Vec<RenderCell>],
    ix: usize,
    iy: usize,
    iw: usize,
    ih: usize,
    text: &str,
) {
    if iw == 0 || ih == 0 {
        return;
    }
    let chars: Vec<char> = text.chars().take(iw).collect();
    let start_x = ix + (iw - chars.len()) / 2;
    let mid_y = iy + ih / 2;
    for (i, ch) in chars.into_iter().enumerate() {
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
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: false,
            application_cursor_keys: false,
        }
    }

    fn cell_with(pane_id: PaneId, snapshot: Option<PaneSnapshot>) -> ViewCell {
        ViewCell {
            conn: ConnId::Local,
            pane_id,
            snapshot,
            disconnected: false,
            title: None,
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
        let snap = PaneSnapshot {
            cols,
            rows,
            cells,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: false,
            application_cursor_keys: false,
        };
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
        // A cell with no snapshot yet shows a `waiting…` placeholder.
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains("waiting"));
    }

    #[test]
    fn composite_disconnected_cell_shows_label() {
        let mut cell = cell_with(1, Some(snap_filled(40, 20, 'A')));
        cell.disconnected = true;
        let view = ClientView {
            name: "v".into(),
            cells: vec![cell],
            layout: ViewLayout::Grid,
            focused: 0,
        };
        let buf = composite(&view, area(80, 24));
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains("disconnected"));
        // The stale snapshot content must NOT bleed through.
        assert!(!joined.contains('A'));
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
    fn cell_at_hit_tests_grid() {
        // 2 cells side-by-side across an 80-wide area: left half -> 0, right -> 1.
        let view = ClientView {
            name: "v".into(),
            cells: vec![cell_with(1, None), cell_with(2, None)],
            layout: ViewLayout::Grid,
            focused: 0,
        };
        let a = area(80, 24);
        let rects = cell_rects(ViewLayout::Grid, 2, a);
        // A point inside each rect resolves to that index.
        for (i, r) in rects.iter().enumerate() {
            let x = r.x + r.width / 2;
            let y = r.y + r.height / 2;
            assert_eq!(cell_at(&view, a, x, y), Some(i));
        }
        // Empty view: no hit.
        let empty = ClientView::new("e".into());
        assert_eq!(cell_at(&empty, a, 10, 10), None);
    }

    #[test]
    fn cell_at_monocle_keeps_focus() {
        let view = ClientView {
            name: "v".into(),
            cells: vec![cell_with(1, None), cell_with(2, None)],
            layout: ViewLayout::Monocle,
            focused: 1,
        };
        assert_eq!(cell_at(&view, area(80, 24), 5, 5), Some(1));
    }

    #[test]
    fn focused_cursor_only_when_visible_and_focused() {
        let mut snap = snap_filled(40, 10, 'A');
        snap.cursor_visible = true;
        snap.cursor_x = 3;
        snap.cursor_y = 9; // last row of the snapshot
        let view = ClientView {
            name: "v".into(),
            cells: vec![cell_with(1, Some(snap.clone())), cell_with(2, Some(snap))],
            layout: ViewLayout::Grid,
            focused: 0,
        };
        let a = area(80, 24);
        // Focused cell 0: interior origin (ix=1, iy=1); snapshot (10 rows) fits in
        // the interior so start=0 -> cursor row = iy + 9, col = ix + 3.
        let rects = cell_rects(ViewLayout::Grid, 2, a);
        let f = rects[0];
        let got = focused_cursor(&view, a).expect("cursor shown");
        assert_eq!(got, (f.x + 1 + 3, f.y + 1 + 9));

        // A hidden source cursor -> no cursor.
        let mut hidden = snap_filled(40, 10, 'A');
        hidden.cursor_visible = false;
        let view2 = ClientView {
            name: "v".into(),
            cells: vec![cell_with(1, Some(hidden))],
            layout: ViewLayout::Grid,
            focused: 0,
        };
        assert_eq!(focused_cursor(&view2, a), None);

        // A disconnected focused cell -> no cursor.
        let mut cell = cell_with(1, Some(snap_filled(40, 10, 'A')));
        if let Some(s) = cell.snapshot.as_mut() {
            s.cursor_visible = true;
        }
        cell.disconnected = true;
        let view3 = ClientView {
            name: "v".into(),
            cells: vec![cell],
            layout: ViewLayout::Grid,
            focused: 0,
        };
        assert_eq!(focused_cursor(&view3, a), None);
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

    #[test]
    fn composite_empty_view_shows_hint_full_size() {
        // A view with zero cells (e.g. a freshly-created `w n` view) must
        // composite to a full-size buffer without panicking, and show a centered
        // "Add panes to this view" hint. The full-size invariant lets an overlay
        // (session manager, view picker) render on top of an empty view:
        // `paint_view` blits this buffer, then lays the overlay over it. Both
        // Grid and Monocle must hold.
        for layout in [ViewLayout::Grid, ViewLayout::Monocle] {
            let view = ClientView {
                name: "empty".into(),
                cells: vec![],
                layout,
                focused: 0,
            };
            let a = area(80, 24);
            let buf = composite(&view, a);
            assert_eq!(buf.len(), a.height as usize, "row count for {layout:?}");
            assert!(
                buf.iter().all(|row| row.len() == a.width as usize),
                "col count for {layout:?}"
            );
            let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
            assert!(
                joined.contains("Add panes to this view"),
                "empty-view hint missing for {layout:?}"
            );
        }
    }

    #[test]
    fn composite_empty_view_hint_clamps_on_tiny_area() {
        // On an area too small for the full label it must not panic and must
        // stay within bounds (clamped).
        let view = ClientView::new("empty".into());
        for (w, h) in [(1u16, 1u16), (5, 1), (10, 3), (0, 0)] {
            let buf = composite(&view, area(w, h));
            assert_eq!(buf.len(), h as usize);
            assert!(buf.iter().all(|row| row.len() == w as usize));
        }
    }
}
