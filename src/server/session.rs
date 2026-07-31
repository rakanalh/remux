//! Session model for Remux terminal multiplexer.
//!
//! This module manages the bookkeeping for sessions, folders, and tabs.
//! It is pure -- no PTY management, no I/O -- just state management.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::layout::{
    self, find_neighbor, relocate_pane_to_edge, swap_panes, Direction, FocusDirection, GridLayout,
    LayoutMode, LayoutNode, PaneId, Rect,
};
use crate::config::BorderStyle;
use crate::protocol::{
    CellId, ConnDescriptor, FolderTreeEntry, PaneTreeEntry, SessionTreeEntry, TabTreeEntry, ViewId,
};

/// Unique identifier for a session (its name).
pub type SessionId = String;

/// Unique identifier for a folder (its name).
pub type FolderId = String;

/// Unique identifier for a tab.
pub type TabId = u64;

/// Summary information about a session, returned by listing operations.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    pub folder: Option<String>,
    pub tab_count: usize,
    pub pane_count: usize,
}

/// Summary information about a folder, returned by listing operations.
#[derive(Debug, Clone)]
pub struct FolderInfo {
    pub name: String,
    pub session_count: usize,
}

/// Top-level server state containing all sessions and folders.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServerState {
    pub folders: HashMap<FolderId, Folder>,
    pub sessions: HashMap<SessionId, Session>,
    next_pane_id: u64,
    next_tab_id: u64,
    /// The server-owned shared-view registry. Runtime-only: views alias live
    /// panes and are meaningless once a session is dormant/restored, so they are
    /// never persisted (`#[serde(skip)]`) — a restarted server starts with none.
    #[serde(skip)]
    pub views: Vec<ServerView>,
    /// Monotonic source for [`ViewId`]s. Runtime-only (see `views`); guarded to
    /// stay 1-based even after a deserialize resets it to 0.
    #[serde(skip)]
    next_view_id: ViewId,
}

/// One cell of a [`ServerView`]: a reference to a real pane on a specific
/// connection (from the client's perspective). The server stores the descriptor
/// verbatim and never resolves it — resolution is the client's job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerViewCell {
    /// Stable, per-view identity, also used as the pseudo-[`PaneId`] in the
    /// view's layout tree (so add/remove never invalidates a `custom_tree`).
    pub id: CellId,
    pub conn: ConnDescriptor,
    pub pane_id: PaneId,
}

/// A shared, server-owned View: a virtual tab whose cells alias real panes on
/// any connection. Mirrors the client-side `ClientView` model (same layout
/// engine, same stable-id cell keys, same custom-tree semantics) but lives on
/// the server so every connected client sees one consistent arrangement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerView {
    pub id: ViewId,
    pub name: String,
    pub cells: Vec<ServerViewCell>,
    /// Automatic layout mode; defaults to Grid on create (matching the client).
    pub layout: LayoutMode,
    /// Persistent manual arrangement keyed by [`ServerViewCell::id`]; `Some`
    /// once a cell has been resized/moved. While `Some`, `layout_name()` reads
    /// `custom`. `ViewCycleLayout` resets it to `None`.
    pub custom_tree: Option<LayoutNode>,
    /// Index into `cells` of the focused cell.
    pub focused: usize,
    /// Whether only the focused cell is shown (zoom).
    pub zoomed: bool,
    /// Monotonic source for this view's [`CellId`]s.
    next_cell_id: CellId,
}

impl ServerView {
    /// A fresh empty view (Grid layout, no cells).
    fn new(id: ViewId, name: String) -> Self {
        ServerView {
            id,
            name,
            cells: Vec::new(),
            layout: LayoutMode::Grid(GridLayout),
            custom_tree: None,
            focused: 0,
            zoomed: false,
            next_cell_id: 1,
        }
    }

    /// The layout name shown to clients: `custom` while a `custom_tree` is
    /// active, else the automatic mode's name. Mirrors `ClientView::layout_name`.
    pub fn layout_name(&self) -> &str {
        if self.custom_tree.is_some() {
            "custom"
        } else {
            self.layout.name()
        }
    }

    /// The stable id of the focused cell, or `0` when the view is empty.
    fn focused_id(&self) -> CellId {
        self.cells.get(self.focused).map(|c| c.id).unwrap_or(0)
    }

    /// The array index of the cell with stable `id`, if present.
    fn index_of_id(&self, id: CellId) -> Option<usize> {
        self.cells.iter().position(|c| c.id == id)
    }

    /// Build the automatic layout tree over the current cells (keyed by stable
    /// id, focused cell active). Mirrors `ClientView::auto_tree`.
    fn auto_tree(&self) -> LayoutNode {
        let ids: Vec<PaneId> = self.cells.iter().map(|c| c.id).collect();
        self.layout.build_tree(&ids, self.focused_id())
    }

    /// Clamp `focused` into range after the cell list changed.
    fn clamp_focus(&mut self) {
        if self.cells.is_empty() {
            self.focused = 0;
        } else if self.focused >= self.cells.len() {
            self.focused = self.cells.len() - 1;
        }
    }
}

/// A folder groups related sessions together.
#[derive(Debug, Serialize, Deserialize)]
pub struct Folder {
    pub name: String,
    pub session_ids: Vec<SessionId>,
}

/// A session contains one or more tabs.
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub folder: Option<FolderId>,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    /// Border rendering style for this session. Initialized from
    /// `config.appearance.border_style` and toggled at runtime.
    #[serde(default = "default_border_style")]
    pub border_style: BorderStyle,
    /// Tracks an in-progress pane rename: (pane_id, original_name).
    /// Present only while a client is actively typing a new name.
    #[serde(skip)]
    pub rename_state: Option<(PaneId, String)>,
    /// The session's popup-terminal pane, spawned lazily on the first
    /// `PopupToggle` and kept for the session's lifetime.
    ///
    /// A real PTY pane in the daemon's pane map, but deliberately **NOT** a
    /// member of any tab's `pane_order` or layout tree -- it floats above the
    /// layout instead of taking space in it. Runtime-only: PTYs don't survive a
    /// server restart (a restored session re-spawns its shells), so persisting
    /// this would resurrect an empty box.
    #[serde(skip)]
    pub popup_pane: Option<PaneId>,
    /// Whether the popup is currently drawn on top of the layout. Session-scoped
    /// (shared by every attached client, like `Tab::zoomed_pane`) and
    /// runtime-only for the same reason as `popup_pane`.
    #[serde(skip)]
    pub popup_visible: bool,
    /// The popup's `(width_pct, height_pct)` of the session's content area.
    /// Seeded from `config.appearance.popup_{width,height}_pct` and adjusted at
    /// runtime by the resize commands; the adjustment sticks for the session.
    #[serde(skip, default = "default_popup_size")]
    pub popup_size: (u8, u8),
}

impl Session {
    /// The pane a client's input (keys, mouse, scroll) should reach.
    ///
    /// **The popup input chokepoint.** While the popup is visible it owns input,
    /// but `Tab::focused_pane` is deliberately left untouched so hiding the popup
    /// returns input to exactly the pane that had it before.
    pub fn input_target(&self) -> Option<PaneId> {
        if self.popup_visible {
            if let Some(popup) = self.popup_pane {
                return Some(popup);
            }
        }
        self.tabs.get(self.active_tab).map(|t| t.focused_pane)
    }

    /// Clear the popup, returning its pane id so the caller can kill the PTY.
    pub fn take_popup(&mut self) -> Option<PaneId> {
        self.popup_visible = false;
        self.popup_pane.take()
    }

    /// Apply a runtime popup resize by adding `(dw, dh)` percentage points,
    /// clamped to the supported range. Returns the new size.
    pub fn resize_popup(&mut self, dw: i16, dh: i16) -> (u8, u8) {
        let adjust =
            |cur: u8, delta: i16| -> u8 { (cur as i16 + delta).clamp(0, u8::MAX as i16) as u8 };
        self.popup_size = layout::clamp_popup_size((
            adjust(self.popup_size.0, dw),
            adjust(self.popup_size.1, dh),
        ));
        self.popup_size
    }
}

/// Per-tab activity state for background activity monitoring (tmux-like
/// `monitor-activity` / `monitor-silence`).
///
/// Only ever applies to *background* tabs (a tab that is not its session's
/// `active_tab`); the foreground tab is always [`TabActivity::None`] because it
/// is being viewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabActivity {
    /// No pending activity (default / cleared on focus).
    #[default]
    None,
    /// The tab produced new output while in the background ("needs attention").
    Activity,
    /// The tab emitted a terminal bell (BEL). Takes precedence over `Activity`
    /// until the tab is focused.
    Bell,
    /// The tab was active but has since gone quiet ("finished").
    Silent,
}

/// A tab holds a layout tree and tracks the focused pane.
#[derive(Debug, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub name: String,
    pub layout: LayoutNode,
    pub focused_pane: PaneId,
    #[serde(default)]
    pub layout_mode: LayoutMode,
    #[serde(default)]
    pub pane_order: Vec<PaneId>,
    #[serde(default)]
    pub zoomed_pane: Option<PaneId>,
    /// The user's last manually-arranged (`Custom`) layout tree, snapshotted
    /// when the automatic-layout cycle moves away from it so the cycle can
    /// return to it later. `None` until the user has a custom arrangement.
    #[serde(default)]
    pub saved_custom_layout: Option<LayoutNode>,
    /// Runtime-only background activity state. Not persisted: activity is a
    /// live-session concern, meaningless once a session is dormant/restored.
    #[serde(skip)]
    pub activity: TabActivity,
    /// Runtime-only timestamp of the most recent background output, used to
    /// promote `Activity` to `Silent` after a quiet threshold. Not persisted.
    #[serde(skip)]
    pub last_output: Option<Instant>,
}

impl Tab {
    /// **The one answer to "which panes does this tab own".**
    ///
    /// `pane_order` and the layout tree are two views of the same set (see
    /// [`Session::check_structural_invariant`]); consumers that picked one side
    /// at random used to disagree when the two drifted -- a restored session
    /// could silently get no PTY for a pane, or a View cell no title. Everything
    /// that just needs the membership goes through here, in insertion order (the
    /// order automatic layouts rebuild from). The popup pane is deliberately in
    /// neither side and so is never returned.
    pub fn panes(&self) -> &[PaneId] {
        &self.pane_order
    }

    /// The layout to render and size this tab with: the real tree, or -- while a
    /// pane is zoomed -- a synthetic single-pane stack holding the **zoomed**
    /// pane, so it owns the whole area.
    ///
    /// The zoom substitution is exactly this and nowhere else: the render path,
    /// the pane-rect math and the PTY sizing all take their layout from here, so
    /// they can never disagree about how many panes there are (a disagreement is
    /// what made a zoomed *stacked* pane's PTY one row shorter than the area
    /// painted for it under the tmux border style).
    pub fn effective_layout(&self) -> std::borrow::Cow<'_, LayoutNode> {
        match self.zoomed_pane {
            Some(zoomed) => std::borrow::Cow::Owned(LayoutNode::new_stack(zoomed)),
            None => std::borrow::Cow::Borrowed(&self.layout),
        }
    }

    /// Move focus to `pane_id`, carrying an active zoom along with it.
    ///
    /// `zoomed_pane` names the pane that is actually painted full-area, so it
    /// must follow focus: "zoom, then step to the next pane in the stack" keeps
    /// showing the pane you are typing into. Every focus change *within* a
    /// zoomed tab goes through here so the id can never go stale behind the
    /// zoom.
    pub fn focus_pane(&mut self, pane_id: PaneId) {
        self.focused_pane = pane_id;
        if self.zoomed_pane.is_some() {
            self.zoomed_pane = Some(pane_id);
        }
    }

    /// Make `pane_order` agree with the layout tree, keeping the existing order
    /// for the panes both sides know about.
    ///
    /// Only meaningful on deserialized state: `pane_order` is `#[serde(default)]`,
    /// so a state file written before the field existed restores an empty order
    /// with a fully populated tree. Repairing on the way in means the live
    /// invariant holds from the first frame and both consumers of the pane set
    /// see the same panes.
    pub fn reconcile_pane_order(&mut self) {
        let tree: Vec<PaneId> = layout::all_pane_ids(&self.layout);
        let tree_set: HashSet<PaneId> = tree.iter().copied().collect();
        let mut seen: HashSet<PaneId> = HashSet::new();
        let mut repaired: Vec<PaneId> = Vec::new();
        for &id in &self.pane_order {
            if tree_set.contains(&id) && seen.insert(id) {
                repaired.push(id);
            }
        }
        for id in tree {
            if seen.insert(id) {
                repaired.push(id);
            }
        }
        if repaired != self.pane_order {
            log::warn!(
                "tab {}: repaired pane_order {:?} -> {repaired:?}",
                self.id,
                self.pane_order
            );
            self.pane_order = repaired;
        }
        if let Some(zoomed) = self.zoomed_pane {
            if !self.pane_order.contains(&zoomed) {
                log::warn!("tab {}: dropped stale zoomed_pane {zoomed}", self.id);
                self.zoomed_pane = None;
            }
        }
    }
}

/// Return true if a tab currently in [`TabActivity::Activity`] should be
/// promoted to [`TabActivity::Silent`] given the current time `now` and the
/// silence `threshold`.
///
/// Pure and deterministic: takes an injected `now` so the promotion logic can
/// be unit-tested without real sleeps. Only `Activity` is eligible — `Bell`
/// stays `Bell`, and `None`/`Silent` are never promoted.
pub fn should_promote_to_silent(
    activity: TabActivity,
    last_output: Option<Instant>,
    now: Instant,
    threshold: Duration,
) -> bool {
    matches!(activity, TabActivity::Activity)
        && last_output
            .map(|t| now.duration_since(t) >= threshold)
            .unwrap_or(false)
}

fn default_border_style() -> BorderStyle {
    BorderStyle::ZellijStyle
}

/// **The hard structural invariant, as a reusable check.**
///
/// The popup pane is a real PTY that must never be spliced into the layout: if
/// it ever landed in a `pane_order`, a layout tree, or a `zoomed_pane`, then
/// `PaneMove*`/`SetMaster`/an automatic rebuild/a stack splice could capture it
/// and it would start taking space in (or replace) the layout. And the two
/// views of a tab's pane set must not drift, since different consumers read
/// different sides (see [`Tab::panes`]).
///
/// Checks, for **every** tab of `sess`:
/// 1. `popup_pane` is not in `tab.pane_order`;
/// 2. `popup_pane` is not in the tab's layout tree;
/// 3. `tab.zoomed_pane` is not the popup;
/// 4. the layout tree's pane set == `pane_order`'s set, with no duplicates
///    (the structural health check: catches orphans and dupes whether or not a
///    popup is involved);
/// 5. `tab.zoomed_pane`, when set, names a pane the tab still owns -- the id is
///    honoured by the render/sizing paths, so a stale one would paint a dead
///    pane full-screen.
///
/// Returns the first violation as a message rather than panicking, so
/// production code can `debug_assert` on it (see [`debug_check_invariant`]) and
/// tests can turn it into a hard assertion.
pub fn check_structural_invariant(sess: &Session) -> Result<(), String> {
    for (i, tab) in sess.tabs.iter().enumerate() {
        let tree_panes = layout::all_pane_ids(&tab.layout);
        let tree_set: HashSet<PaneId> = tree_panes.iter().copied().collect();
        let order_set: HashSet<PaneId> = tab.pane_order.iter().copied().collect();

        if let Some(popup) = sess.popup_pane {
            if tab.pane_order.contains(&popup) {
                return Err(format!(
                    "popup pane {popup} leaked into tab {i} pane_order {:?}",
                    tab.pane_order
                ));
            }
            if tree_set.contains(&popup) {
                return Err(format!(
                    "popup pane {popup} leaked into tab {i} layout tree {tree_panes:?}"
                ));
            }
            if tab.zoomed_pane == Some(popup) {
                return Err(format!("popup pane {popup} became tab {i}'s zoomed_pane"));
            }
        }

        if tree_panes.len() != tree_set.len() {
            return Err(format!(
                "tab {i} layout tree has duplicate panes: {tree_panes:?}"
            ));
        }
        if tab.pane_order.len() != order_set.len() {
            return Err(format!(
                "tab {i} pane_order has duplicates: {:?}",
                tab.pane_order
            ));
        }
        if tree_set != order_set {
            return Err(format!(
                "tab {i} layout tree {tree_panes:?} and pane_order {:?} disagree",
                tab.pane_order
            ));
        }
        if let Some(zoomed) = tab.zoomed_pane {
            if !order_set.contains(&zoomed) {
                return Err(format!(
                    "tab {i} zoomed_pane {zoomed} is not one of its panes {:?}",
                    tab.pane_order
                ));
            }
        }
    }
    Ok(())
}

/// Debug-build guard over [`check_structural_invariant`], called after every
/// structural mutation of a session (pane create/close, layout rebuilds). A
/// violation is a programming error in the mutation that just ran, so it panics
/// in debug/test builds and costs nothing in release.
pub fn debug_check_invariant(sess: &Session, context: &str) {
    if cfg!(debug_assertions) {
        if let Err(e) = check_structural_invariant(sess) {
            panic!("[{context}] session '{}': {e}", sess.name);
        }
    }
}

/// [`check_structural_invariant`] as a hard assertion, for tests.
#[cfg(test)]
pub(crate) fn assert_popup_invariant(sess: &Session, context: &str) {
    if let Err(e) = check_structural_invariant(sess) {
        panic!("[{context}] {e}");
    }
}

/// Fallback popup size for a session deserialized from disk (the field is
/// `#[serde(skip)]`, so restored sessions never carry one). Mirrors the
/// `AppearanceConfig` default.
fn default_popup_size() -> (u8, u8) {
    (80, 80)
}

impl ServerState {
    /// Create a new empty server state.
    pub fn new() -> Self {
        ServerState {
            folders: HashMap::new(),
            sessions: HashMap::new(),
            next_pane_id: 1,
            next_tab_id: 1,
            views: Vec::new(),
            next_view_id: 1,
        }
    }

    // -- Shared view registry ------------------------------------------------
    //
    // Pure mutators over the runtime-only view registry. Each returns enough for
    // the daemon to decide what to send; the daemon owns the `ViewList`
    // broadcast. `find_view_mut` centralises the fail-silent "unknown id" path
    // so every handler is index/id safe (no panics on a stale view/cell id).

    /// Allocate the next [`ViewId`], guarding against a deserialize that reset
    /// the counter to 0 so ids stay 1-based within a run.
    fn next_view_id(&mut self) -> ViewId {
        if self.next_view_id == 0 {
            self.next_view_id = 1;
        }
        let id = self.next_view_id;
        self.next_view_id += 1;
        id
    }

    fn find_view_mut(&mut self, id: ViewId) -> Option<&mut ServerView> {
        self.views.iter_mut().find(|v| v.id == id)
    }

    /// Create a new empty (Grid-default) view named `name`, returning its id.
    pub fn view_create(&mut self, name: String) -> ViewId {
        let id = self.next_view_id();
        self.views.push(ServerView::new(id, name));
        id
    }

    /// Delete view `id`. Fail-silent if absent.
    pub fn view_delete(&mut self, id: ViewId) {
        self.views.retain(|v| v.id != id);
    }

    /// Rename view `id`. Fail-silent if absent.
    pub fn view_rename(&mut self, id: ViewId, name: String) {
        if let Some(v) = self.find_view_mut(id) {
            v.name = name;
        }
    }

    /// Append cells aliasing the given `(conn, pane_id)` pairs to view `id`,
    /// each assigned a fresh stable [`CellId`]. When a `custom_tree` is active,
    /// each new cell is spliced into it by splitting the focused (else last)
    /// leaf — mirroring `ClientView::add_cell` so the manual arrangement
    /// survives the add. Fail-silent if the view is absent.
    pub fn view_add_cells(&mut self, id: ViewId, cells: Vec<(ConnDescriptor, PaneId)>) {
        if let Some(v) = self.find_view_mut(id) {
            for (conn, pane_id) in cells {
                let cell_id = v.next_cell_id;
                v.next_cell_id += 1;
                // Choose the split target BEFORE pushing, so `focused` still
                // indexes an existing cell (mirrors the client).
                let target = v
                    .cells
                    .get(v.focused)
                    .or_else(|| v.cells.last())
                    .map(|c| c.id);
                v.cells.push(ServerViewCell {
                    id: cell_id,
                    conn,
                    pane_id,
                });
                if let (Some(tree), Some(target_id)) = (v.custom_tree.as_mut(), target) {
                    tree.split_vertical(target_id, cell_id);
                }
            }
        }
    }

    /// Remove cell `cell_id` from view `id`, pruning it from any custom tree and
    /// re-clamping focus. Fail-silent on unknown view/cell.
    pub fn view_remove_cell(&mut self, id: ViewId, cell_id: CellId) {
        if let Some(v) = self.find_view_mut(id) {
            if let Some(idx) = v.index_of_id(cell_id) {
                v.cells.remove(idx);
                if let Some(tree) = v.custom_tree.as_mut() {
                    if tree.close_pane(cell_id).is_none() {
                        v.custom_tree = None;
                    }
                }
                v.clamp_focus();
            }
        }
    }

    /// Focus cell `cell_id` within view `id`. Fail-silent on unknown view/cell.
    pub fn view_set_focus(&mut self, id: ViewId, cell_id: CellId) {
        if let Some(v) = self.find_view_mut(id) {
            if let Some(idx) = v.index_of_id(cell_id) {
                v.focused = idx;
            }
        }
    }

    /// Cycle view `id` to the next automatic layout, dropping any custom tree.
    pub fn view_cycle_layout(&mut self, id: ViewId) {
        if let Some(v) = self.find_view_mut(id) {
            v.layout = v.layout.next();
            v.custom_tree = None;
        }
    }

    /// Toggle focus-cell zoom for view `id`.
    pub fn view_toggle_zoom(&mut self, id: ViewId) {
        if let Some(v) = self.find_view_mut(id) {
            v.zoomed = !v.zoomed;
        }
    }

    /// Resize cell `cell_id` in view `id` by `amount` percent toward `dir`.
    /// Mirrors the client's convention verbatim: Left/Right adjust a Vertical
    /// split, Up/Down a Horizontal one; a fresh custom tree is seeded on first
    /// resize and reverted if the resize changes nothing. Area-independent
    /// (`LayoutNode::resize` walks ratios, no geometry). Fail-silent otherwise.
    pub fn view_resize_cell(
        &mut self,
        id: ViewId,
        cell_id: CellId,
        dir: FocusDirection,
        amount: u16,
    ) {
        let v = match self.find_view_mut(id) {
            Some(v) => v,
            None => return,
        };
        if v.cells.is_empty() || v.index_of_id(cell_id).is_none() {
            return;
        }
        let (direction, delta) = match dir {
            FocusDirection::Left => (Direction::Vertical, -(amount as f32) / 100.0),
            FocusDirection::Right => (Direction::Vertical, amount as f32 / 100.0),
            FocusDirection::Up => (Direction::Horizontal, -(amount as f32) / 100.0),
            FocusDirection::Down => (Direction::Horizontal, amount as f32 / 100.0),
        };
        let had_tree = v.custom_tree.is_some();
        if !had_tree {
            v.custom_tree = Some(v.auto_tree());
        }
        let changed = v
            .custom_tree
            .as_mut()
            .map(|t| t.resize(cell_id, direction, delta))
            .unwrap_or(false);
        if !changed && !had_tree {
            v.custom_tree = None;
        }
    }

    /// Move cell `cell_id` in view `id` toward `dir`: swap with the spatial
    /// neighbor in `dir` if one exists, else relocate to that edge. Mirrors the
    /// client's `PaneMove` semantics. `area` is the reference geometry the
    /// neighbor search runs in.
    ///
    /// Phase-2 TODO: `area` is supplied by the daemon from the min across
    /// currently-connected clients, so the same `ViewMoveCell` can pick a
    /// different neighbor depending on who is connected. A shared view needs its
    /// own canonical area; wire that in when the client interaction is rebuilt.
    pub fn view_move_cell(&mut self, id: ViewId, cell_id: CellId, dir: FocusDirection, area: Rect) {
        let v = match self.find_view_mut(id) {
            Some(v) => v,
            None => return,
        };
        if v.cells.is_empty() || v.index_of_id(cell_id).is_none() {
            return;
        }
        let had_tree = v.custom_tree.is_some();
        if !had_tree {
            v.custom_tree = Some(v.auto_tree());
        }
        let moved = {
            let tree = v.custom_tree.as_ref().unwrap();
            if let Some(neighbor) = find_neighbor(tree, area, cell_id, dir.clone(), 0) {
                swap_panes(v.custom_tree.as_mut().unwrap(), cell_id, neighbor)
            } else if let Some(new_tree) = relocate_pane_to_edge(tree, cell_id, dir) {
                v.custom_tree = Some(new_tree);
                true
            } else {
                false
            }
        };
        if !moved && !had_tree {
            v.custom_tree = None;
        }
    }

    /// Allocate the next pane ID (monotonically increasing).
    pub fn next_pane_id(&mut self) -> PaneId {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        id
    }

    /// Allocate the next tab ID (monotonically increasing).
    fn next_tab_id(&mut self) -> TabId {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    /// Ensure the pane and tab ID counters are higher than any existing ID.
    ///
    /// This is used after restoring persisted state to guard against
    /// corruption where the counters might be lower than the max used ID.
    pub fn ensure_id_counters(&mut self) {
        let max_pane = self
            .sessions
            .values()
            .flat_map(|s| s.tabs.iter())
            .flat_map(|t| layout::all_pane_ids(&t.layout))
            .max()
            .unwrap_or(0);
        let max_tab = self
            .sessions
            .values()
            .flat_map(|s| s.tabs.iter())
            .map(|t| t.id)
            .max()
            .unwrap_or(0);
        if self.next_pane_id <= max_pane {
            self.next_pane_id = max_pane + 1;
        }
        if self.next_tab_id <= max_tab {
            self.next_tab_id = max_tab + 1;
        }
    }

    /// Raise this state's pane/tab id counters so future allocations never
    /// collide with any id used by `other`.
    ///
    /// Used when a dormant snapshot is loaded alongside a fresh live state
    /// (`automatic_restore = false`): sessions created before a resurrect must
    /// allocate ids above the *entire* dormant id range, otherwise a
    /// resurrected pane/tab id would clash with a live one in the global pane
    /// map. Reserves above both `other`'s used ids and its own next counters.
    pub fn reserve_ids_above(&mut self, other: &ServerState) {
        let other_max_pane = other
            .sessions
            .values()
            .flat_map(|s| s.tabs.iter())
            .flat_map(|t| layout::all_pane_ids(&t.layout))
            .max()
            .unwrap_or(0)
            .max(other.next_pane_id.saturating_sub(1));
        let other_max_tab = other
            .sessions
            .values()
            .flat_map(|s| s.tabs.iter())
            .map(|t| t.id)
            .max()
            .unwrap_or(0)
            .max(other.next_tab_id.saturating_sub(1));
        if self.next_pane_id <= other_max_pane {
            self.next_pane_id = other_max_pane + 1;
        }
        if self.next_tab_id <= other_max_tab {
            self.next_tab_id = other_max_tab + 1;
        }
    }

    // -----------------------------------------------------------------------
    // Session CRUD
    // -----------------------------------------------------------------------

    /// Create a new session, optionally in a folder.
    ///
    /// The session starts with one tab containing one pane. If a folder is
    /// specified and does not exist, it is created automatically.
    ///
    /// Returns the initial pane ID.
    pub fn create_session(
        &mut self,
        name: &str,
        folder: Option<&str>,
        border_style: BorderStyle,
        layout_mode: LayoutMode,
        popup_size: (u8, u8),
    ) -> Result<PaneId> {
        log::debug!(
            "session: create_session name={:?}, folder={:?}",
            name,
            folder
        );

        if self.sessions.contains_key(name) {
            bail!("session '{}' already exists", name);
        }

        let pane_id = self.next_pane_id();
        let tab_id = self.next_tab_id();

        let folder_id = if let Some(folder_name) = folder {
            // Create folder if it doesn't exist.
            if !self.folders.contains_key(folder_name) {
                self.folders.insert(
                    folder_name.to_string(),
                    Folder {
                        name: folder_name.to_string(),
                        session_ids: Vec::new(),
                    },
                );
            }
            let f = self
                .folders
                .get_mut(folder_name)
                .expect("folder was just created or already exists");
            if !f.session_ids.contains(&name.to_string()) {
                f.session_ids.push(name.to_string());
            }
            Some(folder_name.to_string())
        } else {
            None
        };

        let tab = Tab {
            id: tab_id,
            name: "Tab 1".to_string(),
            layout: LayoutNode::new_stack(pane_id),
            focused_pane: pane_id,
            layout_mode,
            pane_order: vec![pane_id],
            zoomed_pane: None,
            saved_custom_layout: None,
            activity: TabActivity::None,
            last_output: None,
        };

        let session = Session {
            name: name.to_string(),
            folder: folder_id,
            tabs: vec![tab],
            active_tab: 0,
            border_style,
            rename_state: None,
            popup_pane: None,
            popup_visible: false,
            popup_size: layout::clamp_popup_size(popup_size),
        };

        self.sessions.insert(name.to_string(), session);
        Ok(pane_id)
    }

    /// Rename a session. The new name must be unique.
    pub fn rename_session(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        log::debug!(
            "session: rename_session old={:?}, new={:?}",
            old_name,
            new_name
        );
        if old_name == new_name {
            return Ok(());
        }
        if self.sessions.contains_key(new_name) {
            bail!("session '{}' already exists", new_name);
        }
        let mut session = self
            .sessions
            .remove(old_name)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", old_name))?;

        // Update folder reference.
        if let Some(ref folder_id) = session.folder {
            if let Some(folder) = self.folders.get_mut(folder_id) {
                if let Some(pos) = folder.session_ids.iter().position(|s| s == old_name) {
                    folder.session_ids[pos] = new_name.to_string();
                }
            }
        }

        session.name = new_name.to_string();
        self.sessions.insert(new_name.to_string(), session);
        Ok(())
    }

    /// Delete a session. Returns all pane IDs that need cleanup (e.g., PTY
    /// teardown).
    pub fn delete_session(&mut self, name: &str) -> Result<Vec<PaneId>> {
        log::debug!("session: delete_session name={:?}", name);
        let session = self
            .sessions
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", name))?;

        // Remove from folder.
        if let Some(ref folder_id) = session.folder {
            if let Some(folder) = self.folders.get_mut(folder_id) {
                folder.session_ids.retain(|s| s != name);
            }
        }

        // Collect all pane IDs across all tabs, plus the popup pane: it lives
        // outside every layout tree, so it would otherwise leak its PTY.
        let mut pane_ids = Vec::new();
        for tab in &session.tabs {
            pane_ids.extend(layout::all_pane_ids(&tab.layout));
        }
        pane_ids.extend(session.popup_pane);

        Ok(pane_ids)
    }

    /// List all sessions with summary information.
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self
            .sessions
            .values()
            .map(|s| {
                let pane_count: usize = s
                    .tabs
                    .iter()
                    .map(|t| layout::all_pane_ids(&t.layout).len())
                    .sum();
                SessionInfo {
                    name: s.name.clone(),
                    folder: s.folder.clone(),
                    tab_count: s.tabs.len(),
                    pane_count,
                }
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    // -----------------------------------------------------------------------
    // Folder CRUD
    // -----------------------------------------------------------------------

    /// Create a new folder.
    pub fn create_folder(&mut self, name: &str) -> Result<()> {
        log::debug!("session: create_folder name={:?}", name);
        if self.folders.contains_key(name) {
            bail!("folder '{}' already exists", name);
        }
        self.folders.insert(
            name.to_string(),
            Folder {
                name: name.to_string(),
                session_ids: Vec::new(),
            },
        );
        Ok(())
    }

    /// Rename a folder.
    pub fn rename_folder(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        log::debug!(
            "session: rename_folder old={:?}, new={:?}",
            old_name,
            new_name
        );
        if old_name == new_name {
            return Ok(());
        }
        if self.folders.contains_key(new_name) {
            bail!("folder '{}' already exists", new_name);
        }
        let mut folder = self
            .folders
            .remove(old_name)
            .ok_or_else(|| anyhow::anyhow!("folder '{}' not found", old_name))?;

        // Update all sessions that reference this folder.
        for session_id in &folder.session_ids {
            if let Some(session) = self.sessions.get_mut(session_id) {
                session.folder = Some(new_name.to_string());
            }
        }

        folder.name = new_name.to_string();
        self.folders.insert(new_name.to_string(), folder);
        Ok(())
    }

    /// Delete a folder. The folder must be empty (no sessions).
    pub fn delete_folder(&mut self, name: &str) -> Result<()> {
        log::debug!("session: delete_folder name={:?}", name);
        let folder = self
            .folders
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("folder '{}' not found", name))?;

        if !folder.session_ids.is_empty() {
            bail!(
                "folder '{}' is not empty (contains {} sessions)",
                name,
                folder.session_ids.len()
            );
        }

        self.folders.remove(name);
        Ok(())
    }

    /// Delete a folder and all sessions it contains (cascade).
    ///
    /// Returns a list of `(session_name, pane_ids)` for each deleted session
    /// so callers can clean up PTYs and notify clients.
    pub fn delete_folder_cascade(&mut self, name: &str) -> Result<Vec<(String, Vec<PaneId>)>> {
        log::debug!("session: delete_folder_cascade name={:?}", name);
        let folder = self
            .folders
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("folder '{}' not found", name))?;

        let mut deleted_sessions = Vec::new();
        for session_id in &folder.session_ids {
            if let Some(session) = self.sessions.remove(session_id) {
                let mut pane_ids = Vec::new();
                for tab in &session.tabs {
                    pane_ids.extend(layout::all_pane_ids(&tab.layout));
                }
                deleted_sessions.push((session_id.clone(), pane_ids));
            }
        }

        Ok(deleted_sessions)
    }

    /// List all folders with summary information.
    pub fn list_folders(&self) -> Vec<FolderInfo> {
        let mut infos: Vec<FolderInfo> = self
            .folders
            .values()
            .map(|f| FolderInfo {
                name: f.name.clone(),
                session_count: f.session_ids.len(),
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    // -----------------------------------------------------------------------
    // Tab CRUD
    // -----------------------------------------------------------------------

    /// Create a new tab in the given session. Returns the initial pane ID.
    pub fn create_tab(
        &mut self,
        session: &str,
        name: &str,
        layout_mode: LayoutMode,
    ) -> Result<PaneId> {
        log::debug!("session: create_tab name={:?}, session={:?}", name, session);
        let pane_id = self.next_pane_id();
        let tab_id = self.next_tab_id();

        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        let tab = Tab {
            id: tab_id,
            name: name.to_string(),
            layout: LayoutNode::new_stack(pane_id),
            focused_pane: pane_id,
            layout_mode,
            pane_order: vec![pane_id],
            zoomed_pane: None,
            saved_custom_layout: None,
            activity: TabActivity::None,
            last_output: None,
        };

        sess.tabs.push(tab);
        sess.active_tab = sess.tabs.len() - 1;
        Ok(pane_id)
    }

    /// Close a tab by index. Returns the pane IDs that need cleanup and
    /// whether the session was deleted (if it was the last tab).
    pub fn close_tab(&mut self, session: &str, tab_idx: usize) -> Result<(Vec<PaneId>, bool)> {
        log::debug!(
            "session: close_tab index={}, session={:?}",
            tab_idx,
            session
        );
        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        if tab_idx >= sess.tabs.len() {
            bail!(
                "tab index {} out of range (session has {} tabs)",
                tab_idx,
                sess.tabs.len()
            );
        }

        let tab = sess.tabs.remove(tab_idx);
        let mut pane_ids = layout::all_pane_ids(&tab.layout);

        if sess.tabs.is_empty() {
            // Last tab -- delete the session.
            // We need to remove the session from its folder too.
            let session_name = session.to_string();
            let folder_id = sess.folder.clone();
            // The popup lives outside every layout tree, so the session going
            // away is the only thing that reclaims its PTY.
            pane_ids.extend(sess.take_popup());

            self.sessions.remove(&session_name);

            if let Some(ref fid) = folder_id {
                if let Some(folder) = self.folders.get_mut(fid) {
                    folder.session_ids.retain(|s| s != &session_name);
                }
            }

            return Ok((pane_ids, true));
        }

        // Adjust active_tab if needed.
        if sess.active_tab >= sess.tabs.len() {
            sess.active_tab = sess.tabs.len() - 1;
        } else if sess.active_tab > tab_idx {
            sess.active_tab -= 1;
        }

        // Closing the active tab moves focus to a different existing tab that
        // may carry stale background activity; clear it (harmless no-op when
        // the surviving active tab was already clean).
        if let Some(tab) = sess.tabs.get_mut(sess.active_tab) {
            tab.activity = TabActivity::None;
            tab.last_output = None;
        }

        Ok((pane_ids, false))
    }

    /// Rename a tab by index.
    pub fn rename_tab(&mut self, session: &str, tab_idx: usize, new_name: &str) -> Result<()> {
        log::debug!(
            "session: rename_tab index={}, new_name={:?}, session={:?}",
            tab_idx,
            new_name,
            session
        );
        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        let tab = sess
            .tabs
            .get_mut(tab_idx)
            .ok_or_else(|| anyhow::anyhow!("tab index {} out of range", tab_idx))?;

        tab.name = new_name.to_string();
        Ok(())
    }

    /// Move a tab (identified by `tab_idx`) left/right by `delta` positions
    /// within its session's tab vector.
    ///
    /// The destination index is clamped to `[0, len - 1]`, so out-of-range
    /// deltas saturate at the ends rather than erroring. `active_tab` is
    /// preserved to keep pointing at the *same* tab it did before the move
    /// (tracked by tab id), regardless of whether the moved tab was the active
    /// one. A no-op move (destination == source) returns `Ok(())` unchanged.
    pub fn move_tab(&mut self, session: &str, tab_idx: usize, delta: i32) -> Result<()> {
        log::debug!(
            "session: move_tab index={}, delta={}, session={:?}",
            tab_idx,
            delta,
            session
        );
        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        let len = sess.tabs.len();
        if tab_idx >= len {
            bail!(
                "tab index {} out of range (session has {} tabs)",
                tab_idx,
                len
            );
        }

        // Clamp the destination to the valid range so large deltas saturate.
        let dest = (tab_idx as i32 + delta).clamp(0, len as i32 - 1) as usize;
        if dest == tab_idx {
            return Ok(());
        }

        // Remember which tab is active by identity so we can restore the index
        // after the reorder (the active tab may or may not be the moved one).
        let active_id = sess.tabs.get(sess.active_tab).map(|t| t.id);

        let tab = sess.tabs.remove(tab_idx);
        sess.tabs.insert(dest, tab);

        if let Some(active_id) = active_id {
            if let Some(pos) = sess.tabs.iter().position(|t| t.id == active_id) {
                sess.active_tab = pos;
            }
        }

        Ok(())
    }

    /// Navigate to a tab by index.
    pub fn goto_tab(&mut self, session: &str, tab_idx: usize) -> Result<()> {
        log::debug!("session: goto_tab index={}, session={:?}", tab_idx, session);
        let sess = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

        if tab_idx >= sess.tabs.len() {
            bail!(
                "tab index {} out of range (session has {} tabs)",
                tab_idx,
                sess.tabs.len()
            );
        }

        sess.active_tab = tab_idx;
        // Clear activity for the newly-focused tab: it is now being viewed.
        if let Some(tab) = sess.tabs.get_mut(tab_idx) {
            tab.activity = TabActivity::None;
            tab.last_output = None;
        }
        Ok(())
    }

    /// Record background output for the tab that owns `pane_id`.
    ///
    /// If the owning tab is its session's `active_tab` (foreground / being
    /// viewed), this is a no-op — the foreground tab never accrues activity.
    /// Otherwise the tab's state is updated: `Bell` if `bell` is set (Bell wins
    /// and is never downgraded to `Activity`), else `Activity`. `last_output`
    /// is refreshed to `now` so the silence timer restarts on every new byte.
    pub fn record_pane_activity(&mut self, pane_id: PaneId, bell: bool, now: Instant) {
        for sess in self.sessions.values_mut() {
            let active = sess.active_tab;
            for (idx, tab) in sess.tabs.iter_mut().enumerate() {
                if layout::all_pane_ids(&tab.layout).contains(&pane_id) {
                    if idx == active {
                        // Foreground tab: being viewed, never accrues activity.
                        return;
                    }
                    if bell {
                        tab.activity = TabActivity::Bell;
                    } else if tab.activity != TabActivity::Bell {
                        // Don't downgrade a pending Bell to Activity. New output
                        // also revives a Silent tab back to Activity.
                        tab.activity = TabActivity::Activity;
                    }
                    tab.last_output = Some(now);
                    return;
                }
            }
        }
    }

    /// Promote any background tab that has been quietly in `Activity` past the
    /// `threshold` to `Silent` ("finished"). Returns the names of sessions that
    /// had at least one tab change, so the caller can re-render only those.
    ///
    /// `Bell` tabs are left untouched. `now` is injected for deterministic
    /// testing (see [`should_promote_to_silent`]).
    pub fn promote_silent_tabs(&mut self, now: Instant, threshold: Duration) -> Vec<String> {
        let mut affected = Vec::new();
        for (name, sess) in self.sessions.iter_mut() {
            let mut changed = false;
            for tab in sess.tabs.iter_mut() {
                if should_promote_to_silent(tab.activity, tab.last_output, now, threshold) {
                    tab.activity = TabActivity::Silent;
                    changed = true;
                }
            }
            if changed {
                affected.push(name.clone());
            }
        }
        affected
    }

    /// Clear activity on a session's currently-active tab. Used on attach/focus
    /// so a freshly-viewed tab never shows a stale marker.
    pub fn clear_active_tab_activity(&mut self, session: &str) {
        if let Some(sess) = self.sessions.get_mut(session) {
            let active = sess.active_tab;
            if let Some(tab) = sess.tabs.get_mut(active) {
                tab.activity = TabActivity::None;
                tab.last_output = None;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Session movement
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Session tree (for session manager)
    // -----------------------------------------------------------------------

    /// Build the full session tree hierarchy.
    ///
    /// Returns `(folders, unfiled)` where `folders` contains sessions grouped
    /// by folder, and `unfiled` contains sessions not in any folder.
    ///
    /// `current_session` marks which session the requesting client is attached
    /// to. `client_counts` maps session name to the number of clients attached.
    /// `pane_names` maps pane IDs to display names (e.g. process name).
    pub fn build_session_tree(
        &self,
        current_session: Option<&str>,
        client_counts: &HashMap<String, usize>,
        pane_names: &HashMap<PaneId, String>,
    ) -> (Vec<FolderTreeEntry>, Vec<SessionTreeEntry>) {
        let build_entry = |session: &Session| -> SessionTreeEntry {
            let tabs = session
                .tabs
                .iter()
                .map(|tab| {
                    let panes = layout::all_pane_ids(&tab.layout)
                        .into_iter()
                        .map(|pid| PaneTreeEntry {
                            id: pid,
                            name: pane_names
                                .get(&pid)
                                .cloned()
                                .unwrap_or_else(|| format!("pane-{}", pid)),
                            is_focused: pid == tab.focused_pane,
                        })
                        .collect();
                    TabTreeEntry {
                        id: tab.id,
                        name: tab.name.clone(),
                        panes,
                    }
                })
                .collect();
            SessionTreeEntry {
                name: session.name.clone(),
                tabs,
                client_count: client_counts.get(&session.name).copied().unwrap_or(0),
                is_current: current_session == Some(&session.name),
            }
        };

        let mut folders = Vec::new();
        for folder in self.folders.values() {
            let mut sessions = Vec::new();
            let mut seen = HashSet::new();
            for session_id in &folder.session_ids {
                if !seen.insert(session_id.clone()) {
                    continue; // skip duplicates
                }
                if let Some(session) = self.sessions.get(session_id) {
                    sessions.push(build_entry(session));
                }
            }
            sessions.sort_by(|a, b| a.name.cmp(&b.name));
            folders.push(FolderTreeEntry {
                name: folder.name.clone(),
                sessions,
            });
        }
        folders.sort_by(|a, b| a.name.cmp(&b.name));

        let mut unfiled = Vec::new();
        for session in self.sessions.values() {
            if session.folder.is_none() {
                unfiled.push(build_entry(session));
            }
        }
        unfiled.sort_by(|a, b| a.name.cmp(&b.name));

        (folders, unfiled)
    }

    // -----------------------------------------------------------------------
    // Session movement
    // -----------------------------------------------------------------------

    /// Move a session to a different folder (or to top-level if `None`).
    pub fn move_session(&mut self, session_name: &str, target_folder: Option<&str>) -> Result<()> {
        log::debug!(
            "session: move_session name={:?}, target_folder={:?}",
            session_name,
            target_folder
        );
        let sess = self
            .sessions
            .get_mut(session_name)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session_name))?;

        let old_folder = sess.folder.clone();

        // Remove from old folder.
        if let Some(ref old_fid) = old_folder {
            if let Some(folder) = self.folders.get_mut(old_fid) {
                folder.session_ids.retain(|s| s != session_name);
            }
        }

        // Add to new folder.
        match target_folder {
            Some(folder_name) => {
                // Create folder if it doesn't exist.
                if !self.folders.contains_key(folder_name) {
                    self.folders.insert(
                        folder_name.to_string(),
                        Folder {
                            name: folder_name.to_string(),
                            session_ids: Vec::new(),
                        },
                    );
                }
                let folder = self
                    .folders
                    .get_mut(folder_name)
                    .expect("folder was just created or already exists");
                if !folder.session_ids.contains(&session_name.to_string()) {
                    folder.session_ids.push(session_name.to_string());
                }

                // Re-borrow session mutably.
                let sess = self.sessions.get_mut(session_name).expect("session exists");
                sess.folder = Some(folder_name.to_string());
            }
            None => {
                let sess = self.sessions.get_mut(session_name).expect("session exists");
                sess.folder = None;
            }
        }

        Ok(())
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_server_state() {
        let state = ServerState::new();
        assert!(state.sessions.is_empty());
        assert!(state.folders.is_empty());
    }

    // -- Shared view registry (Phase 2: logic moved here from the client) -----

    #[test]
    fn view_add_cells_assigns_ids_and_splices_custom_tree() {
        use super::super::layout::all_pane_ids;
        let mut st = ServerState::new();
        let id = st.view_create("V".into());
        // Two cells, then seed a custom tree, then add a third: the new cell's
        // stable id is spliced into the manual arrangement (mirrors the old
        // client `add_cell` test that moved to the server).
        st.view_add_cells(
            id,
            vec![(ConnDescriptor::Local, 1), (ConnDescriptor::Local, 2)],
        );
        {
            let v = st.find_view_mut(id).unwrap();
            v.custom_tree = Some(v.auto_tree());
        }
        let before = all_pane_ids(st.views[0].custom_tree.as_ref().unwrap()).len();
        st.view_add_cells(id, vec![(ConnDescriptor::Local, 3)]);
        let v = &st.views[0];
        assert_eq!(v.cells.len(), 3);
        let new_id = v.cells[2].id;
        let ids = all_pane_ids(v.custom_tree.as_ref().unwrap());
        assert_eq!(ids.len(), before + 1);
        assert!(
            ids.contains(&new_id),
            "new cell id spliced into custom tree"
        );
    }

    #[test]
    fn view_remove_cell_prunes_tree_and_clamps_focus() {
        use super::super::layout::all_pane_ids;
        let mut st = ServerState::new();
        let id = st.view_create("V".into());
        st.view_add_cells(
            id,
            vec![(ConnDescriptor::Local, 1), (ConnDescriptor::Local, 2)],
        );
        let (c0, c1) = (st.views[0].cells[0].id, st.views[0].cells[1].id);
        {
            let v = st.find_view_mut(id).unwrap();
            v.custom_tree = Some(v.auto_tree());
            v.focused = 1;
        }
        st.view_remove_cell(id, c0);
        let v = &st.views[0];
        assert_eq!(v.cells.len(), 1);
        let ids = all_pane_ids(v.custom_tree.as_ref().unwrap());
        assert!(!ids.contains(&c0) && ids.contains(&c1));
        assert_eq!(v.focused, 0, "focus clamped after removal");
        // Removing the last cell clears the custom tree back to automatic.
        st.view_remove_cell(id, c1);
        assert!(st.views[0].cells.is_empty());
        assert!(st.views[0].custom_tree.is_none());
    }

    #[test]
    fn view_cycle_layout_and_zoom_toggle() {
        let mut st = ServerState::new();
        let id = st.view_create("V".into());
        st.view_add_cells(
            id,
            vec![(ConnDescriptor::Local, 1), (ConnDescriptor::Local, 2)],
        );
        assert_eq!(st.views[0].layout_name(), "grid");
        {
            let v = st.find_view_mut(id).unwrap();
            v.custom_tree = Some(v.auto_tree());
        }
        assert_eq!(st.views[0].layout_name(), "custom");
        st.view_cycle_layout(id);
        assert!(
            st.views[0].custom_tree.is_none(),
            "cycling layout drops the custom tree"
        );
        assert_ne!(st.views[0].layout_name(), "custom");
        assert!(!st.views[0].zoomed);
        st.view_toggle_zoom(id);
        assert!(st.views[0].zoomed);
        st.view_toggle_zoom(id);
        assert!(!st.views[0].zoomed);
    }

    #[test]
    fn view_set_focus_and_delete_are_id_safe() {
        let mut st = ServerState::new();
        let id = st.view_create("V".into());
        st.view_add_cells(
            id,
            vec![(ConnDescriptor::Local, 1), (ConnDescriptor::Local, 2)],
        );
        let c1 = st.views[0].cells[1].id;
        st.view_set_focus(id, c1);
        assert_eq!(st.views[0].focused, 1);
        // Unknown view / cell ids are fail-silent (no panic).
        st.view_set_focus(999, c1);
        st.view_remove_cell(id, 999);
        st.view_delete(999);
        assert_eq!(st.views.len(), 1);
        st.view_delete(id);
        assert!(st.views.is_empty());
    }

    #[test]
    fn test_next_pane_id() {
        let mut state = ServerState::new();
        assert_eq!(state.next_pane_id(), 1);
        assert_eq!(state.next_pane_id(), 2);
        assert_eq!(state.next_pane_id(), 3);
    }

    #[test]
    fn test_reserve_ids_above_prevents_collision() {
        // A dormant snapshot that used pane ids 1..=3.
        let mut dormant = ServerState::new();
        dormant
            .create_session(
                "alpha",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        dormant
            .create_session(
                "beta",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();

        // A fresh live state would otherwise start allocating at pane id 1,
        // colliding with the dormant snapshot.
        let mut live = ServerState::new();
        live.reserve_ids_above(&dormant);

        // The next live pane id must exceed every id the dormant snapshot used.
        let next = live.next_pane_id();
        assert!(
            next >= dormant.next_pane_id,
            "live next_pane_id {next} should be >= dormant next {}",
            dormant.next_pane_id
        );
        assert!(next > 2);
    }

    /// Build a session named `s` with `n` tabs and return the ordered list of
    /// their tab ids. `create_tab` leaves `active_tab` pointing at the last tab.
    fn state_with_tabs(n: usize) -> (ServerState, Vec<TabId>) {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        for i in 1..n {
            state
                .create_tab("s", &format!("Tab {}", i + 1), LayoutMode::default())
                .unwrap();
        }
        let ids: Vec<TabId> = state.sessions["s"].tabs.iter().map(|t| t.id).collect();
        assert_eq!(ids.len(), n);
        (state, ids)
    }

    fn tab_order(state: &ServerState) -> Vec<TabId> {
        state.sessions["s"].tabs.iter().map(|t| t.id).collect()
    }

    #[test]
    fn test_move_tab_clamps_high_delta() {
        let (mut state, ids) = state_with_tabs(4);
        // Move the first tab far right; destination saturates at the last slot.
        state.move_tab("s", 0, 100).unwrap();
        assert_eq!(tab_order(&state), vec![ids[1], ids[2], ids[3], ids[0]]);
    }

    #[test]
    fn test_move_tab_clamps_low_delta() {
        let (mut state, ids) = state_with_tabs(4);
        // Move the last tab far left; destination saturates at the first slot.
        state.move_tab("s", 3, -100).unwrap();
        assert_eq!(tab_order(&state), vec![ids[3], ids[0], ids[1], ids[2]]);
    }

    #[test]
    fn test_move_tab_preserves_active_by_identity() {
        let (mut state, ids) = state_with_tabs(4);
        // Make tab index 1 the active one.
        state.sessions.get_mut("s").unwrap().active_tab = 1;
        // Move a *different* (non-active) tab across it: index 3 -> front.
        state.move_tab("s", 3, -3).unwrap();
        assert_eq!(tab_order(&state), vec![ids[3], ids[0], ids[1], ids[2]]);
        // active_tab must still point at the same tab (ids[1]), now at index 2.
        assert_eq!(state.sessions["s"].active_tab, 2);
        assert_eq!(state.sessions["s"].tabs[2].id, ids[1]);
    }

    #[test]
    fn test_move_tab_noop_leaves_state_unchanged() {
        let (mut state, ids) = state_with_tabs(4);
        state.sessions.get_mut("s").unwrap().active_tab = 2;
        // delta 0 is a no-op.
        state.move_tab("s", 1, 0).unwrap();
        assert_eq!(tab_order(&state), ids);
        assert_eq!(state.sessions["s"].active_tab, 2);
    }

    #[test]
    fn test_move_tab_out_of_range_index_errors() {
        let (mut state, _ids) = state_with_tabs(3);
        assert!(state.move_tab("s", 10, 1).is_err());
    }

    #[test]
    fn test_move_tab_missing_session_errors() {
        let mut state = ServerState::new();
        assert!(state.move_tab("nope", 0, 1).is_err());
    }

    #[test]
    fn test_create_session() {
        let mut state = ServerState::new();
        let pane_id = state
            .create_session(
                "test",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        assert_eq!(pane_id, 1);

        let sess = state.sessions.get("test").unwrap();
        assert_eq!(sess.name, "test");
        assert!(sess.folder.is_none());
        assert_eq!(sess.tabs.len(), 1);
        assert_eq!(sess.active_tab, 0);
    }

    #[test]
    fn test_create_session_with_folder() {
        let mut state = ServerState::new();
        state
            .create_session(
                "test",
                Some("work"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();

        assert!(state.folders.contains_key("work"));
        let folder = state.folders.get("work").unwrap();
        assert_eq!(folder.session_ids, vec!["test"]);

        let sess = state.sessions.get("test").unwrap();
        assert_eq!(sess.folder, Some("work".to_string()));
    }

    #[test]
    fn test_create_session_duplicate_name() {
        let mut state = ServerState::new();
        state
            .create_session(
                "test",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        let result = state.create_session(
            "test",
            None,
            BorderStyle::ZellijStyle,
            LayoutMode::default(),
            (80, 80),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_rename_session() {
        let mut state = ServerState::new();
        state
            .create_session(
                "old",
                Some("folder"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state.rename_session("old", "new").unwrap();

        assert!(!state.sessions.contains_key("old"));
        assert!(state.sessions.contains_key("new"));

        let folder = state.folders.get("folder").unwrap();
        assert!(folder.session_ids.contains(&"new".to_string()));
        assert!(!folder.session_ids.contains(&"old".to_string()));
    }

    #[test]
    fn test_rename_session_duplicate() {
        let mut state = ServerState::new();
        state
            .create_session(
                "a",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_session(
                "b",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();

        let result = state.rename_session("a", "b");
        assert!(result.is_err());
    }

    #[test]
    fn test_rename_session_same_name() {
        let mut state = ServerState::new();
        state
            .create_session(
                "a",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state.rename_session("a", "a").unwrap();
    }

    #[test]
    fn test_delete_session() {
        let mut state = ServerState::new();
        state
            .create_session(
                "test",
                Some("folder"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        let pane_ids = state.delete_session("test").unwrap();

        assert_eq!(pane_ids, vec![1]);
        assert!(!state.sessions.contains_key("test"));

        let folder = state.folders.get("folder").unwrap();
        assert!(folder.session_ids.is_empty());
    }

    #[test]
    fn test_delete_session_not_found() {
        let mut state = ServerState::new();
        assert!(state.delete_session("nope").is_err());
    }

    #[test]
    fn test_list_sessions() {
        let mut state = ServerState::new();
        state
            .create_session(
                "b",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_session(
                "a",
                Some("f"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();

        let list = state.list_sessions();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "a");
        assert_eq!(list[0].folder, Some("f".to_string()));
        assert_eq!(list[1].name, "b");
        assert!(list[1].folder.is_none());
    }

    #[test]
    fn test_create_folder() {
        let mut state = ServerState::new();
        state.create_folder("work").unwrap();
        assert!(state.folders.contains_key("work"));
    }

    #[test]
    fn test_create_folder_duplicate() {
        let mut state = ServerState::new();
        state.create_folder("work").unwrap();
        assert!(state.create_folder("work").is_err());
    }

    #[test]
    fn test_rename_folder() {
        let mut state = ServerState::new();
        state.create_folder("old").unwrap();
        state
            .create_session(
                "s",
                Some("old"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state.rename_folder("old", "new").unwrap();

        assert!(!state.folders.contains_key("old"));
        assert!(state.folders.contains_key("new"));

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.folder, Some("new".to_string()));
    }

    #[test]
    fn test_delete_folder_empty() {
        let mut state = ServerState::new();
        state.create_folder("work").unwrap();
        state.delete_folder("work").unwrap();
        assert!(!state.folders.contains_key("work"));
    }

    #[test]
    fn test_delete_folder_not_empty() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                Some("work"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        assert!(state.delete_folder("work").is_err());
    }

    #[test]
    fn test_list_folders() {
        let mut state = ServerState::new();
        state.create_folder("b").unwrap();
        state.create_folder("a").unwrap();

        let list = state.list_folders();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "a");
        assert_eq!(list[1].name, "b");
    }

    #[test]
    fn test_create_tab() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        let pane_id = state
            .create_tab("s", "new-tab", LayoutMode::default())
            .unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs.len(), 2);
        assert_eq!(sess.active_tab, 1);
        assert_eq!(sess.tabs[1].name, "new-tab");
        assert_eq!(sess.tabs[1].focused_pane, pane_id);
    }

    #[test]
    fn test_close_tab() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();

        let (pane_ids, deleted) = state.close_tab("s", 0).unwrap();
        assert!(!deleted);
        assert_eq!(pane_ids.len(), 1);

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs.len(), 1);
        assert_eq!(sess.active_tab, 0);
    }

    #[test]
    fn test_close_last_tab_deletes_session() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                Some("f"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();

        let (pane_ids, deleted) = state.close_tab("s", 0).unwrap();
        assert!(deleted);
        assert_eq!(pane_ids.len(), 1);
        assert!(!state.sessions.contains_key("s"));

        // Session should be removed from folder too.
        let folder = state.folders.get("f").unwrap();
        assert!(folder.session_ids.is_empty());
    }

    #[test]
    fn test_rename_tab() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state.rename_tab("s", 0, "renamed").unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs[0].name, "renamed");
    }

    #[test]
    fn test_goto_tab() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        state.goto_tab("s", 0).unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.active_tab, 0);
    }

    #[test]
    fn test_record_activity_background_tab_only() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        // pane 1 is in tab 0 (initially active). Create tab 1 (now active) with
        // pane 2. So tab 0 is now a background tab holding pane 1.
        let pane2 = state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        let now = Instant::now();

        // Output on the background tab's pane => Activity.
        state.record_pane_activity(1, false, now);
        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs[0].activity, TabActivity::Activity);
        assert!(sess.tabs[0].last_output.is_some());
        // The active (foreground) tab never accrues activity.
        assert_eq!(sess.tabs[1].activity, TabActivity::None);

        // Output on the active tab's pane => still None.
        state.record_pane_activity(pane2, false, now);
        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs[1].activity, TabActivity::None);
    }

    #[test]
    fn test_record_activity_bell_wins_and_no_downgrade() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        let now = Instant::now();

        // Bell on background tab 0 => Bell.
        state.record_pane_activity(1, true, now);
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Bell
        );

        // Subsequent plain output must NOT downgrade Bell to Activity.
        state.record_pane_activity(1, false, now);
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Bell
        );
    }

    #[test]
    fn test_goto_tab_clears_activity() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        // Give background tab 0 some activity.
        state.record_pane_activity(1, true, Instant::now());
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Bell
        );

        // Switching to tab 0 clears its activity.
        state.goto_tab("s", 0).unwrap();
        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.tabs[0].activity, TabActivity::None);
        assert!(sess.tabs[0].last_output.is_none());
    }

    #[test]
    fn test_should_promote_to_silent_pure() {
        let base = Instant::now();
        let threshold = Duration::from_secs(3);
        let last = Some(base);

        // Activity older than threshold => promote.
        assert!(should_promote_to_silent(
            TabActivity::Activity,
            last,
            base + Duration::from_secs(4),
            threshold
        ));
        // Activity younger than threshold => no promote.
        assert!(!should_promote_to_silent(
            TabActivity::Activity,
            last,
            base + Duration::from_secs(1),
            threshold
        ));
        // Bell stays Bell regardless of age.
        assert!(!should_promote_to_silent(
            TabActivity::Bell,
            last,
            base + Duration::from_secs(10),
            threshold
        ));
        // None / Silent never promoted.
        assert!(!should_promote_to_silent(
            TabActivity::None,
            last,
            base + Duration::from_secs(10),
            threshold
        ));
        // No last_output => never promoted.
        assert!(!should_promote_to_silent(
            TabActivity::Activity,
            None,
            base + Duration::from_secs(10),
            threshold
        ));
    }

    #[test]
    fn test_promote_silent_tabs_transitions_activity() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();
        let base = Instant::now();
        // Background tab 0: Activity as of `base`.
        state.record_pane_activity(1, false, base);
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Activity
        );

        // Past the threshold, it promotes to Silent and reports the session.
        let affected =
            state.promote_silent_tabs(base + Duration::from_secs(4), Duration::from_secs(3));
        assert_eq!(affected, vec!["s".to_string()]);
        assert_eq!(
            state.sessions.get("s").unwrap().tabs[0].activity,
            TabActivity::Silent
        );

        // Running again is idempotent: nothing changes (empty affected list).
        let affected2 =
            state.promote_silent_tabs(base + Duration::from_secs(8), Duration::from_secs(3));
        assert!(affected2.is_empty());
    }

    #[test]
    fn test_goto_tab_out_of_range() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        assert!(state.goto_tab("s", 5).is_err());
    }

    #[test]
    fn test_move_session_to_folder() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state.move_session("s", Some("new-folder")).unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert_eq!(sess.folder, Some("new-folder".to_string()));

        let folder = state.folders.get("new-folder").unwrap();
        assert!(folder.session_ids.contains(&"s".to_string()));
    }

    #[test]
    fn test_move_session_between_folders() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                Some("old"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state.move_session("s", Some("new")).unwrap();

        let old_folder = state.folders.get("old").unwrap();
        assert!(old_folder.session_ids.is_empty());

        let new_folder = state.folders.get("new").unwrap();
        assert!(new_folder.session_ids.contains(&"s".to_string()));
    }

    #[test]
    fn test_move_session_to_top_level() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                Some("folder"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state.move_session("s", None).unwrap();

        let sess = state.sessions.get("s").unwrap();
        assert!(sess.folder.is_none());

        let folder = state.folders.get("folder").unwrap();
        assert!(folder.session_ids.is_empty());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s1",
                Some("work"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_tab("s1", "tab2", LayoutMode::default())
            .unwrap();
        state
            .create_session(
                "s2",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();

        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: ServerState = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.sessions.len(), 2);
        assert!(deserialized.sessions.contains_key("s1"));
        assert!(deserialized.sessions.contains_key("s2"));
        assert!(deserialized.folders.contains_key("work"));
    }

    #[test]
    fn test_build_session_tree_empty() {
        let state = ServerState::new();
        let counts = HashMap::new();
        let pane_names = HashMap::new();
        let (folders, unfiled) = state.build_session_tree(None, &counts, &pane_names);
        assert!(folders.is_empty());
        assert!(unfiled.is_empty());
    }

    #[test]
    fn test_build_session_tree_folders_and_unfiled() {
        let mut state = ServerState::new();
        state
            .create_session(
                "proj",
                Some("work"),
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_session(
                "scratch",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();

        let mut counts = HashMap::new();
        counts.insert("proj".to_string(), 2);
        let pane_names = HashMap::new();

        let (folders, unfiled) = state.build_session_tree(Some("proj"), &counts, &pane_names);
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "work");
        assert_eq!(folders[0].sessions.len(), 1);
        assert_eq!(folders[0].sessions[0].name, "proj");
        assert!(folders[0].sessions[0].is_current);
        assert_eq!(folders[0].sessions[0].client_count, 2);

        assert_eq!(unfiled.len(), 1);
        assert_eq!(unfiled[0].name, "scratch");
        assert!(!unfiled[0].is_current);
        assert_eq!(unfiled[0].client_count, 0);
    }

    #[test]
    fn test_build_session_tree_with_tabs_and_panes() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();
        state
            .create_tab("s", "tab2", LayoutMode::default())
            .unwrap();

        let counts = HashMap::new();
        let mut pane_names = HashMap::new();
        pane_names.insert(1, "zsh".to_string());
        pane_names.insert(2, "vim".to_string());

        let (_, unfiled) = state.build_session_tree(None, &counts, &pane_names);
        assert_eq!(unfiled.len(), 1);
        assert_eq!(unfiled[0].tabs.len(), 2);
        // First tab should have pane with name "zsh"
        assert_eq!(unfiled[0].tabs[0].panes[0].name, "zsh");
    }

    #[test]
    fn test_build_session_tree_custom_pane_name_wins() {
        let mut state = ServerState::new();
        state
            .create_session(
                "s",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::default(),
                (80, 80),
            )
            .unwrap();

        // Identify the first pane and give it a user-set custom name (as PaneRename does).
        let pane_id = {
            let sess = state.sessions.get_mut("s").unwrap();
            let tab = &mut sess.tabs[0];
            let pane_id = layout::all_pane_ids(&tab.layout)[0];
            assert!(layout::set_pane_custom_name(
                &mut tab.layout,
                pane_id,
                "XYZZY"
            ));
            pane_id
        };

        // Simulate the daemon: start from the auto-detected process name, then
        // apply the custom-name override (mirrors handle_list_session_tree).
        let counts = HashMap::new();
        let mut pane_names = HashMap::new();
        pane_names.insert(pane_id, "zsh".to_string());
        for sess in state.sessions.values() {
            for tab in &sess.tabs {
                for pid in layout::all_pane_ids(&tab.layout) {
                    if let Some(Some(custom)) = layout::get_pane_custom_name(&tab.layout, pid) {
                        pane_names.insert(pid, custom);
                    }
                }
            }
        }

        let (_, unfiled) = state.build_session_tree(None, &counts, &pane_names);
        assert_eq!(unfiled.len(), 1);
        // The custom name must win over the auto-detected process name.
        assert_eq!(unfiled[0].tabs[0].panes[0].name, "XYZZY");
    }
}

/// Tests for the **hard popup invariant**: the popup pane is a real PTY that
/// must never be spliced into a tab's `pane_order`, its layout tree, or its
/// `zoomed_pane` -- otherwise `PaneMove*`, `SetMaster`, an automatic rebuild or a
/// stack splice could capture it and it would start taking space in the layout.
///
/// Every case runs `assert_popup_invariant` after the mutation, and each
/// structural mutation is exercised BOTH with the popup visible and with it
/// hidden-but-existing (the pane exists either way, so both states can leak).
///
/// Also home to the wider structural invariant the popup rules are part of
/// (`check_structural_invariant`, which production code debug-asserts on) and to
/// the `Tab` helpers it constrains -- `panes`, `effective_layout`, `focus_pane`
/// and `reconcile_pane_order` -- since they share these fixtures.
#[cfg(test)]
mod popup_invariant_tests {
    use super::*;
    use crate::server::layout::{
        BspLayout, CustomLayout, Direction, GridLayout, MasterLayout, MonocleLayout,
    };

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 30,
    };

    /// A session with `pane_count` real panes in one tab, plus a popup pane whose
    /// id is allocated from the same counter (so an id-confusion bug would show
    /// up as a real collision rather than being masked by a magic id).
    fn state_with_popup(pane_count: usize, visible: bool) -> (ServerState, String, PaneId) {
        let mut st = ServerState::new();
        let first = st
            .create_session(
                "main",
                None,
                BorderStyle::ZellijStyle,
                LayoutMode::Custom(CustomLayout),
                (80, 80),
            )
            .expect("create_session");
        // Grow the tab to `pane_count` panes via real splits.
        let mut ids = vec![first];
        for _ in 1..pane_count {
            let new_id = st.next_pane_id();
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            let focused = tab.focused_pane;
            tab.layout.split_vertical(focused, new_id);
            tab.pane_order.push(new_id);
            tab.focused_pane = new_id;
            ids.push(new_id);
        }
        // Allocate the popup id AFTER the real panes, as the daemon does.
        let popup = st.next_pane_id();
        let sess = st.sessions.get_mut("main").expect("session");
        sess.popup_pane = Some(popup);
        sess.popup_visible = visible;
        assert_popup_invariant(sess, "fixture");
        (st, "main".to_string(), popup)
    }

    fn sess_of<'a>(st: &'a ServerState, name: &str) -> &'a Session {
        st.sessions.get(name).expect("session")
    }

    /// Run `f` against a fresh popup session in BOTH popup states, asserting the
    /// invariant afterwards each time.
    fn both_states(label: &str, pane_count: usize, mut f: impl FnMut(&mut ServerState, PaneId)) {
        for visible in [true, false] {
            let (mut st, name, popup) = state_with_popup(pane_count, visible);
            f(&mut st, popup);
            let ctx = format!("{label} (popup_visible={visible})");
            // The session may have been removed by the mutation (e.g. close_tab
            // on the last tab); only assert while it still exists.
            if let Some(sess) = st.sessions.get(&name) {
                assert_popup_invariant(sess, &ctx);
                assert_eq!(
                    sess.popup_pane,
                    Some(popup),
                    "[{ctx}] popup identity changed"
                );
            }
        }
    }

    // -- The structural command matrix --------------------------------------

    #[test]
    fn pane_move_all_directions_never_capture_the_popup() {
        for dir in [
            FocusDirection::Left,
            FocusDirection::Right,
            FocusDirection::Up,
            FocusDirection::Down,
        ] {
            both_states(&format!("PaneMove {dir:?}"), 3, |st, _popup| {
                let sess = st.sessions.get_mut("main").expect("session");
                let tab = sess.tabs.get_mut(0).expect("tab");
                // Mirrors the daemon's PaneMove* arm: swap with the neighbor, else
                // relocate to the edge.
                if let Some(neighbor) =
                    find_neighbor(&tab.layout, AREA, tab.focused_pane, dir.clone(), 0)
                {
                    swap_panes(&mut tab.layout, tab.focused_pane, neighbor);
                } else if let Some(new_tree) =
                    relocate_pane_to_edge(&tab.layout, tab.focused_pane, dir.clone())
                {
                    tab.layout = new_tree;
                }
            });
        }
    }

    #[test]
    fn pane_focus_all_directions_never_capture_the_popup() {
        for dir in [
            FocusDirection::Left,
            FocusDirection::Right,
            FocusDirection::Up,
            FocusDirection::Down,
        ] {
            both_states(&format!("PaneFocus {dir:?}"), 3, |st, popup| {
                let sess = st.sessions.get_mut("main").expect("session");
                let tab = sess.tabs.get_mut(0).expect("tab");
                if let Some(target) = layout::focus_in_direction(
                    &mut tab.layout,
                    AREA,
                    tab.focused_pane,
                    dir.clone(),
                    0,
                ) {
                    tab.focused_pane = target;
                }
                // Directional focus can never land on the popup: it is not in the
                // tree, so no neighbor search can reach it.
                assert_ne!(tab.focused_pane, popup);
            });
        }
    }

    #[test]
    fn layout_next_in_every_mode_never_captures_the_popup() {
        for mode in [
            LayoutMode::Bsp(BspLayout),
            LayoutMode::Master(MasterLayout::default()),
            LayoutMode::Monocle(MonocleLayout),
            LayoutMode::Grid(GridLayout),
            LayoutMode::Custom(CustomLayout),
        ] {
            let label = format!("LayoutNext -> {}", mode.name());
            both_states(&label, 4, |st, _popup| {
                let sess = st.sessions.get_mut("main").expect("session");
                let tab = sess.tabs.get_mut(0).expect("tab");
                tab.layout_mode = mode.clone();
                if tab.layout_mode.is_automatic() {
                    // The rebuild reads `pane_order` -- the exact route by which a
                    // leaked popup id would become a real layout leaf.
                    tab.layout = tab
                        .layout_mode
                        .build_tree(&tab.pane_order, tab.focused_pane);
                }
            });
        }
    }

    #[test]
    fn set_master_never_promotes_the_popup() {
        both_states("SetMaster", 3, |st, popup| {
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            tab.layout_mode = LayoutMode::Master(MasterLayout::default());
            if let LayoutMode::Master(ref mut ml) = tab.layout_mode {
                ml.master_pane = Some(tab.focused_pane);
                tab.layout = tab
                    .layout_mode
                    .build_tree(&tab.pane_order, tab.focused_pane);
            }
            assert!(!layout::all_pane_ids(&tab.layout).contains(&popup));
        });
    }

    #[test]
    fn pane_toggle_zoom_never_zooms_the_popup() {
        both_states("PaneToggleZoom", 2, |st, popup| {
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            tab.zoomed_pane = Some(tab.focused_pane);
            assert_ne!(tab.zoomed_pane, Some(popup));
        });
    }

    #[test]
    fn splits_and_stack_ops_never_capture_the_popup() {
        both_states("PaneSplitVertical", 2, |st, _popup| {
            let new_id = st.next_pane_id();
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            let focused = tab.focused_pane;
            tab.layout.split_vertical(focused, new_id);
            tab.pane_order.push(new_id);
            tab.focused_pane = new_id;
        });
        both_states("PaneSplitHorizontal", 2, |st, _popup| {
            let new_id = st.next_pane_id();
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            let focused = tab.focused_pane;
            tab.layout.split_horizontal(focused, new_id);
            tab.pane_order.push(new_id);
            tab.focused_pane = new_id;
        });
        both_states("PaneStackAdd", 2, |st, _popup| {
            let new_id = st.next_pane_id();
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            let focused = tab.focused_pane;
            tab.layout.add_to_stack(focused, new_id);
            tab.pane_order.push(new_id);
            tab.focused_pane = new_id;
        });
        both_states("PaneStackNext/Prev", 3, |st, popup| {
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            if let Some(next) = tab.layout.stack_next(tab.focused_pane) {
                tab.focused_pane = next;
            }
            if let Some(prev) = tab.layout.stack_prev(tab.focused_pane) {
                tab.focused_pane = prev;
            }
            assert_ne!(tab.focused_pane, popup);
        });
    }

    #[test]
    fn resize_never_captures_the_popup() {
        both_states("ResizeLeft", 3, |st, _popup| {
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            tab.layout
                .resize(tab.focused_pane, Direction::Vertical, -0.05);
            tab.layout
                .resize(tab.focused_pane, Direction::Horizontal, 0.05);
        });
    }

    #[test]
    fn pane_close_never_captures_the_popup() {
        both_states("PaneClose", 3, |st, _popup| {
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            let victim = tab.focused_pane;
            if let Some(nf) = tab.layout.close_pane(victim) {
                tab.pane_order.retain(|&id| id != victim);
                tab.focused_pane = nf;
            }
        });
    }

    #[test]
    fn tab_new_and_tab_close_never_capture_the_popup() {
        both_states("TabNew", 2, |st, _popup| {
            st.create_tab("main", "Tab 2", LayoutMode::Bsp(BspLayout))
                .expect("create_tab");
        });
        both_states("TabClose (not last)", 2, |st, _popup| {
            st.create_tab("main", "Tab 2", LayoutMode::Bsp(BspLayout))
                .expect("create_tab");
            let (_panes, deleted) = st.close_tab("main", 1).expect("close_tab");
            assert!(!deleted);
        });
    }

    // -- Teardown reclaims the popup PTY ------------------------------------

    #[test]
    fn close_tab_on_last_tab_returns_the_popup_pane() {
        for visible in [true, false] {
            let (mut st, _, popup) = state_with_popup(2, visible);
            let (panes, deleted) = st.close_tab("main", 0).expect("close_tab");
            assert!(deleted, "closing the only tab deletes the session");
            assert!(
                panes.contains(&popup),
                "popup pane {popup} must be reclaimed with the session, got {panes:?}"
            );
            assert!(!st.sessions.contains_key("main"));
        }
    }

    #[test]
    fn delete_session_returns_the_popup_pane() {
        let (mut st, _, popup) = state_with_popup(2, true);
        let panes = st.delete_session("main").expect("delete_session");
        assert!(
            panes.contains(&popup),
            "popup pane {popup} must be reclaimed, got {panes:?}"
        );
    }

    // -- The converse: normal panes and the popup don't contaminate ---------

    #[test]
    fn a_regular_pane_never_becomes_the_popup() {
        let (mut st, _, popup) = state_with_popup(3, true);
        let real: Vec<PaneId> = {
            let sess = sess_of(&st, "main");
            sess.tabs[0].pane_order.clone()
        };
        // Every structural op above already ran; re-verify the popup id is
        // disjoint from the real pane set and unchanged by more mutation.
        for id in &real {
            assert_ne!(*id, popup, "real pane {id} collided with the popup id");
        }
        st.create_tab("main", "Tab 2", LayoutMode::Bsp(BspLayout))
            .expect("create_tab");
        let sess = sess_of(&st, "main");
        assert_eq!(sess.popup_pane, Some(popup));
        for tab in &sess.tabs {
            assert!(!tab.pane_order.contains(&popup));
        }
        assert_popup_invariant(sess, "regular pane never becomes the popup");
    }

    #[test]
    fn closing_normal_panes_does_not_disturb_the_popup() {
        let (mut st, _, popup) = state_with_popup(3, true);
        let order = sess_of(&st, "main").tabs[0].pane_order.clone();
        // Close every real pane but the last one.
        for victim in order.iter().take(order.len() - 1) {
            let sess = st.sessions.get_mut("main").expect("session");
            let tab = sess.tabs.get_mut(0).expect("tab");
            if let Some(nf) = tab.layout.close_pane(*victim) {
                tab.pane_order.retain(|id| id != victim);
                tab.focused_pane = nf;
            }
            assert_eq!(sess.popup_pane, Some(popup), "popup lost on pane close");
            assert!(sess.popup_visible, "popup visibility lost on pane close");
            assert_popup_invariant(sess, "closing normal panes");
        }
    }

    // -- Popup state behavior -----------------------------------------------

    #[test]
    fn input_target_is_the_popup_only_while_visible() {
        let (mut st, _, popup) = state_with_popup(2, true);
        let focused = sess_of(&st, "main").tabs[0].focused_pane;
        assert_eq!(sess_of(&st, "main").input_target(), Some(popup));

        // Hiding returns input to EXACTLY the pane that had it -- the whole point
        // of not touching `tab.focused_pane`.
        st.sessions.get_mut("main").expect("session").popup_visible = false;
        assert_eq!(sess_of(&st, "main").input_target(), Some(focused));

        // Visible but with no pane yet (never toggled) falls back too.
        let sess = st.sessions.get_mut("main").expect("session");
        sess.popup_visible = true;
        sess.popup_pane = None;
        assert_eq!(sess_of(&st, "main").input_target(), Some(focused));
    }

    #[test]
    fn take_popup_clears_both_fields() {
        let (mut st, _, popup) = state_with_popup(2, true);
        let sess = st.sessions.get_mut("main").expect("session");
        assert_eq!(sess.take_popup(), Some(popup));
        assert_eq!(sess.popup_pane, None);
        assert!(!sess.popup_visible);
        assert_eq!(sess.take_popup(), None, "second take is a no-op");
    }

    #[test]
    fn resize_popup_clamps_and_sticks() {
        let (mut st, _, _) = state_with_popup(2, true);
        let sess = st.sessions.get_mut("main").expect("session");
        assert_eq!(sess.popup_size, (80, 80));
        assert_eq!(sess.resize_popup(5, 0), (85, 80));
        assert_eq!(sess.resize_popup(0, 5), (85, 85));
        assert_eq!(sess.popup_size, (85, 85), "the adjustment sticks");
        // Clamp at both ends, including a shrink that would underflow u8.
        for _ in 0..10 {
            sess.resize_popup(20, 20);
        }
        assert_eq!(sess.popup_size, (100, 100));
        for _ in 0..20 {
            sess.resize_popup(-20, -20);
        }
        assert_eq!(sess.popup_size, (20, 20));
    }

    #[test]
    fn popup_state_is_never_persisted() {
        let (st, _, _) = state_with_popup(2, true);
        let json = serde_json::to_string(&st).expect("serialize");
        assert!(
            !json.contains("popup"),
            "popup state must not be persisted (PTYs don't survive a restart): {json}"
        );
        let back: ServerState = serde_json::from_str(&json).expect("deserialize");
        let sess = back.sessions.get("main").expect("session");
        assert_eq!(sess.popup_pane, None);
        assert!(!sess.popup_visible);
        assert_eq!(
            sess.popup_size,
            (80, 80),
            "restored sessions get the default size"
        );
    }
    // -- The structural invariant, in production -----------------------------
    //
    // `check_structural_invariant` is compiled into release builds (guarded by
    // `debug_check_invariant`), so these assert on the production function
    // rather than on a test-only mirror of it.

    #[test]
    fn invariant_accepts_a_healthy_session_with_a_popup() {
        for visible in [true, false] {
            let (st, name, _popup) = state_with_popup(3, visible);
            assert!(
                check_structural_invariant(sess_of(&st, &name)).is_ok(),
                "the popup pane must never trip the invariant (visible={visible})"
            );
        }
    }

    #[test]
    fn invariant_catches_a_pane_missing_from_the_tree() {
        let (mut st, name, _popup) = state_with_popup(2, false);
        st.sessions.get_mut(&name).expect("session").tabs[0]
            .pane_order
            .push(4242);
        let err = check_structural_invariant(sess_of(&st, &name)).expect_err("must be caught");
        assert!(err.contains("disagree"), "{err}");
    }

    #[test]
    fn invariant_catches_a_pane_missing_from_pane_order() {
        let (mut st, name, _popup) = state_with_popup(2, false);
        let tab = &mut st.sessions.get_mut(&name).expect("session").tabs[0];
        let orphan = tab.pane_order.pop().expect("a pane to orphan");
        let err = check_structural_invariant(sess_of(&st, &name)).expect_err("must be caught");
        assert!(err.contains("disagree"), "orphan {orphan}: {err}");
    }

    #[test]
    fn invariant_catches_a_duplicate_in_pane_order() {
        let (mut st, name, _popup) = state_with_popup(2, false);
        let tab = &mut st.sessions.get_mut(&name).expect("session").tabs[0];
        let dup = tab.pane_order[0];
        tab.pane_order.push(dup);
        let err = check_structural_invariant(sess_of(&st, &name)).expect_err("must be caught");
        assert!(err.contains("duplicates"), "{err}");
    }

    #[test]
    fn invariant_catches_the_popup_leaking_into_the_layout() {
        let (mut st, name, popup) = state_with_popup(2, true);
        let tab = &mut st.sessions.get_mut(&name).expect("session").tabs[0];
        let focused = tab.focused_pane;
        tab.layout.add_to_stack(focused, popup);
        tab.pane_order.push(popup);
        let err = check_structural_invariant(sess_of(&st, &name)).expect_err("must be caught");
        assert!(err.contains("leaked"), "{err}");
    }

    #[test]
    fn invariant_catches_a_stale_zoomed_pane() {
        let (mut st, name, _popup) = state_with_popup(2, false);
        st.sessions.get_mut(&name).expect("session").tabs[0].zoomed_pane = Some(9999);
        let err = check_structural_invariant(sess_of(&st, &name)).expect_err("must be caught");
        assert!(err.contains("zoomed_pane"), "{err}");
    }

    #[test]
    fn invariant_catches_the_popup_as_zoomed_pane() {
        let (mut st, name, popup) = state_with_popup(2, true);
        st.sessions.get_mut(&name).expect("session").tabs[0].zoomed_pane = Some(popup);
        let err = check_structural_invariant(sess_of(&st, &name)).expect_err("must be caught");
        assert!(err.contains("zoomed_pane"), "{err}");
    }

    #[test]
    fn reconcile_pane_order_repairs_a_deserialized_tab() {
        let (mut st, name, _popup) = state_with_popup(3, false);
        let tab = &mut st.sessions.get_mut(&name).expect("session").tabs[0];
        // The shape an old snapshot restores as: a full tree, no `pane_order`.
        let tree = layout::all_pane_ids(&tab.layout);
        tab.pane_order.clear();
        tab.zoomed_pane = Some(4242);
        tab.reconcile_pane_order();
        assert_eq!(tab.pane_order, tree, "pane_order rebuilt from the tree");
        assert_eq!(tab.zoomed_pane, None, "the stale zoom id is dropped");
        assert!(check_structural_invariant(sess_of(&st, &name)).is_ok());
    }

    #[test]
    fn reconcile_pane_order_drops_orphans_and_keeps_order() {
        let (mut st, name, _popup) = state_with_popup(3, false);
        let tab = &mut st.sessions.get_mut(&name).expect("session").tabs[0];
        let tree = layout::all_pane_ids(&tab.layout);
        let kept: Vec<PaneId> = tab.pane_order.iter().rev().copied().collect();
        tab.pane_order = kept.clone();
        tab.pane_order.push(4242); // not in the tree
        tab.reconcile_pane_order();
        assert_eq!(tab.pane_order, kept, "existing order is preserved");
        assert!(!tab.pane_order.contains(&4242), "orphans are dropped");
        assert_eq!(
            tab.pane_order.len(),
            tree.len(),
            "every tree pane is present"
        );
    }

    // -- Zoom: `zoomed_pane` is an id, and it is honoured --------------------

    #[test]
    fn effective_layout_is_the_real_tree_when_not_zoomed() {
        let (st, name, _popup) = state_with_popup(3, false);
        let tab = &sess_of(&st, &name).tabs[0];
        let effective = tab.effective_layout();
        assert_eq!(
            layout::all_pane_ids(&effective),
            layout::all_pane_ids(&tab.layout)
        );
    }

    #[test]
    fn effective_layout_honours_the_recorded_zoom_id_not_the_focus() {
        let (mut st, name, _popup) = state_with_popup(3, false);
        let tab = &mut st.sessions.get_mut(&name).expect("session").tabs[0];
        let panes = layout::all_pane_ids(&tab.layout);
        let (zoomed, other) = (panes[0], panes[2]);
        tab.zoomed_pane = Some(zoomed);
        // A focus change that did NOT go through `focus_pane` must not silently
        // redirect the zoom to a different pane.
        tab.focused_pane = other;
        assert_eq!(
            layout::all_pane_ids(&tab.effective_layout()),
            vec![zoomed],
            "the zoom shows the pane it names"
        );
    }

    #[test]
    fn focus_pane_carries_an_active_zoom_and_is_inert_without_one() {
        let (mut st, name, _popup) = state_with_popup(3, false);
        let tab = &mut st.sessions.get_mut(&name).expect("session").tabs[0];
        let panes = layout::all_pane_ids(&tab.layout);
        let (first, last) = (panes[0], panes[2]);

        tab.focus_pane(last);
        assert_eq!(tab.focused_pane, last);
        assert_eq!(tab.zoomed_pane, None, "no zoom is created out of thin air");

        tab.zoomed_pane = Some(last);
        tab.focus_pane(first);
        assert_eq!(tab.focused_pane, first);
        assert_eq!(
            tab.zoomed_pane,
            Some(first),
            "the zoom follows focus, so the id never goes stale"
        );
        assert_eq!(layout::all_pane_ids(&tab.effective_layout()), vec![first]);
        assert!(check_structural_invariant(sess_of(&st, &name)).is_ok());
    }

    #[test]
    fn panes_accessor_agrees_with_the_layout_tree() {
        let (st, name, _popup) = state_with_popup(3, true);
        let tab = &sess_of(&st, &name).tabs[0];
        let mut from_tree = layout::all_pane_ids(&tab.layout);
        let mut from_accessor = tab.panes().to_vec();
        from_tree.sort_unstable();
        from_accessor.sort_unstable();
        assert_eq!(from_accessor, from_tree);
        assert!(
            !tab.panes()
                .contains(&sess_of(&st, &name).popup_pane.expect("popup")),
            "the popup is in neither side"
        );
    }
}
