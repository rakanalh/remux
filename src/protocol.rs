use serde::{Deserialize, Serialize};

use crate::server::layout::FocusDirection;

/// Unique identifier for a pane within the server.
///
/// This is a plain `u64` alias (kept in sync with `server::layout::PaneId`) so
/// the wire types don't have to name that type. Note the module does depend on
/// `server::layout` for [`FocusDirection`] (used by the view resize/move
/// intents) — that direction is acyclic because `server::layout` itself only
/// depends on `serde`.
pub type PaneId = u64;

/// Unique identifier for a shared server-side View. Plain `u64` alias, matching
/// [`PaneId`]'s style.
pub type ViewId = u64;

/// Unique, per-view stable identifier for a View cell. Plain `u64` alias.
pub type CellId = u64;

/// Identifies which connection (from the *client's* perspective) a shared-view
/// cell's pane lives on. Mirrors the client's `ConnId`, but defined here so the
/// wire protocol carries no dependency on client internals. The server stores
/// and echoes it verbatim; it does not resolve remote descriptors itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnDescriptor {
    /// The local server the client is directly attached to.
    Local,
    /// A named remote from the client's `[remotes]` config.
    Remote(String),
}

// ---------------------------------------------------------------------------
// Version handshake
// ---------------------------------------------------------------------------

/// The wire protocol version. Bump when a breaking change is made to the
/// framed message shapes exchanged between client and server.
///
/// Went 4 -> 6, skipping 5, deliberately. Two lines of work each bumped 4 -> 5
/// independently and with DIFFERENT message sets -- `ViewSetMaster` on master,
/// `SubscribeSessionTree`/`UnsubscribeSessionTree` on the sidebar branch. Two
/// peers both claiming 5 while speaking different wires would complete the
/// `Hello`/`Welcome` handshake and only then disagree, which is precisely what
/// this number exists to prevent. Merging the two therefore resolves to 6 so
/// that no build of either lineage can be mistaken for the merged protocol.
///
/// 6 -> 7: the file-manager sidebar plugin. Adds
/// [`ClientMessage::SpawnAuxPane`]/[`ClientMessage::KillAuxPane`] and
/// [`ServerMessage::AuxPaneSpawned`], the `cwd`/`is_active` session-tree fields
/// those need to follow the focused pane, and
/// [`ServerMessage::SessionBorderStyle`].
///
/// 7 -> 8: the agents sidebar plugin. Adds
/// [`ClientMessage::SubscribeAgents`]/[`ClientMessage::UnsubscribeAgents`] and
/// [`ServerMessage::AgentList`].
pub const PROTOCOL_VERSION: u32 = 8;

/// Full build version string ("0.1.0+<githash>") used in Hello/Welcome so
/// version skew between rebuilt binaries is detectable. Falls back to
/// "<version>+unknown" when git metadata is unavailable (e.g. crates.io build).
pub fn build_version() -> String {
    format!("{}+{}", env!("CARGO_PKG_VERSION"), env!("REMUX_GIT_HASH"))
}

/// First frame sent by a connecting client, announcing its protocol/build.
///
/// FROZEN WIRE SHAPE — never rename/remove/retype existing fields; only add
/// `#[serde(default)]` optional fields. This is the one message exchanged
/// before version-compatible messaging is established, so a version skew must
/// be detectable here rather than crashing mid-session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub remux_version: String,
}

/// First frame sent by the server in response to a `Hello`.
///
/// FROZEN WIRE SHAPE — never rename/remove/retype existing fields; only add
/// `#[serde(default)]` optional fields. This is the one message exchanged
/// before version-compatible messaging is established, so a version skew must
/// be detectable here rather than crashing mid-session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol_version: u32,
    pub remux_version: String,
}

// ---------------------------------------------------------------------------
// Client -> Server
// ---------------------------------------------------------------------------

/// Messages sent from a Remux client to the server over the Unix socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Attach to an existing session by name.
    Attach { session_name: String },
    /// Detach from the currently attached session.
    Detach,
    /// Send raw input bytes to the active pane's PTY.
    Input { data: Vec<u8> },
    /// Notify the server that the client terminal was resized.
    Resize { cols: u16, rows: u16 },
    /// Execute a command (typically triggered from Normal-mode keybindings).
    Command(RemuxCommand),
    /// Create a new session, optionally inside a folder.
    CreateSession {
        name: String,
        folder: Option<String>,
    },
    /// Request the list of active sessions.
    ListSessions,
    /// Kill (destroy) a session by name.
    KillSession { name: String },
    /// Notify the server that the client's input mode changed.
    ModeChanged { mode: String },
    /// A mouse click at the given coordinates.
    ///
    /// `pane_id` selects the coordinate space and the routing (see
    /// [`MouseDrag`](ClientMessage::MouseDrag)): `None` = screen coordinates in
    /// the client's foreground session, `Some(id)` = content coordinates in that
    /// pane, routed by identity.
    ///
    /// `release` distinguishes the button going DOWN (`false`, the default and
    /// what an older client always means) from a button-UP that never moved. A
    /// gesture that moved reports its release through
    /// [`MouseDrag`](ClientMessage::MouseDrag) with `is_final`; one that did not
    /// has no drag to finalize, so the release arrives here. A mouse-tracking
    /// application needs both halves or it latches the button down forever.
    MouseClick {
        x: u16,
        y: u16,
        #[serde(default)]
        pane_id: Option<PaneId>,
        #[serde(default)]
        release: bool,
    },
    /// A mouse drag selection from start to end coordinates.
    ///
    /// `pane_id` selects the coordinate space and the routing:
    /// - `None` (the default, and what an older client sends): screen
    ///   coordinates, resolved against the client's foreground session layout.
    /// - `Some(id)`: coordinates are **content-relative** to that pane's own
    ///   rendered grid (0-based, borders already subtracted), and the gesture is
    ///   routed by pane identity. This is what a View cell uses: a client
    ///   displaying a view is detached, so it has no foreground session for the
    ///   screen-coordinate path to resolve against.
    ///
    /// `#[serde(default)]` keeps the wire shape compatible in both directions —
    /// an older server decodes a new client's pane-scoped drag as `None`
    /// (session-scoped), which, since such a client is detached while in a view,
    /// no-ops in the handler rather than selecting in the wrong pane.
    MouseDrag {
        start_x: u16,
        start_y: u16,
        end_x: u16,
        end_y: u16,
        /// `true` when the mouse button was released (final drag event).
        is_final: bool,
        #[serde(default)]
        pane_id: Option<PaneId>,
    },
    /// A mouse wheel event at the given full-screen 0-based coordinates.
    /// `up` is true for wheel-up, false for wheel-down. The server decides
    /// whether to forward it to the pane's application or scroll remux.
    MouseScroll { x: u16, y: u16, up: bool },
    /// Request the scrollback content for the active pane (for search).
    RequestScrollback,
    /// Send search match info to the server for status bar display.
    SearchInfo { current: usize, total: usize },
    /// Request the full session tree (folders, sessions, tabs, panes).
    ListSessionTree,
    /// Subscribe to unsolicited [`ServerMessage::SessionTree`] pushes whenever
    /// the structure changes, instead of polling with `ListSessionTree`.
    ///
    /// Per-connection and independent of attachment, exactly like
    /// [`ClientMessage::SubscribePane`]: a client that has no session in the
    /// foreground still receives them. One `SessionTree` is sent immediately in
    /// answer, so a subscriber's panel is populated at once rather than staying
    /// blank until the next change. Dropped automatically when the connection
    /// goes away.
    SubscribeSessionTree,
    /// Stop receiving [`ServerMessage::SessionTree`] pushes. A no-op for a
    /// client that never subscribed.
    UnsubscribeSessionTree,
    /// Subscribe to unsolicited [`ServerMessage::AgentList`] pushes: the panes
    /// on this server that are running an AI coding agent, and what each one is
    /// doing.
    ///
    /// A SECOND subscription rather than a field on the session tree, because
    /// the two are dirtied by different things. The tree changes structurally --
    /// a few times a second at worst -- while agent state changes with pane
    /// OUTPUT. Folding them together would drive the tree's per-pane `/proc`
    /// sweep from every keystroke echo.
    ///
    /// Per-connection and independent of attachment, exactly like
    /// [`ClientMessage::SubscribeSessionTree`]. One `AgentList` is sent
    /// immediately in answer, so a subscriber's panel is populated at once.
    /// Dropped automatically when the connection goes away.
    SubscribeAgents,
    /// Stop receiving [`ServerMessage::AgentList`] pushes. A no-op for a client
    /// that never subscribed.
    UnsubscribeAgents,
    /// Materialize a dormant (saved-but-not-live) session into a live session
    /// by name, reusing the startup restore path. Only meaningful when the
    /// server was started with `save_sessions = true` and
    /// `automatic_restore = false`.
    ResurrectSession { name: String },
    /// Scroll the focused pane by delta lines (positive = up/back, negative = down/forward).
    /// The server owns the scroll offset and clamps it to valid range.
    ScrollDelta { delta: i32 },
    /// Reset scroll to live view (offset 0).
    ScrollReset,
    /// Request scrollback info (total line count) for the active pane.
    RequestScrollbackInfo,
    /// Subscribe to a pane's rendered content, streamed regardless of which
    /// session/tab this client has in the foreground. Used by View cells that
    /// alias a real pane. `cols`/`rows` are the subscribing cell's desired size.
    ///
    /// `size_demand` distinguishes a cell that SHOWS the pane from one that only
    /// watches it:
    /// - `true` (a cell the view's layout draws): the cell demands the pane
    ///   reflow to `(cols, rows)` — it is folded into the pane's
    ///   min-across-viewers effective size, so the pane fits the cell.
    /// - `false` (a watch-only subscription: a cell hidden by the layout, a cell
    ///   whose pane is session-visible, or a pure observer): the content is
    ///   streamed but NO size constraint is imposed, so merely watching never
    ///   reflows the source pane.
    ///
    /// `#[serde(default)]` on `size_demand` means an older peer that omits it
    /// decodes as `false` (no constraint) — the safe direction.
    SubscribePane {
        pane_id: PaneId,
        cols: u16,
        rows: u16,
        #[serde(default)]
        size_demand: bool,
    },
    /// Stop receiving `PaneContent` for a previously subscribed pane.
    UnsubscribePane { pane_id: PaneId },
    /// Route raw input bytes to a pane by identity, independent of this
    /// client's foreground session/tab. Mirrors `Input`, but instead of
    /// resolving the target from the foreground focus it targets `pane_id`
    /// explicitly -- this is what a focused View cell uses to type into the
    /// real pane it aliases, wherever that pane actually lives.
    InputToPane { pane_id: PaneId, data: Vec<u8> },
    /// Spawn an **auxiliary pane**: a PTY that belongs to no layout tree, owned
    /// by the requesting client and reaped when that client goes away.
    ///
    /// This is the server half of a sidebar panel that hosts a full-screen TUI
    /// (the `files` plugin's file manager). It is not a new concept on the
    /// server: `spawn_pane` already inserts into the flat pane map and touches
    /// no layout, and everything that enumerates panes for display or
    /// persistence walks a `LayoutNode` -- so an aux pane is invisible to the
    /// session tree, to `remux` layouts and to save/restore by construction.
    ///
    /// `command` is REQUIRED (no default shell fallback): the plugin exists to
    /// host a specific program, and a wrong guess spawns something the user then
    /// has to hunt down and kill. `cwd` is the directory to start it in; `None`
    /// inherits the server's.
    ///
    /// Answered with [`ServerMessage::AuxPaneSpawned`]. The client then
    /// `SubscribePane`s the returned id exactly as a View cell does -- the whole
    /// streaming half is the Views machinery unchanged.
    SpawnAuxPane {
        cols: u16,
        rows: u16,
        command: String,
        cwd: Option<String>,
    },
    /// Kill an auxiliary pane this client spawned. Ignored for a pane this
    /// connection does not own, so one client can never reap another's.
    ///
    /// The clean counterpart to the disconnect reap: a panel re-targeting to a
    /// new directory kills its old pane rather than waiting for the client to
    /// exit.
    KillAuxPane { pane_id: PaneId },
    /// Scroll a subscribed pane's own scroll view by `lines` (per-subscriber,
    /// by pane identity), independent of this client's foreground scroll. Used
    /// by a View cell's mouse wheel: the server adjusts a per-(client, pane)
    /// scroll offset, clamps it to the pane's scrollback, and streams a fresh
    /// `PaneContent` rendered at that offset back to this client only. `up`
    /// scrolls back into history, `!up` forward toward the live view.
    /// `x`/`y` are the wheel position in the pane's own **content** coordinates
    /// (0-based, borders already subtracted), the same space
    /// [`MouseDrag`](ClientMessage::MouseDrag) uses when `pane_id` is set. They
    /// matter only when the pane's application has mouse tracking on, in which
    /// case the wheel is forwarded to it as a mouse report at that position
    /// instead of scrolling remux's own view. `#[serde(default)]` keeps an older
    /// client's coordinate-less wheel decodable: it lands on the pane's
    /// top-left cell, which is the only place a position-less report can go.
    ScrollPane {
        pane_id: PaneId,
        up: bool,
        lines: u16,
        #[serde(default)]
        x: u16,
        #[serde(default)]
        y: u16,
    },

    // -- Shared View intents ------------------------------------------------
    // Client → server requests to mutate the server-owned shared-view registry.
    // Every mutation is followed by a `ViewList` broadcast to all clients, so
    // views are consistent across every connected terminal. `ViewCreate` also
    // gets a direct `ViewCreated { id }` ack to the requester.
    /// Create a new (empty, Grid-default) shared view named `name`.
    ViewCreate { name: String },
    /// Delete the view `id` (no-op if it doesn't exist).
    ViewDelete { id: ViewId },
    /// Rename view `id` to `name`.
    ViewRename { id: ViewId, name: String },
    /// Append cells aliasing the given `(connection, pane)` pairs to view `id`.
    ViewAddCells {
        id: ViewId,
        cells: Vec<(ConnDescriptor, PaneId)>,
    },
    /// Remove the cell `cell_id` from view `id`.
    ViewRemoveCell { id: ViewId, cell_id: CellId },
    /// Focus the cell `cell_id` within view `id`.
    ViewSetFocus { id: ViewId, cell_id: CellId },
    /// Cycle view `id` to the next automatic layout (dropping any custom tree).
    ViewCycleLayout { id: ViewId },
    /// Toggle focus-cell zoom for view `id`.
    ViewToggleZoom { id: ViewId },
    /// Switch view `id` to the Master layout and promote its focused cell into
    /// the master slot (dropping any custom tree). The view's own `focused` is
    /// authoritative -- focus is server-owned and set by `ViewSetFocus` -- so no
    /// cell id travels, mirroring `ViewToggleZoom`.
    ViewSetMaster { id: ViewId },
    /// Resize the cell `cell_id` in view `id` by `amount` percent toward `dir`.
    ViewResizeCell {
        id: ViewId,
        cell_id: CellId,
        dir: FocusDirection,
        amount: u16,
    },
    /// Move the cell `cell_id` in view `id` toward `dir` (swap with the spatial
    /// neighbor, else relocate to that edge).
    ViewMoveCell {
        id: ViewId,
        cell_id: CellId,
        dir: FocusDirection,
    },
}

// ---------------------------------------------------------------------------
// Server -> Client
// ---------------------------------------------------------------------------

/// Messages sent from the Remux server to a connected client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Full screen render (sent on attach or after a major layout change).
    FullRender {
        cells: Vec<Vec<RenderCell>>,
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
        /// The focused pane's rectangle in the composited buffer.
        focused_pane_rect: Option<PaneRect>,
        /// Whether the focused pane has application cursor keys (DECCKM) active.
        #[serde(default)]
        application_cursor_keys: bool,
        /// Index in the combined scrollback+grid buffer of the first displayed line.
        #[serde(default)]
        viewport_top: usize,
        /// How far the viewport is scrolled back from the live tail, in lines;
        /// 0 means the client is watching the live tail.
        ///
        /// **Not derivable from `viewport_top`.** That field is an absolute line
        /// index, so at maximum scroll (`scroll_offset == max_scroll_offset()`,
        /// i.e. the first line of history is the top visible row) it is exactly
        /// `0` -- byte-identical to what the live tail reports. A client that
        /// inferred "am I scrolled?" from it was blind at precisely the maximum,
        /// and so never asked to return to the tail.
        #[serde(default)]
        scroll_offset: usize,
    },
    /// Incremental render update (diff from previous frame).
    RenderDiff {
        changes: Vec<CellChange>,
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
        /// The focused pane's rectangle in the composited buffer.
        focused_pane_rect: Option<PaneRect>,
        /// Whether the focused pane has application cursor keys (DECCKM) active.
        #[serde(default)]
        application_cursor_keys: bool,
        /// Index in the combined scrollback+grid buffer of the first displayed line.
        #[serde(default)]
        viewport_top: usize,
        /// How far the viewport is scrolled back from the live tail, in lines;
        /// 0 means the client is watching the live tail.
        ///
        /// **Not derivable from `viewport_top`.** That field is an absolute line
        /// index, so at maximum scroll (`scroll_offset == max_scroll_offset()`,
        /// i.e. the first line of history is the top visible row) it is exactly
        /// `0` -- byte-identical to what the live tail reports. A client that
        /// inferred "am I scrolled?" from it was blind at precisely the maximum,
        /// and so never asked to return to the tail.
        #[serde(default)]
        scroll_offset: usize,
    },
    /// Optimized scroll render: shift content within a pane rect and render
    /// only the new rows that appeared.
    ScrollRender {
        /// Pane content area to scroll within.
        pane_x: u16,
        pane_y: u16,
        pane_width: u16,
        pane_height: u16,
        /// Rows to scroll. Positive = content moves UP (new rows at top).
        /// Negative = content moves DOWN (new rows at bottom).
        delta: i16,
        /// The new rows to render. Length = abs(delta).
        new_rows: Vec<Vec<RenderCell>>,
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
        focused_pane_rect: Option<PaneRect>,
        application_cursor_keys: bool,
        /// Index in the combined scrollback+grid buffer of the first displayed line.
        #[serde(default)]
        viewport_top: usize,
        /// How far the viewport is scrolled back from the live tail, in lines;
        /// 0 means the client is watching the live tail.
        ///
        /// **Not derivable from `viewport_top`.** That field is an absolute line
        /// index, so at maximum scroll (`scroll_offset == max_scroll_offset()`,
        /// i.e. the first line of history is the top visible row) it is exactly
        /// `0` -- byte-identical to what the live tail reports. A client that
        /// inferred "am I scrolled?" from it was blind at precisely the maximum,
        /// and so never asked to return to the tail.
        #[serde(default)]
        scroll_offset: usize,
    },
    /// Response to a `ListSessions` request.
    SessionList { sessions: Vec<SessionListEntry> },
    /// An error response.
    Error { message: String },
    /// Asynchronous session event notification.
    Event(SessionEvent),
    /// Request the client to copy data to the system clipboard via OSC 52.
    CopyToClipboard { data: String },
    /// Response to a `RequestScrollback` request with the pane's text content.
    ScrollbackContent { lines: Vec<String> },
    /// Response to a `RequestScrollbackInfo` request with the total line count.
    ScrollbackInfo { total_lines: usize },
    /// The border style the server composites the client's newly attached
    /// session with.
    ///
    /// Sent on every successful `Attach`, BEFORE that attach's first frame.
    /// `Session::border_style` is per-session SERVER state that the server's own
    /// `ToggleStyle` handler flips, while the client's copy -- the one that
    /// frames the sidebars -- is seeded once from `appearance.border_style` and
    /// was never re-learned. Two reachable consequences, both on default
    /// keybindings: toggle, detach, reattach, and the panes come back in tmux
    /// style while the sidebar is still framed in zellij; or toggle in one
    /// session and switch to another, and they disagree the other way round.
    /// This is the message that lets an attach resync them.
    SessionBorderStyle { style: crate::config::BorderStyle },
    /// Answer to [`ClientMessage::SpawnAuxPane`]: the id of the pane just
    /// spawned, or `None` if the spawn failed.
    ///
    /// No correlation id. A client requests at most one aux pane per panel and
    /// its requests are serialised by the single per-connection writer task, so
    /// answers arrive in request order; the client matches them against a FIFO
    /// of pending requesters.
    ///
    /// **That design is why failure must still be answered.** A request that got
    /// no reply would sit at the head of the client's queue for ever and claim
    /// the NEXT panel's answer -- one panel showing another's directory, the
    /// other waiting on "starting…" with no way back. Nothing forbids two
    /// `files` panels, so `None` is not a formality: it is what keeps the
    /// correlation-free matching honest.
    AuxPaneSpawned { pane_id: Option<PaneId> },
    /// The panes running an AI coding agent, pushed to every client subscribed
    /// with [`ClientMessage::SubscribeAgents`].
    ///
    /// A full list each time, not a diff: it is a handful of entries at most,
    /// and a diff would need the client to hold a baseline that a dropped push
    /// could desynchronize.
    AgentList {
        #[serde(default)]
        agents: Vec<AgentEntry>,
        /// Whether this server can detect agents AT ALL.
        ///
        /// Detection reads the PTY's foreground process group out of `/proc`,
        /// so a non-Linux server can never list anything. Without this the
        /// panel would render an empty list there -- indistinguishable from
        /// "you have no agents running", and exactly the sort of thing that
        /// gets reported as a bug. The panel says so instead.
        ///
        /// It belongs on the wire rather than being a `cfg!` in the client:
        /// a macOS client attached to a Linux server detects fine, and it is
        /// the SERVER's platform that decides. Defaults to `true`, so a peer
        /// that omits it is assumed capable.
        #[serde(default = "default_true")]
        detection_supported: bool,
    },
    /// Response to a `ListSessionTree` request with the full hierarchy.
    SessionTree {
        folders: Vec<FolderTreeEntry>,
        unfiled: Vec<SessionTreeEntry>,
        /// Names of dormant (saved-but-not-live) sessions that can be
        /// resurrected. Empty unless the server runs with `save_sessions` and
        /// `automatic_restore = false`. `#[serde(default)]` keeps the field
        /// optional on the wire for back-compat with older peers.
        #[serde(default)]
        dormant: Vec<String>,
    },
    /// A full snapshot of one pane's rendered screen, pushed to every client
    /// subscribed to `pane_id`. Independent of the client's foreground
    /// session/tab. Full snapshot each change for now (no diffing).
    PaneContent {
        pane_id: PaneId,
        cols: u16,
        rows: u16,
        cells: Vec<Vec<RenderCell>>,
        /// Source pane's cursor position (already clamped to `cols`/`rows`) and
        /// visibility, so a focused View cell can render the real cursor.
        #[serde(default)]
        cursor_x: u16,
        #[serde(default)]
        cursor_y: u16,
        #[serde(default)]
        cursor_visible: bool,
        /// The source pane's DECCKM (application cursor keys) state, so a focused
        /// cell encodes arrows/nav for THAT pane, not the foreground session's.
        #[serde(default)]
        application_cursor_keys: bool,
        /// The pane's session and tab names, for the cell's border title
        /// (`session / tab`, host-prefixed for remotes by the client). Kept live
        /// so a rename updates the label. `#[serde(default)]` keeps the message
        /// decodable from an older peer that omits them.
        #[serde(default)]
        session_name: String,
        #[serde(default)]
        tab_name: String,
        /// Whether the source pane is currently "session-visible" -- shown in the
        /// active tab of at least one attached client, so its real session drives
        /// it at full size. A View cell whose pane is session-visible renders an
        /// "Active in session" placeholder instead of the (full-size) streamed
        /// content and imposes no size demand. `#[serde(default)]` (false) keeps
        /// the message decodable from an older peer that omits it -- the safe
        /// direction (treat as not-visible = stream content as before).
        #[serde(default)]
        session_visible: bool,
    },
    /// Direct acknowledgement to the client that issued a `ViewCreate`, carrying
    /// the id the new view was assigned. Sent alongside the `ViewList` broadcast
    /// so the creator can immediately refer to the view it just made.
    ViewCreated { id: ViewId },
    /// The full current state of every shared view. Broadcast to ALL connected
    /// clients after any view mutation, and pushed once to a client when it
    /// connects, so every terminal stays in sync with the shared registry.
    ViewList {
        #[serde(default)]
        views: Vec<ViewInfo>,
    },
}

// ---------------------------------------------------------------------------
// Shared view snapshot (server -> client)
// ---------------------------------------------------------------------------

/// One cell of a shared view: a reference to a real pane on a specific
/// connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellInfo {
    /// Stable, per-view identity for this cell.
    pub id: CellId,
    /// Which connection (from the client's perspective) hosts the pane.
    pub conn: ConnDescriptor,
    /// The aliased pane's id on that connection.
    pub pane_id: PaneId,
}

/// A snapshot of one shared view's state, as carried in [`ServerMessage::ViewList`].
///
/// Fields marked `#[serde(default)]` may grow in later protocol revisions
/// without breaking older peers.
///
/// Not `PartialEq`: the embedded [`LayoutMode`]/[`LayoutNode`] are not `PartialEq`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewInfo {
    pub id: ViewId,
    pub name: String,
    #[serde(default)]
    pub cells: Vec<CellInfo>,
    /// The view's automatic layout mode. The full [`LayoutMode`] travels so a
    /// (Phase 2) client can composite the cells without an extra round trip.
    #[serde(default)]
    pub layout: crate::server::layout::LayoutMode,
    /// The persistent manual arrangement, once the user has resized/moved a
    /// cell. `None` means the automatic `layout` is in effect; `Some` means the
    /// view reports its layout name as `custom`.
    #[serde(default)]
    pub custom_tree: Option<crate::server::layout::LayoutNode>,
    /// Index of the focused cell within `cells`.
    #[serde(default)]
    pub focused: usize,
    /// Whether the focused cell is zoomed to fill the view.
    #[serde(default)]
    pub zoomed: bool,
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

/// What an agent pane is doing, as the server classifies it.
///
/// Three states, deliberately, and honestly: `Background`, `Error` and progress
/// reporting need lifecycle hooks the agents do not give us yet, and five states
/// that misreport are worse than three that do not.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentState {
    /// The agent is showing a visible approval, question or permission prompt
    /// and is blocked on the user.
    ///
    /// **Outranks [`AgentState::Working`]**, and never decays on silence -- a
    /// blocked agent produces no output PRECISELY BECAUSE it is waiting, so a
    /// state that decayed would vanish at the moment the user most needs it.
    NeedsInput,
    /// Output reached this pane within the working window.
    Working,
    /// Neither of the above.
    Idle,
}

fn default_true() -> bool {
    true
}

/// One pane running an AI coding agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEntry {
    pub pane_id: PaneId,
    /// The session the pane lives in, for the panel's label and its jump.
    pub session: String,
    /// The index of the tab within that session.
    pub tab_index: usize,
    /// The detected foreground command, e.g. `"claude"`. One of the configured
    /// agent commands by construction: a pane running anything else is not an
    /// entry at all.
    pub command: String,
    pub state: AgentState,
}

// ---------------------------------------------------------------------------
// Pane geometry (sent from server to client for scoped visual mode)
// ---------------------------------------------------------------------------

/// Rectangle describing a focused pane's position and size in the composited
/// screen buffer. Sent alongside render messages so the client can scope
/// visual-mode selection to the active pane.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

// ---------------------------------------------------------------------------
// Rendering primitives
// ---------------------------------------------------------------------------

/// Serde default for [`RenderCell::width`]. Older peers that omit the field
/// decode as normal (single-column) width.
fn default_cell_width() -> u8 {
    1
}

/// A single cell in the rendered terminal grid.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderCell {
    pub c: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Display width in columns: `1` normal, `2` wide lead (CJK/emoji), `0`
    /// continuation cell placed after a wide lead. Defaults to `1` on the wire
    /// for back-compat with peers that predate the field.
    #[serde(default = "default_cell_width")]
    pub width: u8,
    /// Zero-width combining marks composed onto the base glyph `c`. Empty in the
    /// overwhelmingly common case; `skip_serializing_if` keeps empty cells at
    /// zero wire bytes so ASCII rendering is unchanged on the wire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub combining: Vec<char>,
    /// OSC 8 hyperlink target URI for this cell, if any. Omitted on the wire in
    /// the common (non-linked) case; older peers that predate the field decode
    /// it as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperlink: Option<String>,
}

impl Default for RenderCell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: CellColor::Default,
            bg: CellColor::Default,
            bold: false,
            italic: false,
            underline: false,
            width: 1,
            combining: Vec::new(),
            hyperlink: None,
        }
    }
}

/// A single changed cell for diff-based rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellChange {
    pub x: u16,
    pub y: u16,
    pub cell: RenderCell,
}

/// Terminal cell color representation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CellColor {
    /// Use the terminal's default foreground/background.
    Default,
    /// Standard 256-color palette index.
    Indexed(u8),
    /// True-color RGB value.
    Rgb(u8, u8, u8),
}

// ---------------------------------------------------------------------------
// Session tree entries (for session manager)
// ---------------------------------------------------------------------------

/// A folder containing sessions in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderTreeEntry {
    pub name: String,
    pub sessions: Vec<SessionTreeEntry>,
}

/// A session entry in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTreeEntry {
    pub name: String,
    pub tabs: Vec<TabTreeEntry>,
    pub client_count: usize,
    pub is_current: bool,
}

/// A tab entry in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabTreeEntry {
    pub id: u64,
    pub name: String,
    pub panes: Vec<PaneTreeEntry>,
    /// Whether this is its session's ACTIVE tab.
    ///
    /// Needed because [`PaneTreeEntry::is_focused`] is per-tab -- every tab
    /// marks its own focused pane -- so without this a consumer cannot tell
    /// which of them is *the* focused pane of the session. `#[serde(default)]`
    /// decodes an older peer's tree as "no active tab", which reads as "unknown"
    /// rather than lying about one.
    #[serde(default)]
    pub is_active: bool,
}

/// A pane entry in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneTreeEntry {
    pub id: u64,
    pub name: String,
    pub is_focused: bool,
    /// The pane's current working directory, for the `files` sidebar plugin to
    /// follow.
    ///
    /// Filled ONLY for the focused pane of an active tab: resolving it is a
    /// `/proc` readlink per pane (`persistence::get_pane_cwd`), and no consumer
    /// wants the other panes' directories. `None` everywhere else, and for a
    /// pane whose cwd could not be read.
    #[serde(default)]
    pub cwd: Option<String>,
}

// ---------------------------------------------------------------------------
// Session metadata
// ---------------------------------------------------------------------------

/// Entry returned in a session list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub name: String,
    pub folder: Option<String>,
    pub tab_count: usize,
    pub client_count: usize,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// All commands that can be executed within Remux, either from keybindings or
/// the command line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RemuxCommand {
    // -- Tab commands -------------------------------------------------------
    TabNew,
    TabClose,
    TabRename(String),
    TabGoto(usize),
    TabNext,
    TabPrev,
    TabMove(usize),

    // -- Pane commands ------------------------------------------------------
    PaneNew,
    PaneClose,
    PaneSplitVertical,
    PaneSplitHorizontal,
    PaneFocusLeft,
    PaneFocusRight,
    PaneFocusUp,
    PaneFocusDown,
    PaneStackAdd,
    PaneStackNext,
    PaneStackPrev,
    PaneMoveLeft,
    PaneMoveRight,
    PaneMoveUp,
    PaneMoveDown,
    PaneRename(String),
    PaneToggleZoom,
    /// Show/hide the session's popup terminal -- a floating, centered pane drawn
    /// on top of the layout. One popup per session, shared by every attached
    /// client; the pane is spawned lazily on the first toggle and its shell keeps
    /// running while hidden.
    PopupToggle,

    // -- Resize commands ----------------------------------------------------
    ResizeLeft(u16),
    ResizeRight(u16),
    ResizeUp(u16),
    ResizeDown(u16),

    // -- Session commands ---------------------------------------------------
    SessionNew {
        name: String,
        folder: Option<String>,
    },
    SessionDetach,
    SessionRename(String),
    SessionList,
    /// Toggle to the previously-attached session ("last session", like tmux's
    /// last-session). CLIENT-side: intercepted by the client and never
    /// forwarded to the server.
    SessionSwitchLast,

    // -- Folder commands ----------------------------------------------------
    FolderNew(String),
    FolderDelete(String),
    FolderList,
    FolderMoveSession {
        session: String,
        folder: Option<String>,
    },

    // -- Buffer commands ----------------------------------------------------
    BufferEditInEditor,

    // -- Layout commands ------------------------------------------------------
    ToggleStyle,
    LayoutNext,
    SetMaster,

    // -- System / mode commands ---------------------------------------------
    SessionSave,
    EnterNormal,
    EnterCommandMode,
    EnterVisualMode,
    /// Send raw key bytes to the active pane's PTY (used for leader-leader normal mode).
    SendKey(Vec<u8>),
    /// Enter search mode (client-side mode transition).
    EnterSearchMode,
    /// Open the session manager (client-side mode transition).
    OpenSessionManager,
    /// Connect to a remote (client-side command). The argument is either an
    /// SSH destination (`user@host` or an `~/.ssh/config` Host alias) or the
    /// name of a remote already declared in `[remotes]`. Opens the session
    /// manager and makes the remote and its sessions visible.
    RemoteConnect(String),
    /// Open folder selection popup to move current session (client-side only).
    SessionMoveToFolder,
    /// Switch to a specific tab in a specific session.
    SessionSwitchTab {
        session: String,
        tab_index: usize,
    },
    /// Switch to a specific pane in a specific session and tab.
    SessionSwitchPane {
        session: String,
        tab_index: usize,
        pane_id: u64,
    },
    /// Close a tab by index in a specific session.
    TabCloseByIndex {
        session: String,
        tab_index: usize,
    },

    // -- Explicit-target structural commands --------------------------------
    // These operate on an arbitrary (named/indexed) target rather than the
    // requesting client's attached session, so the session manager can edit
    // sessions/folders/tabs/panes it is not currently attached to. Like the
    // other explicit-target commands above (SessionSwitchTab, TabCloseByIndex)
    // they are internal protocol commands issued by the session-manager UI and
    // are deliberately absent from the action registry (`action_specs()`), so
    // no binding string can name them -- a binding cannot supply their
    // structural arguments.
    /// Rename session `old` to `new`. Fail-silently if `old` is missing or
    /// `new` already exists.
    SessionRenameByName {
        old: String,
        new: String,
    },
    /// Rename folder `old` to `new`. Fail-silently if `old` is missing or
    /// `new` already exists.
    FolderRename {
        old: String,
        new: String,
    },
    /// Create a new tab (with its default pane) in the named target session.
    TabNewInSession {
        session: String,
    },
    /// Set the name of a tab by index in the target session.
    TabRenameByIndex {
        session: String,
        tab_index: usize,
        name: String,
    },
    /// Add a pane to a tab (by index) in the target session.
    PaneNewInTab {
        session: String,
        tab_index: usize,
    },
    /// Close a pane by id in the target session.
    PaneCloseById {
        session: String,
        pane_id: u64,
    },
    /// Set the custom name of a pane by id in the target session.
    PaneRenameById {
        session: String,
        pane_id: u64,
        name: String,
    },
    /// Move a tab (by index) left/right by `delta` within the target session.
    TabMoveByIndex {
        session: String,
        tab_index: usize,
        delta: i32,
    },
}

// ---------------------------------------------------------------------------
// Action registry
// ---------------------------------------------------------------------------

/// A client-only action: something a binding (or the command palette) can ask
/// for that has no [`RemuxCommand`] equivalent because it never leaves the
/// client -- it opens an overlay or edits client-side view state.
///
/// The client maps each of these to an `InputAction`; see
/// `crate::client::input::InputHandler::begin_client_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAction {
    /// Open the command palette overlay.
    CommandPaletteOpen,
    /// Open the quick session switcher overlay.
    SessionQuickSwitch,
    /// Prompt for a name and create a new (client-side) view.
    ViewNew,
    /// Open the pane picker to add a cell to the active view.
    ViewAddPane,
    /// Prompt for a new name for the active view.
    ViewRename,
    /// Drop the focused cell from the active view.
    ViewRemovePane,
    /// Cycle the active view's layout mode.
    ViewLayoutNext,
    /// Leave the active view (it keeps existing).
    ViewClose,
    /// Delete the active view for everyone.
    ViewDelete,
    /// Toggle the left sidebar's visibility.
    SidebarToggleLeft,
    /// Toggle the right sidebar's visibility.
    SidebarToggleRight,
    /// Toggle the bottom sidebar's visibility.
    SidebarToggleBottom,
    /// Focus the left sidebar, opening it if hidden.
    SidebarFocusLeft,
    /// Focus the right sidebar, opening it if hidden.
    SidebarFocusRight,
    /// Focus the bottom sidebar, opening it if hidden.
    SidebarFocusBottom,
    /// Cycle focus through every visible panel, then back to the content area.
    SidebarCycle,
}

/// What an action string resolves to: either a command the server executes or
/// a client-only action.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A command to send to the server (some are intercepted client-side
    /// first; see the client's action chain).
    Server(RemuxCommand),
    /// An action the client performs itself.
    Client(ClientAction),
}

/// One entry in the action registry: everything the rest of the program needs
/// to know about a bindable action string.
///
/// This table is the SINGLE source of truth. [`command_names`] is derived from
/// it, [`crate::config::keybindings::resolve_action`] refuses any name that is
/// not in it, and [`crate::config::keybindings::humanize_command`] takes its
/// label overrides from it -- so the palette listing, the parser, the which-key
/// labels and config validation cannot drift apart.
#[derive(Debug, Clone, Copy)]
pub struct ActionSpec {
    /// The PascalCase name a binding string starts with.
    pub name: &'static str,
    /// Argument hint shown in the palette, e.g. `<name>`.
    pub hint: Option<&'static str>,
    /// A representative argument string, so tests can round-trip every
    /// arg-taking entry through the resolver.
    pub sample_args: Option<&'static str>,
    /// `Some` for client-only actions; `None` for server commands.
    pub client: Option<ClientAction>,
    /// Whether the command palette offers this action.
    pub palette: bool,
    /// Which-key label override; `None` falls back to a PascalCase split.
    pub label: Option<&'static str>,
}

impl ActionSpec {
    /// A server command taking no arguments.
    fn server(name: &'static str) -> Self {
        Self {
            name,
            hint: None,
            sample_args: None,
            client: None,
            palette: true,
            label: None,
        }
    }

    /// A client-only action (never takes arguments).
    fn client(name: &'static str, action: ClientAction) -> Self {
        Self {
            client: Some(action),
            ..Self::server(name)
        }
    }

    /// Declare the argument hint shown in the palette and a sample argument
    /// string that makes the action resolvable.
    fn arg(mut self, hint: &'static str, sample_args: &'static str) -> Self {
        self.hint = Some(hint);
        self.sample_args = Some(sample_args);
        self
    }

    /// Hide the action from the command palette.
    fn hidden(mut self) -> Self {
        self.palette = false;
        self
    }

    /// Override the which-key label.
    fn label(mut self, label: &'static str) -> Self {
        self.label = Some(label);
        self
    }
}

/// The action registry: every action string a keybinding or the command
/// palette may name.
///
/// Deliberate exclusions: the explicit-target structural commands
/// (`SessionRenameByName`, `PaneCloseById`, ...) are internal protocol
/// commands issued by the session-manager UI -- a keybinding string cannot
/// supply their structural arguments, so they are not bindable at all.
pub fn action_specs() -> &'static [ActionSpec] {
    static SPECS: std::sync::OnceLock<Vec<ActionSpec>> = std::sync::OnceLock::new();
    SPECS.get_or_init(|| {
        vec![
            ActionSpec::server("TabNew"),
            ActionSpec::server("TabClose"),
            ActionSpec::server("TabRename").arg("<name>", "work"),
            ActionSpec::server("TabGoto").arg("<index>", "0"),
            ActionSpec::server("TabNext"),
            ActionSpec::server("TabPrev"),
            ActionSpec::server("TabMove").arg("<index>", "1"),
            ActionSpec::server("PaneNew"),
            ActionSpec::server("PaneClose"),
            ActionSpec::server("PaneSplitVertical"),
            ActionSpec::server("PaneSplitHorizontal"),
            ActionSpec::server("PaneFocusLeft"),
            ActionSpec::server("PaneFocusRight"),
            ActionSpec::server("PaneFocusUp"),
            ActionSpec::server("PaneFocusDown"),
            ActionSpec::server("PaneStackAdd"),
            ActionSpec::server("PaneStackNext"),
            ActionSpec::server("PaneStackPrev"),
            ActionSpec::server("PaneMoveLeft"),
            ActionSpec::server("PaneMoveRight"),
            ActionSpec::server("PaneMoveUp"),
            ActionSpec::server("PaneMoveDown"),
            ActionSpec::server("PaneRename").arg("<name>", "shell"),
            ActionSpec::server("PaneToggleZoom"),
            ActionSpec::server("PopupToggle"),
            ActionSpec::server("ResizeLeft").arg("<amount>", "5"),
            ActionSpec::server("ResizeRight").arg("<amount>", "5"),
            ActionSpec::server("ResizeUp").arg("<amount>", "5"),
            ActionSpec::server("ResizeDown").arg("<amount>", "5"),
            ActionSpec::server("SessionNew").arg("<name> [folder]", "dev"),
            ActionSpec::server("SessionDetach"),
            ActionSpec::server("SessionRename").arg("<name>", "dev"),
            ActionSpec::server("SessionList"),
            ActionSpec::server("SessionSave"),
            ActionSpec::server("FolderNew").arg("<name>", "projects"),
            ActionSpec::server("FolderDelete").arg("<name>", "projects"),
            ActionSpec::server("FolderList"),
            ActionSpec::server("FolderMoveSession").arg("<session> [folder]", "dev projects"),
            ActionSpec::server("BufferEditInEditor"),
            ActionSpec::server("OpenSessionManager"),
            ActionSpec::server("RemoteConnect").arg("<user@host|alias>", "pi"),
            ActionSpec::server("SessionMoveToFolder"),
            ActionSpec::server("SessionSwitchLast").label("last session"),
            ActionSpec::server("ToggleStyle"),
            ActionSpec::server("LayoutNext").label("next layout"),
            ActionSpec::server("SetMaster").label("set master"),
            ActionSpec::server("EnterNormal"),
            ActionSpec::server("EnterCommandMode"),
            ActionSpec::server("EnterVisualMode"),
            ActionSpec::server("EnterSearchMode"),
            // Send raw key bytes to the focused pane. Hidden from the palette:
            // its argument is a key notation, which is a binding-file concept
            // (`SendKey Ctrl-a`), not something to pick from a list.
            ActionSpec::server("SendKey")
                .arg("<key>", "Ctrl-a")
                .hidden(),
            // -- Client-only actions ----------------------------------------
            ActionSpec::client("CommandPaletteOpen", ClientAction::CommandPaletteOpen)
                .label("command palette"),
            ActionSpec::client("SessionQuickSwitch", ClientAction::SessionQuickSwitch)
                .label("switch session"),
            ActionSpec::client("ViewNew", ClientAction::ViewNew).label("new view"),
            ActionSpec::client("ViewAddPane", ClientAction::ViewAddPane).label("add pane"),
            ActionSpec::client("ViewRename", ClientAction::ViewRename).label("rename view"),
            ActionSpec::client("ViewRemovePane", ClientAction::ViewRemovePane).label("remove cell"),
            ActionSpec::client("ViewLayoutNext", ClientAction::ViewLayoutNext).label("layout next"),
            ActionSpec::client("ViewClose", ClientAction::ViewClose).label("close view"),
            ActionSpec::client("ViewDelete", ClientAction::ViewDelete).label("delete view"),
            ActionSpec::client("SidebarToggleLeft", ClientAction::SidebarToggleLeft)
                .label("toggle left sidebar"),
            ActionSpec::client("SidebarToggleRight", ClientAction::SidebarToggleRight)
                .label("toggle right sidebar"),
            ActionSpec::client("SidebarToggleBottom", ClientAction::SidebarToggleBottom)
                .label("toggle bottom sidebar"),
            ActionSpec::client("SidebarFocusLeft", ClientAction::SidebarFocusLeft)
                .label("focus left sidebar"),
            ActionSpec::client("SidebarFocusRight", ClientAction::SidebarFocusRight)
                .label("focus right sidebar"),
            ActionSpec::client("SidebarFocusBottom", ClientAction::SidebarFocusBottom)
                .label("focus bottom sidebar"),
            ActionSpec::client("SidebarCycle", ClientAction::SidebarCycle)
                .label("cycle sidebar focus"),
        ]
    })
}

/// Look up an action name in the registry.
pub fn action_spec(name: &str) -> Option<&'static ActionSpec> {
    action_specs().iter().find(|spec| spec.name == name)
}

/// Return the command names the palette lists (PascalCase strings that
/// [`crate::config::keybindings::resolve_action`] accepts). Commands that take
/// parameters include a hint suffix after a space.
///
/// Derived from [`action_specs`] -- do not maintain a second list here.
pub fn command_names() -> Vec<(&'static str, Option<&'static str>)> {
    action_specs()
        .iter()
        .filter(|spec| spec.palette)
        .map(|spec| (spec.name, spec.hint))
        .collect()
}

// ---------------------------------------------------------------------------
// Session events
// ---------------------------------------------------------------------------

/// Asynchronous events that the server pushes to connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    SessionCreated(String),
    SessionDeleted(String),
    PaneExited { pane_id: PaneId, exit_code: i32 },
}

// ---------------------------------------------------------------------------
// Wire format helpers -- length-prefixed JSON over Unix sockets
//
// Frame layout: [4 bytes big-endian payload length][JSON payload]
// ---------------------------------------------------------------------------

/// Serialize a message into a length-prefixed JSON frame.
pub fn encode_message<T: Serialize>(msg: &T) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)?;
    log::trace!("protocol: encode_message bytes={}", json.len());
    let len = (json.len() as u32).to_be_bytes();
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Read the payload length from a 4-byte big-endian header.
pub fn decode_message_length(header: &[u8; 4]) -> usize {
    let len = u32::from_be_bytes(*header) as usize;
    log::trace!("protocol: decode_message_length bytes={}", len);
    len
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_client_message() {
        let msg = ClientMessage::Attach {
            session_name: "main".into(),
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::Attach { session_name } => assert_eq!(session_name, "main"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_server_message() {
        let msg = ServerMessage::Error {
            message: "not found".into(),
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ServerMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ServerMessage::Error { message } => assert_eq!(message, "not found"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn render_cell_width_defaults_to_one_when_absent() {
        // A peer that predates the `width` field omits it on the wire; it must
        // decode as normal (single-column) width via the serde default.
        let json = r#"{"c":"a","fg":"Default","bg":"Default","bold":false,"italic":false,"underline":false}"#;
        let cell: RenderCell = serde_json::from_str(json).unwrap();
        assert_eq!(cell.width, 1);

        // A present width field is preserved through a round trip.
        let wide = RenderCell {
            c: '中',
            width: 2,
            ..RenderCell::default()
        };
        let encoded = serde_json::to_string(&wide).unwrap();
        let decoded: RenderCell = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.width, 2);
    }

    #[test]
    fn round_trip_command() {
        let msg = ClientMessage::Command(RemuxCommand::TabNew);
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::Command(RemuxCommand::TabNew) => {}
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_explicit_target_commands() {
        // Each explicit-target command must survive a length-prefixed JSON
        // round trip wrapped in a ClientMessage::Command, exactly as it travels
        // on the wire between the session-manager client and the daemon.
        let cases = vec![
            RemuxCommand::SessionRenameByName {
                old: "old".into(),
                new: "new".into(),
            },
            RemuxCommand::FolderRename {
                old: "work".into(),
                new: "play".into(),
            },
            RemuxCommand::TabNewInSession {
                session: "main".into(),
            },
            RemuxCommand::TabRenameByIndex {
                session: "main".into(),
                tab_index: 2,
                name: "logs".into(),
            },
            RemuxCommand::PaneNewInTab {
                session: "main".into(),
                tab_index: 1,
            },
            RemuxCommand::PaneCloseById {
                session: "main".into(),
                pane_id: 42,
            },
            RemuxCommand::PaneRenameById {
                session: "main".into(),
                pane_id: 42,
                name: "editor".into(),
            },
            RemuxCommand::TabMoveByIndex {
                session: "main".into(),
                tab_index: 3,
                delta: -1,
            },
        ];

        for cmd in cases {
            let msg = ClientMessage::Command(cmd.clone());
            let encoded = encode_message(&msg).unwrap();
            let len = decode_message_length(encoded[..4].try_into().unwrap());
            let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
            match decoded {
                ClientMessage::Command(decoded_cmd) => assert_eq!(decoded_cmd, cmd),
                other => panic!("unexpected variant: {other:?}"),
            }
        }
    }

    #[test]
    fn encode_length_is_correct() {
        let msg = ClientMessage::Detach;
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        assert_eq!(len, encoded.len() - 4);
    }

    #[test]
    fn render_cell_default() {
        let cell = RenderCell::default();
        assert_eq!(cell.c, ' ');
        assert_eq!(cell.fg, CellColor::Default);
        assert!(!cell.bold);
        assert!(cell.combining.is_empty());
    }

    #[test]
    fn render_cell_empty_combining_is_not_serialized() {
        // The overwhelmingly common case (no combining marks) must add zero wire
        // bytes: `skip_serializing_if` omits the field from the JSON entirely.
        let cell = RenderCell::default();
        let json = serde_json::to_string(&cell).unwrap();
        assert!(
            !json.contains("combining"),
            "empty combining must be skipped, got: {json}"
        );

        // Absent on the wire decodes back to an empty vec via serde default.
        let decoded: RenderCell = serde_json::from_str(&json).unwrap();
        assert!(decoded.combining.is_empty());
    }

    #[test]
    fn render_cell_combining_round_trips() {
        // A decomposed `é` (base 'e' + U+0301) round-trips with its marks.
        let cell = RenderCell {
            c: 'e',
            combining: vec!['\u{301}'],
            ..RenderCell::default()
        };
        let json = serde_json::to_string(&cell).unwrap();
        assert!(json.contains("combining"));
        let decoded: RenderCell = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.c, 'e');
        assert_eq!(decoded.combining, vec!['\u{301}']);
    }

    #[test]
    fn round_trip_mouse_click() {
        let msg = ClientMessage::MouseClick {
            x: 42,
            y: 10,
            pane_id: None,
            release: false,
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::MouseClick {
                x,
                y,
                pane_id,
                release,
            } => {
                assert_eq!(x, 42);
                assert_eq!(y, 10);
                assert_eq!(pane_id, None);
                assert!(!release);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// The button-up half of a click round-trips, and the wheel carries the
    /// position a mouse report needs. Both are `#[serde(default)]` additions, so
    /// a payload from an older peer still decodes -- to the press-at-origin
    /// meaning it always had, which is what keeps them off `PROTOCOL_VERSION`.
    #[test]
    fn mouse_release_and_wheel_position_round_trip_and_default() {
        let msg = ClientMessage::MouseClick {
            x: 4,
            y: 5,
            pane_id: Some(9),
            release: true,
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        match serde_json::from_slice::<ClientMessage>(&encoded[4..4 + len]).unwrap() {
            ClientMessage::MouseClick { release, .. } => assert!(release),
            other => panic!("unexpected variant: {other:?}"),
        }
        let legacy = br#"{"MouseClick":{"x":1,"y":2,"pane_id":3}}"#;
        match serde_json::from_slice::<ClientMessage>(legacy).unwrap() {
            ClientMessage::MouseClick { release, .. } => assert!(!release),
            other => panic!("unexpected variant: {other:?}"),
        }

        let msg = ClientMessage::ScrollPane {
            pane_id: 3,
            up: true,
            lines: 3,
            x: 11,
            y: 7,
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        match serde_json::from_slice::<ClientMessage>(&encoded[4..4 + len]).unwrap() {
            ClientMessage::ScrollPane { x, y, .. } => assert_eq!((x, y), (11, 7)),
            other => panic!("unexpected variant: {other:?}"),
        }
        let legacy = br#"{"ScrollPane":{"pane_id":3,"up":false,"lines":3}}"#;
        match serde_json::from_slice::<ClientMessage>(legacy).unwrap() {
            ClientMessage::ScrollPane { x, y, .. } => assert_eq!((x, y), (0, 0)),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// A pane-scoped gesture round-trips its target, and a payload written by an
    /// older peer (no `pane_id` at all) still decodes -- as the session-scoped
    /// `None`, which is what keeps the change off `PROTOCOL_VERSION`.
    #[test]
    fn mouse_pane_id_round_trips_and_defaults() {
        let msg = ClientMessage::MouseDrag {
            start_x: 1,
            start_y: 2,
            end_x: 3,
            end_y: 4,
            is_final: true,
            pane_id: Some(77),
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::MouseDrag { pane_id, .. } => assert_eq!(pane_id, Some(77)),
            other => panic!("unexpected variant: {other:?}"),
        }

        let legacy = br#"{"MouseClick":{"x":1,"y":2}}"#;
        match serde_json::from_slice::<ClientMessage>(legacy).unwrap() {
            ClientMessage::MouseClick { pane_id, .. } => assert_eq!(pane_id, None),
            other => panic!("unexpected variant: {other:?}"),
        }
        let legacy =
            br#"{"MouseDrag":{"start_x":1,"start_y":2,"end_x":3,"end_y":4,"is_final":false}}"#;
        match serde_json::from_slice::<ClientMessage>(legacy).unwrap() {
            ClientMessage::MouseDrag { pane_id, .. } => assert_eq!(pane_id, None),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_mouse_drag() {
        let msg = ClientMessage::MouseDrag {
            start_x: 5,
            start_y: 3,
            end_x: 20,
            end_y: 7,
            is_final: false,
            pane_id: None,
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::MouseDrag {
                start_x,
                start_y,
                end_x,
                end_y,
                is_final,
                ..
            } => {
                assert_eq!(start_x, 5);
                assert_eq!(start_y, 3);
                assert_eq!(end_x, 20);
                assert_eq!(end_y, 7);
                assert!(!is_final);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_list_session_tree() {
        let msg = ClientMessage::ListSessionTree;
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        assert!(matches!(decoded, ClientMessage::ListSessionTree));
    }

    #[test]
    fn round_trip_subscribe_session_tree() {
        for msg in [
            ClientMessage::SubscribeSessionTree,
            ClientMessage::UnsubscribeSessionTree,
        ] {
            let encoded = encode_message(&msg).unwrap();
            let len = decode_message_length(encoded[..4].try_into().unwrap());
            let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
            match (&msg, &decoded) {
                (ClientMessage::SubscribeSessionTree, ClientMessage::SubscribeSessionTree) => {}
                (ClientMessage::UnsubscribeSessionTree, ClientMessage::UnsubscribeSessionTree) => {}
                _ => panic!("round trip changed the variant: {decoded:?}"),
            }
        }
    }

    /// Both new variants are unit variants, so they ride the wire as bare
    /// JSON strings -- the shape the frame harness sends.
    #[test]
    fn subscribe_session_tree_is_a_bare_string_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&ClientMessage::SubscribeSessionTree).unwrap(),
            "\"SubscribeSessionTree\""
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::UnsubscribeSessionTree).unwrap(),
            "\"UnsubscribeSessionTree\""
        );
    }

    #[test]
    fn round_trip_session_tree() {
        let msg = ServerMessage::SessionTree {
            folders: vec![FolderTreeEntry {
                name: "work".to_string(),
                sessions: vec![SessionTreeEntry {
                    name: "proj".to_string(),
                    tabs: vec![TabTreeEntry {
                        is_active: true,
                        id: 1,
                        name: "Tab 1".to_string(),
                        panes: vec![PaneTreeEntry {
                            cwd: None,
                            id: 10,
                            name: "zsh".to_string(),
                            is_focused: true,
                        }],
                    }],
                    client_count: 1,
                    is_current: true,
                }],
            }],
            unfiled: vec![SessionTreeEntry {
                name: "scratch".to_string(),
                tabs: vec![],
                client_count: 0,
                is_current: false,
            }],
            dormant: vec!["saved-a".to_string()],
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ServerMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ServerMessage::SessionTree {
                folders,
                unfiled,
                dormant,
            } => {
                assert_eq!(folders.len(), 1);
                assert_eq!(folders[0].name, "work");
                assert_eq!(folders[0].sessions[0].name, "proj");
                assert!(folders[0].sessions[0].is_current);
                assert_eq!(unfiled.len(), 1);
                assert_eq!(unfiled[0].name, "scratch");
                assert_eq!(dormant, vec!["saved-a".to_string()]);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_session_switch_tab() {
        let msg = ClientMessage::Command(RemuxCommand::SessionSwitchTab {
            session: "main".to_string(),
            tab_index: 2,
        });
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::Command(RemuxCommand::SessionSwitchTab { session, tab_index }) => {
                assert_eq!(session, "main");
                assert_eq!(tab_index, 2);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_session_switch_pane() {
        let msg = ClientMessage::Command(RemuxCommand::SessionSwitchPane {
            session: "dev".to_string(),
            tab_index: 0,
            pane_id: 42,
        });
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::Command(RemuxCommand::SessionSwitchPane {
                session,
                tab_index,
                pane_id,
            }) => {
                assert_eq!(session, "dev");
                assert_eq!(tab_index, 0);
                assert_eq!(pane_id, 42);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn build_version_has_git_suffix() {
        let v = build_version();
        // The version carries a "+<build-id>" suffix so rebuilds are distinguishable.
        assert!(v.contains('+'), "build_version must contain '+': {v}");
        let suffix = v.split_once('+').map(|(_, s)| s).unwrap_or("");
        assert!(
            !suffix.is_empty(),
            "build id after '+' must be non-empty: {v}"
        );
    }

    #[test]
    fn round_trip_hello_welcome() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            remux_version: "1.2.3".into(),
        };
        let encoded = encode_message(&hello).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: Hello = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        assert_eq!(decoded.remux_version, "1.2.3");

        let welcome = Welcome {
            protocol_version: PROTOCOL_VERSION,
            remux_version: "1.2.3".into(),
        };
        let encoded = encode_message(&welcome).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: Welcome = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn round_trip_view_intents() {
        use crate::server::layout::FocusDirection;
        let cases = vec![
            ClientMessage::ViewCreate { name: "V1".into() },
            ClientMessage::ViewDelete { id: 7 },
            ClientMessage::ViewRename {
                id: 7,
                name: "V2".into(),
            },
            ClientMessage::ViewAddCells {
                id: 7,
                cells: vec![
                    (ConnDescriptor::Local, 3),
                    (ConnDescriptor::Remote("box".into()), 9),
                ],
            },
            ClientMessage::ViewRemoveCell { id: 7, cell_id: 2 },
            ClientMessage::ViewSetFocus { id: 7, cell_id: 2 },
            ClientMessage::ViewCycleLayout { id: 7 },
            ClientMessage::ViewToggleZoom { id: 7 },
            ClientMessage::ViewSetMaster { id: 7 },
            ClientMessage::ViewResizeCell {
                id: 7,
                cell_id: 2,
                dir: FocusDirection::Right,
                amount: 5,
            },
            ClientMessage::ViewMoveCell {
                id: 7,
                cell_id: 2,
                dir: FocusDirection::Down,
            },
        ];
        for msg in cases {
            let encoded = encode_message(&msg).unwrap();
            let len = decode_message_length(encoded[..4].try_into().unwrap());
            let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
            // Compare via debug string (ClientMessage isn't PartialEq).
            assert_eq!(format!("{decoded:?}"), format!("{msg:?}"));
        }
    }

    #[test]
    fn round_trip_view_list() {
        let msg = ServerMessage::ViewList {
            views: vec![ViewInfo {
                id: 1,
                name: "V1".into(),
                cells: vec![CellInfo {
                    id: 4,
                    conn: ConnDescriptor::Local,
                    pane_id: 12,
                }],
                layout: crate::server::layout::LayoutMode::Grid(crate::server::layout::GridLayout),
                custom_tree: None,
                focused: 0,
                zoomed: false,
            }],
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ServerMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ServerMessage::ViewList { views } => {
                assert_eq!(views.len(), 1);
                assert_eq!(views[0].name, "V1");
                assert_eq!(views[0].cells.len(), 1);
                assert_eq!(views[0].cells[0].id, 4);
                assert_eq!(views[0].cells[0].pane_id, 12);
                assert_eq!(views[0].cells[0].conn, ConnDescriptor::Local);
                assert_eq!(views[0].layout.name(), "grid");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_view_created() {
        let msg = ServerMessage::ViewCreated { id: 42 };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ServerMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        assert!(matches!(decoded, ServerMessage::ViewCreated { id: 42 }));
    }

    #[test]
    fn round_trip_copy_to_clipboard() {
        let msg = ServerMessage::CopyToClipboard {
            data: "hello world".to_string(),
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ServerMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ServerMessage::CopyToClipboard { data } => {
                assert_eq!(data, "hello world");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    // -- Action registry -------------------------------------------------------

    /// Classify every `RemuxCommand` variant: either it is bindable (and the
    /// registry name it is reached by) or it is a deliberate exclusion.
    ///
    /// The match has NO wildcard arm on purpose. Adding a `RemuxCommand`
    /// variant fails to compile here until someone decides which it is -- the
    /// compile-time half of the anti-drift guarantee (the runtime half lives in
    /// `config::keybindings::tests::every_registry_entry_resolves`).
    fn registry_name(cmd: &RemuxCommand) -> Option<&'static str> {
        match cmd {
            RemuxCommand::TabNew => Some("TabNew"),
            RemuxCommand::TabClose => Some("TabClose"),
            RemuxCommand::TabRename(_) => Some("TabRename"),
            RemuxCommand::TabGoto(_) => Some("TabGoto"),
            RemuxCommand::TabNext => Some("TabNext"),
            RemuxCommand::TabPrev => Some("TabPrev"),
            RemuxCommand::TabMove(_) => Some("TabMove"),
            RemuxCommand::PaneNew => Some("PaneNew"),
            RemuxCommand::PaneClose => Some("PaneClose"),
            RemuxCommand::PaneSplitVertical => Some("PaneSplitVertical"),
            RemuxCommand::PaneSplitHorizontal => Some("PaneSplitHorizontal"),
            RemuxCommand::PaneFocusLeft => Some("PaneFocusLeft"),
            RemuxCommand::PaneFocusRight => Some("PaneFocusRight"),
            RemuxCommand::PaneFocusUp => Some("PaneFocusUp"),
            RemuxCommand::PaneFocusDown => Some("PaneFocusDown"),
            RemuxCommand::PaneStackAdd => Some("PaneStackAdd"),
            RemuxCommand::PaneStackNext => Some("PaneStackNext"),
            RemuxCommand::PaneStackPrev => Some("PaneStackPrev"),
            RemuxCommand::PaneMoveLeft => Some("PaneMoveLeft"),
            RemuxCommand::PaneMoveRight => Some("PaneMoveRight"),
            RemuxCommand::PaneMoveUp => Some("PaneMoveUp"),
            RemuxCommand::PaneMoveDown => Some("PaneMoveDown"),
            RemuxCommand::PaneRename(_) => Some("PaneRename"),
            RemuxCommand::PaneToggleZoom => Some("PaneToggleZoom"),
            RemuxCommand::PopupToggle => Some("PopupToggle"),
            RemuxCommand::ResizeLeft(_) => Some("ResizeLeft"),
            RemuxCommand::ResizeRight(_) => Some("ResizeRight"),
            RemuxCommand::ResizeUp(_) => Some("ResizeUp"),
            RemuxCommand::ResizeDown(_) => Some("ResizeDown"),
            RemuxCommand::SessionNew { .. } => Some("SessionNew"),
            RemuxCommand::SessionDetach => Some("SessionDetach"),
            RemuxCommand::SessionRename(_) => Some("SessionRename"),
            RemuxCommand::SessionList => Some("SessionList"),
            RemuxCommand::SessionSave => Some("SessionSave"),
            RemuxCommand::SessionSwitchLast => Some("SessionSwitchLast"),
            RemuxCommand::SessionMoveToFolder => Some("SessionMoveToFolder"),
            RemuxCommand::FolderNew(_) => Some("FolderNew"),
            RemuxCommand::FolderDelete(_) => Some("FolderDelete"),
            RemuxCommand::FolderList => Some("FolderList"),
            RemuxCommand::FolderMoveSession { .. } => Some("FolderMoveSession"),
            RemuxCommand::BufferEditInEditor => Some("BufferEditInEditor"),
            RemuxCommand::OpenSessionManager => Some("OpenSessionManager"),
            RemuxCommand::RemoteConnect(_) => Some("RemoteConnect"),
            RemuxCommand::ToggleStyle => Some("ToggleStyle"),
            RemuxCommand::LayoutNext => Some("LayoutNext"),
            RemuxCommand::SetMaster => Some("SetMaster"),
            RemuxCommand::EnterNormal => Some("EnterNormal"),
            RemuxCommand::EnterCommandMode => Some("EnterCommandMode"),
            RemuxCommand::EnterVisualMode => Some("EnterVisualMode"),
            RemuxCommand::EnterSearchMode => Some("EnterSearchMode"),
            RemuxCommand::SendKey(_) => Some("SendKey"),

            // Deliberate exclusions: explicit-target commands the session
            // manager issues internally. A binding string cannot supply their
            // structural arguments, so they are not bindable.
            RemuxCommand::SessionSwitchTab { .. }
            | RemuxCommand::SessionSwitchPane { .. }
            | RemuxCommand::TabCloseByIndex { .. }
            | RemuxCommand::SessionRenameByName { .. }
            | RemuxCommand::FolderRename { .. }
            | RemuxCommand::TabNewInSession { .. }
            | RemuxCommand::TabRenameByIndex { .. }
            | RemuxCommand::PaneNewInTab { .. }
            | RemuxCommand::PaneCloseById { .. }
            | RemuxCommand::PaneRenameById { .. }
            | RemuxCommand::TabMoveByIndex { .. } => None,
        }
    }

    /// Every server entry in the registry builds the command that classifies
    /// back to that same entry -- the registry and the enum agree on names.
    #[test]
    fn registry_server_entries_match_their_command_variant() {
        for spec in action_specs().iter().filter(|s| s.client.is_none()) {
            let input = match spec.sample_args {
                Some(args) => format!("{} {}", spec.name, args),
                None => spec.name.to_string(),
            };
            let cmd = match crate::config::keybindings::resolve_action(&input) {
                Some(Action::Server(cmd)) => cmd,
                other => panic!("registry entry '{}' resolved to {other:?}", spec.name),
            };
            assert_eq!(
                registry_name(&cmd),
                Some(spec.name),
                "registry entry '{}' builds a command classified elsewhere",
                spec.name
            );
        }
    }

    /// Nothing bindable is missing from the registry: a variant classified as
    /// bindable must be listed.
    #[test]
    fn classified_names_are_registry_names() {
        for cmd in [
            RemuxCommand::EnterSearchMode,
            RemuxCommand::PopupToggle,
            RemuxCommand::SendKey(vec![1]),
            RemuxCommand::SessionSwitchLast,
        ] {
            let name = registry_name(&cmd).expect("classified as bindable");
            assert!(
                action_spec(name).is_some(),
                "'{name}' is classified as bindable but missing from the registry"
            );
        }
        // ... and an excluded variant stays out of it.
        assert_eq!(
            registry_name(&RemuxCommand::PaneCloseById {
                session: "s".into(),
                pane_id: 1
            }),
            None
        );
    }
}
