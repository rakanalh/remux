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
use crate::server::compositor::build_top_border_content;
use crate::server::layout::{
    compute_layout, focus_in_direction, FocusDirection, GridLayout, LayoutMode, LayoutNode, Rect,
};

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
    /// Whether the source pane is currently "session-visible" -- shown in the
    /// active tab of at least one attached client, so its real session drives it
    /// at full size. When `true`, the cell renders an "Active in session"
    /// placeholder instead of this (full-size) content, and sends no size demand.
    pub session_visible: bool,
}

/// One cell of a view: a reference to a real pane on a specific connection,
/// plus the most recent snapshot received for it (`None` until the first
/// `PaneContent` arrives).
///
/// A cell has four observable states, distinguished without a separate enum:
/// - **waiting**: `snapshot == None && !disconnected` — subscribed but no
///   `PaneContent` has arrived yet (shows `waiting for <title>…`).
/// - **active-in-session**: latest snapshot's `session_visible == true` — the
///   source pane is shown full-size in its real session, so the cell shows an
///   `● Active in <title>` placeholder instead of the streamed content and sends
///   no size demand (see [`ViewCell::is_session_visible`]).
/// - **live**: `snapshot == Some(_)` and not session-visible — compositing the
///   latest snapshot.
/// - **disconnected**: `disconnected == true` — the source connection dropped
///   (or a send to it failed); shows `disconnected` and takes no more input.
///
/// No state ever renders blank: each has a placeholder or content.
#[derive(Debug, Clone)]
pub struct ViewCell {
    /// Stable, per-view identity for this cell, assigned by
    /// [`ClientView::add_cell`] from a monotonic counter. Used as the
    /// pseudo-[`PaneId`] in the layout tree so that adding or removing cells
    /// (which shifts array indices) never invalidates a persistent
    /// `custom_tree`. Everything index-based (`focused`, `move_focus`,
    /// `cell_at`) keeps using the array index; only the tree keys off `id`.
    pub id: u64,
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
    ///
    /// The `id` is left 0 here; [`ClientView::add_cell`] is the sole owner of id
    /// assignment and stamps a unique id when it takes the cell. Construct cells
    /// only through `add_cell` so ids stay unique per view.
    pub fn new(conn: ConnId, pane_id: PaneId) -> Self {
        Self {
            id: 0,
            conn,
            pane_id,
            snapshot: None,
            disconnected: false,
            title: None,
        }
    }

    /// Whether the cell's source pane is currently session-visible (its latest
    /// snapshot reports it shown full-size in its real session). Such a cell
    /// renders the `● Active in <title>` placeholder, sends no size demand, and
    /// suppresses raw text input to the pane (view-management shortcuts still
    /// act on the view). `false` while `waiting` (no snapshot yet).
    pub fn is_session_visible(&self) -> bool {
        self.snapshot
            .as_ref()
            .map(|s| s.session_visible)
            .unwrap_or(false)
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
    /// A persistent, mutable arrangement of the cells, keyed by cell [`id`](ViewCell::id).
    /// `Some` once the user has manually resized ([`ResizeLeft`](crate::protocol::RemuxCommand::ResizeLeft) …)
    /// or moved ([`PaneMoveLeft`](crate::protocol::RemuxCommand::PaneMoveLeft) …) a
    /// cell; while `Some` the layout name reads `custom` and rects come from this
    /// tree instead of a fresh automatic build. `LayoutNext` resets it to `None`.
    pub custom_tree: Option<LayoutNode>,
    /// When `true`, only the focused cell is shown (filling the whole cell area);
    /// every other cell is hidden. Mirrors a normal tab's zoom. Independent of
    /// `layout`/`custom_tree`, exactly as the server's `zoomed_pane` is
    /// independent of `layout_mode`.
    pub zoomed: bool,
    /// Monotonic source for [`ViewCell::id`]. Only ever increments, so a removed
    /// cell's id is never reused within the view's lifetime.
    next_cell_id: u64,
}

impl ClientView {
    /// Create an empty view with the given name (Grid layout, no cells).
    pub fn new(name: String) -> Self {
        Self {
            name,
            cells: Vec::new(),
            layout: LayoutMode::Grid(GridLayout),
            focused: 0,
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1,
        }
    }

    /// Append a cell aliasing `(conn, pane_id)`, assigning it a fresh stable
    /// [`id`](ViewCell::id). When a `custom_tree` is active, the new cell is
    /// spliced into it by splitting the focused (or, if none, the last) leaf —
    /// the same way a normal tab splits when a pane is added — so the manual
    /// arrangement survives the add. With no `custom_tree` the automatic layout
    /// simply recomputes on the next render. Returns the new cell's id.
    pub fn add_cell(&mut self, conn: ConnId, pane_id: PaneId) -> u64 {
        let id = self.next_cell_id;
        self.next_cell_id += 1;
        let mut cell = ViewCell::new(conn, pane_id);
        cell.id = id;
        // Choose the leaf to split BEFORE pushing (so `focused` still indexes an
        // existing cell). Splitting the focused cell's leaf mirrors a tab split.
        let target = self
            .cells
            .get(self.focused)
            .or_else(|| self.cells.last())
            .map(|c| c.id);
        self.cells.push(cell);
        if let (Some(tree), Some(target_id)) = (self.custom_tree.as_mut(), target) {
            // Split the target leaf; the new cell becomes the split's second
            // child. Vertical (left/right) matches `LayoutNode::split_vertical`'s
            // default orientation for a fresh split.
            tree.split_vertical(target_id, id);
        }
        id
    }

    /// Build the automatic layout tree over the current cells (keyed by stable
    /// id, with the focused cell as the active pane). Used both to place cells
    /// when there is no `custom_tree` and to seed a `custom_tree` on the first
    /// manual resize/move.
    pub fn auto_tree(&self) -> LayoutNode {
        let ids: Vec<PaneId> = self.cells.iter().map(|c| c.id).collect();
        let focused_id = self.focused_id();
        self.layout.build_tree(&ids, focused_id)
    }

    /// The stable id of the focused cell, or `0` when the view is empty.
    pub fn focused_id(&self) -> u64 {
        self.cells.get(self.focused).map(|c| c.id).unwrap_or(0)
    }

    /// The array index of the cell with stable `id`, if present.
    pub fn index_of_id(&self, id: u64) -> Option<usize> {
        self.cells.iter().position(|c| c.id == id)
    }

    /// The layout name shown in the status bar: `custom` while a `custom_tree`
    /// is active, otherwise the automatic mode's name.
    pub fn layout_name(&self) -> &str {
        if self.custom_tree.is_some() {
            "custom"
        } else {
            self.layout.name()
        }
    }

    /// Prune the cell with stable `id` from `custom_tree` (if one is active),
    /// collapsing the tree via the normal close path. When the tree becomes
    /// empty it is cleared to `None` so the view falls back to automatic layout.
    pub fn prune_from_tree(&mut self, id: u64) {
        if let Some(tree) = self.custom_tree.as_mut() {
            if tree.close_pane(id).is_none() {
                self.custom_tree = None;
            }
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

    /// Move focus in the given direction. Returns `true` if the focused cell
    /// actually changed.
    ///
    /// `cells` is the already-reduced cell area (`cells_area(terminal)`), the
    /// SAME rect the cell rects are laid out in, so the neighbor search runs on
    /// the geometry the user sees.
    ///
    /// Monocle keeps a paging model: only the focused cell is visible, so
    /// left/up = previous cell, right/down = next cell through the underlying
    /// list. Every other layout (grid/bsp/master/custom) is geometry-driven via
    /// the server's [`focus_in_direction`] over the view's current cell tree
    /// (its `custom_tree` when set, else a fresh automatic tree over the cell
    /// pseudo-ids), so focus stays correct after a move/resize restructures the
    /// tree. The tree is cloned for the query so focus movement never mutates a
    /// persisted arrangement.
    pub fn move_focus(&mut self, dir: FocusDirection, cells: Rect) -> bool {
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
        // Geometry-driven: resolve the spatial neighbor on the real cell tree.
        let mut tree = match &self.custom_tree {
            Some(t) => t.clone(),
            None => self.auto_tree(),
        };
        let focused_id = self.focused_id();
        if let Some(new_id) = focus_in_direction(&mut tree, cells, focused_id, dir, 0) {
            if let Some(idx) = self.index_of_id(new_id) {
                let changed = idx != self.focused;
                self.focused = idx;
                return changed;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

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
pub fn monocle_strip_rect(view: &ClientView, area: Rect) -> Option<Rect> {
    // No strip when zoomed (the focused cell fills everything) or when a manual
    // `custom_tree` is active (cells are tiled, not stacked). Keeping the guard
    // here means `cell_rects`, `composite` and `cell_at` can never disagree
    // about whether the strip row exists.
    if view.zoomed || view.custom_tree.is_some() {
        return None;
    }
    if !matches!(view.layout, LayoutMode::Monocle(_)) {
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
/// Returns a vector of exactly `n` entries (empty when the view has no cells).
///
/// The cells are keyed by their stable [`ViewCell::id`], so an active
/// `custom_tree` (built once and mutated by resize/move) survives cell add/remove
/// without index shifts corrupting it. When `zoomed`, only the focused cell is
/// placed (filling the whole cell area); when a `custom_tree` is present it drives
/// the rects; otherwise a fresh automatic tree is built from the current ids.
pub fn cell_rects(view: &ClientView, area: Rect) -> Vec<Option<Rect>> {
    let n = view.cells.len();
    if n == 0 {
        return Vec::new();
    }
    // Zoom: the focused cell fills the entire cell area, everything else hidden.
    // The strip is suppressed while zoomed (see `monocle_strip_rect`), so the
    // focused cell gets the full `cells_area`.
    if view.zoomed {
        let mut out = vec![None; n];
        if view.focused < n {
            out[view.focused] = Some(cells_area(area));
        }
        return out;
    }
    let mut inner = cells_area(area);
    // Monocle reserves the top row of the cell area for the title strip, so the
    // focused cell tiles the region BELOW it (never overwriting the strip).
    if let Some(strip) = monocle_strip_rect(view, area) {
        inner.y = inner.y.saturating_add(strip.height);
        inner.height = inner.height.saturating_sub(strip.height);
    }
    // Persistent manual arrangement wins; otherwise recompute automatically.
    let tree = match &view.custom_tree {
        Some(t) => std::borrow::Cow::Borrowed(t),
        None => std::borrow::Cow::Owned(view.auto_tree()),
    };
    let placed = compute_layout(&tree, inner, 0);
    let mut out = vec![None; n];
    for (id, rect) in placed {
        if let Some(idx) = view.index_of_id(id) {
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
pub fn composite(
    view: &ClientView,
    area: Rect,
    theme: &CompositorTheme,
    mode: &str,
) -> Vec<Vec<RenderCell>> {
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

    let rects = cell_rects(view, area);
    for (i, cell) in view.cells.iter().enumerate() {
        if let Some(Some(rect)) = rects.get(i) {
            draw_cell(&mut buf, area, *rect, cell, i == view.focused);
        }
    }
    // Monocle shows only the focused cell, so draw a top strip listing EVERY
    // cell's title (like a regular Monocle tab's stacked-pane strip) to reveal
    // the panes the user can page to. Drawn LAST so it always wins the reserved
    // row even if the cell geometry above ever regressed.
    if let Some(strip) = monocle_strip_rect(view, area) {
        draw_monocle_strip(&mut buf, area, strip, view, theme, mode);
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
    if let Some(strip) = monocle_strip_rect(view, area) {
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
    let rects = cell_rects(view, area);
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
    // A session-visible cell renders a placeholder, not the pane's content, so it
    // shows no terminal cursor.
    if snap.session_visible {
        return None;
    }
    if !snap.cursor_visible {
        return None;
    }
    let rects = cell_rects(view, area);
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
        // The source pane is shown full-size in its real session: don't render
        // the (full-size, un-reflowed) streamed content into this smaller cell.
        // Show a centered placeholder naming where it's active instead.
        Some(snap) if snap.session_visible => {
            let label = format!("● Active in {}", cell_title(cell));
            draw_centered(buf, ix, iy, iw, ih, &label);
        }
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

/// Fixed per-tab width for the Monocle strip: the longest title plus one padding
/// column on each side (as the regular strip does: `max_name_len + 2`), capped
/// to the strip width. `0` when there are no titles.
///
/// This mirrors the `tab_width` computed inside
/// [`build_top_border_content`](crate::server::compositor::build_top_border_content):
/// `draw_monocle_strip` renders via that shared function while [`cell_at`]
/// hit-tests via [`strip_segments`], so both must derive tab boundaries from the
/// identical formula over the identical titles (`cell_title`, never empty).
fn strip_tab_width(titles: &[String], width: usize) -> usize {
    let max_name = titles.iter().map(|t| t.chars().count()).max().unwrap_or(0);
    (max_name + 2).min(width)
}

/// Lay out the Monocle title strip's tab entries within `width` columns,
/// mirroring the regular stacked-pane strip: a leading space, then equal-width
/// tabs (`strip_tab_width`) separated by a 3-column `" | "` separator.
/// Returns `(cell_index, start, end)` column offsets (relative to the strip's
/// left edge, `end` exclusive) for every entry at least partially visible;
/// entries past the right edge are dropped and the last visible one is clipped.
/// Used by [`cell_at`] for hit-testing; its boundaries match the columns
/// [`build_top_border_content`](crate::server::compositor::build_top_border_content)
/// paints in [`draw_monocle_strip`] (same leading space, same `" | "` 3-column
/// separator, same `strip_tab_width`), so a click always lands on the tab drawn
/// there.
fn strip_segments(titles: &[String], width: usize) -> Vec<(usize, usize, usize)> {
    let mut segs = Vec::new();
    let tab_width = strip_tab_width(titles, width);
    if tab_width == 0 {
        return segs;
    }
    let mut x = 1usize; // leading space before the first tab
    for i in 0..titles.len() {
        if i > 0 {
            // 3-column " | " separator between tabs.
            x += 3;
        }
        if x >= width {
            break;
        }
        let start = x;
        let end = (x + tab_width).min(width);
        segs.push((i, start, end));
        x = end;
    }
    segs
}

/// Draw the Monocle title strip on `strip` (the reserved top row of the cell
/// area): a tab-like list of EVERY cell's title, rendered by the SAME server
/// function a normal stacked pane's top border uses
/// ([`build_top_border_content`](crate::server::compositor::build_top_border_content)),
/// so the strip is pixel-identical to a normal Monocle tab's — fixed tab width,
/// the active tab filled with `theme.mode_colors(mode)`, inactive tabs on
/// `CellColor::Indexed(237)`, `" | "` separators.
///
/// The cells' [`ViewCell::id`]s serve as the pseudo-pane ids and the focused
/// cell index as the active index. `border_fg` is the focused-frame color
/// (`frame_active_fg`): the whole strip belongs to the focused view, exactly as
/// a focused pane's border does.
fn draw_monocle_strip(
    buf: &mut [Vec<RenderCell>],
    area: Rect,
    strip: Rect,
    view: &ClientView,
    theme: &CompositorTheme,
    mode: &str,
) {
    let by = (strip.y as usize).saturating_sub(area.y as usize);
    let bx = (strip.x as usize).saturating_sub(area.x as usize);
    let width = strip.width as usize;
    if width == 0 {
        return;
    }
    // Same titles vector `cell_at`/`strip_segments` hit-test against, so the
    // rendered tab boundaries and the click boundaries derive from identical
    // inputs. `cell_title` is never empty, so the pseudo-id fallback inside
    // `build_top_border_content` never fires and both `max_name_len`
    // computations provably agree.
    let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
    let pseudo_ids: Vec<PaneId> = view.cells.iter().map(|c| c.id).collect();
    let stack_info = Some((titles, pseudo_ids, view.focused));
    let border_fg = theme.frame_active_fg.clone();
    let cells = build_top_border_content(
        &stack_info,
        view.focused_id(),
        &border_fg,
        mode,
        width,
        theme,
    );
    for (i, cell) in cells.into_iter().enumerate() {
        if i >= width {
            break;
        }
        put(buf, by, bx + i, cell);
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
    use crate::server::layout::{
        all_pane_ids, find_neighbor, relocate_pane_to_edge, Direction, MonocleLayout,
    };

    fn area(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    /// Default compositor theme + `NORMAL` mode, for the tests that don't care
    /// about the strip's active-tab color.
    fn tt() -> CompositorTheme {
        CompositorTheme::default()
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
            session_visible: false,
        }
    }

    fn cell_with(pane_id: PaneId, snapshot: Option<PaneSnapshot>) -> ViewCell {
        // In tests the stable id mirrors the pane id (tests use distinct pane
        // ids), so tree keys stay unique.
        ViewCell {
            id: pane_id,
            conn: ConnId::Local,
            pane_id,
            snapshot,
            disconnected: false,
            title: None,
        }
    }

    /// Build a `ClientView` directly from a list of cells (test-only). Cells keep
    /// their own `id`s (see `cell_with`), so `custom_tree` stays `None` and rects
    /// come from the automatic layout.
    fn view_of(cells: Vec<ViewCell>, layout: LayoutMode, focused: usize) -> ClientView {
        ClientView {
            name: "v".into(),
            cells,
            layout,
            focused,
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        }
    }

    /// A view of `n` fresh cells (pane ids `1..=n`) in `layout`, focused on
    /// `focused`. Used by the geometry tests that previously called `cell_rects`
    /// with a bare `(layout, focused, n)`.
    fn view_n(n: usize, layout: LayoutMode, focused: usize) -> ClientView {
        let cells: Vec<ViewCell> = (1..=n as PaneId).map(|id| cell_with(id, None)).collect();
        view_of(cells, layout, focused)
    }

    // -- Prerequisite refactor: stable ids, custom_tree, layout_name ---------

    #[test]
    fn layout_name_is_custom_while_tree_present() {
        let mut v = view_n(2, gridv(), 0);
        assert_eq!(v.layout_name(), "grid");
        v.custom_tree = Some(v.auto_tree());
        assert_eq!(v.layout_name(), "custom");
    }

    #[test]
    fn cell_rects_maps_ids_after_index_shift() {
        // Stable ids mean a custom tree survives removing an earlier cell: the
        // remaining cells still resolve to rects by id, not by array position.
        let mut v = view_n(3, gridv(), 0);
        v.custom_tree = Some(v.auto_tree());
        let removed = v.cells.remove(0).id; // array indices now shift
        v.prune_from_tree(removed);
        v.clamp_focus();
        let rects = cell_rects(&v, area(120, 40));
        assert_eq!(rects.len(), 2);
        assert!(rects.iter().all(|r| r.is_some()), "both cells still placed");
    }

    // -- #3 Resize (custom_tree divider) -------------------------------------

    #[test]
    fn custom_tree_resize_moves_divider_and_flags_custom() {
        // 2-cell grid => vertical split, focused cell (id 1) is the first child.
        let mut v = view_n(2, gridv(), 0);
        let a = area(120, 40);
        let before = cell_rects(&v, a)[0].unwrap().width;
        // Seed a custom tree and apply the server's ResizeRight convention:
        // Vertical axis, +delta grows the focused (first) child.
        v.custom_tree = Some(v.auto_tree());
        let fid = v.focused_id();
        let changed = v
            .custom_tree
            .as_mut()
            .unwrap()
            .resize(fid, Direction::Vertical, 0.1);
        assert!(changed, "a grid split must be resizable");
        let after = cell_rects(&v, a)[0].unwrap().width;
        assert!(after > before, "divider moved right: {before} -> {after}");
        assert_eq!(v.layout_name(), "custom");
    }

    #[test]
    fn monocle_view_has_no_resizable_split() {
        // A Monocle view is a single stack: seeding a tree and resizing changes
        // nothing (so handle_view_command reverts the seed and stays automatic).
        let mut v = view_n(3, monoclev(), 1);
        v.custom_tree = Some(v.auto_tree());
        let fid = v.focused_id();
        let changed = v
            .custom_tree
            .as_mut()
            .unwrap()
            .resize(fid, Direction::Vertical, 0.1);
        assert!(!changed, "monocle stack has no split to resize");
    }

    // -- #4 Move (swap / relocate) -------------------------------------------

    #[test]
    fn custom_tree_relocate_flips_divider_down() {
        // 2 cells left/right; PaneMoveDown on the focused cell (no Down neighbor)
        // relocates it to the bottom edge, flipping the split vertical->horizontal.
        let mut v = view_n(2, gridv(), 0);
        v.custom_tree = Some(v.auto_tree());
        let fid = v.focused_id();
        let inner = cells_area(area(120, 40));
        assert!(
            find_neighbor(
                v.custom_tree.as_ref().unwrap(),
                inner,
                fid,
                FocusDirection::Down,
                0
            )
            .is_none(),
            "a left/right split has no Down neighbor"
        );
        let nt = relocate_pane_to_edge(v.custom_tree.as_ref().unwrap(), fid, FocusDirection::Down)
            .unwrap();
        v.custom_tree = Some(nt);
        let rects = cell_rects(&v, area(120, 40));
        let (r0, r1) = (rects[0].unwrap(), rects[1].unwrap());
        assert!(r0.y > r1.y, "focused cell (index 0) moved to the bottom");
        assert_eq!(r0.x, r1.x, "now a top/bottom split, same column");
    }

    #[test]
    fn custom_tree_move_swaps_with_neighbor() {
        // 2 cells left/right; PaneMoveRight on the left (focused) cell swaps it
        // with its right neighbor -> the cells trade rectangles.
        let mut v = view_n(2, gridv(), 0);
        v.custom_tree = Some(v.auto_tree());
        let fid = v.focused_id();
        let inner = cells_area(area(120, 40));
        let neighbor = find_neighbor(
            v.custom_tree.as_ref().unwrap(),
            inner,
            fid,
            FocusDirection::Right,
            0,
        )
        .expect("right neighbor exists");
        assert!(crate::server::layout::swap_panes(
            v.custom_tree.as_mut().unwrap(),
            fid,
            neighbor
        ));
        let rects = cell_rects(&v, area(120, 40));
        // Focused cell (index 0, id `fid`) is now on the RIGHT.
        assert!(rects[0].unwrap().x > rects[1].unwrap().x);
    }

    // -- #2 Zoom -------------------------------------------------------------

    #[test]
    fn zoom_shows_only_focused_full_cell_area() {
        let mut v = view_n(3, gridv(), 1);
        v.zoomed = true;
        let a = area(80, 24);
        let rects = cell_rects(&v, a);
        assert_eq!(rects[1], Some(cells_area(a)), "focused fills the cell area");
        assert!(rects[0].is_none() && rects[2].is_none(), "others hidden");
        // Only the focused snapshot is composited.
        let mut v2 = view_n(2, gridv(), 0);
        v2.cells[0].snapshot = Some(snap_filled(40, 10, 'A'));
        v2.cells[1].snapshot = Some(snap_filled(40, 10, 'B'));
        v2.zoomed = true;
        let buf = composite(&v2, a, &tt(), "NORMAL");
        let joined: String = buf.iter().flat_map(|r| r.iter().map(|c| c.c)).collect();
        assert!(joined.contains('A') && !joined.contains('B'));
    }

    #[test]
    fn zoom_suppresses_monocle_strip() {
        let mut v = view_n(3, monoclev(), 1);
        v.zoomed = true;
        assert!(monocle_strip_rect(&v, area(80, 24)).is_none());
    }

    // -- add_cell / prune tree maintenance -----------------------------------

    #[test]
    fn add_cell_splices_into_custom_tree() {
        let mut v = view_n(2, gridv(), 0);
        v.custom_tree = Some(v.auto_tree());
        let before = all_pane_ids(v.custom_tree.as_ref().unwrap()).len();
        let id = v.add_cell(ConnId::Local, 99);
        let ids = all_pane_ids(v.custom_tree.as_ref().unwrap());
        assert_eq!(ids.len(), before + 1);
        assert!(ids.contains(&id), "new cell id spliced into the tree");
        let rects = cell_rects(&v, area(120, 40));
        assert!(rects.iter().all(|r| r.is_some()), "all cells placed");
    }

    #[test]
    fn prune_from_tree_removes_then_clears() {
        let mut v = view_n(2, gridv(), 0);
        v.custom_tree = Some(v.auto_tree());
        let (id0, id1) = (v.cells[0].id, v.cells[1].id);
        v.prune_from_tree(id0);
        let ids = all_pane_ids(v.custom_tree.as_ref().unwrap());
        assert!(!ids.contains(&id0) && ids.contains(&id1));
        v.prune_from_tree(id1);
        assert!(v.custom_tree.is_none(), "emptied tree clears to automatic");
    }

    // -- #1 Monocle strip matches the regular stacked-pane strip -------------

    #[test]
    fn monocle_strip_uses_ascii_pipe_separator() {
        let mut cells: Vec<ViewCell> = (0..2).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("aa".into());
        cells[1].title = Some("bb".into());
        let view = view_of(cells, monoclev(), 0);
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL");
        let row0: String = buf[0].iter().map(|c| c.c).collect();
        // The shared strip function separates tabs with an ASCII " | " (space
        // pipe space), NOT a box-drawing vertical bar.
        assert!(row0.contains(" | "), "ascii pipe separator: {row0:?}");
        assert!(
            !row0.contains('\u{2502}'),
            "strip must not use box-drawing '│': {row0:?}"
        );
    }

    #[test]
    fn monocle_strip_active_tab_has_mode_color_bg() {
        // The strip is rendered by the SAME `build_top_border_content` a normal
        // stacked pane uses: the ACTIVE (focused) tab is a mode-colored block
        // (bold, `theme.mode_colors(mode)` bg/fg), NOT a border/underline.
        let theme = tt();
        let mut cells: Vec<ViewCell> = (0..2).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("aa".into());
        cells[1].title = Some("bb".into());
        let view = view_of(cells, monoclev(), 0); // cell 0 focused/active
        let buf = composite(&view, area(80, 24), &theme, "NORMAL");
        let row0 = &buf[0];
        let (mode_fg, mode_bg) = theme.mode_colors("NORMAL");
        let apos = row0.iter().position(|c| c.c == 'a').unwrap();
        assert_eq!(row0[apos].bg, mode_bg, "active tab bg = mode color block");
        assert_eq!(row0[apos].fg, mode_fg, "active tab fg = mode color");
        assert!(row0[apos].bold, "active tab is bold");
        assert!(
            !row0[apos].underline,
            "active tab is a block, not an underline"
        );
    }

    #[test]
    fn monocle_strip_inactive_tab_has_background_block() {
        // Mirrors the regular strip's inactive-tab grey background (Indexed 237)
        // and inactive-tab fg (`theme.tab_inactive_fg`).
        let theme = tt();
        let mut cells: Vec<ViewCell> = (0..2).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("aa".into());
        cells[1].title = Some("bb".into());
        let view = view_of(cells, monoclev(), 0); // cell 0 focused/active
        let buf = composite(&view, area(80, 24), &theme, "NORMAL");
        let row0 = &buf[0];
        let bpos = row0.iter().position(|c| c.c == 'b').unwrap();
        assert_eq!(row0[bpos].bg, CellColor::Indexed(237));
        assert_eq!(row0[bpos].fg, theme.tab_inactive_fg);
        assert!(!row0[bpos].bold, "inactive tab is not bold");
    }

    #[test]
    fn monocle_strip_render_matches_strip_segments() {
        // Hit-testing (`cell_at` via `strip_segments`) must agree with what
        // `build_top_border_content` actually paints. Assert each tab's rendered
        // start column (first cell whose bg is a tab block, i.e. NOT the strip's
        // default-bg leading space / separator) equals `strip_segments`' start.
        let theme = tt();
        let mut cells: Vec<ViewCell> = (0..3).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("alpha".into());
        cells[1].title = Some("bb".into());
        cells[2].title = Some("gamma".into());
        let view = view_of(cells, monoclev(), 1); // middle cell focused
        let a = area(80, 24);
        let buf = composite(&view, a, &theme, "NORMAL");
        let strip = monocle_strip_rect(&view, a).unwrap();
        let width = strip.width as usize;
        let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
        let segs = strip_segments(&titles, width);
        assert_eq!(segs.len(), 3, "three visible tabs");
        let (_, mode_bg) = theme.mode_colors("NORMAL");
        for (idx, start, end) in segs {
            // Every column of the tab carries a tab background block (mode color
            // for the focused tab, Indexed(237) otherwise) rather than the
            // default-bg spaces used for the leading space and " | " separators.
            let expect_bg = if idx == view.focused {
                mode_bg.clone()
            } else {
                CellColor::Indexed(237)
            };
            for (col, cell) in buf[0].iter().enumerate().take(end).skip(start) {
                assert_eq!(
                    cell.bg, expect_bg,
                    "tab {idx} col {col} bg mismatch (start={start}, end={end})"
                );
            }
            // The column just before `start` is NOT a tab block (leading space or
            // separator), proving `start` is the true left edge.
            if start > 0 {
                assert_ne!(
                    buf[0][start - 1].bg,
                    expect_bg,
                    "column before tab {idx} start must not be part of the block"
                );
            }
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
            assert_eq!(cell_rects(&view_n(n, gridv(), 0), area(80, 24)).len(), n);
            assert_eq!(cell_rects(&view_n(n, monoclev(), 0), area(80, 24)).len(), n);
        }
    }

    #[test]
    fn cell_rects_within_cells_area() {
        let a = area(80, 24);
        let inner = cells_area(a);
        for n in 1..=5 {
            for r in cell_rects(&view_n(n, gridv(), 0), a).into_iter().flatten() {
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
            for r in cell_rects(&view_n(n, gridv(), 0), a).into_iter().flatten() {
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
        let v = view_n(4, monoclev(), 2);
        let rects = cell_rects(&v, a);
        assert_eq!(rects.len(), 4);
        // Only the focused cell (index 2) is placed; it fills the cell area
        // BELOW the reserved title strip (top row), never on the strip row.
        let strip = monocle_strip_rect(&v, a).unwrap();
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
        let mv = view_n(1, monoclev(), 0);
        let gv = view_n(1, gridv(), 0);
        // Monocle: a 1-row strip at the top of the cell area.
        assert_eq!(monocle_strip_rect(&mv, area(80, 24)), Some(area(80, 1)));
        // Non-Monocle layouts have no strip.
        assert_eq!(monocle_strip_rect(&gv, area(80, 24)), None);
        // Height 3: cell area is 2 rows -> strip (1) + a cell row (1).
        assert_eq!(monocle_strip_rect(&mv, area(80, 3)), Some(area(80, 1)));
        // No room (cell area height < 2) -> no strip, so a cell can still show.
        assert_eq!(monocle_strip_rect(&mv, area(80, 2)), None);
        assert_eq!(monocle_strip_rect(&mv, area(80, 1)), None);
        assert_eq!(monocle_strip_rect(&mv, area(80, 0)), None);
        // Zoom and custom_tree both suppress the strip even in Monocle.
        let mut zoomed = view_n(2, monoclev(), 0);
        zoomed.zoomed = true;
        assert_eq!(monocle_strip_rect(&zoomed, area(80, 24)), None);
        let mut custom = view_n(2, monoclev(), 0);
        custom.custom_tree = Some(custom.auto_tree());
        assert_eq!(monocle_strip_rect(&custom, area(80, 24)), None);
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let theme = tt();
        let a = area(80, 24);
        let buf = composite(&view, a, &theme, "NORMAL");
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
        // Shared-function style: the focused entry (beta) is a mode-colored
        // bold block; an unfocused entry (alpha) is on the Indexed(237) inactive
        // background -> visually distinct.
        let (mode_fg, mode_bg) = theme.mode_colors("NORMAL");
        let bpos = row0.find("beta").unwrap();
        let apos = row0.find("alpha").unwrap();
        assert_eq!(buf[0][bpos].fg, mode_fg);
        assert_eq!(buf[0][bpos].bg, mode_bg);
        assert!(buf[0][bpos].bold);
        assert_eq!(buf[0][apos].fg, theme.tab_inactive_fg);
        assert_eq!(buf[0][apos].bg, CellColor::Indexed(237));
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL");
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL");
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let a = area(80, 24);
        // Locate beta's entry on the strip via the same segment layout used to
        // draw it, then hit-test its middle column.
        let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
        let strip = monocle_strip_rect(&view, a).unwrap();
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        for (w, h) in [(1u16, 1u16), (5, 1), (10, 2), (10, 3), (0, 0)] {
            let buf = composite(&view, area(w, h), &tt(), "NORMAL");
            assert_eq!(buf.len(), h as usize);
            assert!(buf.iter().all(|row| row.len() == w as usize));
        }
    }

    #[test]
    fn grid_two_cells_split_left_right() {
        // The n=2 Grid case must remain a left/right split (the PTY harnesses
        // rely on it): cell 0 on the left half, cell 1 on the right half.
        let a = area(120, 40);
        let rects = cell_rects(&view_n(2, gridv(), 0), a);
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL");
        let rects = cell_rects(&view, a);

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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL");
        let rects = cell_rects(&view, a);

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
            session_visible: false,
        };
        let view = ClientView {
            name: "v".into(),
            cells: vec![cell_with(1, Some(snap))],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL");

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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL");
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL");
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL");
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL");
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let a = area(80, 24);
        let rects = cell_rects(&view, a);
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        let a = area(80, 24);
        // Focused cell 0: interior origin (ix=1, iy=1); snapshot (10 rows) fits in
        // the interior so start=0 -> cursor row = iy + 9, col = ix + 3.
        let rects = cell_rects(&view, a);
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
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
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
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
        // Geometry-driven: 4 cells in a grid. Neighbor relations come from the
        // real cell rects, not index arithmetic. Assertions below are validated
        // against the actual `cell_rects` geometry (see the guard block).
        let a = area(80, 24);
        let cells = cells_area(a);
        let mk = || ClientView {
            name: "v".into(),
            cells: (1..=4).map(|id| cell_with(id, None)).collect(),
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        // Sanity: confirm the grid lays 4 cells as a 2x2 (two rows of two), so
        // the neighbor expectations below are the geometric truth.
        let rects: Vec<Rect> = cell_rects(&mk(), a).into_iter().flatten().collect();
        assert_eq!(rects.len(), 4);
        let xs: std::collections::BTreeSet<u16> = rects.iter().map(|r| r.x).collect();
        let ys: std::collections::BTreeSet<u16> = rects.iter().map(|r| r.y).collect();
        assert_eq!(xs.len(), 2, "two distinct columns: {rects:?}");
        assert_eq!(ys.len(), 2, "two distinct rows: {rects:?}");

        let mut view = mk();
        // From top-left: right -> top-right neighbor.
        assert!(view.move_focus(FocusDirection::Right, cells));
        let after_right = view.focused;
        assert_ne!(after_right, 0, "right moved off the top-left cell");
        // Down from there -> the cell below it.
        assert!(view.move_focus(FocusDirection::Down, cells));
        let after_down = view.focused;
        assert_ne!(after_down, after_right, "down moved to another row");
        // Left -> back to the left column (bottom-left).
        assert!(view.move_focus(FocusDirection::Left, cells));
        let after_left = view.focused;
        assert_ne!(after_left, after_down, "left moved off the right column");
        // Up -> back to the top-left where we started.
        assert!(view.move_focus(FocusDirection::Up, cells));
        assert_eq!(view.focused, 0, "up returns to the top-left cell");
        // At an edge: no movement, returns false.
        assert!(!view.move_focus(FocusDirection::Left, cells));
        assert_eq!(view.focused, 0);
        assert!(!view.move_focus(FocusDirection::Up, cells));
        assert_eq!(view.focused, 0);
    }

    #[test]
    fn move_focus_grid_geometry_after_restructure() {
        // The bug fix: two side-by-side cells A|B; move B down so A is on top and
        // B on the bottom; focus-up from B must land on A (index-based grid math
        // used to make this a no-op). Drive it through a `custom_tree` that
        // encodes the restructured A-top/B-bottom split.
        use crate::server::layout::{Direction, LayoutNode};
        let a = area(80, 24);
        let cells = cells_area(a);
        // Build the A(top)/B(bottom) tree over cell ids 1 (A) and 2 (B).
        let tree = LayoutNode::Split {
            direction: Direction::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::new_stack(1)),
            second: Box::new(LayoutNode::new_stack(2)),
        };
        let mut view = ClientView {
            name: "v".into(),
            cells: (1..=2).map(|id| cell_with(id, None)).collect(),
            layout: gridv(),
            focused: 1, // B (bottom) focused
            custom_tree: Some(tree),
            zoomed: false,
            next_cell_id: 1000,
        };
        // Focus UP from the bottom cell must reach the TOP cell (index 0 = A).
        assert!(
            view.move_focus(FocusDirection::Up, cells),
            "focus-up after a down-move must move"
        );
        assert_eq!(
            view.focused, 0,
            "focus-up from bottom lands on the top cell"
        );
        // And back down returns to B.
        assert!(view.move_focus(FocusDirection::Down, cells));
        assert_eq!(view.focused, 1);
    }

    #[test]
    fn move_focus_monocle_pages_through_cells() {
        let cells = cells_area(area(80, 24));
        let mut view = ClientView {
            name: "v".into(),
            cells: (1..=3).map(|id| cell_with(id, None)).collect(),
            layout: monoclev(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
        };
        assert!(view.move_focus(FocusDirection::Right, cells));
        assert_eq!(view.focused, 1);
        assert!(view.move_focus(FocusDirection::Right, cells));
        assert_eq!(view.focused, 2);
        assert!(!view.move_focus(FocusDirection::Right, cells)); // at the end
        assert!(view.move_focus(FocusDirection::Left, cells));
        assert_eq!(view.focused, 1);
    }

    #[test]
    fn move_focus_empty_is_noop() {
        let cells = cells_area(area(80, 24));
        let mut view = ClientView::new("v".into());
        assert!(!view.move_focus(FocusDirection::Right, cells));
        assert_eq!(view.focused, 0);
    }

    #[test]
    fn clamp_focus_after_removal() {
        let mut view = ClientView {
            name: "v".into(),
            cells: (1..=3).map(|id| cell_with(id, None)).collect(),
            layout: gridv(),
            focused: 2,
            custom_tree: None,
            zoomed: false,
            next_cell_id: 1000,
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
                custom_tree: None,
                zoomed: false,
                next_cell_id: 1000,
            };
            let a = area(80, 24);
            let buf = composite(&view, a, &tt(), "NORMAL");
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
            let buf = composite(&view, area(w, h), &tt(), "NORMAL");
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
