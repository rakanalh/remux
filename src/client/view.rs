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
//! cell rect, bottom-anchored so the latest output is visible. Every cell the
//! layout SHOWS demands its interior from its source pane, which reflows to fit
//! (via the server's min-across-viewers sizing), so a pane added to a view fits
//! the cell it is given. Cells the layout hides (Monocle's unfocused ones, the
//! non-zoomed ones under zoom) watch read-only and impose no size demand, so a
//! cell nobody sees never reflows the shared pane.

use crate::client::registry::ConnId;
use crate::config::theme::CompositorTheme;
use crate::config::BorderStyle;
use crate::protocol::{CellColor, ConnDescriptor, PaneId, RenderCell, ViewId, ViewInfo};
use crate::server::compositor::{
    build_top_border_content, draw_right_segments, draw_tmux_dividers, draw_tmux_tab_bar,
    draw_zellij_border, fits_zellij_border, status_right_segments, tab_strip_layout, HitRegions,
};
use crate::server::layout::{
    compute_layout, focus_in_direction, FocusDirection, LayoutMode, LayoutNode, Rect,
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
/// A cell has six observable states, distinguished without a separate enum:
/// - **waiting**: `snapshot == None && !exited && !disconnected && unavailable.is_none()` —
///   subscribed but no `PaneContent` has arrived yet (shows `waiting for <title>…`).
/// - **exited**: `exited == true` — the SERVER reported the source pane gone
///   ([`SessionEvent::PaneExited`](crate::protocol::SessionEvent::PaneExited));
///   shows `pane closed`, takes no more input, and is never re-subscribed.
///   Outranks every other state.
/// - **unavailable**: `unavailable == Some(reason)` — this terminal cannot
///   currently reach the cell's source server (a remote it has not connected, or
///   one whose dial failed), so no snapshot can ever arrive; shows `reason`
///   instead of an eternal `waiting…`.
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
    /// Stable, per-view identity for this cell. Phase 2: assigned by the SERVER
    /// (the [`CellId`](crate::protocol::CellId) of the corresponding
    /// [`CellInfo`](crate::protocol::CellInfo)) and mirrored here via
    /// [`ClientView::from_info`]. Used as the pseudo-[`PaneId`] in the layout
    /// tree so that adding or removing cells (which shifts array indices) never
    /// invalidates a persistent `custom_tree`. Everything index-based
    /// (`focused`, `move_focus`, `cell_at`) keeps using the array index; only the
    /// tree keys off `id`.
    pub id: u64,
    pub conn: ConnId,
    pub pane_id: PaneId,
    pub snapshot: Option<PaneSnapshot>,
    /// Set when the source PANE is gone, as reported by the server
    /// ([`SessionEvent::PaneExited`](crate::protocol::SessionEvent::PaneExited)):
    /// its shell exited, or it was closed from somewhere else. Terminal — a
    /// `PaneId` is never reused, so the cell can only ever show `pane closed`
    /// from here on, and it is skipped by the subscribe pass. Distinct from
    /// `disconnected`, which is about the TRANSPORT: a perfectly healthy
    /// connection to a server whose pane died never trips that flag, which is
    /// why such a cell used to sit on `waiting…` forever.
    pub exited: bool,
    /// Set when the cell's source connection is gone (a send failed or the
    /// connection closed). A disconnected cell renders a `disconnected` label
    /// and silently drops keystrokes instead of crashing the client.
    pub disconnected: bool,
    /// Why this terminal cannot reach the cell's source server right now, as a
    /// short label to render in place of the content (`connecting to <name>…`,
    /// `not connected: <name>`). Set by the subscribe pass when the cell's
    /// connection has no open transport — a shared view can name a remote this
    /// particular terminal never connected — and cleared as soon as a
    /// subscription goes out or a `PaneContent` arrives. Without it such a cell
    /// would sit on `waiting…` forever, since no snapshot can ever come.
    pub unavailable: Option<String>,
    /// `session / tab` title for the cell's source pane, learned from
    /// `PaneContent`. `None` until the first snapshot; kept live so a rename on
    /// the source updates the border label. Remote cells are host-prefixed by
    /// the compositor, not here.
    pub title: Option<String>,
}

impl ViewCell {
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
///
/// Phase 2: this is no longer the authoritative model. It is a per-terminal
/// **render/view-model** rebuilt from the server's [`ViewInfo`] snapshot on every
/// `ViewList` broadcast (see [`ClientView::from_info`]). Membership, `layout`,
/// `custom_tree`, `focused` and `zoomed` all come FROM the server so every
/// terminal mirrors one shared arrangement; only per-terminal render state (each
/// cell's last [`PaneSnapshot`], title, disconnected flag) and pixel geometry are
/// local. The geometry helpers ([`cell_rects`], the Monocle strip, hit-testing,
/// [`ClientView::move_focus`]) stay here because geometry is per-terminal.
#[derive(Debug, Clone)]
pub struct ClientView {
    /// The server-assigned [`ViewId`] this cache entry mirrors. The client keys
    /// its view cache by this id, routes intents (`ViewSetFocus { id, .. }` …)
    /// with it, and re-resolves which view it is displaying across `ViewList`
    /// rebuilds by it.
    pub id: ViewId,
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
}

/// Map a wire [`ConnDescriptor`] (as carried in a `ViewInfo` cell) to the
/// client's [`ConnId`]. This is the sync-side half of the descriptor mapping;
/// the reverse (`ConnId → ConnDescriptor`, used when a view-management intent is
/// sent) lives at the intent call sites in `main.rs`.
pub fn conn_from_descriptor(d: &ConnDescriptor) -> ConnId {
    match d {
        ConnDescriptor::Local => ConnId::Local,
        ConnDescriptor::Remote(name) => ConnId::Remote(name.clone()),
    }
}

impl ClientView {
    /// Create an empty view with the given name (Grid layout, no cells).
    /// Test-only: in production a view is only ever built from a server
    /// [`ViewInfo`] via [`ClientView::from_info`].
    #[cfg(test)]
    pub fn new(name: String) -> Self {
        Self {
            id: 0,
            name,
            cells: Vec::new(),
            layout: LayoutMode::Grid(crate::server::layout::GridLayout),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        }
    }

    /// Rebuild this per-terminal view-model from a server [`ViewInfo`] snapshot.
    ///
    /// Membership / `layout` / `custom_tree` / `focused` / `zoomed` are taken
    /// verbatim from the server (the shared arrangement). Per-terminal render
    /// state — each cell's last [`PaneSnapshot`], learned title, and
    /// exited / disconnected / unavailable flags — is carried over from `prev` (this view's previous cache entry, if
    /// any) so a resync never drops already-streamed content nor flashes
    /// `waiting…`. Render state is matched first by the stable [`ViewCell::id`],
    /// then by `(conn, pane_id)` so a freshly-added cell aliasing an
    /// already-streaming pane shows its content immediately.
    pub fn from_info(info: &ViewInfo, prev: Option<&ClientView>) -> ClientView {
        let cells: Vec<ViewCell> = info
            .cells
            .iter()
            .map(|ci| {
                let conn = conn_from_descriptor(&ci.conn);
                let carried = prev.and_then(|p| {
                    p.cells.iter().find(|c| c.id == ci.id).or_else(|| {
                        p.cells
                            .iter()
                            .find(|c| c.conn == conn && c.pane_id == ci.pane_id)
                    })
                });
                ViewCell {
                    id: ci.id,
                    conn,
                    pane_id: ci.pane_id,
                    snapshot: carried.and_then(|c| c.snapshot.clone()),
                    exited: carried.map(|c| c.exited).unwrap_or(false),
                    disconnected: carried.map(|c| c.disconnected).unwrap_or(false),
                    unavailable: carried.and_then(|c| c.unavailable.clone()),
                    title: carried.and_then(|c| c.title.clone()),
                }
            })
            .collect();
        // Clamp the server's focus index into range (defensive; the server keeps
        // it valid, but an empty view reports 0).
        let focused = if cells.is_empty() {
            0
        } else {
            info.focused.min(cells.len() - 1)
        };
        ClientView {
            id: info.id,
            name: info.name.clone(),
            cells,
            layout: info.layout.clone(),
            focused,
            custom_tree: info.custom_tree.clone(),
            zoomed: info.zoomed,
        }
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
    ///
    /// Phase 2: tree maintenance is owned by the server
    /// ([`ServerState::view_remove_cell`](crate::server::session::ServerState));
    /// the client only mirrors the resulting `custom_tree`. Kept as a test-only
    /// helper for the geometry tests below.
    #[cfg(test)]
    pub fn prune_from_tree(&mut self, id: u64) {
        if let Some(tree) = self.custom_tree.as_mut() {
            if tree.close_pane(id).is_none() {
                self.custom_tree = None;
            }
        }
    }

    /// Clamp `focused` into range after the cell list changed.
    ///
    /// Phase 2: focus is server-owned and clamped in [`ClientView::from_info`];
    /// kept as a test-only helper.
    #[cfg(test)]
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

/// The border color for a view cell, resolved from the SAME theme roles a
/// normal tab's panes use: `frame_active_fg` for the focused cell,
/// `frame_fg` for every other one. Threading the theme through is what makes a
/// view's borders indistinguishable from a normal tab's (the previous hardcoded
/// `Indexed(10)`/`Indexed(8)` pair did not match any theme).
fn cell_border_fg(theme: &CompositorTheme, focused: bool) -> CellColor {
    if focused {
        theme.frame_active_fg.clone()
    } else {
        theme.frame_fg.clone()
    }
}

/// The interior (content) region of a cell whose outer rect is `rw` x `rh` at
/// buffer-local `(rx, ry)`, for the given border style. Returns
/// `(ix, iy, iw, ih)`.
///
/// Mirrors the server exactly: `ZellijStyle` insets by one on every side when
/// the rect satisfies [`fits_zellij_border`] (below that the content fills the
/// rect, as `draw_zellij_panes` does); `TmuxStyle` is always edge-to-edge.
/// [`draw_cell`], [`focused_cursor`] and [`cell_content_size`] all go through
/// this, so cursor placement and subscription sizing can never disagree with
/// what was painted.
fn cell_interior(
    rx: usize,
    ry: usize,
    rw: usize,
    rh: usize,
    style: &BorderStyle,
) -> (usize, usize, usize, usize) {
    // `rw`/`rh` always originate from a `Rect`'s u16 fields, so the narrowing
    // back to u16 for the shared threshold check is lossless.
    let bordered =
        matches!(style, BorderStyle::ZellijStyle) && fits_zellij_border(rw as u16, rh as u16);
    if bordered {
        (rx + 1, ry + 1, rw - 2, rh - 2)
    } else {
        (rx, ry, rw, rh)
    }
}

/// The content size (cols, rows) a cell of outer size `rect` can show in the
/// given border style — the size its `SubscribePane` must request so the source
/// pane reflows to exactly the region that gets painted. Zellij style loses one
/// row/column to each border edge; tmux style loses nothing.
pub fn cell_content_size(rect: Rect, style: &BorderStyle) -> (u16, u16) {
    let (_, _, iw, ih) = cell_interior(0, 0, rect.width as usize, rect.height as usize, style);
    (iw as u16, ih as u16)
}

/// Composite a view into an `area.height` x `area.width` buffer of
/// [`RenderCell`]s, ready to hand to
/// [`Renderer::render_full`](crate::client::renderer::Renderer::render_full).
///
/// Cells are placed within [`cells_area`] (the terminal minus the reserved
/// status row); the status row itself is drawn separately by
/// [`draw_status_bar`]. Each cell is framed exactly as a normal tab's pane is in
/// `style` — a rounded, theme-colored box border in `ZellijStyle` (drawn by the
/// server's own [`draw_zellij_border`]), or edge-to-edge content with
/// [`draw_tmux_dividers`] between adjacent cells in `TmuxStyle` — and its
/// snapshot is blitted bottom-anchored into the interior, clipped to the
/// interior's width/height. Cells with no snapshot yet show a centered
/// placeholder label. In `Monocle` only the focused cell is drawn.
///
/// `area` is expected to have its origin at the buffer origin in normal use
/// (the full terminal, `x = y = 0`); rect coordinates are translated back to
/// buffer-local space so a non-zero origin still composites correctly.
pub fn composite(
    view: &ClientView,
    area: Rect,
    theme: &CompositorTheme,
    mode: &str,
    style: &BorderStyle,
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
            draw_cell(
                &mut buf,
                area,
                *rect,
                cell,
                i == view.focused,
                theme,
                mode,
                style,
            );
        }
    }
    // Tmux style draws no per-cell border, so adjacent cells are separated by the
    // same 1-column/1-row dividers the server puts between tmux panes. Fed the
    // buffer-local rects (the shared helper writes in buffer coordinates).
    if matches!(style, BorderStyle::TmuxStyle) {
        let local: Vec<(PaneId, Rect)> = view
            .cells
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                rects.get(i).copied().flatten().map(|r| {
                    (
                        c.id,
                        Rect {
                            x: r.x.saturating_sub(area.x),
                            y: r.y.saturating_sub(area.y),
                            ..r
                        },
                    )
                })
            })
            .collect();
        if local.len() > 1 {
            draw_tmux_dividers(&mut buf, &local, theme);
        }
    }
    // Monocle shows only the focused cell, so draw a top strip listing EVERY
    // cell's title (like a regular Monocle tab's stacked-pane strip) to reveal
    // the panes the user can page to. Drawn LAST so it always wins the reserved
    // row even if the cell geometry above ever regressed.
    if let Some(strip) = monocle_strip_rect(view, area) {
        draw_monocle_strip(&mut buf, area, strip, view, theme, mode, style);
    }
    buf
}

/// Index of the cell whose rect contains the point `(x, y)`, for mouse-click
/// focus. `None` when the view is empty or the click lands outside every cell
/// (including a click on the reserved status row). In `Monocle` only the
/// focused cell has a rect below the strip, but a click landing on a title in
/// the top strip resolves to THAT cell, so clicking a strip entry pages to it;
/// any other in-bounds click resolves to the focused cell.
pub fn cell_at(
    view: &ClientView,
    area: Rect,
    x: u16,
    y: u16,
    style: &BorderStyle,
) -> Option<usize> {
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
            if let Some(entry) = tab_strip_layout(&titles, strip.width as usize, style)
                .into_iter()
                .find(|e| rel >= e.start && rel < e.end)
            {
                return Some(entry.index);
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

/// The region of the screen a cell's content actually occupies, and where the
/// source pane's grid starts within it.
///
/// Every mouse/visual mapping into a cell goes through this so it can never
/// disagree with what [`draw_cell`] painted. `origin` is the top-left of the
/// painted content; `cols`/`rows` is its extent; `src_row0` is the source
/// pane grid row shown on the first painted row.
///
/// `src_row0` is where the two anchoring regimes differ, and getting it
/// backwards shifts every selection by the difference: `draw_cell` shows
/// snapshot rows `start..` with `start = sr.saturating_sub(ih)`, so a snapshot
/// TALLER than the interior is bottom-anchored (`src_row0 = sr - ih`, the top of
/// the pane is cut off) while a SHORTER one is top-aligned at row 0 with blank
/// rows below (`src_row0 = 0`), not floated to the bottom.
pub struct CellContentGeometry {
    /// Screen `(x, y)` of the first painted content cell.
    pub origin: (u16, u16),
    /// Painted content width in columns.
    pub cols: u16,
    /// Painted content height in rows.
    pub rows: u16,
    /// Source-pane grid row displayed on the first painted row.
    pub src_row0: u16,
}

/// Content geometry of cell `idx`, or `None` when it paints no content (no
/// rect, a degenerate interior, or an empty snapshot).
///
/// Coordinates are in `area`'s own space, matching [`cell_at`] — in normal use
/// `area` is the full terminal at the origin, so they are screen coordinates.
/// A cell with no snapshot yet, a disconnected one, or one showing the "Active
/// in session" placeholder has no source grid to map into and yields `None`.
pub fn cell_content_geometry(
    view: &ClientView,
    area: Rect,
    idx: usize,
    style: &BorderStyle,
) -> Option<CellContentGeometry> {
    let cell = view.cells.get(idx)?;
    if cell.exited || cell.disconnected || cell.unavailable.is_some() {
        return None;
    }
    let snap = cell.snapshot.as_ref()?;
    if snap.session_visible {
        return None;
    }
    let rect = cell_rects(view, area).get(idx).copied().flatten()?;
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let (ix, iy, iw, ih) = cell_interior(
        rect.x as usize,
        rect.y as usize,
        rect.width as usize,
        rect.height as usize,
        style,
    );
    if iw == 0 || ih == 0 {
        return None;
    }
    let sr = snap.cells.len();
    if sr == 0 {
        return None;
    }
    // Exactly `draw_cell`'s `start`: bottom-anchored when the snapshot overflows
    // the interior, top-aligned (with blank rows below) when it underfills it.
    let src_row0 = sr.saturating_sub(ih);
    Some(CellContentGeometry {
        origin: (ix as u16, iy as u16),
        cols: iw.min(snap.cols as usize) as u16,
        rows: ih.min(sr) as u16,
        src_row0: src_row0 as u16,
    })
}

/// Map a screen point into cell `idx`'s source-pane content coordinates,
/// clamping to the painted content.
///
/// Clamping (rather than returning `None` off-cell) is what makes drag-select
/// work: the pointer routinely leaves the cell mid-drag, and a drag that runs
/// past the top/bottom content row is exactly the edge the server turns into an
/// auto-scroll step. The gesture stays bound to the cell it started in.
pub fn cell_content_pos(
    view: &ClientView,
    area: Rect,
    idx: usize,
    x: u16,
    y: u16,
    style: &BorderStyle,
) -> Option<(u16, u16)> {
    let g = cell_content_geometry(view, area, idx, style)?;
    let col = x.saturating_sub(g.origin.0).min(g.cols.saturating_sub(1));
    let row_in_view = y.saturating_sub(g.origin.1).min(g.rows.saturating_sub(1));
    Some((col, g.src_row0 + row_in_view))
}

/// Hit-test a screen point to the cell whose *content* contains it, returning
/// `(cell index, content col, content row)`.
///
/// Unlike [`cell_at`] this rejects a point on a cell's border or on the Monocle
/// title strip: those are chrome, not text, so a press there must not start a
/// selection.
pub fn cell_content_at(
    view: &ClientView,
    area: Rect,
    x: u16,
    y: u16,
    style: &BorderStyle,
) -> Option<(usize, u16, u16)> {
    let idx = cell_rects(view, area).iter().position(|r| match r {
        Some(rect) => {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        }
        None => false,
    })?;
    let g = cell_content_geometry(view, area, idx, style)?;
    if x < g.origin.0 || x >= g.origin.0 + g.cols || y < g.origin.1 || y >= g.origin.1 + g.rows {
        return None;
    }
    let (col, row) = cell_content_pos(view, area, idx, x, y, style)?;
    Some((idx, col, row))
}

/// Buffer position `(x, y)` of the FOCUSED cell's terminal cursor, if it
/// should be shown. Only the focused cell shows a cursor, and only when its
/// snapshot's cursor is visible and falls within the (clipped, bottom-anchored)
/// interior. Returns `None` (cursor hidden) otherwise -- unfocused cells,
/// disconnected cells, no snapshot, a hidden source cursor, or a cursor
/// scrolled/clipped out of view. Mirrors [`draw_cell`]'s geometry exactly so
/// the cursor lands on the character it addresses.
pub fn focused_cursor(view: &ClientView, area: Rect, style: &BorderStyle) -> Option<(u16, u16)> {
    let n = view.cells.len();
    if n == 0 {
        return None;
    }
    let cell = view.cells.get(view.focused)?;
    if cell.exited || cell.disconnected {
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
    let (ix, iy, iw, ih) = cell_interior(rx, ry, rw, rh, style);
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

/// The screen region Visual mode must be scoped to while a view is displayed,
/// as `(origin_x, origin_y, cols, rows, cursor_col, cursor_row)` with the cursor
/// relative to the origin.
///
/// Visual mode is a *copy mode over what is painted*: the client extracts the
/// yank from its own front buffer within `pane_offset`/`visible_*`. In a normal
/// tab those come from the server's `focused_pane_rect`, but a client showing a
/// view is detached, so that rect is a stale leftover describing a pane of some
/// other session's layout -- scoping to it put the cursor in the wrong cell at a
/// meaningless offset. This is the view's own answer, derived from the same
/// [`cell_rects`]/[`cell_interior`] geometry that painted the cells.
///
/// The rect is the FOCUSED cell's painted content. When that cell has nothing to
/// select (no snapshot yet, disconnected, or showing the "Active in session"
/// placeholder) the whole interior is used with the cursor at its top-left: the
/// selection is empty, but the cursor is still in the cell the user is looking
/// at, which is the property that was broken. `None` only when the view has no
/// focused cell rect at all (empty view, degenerate geometry) -- the caller then
/// leaves Visual mode scoped as before.
pub fn focused_cell_visual_scope(
    view: &ClientView,
    area: Rect,
    style: &BorderStyle,
) -> Option<(u16, u16, u16, u16, u16, u16)> {
    let idx = view.focused;
    let rect = cell_rects(view, area).get(idx).copied().flatten()?;
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let (ix, iy, iw, ih) = cell_interior(
        rect.x as usize,
        rect.y as usize,
        rect.width as usize,
        rect.height as usize,
        style,
    );
    if iw == 0 || ih == 0 {
        return None;
    }
    match cell_content_geometry(view, area, idx, style) {
        Some(g) => {
            // Place the cursor where the cell's own cursor is drawn when it is
            // visible; otherwise the last content row, mirroring how Visual mode
            // opens at the bottom of a normal pane.
            let (cc, cr) = match focused_cursor(view, area, style) {
                Some((cx, cy)) => (
                    cx.saturating_sub(g.origin.0).min(g.cols.saturating_sub(1)),
                    cy.saturating_sub(g.origin.1).min(g.rows.saturating_sub(1)),
                ),
                None => (0, g.rows.saturating_sub(1)),
            };
            Some((g.origin.0, g.origin.1, g.cols, g.rows, cc, cr))
        }
        None => Some((ix as u16, iy as u16, iw as u16, ih as u16, 0, 0)),
    }
}

/// Write a single cell into the buffer if the coordinates are in range.
fn put(buf: &mut [Vec<RenderCell>], y: usize, x: usize, cell: RenderCell) {
    if let Some(row) = buf.get_mut(y) {
        if let Some(slot) = row.get_mut(x) {
            *slot = cell;
        }
    }
}

/// Draw one cell (border + snapshot) into `buf`. `rect` is in `area`-absolute
/// coordinates; it is translated to buffer-local space using `area`'s origin.
///
/// The frame is drawn by the SERVER's own border code so a view cell is
/// indistinguishable from a normal tab's pane:
/// - `ZellijStyle`: [`draw_zellij_border`] paints the rounded corners, the
///   `frame_active_fg`/`frame_fg` edges and the ` title ` top-border label — the
///   cell's title is handed over as a single-entry "stack", which is exactly the
///   shape a named single pane presents, so `build_top_border_content` renders it
///   identically.
/// - `TmuxStyle`: no border at all (as the server gives tmux panes none); the
///   content runs edge-to-edge and [`composite`] draws the dividers between
///   adjacent cells afterwards. The focus cue is the cursor, exactly as in a
///   normal tmux-style tab.
#[allow(clippy::too_many_arguments)]
fn draw_cell(
    buf: &mut [Vec<RenderCell>],
    area: Rect,
    rect: Rect,
    cell: &ViewCell,
    focused: bool,
    theme: &CompositorTheme,
    mode: &str,
    style: &BorderStyle,
) {
    let ox = area.x as usize;
    let oy = area.y as usize;
    let rx = (rect.x as usize).saturating_sub(ox);
    let ry = (rect.y as usize).saturating_sub(oy);
    let rw = rect.width as usize;
    let rh = rect.height as usize;
    if rw == 0 || rh == 0 {
        return;
    }

    let (ix, iy, iw, ih) = cell_interior(rx, ry, rw, rh, style);

    // A border was inset iff the interior is smaller than the rect.
    if iw < rw && ih < rh {
        // The title travels as a one-entry stack: `build_top_border_content`'s
        // single-pane branch then renders ` title `, byte-for-byte what a named
        // pane's top border shows. `cell_title` is never empty, so the label is
        // always present.
        let stack_info = Some((vec![cell_title(cell)], vec![cell.id], 0));
        draw_zellij_border(
            buf,
            Rect {
                x: rx as u16,
                y: ry as u16,
                width: rw as u16,
                height: rh as u16,
            },
            &cell_border_fg(theme, focused),
            &stack_info,
            cell.id,
            mode,
            theme,
        );
    }

    if iw == 0 || ih == 0 {
        return;
    }

    // The source PANE is gone: the server said so, which outranks every other
    // state -- `disconnected`/`unavailable` are guesses about reachability, this
    // is a reported fact. Any snapshot still held is stale by definition, so it
    // is never painted (the client also drops it on the event).
    if cell.exited {
        draw_centered(buf, ix, iy, iw, ih, "pane closed");
        return;
    }

    // A disconnected cell shows a centered `disconnected` label instead of a
    // (now stale) snapshot -- its source is gone.
    if cell.disconnected {
        draw_centered(buf, ix, iy, iw, ih, "disconnected");
        return;
    }

    // A cell this terminal cannot reach names the reason (`connecting to x…`,
    // `not connected: x`). No snapshot can arrive while that holds, so saying so
    // is strictly better than an eternal `waiting…`; any content still shown
    // would be stale, since even the subscription never went out.
    if let Some(reason) = &cell.unavailable {
        draw_centered(buf, ix, iy, iw, ih, reason);
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
/// remotes) once known, else the cell's identity — `<host>: pane <id>` for a
/// remote source, `pane <id>` for a local one.
///
/// The fallback must read sensibly BOTH standalone (the top-border label) and
/// nested inside a sentence (`waiting for {}…`, `● Active in {}`), which is why
/// it is not itself a status word: a "waiting…" fallback composed into the
/// no-snapshot placeholder produced the nonsense `waiting for waiting……`.
/// Never empty — the Monocle strip's tab widths derive from these titles.
fn cell_title(cell: &ViewCell) -> String {
    cell.title.clone().unwrap_or_else(|| match &cell.conn {
        ConnId::Remote(host) => format!("{host}: pane {}", cell.pane_id),
        ConnId::Local => format!("pane {}", cell.pane_id),
    })
}

// (The Monocle strip's tab geometry lives in
// `compositor::tab_strip_layout` — the SAME function that places the tabs
// `draw_monocle_strip` paints, so a click can never land off the tab drawn
// there. This module used to carry its own copy of the formula, which did not
// model the single-title case and so was off by one for a 1-cell view.)

/// Draw the Monocle title strip on `strip` (the reserved top row of the cell
/// area): a tab-like list of EVERY cell's title, rendered by the SAME server
/// function a normal stacked pane uses in the current `style`, so the strip is
/// pixel-identical to a normal Monocle tab's —
/// [`build_top_border_content`](crate::server::compositor::build_top_border_content)
/// for `ZellijStyle` (top-border tabs: fixed tab width, the active tab filled
/// with `theme.mode_colors(mode)`, inactive tabs on `tab_inactive_bg`,
/// `" | "` separators) and
/// [`draw_tmux_tab_bar`](crate::server::compositor::draw_tmux_tab_bar) for
/// `TmuxStyle` (status-bar-colored bar, `separator_fg` separators).
///
/// The cells' [`ViewCell::id`]s serve as the pseudo-pane ids and the focused
/// cell index as the active index. In zellij style the strip is drawn in the
/// focused-frame color (`frame_active_fg`): the whole strip belongs to the
/// focused view, exactly as a focused pane's border does.
fn draw_monocle_strip(
    buf: &mut [Vec<RenderCell>],
    area: Rect,
    strip: Rect,
    view: &ClientView,
    theme: &CompositorTheme,
    mode: &str,
    style: &BorderStyle,
) {
    let by = (strip.y as usize).saturating_sub(area.y as usize);
    let bx = (strip.x as usize).saturating_sub(area.x as usize);
    let width = strip.width as usize;
    if width == 0 {
        return;
    }
    // Same titles vector `cell_at`/`tab_strip_layout` hit-test against, so the
    // rendered tab boundaries and the click boundaries derive from identical
    // inputs. `cell_title` is never empty, so the pseudo-id fallback inside
    // `build_top_border_content` never fires and both `max_name_len`
    // computations provably agree.
    let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
    let pseudo_ids: Vec<PaneId> = view.cells.iter().map(|c| c.id).collect();
    let stack_info = Some((titles, pseudo_ids, view.focused));
    if matches!(style, BorderStyle::TmuxStyle) {
        // The tmux tab bar writes straight into the buffer at `strip`'s (already
        // buffer-local) position and needs no hit regions here: `cell_at`
        // hit-tests the strip itself via `tab_strip_layout`.
        let mut regions = HitRegions::default();
        draw_tmux_tab_bar(
            buf,
            Rect {
                x: bx as u16,
                y: by as u16,
                width: strip.width,
                height: 1,
            },
            &stack_info,
            mode,
            &mut regions,
            theme,
        );
        return;
    }
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

    // Right side: built and painted by the SERVER's own status-bar helpers, so
    // the layout indicator is styled exactly as a normal tab's -- including the
    // "drop it rather than overlap the left content" rule. It used to be drawn
    // here in `session_name_fg` + bold, which visibly changed the indicator the
    // moment you entered a view.
    let segments = status_right_segments(None, layout_name, theme);
    draw_right_segments(row, cols, x, &segments);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::compositor::TabStripEntry;
    use crate::server::layout::{
        all_pane_ids, find_neighbor, relocate_pane_to_edge, Direction, GridLayout, MonocleLayout,
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

    /// The default border style (rounded zellij boxes), used by every test that
    /// isn't specifically about the tmux-style rendering.
    fn zj() -> BorderStyle {
        BorderStyle::ZellijStyle
    }

    /// The alternative (tmux) border style: no per-cell box, minimal dividers.
    fn tmx() -> BorderStyle {
        BorderStyle::TmuxStyle
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
            exited: false,
            conn: ConnId::Local,
            pane_id,
            snapshot,
            disconnected: false,
            unavailable: None,
            title: None,
        }
    }

    /// Build a `ClientView` directly from a list of cells (test-only). Cells keep
    /// their own `id`s (see `cell_with`), so `custom_tree` stays `None` and rects
    /// come from the automatic layout.
    fn view_of(cells: Vec<ViewCell>, layout: LayoutMode, focused: usize) -> ClientView {
        ClientView {
            id: 0,
            name: "v".into(),
            cells,
            layout,
            focused,
            custom_tree: None,
            zoomed: false,
        }
    }

    /// A view of `n` fresh cells (pane ids `1..=n`) in `layout`, focused on
    /// `focused`. Used by the geometry tests that previously called `cell_rects`
    /// with a bare `(layout, focused, n)`.
    fn view_n(n: usize, layout: LayoutMode, focused: usize) -> ClientView {
        let cells: Vec<ViewCell> = (1..=n as PaneId).map(|id| cell_with(id, None)).collect();
        view_of(cells, layout, focused)
    }

    // -- Mouse/visual mapping into a cell's content --------------------------

    /// A snapshot whose row `r` is the digit `r % 10`, so a mapped row is
    /// identifiable from the character it addresses.
    fn snap_numbered(cols: u16, rows: u16) -> PaneSnapshot {
        let cells = (0..rows as usize)
            .map(|r| {
                vec![
                    RenderCell {
                        c: char::from_digit((r % 10) as u32, 10).unwrap(),
                        ..RenderCell::default()
                    };
                    cols as usize
                ]
            })
            .collect();
        PaneSnapshot {
            cols,
            rows,
            cells,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: false,
            application_cursor_keys: false,
            session_visible: false,
        }
    }

    /// A snapshot TALLER than the interior is bottom-anchored, so the first
    /// painted row is `sr - ih` -- the top of the pane is cut off. Getting this
    /// backwards shifts every selection by the overflow.
    #[test]
    fn cell_content_geometry_bottom_anchors_a_tall_snapshot() {
        let a = area(80, 25);
        let v = view_of(vec![cell_with(1, Some(snap_numbered(40, 100)))], gridv(), 0);
        let g = cell_content_geometry(&v, a, 0, &zj()).expect("geometry");
        let (_, _, _, ih) = cell_interior(0, 0, 80, 24, &zj());
        assert_eq!(g.src_row0 as usize, 100 - ih);
        assert_eq!(g.rows as usize, ih);
        // The top painted row maps to the first row actually shown.
        let (_, row) = cell_content_pos(&v, a, 0, g.origin.0, g.origin.1, &zj()).unwrap();
        assert_eq!(row, g.src_row0);
    }

    /// A snapshot SHORTER than the interior is TOP-aligned with blank rows
    /// below (`draw_cell`'s loop breaks past the snapshot), not floated to the
    /// bottom -- so the first painted row is source row 0.
    #[test]
    fn cell_content_geometry_top_aligns_a_short_snapshot() {
        let a = area(80, 25);
        let v = view_of(vec![cell_with(1, Some(snap_numbered(40, 5)))], gridv(), 0);
        let g = cell_content_geometry(&v, a, 0, &zj()).expect("geometry");
        assert_eq!(g.src_row0, 0);
        assert_eq!(g.rows, 5, "only the rows that exist are content");
        let (_, row) = cell_content_pos(&v, a, 0, g.origin.0, g.origin.1, &zj()).unwrap();
        assert_eq!(row, 0);
        // A point below the content clamps to the last existing row, never past.
        let (_, row) = cell_content_pos(&v, a, 0, g.origin.0, g.origin.1 + 50, &zj()).unwrap();
        assert_eq!(row, 4);
    }

    /// A drag routinely leaves the cell; the point must clamp INTO the anchor
    /// cell rather than escape it, which is what keeps a gesture bound to the
    /// cell it started in (and what turns a run past the edge into a scroll).
    #[test]
    fn cell_content_pos_clamps_a_point_outside_the_cell() {
        let a = area(80, 25);
        let v = view_of(vec![cell_with(1, Some(snap_numbered(40, 10)))], gridv(), 0);
        let g = cell_content_geometry(&v, a, 0, &zj()).unwrap();
        let (col, row) = cell_content_pos(&v, a, 0, 0, 0, &zj()).unwrap();
        assert_eq!(
            (col, row),
            (0, g.src_row0),
            "above/left clamps to the origin"
        );
        let (col, row) = cell_content_pos(&v, a, 0, 500, 500, &zj()).unwrap();
        assert_eq!(col, g.cols - 1);
        assert_eq!(row, g.src_row0 + g.rows - 1);
    }

    /// Borders are chrome: a press on one must not start a selection.
    #[test]
    fn cell_content_at_rejects_the_border_but_accepts_the_interior() {
        let a = area(80, 25);
        let v = view_of(vec![cell_with(1, Some(snap_numbered(40, 10)))], gridv(), 0);
        let rect = cell_rects(&v, a)[0].unwrap();
        assert_eq!(cell_content_at(&v, a, rect.x, rect.y, &zj()), None);
        let g = cell_content_geometry(&v, a, 0, &zj()).unwrap();
        assert_eq!(
            cell_content_at(&v, a, g.origin.0, g.origin.1, &zj()),
            Some((0, 0, g.src_row0))
        );
    }

    /// A cell with nothing to select (no snapshot, or the "Active in session"
    /// placeholder) yields no content mapping at all.
    #[test]
    fn cell_content_geometry_none_without_selectable_content() {
        let a = area(80, 25);
        let v = view_of(vec![cell_with(1, None)], gridv(), 0);
        assert!(cell_content_geometry(&v, a, 0, &zj()).is_none());
        let mut snap = snap_numbered(40, 10);
        snap.session_visible = true;
        let v = view_of(vec![cell_with(1, Some(snap))], gridv(), 0);
        assert!(cell_content_geometry(&v, a, 0, &zj()).is_none());
    }

    /// Bug B: Visual mode must scope to the FOCUSED cell. With four cells in a
    /// Grid and the focus on a RIGHT-hand one, the scope rect has to sit in that
    /// cell -- the symptom was a rect belonging to the foreground session, which
    /// put the cursor in the left-hand cell.
    #[test]
    fn focused_cell_visual_scope_follows_the_focused_cell() {
        let a = area(120, 40);
        let cells: Vec<ViewCell> = (1..=4)
            .map(|id| cell_with(id, Some(snap_numbered(60, 20))))
            .collect();
        let rects: Vec<Rect> = cell_rects(&view_of(cells.clone(), gridv(), 0), a)
            .into_iter()
            .flatten()
            .collect();
        // The cell furthest to the right is the user's "focused on the right one".
        let right = rects
            .iter()
            .enumerate()
            .max_by_key(|(_, r)| r.x)
            .map(|(i, _)| i)
            .unwrap();
        let v = view_of(cells, gridv(), right);
        let (ox, oy, cols, rows, cc, cr) = focused_cell_visual_scope(&v, a, &zj()).expect("scope");
        let rect = rects[right];
        assert!(ox > rect.x - 1 && ox + cols <= rect.x + rect.width);
        assert!(oy >= rect.y && oy + rows <= rect.y + rect.height);
        // The cursor is expressed relative to the origin and stays inside.
        assert!(cc < cols && cr < rows);
        // ... and it is NOT in any other cell.
        for (i, other) in rects.iter().enumerate() {
            if i == right {
                continue;
            }
            let (x, y) = (ox + cc, oy + cr);
            let inside = x >= other.x
                && x < other.x + other.width
                && y >= other.y
                && y < other.y + other.height;
            assert!(!inside, "visual cursor landed in cell {i}");
        }
    }

    /// A focused cell showing a placeholder still scopes to ITS OWN interior --
    /// the selection is empty, but the cursor stays in the cell the user is
    /// looking at, which is the property Bug B broke.
    #[test]
    fn focused_cell_visual_scope_falls_back_to_the_interior() {
        let a = area(80, 25);
        let v = view_of(vec![cell_with(1, None)], gridv(), 0);
        let rect = cell_rects(&v, a)[0].unwrap();
        let (ox, oy, cols, rows, cc, cr) = focused_cell_visual_scope(&v, a, &zj()).expect("scope");
        assert_eq!((cc, cr), (0, 0));
        assert!(ox >= rect.x && oy >= rect.y);
        assert!(ox + cols <= rect.x + rect.width);
        assert!(oy + rows <= rect.y + rect.height);
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
        let buf = composite(&v2, a, &tt(), "NORMAL", &zj());
        let joined: String = buf.iter().flat_map(|r| r.iter().map(|c| c.c)).collect();
        assert!(joined.contains('A') && !joined.contains('B'));
    }

    #[test]
    fn zoom_suppresses_monocle_strip() {
        let mut v = view_n(3, monoclev(), 1);
        v.zoomed = true;
        assert!(monocle_strip_rect(&v, area(80, 24)).is_none());
    }

    // -- prune tree maintenance (test-only helper; splice/prune now server-owned)

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
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL", &zj());
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
        let buf = composite(&view, area(80, 24), &theme, "NORMAL", &zj());
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
        // Mirrors the regular strip's inactive-tab block: the `tab_inactive_bg`
        // / `tab_inactive_fg` roles (which default to the historical
        // `Indexed(237)` / overlay2).
        let theme = tt();
        let mut cells: Vec<ViewCell> = (0..2).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("aa".into());
        cells[1].title = Some("bb".into());
        let view = view_of(cells, monoclev(), 0); // cell 0 focused/active
        let buf = composite(&view, area(80, 24), &theme, "NORMAL", &zj());
        let row0 = &buf[0];
        let bpos = row0.iter().position(|c| c.c == 'b').unwrap();
        assert_eq!(row0[bpos].bg, theme.tab_inactive_bg);
        assert_eq!(row0[bpos].fg, theme.tab_inactive_fg);
        assert!(!row0[bpos].bold, "inactive tab is not bold");
    }

    #[test]
    fn monocle_strip_render_matches_tab_strip_layout() {
        // Hit-testing (`cell_at` via `tab_strip_layout`) must agree with what
        // `build_top_border_content` actually paints. Assert each tab's rendered
        // start column (first cell whose bg is a tab block, i.e. NOT the strip's
        // default-bg leading space / separator) equals the layout's start.
        let theme = tt();
        let mut cells: Vec<ViewCell> = (0..3).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("alpha".into());
        cells[1].title = Some("bb".into());
        cells[2].title = Some("gamma".into());
        let view = view_of(cells, monoclev(), 1); // middle cell focused
        let a = area(80, 24);
        let buf = composite(&view, a, &theme, "NORMAL", &zj());
        let strip = monocle_strip_rect(&view, a).unwrap();
        let width = strip.width as usize;
        let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
        let segs = tab_strip_layout(&titles, width, &zj());
        assert_eq!(segs.len(), 3, "three visible tabs");
        let (_, mode_bg) = theme.mode_colors("NORMAL");
        for TabStripEntry {
            index: idx,
            start,
            end,
        } in segs
        {
            // Every column of the tab carries a tab background block (mode color
            // for the focused tab, Indexed(237) otherwise) rather than the
            // default-bg spaces used for the leading space and " | " separators.
            let expect_bg = if idx == view.focused {
                mode_bg.clone()
            } else {
                theme.tab_inactive_bg.clone()
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
            id: 0,
            name: "v".into(),
            cells,
            layout: monoclev(),
            focused: 1,
            custom_tree: None,
            zoomed: false,
        };
        let theme = tt();
        let a = area(80, 24);
        let buf = composite(&view, a, &theme, "NORMAL", &zj());
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
        assert_eq!(buf[0][apos].bg, theme.tab_inactive_bg);
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
            id: 0,
            name: "v".into(),
            cells: vec![waiting, disconnected],
            layout: monoclev(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL", &zj());
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
            id: 0,
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(78, 24, 'A'))),
                cell_with(2, Some(snap_filled(78, 24, 'B'))),
            ],
            layout: monoclev(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL", &zj());
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
            id: 0,
            name: "v".into(),
            cells,
            layout: monoclev(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let a = area(80, 24);
        // Locate beta's entry on the strip via the same segment layout used to
        // draw it, then hit-test its middle column.
        let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
        let strip = monocle_strip_rect(&view, a).unwrap();
        let segs = tab_strip_layout(&titles, strip.width as usize, &zj());
        let beta = segs.iter().find(|e| e.index == 1).unwrap();
        let mid = strip.x + ((beta.start + beta.end) / 2) as u16;
        assert_eq!(cell_at(&view, a, mid, strip.y, &zj()), Some(1));
        // A click below the strip resolves to the focused cell (0).
        assert_eq!(cell_at(&view, a, 5, 5, &zj()), Some(0));
    }

    #[test]
    fn monocle_strip_click_hits_a_lone_cell_from_its_first_column() {
        // THE off-by-one this refactor fixes. A 1-cell Monocle view's strip is
        // drawn by `build_top_border_content`'s single-title branch as a bare
        // ` title ` chip flush at column 0 -- but the client's own copy of the
        // tab-width formula applied the MULTI-tab leading pad unconditionally and
        // reported the chip at 1..len+3. So a click on the chip's first column
        // missed the only cell there was, and the last column resolved to
        // nothing. Both sides now read `tab_strip_layout`.
        let mut cells: Vec<ViewCell> = (0..1).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("solo / Tab 1".into());
        let view = view_of(cells, monoclev(), 0);
        let a = area(80, 24);
        let strip = monocle_strip_rect(&view, a).unwrap();
        let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
        let segs = tab_strip_layout(&titles, strip.width as usize, &zj());
        assert_eq!(segs[0].start, 0, "a lone chip is flush at the strip start");

        // Every column of the painted chip resolves to cell 0 -- including the
        // FIRST, which is what used to miss.
        for rel in segs[0].start..segs[0].end {
            assert_eq!(
                cell_at(&view, a, strip.x + rel as u16, strip.y, &zj()),
                Some(0),
                "strip column {rel} did not hit the only cell"
            );
        }

        // And the chip really is painted where the layout says it is.
        let buf = composite(&view, a, &tt(), "NORMAL", &zj());
        let text: String = (segs[0].start..segs[0].end)
            .map(|x| buf[strip.y as usize][strip.x as usize + x].c)
            .collect();
        assert_eq!(text, " solo / Tab 1 ");
    }

    #[test]
    fn monocle_strip_hit_testing_is_char_based_for_wide_titles() {
        // A cell title can be multi-byte (`<host>: pane N` for a remote source),
        // and the strip is laid out in COLUMNS. Measuring in bytes would place
        // every tab after the first far to the right of where it is painted.
        let mut cells: Vec<ViewCell> = (0..2).map(|id| cell_with(id, None)).collect();
        cells[0].title = Some("日本語".into());
        cells[1].title = Some("ab".into());
        let view = view_of(cells, monoclev(), 0);
        let a = area(80, 24);
        let strip = monocle_strip_rect(&view, a).unwrap();
        let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
        let segs = tab_strip_layout(&titles, strip.width as usize, &zj());
        // 3 CHARS + 2 padding = 5-wide tabs, not 9 bytes + 2.
        assert_eq!(segs[0].end - segs[0].start, 5);
        for entry in &segs {
            let mid = strip.x + ((entry.start + entry.end) / 2) as u16;
            assert_eq!(
                cell_at(&view, a, mid, strip.y, &zj()),
                Some(entry.index),
                "tab {} mid column resolved elsewhere",
                entry.index
            );
        }
    }

    #[test]
    fn monocle_composite_tiny_area_with_cells_no_panic() {
        // Mirrors the empty-view tiny-area guard, but with cells: Monocle must
        // not panic on degenerate heights. Height 2 is the interesting case
        // (strip row 0, focused rect height 1 -> no border).
        let view = ClientView {
            id: 0,
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(10, 3, 'A'))),
                cell_with(2, None),
            ],
            layout: monoclev(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        for (w, h) in [(1u16, 1u16), (5, 1), (10, 2), (10, 3), (0, 0)] {
            let buf = composite(&view, area(w, h), &tt(), "NORMAL", &zj());
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
            id: 0,
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(40, 24, 'A'))),
                cell_with(2, Some(snap_filled(40, 24, 'B'))),
            ],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL", &zj());
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
            id: 0,
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(40, 24, 'A'))),
                cell_with(2, Some(snap_filled(40, 24, 'B'))),
            ],
            layout: gridv(),
            focused: 1,
            custom_tree: None,
            zoomed: false,
        };
        let a = area(80, 24);
        let theme = tt();
        let buf = composite(&view, a, &theme, "NORMAL", &zj());
        let rects = cell_rects(&view, a);

        // The focused/unfocused distinction is the SAME one the server makes
        // between panes -- `frame_active_fg` vs `frame_fg`, no bold -- because a
        // view cell's border is drawn by the server's own `draw_zellij_border`.
        // (It used to be a hardcoded `Indexed(10)`/`Indexed(8)` + bold pair that
        // matched no theme and never matched a normal tab.)
        let f = rects[1].unwrap();
        let fc = &buf[f.y as usize][f.x as usize];
        assert_eq!(fc.c, '╭');
        assert_eq!(fc.fg, theme.frame_active_fg);
        assert!(!fc.bold);

        // Unfocused cell (index 0) corner is the inactive frame color.
        let u = rects[0].unwrap();
        let uc = &buf[u.y as usize][u.x as usize];
        assert_eq!(uc.c, '╭');
        assert_eq!(uc.fg, theme.frame_fg);
        assert!(!uc.bold);
        assert_ne!(fc.fg, uc.fg);
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
            id: 0,
            name: "v".into(),
            cells: vec![cell_with(1, Some(snap))],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL", &zj());

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
            id: 0,
            name: "v".into(),
            cells: vec![cell_with(1, Some(snap))],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let a = area(80, 24);
        let buf = composite(&view, a, &tt(), "NORMAL", &zj());
        // Interior starts at row 1 (under the top border).
        assert_eq!(buf[1][2].c, 'S');
    }

    #[test]
    fn composite_empty_snapshot_shows_placeholder() {
        let view = ClientView {
            id: 0,
            name: "v".into(),
            cells: vec![cell_with(42, None)],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL", &zj());
        // A cell with no snapshot yet shows a `waiting…` placeholder.
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains("waiting"));
    }

    /// The no-snapshot placeholder nests `cell_title` inside `waiting for {}…`,
    /// so the titleless fallback must be an IDENTITY, never a status word --
    /// a `waiting…` fallback rendered the nonsense `waiting for waiting……`.
    #[test]
    fn composite_titleless_cell_never_doubles_the_placeholder() {
        let mut cell = cell_with(7, None);
        cell.conn = ConnId::Remote("mini".into());
        let view = view_of(vec![cell], gridv(), 0);
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL", &zj());
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(!joined.contains("waiting for waiting"));
        // Both call sites read sensibly: the border label names the cell, and the
        // placeholder names what is being waited for.
        assert!(joined.contains("mini: pane 7"));
        assert!(joined.contains("waiting for mini: pane 7"));
    }

    /// A cell whose source server this terminal cannot reach can never receive a
    /// snapshot, so it must say so instead of waiting forever.
    #[test]
    fn composite_unavailable_cell_shows_reason_not_waiting() {
        let mut cell = cell_with(3, None);
        cell.conn = ConnId::Remote("mini".into());
        cell.unavailable = Some("not connected: mini".to_string());
        let view = view_of(vec![cell], gridv(), 0);
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL", &zj());
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains("not connected: mini"));
        assert!(!joined.contains("waiting"));
    }

    /// `disconnected` (the source dropped) outranks `unavailable` (we never got
    /// there), so a dropped cell keeps its established label.
    #[test]
    fn composite_disconnected_outranks_unavailable() {
        let mut cell = cell_with(4, None);
        cell.disconnected = true;
        cell.unavailable = Some("not connected: mini".to_string());
        let view = view_of(vec![cell], gridv(), 0);
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL", &zj());
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains("disconnected"));
        assert!(!joined.contains("not connected"));
    }

    /// A `ViewList` resync must carry `unavailable` forward, or the cell would
    /// snap back to `waiting…` on every broadcast.
    #[test]
    fn from_info_carries_unavailable_forward() {
        let mut prev_cell = cell_with(5, None);
        prev_cell.conn = ConnId::Remote("mini".into());
        prev_cell.unavailable = Some("not connected: mini".to_string());
        let prev = view_of(vec![prev_cell], gridv(), 0);
        let info = ViewInfo {
            id: prev.id,
            name: prev.name.clone(),
            cells: vec![crate::protocol::CellInfo {
                id: 5,
                conn: ConnDescriptor::Remote("mini".into()),
                pane_id: 5,
            }],
            layout: gridv(),
            custom_tree: None,
            focused: 0,
            zoomed: false,
        };
        let rebuilt = ClientView::from_info(&info, Some(&prev));
        assert_eq!(
            rebuilt.cells[0].unavailable.as_deref(),
            Some("not connected: mini")
        );
    }

    /// A cell whose source pane the server reported gone says `pane closed`, and
    /// the last snapshot it happens to hold is NOT painted -- presenting frozen
    /// content as live is the lie this state exists to prevent.
    #[test]
    fn composite_exited_cell_shows_pane_closed_not_stale_content() {
        let mut cell = cell_with(6, Some(snap_filled(40, 20, 'A')));
        cell.exited = true;
        let view = view_of(vec![cell], gridv(), 0);
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL", &zj());
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains("pane closed"));
        assert!(!joined.contains("waiting"));
        assert!(!joined.contains('A'));
    }

    /// `exited` is a fact the server reported; `disconnected`/`unavailable` are
    /// inferences about reachability. The fact wins.
    #[test]
    fn composite_exited_outranks_disconnected_and_unavailable() {
        let mut cell = cell_with(7, None);
        cell.exited = true;
        cell.disconnected = true;
        cell.unavailable = Some("not connected: mini".to_string());
        let view = view_of(vec![cell], gridv(), 0);
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL", &zj());
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains("pane closed"));
        assert!(!joined.contains("disconnected"));
        assert!(!joined.contains("not connected"));
    }

    /// An exited cell has no source grid, so it offers no content geometry and
    /// shows no cursor even while focused -- a cursor there would invite typing
    /// into a pane that no longer exists.
    #[test]
    fn exited_cell_has_no_content_geometry_or_cursor() {
        let mut cell = cell_with(8, Some(snap_filled(40, 20, 'A')));
        cell.exited = true;
        let view = view_of(vec![cell], gridv(), 0);
        assert!(cell_content_geometry(&view, area(80, 24), 0, &zj()).is_none());
        assert!(focused_cursor(&view, area(80, 24), &zj()).is_none());
    }

    /// A `ViewList` resync must carry `exited` forward. Without it the cell would
    /// reset on every broadcast, be re-subscribed, and flicker between
    /// `waiting…` and `pane closed` as the server re-reported the death.
    #[test]
    fn from_info_carries_exited_forward() {
        let mut prev_cell = cell_with(9, None);
        prev_cell.exited = true;
        let prev = view_of(vec![prev_cell], gridv(), 0);
        let info = ViewInfo {
            id: prev.id,
            name: prev.name.clone(),
            cells: vec![crate::protocol::CellInfo {
                id: 9,
                conn: ConnDescriptor::Local,
                pane_id: 9,
            }],
            layout: gridv(),
            custom_tree: None,
            focused: 0,
            zoomed: false,
        };
        let rebuilt = ClientView::from_info(&info, Some(&prev));
        assert!(rebuilt.cells[0].exited);
    }

    #[test]
    fn composite_disconnected_cell_shows_label() {
        let mut cell = cell_with(1, Some(snap_filled(40, 20, 'A')));
        cell.disconnected = true;
        let view = ClientView {
            id: 0,
            name: "v".into(),
            cells: vec![cell],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL", &zj());
        let joined: String = buf.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(joined.contains("disconnected"));
        // The stale snapshot content must NOT bleed through.
        assert!(!joined.contains('A'));
    }

    #[test]
    fn monocle_draws_only_focused_cell() {
        let view = ClientView {
            id: 0,
            name: "v".into(),
            cells: vec![
                cell_with(1, Some(snap_filled(78, 24, 'A'))),
                cell_with(2, Some(snap_filled(78, 24, 'B'))),
            ],
            layout: monoclev(),
            focused: 1,
            custom_tree: None,
            zoomed: false,
        };
        let buf = composite(&view, area(80, 24), &tt(), "NORMAL", &zj());
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
            id: 0,
            name: "v".into(),
            cells: vec![cell_with(1, None), cell_with(2, None)],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let a = area(80, 24);
        let rects = cell_rects(&view, a);
        // A point inside each rect resolves to that index.
        for (i, r) in rects.iter().enumerate() {
            let r = r.unwrap();
            let x = r.x + r.width / 2;
            let y = r.y + r.height / 2;
            assert_eq!(cell_at(&view, a, x, y, &zj()), Some(i));
        }
        // A click on the reserved status row hits nothing.
        assert_eq!(cell_at(&view, a, 10, a.height - 1, &zj()), None);
        // Empty view: no hit.
        let empty = ClientView::new("e".into());
        assert_eq!(cell_at(&empty, a, 10, 10, &zj()), None);
    }

    #[test]
    fn cell_at_monocle_keeps_focus() {
        let view = ClientView {
            id: 0,
            name: "v".into(),
            cells: vec![cell_with(1, None), cell_with(2, None)],
            layout: monoclev(),
            focused: 1,
            custom_tree: None,
            zoomed: false,
        };
        assert_eq!(cell_at(&view, area(80, 24), 5, 5, &zj()), Some(1));
    }

    #[test]
    fn focused_cursor_only_when_visible_and_focused() {
        let mut snap = snap_filled(40, 10, 'A');
        snap.cursor_visible = true;
        snap.cursor_x = 3;
        snap.cursor_y = 9; // last row of the snapshot
        let view = ClientView {
            id: 0,
            name: "v".into(),
            cells: vec![cell_with(1, Some(snap.clone())), cell_with(2, Some(snap))],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        let a = area(80, 24);
        // Focused cell 0: interior origin (ix=1, iy=1); snapshot (10 rows) fits in
        // the interior so start=0 -> cursor row = iy + 9, col = ix + 3.
        let rects = cell_rects(&view, a);
        let f = rects[0].unwrap();
        let got = focused_cursor(&view, a, &zj()).expect("cursor shown");
        assert_eq!(got, (f.x + 1 + 3, f.y + 1 + 9));

        // A hidden source cursor -> no cursor.
        let mut hidden = snap_filled(40, 10, 'A');
        hidden.cursor_visible = false;
        let view2 = ClientView {
            id: 0,
            name: "v".into(),
            cells: vec![cell_with(1, Some(hidden))],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        assert_eq!(focused_cursor(&view2, a, &zj()), None);

        // A disconnected focused cell -> no cursor.
        let mut cell = cell_with(1, Some(snap_filled(40, 10, 'A')));
        if let Some(s) = cell.snapshot.as_mut() {
            s.cursor_visible = true;
        }
        cell.disconnected = true;
        let view3 = ClientView {
            id: 0,
            name: "v".into(),
            cells: vec![cell],
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
        };
        assert_eq!(focused_cursor(&view3, a, &zj()), None);
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
            id: 0,
            name: "v".into(),
            cells: (1..=4).map(|id| cell_with(id, None)).collect(),
            layout: gridv(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
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
            id: 0,
            name: "v".into(),
            cells: (1..=2).map(|id| cell_with(id, None)).collect(),
            layout: gridv(),
            focused: 1, // B (bottom) focused
            custom_tree: Some(tree),
            zoomed: false,
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
            id: 0,
            name: "v".into(),
            cells: (1..=3).map(|id| cell_with(id, None)).collect(),
            layout: monoclev(),
            focused: 0,
            custom_tree: None,
            zoomed: false,
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
            id: 0,
            name: "v".into(),
            cells: (1..=3).map(|id| cell_with(id, None)).collect(),
            layout: gridv(),
            focused: 2,
            custom_tree: None,
            zoomed: false,
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
                id: 0,
                name: "empty".into(),
                cells: vec![],
                layout,
                focused: 0,
                custom_tree: None,
                zoomed: false,
            };
            let a = area(80, 24);
            let buf = composite(&view, a, &tt(), "NORMAL", &zj());
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
            let buf = composite(&view, area(w, h), &tt(), "NORMAL", &zj());
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

    // -- Border-style parity with a normal tab -------------------------------

    /// Give every stack in `node` the same display names a view gives its
    /// titleless local cells (`cell_title`'s `pane <id>` fallback), so a server
    /// pane's top-border label is directly comparable to a view cell's.
    fn name_stacks(node: &mut LayoutNode) {
        match node {
            LayoutNode::Stack { panes, names, .. } => {
                *names = panes.iter().map(|id| format!("pane {id}")).collect();
            }
            LayoutNode::Split { first, second, .. } => {
                name_stacks(first);
                name_stacks(second);
            }
        }
    }

    /// Composite the same cell arrangement as a NORMAL TAB via the server
    /// compositor: same tree, same content area (`cells_area`, i.e. one row
    /// reserved for the status bar), same focused pane, same theme.
    fn server_frame(view: &ClientView, a: Rect, style: &BorderStyle) -> Vec<Vec<RenderCell>> {
        use crate::screen::Screen;
        use crate::server::compositor::StatusInfo;
        use crate::server::session::TabActivity;
        let mut tree = view.auto_tree();
        name_stacks(&mut tree);
        let inner = cells_area(a);
        let screens: Vec<(PaneId, Screen)> = view
            .cells
            .iter()
            .map(|c| (c.id, Screen::new(a.width, inner.height, 100)))
            .collect();
        let mut pane_screens: std::collections::HashMap<PaneId, &Screen> =
            std::collections::HashMap::new();
        for (id, s) in &screens {
            pane_screens.insert(*id, s);
        }
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "s".to_string(),
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
            layout_mode: "grid".to_string(),
            search_info: None,
        };
        let (buf, _) = crate::server::compositor::composite(
            &tree,
            &pane_screens,
            inner,
            style,
            &status,
            a.width,
            a.height,
            0,
            view.focused_id(),
            None,
            &std::collections::HashMap::new(),
            &tt(),
        );
        buf
    }

    /// The decisive parity check for bug 1: composite the same three-cell Grid
    /// arrangement BOTH as a view (client compositor) and as a normal tab (server
    /// compositor), then assert every border cell agrees on glyph, colors and
    /// bold. It fails the moment either side grows its own box-drawing code.
    #[test]
    fn view_cell_frame_matches_server_pane_frame_zellij() {
        let a = area(80, 24);
        let view = view_n(3, gridv(), 1);
        let vbuf = composite(&view, a, &tt(), "NORMAL", &zj());
        let sbuf = server_frame(&view, a, &BorderStyle::ZellijStyle);

        let mut checked = 0usize;
        for rect in cell_rects(&view, a).into_iter().flatten() {
            let (x0, y0) = (rect.x as usize, rect.y as usize);
            let (x1, y1) = (x0 + rect.width as usize - 1, y0 + rect.height as usize - 1);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    // Perimeter only: the interior holds content, not frame.
                    if y != y0 && y != y1 && x != x0 && x != x1 {
                        continue;
                    }
                    let v = &vbuf[y][x];
                    let s = &sbuf[y][x];
                    assert_eq!(
                        (v.c, &v.fg, &v.bg, v.bold),
                        (s.c, &s.fg, &s.bg, s.bold),
                        "border mismatch at ({x},{y})"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 100, "too few border cells compared: {checked}");

        // And no square corner survives anywhere in the view.
        let all: String = vbuf.iter().flatten().map(|c| c.c).collect();
        assert!(all.contains('╭') && all.contains('╮'));
        assert!(all.contains('╰') && all.contains('╯'));
        for sq in ['┌', '┐', '└', '┘'] {
            assert!(!all.contains(sq), "square corner {sq} still drawn");
        }
    }

    /// Tmux style: the view draws no box at all and puts the SAME dividers the
    /// server puts between tmux panes, in the same places and colors.
    #[test]
    fn view_cells_match_server_panes_tmux() {
        let a = area(80, 24);
        let view = view_n(3, gridv(), 1);
        let vbuf = composite(&view, a, &tt(), "NORMAL", &tmx());
        let sbuf = server_frame(&view, a, &BorderStyle::TmuxStyle);

        let mut dividers = 0usize;
        for (y, srow) in sbuf.iter().enumerate().take(a.height as usize - 1) {
            for (x, s) in srow.iter().enumerate() {
                if s.c == '\u{2502}' || s.c == '\u{2500}' {
                    let v = &vbuf[y][x];
                    assert_eq!((v.c, &v.fg), (s.c, &s.fg), "divider mismatch at ({x},{y})");
                    dividers += 1;
                }
            }
        }
        assert!(dividers > 20, "too few dividers compared: {dividers}");

        // No box border, no rounded corners, no per-cell title in tmux style.
        let all: String = vbuf.iter().flatten().map(|c| c.c).collect();
        for ch in ['╭', '╮', '╰', '╯'] {
            assert!(!all.contains(ch), "tmux style drew a box corner {ch}");
        }
    }

    /// Toggling the style must actually change what a view paints (bug 2: the
    /// view rendered identically in both styles because it ignored the style).
    #[test]
    fn toggling_style_changes_what_a_view_paints() {
        let a = area(80, 24);
        let view = view_n(2, gridv(), 0);
        let z: Vec<char> = composite(&view, a, &tt(), "NORMAL", &zj())
            .iter()
            .flatten()
            .map(|c| c.c)
            .collect();
        let t: Vec<char> = composite(&view, a, &tt(), "NORMAL", &tmx())
            .iter()
            .flatten()
            .map(|c| c.c)
            .collect();
        assert_ne!(z, t);
    }

    /// The subscription size is the region actually painted, per style.
    #[test]
    fn cell_content_size_tracks_the_border_style() {
        let r = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 12,
        };
        assert_eq!(cell_content_size(r, &zj()), (38, 10));
        assert_eq!(cell_content_size(r, &tmx()), (40, 12));
        // Below the shared border threshold zellij also goes edge-to-edge.
        let tiny = Rect {
            width: 2,
            height: 2,
            ..r
        };
        assert_eq!(cell_content_size(tiny, &zj()), (2, 2));
    }

    /// The tmux Monocle strip is the server's tab bar (status-bar background),
    /// and hit-testing follows it: flush left, with no zellij leading space.
    #[test]
    fn monocle_strip_tmux_uses_tab_bar_and_flush_hit_testing() {
        let a = area(80, 24);
        let view = view_n(3, monoclev(), 1);
        let theme = tt();
        let buf = composite(&view, a, &theme, "NORMAL", &tmx());
        let strip = monocle_strip_rect(&view, a).expect("strip");
        let row = &buf[strip.y as usize];
        let last = strip.width as usize - 1;
        // The tmux tab bar fills the whole row with the status-bar background;
        // the zellij top-border strip writes only its tab cells and leaves the
        // rest of the row on the default background.
        assert_eq!(row[last].bg, theme.status_bar_bg);
        let zrow = &composite(&view, a, &theme, "NORMAL", &zj())[strip.y as usize];
        assert_eq!(zrow[last].bg, CellColor::Default);
        let text: String = row.iter().map(|c| c.c).collect();
        assert!(text.contains("pane 1"), "titles missing: {text:?}");

        let titles: Vec<String> = view.cells.iter().map(cell_title).collect();
        let zsegs = tab_strip_layout(&titles, strip.width as usize, &zj());
        let tsegs = tab_strip_layout(&titles, strip.width as usize, &tmx());
        assert_eq!(
            zsegs[0].start, 1,
            "zellij strip starts after a leading space"
        );
        assert_eq!(tsegs[0].start, 0, "tmux tab bar starts flush left");
        // A click on the first tab still resolves to cell 0 in tmux style.
        assert_eq!(cell_at(&view, a, strip.x, strip.y, &tmx()), Some(0));
    }
}
