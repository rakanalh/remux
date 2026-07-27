//! Client-side "View": a virtual tab whose cells alias real panes.
//!
//! A [`ClientView`] is a purely client-side construct. It does not exist on any
//! server; instead it references a set of real panes (each identified by the
//! connection it lives on plus its [`PaneId`]) and composites the per-pane
//! [`PaneContent`](crate::protocol::ServerMessage::PaneContent) snapshots the
//! server streams for those panes into a single grid. The event loop owns the
//! list of views and feeds fresh snapshots in as they arrive; everything in
//! this module is pure geometry + buffer composition so it can be unit-tested
//! headlessly (no terminal, no sockets). The one exception is
//! [`draw_status_bar`], which takes an already-resolved
//! [`CompositorTheme`](crate::config::theme::CompositorTheme) of `CellColor`s so
//! the bottom bar mirrors the normal (server) status bar.
//!
//! Layout note: a view arranges its cells with the SAME automatic layout engine
//! the server uses ([`LayoutMode`]: Bsp / Master / Monocle / Grid). Each cell is
//! treated as a pseudo-pane whose id is its index, so the engine's `build_tree`
//! and `compute_layout` place the cells; Monocle shows only the focused cell.
//! The bottom row of the terminal is reserved for the view's status bar (see
//! [`cells_area`]), so cell rects never overwrite it.
//!
//! Sizing note: cells render a pane's snapshot clipped and letterboxed into the
//! cell rect, bottom-anchored so the latest output is visible. Under Model B
//! (focus-to-zoom) only the FOCUSED cell demands a size from its source pane
//! (the pane reflows to fit it, via the server's min-across-viewers sizing);
//! unfocused cells watch read-only and impose no size demand, so merely watching
//! never reflows the shared pane.

use crate::client::registry::ConnId;
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, PaneId, RenderCell};
use crate::server::layout::{compute_layout, FocusDirection, GridLayout, LayoutMode, Rect};

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

/// A client-side virtual tab compositing several panes.
#[derive(Debug, Clone)]
pub struct ClientView {
    pub name: String,
    pub cells: Vec<ViewCell>,
    /// How the cells are arranged. Reuses the server's automatic layout engine
    /// (Bsp / Master / Monocle / Grid); [`LayoutMode::next`] cycles through them
    /// (Custom is excluded). Defaults to Grid.
    pub layout: LayoutMode,
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
            layout: LayoutMode::Grid(GridLayout),
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
    /// geometry as the Grid layout, so focus tracks a grid regardless of the
    /// active layout. Returns `true` if the focused cell actually changed.
    ///
    /// This is deliberately layout-agnostic: under Bsp/Master the geometric
    /// neighbor may not be the grid neighbor, but keeping one predictable
    /// paging model across layouts is simpler and matches the previous
    /// behavior. Monocle has only one visible cell, but focus still moves
    /// through the underlying cell list (left/up = previous, right/down = next)
    /// so the user can page between panes.
    pub fn move_focus(&mut self, dir: FocusDirection) -> bool {
        let n = self.cells.len();
        if n == 0 {
            return false;
        }
        if matches!(self.layout, LayoutMode::Monocle(_)) {
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

/// The sub-rectangle of `area` available to cells: `area` minus the bottom row,
/// which is reserved for the view's status bar. Everything that positions cells
/// ([`cell_rects`], and therefore [`composite`], [`cell_at`],
/// [`focused_cursor`] and the subscription sizing) goes through this, so cells
/// can never overwrite the status row.
pub fn cells_area(area: Rect) -> Rect {
    Rect {
        height: area.height.saturating_sub(1),
        ..area
    }
}

/// The 1-row strip at the TOP of the cell area reserved, in `Monocle` only, for
/// a tab-like list of every cell's title (mirroring a regular Monocle tab's
/// stacked-pane strip). Returns that rect, or `None` for non-Monocle layouts and
/// for a cell area too short to host both the strip and a cell below it
/// (`cells_area().height < 2`). The focused cell renders BELOW this strip; see
/// [`cell_rects`], which subtracts it so the focused cell never overwrites it.
pub fn monocle_strip_rect(layout: &LayoutMode, area: Rect) -> Option<Rect> {
    if !matches!(layout, LayoutMode::Monocle(_)) {
        return None;
    }
    let inner = cells_area(area);
    if inner.height < 2 {
        return None;
    }
    Some(Rect { height: 1, ..inner })
}

/// Compute the outer rectangle for each of `n` cells within `area` (minus the
/// reserved status row), using the automatic layout engine.
///
/// Each cell index `i` is treated as a pseudo-[`PaneId`] (`i as PaneId`); the
/// layout's `build_tree` places those pseudo-panes and [`compute_layout`] turns
/// the tree into rects. The result is indexed by cell: `out[i]` is `Some(rect)`
/// when cell `i` is visible in the current layout, or `None` when it is hidden
/// (Monocle hides every cell except the focused one).
///
/// Returns a vector of exactly `n` entries (empty when `n == 0`).
pub fn cell_rects(layout: &LayoutMode, focused: usize, n: usize, area: Rect) -> Vec<Option<Rect>> {
    if n == 0 {
        return Vec::new();
    }
    let mut inner = cells_area(area);
    // Monocle reserves the top row of the cell area for the title strip, so the
    // focused cell tiles the region BELOW it (never overwriting the strip).
    if let Some(strip) = monocle_strip_rect(layout, area) {
        inner.y = inner.y.saturating_add(strip.height);
        inner.height = inner.height.saturating_sub(strip.height);
    }
    let ids: Vec<PaneId> = (0..n).map(|i| i as PaneId).collect();
    let tree = layout.build_tree(&ids, focused as PaneId);
    let placed = compute_layout(&tree, inner, 0);
    let mut out = vec![None; n];
    for (pid, rect) in placed {
        let idx = pid as usize;
        if idx < n {
            out[idx] = Some(rect);
        }
    }
    out
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
/// Cells are placed within [`cells_area`] (the terminal minus the reserved
/// status row); the status row itself is drawn separately by
/// [`draw_status_bar`]. Each cell gets a box border (the focused cell's border
/// is drawn bold in a distinct color) and its snapshot is blitted
/// bottom-anchored into the box's interior, clipped to the interior's
/// width/height. Cells with no snapshot yet show a centered placeholder label.
/// In `Monocle` only the focused cell is drawn.
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
    let inner = cells_area(area);
    // A view with no cells (freshly created, or all removed) shows a centered
    // hint rather than a blank screen. `draw_centered` clamps the label to the
    // available width, so it degrades gracefully on a tiny terminal. Keep it
    // within the cell region so it never lands on the status row.
    if view.cells.is_empty() {
        let ih = (inner.height as usize).max(1);
        draw_centered(
            &mut buf,
            0,
            0,
            inner.width as usize,
            ih,
            "Add panes to this view",
        );
        return buf;
    }

    let rects = cell_rects(&view.layout, view.focused, view.cells.len(), area);
    for (i, cell) in view.cells.iter().enumerate() {
        if let Some(Some(rect)) = rects.get(i) {
            draw_cell(&mut buf, area, *rect, cell, i == view.focused);
        }
    }
    // Monocle shows only the focused cell, so draw a top strip listing EVERY
    // cell's title (like a regular Monocle tab's stacked-pane strip) to reveal
    // the panes the user can page to. Drawn LAST so it always wins the reserved
    // row even if the cell geometry above ever regressed.
    if let Some(strip) = monocle_strip_rect(&view.layout, area) {
        draw_monocle_strip(&mut buf, area, strip, view);
    }
    buf
}

/// Index of the cell whose rect contains the point `(x, y)`, for mouse-click
/// focus. `None` when the view is empty or the click lands outside every cell
/// (including a click on the reserved status row). In `Monocle` only the
/// focused cell has a rect below the strip, but a click landing on a title in
/// the top strip resolves to THAT cell, so clicking a strip entry pages to it;
/// any other in-bounds click resolves to the focused cell.
pub fn cell_at(view: &ClientView, area: Rect, x: u16, y: u16) -> Option<usize> {
    if view.cells.is_empty() {
        return None;
    }
    // Monocle title strip: a click on a strip entry focuses that cell. The strip
    // (row `strip.y`) and the focused cell rect (below it) never overlap, so
    // checking it first is unambiguous.
    if let Some(strip) = monocle_strip_rect(&view.layout, area) {
        if y == strip.y && x >= strip.x && x < strip.x + strip.width {
            let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
            let rel = (x - strip.x) as usize;
            if let Some((idx, _, _)) = strip_segments(&titles, strip.width as usize)
                .into_iter()
                .find(|(_, s, e)| rel >= *s && rel < *e)
            {
                return Some(idx);
            }
        }
    }
    let rects = cell_rects(&view.layout, view.focused, view.cells.len(), area);
    rects.iter().position(|r| match r {
        Some(rect) => {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        }
        None => false,
    })
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
    let rects = cell_rects(&view.layout, view.focused, n, area);
    let rect = rects.get(view.focused).copied().flatten()?;
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

/// Lay out the Monocle title strip's tab entries within `width` columns.
/// Returns `(cell_index, start, end)` column offsets (relative to the strip's
/// left edge, `end` exclusive) for every entry that is at least partially
/// visible; entries past the right edge are dropped and the last visible one is
/// clipped. Each entry renders as ` {title} ` with a single separator column
/// between entries. Shared by [`draw_monocle_strip`] (rendering) and
/// [`cell_at`] (hit-testing) so a click always lands on the entry drawn there.
fn strip_segments(titles: &[String], width: usize) -> Vec<(usize, usize, usize)> {
    let mut segs = Vec::new();
    let mut x = 0usize;
    for (i, title) in titles.iter().enumerate() {
        if i > 0 {
            // Separator column between entries.
            if x >= width {
                break;
            }
            x += 1;
        }
        if x >= width {
            break;
        }
        let label_len = title.chars().count() + 2; // one padding space each side
        let start = x;
        let end = (x + label_len).min(width);
        segs.push((i, start, end));
        x = end;
    }
    segs
}

/// Draw the Monocle title strip on `strip` (the reserved top row of the cell
/// area): a tab-like list of EVERY cell's title, with the focused cell's entry
/// highlighted in the focused-border style so it matches the rest of the UI.
/// Theme-free by design (like the rest of [`composite`]); only reuses the
/// `FOCUSED_BORDER`/`UNFOCUSED_BORDER` colors the cell borders already use.
fn draw_monocle_strip(buf: &mut [Vec<RenderCell>], area: Rect, strip: Rect, view: &ClientView) {
    let by = (strip.y as usize).saturating_sub(area.y as usize);
    let bx = (strip.x as usize).saturating_sub(area.x as usize);
    let width = strip.width as usize;
    if width == 0 {
        return;
    }
    let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
    let segs = strip_segments(&titles, width);
    for (idx, start, end) in segs {
        let focused = idx == view.focused;
        // Separator before every entry but the first.
        if start > 0 {
            put(buf, by, bx + start - 1, border_cell('\u{2502}', false));
        }
        let label = format!(" {} ", titles[idx]);
        for (i, ch) in label.chars().enumerate() {
            if start + i >= end {
                break;
            }
            put(buf, by, bx + start + i, border_cell(ch, focused));
        }
    }
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
// Status bar
// ---------------------------------------------------------------------------

/// Write `s` into `row` starting at column `x` (clamped to `cols`), styled with
/// `fg`/`bg`/`bold`. Returns the column just past the written text.
fn put_str(
    row: &mut [RenderCell],
    mut x: usize,
    cols: usize,
    s: &str,
    fg: &CellColor,
    bg: &CellColor,
    bold: bool,
) -> usize {
    for ch in s.chars() {
        if x < cols && x < row.len() {
            row[x] = RenderCell {
                c: ch,
                fg: fg.clone(),
                bg: bg.clone(),
                bold,
                ..RenderCell::default()
            };
        }
        x += 1;
    }
    x
}

/// Draw the view's status bar on the LAST row of `area` (the row reserved by
/// [`cells_area`]). Mirrors the normal (server) status bar's left/right layout:
/// the input `mode` (`[NORMAL]`, themed like the real bar), the `view_name`,
/// the focused `cell_title` (`session / tab`, host-prefixed for remotes), and
/// the `layout_name` (bsp/master/monocle/grid) right-aligned.
///
/// Takes an already-resolved [`CompositorTheme`] so the colors match the normal
/// bar exactly (built from the same `ThemeConfig`).
pub fn draw_status_bar(
    buf: &mut [Vec<RenderCell>],
    area: Rect,
    mode: &str,
    view_name: &str,
    cell_title: Option<&str>,
    layout_name: &str,
    theme: &CompositorTheme,
) {
    let cols = area.width as usize;
    if cols == 0 || area.height == 0 {
        return;
    }
    let bar_row = (area.height - 1) as usize;
    let row = match buf.get_mut(bar_row) {
        Some(r) => r,
        None => return,
    };

    // Fill the bar background.
    let end = cols.min(row.len());
    for slot in row.iter_mut().take(end) {
        *slot = RenderCell {
            c: ' ',
            fg: theme.status_bar_fg.clone(),
            bg: theme.status_bar_bg.clone(),
            ..RenderCell::default()
        };
    }

    // Left side: [MODE] view_name │ cell_title
    let (mode_fg, mode_bg) = theme.mode_colors(mode);
    let mut x = 0;
    x = put_str(
        row,
        x,
        cols,
        &format!(" [{mode}] "),
        &mode_fg,
        &mode_bg,
        true,
    );
    x = put_str(
        row,
        x,
        cols,
        &format!(" {view_name} "),
        &theme.session_name_fg,
        &theme.status_bar_bg,
        false,
    );
    x = put_str(
        row,
        x,
        cols,
        "\u{2502}",
        &theme.separator_fg,
        &theme.status_bar_bg,
        false,
    );
    if let Some(title) = cell_title {
        x = put_str(
            row,
            x,
            cols,
            &format!(" {title} "),
            &theme.status_bar_fg,
            &theme.status_bar_bg,
            false,
        );
    }

    // Right side: the layout name, right-aligned when there is room; otherwise
    // just appended after the left content.
    let layout_seg = format!(" {layout_name} ");
    let lw = layout_seg.chars().count();
    let start = if cols > lw && cols - lw > x {
        cols - lw
    } else {
        x
    };
    put_str(
        row,
        start,
        cols,
        &layout_seg,
        &theme.session_name_fg,
        &theme.status_bar_bg,
        true,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::layout::MonocleLayout;

    fn area(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    fn gridv() -> LayoutMode {
        LayoutMode::Grid(GridLayout)
    }

    fn monoclev() -> LayoutMode {
        LayoutMode::Monocle(MonocleLayout)
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
    fn cells_area_reserves_status_row() {
        assert_eq!(cells_area(area(80, 24)), area(80, 23));
        // Degenerate heights never underflow.
        assert_eq!(cells_area(area(80, 1)).height, 0);
        assert_eq!(cells_area(area(80, 0)).height, 0);
    }

    #[test]
    fn cell_rects_returns_n_entries() {
        for n in 0..=5 {
            assert_eq!(cell_rects(&gridv(), 0, n, area(80, 24)).len(), n);
            assert_eq!(cell_rects(&monoclev(), 0, n, area(80, 24)).len(), n);
        }
    }

    #[test]
    fn cell_rects_within_cells_area() {
        let a = area(80, 24);
        let inner = cells_area(a);
        for n in 1..=5 {
            for r in cell_rects(&gridv(), 0, n, a).into_iter().flatten() {
                assert!(r.x >= inner.x && r.y >= inner.y);
                assert!(r.x + r.width <= inner.x + inner.width);
                assert!(r.y + r.height <= inner.y + inner.height);
                assert!(r.width > 0 && r.height > 0);
            }
        }
    }

    #[test]
    fn cell_rects_grid_tiles_without_gaps_or_overlap() {
        // With gap 0 the layout engine tiles the cell area exactly. Paint a
        // coverage grid over `cells_area`; every pixel must be covered once.
        let a = area(37, 19); // deliberately not divisible, exercises remainder
        let inner = cells_area(a);
        for n in 1..=5 {
            let mut cover = vec![vec![0u8; inner.width as usize]; inner.height as usize];
            for r in cell_rects(&gridv(), 0, n, a).into_iter().flatten() {
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
    fn monocle_only_focused_has_rect() {
        let a = area(80, 24);
        let rects = cell_rects(&monoclev(), 2, 4, a);
        assert_eq!(rects.len(), 4);
        // Only the focused cell (index 2) is placed; it fills the cell area
        // BELOW the reserved title strip (top row), never on the strip row.
        let strip = monocle_strip_rect(&monoclev(), a).unwrap();
        let below = Rect {
            y: strip.y + strip.height,
            height: cells_area(a).height - strip.height,
            ..cells_area(a)
        };
        assert_eq!(rects[2], Some(below));
        assert!(below.y > strip.y, "focused cell sits below the strip");
        for (i, r) in rects.iter().enumerate() {
            if i != 2 {
                assert!(r.is_none(), "cell {i} should be hidden in monocle");
            }
        }
    }

    #[test]
    fn monocle_strip_rect_reserves_top_row() {
        // Monocle: a 1-row strip at the top of the cell area.
        assert_eq!(
            monocle_strip_rect(&monoclev(), area(80, 24)),
            Some(area(80, 1))
        );
        // Non-Monocle layouts have no strip.
        assert_eq!(monocle_strip_rect(&gridv(), area(80, 24)), None);
        // Height 3: cell area is 2 rows -> strip (1) + a cell row (1).
        assert_eq!(
            monocle_strip_rect(&monoclev(), area(80, 3)),
            Some(area(80, 1))
        );
        // No room (cell area height < 2) -> no strip, so a cell can still show.
        assert_eq!(monocle_strip_rect(&monoclev(), area(80, 2)), None);
        assert_eq!(monocle_strip_rect(&monoclev(), area(80, 1)), None);
        assert_eq!(monocle_strip_rect(&monoclev(), area(80, 0)), None);
    }

    #[test]
    fn monocle_strip_lists_every_title_focused_distinct() {
        let mut cells: Vec<ViewCell> = (0..3).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("alpha / Tab 1".into());
        cells[1].title = Some("beta / Tab 1".into());
        cells[2].title = Some("gamma / Tab 1".into());
        let view = ClientView {
            name: "v".into(),
            cells,
            layout: monoclev(),
            focused: 1,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);
        // Row 0 is the strip: it lists ALL three titles.
        let row0: String = buf[0].iter().map(|c| c.c).collect();
        assert!(
            row0.contains("alpha / Tab 1"),
            "strip missing alpha: {row0:?}"
        );
        assert!(
            row0.contains("beta / Tab 1"),
            "strip missing beta: {row0:?}"
        );
        assert!(
            row0.contains("gamma / Tab 1"),
            "strip missing gamma: {row0:?}"
        );
        // The strip carries no box-drawing cell corners (it is not a cell).
        assert!(!row0.contains('┌') && !row0.contains('┐'));
        // The focused entry (beta) is drawn in the focused-border style; an
        // unfocused entry (alpha) is not -> visually distinct.
        let bpos = row0.find("beta").unwrap();
        let apos = row0.find("alpha").unwrap();
        assert_eq!(buf[0][bpos].fg, FOCUSED_BORDER);
        assert!(buf[0][bpos].bold);
        assert_eq!(buf[0][apos].fg, UNFOCUSED_BORDER);
        assert!(!buf[0][apos].bold);
    }

    #[test]
    fn monocle_strip_lists_titles_even_when_focused_waiting() {
        // Focused cell has no snapshot yet (waiting) and an unfocused cell is
        // disconnected: the strip still lists every cell by title, and the
        // focused area shows the waiting placeholder (below the strip).
        let mut waiting = cell_with(0, None); // focused, no snapshot
        waiting.title = Some("alpha / Tab 1".into());
        let mut disconnected = cell_with(1, Some(snap_filled(40, 10, 'B')));
        disconnected.title = Some("beta / Tab 1".into());
        disconnected.disconnected = true;
        let view = ClientView {
            name: "v".into(),
            cells: vec![waiting, disconnected],
            layout: monoclev(),
            focused: 0,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);
        let row0: String = buf[0].iter().map(|c| c.c).collect();
        assert!(row0.contains("alpha / Tab 1"));
        assert!(row0.contains("beta / Tab 1"));
        // The focused (waiting) cell's placeholder shows BELOW the strip.
        let below: String = buf[1..]
            .iter()
            .flat_map(|row| row.iter().map(|c| c.c))
            .collect();
        assert!(
            below.contains("waiting"),
            "focused waiting placeholder missing"
        );
    }

    #[test]
    fn monocle_content_below_strip_not_on_it() {
        // The focused snapshot must never land on the strip row (row 0).
        let view = ClientView {
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(78, 24, 'A'))),
                cell_with(2, Some(snap_filled(78, 24, 'B'))),
            ],
            layout: monoclev(),
            focused: 0,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);
        // Row 0 is the strip; the focused snapshot char 'A' appears only below.
        assert!(
            !buf[0].iter().any(|c| c.c == 'A'),
            "content leaked onto strip"
        );
        let below_has_a = buf[1..].iter().any(|row| row.iter().any(|c| c.c == 'A'));
        assert!(below_has_a, "focused content missing below the strip");
    }

    #[test]
    fn monocle_strip_click_focuses_that_cell() {
        let mut cells: Vec<ViewCell> = (0..3).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("alpha / Tab 1".into());
        cells[1].title = Some("beta / Tab 1".into());
        cells[2].title = Some("gamma / Tab 1".into());
        let view = ClientView {
            name: "v".into(),
            cells,
            layout: monoclev(),
            focused: 0,
        };
        let a = area(80, 24);
        // Locate beta's entry on the strip via the same segment layout used to
        // draw it, then hit-test its middle column.
        let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
        let strip = monocle_strip_rect(&monoclev(), a).unwrap();
        let segs = strip_segments(&titles, strip.width as usize);
        let (_, s, e) = segs.iter().copied().find(|(i, _, _)| *i == 1).unwrap();
        let mid = strip.x + ((s + e) / 2) as u16;
        assert_eq!(cell_at(&view, a, mid, strip.y), Some(1));
        // A click below the strip resolves to the focused cell (0).
        assert_eq!(cell_at(&view, a, 5, 5), Some(0));
    }

    #[test]
    fn monocle_composite_tiny_area_with_cells_no_panic() {
        // Mirrors the empty-view tiny-area guard, but with cells: Monocle must
        // not panic on degenerate heights. Height 2 is the interesting case
        // (strip row 0, focused rect height 1 -> no border).
        let view = ClientView {
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(10, 3, 'A'))),
                cell_with(2, None),
            ],
            layout: monoclev(),
            focused: 0,
        };
        for (w, h) in [(1u16, 1u16), (5, 1), (10, 2), (10, 3), (0, 0)] {
            let buf = composite(&view, area(w, h));
            assert_eq!(buf.len(), h as usize);
            assert!(buf.iter().all(|row| row.len() == w as usize));
        }
    }

    #[test]
    fn grid_two_cells_split_left_right() {
        // The n=2 Grid case must remain a left/right split (the PTY harnesses
        // rely on it): cell 0 on the left half, cell 1 on the right half.
        let a = area(120, 40);
        let rects = cell_rects(&gridv(), 0, 2, a);
        let r0 = rects[0].expect("cell 0 placed");
        let r1 = rects[1].expect("cell 1 placed");
        assert_eq!(r0.x, 0);
        assert!(r1.x >= r0.width, "cell 1 must start at/after the split");
        // Both roughly half the width.
        assert!(r0.width >= 58 && r0.width <= 62);
        assert!(r1.width >= 58 && r1.width <= 62);
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
            layout: gridv(),
            focused: 0,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);
        let rects = cell_rects(&gridv(), 0, 2, a);

        // Interior center of cell 0 must be 'A', cell 1 must be 'B'.
        for (rect, marker) in [(rects[0].unwrap(), 'A'), (rects[1].unwrap(), 'B')] {
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
            layout: gridv(),
            focused: 1,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);
        let rects = cell_rects(&gridv(), 1, 2, a);

        // Focused cell (index 1) top-left corner is bold + focused color.
        let f = rects[1].unwrap();
        let fc = &buf[f.y as usize][f.x as usize];
        assert_eq!(fc.fg, FOCUSED_BORDER);
        assert!(fc.bold);

        // Unfocused cell (index 0) corner is not.
        let u = rects[0].unwrap();
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
            layout: gridv(),
            focused: 0,
        };
        let a = area(80, 24);
        let buf = composite(&view, a);

        // The single cell fills the cell area (area minus the status row); its
        // interior bottom row (just above the box border) must be 'L'.
        let inner_bottom = cells_area(a).height as usize - 2;
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
            layout: gridv(),
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
            layout: gridv(),
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
            layout: gridv(),
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
            layout: monoclev(),
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
            layout: gridv(),
            focused: 0,
        };
        let a = area(80, 24);
        let rects = cell_rects(&gridv(), 0, 2, a);
        // A point inside each rect resolves to that index.
        for (i, r) in rects.iter().enumerate() {
            let r = r.unwrap();
            let x = r.x + r.width / 2;
            let y = r.y + r.height / 2;
            assert_eq!(cell_at(&view, a, x, y), Some(i));
        }
        // A click on the reserved status row hits nothing.
        assert_eq!(cell_at(&view, a, 10, a.height - 1), None);
        // Empty view: no hit.
        let empty = ClientView::new("e".into());
        assert_eq!(cell_at(&empty, a, 10, 10), None);
    }

    #[test]
    fn cell_at_monocle_keeps_focus() {
        let view = ClientView {
            name: "v".into(),
            cells: vec![cell_with(1, None), cell_with(2, None)],
            layout: monoclev(),
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
            layout: gridv(),
            focused: 0,
        };
        let a = area(80, 24);
        // Focused cell 0: interior origin (ix=1, iy=1); snapshot (10 rows) fits in
        // the interior so start=0 -> cursor row = iy + 9, col = ix + 3.
        let rects = cell_rects(&gridv(), 0, 2, a);
        let f = rects[0].unwrap();
        let got = focused_cursor(&view, a).expect("cursor shown");
        assert_eq!(got, (f.x + 1 + 3, f.y + 1 + 9));

        // A hidden source cursor -> no cursor.
        let mut hidden = snap_filled(40, 10, 'A');
        hidden.cursor_visible = false;
        let view2 = ClientView {
            name: "v".into(),
            cells: vec![cell_with(1, Some(hidden))],
            layout: gridv(),
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
            layout: gridv(),
            focused: 0,
        };
        assert_eq!(focused_cursor(&view3, a), None);
    }

    #[test]
    fn layout_next_cycles_through_all_automatic_modes() {
        // Default Grid; next() walks the automatic modes and never yields Custom.
        let names: Vec<String> = {
            let mut m = LayoutMode::Grid(GridLayout);
            let mut out = Vec::new();
            for _ in 0..4 {
                m = m.next();
                out.push(m.name().to_string());
            }
            out
        };
        assert_eq!(names, vec!["bsp", "master", "monocle", "grid"]);
    }

    #[test]
    fn move_focus_grid_navigation() {
        // 4 cells => 2x2 grid (cols=2). Focus starts at 0 (top-left).
        let mut view = ClientView {
            name: "v".into(),
            cells: (1..=4).map(|id| cell_with(id, None)).collect(),
            layout: gridv(),
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
    fn move_focus_monocle_pages_through_cells() {
        let mut view = ClientView {
            name: "v".into(),
            cells: (1..=3).map(|id| cell_with(id, None)).collect(),
            layout: monoclev(),
            focused: 0,
        };
        assert!(view.move_focus(FocusDirection::Right));
        assert_eq!(view.focused, 1);
        assert!(view.move_focus(FocusDirection::Right));
        assert_eq!(view.focused, 2);
        assert!(!view.move_focus(FocusDirection::Right)); // at the end
        assert!(view.move_focus(FocusDirection::Left));
        assert_eq!(view.focused, 1);
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
            layout: gridv(),
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
        for layout in [gridv(), monoclev()] {
            let view = ClientView {
                name: "empty".into(),
                cells: vec![],
                layout,
                focused: 0,
            };
            let a = area(80, 24);
            let buf = composite(&view, a);
            assert_eq!(buf.len(), a.height as usize, "row count");
            assert!(
                buf.iter().all(|row| row.len() == a.width as usize),
                "col count"
            );
            let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
            assert!(
                joined.contains("Add panes to this view"),
                "empty-view hint missing"
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

    #[test]
    fn status_bar_shows_name_and_layout() {
        let a = area(80, 24);
        let mut buf = vec![vec![RenderCell::default(); a.width as usize]; a.height as usize];
        let theme = CompositorTheme::default();
        draw_status_bar(
            &mut buf,
            a,
            "NORMAL",
            "MyView",
            Some("alpha / Tab 1"),
            "grid",
            &theme,
        );
        let bar: String = buf[a.height as usize - 1].iter().map(|c| c.c).collect();
        assert!(bar.contains("[NORMAL]"), "mode missing: {bar:?}");
        assert!(bar.contains("MyView"), "view name missing: {bar:?}");
        assert!(bar.contains("alpha / Tab 1"), "title missing: {bar:?}");
        assert!(bar.contains("grid"), "layout name missing: {bar:?}");
    }

    #[test]
    fn status_bar_handles_no_title() {
        // An empty view (no focused cell title) must not panic.
        let a = area(40, 10);
        let mut buf = vec![vec![RenderCell::default(); a.width as usize]; a.height as usize];
        let theme = CompositorTheme::default();
        draw_status_bar(&mut buf, a, "NORMAL", "Empty", None, "grid", &theme);
        let bar: String = buf[a.height as usize - 1].iter().map(|c| c.c).collect();
        assert!(bar.contains("Empty"));
        assert!(bar.contains("grid"));
    }
}
