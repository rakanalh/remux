//! Server daemon implementation.
//!
//! This module provides the Remux daemon process, Unix socket communication
//! helpers, and the main server event loop.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use crate::config::{BorderStyle, Config};
use crate::protocol;
use crate::protocol::*;
use crate::screen::Screen;
use crate::server::compositor::{
    composite, fits_zellij_border, hit_test, is_multi_stack, pane_content_rect, ClickTarget,
    HitRegions, MouseSelection, StatusInfo,
};
use crate::server::layout::{
    self, BspLayout, CustomLayout, LayoutMode, LayoutNode, MasterLayout, PaneId, Rect,
};
use crate::server::persistence::{self, PersistedState};
use crate::server::pty::{self, Pty};
use crate::server::session::{self, Folder, ServerState, Session};

/// In-memory store of dormant (saved-but-not-live) sessions.
///
/// Populated at startup when `save_sessions = true` and
/// `automatic_restore = false`: the persisted snapshot is loaded here instead
/// of being brought live. Sessions migrate out of this store (into live
/// `ServerState`) when the client resurrects them. Empty in every other mode,
/// so the merge-on-save is a no-op on the default path.
pub type DormantStore = Arc<Mutex<PersistedState>>;

/// Type alias for the per-client previous-frame cache used for diff rendering.
///
/// Keyed by `client_id` (not session name): each client is diffed against what
/// *it* last displayed. Multiple clients of different sizes can attach to one
/// session, so a shared session-keyed baseline would poison diffs across
/// differently-sized clients. All render paths composite at one consistent
/// session render size (min over attached clients), so a given client never
/// mixes differently-sized frames.
pub type PrevFrameCache = Arc<Mutex<HashMap<u64, Vec<Vec<RenderCell>>>>>;

/// Read the process name from `/proc/<pid>/comm`.
///
/// Falls back to `"shell"` if the file is unreadable.
fn get_process_name(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "shell".to_string())
}

/// Return the runtime directory used for the socket and pid files.
fn runtime_dir() -> PathBuf {
    dirs::runtime_dir()
        .or_else(|| std::env::var("XDG_RUNTIME_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Return the path to the Unix domain socket used for client-server
/// communication.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("remux.sock")
}

/// Return the path to the pid file recording the running server's PID.
///
/// Written on startup and removed on graceful shutdown; used by `remux stop`
/// to signal the server (SIGTERM) for a clean save-and-exit.
pub fn pid_path() -> PathBuf {
    runtime_dir().join("remux.pid")
}

/// Write a length-prefixed JSON message to an async writer.
pub async fn write_message<W, T>(writer: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let frame = protocol::encode_message(msg)?;
    writer
        .write_all(&frame)
        .await
        .context("writing message frame")?;
    writer.flush().await.context("flushing writer")?;
    Ok(())
}

/// Read a length-prefixed JSON message from an async reader.
///
/// Returns `Ok(None)` if the connection was closed (EOF on the length header).
pub async fn read_message<T>(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("reading message header"),
    }

    let len = protocol::decode_message_length(&header);
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("reading message payload")?;

    let msg: T = serde_json::from_slice(&payload).context("deserializing message")?;
    Ok(Some(msg))
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Data associated with a single pane: its PTY and screen buffer.
struct PaneData {
    pty: Pty,
    screen: Screen,
    /// Receiving end for PTY output from the background reader task.
    pty_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// True once a PTY-forwarding task has been spawned for this pane.
    /// start_pty_forwarding is called from many sites (attach, session/tab
    /// switches); without this guard each call would spawn a competing task
    /// and chunks could be processed out of order, corrupting the stream.
    forwarding_started: bool,
    /// The `session_visible` flag last streamed to this pane's View-cell
    /// subscribers. Tracked so a visibility flip (tab switch / attach / detach)
    /// that does NOT change the pane's size still pushes a fresh `PaneContent`
    /// (so cells flip live between the "Active in session" placeholder and the
    /// streamed content), while steady state stays quiet.
    streamed_session_visible: bool,
}

// MouseSelection is imported from compositor.

/// An in-progress mouse drag-selection gesture for a client.
///
/// The gesture's anchor (where the drag started) is stored in *absolute*,
/// eviction-stable coordinates ([`Screen::abs_of_row`]) rather than viewport
/// rows, so that when the view auto-scrolls into scrollback during the drag the
/// anchor stays pinned to the same logical buffer line instead of drifting. The
/// viewport-relative `MouseSelection` is re-derived from this each event.
struct DragSession {
    /// Pane the gesture belongs to; a drag into a different pane starts fresh.
    pane_id: PaneId,
    /// Anchor column in the pane's content area.
    anchor_col: u16,
    /// Anchor row as a stable absolute line id.
    anchor_abs: usize,
    /// The moving end of the selection, in the same absolute, eviction-stable
    /// coordinates as the anchor. Persisted (unlike the old per-call local) so a
    /// wheel-scroll during the drag can extend the selection to the newly
    /// revealed edge without a fresh mouse event -- keeping the highlight and the
    /// yankable range derived from one absolute range.
    end_abs: usize,
    /// The moving end's column in the pane's content area.
    end_col: u16,
}

/// A connected client with metadata about which session it is attached to.
struct ClientConnection {
    session_name: Option<String>,
    /// Sender to push `ServerMessage`s to this client's writer task.
    tx: mpsc::UnboundedSender<ServerMessage>,
    cols: u16,
    rows: u16,
    /// The client's current input mode (e.g. "NORMAL", "COMMAND", "VISUAL").
    mode: String,
    /// Active mouse selection, if any.
    mouse_selection: Option<MouseSelection>,
    /// Search match info: (current_match, total_matches).
    search_info: Option<(usize, usize)>,
    /// Scroll offset for the focused pane (0 = live view, >0 = scrolled back).
    scroll_offset: usize,
    /// Previous scroll_offset, for detecting scroll delta.
    prev_scroll_offset: usize,
    /// Set when the client's scroll offset changed; forces the next broadcast
    /// to send a FullRender so the diff baseline can't desync from the client's
    /// screen across a scroll transition.
    needs_full_render: bool,
    /// In-progress mouse drag-selection gesture, if any. Tracks the drag anchor
    /// in eviction-stable absolute coordinates so edge auto-scroll doesn't drift.
    drag: Option<DragSession>,
    /// Armed when a drag is resting on a scrollable content edge: stores the last
    /// drag coordinates `(start_x, start_y, end_x, end_y)` so the per-client
    /// reader loop can replay the edge-scroll step on a repeating timer while the
    /// pointer is held still (terminals stop emitting drag events without motion).
    /// `None` disarms the timer -- set at construction, on click, on release, and
    /// whenever the scroll reaches its bound.
    autoscroll_repeat: Option<(u16, u16, u16, u16)>,
    /// Panes this client has subscribed to via `SubscribePane`, mapped to the
    /// subscribing cell's size demand: `Some((cols, rows))` for a cell that
    /// SHOWS the pane and so demands it reflow to fit, or `None` for a
    /// watch-only subscription that imposes no size constraint (a cell hidden by
    /// the view's layout, a session-visible pane, or a plain observer).
    /// Folded into the pane's min-across-viewers effective size by
    /// [`recompute_pane_size`]. Each subscribed pane receives a `PaneContent`
    /// snapshot on subscribe and on every change, regardless of which
    /// session/tab this client has in the foreground.
    subscribed_panes: std::collections::HashMap<PaneId, Option<(u16, u16)>>,
    /// Per-(this client, pane) scroll offset into a subscribed pane's scrollback,
    /// driven by `ScrollPane` (a View cell's mouse wheel). Independent of the
    /// foreground `scroll_offset` and of other clients viewing the same pane.
    /// `0`/absent = live view. Cleared on `UnsubscribePane` and pane close.
    pane_scroll: std::collections::HashMap<PaneId, usize>,
    /// Per-(this client, pane) drag-selection over a subscribed pane, in the
    /// pane's own content coordinates. The View-cell analog of
    /// `mouse_selection`: a client displaying a view is detached, so the
    /// session-scoped selection has no pane to attach to. Rendered into the
    /// per-subscriber `PaneContent` by [`stream_pane_content`].
    pane_selection: std::collections::HashMap<PaneId, MouseSelection>,
    /// In-progress pane-scoped drag gesture (a View cell), if any. Separate from
    /// `drag` so a session drag and a cell drag can never be confused for one
    /// another; the anchor is likewise kept in eviction-stable absolute
    /// coordinates so cell edge auto-scroll doesn't drift it.
    pane_drag: Option<DragSession>,
    /// The pane-scoped analog of `autoscroll_repeat`: `(pane_id, start_x,
    /// start_y, end_x, end_y)` in the pane's content coordinates, replayed by
    /// the ticker task while a cell drag rests on a scrollable content edge.
    pane_autoscroll_repeat: Option<(PaneId, u16, u16, u16, u16)>,
    /// Set by `SubscribeSessionTree`: this client receives an unsolicited
    /// [`ServerMessage::SessionTree`] whenever the structure changes, instead
    /// of polling with `ListSessionTree`.
    ///
    /// Per-connection and independent of attachment, exactly like
    /// [`ClientConnection::subscribed_panes`]. Living here rather than in a
    /// side table is what makes disconnect cleanup automatic: dropping the
    /// `ClientConnection` in [`handle_client_disconnect`] drops the
    /// subscription with it, so there is no second registry to leak.
    session_tree_subscribed: bool,
}

/// The Remux server.
pub struct RemuxServer {
    state: Arc<Mutex<ServerState>>,
    panes: Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: Arc<Config>,
    clients: Arc<Mutex<HashMap<u64, ClientConnection>>>,
    /// Monotonically increasing counter for stable client IDs.
    next_client_id: Arc<AtomicU64>,
    /// Previous composite frame per client (keyed by `client_id`), for diff
    /// computation. See [`PrevFrameCache`].
    prev_frames: Arc<Mutex<HashMap<u64, Vec<Vec<RenderCell>>>>>,
    /// Dormant (saved-but-not-live) sessions awaiting resurrection. See
    /// [`DormantStore`].
    dormant: DormantStore,
}

// ---------------------------------------------------------------------------
// Server implementation
// ---------------------------------------------------------------------------

impl RemuxServer {
    /// Create a new server instance.
    fn new(config: Config) -> Self {
        Self {
            state: Arc::new(Mutex::new(ServerState::new())),
            panes: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
            clients: Arc::new(Mutex::new(HashMap::new())),
            next_client_id: Arc::new(AtomicU64::new(0)),
            prev_frames: Arc::new(Mutex::new(HashMap::new())),
            dormant: Arc::new(Mutex::new(PersistedState {
                state: ServerState::new(),
                pane_cwds: HashMap::new(),
            })),
        }
    }

    /// Start the server: bind socket, accept connections, run the event loop.
    pub async fn run(config: Config) -> Result<()> {
        let server = Self::new(config);

        // Load persisted state before accepting connections. Behavior depends
        // on two config flags:
        //   save_sessions=false            -> no persistence at all (skip load).
        //   save_sessions, automatic_restore -> bring persisted sessions live.
        //   save_sessions, !automatic_restore -> load persisted sessions as
        //                                        dormant/resurrectable instead.
        if server.config.general.save_sessions {
            match crate::server::persistence::load_state() {
                Ok(Some(persisted)) => {
                    if server.config.general.automatic_restore {
                        log::info!("restoring persisted state");
                        if let Err(e) = restore_state(&server, persisted).await {
                            log::warn!("failed to restore state: {e}, starting fresh");
                        }
                    } else {
                        let count = persisted.state.sessions.len();
                        log::info!(
                            "automatic_restore disabled: loaded {count} session(s) as dormant"
                        );
                        // Reserve live id space above the whole dormant id range
                        // so sessions created before a resurrect never collide
                        // with dormant pane/tab ids in the global pane map.
                        {
                            let mut st = server.state.lock().await;
                            st.reserve_ids_above(&persisted.state);
                        }
                        let mut d = server.dormant.lock().await;
                        *d = persisted;
                    }
                }
                Ok(None) => {
                    log::info!("no persisted state found, starting fresh");
                }
                Err(e) => {
                    log::warn!("failed to load persisted state: {e}, starting fresh");
                }
            }
        } else {
            log::info!("save_sessions disabled: persistence is off");
        }

        let path = socket_path();
        // Create parent directory if needed.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating socket directory")?;
        }
        // Remove stale socket if present.
        let _ = std::fs::remove_file(&path);

        let listener = UnixListener::bind(&path).context("binding Unix listener")?;
        log::info!("server listening on {}", path.display());

        // Record our PID so `remux stop` can signal us for a graceful save-and-
        // exit. Best-effort: a write failure here must not abort startup.
        let pid_file = pid_path();
        match std::fs::write(&pid_file, std::process::id().to_string()) {
            Ok(()) => log::info!("wrote pid file {}", pid_file.display()),
            Err(e) => log::warn!("failed to write pid file {}: {e}", pid_file.display()),
        }

        // Set up signal handlers for graceful shutdown.
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("registering SIGTERM handler")?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("registering SIGINT handler")?;

        // Session-tree push task. Sleeping a full interval after each broadcast
        // is the coalescing: every change arriving during that sleep collapses
        // into the single permit `Notify` holds, so a burst of structural
        // commands costs subscribers one extra push rather than one per
        // command. The first change after a quiet period goes out immediately
        // (the task is parked on `notified()`, which consumes the permit at
        // once), so the common case is not delayed. Idle costs nothing.
        {
            let state = Arc::clone(&server.state);
            let panes = Arc::clone(&server.panes);
            let clients = Arc::clone(&server.clients);
            let dormant = Arc::clone(&server.dormant);
            tokio::spawn(async move {
                loop {
                    SESSION_TREE_DIRTY.notified().await;
                    broadcast_session_tree(&state, &panes, &clients, &dormant).await;
                    tokio::time::sleep(SESSION_TREE_PUSH_INTERVAL).await;
                }
            });
        }

        // Periodic tick that promotes quiet background `Activity` tabs to
        // `Silent` ("finished"). Runs in the real server binary, so
        // `Instant::now()`/`interval` are unrestricted.
        const SILENCE_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(3);
        let mut activity_tick = tokio::time::interval(std::time::Duration::from_secs(1));

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            server.handle_new_connection(stream).await;
                        }
                        Err(e) => {
                            log::error!("accept error: {e}");
                        }
                    }
                }
                _ = activity_tick.tick() => {
                    // Scan for background tabs that have gone silent past the
                    // threshold. Only broadcast to sessions that actually
                    // changed, so idle clients aren't woken every second.
                    let affected = {
                        let mut st = server.state.lock().await;
                        st.promote_silent_tabs(std::time::Instant::now(), SILENCE_THRESHOLD)
                    };
                    for session_name in affected {
                        broadcast_full_render(
                            &session_name,
                            &server.state,
                            &server.panes,
                            &server.clients,
                            &server.config,
                            &server.prev_frames,
                        )
                        .await;
                    }
                }
                _ = sigterm.recv() => {
                    log::info!("received SIGTERM, shutting down");
                    break;
                }
                _ = sigint.recv() => {
                    log::info!("received SIGINT, shutting down");
                    break;
                }
            }
        }

        // Graceful shutdown: clean up resources.
        server.shutdown(&path).await;
        Ok(())
    }

    /// Perform graceful shutdown: save state, drop panes, remove socket.
    async fn shutdown(&self, socket_path: &std::path::Path) {
        // Persistence is fully off when save_sessions is disabled.
        if !self.config.general.save_sessions {
            log::info!("save_sessions disabled: skipping state save on shutdown");
            let _ = std::fs::remove_file(socket_path);
            let _ = std::fs::remove_file(pid_path());
            log::info!("shutdown complete");
            return;
        }

        log::info!("saving state before shutdown...");

        // Save persistent state.
        let state = self.state.lock().await;
        let panes = self.panes.lock().await;

        let mut pane_cwds = std::collections::HashMap::new();
        for (&pane_id, pane_data) in panes.iter() {
            if let Some(cwd) = crate::server::persistence::get_pane_cwd(pane_data.pty.child_pid) {
                pane_cwds.insert(pane_id, cwd);
            }
        }

        if let Ok(mut persisted) =
            crate::server::persistence::PersistedState::from_server(&state, &pane_cwds)
        {
            // Persist live + still-dormant sessions so a live-only save never
            // clobbers un-resurrected dormant sessions on disk.
            {
                let dormant = self.dormant.lock().await;
                merge_dormant_into(&mut persisted, &dormant);
            }
            if let Err(e) = crate::server::persistence::save_state(&persisted) {
                log::error!("failed to save state on shutdown: {e}");
            } else {
                log::info!("state saved successfully");
            }
        }

        // Drop locks before cleanup.
        drop(state);
        drop(panes);

        // Remove socket and pid files.
        let _ = std::fs::remove_file(socket_path);
        let _ = std::fs::remove_file(pid_path());
        log::info!("shutdown complete");
    }

    /// Handle a newly accepted client connection.
    async fn handle_new_connection(&self, stream: tokio::net::UnixStream) {
        let (mut read_half, mut write_half) = stream.into_split();

        // Version handshake: exchange Hello/Welcome as the first frames, directly
        // on the split halves before wiring up the ServerMessage channel or
        // spawning the reader/writer tasks. Bounded by a timeout so a silent peer
        // cannot hold this open indefinitely. The server is lenient about version
        // skew (it logs and proceeds); the client decides whether to abort.
        let handshake = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let hello: Hello = read_message(&mut read_half)
                .await?
                .context("client closed connection during handshake")?;
            log::info!(
                "server: handshake from remux {} protocol v{}",
                hello.remux_version,
                hello.protocol_version
            );
            if hello.protocol_version != PROTOCOL_VERSION {
                log::warn!(
                    "server: protocol version mismatch (client v{}, server v{}); proceeding leniently",
                    hello.protocol_version,
                    PROTOCOL_VERSION
                );
            }
            let welcome = Welcome {
                protocol_version: PROTOCOL_VERSION,
                remux_version: crate::protocol::build_version(),
            };
            write_message(&mut write_half, &welcome).await?;
            anyhow::Ok(())
        })
        .await;
        match handshake {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::warn!("server: handshake failed: {e}; dropping connection");
                return;
            }
            Err(_) => {
                log::warn!("server: handshake timed out; dropping connection");
                return;
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

        let client_id = {
            let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
            let mut clients = self.clients.lock().await;
            clients.insert(
                id,
                ClientConnection {
                    session_name: None,
                    tx,
                    cols: 80,
                    rows: 24,
                    mode: "NORMAL".to_string(),
                    mouse_selection: None,
                    search_info: None,
                    scroll_offset: 0,
                    prev_scroll_offset: 0,
                    needs_full_render: false,
                    drag: None,
                    autoscroll_repeat: None,
                    subscribed_panes: std::collections::HashMap::new(),
                    pane_scroll: std::collections::HashMap::new(),
                    pane_selection: std::collections::HashMap::new(),
                    pane_drag: None,
                    pane_autoscroll_repeat: None,
                    session_tree_subscribed: false,
                },
            );
            log::debug!("server: new client connection, assigned client_id={id}");
            id
        };

        // Spawn writer task.
        let mut writer = write_half;
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Err(e) = write_message(&mut writer, &msg).await {
                    log::debug!("client writer error: {e}");
                    break;
                }
            }
        });

        // Initial view sync: a freshly connected client immediately learns the
        // current shared views, so a brand-new terminal is in sync with the
        // shared registry before it does anything. Sent unconditionally (an
        // empty registry sends an empty `ViewList`, which is still correct
        // "nothing here" information a client can rely on).
        {
            let st = self.state.lock().await;
            let msg = build_view_list_message(&st);
            drop(st);
            let cls = self.clients.lock().await;
            if let Some(conn) = cls.get(&client_id) {
                let _ = conn.tx.send(msg);
            }
        }

        // Spawn reader task.
        let state = Arc::clone(&self.state);
        let panes = Arc::clone(&self.panes);
        let clients = Arc::clone(&self.clients);
        let config = Arc::clone(&self.config);
        let prev_frames = Arc::clone(&self.prev_frames);
        let dormant = Arc::clone(&self.dormant);

        // Clones for the drag-autoscroll ticker task (see below).
        let ts_state = Arc::clone(&self.state);
        let ts_panes = Arc::clone(&self.panes);
        let ts_clients = Arc::clone(&self.clients);
        let ts_config = Arc::clone(&self.config);
        let ts_prev_frames = Arc::clone(&self.prev_frames);

        tokio::spawn(async move {
            let mut reader = read_half;
            loop {
                match read_message::<ClientMessage>(&mut reader).await {
                    Ok(Some(msg)) => {
                        if let Err(e) = handle_client_message(
                            client_id,
                            msg,
                            &state,
                            &panes,
                            &clients,
                            &config,
                            &prev_frames,
                            &dormant,
                        )
                        .await
                        {
                            log::error!("error handling client message: {e}");
                            let cls = clients.lock().await;
                            if let Some(client) = cls.get(&client_id) {
                                let _ = client.tx.send(ServerMessage::Error {
                                    message: format!("{e}"),
                                });
                            }
                        }
                    }
                    Ok(None) => {
                        log::info!("client {client_id} disconnected");
                        handle_client_disconnect(client_id, &clients, &prev_frames).await;
                        // A hard disconnect of the client that made a pane
                        // session-visible must flip any View cell on that pane
                        // from the "Active in session" placeholder to live
                        // content; the two `handle_detach` sites don't cover this.
                        refresh_subscribed_panes(&state, &panes, &clients, &config).await;
                        break;
                    }
                    Err(e) => {
                        log::error!("error reading from client {client_id}: {e}");
                        handle_client_disconnect(client_id, &clients, &prev_frames).await;
                        // A hard disconnect of the client that made a pane
                        // session-visible must flip any View cell on that pane
                        // from the "Active in session" placeholder to live
                        // content; the two `handle_detach` sites don't cover this.
                        refresh_subscribed_panes(&state, &panes, &clients, &config).await;
                        break;
                    }
                }
            }
        });

        // Spawn the drag-autoscroll ticker task. When a drag rests still on a
        // scrollable content edge, terminals stop sending drag events, so nothing
        // in the reader loop would advance the scroll. This timer replays the
        // edge-scroll step at a constant rate while `autoscroll_repeat` is armed
        // (set/cleared by `handle_mouse_drag` / `handle_mouse_click`).
        //
        // This lives in its own task rather than a `tokio::select!` arm of the
        // reader loop on purpose: `read_message` reads a frame's header and
        // payload into locals across two awaits and is NOT cancel-safe, so if a
        // timer tick cancelled a partially-read frame the consumed bytes would be
        // lost and the protocol stream would desync. A separate task keeps the
        // reader loop's read future alive to completion. The task self-terminates
        // when the client is removed from the map on disconnect.
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(40));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // Read (and drop the lock before calling into handle_mouse_drag,
                // which locks `clients` itself). Break when the client is gone.
                let (armed, pane_armed) = {
                    let cls = ts_clients.lock().await;
                    match cls.get(&client_id) {
                        Some(c) => (c.autoscroll_repeat, c.pane_autoscroll_repeat),
                        None => break,
                    }
                };
                if let Some((sx, sy, ex, ey)) = armed {
                    if let Err(e) = handle_mouse_drag(
                        client_id,
                        sx,
                        sy,
                        ex,
                        ey,
                        false,
                        &ts_state,
                        &ts_panes,
                        &ts_clients,
                        &ts_config,
                        &ts_prev_frames,
                    )
                    .await
                    {
                        log::error!("autoscroll drag error: {e}");
                    }
                }
                // The same replay for a View cell's gesture: a drag resting on a
                // cell's content edge scrolls that cell's source pane. The two
                // are mutually exclusive in practice (a client is either
                // attached to a session or displaying a view), but each is armed
                // and disarmed by its own handler, so both are simply checked.
                if let Some((pane_id, sx, sy, ex, ey)) = pane_armed {
                    if let Err(e) = handle_pane_mouse_drag(
                        client_id,
                        pane_id,
                        sx,
                        sy,
                        ex,
                        ey,
                        false,
                        &ts_state,
                        &ts_panes,
                        &ts_clients,
                        &ts_config,
                    )
                    .await
                    {
                        log::error!("pane autoscroll drag error: {e}");
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn handle_client_message(
    client_id: u64,
    msg: ClientMessage,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    // Log a summary of every incoming client message.
    match &msg {
        ClientMessage::Input { data } => {
            log::debug!(
                "server: client_id={client_id} msg=Input({} bytes)",
                data.len()
            );
        }
        ClientMessage::ScrollDelta { delta } => {
            log::debug!("server: client_id={client_id} msg=ScrollDelta(delta={delta})");
        }
        ClientMessage::MouseDrag {
            start_x,
            start_y,
            end_x,
            end_y,
            is_final,
            pane_id,
        } => {
            log::debug!(
                "server: client_id={client_id} msg=MouseDrag(start=({start_x},{start_y}), end=({end_x},{end_y}), is_final={is_final}, pane_id={pane_id:?})"
            );
        }
        other => {
            log::debug!("server: client_id={client_id} msg={other:?}");
        }
    }

    match msg {
        ClientMessage::Attach { session_name } => {
            let result = handle_attach(
                client_id,
                &session_name,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            // The tree's `client_count` and `is_current` are derived from the
            // client map, so attaching changes what every subscriber renders --
            // not only what this client renders.
            mark_session_tree_dirty();
            result
        }
        ClientMessage::Detach => {
            handle_detach(client_id, clients).await;
            mark_session_tree_dirty();
            // Detaching may make this client's active-tab panes no longer
            // session-visible; re-evaluate subscribed panes so any View cell on
            // them flips from the "Active in session" placeholder to live content.
            refresh_subscribed_panes(state, panes, clients, config).await;
            Ok(())
        }
        ClientMessage::Input { data } => {
            handle_input(client_id, &data, state, panes, clients).await?;
            // Typing leaves scrollback, as it does in tmux and zellij. The
            // server owns `scroll_offset`, so the server ends it -- see
            // `snap_client_to_live_tail` for why the client cannot be trusted to
            // ask.
            snap_client_to_live_tail(client_id, state, panes, clients, config, prev_frames).await;
            Ok(())
        }
        ClientMessage::Resize { cols, rows } => {
            handle_resize(
                client_id,
                cols,
                rows,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await
        }
        ClientMessage::Command(cmd) => {
            handle_command(
                client_id,
                cmd,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await
        }
        ClientMessage::CreateSession { name, folder } => {
            let result = handle_create_session(
                client_id,
                &name,
                folder.as_deref(),
                state,
                panes,
                clients,
                config,
            )
            .await;
            save_if_enabled(state, panes, config, dormant).await;
            mark_session_tree_dirty();
            result
        }
        ClientMessage::ListSessions => handle_list_sessions(client_id, state, clients).await,
        ClientMessage::KillSession { name } => {
            let result = handle_kill_session(&name, state, panes, clients).await;
            save_if_enabled(state, panes, config, dormant).await;
            mark_session_tree_dirty();
            result
        }
        ClientMessage::ListSessionTree => {
            handle_list_session_tree(client_id, state, panes, clients, dormant).await
        }
        ClientMessage::SubscribeSessionTree => {
            {
                let mut cls = clients.lock().await;
                if let Some(conn) = cls.get_mut(&client_id) {
                    conn.session_tree_subscribed = true;
                }
            }
            // Answer at once, so a subscriber's panel is populated immediately
            // rather than staying blank until the next structural change.
            send_session_tree_to(&[client_id], state, panes, clients, dormant).await;
            Ok(())
        }
        ClientMessage::UnsubscribeSessionTree => {
            let mut cls = clients.lock().await;
            if let Some(conn) = cls.get_mut(&client_id) {
                conn.session_tree_subscribed = false;
            }
            Ok(())
        }
        ClientMessage::ResurrectSession { name } => {
            let result = handle_resurrect_session(
                &name,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            save_if_enabled(state, panes, config, dormant).await;
            mark_session_tree_dirty();
            result
        }
        ClientMessage::RequestScrollback => {
            handle_request_scrollback(client_id, state, panes, clients).await
        }
        ClientMessage::SearchInfo { current, total } => {
            handle_search_info(client_id, current, total, clients).await;
            Ok(())
        }
        ClientMessage::ModeChanged { mode } => {
            handle_mode_changed(client_id, &mode, state, panes, clients, config, prev_frames).await
        }
        // `pane_id: Some(..)` routes the gesture by pane identity in that
        // pane's own content coordinates (a View cell); `None` keeps the
        // original screen-coordinate, foreground-session path.
        ClientMessage::MouseClick {
            x,
            y,
            pane_id,
            release,
        } => match pane_id {
            Some(pid) => {
                handle_pane_mouse_click(client_id, pid, x, y, release, state, panes, clients).await
            }
            None => {
                handle_mouse_click(
                    client_id,
                    x,
                    y,
                    release,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                )
                .await
            }
        },
        ClientMessage::MouseDrag {
            start_x,
            start_y,
            end_x,
            end_y,
            is_final,
            pane_id,
        } => match pane_id {
            Some(pid) => {
                handle_pane_mouse_drag(
                    client_id, pid, start_x, start_y, end_x, end_y, is_final, state, panes,
                    clients, config,
                )
                .await
            }
            None => {
                handle_mouse_drag(
                    client_id,
                    start_x,
                    start_y,
                    end_x,
                    end_y,
                    is_final,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                )
                .await
            }
        },
        ClientMessage::ScrollDelta { delta } => {
            handle_scroll_delta(client_id, delta, state, panes, clients, config, prev_frames).await
        }
        ClientMessage::MouseScroll { x, y, up } => {
            handle_mouse_scroll(
                client_id,
                x,
                y,
                up,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await
        }
        ClientMessage::ScrollReset => {
            log::debug!("server: ScrollReset client_id={client_id}");
            {
                let mut cls = clients.lock().await;
                if let Some(client) = cls.get_mut(&client_id) {
                    client.scroll_offset = 0;
                    client.prev_scroll_offset = 0;
                    client.needs_full_render = true;
                }
            }
            let session_name = {
                let cls = clients.lock().await;
                cls.get(&client_id).and_then(|c| c.session_name.clone())
            };
            if let Some(session_name) = session_name {
                send_full_render_to_client(
                    client_id,
                    &session_name,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                )
                .await;
            }
            Ok(())
        }
        ClientMessage::RequestScrollbackInfo => {
            // Take `clients` and RELEASE it before touching `state`. Holding it
            // across the `state` lock would be a `clients -> state` order, and
            // every other path here -- `handle_list_sessions`, `handle_attach`,
            // `send_session_tree_to` -- takes `state -> clients`. Two tasks on
            // the two orders deadlock the whole daemon, and the session-tree
            // pusher now runs `state -> clients` on a background task whenever
            // anyone is subscribed, which a sidebar does permanently.
            let session_name = {
                let cls = clients.lock().await;
                cls.get(&client_id).and_then(|c| c.session_name.clone())
            };
            let focused_pane_id = match session_name {
                Some(ref sn) => {
                    let st = state.lock().await;
                    st.sessions
                        .get(sn)
                        .and_then(|sess| sess.tabs.get(sess.active_tab).map(|t| t.focused_pane))
                }
                None => None,
            };
            if let (Some(_sn), Some(fp)) = (session_name, focused_pane_id) {
                let total_lines = {
                    let ps = panes.lock().await;
                    ps.get(&fp).map(|p| p.screen.total_lines()).unwrap_or(0)
                };
                let cls = clients.lock().await;
                if let Some(client) = cls.get(&client_id) {
                    let _ = client
                        .tx
                        .send(ServerMessage::ScrollbackInfo { total_lines });
                }
            }
            Ok(())
        }
        ClientMessage::SubscribePane {
            pane_id,
            cols,
            rows,
            size_demand,
        } => {
            // Subscribing to a pane that is already gone gets an explicit answer,
            // never silence: the snapshot builder returns `None` for a missing
            // pane and the send below is simply skipped, so recording the
            // subscription would leave the cell on `waiting…` forever with no way
            // to learn the truth. Report the death instead and record nothing.
            if !panes.lock().await.contains_key(&pane_id) {
                log::info!("server: SubscribePane pane_id={pane_id} is gone; reporting PaneExited");
                let cls = clients.lock().await;
                if let Some(conn) = cls.get(&client_id) {
                    let _ = conn.tx.send(ServerMessage::Event(SessionEvent::PaneExited {
                        pane_id,
                        exit_code: EXIT_CODE_UNKNOWN,
                    }));
                }
                return Ok(());
            }
            // Record the subscriber's size demand (folded by min-across-viewers
            // sizing) and send an immediate snapshot.
            {
                let mut cls = clients.lock().await;
                if let Some(conn) = cls.get_mut(&client_id) {
                    let demand = if size_demand {
                        Some((cols, rows))
                    } else {
                        None
                    };
                    // A FRESH subscribe starts at the live view with nothing
                    // selected. A RE-subscribe of a pane this client already
                    // watches does not: the client re-subscribes its cells
                    // whenever the view's focus or geometry changes (the size
                    // demand follows focus), and resetting there would throw away
                    // a scroll position or a drag anchor mid-gesture just because
                    // the user clicked a cell.
                    if conn.subscribed_panes.insert(pane_id, demand).is_none() {
                        conn.pane_scroll.remove(&pane_id);
                        conn.pane_selection.remove(&pane_id);
                    }
                }
            }
            // Snapshot at whatever offset/selection this client now holds.
            stream_pane_content(pane_id, state, panes, clients).await;
            // The new/updated demand may shrink (or release) the pane's effective
            // size; recompute and re-stream if it changed.
            recompute_pane_size(pane_id, state, panes, clients, config).await;
            Ok(())
        }
        ClientMessage::UnsubscribePane { pane_id } => {
            {
                let mut cls = clients.lock().await;
                if let Some(conn) = cls.get_mut(&client_id) {
                    conn.subscribed_panes.remove(&pane_id);
                    conn.pane_scroll.remove(&pane_id);
                    conn.pane_selection.remove(&pane_id);
                    if conn.pane_drag.as_ref().map(|d| d.pane_id) == Some(pane_id) {
                        conn.pane_drag = None;
                        conn.pane_autoscroll_repeat = None;
                    }
                }
            }
            // Dropping a viewer may let the pane grow back; recompute.
            recompute_pane_size(pane_id, state, panes, clients, config).await;
            Ok(())
        }
        ClientMessage::InputToPane { pane_id, data } => {
            // Route input to a pane by identity (View cell), independent of this
            // client's foreground session/tab. No focus lookup -- the target is
            // explicit. We deliberately do NOT replicate handle_input's Ctrl+L
            // scrollback-clearing special case: that is foreground-clear-screen
            // UX and is not wanted for targeted cell input. The pane's own PTY
            // output will trigger the existing forwarding task, which fans out
            // PaneContent to subscribers, so no explicit broadcast is needed.
            let alive = {
                let ps = panes.lock().await;
                match ps.get(&pane_id) {
                    Some(pane_data) => {
                        if let Err(e) = pane_data.pty.write_input(&data) {
                            log::warn!("server: InputToPane pane_id={pane_id} write failed: {e}");
                        }
                        true
                    }
                    None => false,
                }
            };
            if !alive {
                // The pane is gone. Answering keeps the keystroke from vanishing
                // without a trace: the sender learns its cell is dead and stops
                // typing into the void.
                log::warn!("server: InputToPane pane_id={pane_id} dropped: pane is gone");
                let cls = clients.lock().await;
                if let Some(conn) = cls.get(&client_id) {
                    let _ = conn.tx.send(ServerMessage::Event(SessionEvent::PaneExited {
                        pane_id,
                        exit_code: EXIT_CODE_UNKNOWN,
                    }));
                }
            }
            Ok(())
        }
        ClientMessage::ScrollPane {
            pane_id,
            up,
            lines,
            x,
            y,
        } => {
            // A View cell's wheel takes the SAME routing decision the session
            // wheel takes (`handle_mouse_scroll`) -- the whole reason the wheel
            // did nothing over a cell running a mouse-aware application is that
            // this path used to skip it and scroll a scrollback the alternate
            // screen does not have.
            let copy_mode = client_in_copy_mode(clients, client_id).await;
            let (max_off, route) = {
                let ps = panes.lock().await;
                match ps.get(&pane_id) {
                    Some(pd) => (
                        pd.screen.max_scroll_offset(),
                        mouse_route(&pd.screen, MouseGesture::Wheel, copy_mode),
                    ),
                    // Pane gone: nothing to scroll.
                    None => return Ok(()),
                }
            };
            // Only a subscribed client may drive a pane it watches (the same
            // guard the scroll path below applies).
            {
                let cls = clients.lock().await;
                match cls.get(&client_id) {
                    Some(conn) if conn.subscribed_panes.contains_key(&pane_id) => {}
                    _ => return Ok(()),
                }
            }
            match route {
                MouseRoute::App { sgr, .. } => {
                    // Cell coordinates arrive content-relative; a report is
                    // 1-based.
                    let bytes = wheel_report(sgr, up, x + 1, y + 1);
                    log::debug!(
                        "server: ScrollPane->app client_id={client_id} pane_id={pane_id} sgr={sgr} up={up} col={} row={}",
                        x + 1,
                        y + 1
                    );
                    return write_to_pane(panes, pane_id, &bytes).await;
                }
                MouseRoute::AltArrows { app_cursor } => {
                    let bytes = alt_scroll_arrows(app_cursor, up, lines.max(1));
                    log::debug!(
                        "server: ScrollPane->alt-arrows client_id={client_id} pane_id={pane_id} app_cursor={app_cursor} up={up}"
                    );
                    return write_to_pane(panes, pane_id, &bytes).await;
                }
                MouseRoute::Remux { .. } => {}
            }
            // Per-(client, pane) scrollback for a View cell. Clamp to the pane's
            // max scroll offset, render a snapshot at the new offset, and send it
            // to THIS client only -- the offset is per-subscriber.
            let new_off = {
                let mut cls = clients.lock().await;
                match cls.get_mut(&client_id) {
                    // Only a subscribed client may scroll a pane it watches.
                    Some(conn) if conn.subscribed_panes.contains_key(&pane_id) => {
                        let cur = conn.pane_scroll.get(&pane_id).copied().unwrap_or(0);
                        let next = if up {
                            (cur + lines as usize).min(max_off)
                        } else {
                            cur.saturating_sub(lines as usize)
                        };
                        if next == 0 {
                            conn.pane_scroll.remove(&pane_id);
                        } else {
                            conn.pane_scroll.insert(pane_id, next);
                        }
                        next
                    }
                    _ => return Ok(()),
                }
            };
            // A live selection has to follow the scroll: extend it while a drag
            // is in flight, drop it otherwise (see
            // `rescope_pane_selection_on_scroll`).
            rescope_pane_selection_on_scroll(client_id, pane_id, up, new_off, panes, clients).await;
            // Repaint through the shared per-subscriber path so the snapshot is
            // rendered at THIS client's new offset WITH its selection applied --
            // a bespoke render here would silently drop the highlight. Other
            // subscribers re-render at their own unchanged state, so they simply
            // receive the frame they already had.
            stream_pane_content(pane_id, state, panes, clients).await;
            Ok(())
        }

        // -- Shared View intents --------------------------------------------
        // Each mutates the server-owned registry (behind the `state` lock) then
        // broadcasts the full `ViewList` to every connected client via the one
        // helper. `ViewCreate` additionally acks the creator directly.
        ClientMessage::ViewCreate { name } => {
            let id = {
                let mut st = state.lock().await;
                st.view_create(name)
            };
            {
                let cls = clients.lock().await;
                if let Some(conn) = cls.get(&client_id) {
                    let _ = conn.tx.send(ServerMessage::ViewCreated { id });
                }
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewDelete { id } => {
            {
                let mut st = state.lock().await;
                st.view_delete(id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewRename { id, name } => {
            {
                let mut st = state.lock().await;
                st.view_rename(id, name);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewAddCells { id, cells } => {
            {
                let mut st = state.lock().await;
                st.view_add_cells(id, cells);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewRemoveCell { id, cell_id } => {
            {
                let mut st = state.lock().await;
                st.view_remove_cell(id, cell_id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewSetFocus { id, cell_id } => {
            {
                let mut st = state.lock().await;
                st.view_set_focus(id, cell_id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewCycleLayout { id } => {
            {
                let mut st = state.lock().await;
                st.view_cycle_layout(id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewToggleZoom { id } => {
            {
                let mut st = state.lock().await;
                st.view_toggle_zoom(id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewSetMaster { id } => {
            {
                let mut st = state.lock().await;
                st.view_set_master(id);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewResizeCell {
            id,
            cell_id,
            dir,
            amount,
        } => {
            {
                let mut st = state.lock().await;
                st.view_resize_cell(id, cell_id, dir, amount);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
        ClientMessage::ViewMoveCell { id, cell_id, dir } => {
            let area = view_reference_area(clients).await;
            {
                let mut st = state.lock().await;
                st.view_move_cell(id, cell_id, dir, area);
            }
            broadcast_view_list(state, clients).await;
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_attach(
    client_id: u64,
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    {
        let st = state.lock().await;
        if !st.sessions.contains_key(session_name) {
            let cls = clients.lock().await;
            if let Some(client) = cls.get(&client_id) {
                let _ = client.tx.send(ServerMessage::Error {
                    message: format!("session '{}' not found", session_name),
                });
            }
            return Ok(());
        }
    }

    let (cols, rows) = {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            client.session_name = Some(session_name.to_string());
            (client.cols, client.rows)
        } else {
            return Ok(());
        }
    };

    log::debug!("server: client_id={client_id} attach session={session_name:?} dims={cols}x{rows}");

    // The active tab is now being viewed; clear any stale activity marker.
    {
        let mut st = state.lock().await;
        st.clear_active_tab_activity(session_name);
    }

    // Resize panes to match the attaching client's terminal dimensions.
    resize_session_panes(session_name, state, panes, clients, config).await?;

    // Invalidate every attached client's baseline: this client's attach may
    // change the session render size (min over clients), so all clients must
    // re-render full at the new size on their next frame.
    invalidate_session_baselines(session_name, clients, prev_frames).await;

    send_full_render_to_client(
        client_id,
        session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
    )
    .await;

    start_pty_forwarding(
        session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        dormant,
    )
    .await;
    Ok(())
}

async fn handle_detach(client_id: u64, clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>) {
    log::debug!("server: client_id={client_id} detach");
    let mut cls = clients.lock().await;
    if let Some(client) = cls.get_mut(&client_id) {
        client.session_name = None;
    }
}

async fn handle_input(
    client_id: u64,
    data: &[u8],
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let session_name = {
        let cls = clients.lock().await;
        cls.get(&client_id).and_then(|c| c.session_name.clone())
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    // THE input chokepoint: `Session::input_target` routes to the popup while it
    // is visible, without touching `tab.focused_pane`.
    let active_pane = {
        let st = state.lock().await;
        match st
            .sessions
            .get(&session_name)
            .and_then(Session::input_target)
        {
            Some(p) => p,
            None => return Ok(()),
        }
    };

    log::debug!(
        "server: client_id={client_id} input {} bytes -> pane_id={active_pane}",
        data.len()
    );

    let mut ps = panes.lock().await;
    if let Some(pane_data) = ps.get_mut(&active_pane) {
        pane_data.pty.write_input(data)?;

        // Ctrl+L (0x0C / FF): shells' readline/zsh clear-screen typically emits
        // only \e[H\e[2J (no \e[3J), so the scrollback would otherwise survive.
        // Honor the user's expectation that Ctrl+L also drops the pane's
        // scrollback. Gate on NOT being in alt-screen: full-screen apps like
        // vim use Ctrl+L for their own redraw and keep no scrollback, so they
        // must be unaffected. The shell repaints itself, so no render/broadcast
        // is needed; handle_scroll_delta re-clamps any stale client offsets
        // against the now-smaller total line count.
        if data.contains(&0x0C) && !pane_data.screen.alt_screen_active {
            pane_data.screen.scrollback.clear();
        }
    }
    Ok(())
}

/// Return this client's viewport to the live tail, repainting only if it moved.
///
/// **Why the server does this and not the client.** `scroll_offset` is
/// server-owned state, and the render messages' `viewport_top` is an absolute
/// line index -- which is exactly `0` at maximum scroll, the same value the live
/// tail reports. A client that inferred "am I scrolled?" from `viewport_top`
/// was therefore blind at precisely the maximum, so it never sent the
/// `ScrollReset` that every one of its unstick paths (typing, Escape back to
/// Normal, leaving Visual, cancelling Search) is gated on. The session then
/// looked completely dead: the application kept answering keystrokes and the
/// server kept rendering, but always at the pinned offset, so the output landed
/// below the viewport and the screen never changed. Ending the scroll here means
/// it ends whatever the client believes.
///
/// The render messages now carry a real `scroll_offset` beside `viewport_top`,
/// so a current client does see the maximum for what it is and sends the
/// `ScrollReset` itself -- arriving *before* the `Input` for the same keystroke,
/// which makes this a no-op on that path. It stays because the field is
/// `#[serde(default)]`: a client older than it still reads 0 and is still blind,
/// and this is what unsticks that client's session anyway.
///
/// A no-op for a client already at the tail, which is the overwhelmingly common
/// case -- so this costs one lock and no repaint per keystroke.
///
/// Scoped tightly, so it can only ever undo a scroll the same keystroke made
/// pointless:
///
/// * Only the client that typed. Another client scrolled back through the same
///   session keeps its offset -- `scroll_offset` is per-client and this touches
///   exactly one entry.
/// * Only the attached-session viewport. A View cell's scrollback is the
///   per-(client, pane) `pane_scroll`, which this never reads, so cell input
///   (`InputToPane`, a different arm entirely) cannot move a scrolled cell.
/// * Never in an explicit scrollback mode. Visual (remux's copy mode) and Search
///   are sessions in history that the user drives with keys, and yanking them to
///   the bottom mid-selection would destroy the selection. Belt and braces
///   today: `handle_visual_key`/`handle_search_key` return no `SendToPty`, so
///   neither mode can reach this arm -- but the invariant is what makes the rule
///   safe, so it is enforced rather than assumed.
async fn snap_client_to_live_tail(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    let session_name = {
        let mut cls = clients.lock().await;
        match cls.get_mut(&client_id) {
            Some(client)
                if client.scroll_offset != 0
                    && client.mode != COPY_MODE
                    && client.mode != SEARCH_MODE =>
            {
                log::debug!(
                    "server: input returns client_id={client_id} to the live tail from offset={}",
                    client.scroll_offset
                );
                client.scroll_offset = 0;
                client.prev_scroll_offset = 0;
                // The client's diff baseline is a scrolled frame, so the repaint
                // below must be a full one.
                client.needs_full_render = true;
                client.session_name.clone()
            }
            _ => return,
        }
    };
    if let Some(session_name) = session_name {
        send_full_render_to_client(
            client_id,
            &session_name,
            state,
            panes,
            clients,
            config,
            prev_frames,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_resize(
    client_id: u64,
    cols: u16,
    rows: u16,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let session_name = {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            client.cols = cols;
            client.rows = rows;
            client.session_name.clone()
        } else {
            None
        }
    };

    log::debug!("server: client_id={client_id} resize cols={cols} rows={rows}");

    if let Some(session_name) = session_name {
        resize_session_panes(&session_name, state, panes, clients, config).await?;
        // Invalidate all attached clients' baselines: a resize changes the
        // session render size, so every client must re-render full at the new
        // size on the broadcast below.
        invalidate_session_baselines(&session_name, clients, prev_frames).await;
        broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    client_id: u64,
    cmd: RemuxCommand,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    let (session_name, cols, rows) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => (c.session_name.clone(), c.cols, c.rows),
            None => return Ok(()),
        }
    };
    // Most commands act on the requesting client's attached session and are
    // ignored when it has none. A few structural commands operate on explicit
    // targets instead (folder create/delete/move, close-tab-by-index), so they
    // are allowed through even without an attached session. This lets the
    // session manager edit folders/tabs on a remote the client has connected to
    // but not switched to (attached). Their arms never read `session_name`.
    let session_name = match session_name {
        Some(s) => s,
        None => {
            if matches!(
                cmd,
                RemuxCommand::FolderNew(_)
                    | RemuxCommand::FolderDelete(_)
                    | RemuxCommand::FolderMoveSession { .. }
                    | RemuxCommand::TabCloseByIndex { .. }
                    | RemuxCommand::SessionRenameByName { .. }
                    | RemuxCommand::FolderRename { .. }
                    | RemuxCommand::TabNewInSession { .. }
                    | RemuxCommand::TabRenameByIndex { .. }
                    | RemuxCommand::PaneNewInTab { .. }
                    | RemuxCommand::PaneCloseById { .. }
                    | RemuxCommand::PaneRenameById { .. }
                    | RemuxCommand::TabMoveByIndex { .. }
            ) {
                String::new()
            } else {
                return Ok(());
            }
        }
    };

    log::debug!("server: client_id={client_id} command={cmd:?} session={session_name:?}");

    // -- Popup command guard -----------------------------------------------
    //
    // **THE layout-mutation chokepoint, and the structural half of the hard
    // invariant.** While the popup is visible it is the input target, so a
    // layout-mutating command would run with the popup as its subject -- and
    // `PaneMove*` (swap_panes / relocate_pane_to_edge), `SetMaster` (promotes a
    // pane), `LayoutNext` (rebuilds from `pane_order`) and the stack ops
    // (splice nodes) would each be a route for the popup pane to get captured by
    // the layout. Blocking them here means no such command can ever run with a
    // popup subject; the other half of the invariant is that nothing ever
    // *inserts* the popup id into `pane_order` or a tree in the first place.
    //
    // Three-way, in order: reroute -> block -> pass.
    if !session_name.is_empty() {
        let popup_visible = {
            let st = state.lock().await;
            st.sessions
                .get(&session_name)
                .map(|s| s.popup_visible && s.popup_pane.is_some())
                .unwrap_or(false)
        };
        if popup_visible {
            match cmd {
                // Reroute: resize adjusts the popup's own size; close closes the
                // popup (resolved via the popup-aware `input_target` below).
                RemuxCommand::ResizeLeft(_)
                | RemuxCommand::ResizeRight(_)
                | RemuxCommand::ResizeUp(_)
                | RemuxCommand::ResizeDown(_) => {
                    let (dw, dh) = match cmd {
                        RemuxCommand::ResizeLeft(a) => (-(a as i16), 0),
                        RemuxCommand::ResizeRight(a) => (a as i16, 0),
                        RemuxCommand::ResizeUp(a) => (0, -(a as i16)),
                        RemuxCommand::ResizeDown(a) => (0, a as i16),
                        _ => unreachable!(),
                    };
                    {
                        let mut st = state.lock().await;
                        if let Some(sess) = st.sessions.get_mut(&session_name) {
                            let new_size = sess.resize_popup(dw, dh);
                            log::debug!("server: popup resize -> {new_size:?}");
                        }
                    }
                    resize_session_panes(&session_name, state, panes, clients, config).await?;
                    invalidate_session_baselines(&session_name, clients, prev_frames).await;
                    broadcast_full_render(
                        &session_name,
                        state,
                        panes,
                        clients,
                        config,
                        prev_frames,
                    )
                    .await;
                    return Ok(());
                }
                // Block: everything that reshapes the layout tree, `pane_order`,
                // or `zoomed_pane`. Two deliberate additions beyond the layout
                // mutators: `PaneNew` (it pushes to `pane_order`, and the user is
                // looking at the popup), and `PaneRename` -- the rename targets
                // `tab.focused_pane`, so starting one while the popup is up would
                // accumulate a hidden rename whose border cursor is suppressed by
                // the popup's cursor override, with the user's keystrokes going
                // to the popup's shell instead.
                RemuxCommand::PaneRename(_)
                | RemuxCommand::PaneFocusLeft
                | RemuxCommand::PaneFocusRight
                | RemuxCommand::PaneFocusUp
                | RemuxCommand::PaneFocusDown
                | RemuxCommand::PaneMoveLeft
                | RemuxCommand::PaneMoveRight
                | RemuxCommand::PaneMoveUp
                | RemuxCommand::PaneMoveDown
                | RemuxCommand::PaneSplitVertical
                | RemuxCommand::PaneSplitHorizontal
                | RemuxCommand::PaneNew
                | RemuxCommand::PaneStackAdd
                | RemuxCommand::PaneStackNext
                | RemuxCommand::PaneStackPrev
                | RemuxCommand::PaneToggleZoom
                | RemuxCommand::LayoutNext
                | RemuxCommand::SetMaster => {
                    log::debug!("server: command {cmd:?} is a no-op while the popup is open");
                    return Ok(());
                }
                // Pass: PopupToggle, PaneClose (popup-aware target), tab
                // switching (the popup is session-scoped and survives it), and
                // everything non-layout.
                _ => {}
            }
        }
    }

    match cmd {
        RemuxCommand::PopupToggle => {
            // Lazily spawn the popup pane on first use, then just flip
            // visibility. The pane is registered in the pane map ONLY -- never in
            // `tab.pane_order`, never in a layout tree.
            let (spawn, source_pane_id) = {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let source = sess.tabs.get(sess.active_tab).map(|t| t.focused_pane);
                match sess.popup_pane {
                    Some(existing) => {
                        sess.popup_visible = !sess.popup_visible;
                        log::debug!(
                            "server: PopupToggle session={session_name:?} pane_id={existing} visible={}",
                            sess.popup_visible
                        );
                        (None, source)
                    }
                    None => {
                        let new_id = st.next_pane_id();
                        // Re-borrow: `next_pane_id` needed `st` mutably.
                        let sess = match st.sessions.get_mut(&session_name) {
                            Some(s) => s,
                            None => return Ok(()),
                        };
                        sess.popup_pane = Some(new_id);
                        sess.popup_visible = true;
                        log::debug!(
                            "server: PopupToggle session={session_name:?} spawned popup pane_id={new_id}"
                        );
                        (Some(new_id), source)
                    }
                }
            };
            if let Some(new_id) = spawn {
                // Inherit the focused pane's cwd, exactly like a split does.
                let focused_cwd = {
                    let panes_lock = panes.lock().await;
                    source_pane_id
                        .and_then(|id| panes_lock.get(&id))
                        .and_then(|p| persistence::get_pane_cwd(p.pty.child_pid))
                };
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: cols,
                    height: rows.saturating_sub(1),
                };
                let popup_size = {
                    let st = state.lock().await;
                    st.sessions
                        .get(&session_name)
                        .map(|s| s.popup_size)
                        .unwrap_or((80, 80))
                };
                let rect = layout::popup_rect(area, popup_size);
                spawn_pane(
                    new_id,
                    rect.width.saturating_sub(2).max(1),
                    rect.height.saturating_sub(2).max(1),
                    None,
                    focused_cwd.as_deref().map(std::path::Path::new),
                    panes,
                    config,
                )
                .await?;
                start_pty_forwarding(
                    &session_name,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                    dormant,
                )
                .await;
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            // Showing/hiding the popup repaints a large region; force a clean
            // full render so no diff baseline can keep stale popup cells.
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabNew => {
            // Capture the source pane (active tab's focused pane) BEFORE
            // create_tab, which flips active_tab to the new empty tab. The new
            // tab inherits the source pane's cwd.
            let (pane_id, source_pane_id) = {
                let mut st = state.lock().await;
                let source_pane_id = st
                    .sessions
                    .get(&session_name)
                    .and_then(|s| s.tabs.get(s.active_tab))
                    .map(|t| t.focused_pane);
                let tab_count = st
                    .sessions
                    .get(&session_name)
                    .map(|s| s.tabs.len())
                    .unwrap_or(0);
                let tab_name = format!("Tab {}", tab_count + 1);
                let pane_id = st.create_tab(&session_name, &tab_name, LayoutMode::default())?;
                (pane_id, source_pane_id)
            };
            let focused_cwd = {
                let panes_lock = panes.lock().await;
                source_pane_id
                    .and_then(|id| panes_lock.get(&id))
                    .and_then(|p| persistence::get_pane_cwd(p.pty.child_pid))
            };
            log::debug!("server: TabNew session={session_name:?} new pane_id={pane_id}");
            spawn_pane(
                pane_id,
                cols,
                rows,
                None,
                focused_cwd.as_deref().map(std::path::Path::new),
                panes,
                config,
            )
            .await?;
            start_pty_forwarding(
                &session_name,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            // Resize the new tab's panes to the drawn content area (mirrors the
            // PaneSplit* path) so the child sees the same rows the compositor
            // draws — otherwise the footer is clipped.
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            // Tab switch: the whole displayed content changes, so invalidate
            // all clients' baselines to force a clean full render.
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabClose => {
            let tab_idx = {
                let st = state.lock().await;
                match st.sessions.get(&session_name) {
                    Some(s) => s.active_tab,
                    None => return Ok(()),
                }
            };
            log::debug!("server: TabClose session={session_name:?} tab_idx={tab_idx}");
            let (pane_ids, deleted) = {
                let mut st = state.lock().await;
                st.close_tab(&session_name, tab_idx)?
            };
            reap_panes(&pane_ids, panes, clients).await;
            if deleted {
                // Last tab closed -> the session was removed. Re-point attached
                // clients onto another session (or notify them if none remain);
                // don't broadcast the now-dead session.
                handle_session_removed(&session_name, state, panes, clients, config, prev_frames)
                    .await;
            } else {
                invalidate_session_baselines(&session_name, clients, prev_frames).await;
                broadcast_full_render(&session_name, state, panes, clients, config, prev_frames)
                    .await;
            }
        }
        RemuxCommand::TabGoto(idx) => {
            {
                let mut st = state.lock().await;
                st.goto_tab(&session_name, idx)?;
            }
            // Resize the newly-active tab's panes to the drawn content area
            // before rendering so the footer isn't clipped.
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabNext => {
            {
                let mut st = state.lock().await;
                let next = {
                    let sess = match st.sessions.get(&session_name) {
                        Some(s) => s,
                        None => return Ok(()),
                    };
                    (sess.active_tab + 1) % sess.tabs.len()
                };
                st.goto_tab(&session_name, next)?;
            }
            // Resize the newly-active tab's panes to the drawn content area
            // before rendering so the footer isn't clipped.
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabPrev => {
            {
                let mut st = state.lock().await;
                let prev = {
                    let sess = match st.sessions.get(&session_name) {
                        Some(s) => s,
                        None => return Ok(()),
                    };
                    if sess.active_tab == 0 {
                        sess.tabs.len().saturating_sub(1)
                    } else {
                        sess.active_tab - 1
                    }
                };
                st.goto_tab(&session_name, prev)?;
            }
            // Resize the newly-active tab's panes to the drawn content area
            // before rendering so the footer isn't clipped.
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            invalidate_session_baselines(&session_name, clients, prev_frames).await;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabRename(name) => {
            log::debug!("server: TabRename session={session_name:?} new_name={name:?}");
            {
                let mut st = state.lock().await;
                let idx = {
                    match st.sessions.get(&session_name) {
                        Some(s) => s.active_tab,
                        None => return Ok(()),
                    }
                };
                st.rename_tab(&session_name, idx, &name)?;
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneSplitVertical | RemuxCommand::PaneSplitHorizontal => {
            let placement = if matches!(cmd, RemuxCommand::PaneSplitVertical) {
                PanePlacement::SplitVertical
            } else {
                PanePlacement::SplitHorizontal
            };
            create_pane_in_tab(
                &session_name,
                None,
                placement,
                cols,
                rows,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?;
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneClose => {
            // Popup-aware target: while the popup is open this closes the popup
            // (killing its shell and clearing the state), which `close_pane`
            // handles in its dedicated popup branch.
            let closed_pane = {
                let st = state.lock().await;
                match st
                    .sessions
                    .get(&session_name)
                    .and_then(Session::input_target)
                {
                    Some(p) => p,
                    None => return Ok(()),
                }
            };
            log::debug!("server: PaneClose pane_id={closed_pane}");
            close_pane(
                closed_pane,
                &session_name,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await;
        }
        RemuxCommand::PaneFocusLeft
        | RemuxCommand::PaneFocusRight
        | RemuxCommand::PaneFocusUp
        | RemuxCommand::PaneFocusDown => {
            let direction = match cmd {
                RemuxCommand::PaneFocusLeft => layout::FocusDirection::Left,
                RemuxCommand::PaneFocusRight => layout::FocusDirection::Right,
                RemuxCommand::PaneFocusUp => layout::FocusDirection::Up,
                RemuxCommand::PaneFocusDown => layout::FocusDirection::Down,
                _ => unreachable!(),
            };
            log::debug!("server: PaneFocus direction={direction:?}");
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: cols,
                    height: rows.saturating_sub(1),
                };
                // Stack-aware directional focus (zellij behavior): step within a
                // multi-pane stack first, else fall back to the spatial neighbor.
                if let Some(target) = layout::focus_in_direction(
                    &mut tab.layout,
                    area,
                    tab.focused_pane,
                    direction,
                    0,
                ) {
                    tab.focus_pane(target);
                }
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneMoveLeft
        | RemuxCommand::PaneMoveRight
        | RemuxCommand::PaneMoveUp
        | RemuxCommand::PaneMoveDown => {
            let direction = match cmd {
                RemuxCommand::PaneMoveLeft => layout::FocusDirection::Left,
                RemuxCommand::PaneMoveRight => layout::FocusDirection::Right,
                RemuxCommand::PaneMoveUp => layout::FocusDirection::Up,
                RemuxCommand::PaneMoveDown => layout::FocusDirection::Down,
                _ => unreachable!(),
            };
            log::debug!("server: PaneMove direction={direction:?}");
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: cols,
                    height: rows.saturating_sub(1),
                };
                // Swap the focused pane with its spatial neighbor in `direction`.
                // Focus stays on the moved pane (its id is unchanged; only its
                // slot in the tree changes).
                if let Some(neighbor) =
                    layout::find_neighbor(&tab.layout, area, tab.focused_pane, direction.clone(), 0)
                {
                    // Adjacent reorder: swap with the neighbor in `direction`.
                    if layout::swap_panes(&mut tab.layout, tab.focused_pane, neighbor)
                        && tab.layout_mode.is_automatic()
                    {
                        // A manual move ejects to Custom so an automatic rebuild
                        // from `pane_order` doesn't revert the swap.
                        tab.layout_mode = LayoutMode::Custom(CustomLayout);
                    }
                } else if let Some(new_tree) =
                    layout::relocate_pane_to_edge(&tab.layout, tab.focused_pane, direction)
                {
                    // No neighbor in `direction`: the focused pane is at that
                    // edge. Relocate it, restructuring the layout, and always
                    // eject to Custom so it isn't rebuilt away.
                    tab.layout = new_tree;
                    tab.layout_mode = LayoutMode::Custom(CustomLayout);
                }
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneStackAdd => {
            create_pane_in_tab(
                &session_name,
                None,
                PanePlacement::Stack,
                cols,
                rows,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?;
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneStackNext => {
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                if let Some(next) = tab.layout.stack_next(tab.focused_pane) {
                    tab.focus_pane(next);
                }
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneStackPrev => {
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                if let Some(prev) = tab.layout.stack_prev(tab.focused_pane) {
                    tab.focus_pane(prev);
                }
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::ResizeLeft(amount)
        | RemuxCommand::ResizeRight(amount)
        | RemuxCommand::ResizeUp(amount)
        | RemuxCommand::ResizeDown(amount) => {
            let (direction, delta) = match &cmd {
                RemuxCommand::ResizeLeft(_) => {
                    (layout::Direction::Vertical, -(amount as f32) / 100.0)
                }
                RemuxCommand::ResizeRight(_) => {
                    (layout::Direction::Vertical, amount as f32 / 100.0)
                }
                RemuxCommand::ResizeUp(_) => {
                    (layout::Direction::Horizontal, -(amount as f32) / 100.0)
                }
                RemuxCommand::ResizeDown(_) => {
                    (layout::Direction::Horizontal, amount as f32 / 100.0)
                }
                _ => unreachable!(),
            };
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // Eject to Custom mode on manual resize
                if tab.layout_mode.is_automatic() {
                    tab.layout_mode = LayoutMode::Custom(CustomLayout);
                }
                tab.layout.resize(tab.focused_pane, direction, delta);
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::ToggleStyle => {
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                sess.border_style = match sess.border_style {
                    BorderStyle::ZellijStyle => BorderStyle::TmuxStyle,
                    BorderStyle::TmuxStyle => BorderStyle::ZellijStyle,
                };
                log::debug!("server: ToggleStyle new_style={:?}", sess.border_style);
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SessionDetach => {
            handle_detach(client_id, clients).await;
        }
        RemuxCommand::SessionList => {
            handle_list_sessions(client_id, state, clients).await?;
        }
        RemuxCommand::SessionRename(new_name) => {
            log::debug!("server: SessionRename old={session_name:?} new={new_name:?}");
            {
                let mut st = state.lock().await;
                st.rename_session(&session_name, &new_name)?;
            }
            let mut cls = clients.lock().await;
            for client in cls.values_mut() {
                if client.session_name.as_deref() == Some(&session_name) {
                    client.session_name = Some(new_name.clone());
                }
            }
        }
        RemuxCommand::PaneRename(name) => {
            log::debug!("server: PaneRename new_name={name:?}");
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                // Clear rename state now that the rename is committed.
                sess.rename_state = None;
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                layout::set_pane_custom_name(&mut tab.layout, tab.focused_pane, &name);
                layout::set_pane_name(&mut tab.layout, tab.focused_pane, &name);
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SendKey(bytes) => {
            // Forward raw key bytes to the active pane's PTY.
            let pane_id = {
                let st = state.lock().await;
                let sess = match st.sessions.get(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                tab.focused_pane
            };
            let panes_lock = panes.lock().await;
            if let Some(pane) = panes_lock.get(&pane_id) {
                if let Err(e) = pane.pty.write_input(&bytes) {
                    log::error!("failed to write SendKey to pane {pane_id}: {e}");
                }
            }
        }
        RemuxCommand::PaneNew => {
            create_pane_in_tab(
                &session_name,
                None,
                PanePlacement::Auto,
                cols,
                rows,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?;
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::LayoutNext => {
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // Snapshot the current custom arrangement *before* touching the
                // tree, so the cycle can return to it later. Order matters: a
                // rebuild below would otherwise overwrite `tab.layout`.
                if matches!(tab.layout_mode, LayoutMode::Custom(_)) {
                    tab.saved_custom_layout = Some(tab.layout.clone());
                }
                // Cycling the layout releases the zoom, for the same reason a
                // new pane does: the tab is showing a different arrangement, and
                // a zoom would hide all of it. Cleared *first*, so the
                // `focus_pane` below can't carry a stale zoom into the restored
                // tree -- and so the cycle can never park a live zoom in Monocle,
                // the one mode that refuses to take one.
                tab.zoomed_pane = None;
                // Grid (the last automatic before wrap) returns to Custom only
                // when the saved tree is still restorable (its pane set matches
                // the live panes); otherwise the cycle stays automatic.
                let restorable =
                    saved_custom_is_restorable(&tab.saved_custom_layout, &tab.pane_order);
                tab.layout_mode = next_layout_mode(&tab.layout_mode, restorable);
                log::debug!("server: LayoutNext new_mode={}", tab.layout_mode.name());
                if tab.layout_mode.is_automatic() {
                    tab.layout = tab
                        .layout_mode
                        .build_tree(&tab.pane_order, tab.focused_pane);
                } else if let Some(saved) = tab.saved_custom_layout.clone() {
                    // Custom: restore the remembered arrangement. Adopt its own
                    // active-pane markers so the focused pane stays visible.
                    tab.layout = saved;
                    if let Some(active) = tab.layout.active_pane() {
                        tab.focus_pane(active);
                    }
                }
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SetMaster => {
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // Snapshot the manual arrangement *before* the rebuild below
                // discards it, exactly as `LayoutNext` does -- otherwise
                // promoting a master out of a hand-built (e.g. stacked) layout
                // would lose it with no way back through the cycle.
                if matches!(tab.layout_mode, LayoutMode::Custom(_)) {
                    tab.saved_custom_layout = Some(tab.layout.clone());
                }
                // Promoting a master rebuilds the arrangement, so it releases
                // the zoom too (see `LayoutNext`).
                tab.zoomed_pane = None;
                // Switch to Master layout if not already in it.
                if !matches!(tab.layout_mode, LayoutMode::Master(_)) {
                    tab.layout_mode = LayoutMode::Master(MasterLayout::default());
                }
                if let LayoutMode::Master(ref mut master_layout) = tab.layout_mode {
                    master_layout.master_pane = Some(tab.focused_pane);
                    tab.layout = tab
                        .layout_mode
                        .build_tree(&tab.pane_order, tab.focused_pane);
                }
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SessionSwitchTab { session, tab_index } => {
            // Attach the client to the specified session and switch to the tab.
            {
                let mut cls = clients.lock().await;
                if let Some(client) = cls.get_mut(&client_id) {
                    client.session_name = Some(session.clone());
                }
            }
            {
                let mut st = state.lock().await;
                if let Err(e) = st.goto_tab(&session, tab_index) {
                    log::error!("SessionSwitchTab: {e}");
                    return Ok(());
                }
            }
            start_pty_forwarding(
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            resize_session_panes(&session, state, panes, clients, config).await?;
            // Invalidate all attached clients' baselines to force a fresh full
            // render (not a diff against stale content from a previous tab).
            invalidate_session_baselines(&session, clients, prev_frames).await;
            send_full_render_to_client(
                client_id,
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await;
        }
        RemuxCommand::SessionSwitchPane {
            session,
            tab_index,
            pane_id,
        } => {
            // Attach to session, switch tab, and focus pane.
            {
                let mut cls = clients.lock().await;
                if let Some(client) = cls.get_mut(&client_id) {
                    client.session_name = Some(session.clone());
                }
            }
            {
                let mut st = state.lock().await;
                if let Err(e) = st.goto_tab(&session, tab_index) {
                    log::error!("SessionSwitchPane: goto_tab: {e}");
                    return Ok(());
                }
                if let Some(sess) = st.sessions.get_mut(&session) {
                    if let Some(tab) = sess.tabs.get_mut(sess.active_tab) {
                        tab.focus_pane(pane_id);
                    }
                }
            }
            start_pty_forwarding(
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            resize_session_panes(&session, state, panes, clients, config).await?;
            // Invalidate all attached clients' baselines to force a fresh full
            // render.
            invalidate_session_baselines(&session, clients, prev_frames).await;
            send_full_render_to_client(
                client_id,
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await;
        }
        RemuxCommand::TabCloseByIndex {
            session: target_session,
            tab_index,
        } => {
            let close_result = {
                let mut st = state.lock().await;
                st.close_tab(&target_session, tab_index)
            };
            match close_result {
                Ok((pane_ids, session_deleted)) => {
                    reap_panes(&pane_ids, panes, clients).await;
                    if session_deleted {
                        // Last tab of the target session closed -> it was
                        // removed. Re-point its attached clients onto another
                        // session (or notify them if none remain).
                        handle_session_removed(
                            &target_session,
                            state,
                            panes,
                            clients,
                            config,
                            prev_frames,
                        )
                        .await;
                    } else {
                        // Closing a tab may switch the active tab. The requesting
                        // client (session manager) is generally viewing a
                        // different session, so its cols/rows don't apply here.
                        // Derive the target session's dims from its own attached
                        // clients (min, as broadcast_full_render does) and resize
                        // the newly-active tab's panes so the footer isn't clipped.
                        let target_dims = {
                            let cls = clients.lock().await;
                            let cols = cls
                                .values()
                                .filter(|c| c.session_name.as_deref() == Some(&target_session))
                                .map(|c| c.cols)
                                .min();
                            let rows = cls
                                .values()
                                .filter(|c| c.session_name.as_deref() == Some(&target_session))
                                .map(|c| c.rows)
                                .min();
                            cols.zip(rows)
                        };
                        if target_dims.is_some() {
                            resize_session_panes(&target_session, state, panes, clients, config)
                                .await?;
                        }
                        invalidate_session_baselines(&target_session, clients, prev_frames).await;
                        broadcast_full_render(
                            &target_session,
                            state,
                            panes,
                            clients,
                            config,
                            prev_frames,
                        )
                        .await;
                    }
                }
                Err(e) => {
                    log::error!("TabCloseByIndex: {e}");
                }
            }
        }
        RemuxCommand::FolderNew(name) => {
            let mut st = state.lock().await;
            st.create_folder(&name)?;
        }
        RemuxCommand::FolderDelete(name) => {
            let deleted_sessions = {
                let mut st = state.lock().await;
                st.delete_folder_cascade(&name)?
            };
            // Clean up panes and notify clients for each deleted session.
            {
                let doomed: Vec<PaneId> = deleted_sessions
                    .iter()
                    .flat_map(|(_, pane_ids)| pane_ids.iter().copied())
                    .collect();
                reap_panes(&doomed, panes, clients).await;
            }
            {
                let mut cls = clients.lock().await;
                for (session_name, _) in &deleted_sessions {
                    for client in cls.values_mut() {
                        if client.session_name.as_deref() == Some(session_name) {
                            client.session_name = None;
                            let _ =
                                client
                                    .tx
                                    .send(ServerMessage::Event(SessionEvent::SessionDeleted(
                                        session_name.clone(),
                                    )));
                        }
                    }
                }
            }
        }
        RemuxCommand::FolderMoveSession { session, folder } => {
            let mut st = state.lock().await;
            st.move_session(&session, folder.as_deref())?;
        }
        RemuxCommand::FolderList => {
            // Handled via SessionList or ListSessionTree.
        }
        RemuxCommand::SessionNew { name, folder } => {
            handle_create_session(
                client_id,
                &name,
                folder.as_deref(),
                state,
                panes,
                clients,
                config,
            )
            .await?;
        }
        RemuxCommand::TabMove(idx) => {
            let mut st = state.lock().await;
            if let Some(sess) = st.sessions.get_mut(&session_name) {
                let target = idx.saturating_sub(1).min(sess.tabs.len().saturating_sub(1));
                let current = sess.active_tab;
                if current != target && current < sess.tabs.len() {
                    let tab = sess.tabs.remove(current);
                    sess.tabs.insert(target, tab);
                    sess.active_tab = target;
                }
            }
            drop(st);
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneToggleZoom => {
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // `Tab::toggle_zoom` owns the rule that only *engaging* a zoom
                // can be refused (Monocle is already fullscreen); releasing one
                // always works, so the zoom can never become unreachable.
                let changed = tab.toggle_zoom();
                log::debug!(
                    "server: PaneToggleZoom pane={} zoomed={} changed={changed}",
                    tab.focused_pane,
                    tab.zoomed_pane.is_some()
                );
                // Nothing moved (a redundant zoom request in Monocle): skip the
                // resize + repaint entirely, as this arm always has.
                if !changed {
                    return Ok(());
                }
            }
            resize_session_panes(&session_name, state, panes, clients, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        // -- Explicit-target structural commands ----------------------------
        // These act on a named/indexed target rather than the requester's
        // attached session (the session manager may be attached elsewhere or
        // nowhere). They never read the local `session_name`. On failure they
        // fail silently (log + early return) rather than propagating an error,
        // and after a successful mutation they refresh the *target* session's
        // attached clients (if any) via `refresh_target_session`.
        RemuxCommand::SessionRenameByName { old, new } => {
            log::debug!("server: SessionRenameByName old={old:?} new={new:?}");
            {
                let mut st = state.lock().await;
                if let Err(e) = st.rename_session(&old, &new) {
                    log::info!("SessionRenameByName: {e}");
                    return Ok(());
                }
            }
            // Retarget any clients still attached under the old name so their
            // input/render continues to resolve to the renamed session.
            {
                let mut cls = clients.lock().await;
                for client in cls.values_mut() {
                    if client.session_name.as_deref() == Some(&old) {
                        client.session_name = Some(new.clone());
                    }
                }
            }
        }
        RemuxCommand::FolderRename { old, new } => {
            log::debug!("server: FolderRename old={old:?} new={new:?}");
            let mut st = state.lock().await;
            if let Err(e) = st.rename_folder(&old, &new) {
                log::info!("FolderRename: {e}");
            }
        }
        RemuxCommand::TabNewInSession { session } => {
            // Mirror TabNew but on the named target session, using that
            // session's own render dimensions (the requester may be attached
            // elsewhere or nowhere).
            let (tcols, trows) = session_render_size(&session, clients).await;
            // Capture the target session's source pane (its active tab's focused
            // pane) BEFORE create_tab flips active_tab to the new empty tab.
            let (pane_id, source_pane_id) = {
                let mut st = state.lock().await;
                let source_pane_id = st
                    .sessions
                    .get(&session)
                    .and_then(|s| s.tabs.get(s.active_tab))
                    .map(|t| t.focused_pane);
                let tab_count = match st.sessions.get(&session) {
                    Some(s) => s.tabs.len(),
                    None => {
                        log::info!("TabNewInSession: session '{session}' not found");
                        return Ok(());
                    }
                };
                let tab_name = format!("Tab {}", tab_count + 1);
                match st.create_tab(&session, &tab_name, LayoutMode::default()) {
                    Ok(pid) => (pid, source_pane_id),
                    Err(e) => {
                        log::info!("TabNewInSession: {e}");
                        return Ok(());
                    }
                }
            };
            let focused_cwd = {
                let panes_lock = panes.lock().await;
                source_pane_id
                    .and_then(|id| panes_lock.get(&id))
                    .and_then(|p| persistence::get_pane_cwd(p.pty.child_pid))
            };
            log::debug!("server: TabNewInSession session={session:?} new pane_id={pane_id}");
            spawn_pane(
                pane_id,
                tcols,
                trows,
                None,
                focused_cwd.as_deref().map(std::path::Path::new),
                panes,
                config,
            )
            .await?;
            // create_tab makes the new tab active, so forwarding starts for its
            // pane here. (For a mutation on a non-active tab, forwarding would
            // instead begin on the next SessionSwitchTab; the guard makes the
            // repeat call safe.)
            start_pty_forwarding(
                &session,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await;
            refresh_target_session(&session, state, panes, clients, config, prev_frames).await?;
        }
        RemuxCommand::TabRenameByIndex {
            session,
            tab_index,
            name,
        } => {
            log::debug!(
                "server: TabRenameByIndex session={session:?} idx={tab_index} name={name:?}"
            );
            {
                let mut st = state.lock().await;
                if let Err(e) = st.rename_tab(&session, tab_index, &name) {
                    log::info!("TabRenameByIndex: {e}");
                    return Ok(());
                }
            }
            refresh_target_session(&session, state, panes, clients, config, prev_frames).await?;
        }
        RemuxCommand::PaneNewInTab { session, tab_index } => {
            // Same placement as `PaneNew`, but on tab `tab_index` of the named
            // target session (`PaneNew` operates on the requester's focused tab),
            // sized from that session's own render size and refreshing only the
            // clients attached to it.
            let (tcols, trows) = session_render_size(&session, clients).await;
            if create_pane_in_tab(
                &session,
                Some(tab_index),
                PanePlacement::Auto,
                tcols,
                trows,
                state,
                panes,
                clients,
                config,
                prev_frames,
                dormant,
            )
            .await?
            .is_none()
            {
                return Ok(());
            }
            refresh_target_session(&session, state, panes, clients, config, prev_frames).await?;
        }
        RemuxCommand::PaneCloseById { session, pane_id } => {
            log::debug!("server: PaneCloseById session={session:?} pane_id={pane_id}");
            // If the pane is in the target session's active tab, reuse close_pane
            // (which handles layout collapse, tab removal, last-session removal,
            // client switch, and its own resize/broadcast). If it lives in a
            // background tab, close_pane would silently no-op, so handle that
            // path inline here.
            let in_active_tab = {
                let st = state.lock().await;
                st.sessions
                    .get(&session)
                    .and_then(|s| s.tabs.get(s.active_tab))
                    .map(|t| t.pane_order.contains(&pane_id))
                    .unwrap_or(false)
            };
            if in_active_tab {
                close_pane(
                    pane_id,
                    &session,
                    state,
                    panes,
                    clients,
                    config,
                    prev_frames,
                )
                .await;
            } else {
                // Background-tab path: mirror close_pane's non-active-tab-safe
                // subset (layout collapse + pane_order retain + focus/rebuild,
                // and tab removal if it became empty). A background tab implies
                // at least the active tab also exists, so the session can never
                // be emptied here — the last-session/switch/disconnect branch is
                // unreachable and intentionally omitted.
                {
                    let mut st = state.lock().await;
                    let sess = match st.sessions.get_mut(&session) {
                        Some(s) => s,
                        None => return Ok(()),
                    };
                    let tab_idx = match sess
                        .tabs
                        .iter()
                        .position(|t| t.pane_order.contains(&pane_id))
                    {
                        Some(i) => i,
                        None => {
                            log::info!(
                                "PaneCloseById: pane {pane_id} not found in session '{session}'"
                            );
                            return Ok(());
                        }
                    };
                    let tab = &mut sess.tabs[tab_idx];
                    let new_focus = tab.layout.close_pane(pane_id);
                    tab.pane_order.retain(|&id| id != pane_id);
                    // Closing the zoomed pane un-zooms the tab: `zoomed_pane` is
                    // the pane that gets painted full-area, so keeping a dead id
                    // there would paint a pane that no longer exists.
                    if tab.zoomed_pane == Some(pane_id) {
                        tab.zoomed_pane = None;
                    }
                    match new_focus {
                        Some(nf) => {
                            tab.focus_pane(nf);
                            if tab.layout_mode.is_automatic() {
                                tab.layout = tab.layout_mode.build_tree(&tab.pane_order, nf);
                            }
                            session::debug_check_invariant(sess, "PaneCloseById");
                        }
                        None => {
                            // Last pane in this background tab -> remove the tab.
                            // Adjust active_tab if it sat after the removed tab
                            // (close_pane never does this — it only removes the
                            // active tab itself).
                            sess.tabs.remove(tab_idx);
                            if tab_idx < sess.active_tab {
                                sess.active_tab -= 1;
                            }
                        }
                    }
                }
                reap_panes(&[pane_id], panes, clients).await;
                refresh_target_session(&session, state, panes, clients, config, prev_frames)
                    .await?;
            }
        }
        RemuxCommand::PaneRenameById {
            session,
            pane_id,
            name,
        } => {
            log::debug!(
                "server: PaneRenameById session={session:?} pane_id={pane_id} name={name:?}"
            );
            {
                let mut st = state.lock().await;
                let sess = match st.sessions.get_mut(&session) {
                    Some(s) => s,
                    None => {
                        log::info!("PaneRenameById: session '{session}' not found");
                        return Ok(());
                    }
                };
                // Locate the tab owning this pane (any tab, not just the active
                // one) and set its custom name, mirroring PaneRename.
                let mut found = false;
                for tab in sess.tabs.iter_mut() {
                    if tab.pane_order.contains(&pane_id) {
                        layout::set_pane_custom_name(&mut tab.layout, pane_id, &name);
                        layout::set_pane_name(&mut tab.layout, pane_id, &name);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::info!("PaneRenameById: pane {pane_id} not found in session '{session}'");
                    return Ok(());
                }
            }
            refresh_target_session(&session, state, panes, clients, config, prev_frames).await?;
        }
        RemuxCommand::TabMoveByIndex {
            session,
            tab_index,
            delta,
        } => {
            log::debug!("server: TabMoveByIndex session={session:?} idx={tab_index} delta={delta}");
            {
                let mut st = state.lock().await;
                if let Err(e) = st.move_tab(&session, tab_index, delta) {
                    log::info!("TabMoveByIndex: {e}");
                    return Ok(());
                }
            }
            refresh_target_session(&session, state, panes, clients, config, prev_frames).await?;
        }
        _ => {
            log::debug!("unhandled command: {cmd:?}");
        }
    }

    // Persist state after every command that may have changed structure. The
    // same hook feeds the session-tree push: a command that may have changed
    // structure is exactly a command that may have changed what a subscriber's
    // tree shows. Deliberately here rather than inside `save_if_enabled`, whose
    // body is skipped entirely when `save_sessions = false` -- pushes must not
    // depend on the persistence toggle.
    save_if_enabled(state, panes, config, dormant).await;
    mark_session_tree_dirty();

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_create_session(
    client_id: u64,
    name: &str,
    folder: Option<&str>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) -> Result<()> {
    let (cols, rows) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => (c.cols, c.rows),
            None => (80, 24),
        }
    };
    let pane_id = {
        let mut st = state.lock().await;
        let border_style = config.appearance.border_style.clone();
        let layout_mode = config.appearance.default_layout.to_layout_mode();
        let popup_size = (
            config.appearance.popup_width_pct,
            config.appearance.popup_height_pct,
        );
        st.create_session(name, folder, border_style, layout_mode, popup_size)?
    };
    log::debug!("server: CreateSession name={name:?} folder={folder:?} pane_id={pane_id}");
    spawn_pane(pane_id, cols, rows, None, None, panes, config).await?;

    // Announce to EVERY client, not just the creator. A session manager open in
    // another terminal has no timer -- every tree refresh is event-driven -- so
    // a creator-only notification left every other terminal's tree stale until
    // some unrelated action happened to refresh it. Symmetric with
    // `SessionDeleted`, which already reaches every client it concerns.
    let cls = clients.lock().await;
    for client in cls.values() {
        let _ = client
            .tx
            .send(ServerMessage::Event(SessionEvent::SessionCreated(
                name.to_string(),
            )));
    }
    Ok(())
}

async fn handle_list_sessions(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let st = state.lock().await;
    let cls = clients.lock().await;
    let entries: Vec<SessionListEntry> = st
        .list_sessions()
        .into_iter()
        .map(|info| {
            let client_count = cls
                .values()
                .filter(|c| c.session_name.as_deref() == Some(&info.name))
                .count();
            SessionListEntry {
                name: info.name,
                folder: info.folder,
                tab_count: info.tab_count,
                client_count,
            }
        })
        .collect();
    if let Some(client) = cls.get(&client_id) {
        let _ = client
            .tx
            .send(ServerMessage::SessionList { sessions: entries });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Session-tree push
// ---------------------------------------------------------------------------

/// Minimum spacing between two `SessionTree` broadcasts, so a burst of
/// structural commands costs subscribers one extra push rather than one push
/// per command.
const SESSION_TREE_PUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Wakes the session-tree push task when something a subscriber's tree
/// *displays* has changed.
///
/// A process-global signal rather than a handle threaded through the call
/// graph, for two reasons:
///
/// 1. One of the change points is [`update_auto_pane_names`], reached from
///    `broadcast_full_render` and `send_full_render_to_client` -- some fifty
///    call sites that would each have to carry a handle they have no other use
///    for.
/// 2. More importantly it keeps [`mark_session_tree_dirty`] *synchronous*. The
///    change points run while the `state`/`panes` guards are held; an async
///    broadcast there would relock them on the same task and deadlock, whereas
///    a bare `notify_one` cannot.
///
/// The coalescing falls out of `Notify`'s semantics: it stores at most one
/// permit, so any number of changes arriving while the pusher is asleep wake it
/// exactly once. The server is a singleton process, which is the scope a global
/// is correct at here.
static SESSION_TREE_DIRTY: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Record that the session tree has changed. Cheap and non-blocking; safe to
/// call while holding any server lock. With no subscribers the pusher wakes,
/// finds none, and parks again.
fn mark_session_tree_dirty() {
    SESSION_TREE_DIRTY.notify_one();
}

/// Build and send [`ServerMessage::SessionTree`] to each client in `targets`,
/// skipping ids that are no longer connected.
///
/// The expensive inputs -- the dormant name list, per-session client counts and
/// one `get_process_name` per live pane -- are snapshotted once and shared;
/// only `build_session_tree` runs per recipient, because `is_current` is
/// recipient-relative. Backpressure is a non-issue and deliberately so: `tx` is
/// an unbounded channel, so the send cannot block the daemon on a slow client,
/// and a send to a dead connection returns `Err`, which is discarded exactly as
/// [`broadcast_view_list`] does.
///
/// Locks in the codebase's `dormant` -> `state` -> `clients` -> `panes` order.
async fn send_session_tree_to(
    targets: &[u64],
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    dormant: &DormantStore,
) {
    if targets.is_empty() {
        return;
    }

    let dormant_names: Vec<String> = {
        let d = dormant.lock().await;
        let mut names: Vec<String> = d.state.sessions.keys().cloned().collect();
        names.sort();
        names
    };

    let st = state.lock().await;
    let cls = clients.lock().await;

    // Compute client counts per session.
    let mut client_counts: HashMap<String, usize> = HashMap::new();
    for c in cls.values() {
        if let Some(ref sn) = c.session_name {
            *client_counts.entry(sn.clone()).or_insert(0) += 1;
        }
    }

    // Compute pane names from PTY process names.
    let mut pane_names: HashMap<PaneId, String> = HashMap::new();
    let ps = panes.lock().await;
    for (&pid, pane) in ps.iter() {
        let name = get_process_name(pane.pty.child_pid.as_raw());
        pane_names.insert(pid, name);
    }

    // A user-set custom pane name (PaneRename) takes precedence over the
    // auto-detected process name, matching what the pane border shows.
    for sess in st.sessions.values() {
        for tab in &sess.tabs {
            for pane_id in layout::all_pane_ids(&tab.layout) {
                if let Some(Some(custom)) = layout::get_pane_custom_name(&tab.layout, pane_id) {
                    pane_names.insert(pane_id, custom);
                }
            }
        }
    }

    for &target in targets {
        let Some(client) = cls.get(&target) else {
            continue;
        };
        // `is_current` is per-recipient, so the tree is built per target from
        // the one shared snapshot above.
        let (folders, unfiled) =
            st.build_session_tree(client.session_name.as_deref(), &client_counts, &pane_names);
        let _ = client.tx.send(ServerMessage::SessionTree {
            folders,
            unfiled,
            dormant: dormant_names.clone(),
        });
    }
}

/// Push the current tree to every `SubscribeSessionTree` subscriber.
///
/// Subscribers are read here, at fire time, rather than captured when the
/// change was recorded, so a client that unsubscribed during the coalescing
/// window is simply not in the list. That narrows the race but does not close
/// it: an `UnsubscribeSessionTree` landing between this collect and the send
/// inside [`send_session_tree_to`] still gets one last `SessionTree`. Harmless
/// -- a stale tree is the same payload the client asked for a moment ago -- but
/// a client must tolerate one trailing push rather than treat it as a protocol
/// violation.
async fn broadcast_session_tree(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    dormant: &DormantStore,
) {
    let targets: Vec<u64> = {
        let cls = clients.lock().await;
        cls.iter()
            .filter(|(_, c)| c.session_tree_subscribed)
            .map(|(&id, _)| id)
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    send_session_tree_to(&targets, state, panes, clients, dormant).await;
}

async fn handle_list_session_tree(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    dormant: &DormantStore,
) -> Result<()> {
    send_session_tree_to(&[client_id], state, panes, clients, dormant).await;
    Ok(())
}

async fn handle_kill_session(
    name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let pane_ids = {
        let mut st = state.lock().await;
        st.delete_session(name)?
    };
    log::debug!(
        "server: KillSession name={name:?} panes_removed={}",
        pane_ids.len()
    );
    reap_panes(&pane_ids, panes, clients).await;
    let mut cls = clients.lock().await;
    for client in cls.values_mut() {
        if client.session_name.as_deref() == Some(name) {
            client.session_name = None;
            let _ = client
                .tx
                .send(ServerMessage::Event(SessionEvent::SessionDeleted(
                    name.to_string(),
                )));
        }
    }
    Ok(())
}

async fn handle_client_disconnect(
    client_id: u64,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    prev_frames: &PrevFrameCache,
) {
    log::debug!("server: client_id={client_id} disconnected, removing from client map");
    {
        let mut cls = clients.lock().await;
        cls.remove(&client_id);
    }
    // Drop this client's diff baseline so the per-client cache doesn't grow
    // unbounded as clients come and go.
    {
        let mut pf = prev_frames.lock().await;
        pf.remove(&client_id);
    }
    // Removing the `ClientConnection` above dropped any session-tree
    // subscription with it -- but it also changed the `client_count` every
    // *remaining* subscriber displays, so the survivors need a push.
    mark_session_tree_dirty();
}

async fn handle_request_scrollback(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let session_name = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => c.session_name.clone(),
            None => return Ok(()),
        }
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    // Find the active pane -- popup-aware, so search/copy-mode reads the popup's
    // own scrollback while the popup is the thing on screen.
    let active_pane_id = {
        let st = state.lock().await;
        match st
            .sessions
            .get(&session_name)
            .and_then(Session::input_target)
        {
            Some(p) => p,
            None => return Ok(()),
        }
    };

    // Read scrollback content from the pane's screen.
    let lines: Vec<String> = {
        let ps = panes.lock().await;
        match ps.get(&active_pane_id) {
            Some(pane_data) => {
                let content = pane_data.screen.scrollback_content();
                content.lines().map(|l| l.to_string()).collect()
            }
            None => Vec::new(),
        }
    };

    // Send back to client.
    let cls = clients.lock().await;
    if let Some(client) = cls.get(&client_id) {
        let _ = client.tx.send(ServerMessage::ScrollbackContent { lines });
    }

    Ok(())
}

async fn handle_search_info(
    client_id: u64,
    current: usize,
    total: usize,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    log::debug!("server: SearchInfo client_id={client_id} current={current} total={total}");
    let mut cls = clients.lock().await;
    if let Some(client) = cls.get_mut(&client_id) {
        if total == 0 {
            client.search_info = None;
        } else {
            client.search_info = Some((current, total));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_mode_changed(
    client_id: u64,
    mode: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    log::debug!("server: ModeChanged client_id={client_id} new_mode={mode:?}");
    let session_name = {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            client.mode = mode.to_string();
            // Clear search info when leaving search mode.
            if mode != "SEARCH" {
                client.search_info = None;
            }
            client.session_name.clone()
        } else {
            None
        }
    };

    if let Some(session_name) = session_name {
        broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mouse handling
// ---------------------------------------------------------------------------

/// Build a wheel mouse-report byte sequence for a pane with mouse tracking on.
///
/// Button code is 64 for wheel-up, 65 for wheel-down. `col`/`row` are
/// pane-relative 1-based coordinates. When `sgr` is true, the SGR (1006)
/// encoding is used; otherwise the legacy X10 encoding (offset +32, single
/// byte per field, saturated) is emitted.
fn wheel_report(sgr: bool, up: bool, col: u16, row: u16) -> Vec<u8> {
    let btn: u16 = if up { 64 } else { 65 };
    if sgr {
        format!("\x1b[<{btn};{col};{row}M").into_bytes()
    } else {
        let b = (32u32 + btn as u32).min(255) as u8;
        let c = (32u32 + col as u32).min(255) as u8;
        let r = (32u32 + row as u32).min(255) as u8;
        vec![0x1b, b'[', b'M', b, c, r]
    }
}

/// Build a single arrow-key escape sequence for the alternate-scroll fallback.
///
/// Up = `A`, Down = `B`. With `app_cursor` (DECCKM) the SS3 form `ESC O x` is
/// used, otherwise the CSI form `ESC [ x`.
fn arrow_report(app_cursor: bool, up: bool) -> Vec<u8> {
    let final_byte = if up { b'A' } else { b'B' };
    let mid = if app_cursor { b'O' } else { b'[' };
    vec![0x1b, mid, final_byte]
}

/// The three phases of a left-button gesture, as a mouse-tracking application
/// expects to see them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ButtonPhase {
    /// Button went down.
    Press,
    /// Pointer moved with the button held (only sent to apps that asked for
    /// motion, i.e. modes 1002/1003).
    Motion,
    /// Button came back up.
    Release,
}

/// Build a left-button mouse-report byte sequence, the [`wheel_report`] sibling
/// for press/motion/release. `col`/`row` are pane-relative 1-based coordinates.
///
/// SGR (1006) distinguishes press from release by the FINAL BYTE (`M` vs `m`)
/// and so keeps the button number on both; the legacy X10 encoding has no
/// lowercase form and reports a release as button 3 ("no button"). Motion adds
/// the +32 motion bit in both encodings.
fn button_report(sgr: bool, phase: ButtonPhase, col: u16, row: u16) -> Vec<u8> {
    // Left button = 0; motion sets bit 5; X10 spells a release as button 3.
    let btn: u16 = match phase {
        ButtonPhase::Press => 0,
        ButtonPhase::Motion => 32,
        ButtonPhase::Release if sgr => 0,
        ButtonPhase::Release => 3,
    };
    if sgr {
        let final_byte = if phase == ButtonPhase::Release {
            'm'
        } else {
            'M'
        };
        format!("\x1b[<{btn};{col};{row}{final_byte}").into_bytes()
    } else {
        let b = (32u32 + btn as u32).min(255) as u8;
        let c = (32u32 + col as u32).min(255) as u8;
        let r = (32u32 + row as u32).min(255) as u8;
        vec![0x1b, b'[', b'M', b, c, r]
    }
}

/// Which kind of mouse event is being routed. The two differ only in their
/// no-tracking fallback on the alternate screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MouseGesture {
    /// A wheel notch.
    Wheel,
    /// A left-button press, drag or release.
    Button,
}

/// What remux should do with a mouse event that landed on a pane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MouseRoute {
    /// Forward an encoded mouse report to the application. `sgr` selects the
    /// 1006 encoding over legacy X10; `motion` is whether the app asked for
    /// drag/motion reports (1002/1003) rather than presses only (1000).
    App { sgr: bool, motion: bool },
    /// Alternate-scroll fallback: an alt-screen application with no mouse
    /// tracking gets arrow keys for the wheel, since it has no scrollback of
    /// its own for remux to scroll.
    AltArrows { app_cursor: bool },
    /// Handle the event in remux: scrollback scrolling and text selection.
    ///
    /// `scrollback` is false on the alternate screen, where remux's scrollback
    /// holds the PRIMARY screen's history — text that has nothing to do with
    /// what the pane is showing. Dragging or wheeling into it would replace a
    /// full-screen app's display with unrelated history, and (because an
    /// alt-screen app's own output keeps feeding that scrollback) an edge
    /// auto-scroll aimed at it can never reach the end. So the caller must
    /// neither move the offset nor arm the repeat timer when this is false.
    Remux { scrollback: bool },
}

/// **The one mouse-routing decision in the server.** Every mouse path —
/// session-scoped (`MouseScroll`/`MouseClick`/`MouseDrag` with no `pane_id`)
/// and pane-scoped (`ScrollPane` and the `pane_id` forms, i.e. a View cell) —
/// asks this and nothing else, so a view and a session treat the same pane
/// identically.
///
/// Precedence matches what a terminal multiplexer is expected to do:
/// 0. `copy_mode` (remux's Visual mode) is an explicit "the mouse is MINE"
///    request from the user, so it outranks the application — the same reason
///    the client already refuses to forward the wheel in Visual mode;
/// 1. the application asked for mouse events (1000/1002/1003) — it gets them,
///    and remux does no selection or scrolling of its own;
/// 2. otherwise, on the alternate screen there is no meaningful remux
///    scrollback, so a wheel becomes arrow keys and a drag stays put;
/// 3. otherwise it is a plain shell: remux scrolls and selects as always.
fn mouse_route(screen: &Screen, gesture: MouseGesture, copy_mode: bool) -> MouseRoute {
    if copy_mode {
        // Selection still works over a full-screen app; only the scrolling that
        // would chase the primary screen's history is withheld.
        MouseRoute::Remux {
            scrollback: !screen.alt_screen_active,
        }
    } else if screen.mouse_tracking {
        MouseRoute::App {
            sgr: screen.mouse_sgr,
            motion: screen.mouse_motion,
        }
    } else if screen.alt_screen_active {
        match gesture {
            MouseGesture::Wheel => MouseRoute::AltArrows {
                app_cursor: screen.application_cursor_keys,
            },
            MouseGesture::Button => MouseRoute::Remux { scrollback: false },
        }
    } else {
        MouseRoute::Remux { scrollback: true }
    }
}

/// Map a full-screen coordinate into the 1-based, content-relative coordinates
/// a mouse report carries, applying the same border inset the compositor drew
/// the pane with. Shared by every session-scoped forwarding site; the
/// pane-scoped ones receive content coordinates already (the client owns a
/// View cell's geometry) and only add the 1-based bias.
fn report_coords(config: &Config, pane_rect: Rect, x: u16, y: u16) -> (u16, u16) {
    let border_offset: u16 = match config.appearance.border_style {
        BorderStyle::ZellijStyle if fits_zellij_border(pane_rect.width, pane_rect.height) => 1,
        _ => 0,
    };
    let col = x
        .saturating_sub(pane_rect.x + border_offset)
        .saturating_add(1);
    let row = y
        .saturating_sub(pane_rect.y + border_offset)
        .saturating_add(1);
    (col, row)
}

/// Write bytes to a pane's PTY, if the pane is still alive.
async fn write_to_pane(
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    pane_id: PaneId,
    bytes: &[u8],
) -> Result<()> {
    let ps = panes.lock().await;
    if let Some(pane_data) = ps.get(&pane_id) {
        pane_data.pty.write_input(bytes)?;
    }
    Ok(())
}

/// Read the routing decision for a pane, or `None` if the pane is gone.
async fn route_of_pane(
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    pane_id: PaneId,
    gesture: MouseGesture,
    copy_mode: bool,
) -> Option<MouseRoute> {
    let ps = panes.lock().await;
    ps.get(&pane_id)
        .map(|p| mouse_route(&p.screen, gesture, copy_mode))
}

/// remux's Visual mode is its copy-mode, and it claims the mouse (see
/// [`mouse_route`]). The client reports its mode with `ModeChanged` whether it
/// is attached or displaying a view, so this reads the same for both paths.
const COPY_MODE: &str = "VISUAL";

/// The scrollback search prompt. Like [`COPY_MODE`] it is an explicit trip into
/// history, so [`snap_client_to_live_tail`] leaves it alone.
const SEARCH_MODE: &str = "SEARCH";

/// Whether this client currently has the mouse to itself.
async fn client_in_copy_mode(
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    client_id: u64,
) -> bool {
    let cls = clients.lock().await;
    cls.get(&client_id).map(|c| c.mode.as_str()) == Some(COPY_MODE)
}

/// Forward one phase of a left-button gesture to a tracking application.
///
/// Motion is dropped for an application that only asked for press/release
/// (mode 1000). Press and release are always sent: an app that gets a press
/// without its release latches the button down.
async fn forward_button(
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    pane_id: PaneId,
    sgr: bool,
    motion: bool,
    phase: ButtonPhase,
    col: u16,
    row: u16,
) -> Result<()> {
    if phase == ButtonPhase::Motion && !motion {
        return Ok(());
    }
    let bytes = button_report(sgr, phase, col, row);
    log::debug!(
        "server: mouse->app pane_id={pane_id} phase={phase:?} sgr={sgr} col={col} row={row}"
    );
    write_to_pane(panes, pane_id, &bytes).await
}

/// Apply a scroll delta to the client's server-owned scroll offset, clamp it to
/// the valid range, and send a render if the offset changed. Shared by the
/// `ScrollDelta` message (keyboard/visual scrolling) and the plain-shell
/// fallback of `MouseScroll`.
#[allow(clippy::too_many_arguments)]
async fn handle_scroll_delta(
    client_id: u64,
    delta: i32,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    // Apply delta to server-owned scroll offset, clamp to valid range.
    let (session_name, old_offset) = {
        let cls = clients.lock().await;
        let sn = cls.get(&client_id).and_then(|c| c.session_name.clone());
        let so = cls.get(&client_id).map(|c| c.scroll_offset).unwrap_or(0);
        (sn, so)
    };
    let new_offset = if delta > 0 {
        old_offset.saturating_add(delta as usize)
    } else {
        old_offset.saturating_sub((-delta) as usize)
    };
    // The pane that owns input also owns this client's scroll offset (and any
    // active selection) -- so the popup scrolls while it is up. Resolved once,
    // reused for the clamp and the selection-extend.
    let focused_pane_id = match &session_name {
        Some(sn) => {
            let st = state.lock().await;
            st.sessions.get(sn).and_then(Session::input_target)
        }
        None => None,
    };
    // Clamp to max scrollable range
    let max_offset = if let Some(fp) = focused_pane_id {
        let ps = panes.lock().await;
        // Clamp against the focused pane's inner grid height
        // (screen.rows == grid.len()), which is exactly the number of content
        // rows blit_screen draws for the pane. Using the client terminal rows
        // here would over-subtract (it ignores the status bar and pane
        // borders), leaving the earliest scrollback lines unreachable at max
        // scroll.
        ps.get(&fp)
            .map(|p| p.screen.max_scroll_offset())
            .unwrap_or(0)
    } else {
        0
    };
    // Everything `max_scrollable` is made of, so a report of "it stops scrolling
    // too early" can be diagnosed from one log line instead of a round trip.
    // `max_scrollable` is exactly `scrollback.len()` (grid.len() == rows), so a
    // small value is always a small scrollback, never a clamp: `alt` tells you
    // the history is parked and deliberately unreachable, a `region` other than
    // 0..rows-1 tells you the application suppressed accumulation (only a scroll
    // region starting at row 0 feeds scrollback), and `evicted` tells you the
    // limit was hit.
    let detail = if let Some(fp) = focused_pane_id {
        let ps = panes.lock().await;
        ps.get(&fp)
            .map(|p| {
                let s = &p.screen;
                format!(
                    " pane_id={fp} alt={} sb={} evicted={} grid={} rows={} cols={} region={}..{}",
                    s.alt_screen_active,
                    s.scrollback.len(),
                    s.lines_evicted,
                    s.grid.len(),
                    s.rows,
                    s.cols,
                    s.scroll_top,
                    s.scroll_bottom,
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let clamped = new_offset.min(max_offset);
    let changed = clamped != old_offset;
    log::debug!(
        "server: ScrollDelta client_id={client_id} delta={delta} new_offset={clamped} max_scrollable={max_offset}{detail}"
    );
    {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            client.scroll_offset = clamped;
            client.needs_full_render = true;
        }
    }
    // If a mouse drag-selection is active on the focused pane, extend it to the
    // newly revealed edge so a scroll (mouse wheel or keyboard) grows the
    // selection to cover the scrolled-into text -- mirroring drag-autoscroll. The
    // anchor stays pinned in absolute coords; the moving end follows the scroll,
    // and both the highlight and the yankable range derive from that one range.
    if changed {
        if let Some(fp) = focused_pane_id {
            extend_selection_on_scroll(client_id, fp, delta > 0, clamped, panes, clients).await;
        }
    }
    // Only render if the offset actually changed (skip at boundary).
    if changed {
        if let Some(session_name) = session_name {
            send_full_render_to_client(
                client_id,
                &session_name,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await;
        }
    }
    Ok(())
}

/// Extend an active mouse drag-selection to the edge revealed by a scroll, so a
/// wheel/keyboard scroll while a selection is live grows it to cover the
/// scrolled-into text (mirroring drag-autoscroll). `up` is the scroll direction
/// (`true` = back into history, revealing the TOP edge). The anchor is left
/// pinned in absolute coords; only the moving end (`DragSession::end_abs`) and
/// the re-projected `MouseSelection` are updated. A no-op unless the client has a
/// drag gesture on `pane_id`. Locks `clients` then `panes` then `clients` again,
/// never nested, matching the surrounding lock discipline.
async fn extend_selection_on_scroll(
    client_id: u64,
    pane_id: PaneId,
    up: bool,
    new_offset: usize,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    // Read the anchor of any active drag on this pane.
    let anchor = {
        let cls = clients.lock().await;
        match cls.get(&client_id).and_then(|c| c.drag.as_ref()) {
            Some(d) if d.pane_id == pane_id => Some((d.anchor_col, d.anchor_abs)),
            _ => None,
        }
    };
    let (anchor_col, anchor_abs) = match anchor {
        Some(a) => a,
        None => return,
    };
    // Project the anchor and the revealed edge into the new viewport using the
    // pane's own geometry (rows/cols == the pane's content area).
    let projected = {
        let ps = panes.lock().await;
        ps.get(&pane_id).map(|pd| {
            let screen = &pd.screen;
            let rows = screen.rows;
            let cols = screen.cols;
            // The moving end pins to the revealed edge: top row when scrolling
            // back, bottom row when scrolling forward.
            let end_row = if up { 0 } else { rows.saturating_sub(1) };
            let end_col = if up { 0 } else { cols.saturating_sub(1) };
            let end_abs = screen.abs_of_row(new_offset, end_row);
            let anchor_row = screen
                .row_of_abs(new_offset, anchor_abs)
                .clamp(0, rows.saturating_sub(1) as i64) as u16;
            (end_row, end_col, end_abs, anchor_row)
        })
    };
    let (end_row, end_col, end_abs, anchor_row) = match projected {
        Some(v) => v,
        None => return,
    };
    // Commit the new end (absolute) and the re-projected viewport selection.
    let mut cls = clients.lock().await;
    if let Some(client) = cls.get_mut(&client_id) {
        if let Some(d) = client.drag.as_mut() {
            if d.pane_id == pane_id {
                d.end_abs = end_abs;
                d.end_col = end_col;
            }
        }
        client.mouse_selection = Some(MouseSelection {
            pane_id,
            start: (anchor_col, anchor_row),
            end: (end_col, end_row),
        });
    }
}

/// Keep a View cell's selection honest across a `ScrollPane` (wheel) step.
///
/// The pane-scoped counterpart of [`extend_selection_on_scroll`], with the extra
/// case that path does not have to handle:
///
/// * **A drag is in flight** -- the wheel EXTENDS the selection to the newly
///   revealed edge, exactly as a session drag does, so the highlight keeps
///   matching what a release would yank. The anchor is absolute, so it stays
///   pinned to its logical line.
/// * **No drag, but a selection is still up** (only possible with
///   `mouse_auto_yank = false`, which leaves the highlight for keyboard
///   adjustment) -- the selection is CLEARED. `MouseSelection` is
///   viewport-relative, so scrolling the content out from under it would leave
///   the grey block sitting on whatever text happened to land in those rows.
async fn rescope_pane_selection_on_scroll(
    client_id: u64,
    pane_id: PaneId,
    up: bool,
    new_offset: usize,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    let anchor = {
        let cls = clients.lock().await;
        match cls.get(&client_id).and_then(|c| c.pane_drag.as_ref()) {
            Some(d) if d.pane_id == pane_id => Some((d.anchor_col, d.anchor_abs)),
            _ => None,
        }
    };
    let (anchor_col, anchor_abs) = match anchor {
        Some(a) => a,
        None => {
            // No gesture to extend: a leftover highlight cannot survive the
            // scroll, so drop it rather than let it point at the wrong text.
            let mut cls = clients.lock().await;
            if let Some(client) = cls.get_mut(&client_id) {
                client.pane_selection.remove(&pane_id);
            }
            return;
        }
    };
    let projected = {
        let ps = panes.lock().await;
        ps.get(&pane_id).map(|pd| {
            let screen = &pd.screen;
            let (rows, cols) = (screen.rows, screen.cols);
            // The moving end pins to the revealed edge: the top row scrolling
            // back into history, the bottom row scrolling forward.
            let end_row = if up { 0 } else { rows.saturating_sub(1) };
            let end_col = if up { 0 } else { cols.saturating_sub(1) };
            let end_abs = screen.abs_of_row(new_offset, end_row);
            let anchor_row = screen
                .row_of_abs(new_offset, anchor_abs)
                .clamp(0, rows.saturating_sub(1) as i64) as u16;
            (end_row, end_col, end_abs, anchor_row)
        })
    };
    let (end_row, end_col, end_abs, anchor_row) = match projected {
        Some(v) => v,
        None => return,
    };
    let mut cls = clients.lock().await;
    if let Some(client) = cls.get_mut(&client_id) {
        if let Some(d) = client.pane_drag.as_mut() {
            if d.pane_id == pane_id {
                d.end_abs = end_abs;
                d.end_col = end_col;
            }
        }
        client.pane_selection.insert(
            pane_id,
            MouseSelection {
                pane_id,
                start: (anchor_col, anchor_row),
                end: (end_col, end_row),
            },
        );
    }
}

/// Route a mouse wheel event. If the pane under the cursor has mouse tracking
/// enabled, forward a wheel report to its application. Otherwise, if it is on
/// the alternate screen, emit the alternate-scroll arrow-key fallback. For a
/// plain shell, fall back to remux's own scrollback scroll.
#[allow(clippy::too_many_arguments)]
async fn handle_mouse_scroll(
    client_id: u64,
    x: u16,
    y: u16,
    up: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let (session_name, cols, rows, mode) = {
        let cls = clients.lock().await;
        let client = match cls.get(&client_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        (
            client.session_name.clone(),
            client.cols,
            client.rows,
            client.mode.clone(),
        )
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    // Build composite to get hit regions and pane rects.
    update_auto_pane_names(&session_name, state, panes).await;
    let (_cells, _cx, _cy, _cv, _cs, _fpr, hit_regions, pane_rects, _ack, popup) = build_composite(
        &session_name,
        cols,
        rows,
        &mode,
        state,
        panes,
        config,
        None,
        None,
        0,
        &config.compositor_theme(),
    )
    .await;

    // Find the pane under the cursor; fall back to whichever pane owns input
    // (the popup while it is up, else the active tab's focused pane).
    let target = hit_test_with_popup(x, y, &hit_regions, &pane_rects, popup);
    let target_pane = match target {
        ClickTarget::Pane(id) => id,
        _ => {
            let st = state.lock().await;
            match st
                .sessions
                .get(&session_name)
                .and_then(Session::input_target)
            {
                Some(fp) => fp,
                None => return Ok(()),
            }
        }
    };

    // Find the target pane's rect for coordinate mapping.
    let pane_rect = match rect_of_pane(&pane_rects, popup, target_pane) {
        Some(r) => r,
        None => return Ok(()),
    };

    // The shared routing decision (see `mouse_route`).
    let route =
        match route_of_pane(panes, target_pane, MouseGesture::Wheel, mode == COPY_MODE).await {
            Some(r) => r,
            None => return Ok(()),
        };

    match route {
        MouseRoute::App { sgr, .. } => {
            let (col, row) = report_coords(config, pane_rect, x, y);
            let bytes = wheel_report(sgr, up, col, row);
            log::debug!(
                "server: MouseScroll->app client_id={client_id} pane_id={target_pane} sgr={sgr} up={up} col={col} row={row}"
            );
            write_to_pane(panes, target_pane, &bytes).await
        }
        MouseRoute::AltArrows { app_cursor } => {
            // Alternate-scroll fallback: three arrow keys per wheel notch.
            let bytes = alt_scroll_arrows(app_cursor, up, WHEEL_LINES);
            log::debug!(
                "server: MouseScroll->alt-arrows client_id={client_id} pane_id={target_pane} app_cursor={app_cursor} up={up}"
            );
            write_to_pane(panes, target_pane, &bytes).await
        }
        MouseRoute::Remux { .. } => {
            // Plain shell: preserve remux's own scrollback scroll (delta ±3).
            let delta = if up {
                WHEEL_LINES as i32
            } else {
                -(WHEEL_LINES as i32)
            };
            log::debug!("server: MouseScroll->remux-scroll client_id={client_id} delta={delta}");
            handle_scroll_delta(client_id, delta, state, panes, clients, config, prev_frames).await
        }
    }
}

/// Lines a single wheel notch moves — the scrollback delta remux applies itself,
/// and the number of arrow keys the alternate-scroll fallback sends.
const WHEEL_LINES: u16 = 3;

/// `lines` repetitions of the alternate-scroll arrow key.
fn alt_scroll_arrows(app_cursor: bool, up: bool, lines: u16) -> Vec<u8> {
    let one = arrow_report(app_cursor, up);
    let mut bytes = Vec::with_capacity(one.len() * lines as usize);
    for _ in 0..lines {
        bytes.extend_from_slice(&one);
    }
    bytes
}

/// Popup-aware hit test: **the mouse chokepoint.**
///
/// The popup floats above everything, so any coordinate inside its rect resolves
/// to the popup pane and is checked BEFORE the layout's own regions. Doing the
/// popup check here (rather than splicing the popup into `pane_rects`) is what
/// lets `pane_rects` keep meaning "the layout's rects, popup-independent".
fn hit_test_with_popup(
    x: u16,
    y: u16,
    regions: &HitRegions,
    pane_rects: &[(PaneId, Rect)],
    popup: Option<(PaneId, Rect)>,
) -> ClickTarget {
    if let Some((pane_id, r)) = popup {
        if x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height {
            return ClickTarget::Pane(pane_id);
        }
    }
    hit_test(x, y, regions, pane_rects)
}

/// The screen rect of `pane_id` for coordinate mapping, popup included. The
/// popup is never in `pane_rects`, so it must be resolved separately.
fn rect_of_pane(
    pane_rects: &[(PaneId, Rect)],
    popup: Option<(PaneId, Rect)>,
    pane_id: PaneId,
) -> Option<Rect> {
    if let Some((pid, r)) = popup {
        if pid == pane_id {
            return Some(r);
        }
    }
    pane_rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .map(|(_, r)| *r)
}

/// Disarm the drag-autoscroll repeat timer for a client. Called from the early
/// returns in [`handle_mouse_drag`] that can be reached while a drag is armed
/// (the target pane's process exited, the drag no longer hits a pane, etc.), so
/// the ticker task stops re-invoking a drag that can never make progress instead
/// of spinning at the tick rate.
async fn disarm_autoscroll(clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>, client_id: u64) {
    let mut cls = clients.lock().await;
    if let Some(c) = cls.get_mut(&client_id) {
        c.autoscroll_repeat = None;
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_mouse_click(
    client_id: u64,
    x: u16,
    y: u16,
    release: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let (session_name, cols, rows, mode) = {
        let mut cls = clients.lock().await;
        let client = match cls.get_mut(&client_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        // Clear any active selection on press. Also end any in-progress drag
        // gesture so the next drag starts a fresh anchor (a click always begins
        // a new selection in the same or a different pane). A RELEASE never
        // clears: with `mouse_auto_yank = false` the drag that just ended left a
        // highlight up on purpose, and wiping it here would undo that.
        if !release {
            client.mouse_selection = None;
            client.drag = None;
        }
        // Either half of a click ends any in-progress edge auto-scroll.
        client.autoscroll_repeat = None;
        (
            client.session_name.clone(),
            client.cols,
            client.rows,
            client.mode.clone(),
        )
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    // Build composite to get hit regions and pane rects.
    update_auto_pane_names(&session_name, state, panes).await;
    let (_cells, _cx, _cy, _cv, _cs, _fpr, hit_regions, pane_rects, _ack, popup) = build_composite(
        &session_name,
        cols,
        rows,
        &mode,
        state,
        panes,
        config,
        None,
        None,
        0,
        &config.compositor_theme(),
    )
    .await;

    let target = hit_test_with_popup(x, y, &hit_regions, &pane_rects, popup);
    log::debug!(
        "server: MouseClick client_id={client_id} x={x} y={y} release={release} target={target:?}"
    );

    // A pane whose application asked for mouse events gets the press/release
    // itself (the same policy the wheel and a View cell's click follow). Focus
    // still moves first, exactly as tmux does: clicking a pane selects it AND
    // the application sees the click. While the popup is up it owns input, so
    // only the popup's own pane may be driven -- the same rule the focus arms
    // below apply, and forwarding to a pane hidden behind the popup would break
    // it.
    let popup_pane = popup.map(|(pid, _)| pid);
    if let ClickTarget::Pane(pane_id) = target {
        if popup_pane.is_none() || popup_pane == Some(pane_id) {
            if let (Some(MouseRoute::App { sgr, motion }), Some(rect)) = (
                route_of_pane(panes, pane_id, MouseGesture::Button, mode == COPY_MODE).await,
                rect_of_pane(&pane_rects, popup, pane_id),
            ) {
                let (col, row) = report_coords(config, rect, x, y);
                let phase = if release {
                    ButtonPhase::Release
                } else {
                    ButtonPhase::Press
                };
                forward_button(panes, pane_id, sgr, motion, phase, col, row).await?;
            }
        }
    }
    // A release only forwards; it must not re-run the focus/tab side effects a
    // press already applied.
    if release {
        return Ok(());
    }

    match target {
        // While the popup is up it already owns input, and moving the layout's
        // focus underneath it would silently change where input lands once the
        // popup hides. So pane/stack-label clicks are no-ops (stack labels also
        // mutate the layout tree). Tab clicks still work: the popup is
        // session-scoped and deliberately survives a tab switch.
        ClickTarget::Pane(_) | ClickTarget::StackLabel(_) if popup.is_some() => {}
        ClickTarget::Pane(pane_id) => {
            let mut st = state.lock().await;
            let sess = match st.sessions.get_mut(&session_name) {
                Some(s) => s,
                None => return Ok(()),
            };
            let tab = match sess.tabs.get_mut(sess.active_tab) {
                Some(t) => t,
                None => return Ok(()),
            };
            if tab.focused_pane != pane_id {
                tab.focus_pane(pane_id);
                drop(st);
                // `PaneTreeEntry::is_focused` is in the pushed payload, so
                // click-to-focus is a tree change exactly as keyboard focus is.
                mark_session_tree_dirty();
                broadcast_full_render(&session_name, state, panes, clients, config, prev_frames)
                    .await;
            }
        }
        ClickTarget::Tab(tab_index) => {
            {
                let mut st = state.lock().await;
                let _ = st.goto_tab(&session_name, tab_index);
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        ClickTarget::StackLabel(pane_id) => {
            // Activate the stacked pane.
            let mut st = state.lock().await;
            let sess = match st.sessions.get_mut(&session_name) {
                Some(s) => s,
                None => return Ok(()),
            };
            let tab = match sess.tabs.get_mut(sess.active_tab) {
                Some(t) => t,
                None => return Ok(()),
            };
            // Walk layout to find the stack containing pane_id and set it active.
            activate_pane_in_stack(&mut tab.layout, pane_id);
            tab.focus_pane(pane_id);
            drop(st);
            // Changes both the stack's active pane and `is_focused`; both are
            // in the pushed payload.
            mark_session_tree_dirty();
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        ClickTarget::None => {}
    }

    Ok(())
}

/// Pure decision for the layout cycle's next mode, factored out for testing.
///
/// The automatic cycle is `Bsp -> Master -> Monocle -> Grid -> Bsp`. Two
/// custom-aware detours ride on top: leaving `Custom` starts the automatic
/// cycle at `Bsp`, and reaching `Grid` (the last automatic before wrap) returns
/// to `Custom` when a restorable custom layout was remembered
/// (`has_saved_custom`).
fn next_layout_mode(current: &LayoutMode, has_saved_custom: bool) -> LayoutMode {
    match current {
        LayoutMode::Custom(_) => LayoutMode::Bsp(BspLayout),
        LayoutMode::Grid(_) if has_saved_custom => LayoutMode::Custom(CustomLayout),
        other => other.next(),
    }
}

/// Return true if `saved` can be restored as the current custom layout, i.e.
/// it exists and its leaf pane-id set exactly equals the live `pane_order`.
///
/// The comparison is order-independent: the saved tree's traversal order need
/// not match `pane_order`'s insertion order. A mismatch (panes added/removed
/// while in an automatic layout) makes the snapshot stale and non-restorable.
fn saved_custom_is_restorable(saved: &Option<LayoutNode>, pane_order: &[PaneId]) -> bool {
    let Some(tree) = saved else {
        return false;
    };
    let mut saved_ids = layout::all_pane_ids(tree);
    saved_ids.sort_unstable();
    let mut live_ids = pane_order.to_vec();
    live_ids.sort_unstable();
    saved_ids == live_ids
}

/// Activate a specific pane within its stack in the layout tree.
fn activate_pane_in_stack(node: &mut layout::LayoutNode, pane_id: PaneId) {
    match node {
        layout::LayoutNode::Stack { panes, active, .. } => {
            if let Some(pos) = panes.iter().position(|&p| p == pane_id) {
                *active = pos;
            }
        }
        layout::LayoutNode::Split { first, second, .. } => {
            activate_pane_in_stack(first, pane_id);
            activate_pane_in_stack(second, pane_id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_mouse_drag(
    client_id: u64,
    start_x: u16,
    start_y: u16,
    end_x: u16,
    end_y: u16,
    is_final: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    log::debug!(
        "server: MouseDrag client_id={client_id} start=({start_x},{start_y}) end=({end_x},{end_y}) is_final={is_final}"
    );
    let (session_name, cols, rows, mode) = {
        let cls = clients.lock().await;
        let client = match cls.get(&client_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        (
            client.session_name.clone(),
            client.cols,
            client.rows,
            client.mode.clone(),
        )
    };
    let session_name = match session_name {
        Some(s) => s,
        None => {
            disarm_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };

    // Build composite to get pane rects for coordinate mapping.
    update_auto_pane_names(&session_name, state, panes).await;
    let (_cells, _cx, _cy, _cv, _cs, _fpr, hit_regions, pane_rects, _ack, popup) = build_composite(
        &session_name,
        cols,
        rows,
        &mode,
        state,
        panes,
        config,
        None,
        None,
        0,
        &config.compositor_theme(),
    )
    .await;

    // Find which pane the drag started in (the popup first, when it is up).
    let start_target = hit_test_with_popup(start_x, start_y, &hit_regions, &pane_rects, popup);
    let target_pane = match start_target {
        ClickTarget::Pane(id) => id,
        _ => {
            disarm_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };

    // Find the pane's rect for coordinate mapping.
    let pane_rect = match rect_of_pane(&pane_rects, popup, target_pane) {
        Some(r) => r,
        None => {
            disarm_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };

    // The shared routing decision (see `mouse_route`). An application that asked
    // for mouse events owns the gesture: it gets the motion/release report and
    // remux selects nothing, so the same drag that would highlight text in a
    // shell drives the application's own selection instead.
    let route =
        match route_of_pane(panes, target_pane, MouseGesture::Button, mode == COPY_MODE).await {
            Some(r) => r,
            None => {
                disarm_autoscroll(clients, client_id).await;
                return Ok(());
            }
        };
    if let MouseRoute::App { sgr, motion } = route {
        disarm_autoscroll(clients, client_id).await;
        let (col, row) = report_coords(config, pane_rect, end_x, end_y);
        let phase = if is_final {
            ButtonPhase::Release
        } else {
            ButtonPhase::Motion
        };
        return forward_button(panes, target_pane, sgr, motion, phase, col, row).await;
    }
    // False on the alternate screen: remux's scrollback belongs to the PRIMARY
    // screen there, so an edge drag must neither scroll into it nor arm the
    // repeat timer for a scroll that can never finish.
    let may_scroll = matches!(route, MouseRoute::Remux { scrollback: true });

    // Compute border offset based on style.
    let border_offset: u16 = match config.appearance.border_style {
        BorderStyle::ZellijStyle if fits_zellij_border(pane_rect.width, pane_rect.height) => 1,
        _ => 0,
    };
    let content_width = pane_rect.width.saturating_sub(border_offset * 2);
    let content_height = pane_rect.height.saturating_sub(border_offset * 2);

    // Map screen coordinates to pane-local coordinates (relative to content
    // area). Columns clamp to the content width; the start row clamps into the
    // content area too, but the end row is kept both raw (for edge detection)
    // and as a clamped display row.
    let local_start_x = start_x
        .saturating_sub(pane_rect.x + border_offset)
        .min(content_width.saturating_sub(1));
    let local_start_y = start_y
        .saturating_sub(pane_rect.y + border_offset)
        .min(content_height.saturating_sub(1));
    let local_end_x = end_x
        .saturating_sub(pane_rect.x + border_offset)
        .min(content_width.saturating_sub(1));
    let raw_end_row = end_y.saturating_sub(pane_rect.y + border_offset);
    let end_row = raw_end_row.min(content_height.saturating_sub(1));

    // Edge detection: is the drag end on the top or bottom content row?
    let at_top = end_row == 0;
    let at_bottom = content_height > 0 && end_row == content_height - 1;

    // Auto-scroll only applies when the drag target is the client's FOCUSED
    // pane -- the per-client scroll_offset belongs to that pane. A non-focused
    // target always renders at the live view (offset 0).
    let focused_pane = {
        let st = state.lock().await;
        st.sessions
            .get(&session_name)
            .and_then(|s| s.tabs.get(s.active_tab).map(|t| t.focused_pane))
    };
    let is_focused = focused_pane == Some(target_pane);

    // Read the client's current scroll offset and the state of any in-progress
    // drag gesture.
    let (scroll_offset, gesture) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => (
                c.scroll_offset,
                c.drag
                    .as_ref()
                    .map(|d| (d.pane_id, d.anchor_col, d.anchor_abs)),
            ),
            None => return Ok(()),
        }
    };
    // A drag into a different pane (or with no active gesture) starts fresh.
    let new_gesture = match gesture {
        Some((pid, _, _)) => pid != target_pane,
        None => true,
    };
    let base_offset = if is_focused { scroll_offset } else { 0 };

    // Compute the new scroll offset (focused pane only), the anchor in absolute
    // coordinates, the end point in absolute coordinates, and the anchor's row
    // projected back into the current viewport. abs_of_row / row_of_abs are pure
    // over (scrollback len, eviction count, rows), so this is one lock scope.
    // Yields `None` (via labeled break) if the target pane vanished, so the
    // panes lock is released before we disarm autoscroll (which locks `clients`)
    // -- avoids nesting the two locks.
    let computed = 'compute: {
        let ps = panes.lock().await;
        let screen = match ps.get(&target_pane) {
            Some(p) => &p.screen,
            None => break 'compute None,
        };

        // Max scroll offset (top of scrollback); also gates whether edge
        // auto-scroll should keep repeating (see `autoscroll_repeat` below).
        let screen_max_scroll_offset = screen.max_scroll_offset();

        // Anchor: reuse the stored one, or capture a fresh anchor from the drag
        // start (in the pre-scroll view) for a new gesture.
        let (anchor_col, anchor_abs) = if new_gesture {
            (local_start_x, screen.abs_of_row(base_offset, local_start_y))
        } else {
            let (_, col, abs) = gesture.unwrap();
            (col, abs)
        };

        // Edge auto-scroll, focused pane only. A final drag (button release) never
        // scrolls: the selection must end exactly where the user let go, so the
        // yanked range matches the highlight shown at release instead of pulling
        // in one extra edge line.
        let new_offset = if is_focused && !is_final && may_scroll {
            if at_top {
                (scroll_offset + 1).min(screen_max_scroll_offset)
            } else if at_bottom {
                scroll_offset.saturating_sub(1)
            } else {
                scroll_offset
            }
        } else {
            scroll_offset
        };
        let new_base = if is_focused { new_offset } else { 0 };

        let end_abs = screen.abs_of_row(new_base, end_row);
        let anchor_row_i64 = screen.row_of_abs(new_base, anchor_abs);
        Some((
            new_offset,
            anchor_col,
            anchor_abs,
            end_abs,
            anchor_row_i64,
            screen_max_scroll_offset,
        ))
    };
    let (new_offset, anchor_col, anchor_abs, end_abs, anchor_row_i64, screen_max_scroll_offset) =
        match computed {
            Some(v) => v,
            None => {
                disarm_autoscroll(clients, client_id).await;
                return Ok(());
            }
        };

    // Project the anchor into the current viewport for the (viewport-relative)
    // MouseSelection: an anchor scrolled above the viewport clamps to row 0,
    // one scrolled below clamps to the last content row.
    let anchor_row = anchor_row_i64.clamp(0, content_height.saturating_sub(1) as i64) as u16;
    let end_col = local_end_x;

    // Commit the gesture, the new scroll offset, and the derived selection.
    let offset_changed = is_focused && new_offset != scroll_offset;
    {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            if new_gesture {
                client.drag = Some(DragSession {
                    pane_id: target_pane,
                    anchor_col,
                    anchor_abs,
                    end_abs,
                    end_col,
                });
            } else if let Some(d) = client.drag.as_mut() {
                // Continuing gesture: keep the moving end in absolute coords so a
                // wheel-scroll (which has no mouse coords) can extend from it.
                d.end_abs = end_abs;
                d.end_col = end_col;
            }
            if is_focused {
                client.scroll_offset = new_offset;
                if offset_changed {
                    client.needs_full_render = true;
                }
            }
            client.mouse_selection = Some(MouseSelection {
                pane_id: target_pane,
                start: (anchor_col, anchor_row),
                end: (end_col, end_row),
            });
            // Arm/disarm the repeating edge-scroll timer. Keep firing only while
            // resting on a focused-pane edge that still has room to scroll;
            // disarm exactly at the scrollback top / live bottom so the timer
            // stops instead of spinning. A final drag never arms.
            client.autoscroll_repeat = if is_focused
                && !is_final
                && may_scroll
                && ((at_top && new_offset < screen_max_scroll_offset)
                    || (at_bottom && new_offset > 0))
            {
                Some((start_x, start_y, end_x, end_y))
            } else {
                None
            };
        }
    }

    if is_final {
        // Mouse button released -- decide based on mouse_auto_yank config.
        if config.general.mouse_auto_yank {
            // Extract text over the absolute selection range so the yank is
            // correct even when it spans scrollback.
            let selected_text = {
                let ps = panes.lock().await;
                if let Some(pane_data) = ps.get(&target_pane) {
                    extract_selection_text(
                        &pane_data.screen,
                        anchor_col,
                        anchor_abs,
                        end_col,
                        end_abs,
                    )
                } else {
                    String::new()
                }
            };

            if !selected_text.is_empty() {
                let cls = clients.lock().await;
                if let Some(client) = cls.get(&client_id) {
                    let _ = client.tx.send(ServerMessage::CopyToClipboard {
                        data: selected_text,
                    });
                }
            }

            // Clear selection state after copying.
            {
                let mut cls = clients.lock().await;
                if let Some(client) = cls.get_mut(&client_id) {
                    client.mouse_selection = None;
                }
            }
        }
        // When mouse_auto_yank is false, selection stays visible for keyboard
        // adjustment in visual mode. No copy, no clear.

        // Always end the drag gesture on release.
        {
            let mut cls = clients.lock().await;
            if let Some(client) = cls.get_mut(&client_id) {
                client.drag = None;
            }
        }
    }

    // Re-render for THIS client so its scrolled view + selection show. Use the
    // per-client path (like handle_scroll_delta): broadcast_full_render renders
    // at offset 0 and would not reflect this client's auto-scrolled viewport.
    send_full_render_to_client(
        client_id,
        &session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
    )
    .await;

    Ok(())
}

/// Disarm the pane-scoped drag-autoscroll repeat timer for a client. The
/// [`disarm_autoscroll`] analog for a View cell's gesture.
async fn disarm_pane_autoscroll(
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    client_id: u64,
) {
    let mut cls = clients.lock().await;
    if let Some(c) = cls.get_mut(&client_id) {
        c.pane_autoscroll_repeat = None;
    }
}

/// A left-click inside a View cell: clear that cell's selection and end any
/// gesture on it, so the drag that follows starts from a fresh anchor.
///
/// Sent on the press, and also on a release that never moved -- which is a click,
/// not a selection. That second case is not just tidiness: it is what disarms the
/// repeat timer for a gesture that wandered onto a content edge and came back, so
/// the cell stops auto-scrolling once the button is up. The disarm therefore
/// happens unconditionally; only the (comparatively costly) repaint is skipped
/// when there was no selection to clear.
///
/// The pane-scoped counterpart of [`handle_mouse_click`]. There is no hit
/// testing to do -- the client already resolved which cell was clicked against
/// its own cell rects -- and no focus to move: cell focus is shared view state
/// driven by `ViewSetFocus`, not by this message.
#[allow(clippy::too_many_arguments)]
async fn handle_pane_mouse_click(
    client_id: u64,
    pane_id: PaneId,
    x: u16,
    y: u16,
    release: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let (had_selection, copy_mode) = {
        let mut cls = clients.lock().await;
        match cls.get_mut(&client_id) {
            // Only a subscribed client may drive a pane it watches (same guard
            // as `ScrollPane`).
            Some(conn) if conn.subscribed_panes.contains_key(&pane_id) => {
                let copy_mode = conn.mode == COPY_MODE;
                // A release leaves an intentionally-kept highlight alone; see
                // the same rule in `handle_mouse_click`.
                let had = if release {
                    false
                } else {
                    conn.pane_selection.remove(&pane_id).is_some()
                };
                if !release && conn.pane_drag.as_ref().map(|d| d.pane_id) == Some(pane_id) {
                    conn.pane_drag = None;
                }
                conn.pane_autoscroll_repeat = None;
                (had, copy_mode)
            }
            _ => return Ok(()),
        }
    };
    // Same policy as a session pane: an application that asked for mouse events
    // gets the press/release. Cell coordinates arrive content-relative, so they
    // only need the 1-based bias a report carries.
    if let Some(MouseRoute::App { sgr, motion }) =
        route_of_pane(panes, pane_id, MouseGesture::Button, copy_mode).await
    {
        // A highlight left over from before the application claimed the mouse
        // still has to be repainted away.
        if had_selection {
            stream_pane_content(pane_id, state, panes, clients).await;
        }
        let phase = if release {
            ButtonPhase::Release
        } else {
            ButtonPhase::Press
        };
        return forward_button(panes, pane_id, sgr, motion, phase, x + 1, y + 1).await;
    }
    // Only repaint when something actually changed: a plain click-to-focus in a
    // view is the common case and should cost no extra frame.
    if had_selection {
        stream_pane_content(pane_id, state, panes, clients).await;
    }
    Ok(())
}

/// A left-drag inside a View cell: select text in that cell's source pane.
///
/// The pane-scoped counterpart of [`handle_mouse_drag`], and deliberately built
/// from the same pieces -- [`Screen::abs_of_row`]/[`Screen::row_of_abs`] for an
/// eviction-stable anchor, [`extract_selection_text`] for the yank, and
/// `mouse_auto_yank` for the release semantics -- so a cell selects, scrolls and
/// copies exactly like a normal pane does. The differences are all consequences
/// of the client being *detached* while a view is up:
///
/// * coordinates arrive already content-relative (the client owns the cell
///   geometry; the server has no layout rect for a cell), so there is no
///   composite/hit-test step;
/// * the scroll offset auto-scroll moves is the per-(client, pane) `pane_scroll`
///   that the wheel already drives, not the foreground `scroll_offset`;
/// * the repaint is a per-subscriber `PaneContent`, not a session frame.
#[allow(clippy::too_many_arguments)]
async fn handle_pane_mouse_drag(
    client_id: u64,
    pane_id: PaneId,
    start_x: u16,
    start_y: u16,
    end_x: u16,
    end_y: u16,
    is_final: bool,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) -> Result<()> {
    // Current per-(client, pane) offset and the state of any gesture already
    // running on this pane. A drag on a pane this client does not subscribe to
    // is ignored, mirroring `ScrollPane`.
    let (scroll_offset, gesture, copy_mode) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) if c.subscribed_panes.contains_key(&pane_id) => (
                c.pane_scroll.get(&pane_id).copied().unwrap_or(0),
                c.pane_drag
                    .as_ref()
                    .filter(|d| d.pane_id == pane_id)
                    .map(|d| (d.anchor_col, d.anchor_abs)),
                c.mode == COPY_MODE,
            ),
            _ => {
                disarm_pane_autoscroll(clients, client_id).await;
                return Ok(());
            }
        }
    };
    let new_gesture = gesture.is_none();

    // The shared routing decision (see `mouse_route`), identical to the one the
    // session path makes -- which is the whole point: a cell aliasing a pane
    // running a mouse-aware application drives it, instead of selecting text
    // over the top of it.
    let route = match route_of_pane(panes, pane_id, MouseGesture::Button, copy_mode).await {
        Some(r) => r,
        None => {
            disarm_pane_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };
    if let MouseRoute::App { sgr, motion } = route {
        disarm_pane_autoscroll(clients, client_id).await;
        let phase = if is_final {
            ButtonPhase::Release
        } else {
            ButtonPhase::Motion
        };
        return forward_button(panes, pane_id, sgr, motion, phase, end_x + 1, end_y + 1).await;
    }
    // No scrolling into the primary screen's history from an alt-screen cell,
    // and therefore no repeat timer either (see `MouseRoute::Remux`).
    let may_scroll = matches!(route, MouseRoute::Remux { scrollback: true });

    // Everything that needs the pane's screen, in one lock scope: the content
    // size to clamp against, the anchor/end in absolute coordinates, and the
    // scroll bound that gates the repeat timer.
    let computed = 'compute: {
        let ps = panes.lock().await;
        let screen = match ps.get(&pane_id) {
            Some(pd) => &pd.screen,
            None => break 'compute None,
        };
        let content_width = screen.cols;
        let content_height = screen.rows;
        if content_width == 0 || content_height == 0 {
            break 'compute None;
        }
        let max_scroll = screen.max_scroll_offset();
        // The stored offset can outlive the scrollback it addressed; clamp it
        // before deriving anything from it (`stream_pane_content` clamps the
        // same way when rendering).
        let base_offset = scroll_offset.min(max_scroll);

        let local_start_x = start_x.min(content_width - 1);
        let local_start_y = start_y.min(content_height - 1);
        let end_col = end_x.min(content_width - 1);
        let end_row = end_y.min(content_height - 1);

        // Edge auto-scroll: resting on the top/bottom content row pulls history
        // in. A final drag (release) never scrolls -- the yanked range must match
        // the highlight the user saw when letting go.
        let at_top = end_row == 0;
        let at_bottom = end_row == content_height - 1;
        let new_offset = if is_final || !may_scroll {
            base_offset
        } else if at_top {
            (base_offset + 1).min(max_scroll)
        } else if at_bottom {
            base_offset.saturating_sub(1)
        } else {
            base_offset
        };

        // Anchor in eviction-stable absolute coordinates, captured in the
        // PRE-scroll view for a fresh gesture and reused thereafter, so
        // auto-scrolling under the pointer never drags the anchor along.
        let (anchor_col, anchor_abs) = match gesture {
            Some(g) => g,
            None => (local_start_x, screen.abs_of_row(base_offset, local_start_y)),
        };
        let end_abs = screen.abs_of_row(new_offset, end_row);
        let anchor_row_i64 = screen.row_of_abs(new_offset, anchor_abs);
        // Project the anchor back into the post-scroll viewport for the
        // (viewport-relative) highlight: an anchor scrolled off the top clamps
        // to row 0, one below the fold to the last content row.
        let anchor_row = anchor_row_i64.clamp(0, content_height as i64 - 1) as u16;
        Some((
            new_offset, max_scroll, anchor_col, anchor_abs, anchor_row, end_col, end_row, end_abs,
            at_top, at_bottom,
        ))
    };
    let (
        new_offset,
        max_scroll,
        anchor_col,
        anchor_abs,
        anchor_row,
        end_col,
        end_row,
        end_abs,
        at_top,
        at_bottom,
    ) = match computed {
        Some(v) => v,
        None => {
            disarm_pane_autoscroll(clients, client_id).await;
            return Ok(());
        }
    };

    // Commit the gesture, the new offset and the derived highlight.
    {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            if new_gesture {
                client.pane_drag = Some(DragSession {
                    pane_id,
                    anchor_col,
                    anchor_abs,
                    end_abs,
                    end_col,
                });
            } else if let Some(d) = client.pane_drag.as_mut() {
                d.end_abs = end_abs;
                d.end_col = end_col;
            }
            if new_offset == 0 {
                client.pane_scroll.remove(&pane_id);
            } else {
                client.pane_scroll.insert(pane_id, new_offset);
            }
            client.pane_selection.insert(
                pane_id,
                MouseSelection {
                    pane_id,
                    start: (anchor_col, anchor_row),
                    end: (end_col, end_row),
                },
            );
            // Keep the repeat timer firing only while resting on an edge that
            // still has somewhere to go, so it stops at the scrollback top /
            // live bottom instead of spinning.
            client.pane_autoscroll_repeat = if !is_final
                && may_scroll
                && ((at_top && new_offset < max_scroll) || (at_bottom && new_offset > 0))
            {
                Some((pane_id, start_x, start_y, end_x, end_y))
            } else {
                None
            };
        }
    }

    if is_final {
        if config.general.mouse_auto_yank {
            // Extract over the ABSOLUTE range so a selection dragged through
            // scrollback yanks what it covered, not what is on screen now.
            let selected_text = {
                let ps = panes.lock().await;
                match ps.get(&pane_id) {
                    Some(pd) => {
                        extract_selection_text(&pd.screen, anchor_col, anchor_abs, end_col, end_abs)
                    }
                    None => String::new(),
                }
            };
            let mut cls = clients.lock().await;
            if let Some(client) = cls.get_mut(&client_id) {
                if !selected_text.is_empty() {
                    let _ = client.tx.send(ServerMessage::CopyToClipboard {
                        data: selected_text,
                    });
                }
                client.pane_selection.remove(&pane_id);
            }
        }
        // With mouse_auto_yank off the highlight stays up for keyboard
        // adjustment, exactly as in a normal pane. Either way the gesture ends.
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            client.pane_drag = None;
        }
    }

    // Repaint: every subscriber renders at its own offset/selection, so this
    // shows the highlight (and any auto-scroll) to this client alone.
    stream_pane_content(pane_id, state, panes, clients).await;
    Ok(())
}

/// Extract text from a pane's screen buffer between two selection endpoints
/// given in absolute, eviction-stable line coordinates (see
/// [`Screen::abs_of_row`]). Working in absolute space means the yank is correct
/// even when the selection spans scrollback that has scrolled out of the
/// viewport during a drag.
fn extract_selection_text(
    screen: &Screen,
    anchor_col: u16,
    anchor_abs: usize,
    end_col: u16,
    end_abs: usize,
) -> String {
    // Normalize so the earlier point (in reading order) comes first.
    let (first_abs, first_col, last_abs, last_col) =
        if (anchor_abs, anchor_col) <= (end_abs, end_col) {
            (anchor_abs, anchor_col, end_abs, end_col)
        } else {
            (end_abs, end_col, anchor_abs, anchor_col)
        };

    let mut result = String::new();
    let mut first_line = true;
    for abs in first_abs..=last_abs {
        let row_data = match screen.line_at(screen.array_index_of_abs(abs)) {
            Some(r) => r,
            None => continue,
        };
        let col_start = if abs == first_abs {
            first_col as usize
        } else {
            0
        };
        let col_end = if abs == last_abs {
            (last_col as usize + 1).min(row_data.len())
        } else {
            row_data.len()
        };
        let col_start = col_start.min(row_data.len());
        let col_end = col_end.max(col_start);

        let text: String = row_data[col_start..col_end].iter().map(|c| c.c).collect();
        if !first_line {
            result.push('\n');
        }
        first_line = false;
        result.push_str(text.trim_end());
    }
    result
}

// ---------------------------------------------------------------------------
// Pane management helpers
// ---------------------------------------------------------------------------

/// Where a newly created pane goes in its tab's layout tree.
///
/// The three manual placements splice the tree by hand and therefore **eject the
/// tab to `Custom`** (see [`create_pane_in_tab`]); `Auto` is the layout-driven
/// one that lets an automatic mode rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanePlacement {
    /// Manual vertical split of the focused pane (left/right).
    SplitVertical,
    /// Manual horizontal split of the focused pane (top/bottom).
    SplitHorizontal,
    /// Manual splice into the focused pane's stack node.
    Stack,
    /// Layout-driven: rebuild from the automatic mode, or -- already in
    /// `Custom` -- a vertical split of the focused pane.
    Auto,
}

impl PanePlacement {
    /// Whether this placement edits the tree by hand rather than letting the
    /// layout mode build it.
    fn is_manual(self) -> bool {
        !matches!(self, PanePlacement::Auto)
    }

    /// The PTY's initial size, given the session's render size. A split hands
    /// the new pane about half the axis it divides; the others start full size.
    /// Only the seed size -- `resize_session_panes` corrects it from the real
    /// layout immediately afterwards.
    fn spawn_size(self, cols: u16, rows: u16) -> (u16, u16) {
        match self {
            PanePlacement::SplitVertical => (cols / 2, rows),
            PanePlacement::SplitHorizontal => (cols, rows / 2),
            PanePlacement::Stack | PanePlacement::Auto => (cols, rows),
        }
    }
}

/// **The one pane-creation path.** Insert a new pane into `session_name`'s tab
/// (`tab_index`, or the active tab when `None`) according to `placement`, spawn
/// its PTY inheriting the previously focused pane's CWD, and start forwarding
/// its output.
///
/// Every `RemuxCommand` that creates a pane funnels through here, which is what
/// keeps the two rules below true for all of them:
///
/// * **A manual placement ejects the tab to `Custom`.** `PaneSplit*` and
///   `PaneStackAdd` mutate the tree directly, so leaving the tab in an automatic
///   mode would let the next rebuild (`PaneNew` / `LayoutNext` / `SetMaster`)
///   silently discard the arrangement the user just made -- and, because
///   `saved_custom_layout` is only snapshotted when the mode is *already*
///   `Custom`, discard it unrecoverably. Stacking used to skip this and lose
///   stacks exactly that way.
/// * **A new pane always clears the zoom.** The tab is showing a new
///   arrangement, so the old full-area pane is no longer what the user asked
///   for.
///
/// Returns the new pane's id, or `None` when the session/tab could not be
/// resolved. The caller owns the refresh tail (`resize_session_panes` +
/// `broadcast_full_render`, or `refresh_target_session` for another session's
/// tab), since which clients to repaint is a caller-side decision.
#[allow(clippy::too_many_arguments)]
async fn create_pane_in_tab(
    session_name: &str,
    tab_index: Option<usize>,
    placement: PanePlacement,
    cols: u16,
    rows: u16,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<Option<PaneId>> {
    let (new_pane_id, source_pane_id) = {
        let mut st = state.lock().await;
        let new_pane_id = st.next_pane_id();
        let sess = match st.sessions.get_mut(session_name) {
            Some(s) => s,
            None => {
                log::info!("create_pane_in_tab: session '{session_name}' not found");
                return Ok(None);
            }
        };
        let index = tab_index.unwrap_or(sess.active_tab);
        let tab = match sess.tabs.get_mut(index) {
            Some(t) => t,
            None => {
                log::info!("create_pane_in_tab: tab index {index} out of range");
                return Ok(None);
            }
        };
        let focused = tab.focused_pane;
        // Eject to Custom before touching the tree, so `is_automatic()` below
        // sees the post-eject mode.
        if placement.is_manual() && tab.layout_mode.is_automatic() {
            tab.layout_mode = LayoutMode::Custom(CustomLayout);
        }
        // Push first: an automatic rebuild reads `pane_order`.
        tab.pane_order.push(new_pane_id);
        match placement {
            PanePlacement::SplitVertical => {
                tab.layout.split_vertical(focused, new_pane_id);
            }
            PanePlacement::SplitHorizontal => {
                tab.layout.split_horizontal(focused, new_pane_id);
            }
            PanePlacement::Stack => {
                tab.layout.add_to_stack(focused, new_pane_id);
            }
            PanePlacement::Auto => {
                if tab.layout_mode.is_automatic() {
                    tab.layout = tab.layout_mode.build_tree(&tab.pane_order, new_pane_id);
                } else {
                    tab.layout.split_vertical(focused, new_pane_id);
                }
            }
        }
        tab.focused_pane = new_pane_id;
        tab.zoomed_pane = None;
        log::debug!(
            "server: create_pane_in_tab session={session_name} tab={index} \
             placement={placement:?} new_pane_id={new_pane_id} from focused={focused}"
        );
        session::debug_check_invariant(sess, "create_pane_in_tab");
        (new_pane_id, focused)
    };

    let source_cwd = {
        let panes_lock = panes.lock().await;
        panes_lock
            .get(&source_pane_id)
            .and_then(|p| persistence::get_pane_cwd(p.pty.child_pid))
    };
    let (spawn_cols, spawn_rows) = placement.spawn_size(cols, rows);
    spawn_pane(
        new_pane_id,
        spawn_cols,
        spawn_rows,
        None,
        source_cwd.as_deref().map(std::path::Path::new),
        panes,
        config,
    )
    .await?;
    start_pty_forwarding(
        session_name,
        state,
        panes,
        clients,
        config,
        prev_frames,
        dormant,
    )
    .await;
    Ok(Some(new_pane_id))
}

async fn spawn_pane(
    pane_id: PaneId,
    cols: u16,
    rows: u16,
    command: Option<&str>,
    cwd: Option<&std::path::Path>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
) -> Result<()> {
    let cmd = command.or(config.general.default_shell.as_deref());
    log::debug!("server: spawn_pane pane_id={pane_id} dims={cols}x{rows} cmd={cmd:?} cwd={cwd:?}");
    let pty_instance = Pty::spawn(cols, rows, cmd, cwd)?;
    let raw_fd = pty_instance.master_fd.as_raw_fd();
    let (_reader_handle, pty_rx) = pty::start_reader(raw_fd);
    let screen = Screen::new(cols, rows, config.general.scrollback_lines);

    let mut ps = panes.lock().await;
    ps.insert(
        pane_id,
        PaneData {
            pty: pty_instance,
            screen,
            pty_rx,
            forwarding_started: false,
            streamed_session_visible: false,
        },
    );
    Ok(())
}

async fn resize_session_panes(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    _config: &Arc<Config>,
) -> Result<()> {
    // Home allotments from the session's active-tab layout (the size the panes
    // get from attached clients viewing this session).
    let rects = active_tab_content_sizes(session_name, state, clients).await;
    if rects.is_empty() {
        return Ok(());
    }
    // Whether this session has an attached client viewing it. Its active-tab
    // panes (the `rects` here) are then "session-visible": driven at full size by
    // the real session, so View-cell size demands are IGNORED for them (a cell
    // shows the "Active in session" placeholder instead of shrinking the pane).
    // Only when NO client is attached does a View cell drive a background pane's
    // size (the honest shared-pane reflow), so fold demands in only then.
    let has_client = {
        let cls = clients.lock().await;
        cls.values()
            .any(|c| c.session_name.as_deref() == Some(session_name))
    };
    let sub_mins = if has_client {
        HashMap::new()
    } else {
        // Fold in every View-cell size demand BEFORE touching the panes lock, so
        // we never nest clients under panes.
        let cls = clients.lock().await;
        let mut m: HashMap<PaneId, (u16, u16)> = HashMap::new();
        for (pane_id, _, _) in &rects {
            if let Some(d) = subscriber_min_demand_locked(&cls, *pane_id) {
                m.insert(*pane_id, d);
            }
        }
        m
    };

    {
        let mut ps = panes.lock().await;
        for (pane_id, content_cols, content_rows) in rects {
            if let Some(pane_data) = ps.get_mut(&pane_id) {
                let (mut cols, mut rows) = (content_cols, content_rows);
                if let Some((sc, sr)) = sub_mins.get(&pane_id) {
                    cols = cols.min(*sc);
                    rows = rows.min(*sr);
                }
                let inner_cols = cols.max(1);
                let inner_rows = rows.max(1);
                log::debug!(
                    "resize_session_panes: pane_id={}, content={}x{}, pty/screen resize to cols={} rows={}",
                    pane_id, content_cols, content_rows, inner_cols, inner_rows
                );
                let _ = pane_data.pty.resize(inner_cols, inner_rows);
                pane_data.screen.resize(inner_cols, inner_rows);
            }
        }
    }
    // Size the popup's PTY to its rect interior. A dedicated step, deliberately
    // NOT folded into `active_tab_content_sizes`: that function means "the active
    // tab's panes" and feeds the View-cell size fold, which the popup (which no
    // View cell can reference) has no business entering. The rect derives from
    // `session_render_size`, i.e. min-across-attached-clients, like every other
    // pane.
    {
        let popup = {
            let st = state.lock().await;
            st.sessions
                .get(session_name)
                .and_then(|s| s.popup_pane.map(|id| (id, s.popup_size)))
        };
        if let Some((popup_id, popup_size)) = popup {
            let (cols, rows) = session_render_size(session_name, clients).await;
            let area = Rect {
                x: 0,
                y: 0,
                width: cols,
                height: rows.saturating_sub(1),
            };
            let rect = layout::popup_rect(area, popup_size);
            // Interior = rect minus the 1-cell frame, matching `draw_popup`
            // (the SAME threshold, so the PTY is never sized to a region that
            // was not painted).
            let (inner_cols, inner_rows) = if fits_zellij_border(rect.width, rect.height) {
                (rect.width - 2, rect.height - 2)
            } else {
                (rect.width, rect.height)
            };
            let (inner_cols, inner_rows) = (inner_cols.max(1), inner_rows.max(1));
            let mut ps = panes.lock().await;
            if let Some(pane_data) = ps.get_mut(&popup_id) {
                log::debug!(
                    "resize_session_panes: popup pane_id={popup_id} pty/screen resize to cols={inner_cols} rows={inner_rows}"
                );
                let _ = pane_data.pty.resize(inner_cols, inner_rows);
                pane_data.screen.resize(inner_cols, inner_rows);
            }
        }
    }

    // A resize can change active-tab membership / attachment for this or other
    // sessions (tab switch, attach), flipping which subscribed panes are
    // session-visible; re-evaluate every subscribed pane so cells that just
    // gained or lost visibility flip live. Cheap: only live View cells subscribe.
    refresh_subscribed_panes(state, panes, clients, _config).await;
    Ok(())
}

/// The content (blit) size each pane in a session's active tab gets from the
/// clients attached to that session -- the pane's "home allotment". Empty when
/// the session is unknown, has no active tab, or (via `session_render_size`'s
/// 80x24 fallback) would be meaningless. Border-style aware, mirroring the
/// composite render path so the PTY/screen match the visible content area.
async fn active_tab_content_sizes(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Vec<(PaneId, u16, u16)> {
    let (cols, rows) = session_render_size(session_name, clients).await;
    let st = state.lock().await;
    let sess = match st.sessions.get(session_name) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let tab = match sess.tabs.get(sess.active_tab) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let content_rows = rows.saturating_sub(1);
    let area = Rect {
        x: 0,
        y: 0,
        width: cols,
        height: content_rows,
    };
    // The SAME layout the render path composites with -- under zoom that is a
    // synthetic single-pane stack, so a zoomed *stacked* pane is sized as the
    // single pane it is painted as (asking the real tree here is what left it a
    // row short of its painted area under the tmux border style).
    let effective_layout = tab.effective_layout();
    let pane_rects = layout::compute_layout(&effective_layout, area, 0);

    let mut content_rects = Vec::new();
    for &(pane_id, rect) in &pane_rects {
        let content = pane_content_rect(
            &sess.border_style,
            rect,
            is_multi_stack(&effective_layout, pane_id),
        );
        content_rects.push((pane_id, content.width, content.height));
    }
    content_rects
}

/// The minimum `(cols, rows)` demanded across every client's *sized*
/// (`Some`) subscription to `pane_id`, so the pane fits every cell that shows
/// it. `None` when no client demands a size (either unsubscribed, or only
/// watch-only subscriptions). Takes the already-held
/// clients guard to keep lock ordering explicit at the call sites.
fn subscriber_min_demand_locked(
    cls: &HashMap<u64, ClientConnection>,
    pane_id: PaneId,
) -> Option<(u16, u16)> {
    let mut out: Option<(u16, u16)> = None;
    for conn in cls.values() {
        if let Some(Some((c, r))) = conn.subscribed_panes.get(&pane_id) {
            out = Some(match out {
                Some((oc, or)) => (oc.min(*c), or.min(*r)),
                None => (*c, *r),
            });
        }
    }
    out
}

/// Whether `pane_id` is "session-visible": present in the ACTIVE TAB of at least
/// one attached client's session. View-cell subscriptions do NOT count -- they
/// key off `subscribed_panes`, not `session_name`. A session-visible pane is
/// driven at full size by its real session, so a View cell must neither shrink
/// it nor render its streamed content (the cell shows an "Active in session"
/// placeholder instead). This is the message-facing companion of
/// [`pane_home_allotment`]: it uses the same active-tab-membership + attachment
/// test, but is independent of layout rect availability (e.g. under zoom a
/// non-focused active-tab pane has no home rect yet is still session-visible),
/// so the two never disagree about visibility.
async fn pane_session_visible(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> bool {
    let session_name = {
        let st = state.lock().await;
        st.sessions
            .values()
            .find(|sess| {
                sess.tabs
                    .get(sess.active_tab)
                    .map(|t| t.pane_order.contains(&pane_id))
                    .unwrap_or(false)
            })
            .map(|s| s.name.clone())
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return false,
    };
    let cls = clients.lock().await;
    cls.values()
        .any(|c| c.session_name.as_deref() == Some(session_name.as_str()))
}

/// The pane's home allotment (its size from any attached client viewing the
/// session's active tab), or `None` when the pane is not visible on any
/// attached client (background tab, or a session with no attached client). Such
/// a pane has NO home constraint, so a watch-only View cell never reflows it.
async fn pane_home_allotment(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Option<(u16, u16)> {
    // Which session owns the pane, and does any client have it attached?
    let session_name = {
        let st = state.lock().await;
        let mut found = None;
        for sess in st.sessions.values() {
            if sess
                .tabs
                .get(sess.active_tab)
                .map(|t| t.pane_order.contains(&pane_id))
                .unwrap_or(false)
            {
                found = Some(sess.name.clone());
                break;
            }
        }
        found
    }?;
    let has_client = {
        let cls = clients.lock().await;
        cls.values()
            .any(|c| c.session_name.as_deref() == Some(session_name.as_str()))
    };
    if !has_client {
        return None;
    }
    active_tab_content_sizes(&session_name, state, clients)
        .await
        .into_iter()
        .find(|(pid, _, _)| *pid == pane_id)
        .map(|(_, c, r)| (c, r))
}

/// Recompute a single pane's effective size = componentwise min of its home
/// allotment and every sized View-cell demand, then resize the PTY/screen and
/// re-stream a fresh `PaneContent` to subscribers if it changed. When nothing
/// constrains the pane (no home, no sized subscriber) it is left untouched --
/// so merely watching a pane (a hidden cell or a plain observer, `None` demand)
/// never reflows it. Called on subscribe/unsubscribe (the home path uses
/// `resize_session_panes`).
async fn recompute_pane_size(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    _config: &Arc<Config>,
) {
    // A session-visible pane is driven at full size by its real session, so its
    // View-cell subscriptions are treated as size_demand:false regardless -- the
    // cell shows an "Active in session" placeholder and imposes no constraint.
    let session_visible = pane_session_visible(pane_id, state, clients).await;
    let home = pane_home_allotment(pane_id, state, clients).await;
    let sub = if session_visible {
        None
    } else {
        let cls = clients.lock().await;
        subscriber_min_demand_locked(&cls, pane_id)
    };
    // Effective size = home when session-visible (demand ignored), else the
    // componentwise min of home and the sized demand. `None` when nothing
    // constrains the pane (a non-visible watch-only cell, or a session-visible
    // pane with no home rect under zoom) -- leave the pane untouched then.
    let effective = match (home, sub) {
        (Some((hc, hr)), Some((sc, sr))) => Some((hc.min(sc), hr.min(sr))),
        (Some(h), None) => Some(h),
        (None, Some(s)) => Some(s),
        (None, None) => None,
    };
    // Resize only if a size constraint exists and it actually changed, to avoid
    // needless PTY churn / SIGWINCH.
    let resized = if let Some((ec, er)) = effective.map(|(c, r)| (c.max(1), r.max(1))) {
        let mut ps = panes.lock().await;
        match ps.get_mut(&pane_id) {
            Some(pd) if pd.screen.cols != ec || pd.screen.rows != er => {
                log::debug!(
                    "recompute_pane_size: pane_id={pane_id} {}x{} -> {ec}x{er}",
                    pd.screen.cols,
                    pd.screen.rows
                );
                let _ = pd.pty.resize(ec, er);
                pd.screen.resize(ec, er);
                true
            }
            _ => false,
        }
    } else {
        false
    };
    // Detect a session-visibility flip since the last stream, so a tab
    // switch / attach / detach that changes visibility WITHOUT changing size
    // still pushes a fresh PaneContent and the cell flips live.
    let visibility_changed = {
        let mut ps = panes.lock().await;
        match ps.get_mut(&pane_id) {
            Some(pd) => {
                let changed = pd.streamed_session_visible != session_visible;
                pd.streamed_session_visible = session_visible;
                changed
            }
            None => false,
        }
    };
    if resized || visibility_changed {
        // Re-stream the snapshot (with the current session_visible flag) to every
        // subscriber right away, so a cell that reflowed a pane -- or whose pane
        // just flipped visibility -- updates without waiting for the next PTY
        // output.
        stream_pane_content(pane_id, state, panes, clients).await;
    }
}

/// Re-evaluate every currently-subscribed pane's effective size and
/// session-visibility, pushing a fresh `PaneContent` to its subscribers when
/// either changed (via [`recompute_pane_size`]). Called after any event that can
/// change active-tab membership or client attachment (tab switch, attach,
/// detach, disconnect) but does NOT itself resize the pane -- so View cells flip
/// promptly between live content and the "Active in session" placeholder. The
/// subscribed set is only the live View cells, so this is cheap.
async fn refresh_subscribed_panes(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
) {
    let pane_ids: Vec<PaneId> = {
        let cls = clients.lock().await;
        let mut set = std::collections::HashSet::new();
        for conn in cls.values() {
            for pid in conn.subscribed_panes.keys() {
                set.insert(*pid);
            }
        }
        set.into_iter().collect()
    };
    for pid in pane_ids {
        recompute_pane_size(pid, state, panes, clients, config).await;
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Compute the consistent render size for a session: the minimum cols and rows
/// over all clients currently attached to it. Falls back to 80x24 if no clients
/// are attached. Every render path for a session composites at this size so a
/// client never mixes differently-sized frames. With a single attached client
/// this equals that client's size (behavior unchanged from a single baseline).
async fn session_render_size(
    session_name: &str,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> (u16, u16) {
    let cls = clients.lock().await;
    let cols = cls
        .values()
        .filter(|c| c.session_name.as_deref() == Some(session_name))
        .map(|c| c.cols)
        .min()
        .unwrap_or(80);
    let rows = cls
        .values()
        .filter(|c| c.session_name.as_deref() == Some(session_name))
        .map(|c| c.rows)
        .min()
        .unwrap_or(24);
    (cols, rows)
}

/// Resolve the `(session_name, tab_name)` a pane belongs to, for a View cell's
/// border title. Scans every session's tabs for the one whose `pane_order`
/// contains `pane_id`. Returns empty strings when the pane can't be located
/// (already closed): the client then falls back to `waiting…`.
fn pane_labels(st: &ServerState, pane_id: PaneId) -> (String, String) {
    for sess in st.sessions.values() {
        for tab in &sess.tabs {
            if tab.panes().contains(&pane_id) {
                return (sess.name.clone(), tab.name.clone());
            }
        }
    }
    (String::new(), String::new())
}

/// Deliver an application's `OSC 52` clipboard write (drained from the pane's
/// screen with [`Screen::take_clipboard`]) to the clients that should act on it,
/// as the same `CopyToClipboard` a Remux yank sends.
///
/// **Routing rule: the pane must be one the user is looking at.** A client gets
/// the write when the pane is the one its own keystrokes would reach — the
/// `input_target` of the session it is attached to — or when it is *showing* the
/// pane in a View cell (a `Some(size)` subscription; `None` is watch-only). A
/// pane in a background tab, in a session nobody has in the foreground, or in a
/// View cell hidden by the layout is skipped: a program left running out of
/// sight must not be able to quietly take over the clipboard. Using
/// `input_target` rather than the raw focused pane also means a pane shadowed by
/// a visible popup does not count as focused, matching where input goes.
///
/// Each matching client is sent exactly one message even if both rules apply.
async fn deliver_app_clipboard(
    pane_id: PaneId,
    data: String,
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    // Sessions in which this pane is the current input target. Taken before the
    // clients lock: state-then-clients, never nested the other way.
    let focused_in: Vec<String> = {
        let st = state.lock().await;
        st.sessions
            .iter()
            .filter(|(_, sess)| sess.input_target() == Some(pane_id))
            .map(|(name, _)| name.clone())
            .collect()
    };

    let cls = clients.lock().await;
    for client in cls.values() {
        let attached_and_focused = client
            .session_name
            .as_ref()
            .is_some_and(|name| focused_in.iter().any(|f| f == name));
        let showing_in_view = matches!(client.subscribed_panes.get(&pane_id), Some(Some(_)));
        if attached_and_focused || showing_in_view {
            let _ = client
                .tx
                .send(ServerMessage::CopyToClipboard { data: data.clone() });
        }
    }
}

/// What makes one subscriber's `PaneContent` differ from another's: the
/// scrollback offset it is rendered at, plus its selection flattened to
/// `(start_col, start_row, end_col, end_row)`. Used to memoize the renders in
/// [`stream_pane_content`] so identical viewing states cost one snapshot.
type RenderKey = (usize, Option<(u16, u16, u16, u16)>);

/// Render and send a fresh `PaneContent` for `pane_id` to every client
/// subscribed to it, **rendered per subscriber**.
///
/// Each subscriber sees the pane through its own per-(client, pane) scrollback
/// offset and its own cell drag-selection, so a single shared snapshot is wrong:
/// broadcasting one render at offset 0 is what made a scrolled-back View cell
/// snap to the live tail on the next byte of PTY output. Renders are memoized on
/// `(offset, selection)`, so the common case (all subscribers live, nothing
/// selected) still costs exactly one snapshot.
///
/// A no-op when nothing subscribes -- the snapshot is skipped entirely rather
/// than built and dropped. Acquires the clients, state and panes locks
/// independently (never nested), honoring the codebase's lock ordering.
async fn stream_pane_content(
    pane_id: PaneId,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    // Every subscriber with the viewing state that makes its snapshot differ:
    // its own scrollback offset and its own cell selection. Both are
    // per-(client, pane), so one shared snapshot cannot serve them all -- that
    // was the bug where any PTY output yanked a scrolled-back cell to the live
    // tail, because the fanout rendered once at offset 0 and cloned it.
    let subs: Vec<(u64, usize, Option<MouseSelection>)> = {
        let cls = clients.lock().await;
        cls.iter()
            .filter(|(_, c)| c.subscribed_panes.contains_key(&pane_id))
            .map(|(id, c)| {
                (
                    *id,
                    c.pane_scroll.get(&pane_id).copied().unwrap_or(0),
                    c.pane_selection.get(&pane_id).cloned(),
                )
            })
            .collect()
    };
    if subs.is_empty() {
        return;
    }
    let (session_name, tab_name) = {
        let st = state.lock().await;
        pane_labels(&st, pane_id)
    };
    let session_visible = pane_session_visible(pane_id, state, clients).await;

    // Render, then send: the messages are owned, so the panes lock is released
    // before the clients lock is taken again (never nested).
    let outgoing: Vec<(u64, ServerMessage)> = {
        let ps = panes.lock().await;
        let screen = match ps.get(&pane_id) {
            Some(pd) => &pd.screen,
            // Pane gone between the subscriber scan and here: nothing to send.
            None => return,
        };
        // A stored offset can outlive the scrollback it pointed into (eviction,
        // or a reflow-shrinking resize), so clamp at render time instead of
        // trying to keep every client's map correct on every mutation.
        let max_off = screen.max_scroll_offset();
        // One render per DISTINCT (offset, selection). The steady state is every
        // subscriber live and unselected, which collapses back to exactly one.
        let mut cache: Vec<(RenderKey, ServerMessage)> = Vec::new();
        let mut out = Vec::with_capacity(subs.len());
        for (cid, off, sel) in subs {
            let off = off.min(max_off);
            let key = (
                off,
                sel.as_ref()
                    .map(|s| (s.start.0, s.start.1, s.end.0, s.end.1)),
            );
            let msg = match cache.iter().find(|(k, _)| *k == key) {
                Some((_, m)) => m.clone(),
                None => {
                    let snap = crate::server::compositor::render_pane_snapshot_selected(
                        screen,
                        off,
                        sel.as_ref(),
                    );
                    let m = ServerMessage::PaneContent {
                        pane_id,
                        cols: snap.cols,
                        rows: snap.rows,
                        cells: snap.cells,
                        cursor_x: snap.cursor_x,
                        cursor_y: snap.cursor_y,
                        cursor_visible: snap.cursor_visible,
                        application_cursor_keys: snap.application_cursor_keys,
                        session_name: session_name.clone(),
                        tab_name: tab_name.clone(),
                        session_visible,
                    };
                    cache.push((key, m.clone()));
                    m
                }
            };
            out.push((cid, msg));
        }
        out
    };
    let cls = clients.lock().await;
    for (cid, msg) in outgoing {
        if let Some(conn) = cls.get(&cid) {
            let _ = conn.tx.send(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Shared view helpers
// ---------------------------------------------------------------------------

/// Build the `ViewList` message that mirrors the current shared-view registry.
/// Pure over a locked `ServerState`; the full [`LayoutMode`]/`custom_tree`
/// travel so a client can composite without an extra round trip.
fn build_view_list_message(st: &ServerState) -> ServerMessage {
    let views = st
        .views
        .iter()
        .map(|v| ViewInfo {
            id: v.id,
            name: v.name.clone(),
            cells: v
                .cells
                .iter()
                .map(|c| CellInfo {
                    id: c.id,
                    conn: c.conn.clone(),
                    pane_id: c.pane_id,
                })
                .collect(),
            layout: v.layout.clone(),
            custom_tree: v.custom_tree.clone(),
            focused: v.focused,
            zoomed: v.zoomed,
        })
        .collect();
    ServerMessage::ViewList { views }
}

/// Broadcast the full current `ViewList` to EVERY connected client, so the
/// shared registry stays consistent across all terminals. Called after every
/// view mutation. Locks `state` to snapshot the message, then `clients` to fan
/// out — never nested, matching the codebase lock ordering.
async fn broadcast_view_list(
    state: &Arc<Mutex<ServerState>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    let msg = {
        let st = state.lock().await;
        build_view_list_message(&st)
    };
    let cls = clients.lock().await;
    for conn in cls.values() {
        let _ = conn.tx.send(msg.clone());
    }
}

/// A reference area for view geometry (the `find_neighbor` search in
/// `ViewMoveCell`): the componentwise min terminal size across all connected
/// clients (fallback 80x24), minus one row for the view's status bar — matching
/// the client's `cells_area` convention. See the Phase-2 TODO on
/// [`ServerState::view_move_cell`]: a shared view ultimately needs its own
/// canonical area rather than one derived from the live client population.
async fn view_reference_area(clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>) -> Rect {
    let cls = clients.lock().await;
    let cols = cls.values().map(|c| c.cols).min().unwrap_or(80);
    let rows = cls.values().map(|c| c.rows).min().unwrap_or(24);
    Rect {
        x: 0,
        y: 0,
        width: cols,
        height: rows.saturating_sub(1),
    }
}

/// Refresh a target session after a structural mutation performed on behalf of
/// a client that may be attached to a *different* session (or to none) -- e.g.
/// the session manager editing a session it is not viewing.
///
/// If the target session has any attached clients, this resizes its active
/// tab's panes to those clients' dimensions, invalidates their render
/// baselines, and broadcasts a fresh full render. When no client is attached,
/// there is nothing to display, so it is a no-op (the mutation is still
/// persisted by the caller via `save_if_enabled`). Mirrors the
/// `target_dims.is_some()` guard used by the `TabCloseByIndex` handler.
async fn refresh_target_session(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) -> Result<()> {
    let has_clients = {
        let cls = clients.lock().await;
        cls.values()
            .any(|c| c.session_name.as_deref() == Some(session_name))
    };
    if !has_clients {
        return Ok(());
    }
    resize_session_panes(session_name, state, panes, clients, config).await?;
    invalidate_session_baselines(session_name, clients, prev_frames).await;
    broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
    Ok(())
}

/// Invalidate the previous-frame baselines of every client attached to a
/// session by removing their `client_id` entries. Each affected client then
/// receives a fresh FULL render on its next frame (instead of a diff against a
/// stale, possibly wrong-size baseline). Used on attach/resize/tab changes.
///
/// Collects the client ids under the `clients` lock, drops it, then locks
/// `prev_frames` — this avoids ever holding both locks nested, keeping lock
/// ordering uniform with the render paths and preventing deadlock.
async fn invalidate_session_baselines(
    session_name: &str,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    prev_frames: &PrevFrameCache,
) {
    let ids: Vec<u64> = {
        let cls = clients.lock().await;
        cls.iter()
            .filter(|(_, c)| c.session_name.as_deref() == Some(session_name))
            .map(|(id, _)| *id)
            .collect()
    };
    let mut pf = prev_frames.lock().await;
    for id in ids {
        pf.remove(&id);
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_full_render_to_client(
    client_id: u64,
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    // Render at the consistent session size (min over all attached clients),
    // not the caller's own dimensions, so this client's frames never mix sizes
    // with the shared broadcast path. With a single client this equals the
    // caller's size, so single-client behavior is unchanged.
    let (cols, rows) = session_render_size(session_name, clients).await;
    log::debug!(
        "server: send_full_render_to_client client_id={client_id} session={session_name:?} dims={cols}x{rows}"
    );
    let (mode, selection, client_search_info, scroll_offset) = {
        let cls = clients.lock().await;
        let client = cls.get(&client_id);
        let mode = client
            .map(|c| c.mode.clone())
            .unwrap_or_else(|| "NORMAL".to_string());
        let selection = client.and_then(|c| c.mouse_selection.clone());
        let si = client.and_then(|c| c.search_info);
        let so = client.map(|c| c.scroll_offset).unwrap_or(0);
        (mode, selection, si, so)
    };
    // Update auto-detected pane names before rendering.
    update_auto_pane_names(session_name, state, panes).await;
    let (
        cells,
        cursor_x,
        cursor_y,
        cursor_visible,
        cursor_style,
        focused_pane_rect,
        _hit_regions,
        _pane_rects,
        application_cursor_keys,
        _popup,
    ) = build_composite(
        session_name,
        cols,
        rows,
        &mode,
        state,
        panes,
        config,
        selection.as_ref(),
        client_search_info,
        scroll_offset,
        &config.compositor_theme(),
    )
    .await;
    let cursor_visible = if scroll_offset > 0 {
        false
    } else {
        cursor_visible
    };

    // Compute viewport_top: the scrollback line index of the first displayed line.
    let viewport_top = {
        let st = state.lock().await;
        let fp = st
            .sessions
            .get(session_name)
            .and_then(|s| s.tabs.get(s.active_tab))
            .map(|t| t.focused_pane);
        drop(st);
        if let Some(fp) = fp {
            let ps = panes.lock().await;
            ps.get(&fp)
                .map(|p| {
                    if scroll_offset == 0 {
                        // Not scrolled: blit_screen fast path reads from grid[0]
                        // which is line_at(scrollback.len())
                        p.screen.scrollback.len()
                    } else {
                        // Scrolled: blit_screen_scrolled computes
                        // view_top = total - scroll_offset - pane_h
                        let total = p.screen.total_lines();
                        let pane_h = focused_pane_rect
                            .map(|r| r.height as usize)
                            .unwrap_or(rows.saturating_sub(1) as usize);
                        total.saturating_sub(scroll_offset).saturating_sub(pane_h)
                    }
                })
                .unwrap_or(0)
        } else {
            0
        }
    };

    // Only save this client's baseline for live view (scroll_offset == 0).
    // Scrolled frames must not pollute the diff baseline: when the client
    // returns to the live view it renders here again and repopulates it.
    if scroll_offset == 0 {
        let mut pf = prev_frames.lock().await;
        pf.insert(client_id, cells.clone());
    }

    let mut cls = clients.lock().await;
    if let Some(client) = cls.get_mut(&client_id) {
        let prev_so = client.prev_scroll_offset;
        client.prev_scroll_offset = scroll_offset;

        // Detect incremental scroll for ScrollRender optimization.
        let delta = scroll_offset as i64 - prev_so as i64;
        let abs_delta = delta.unsigned_abs() as usize;

        // The client's render_scroll is a no-op when abs_delta >= pane_height
        // (it expects the caller to fall back to a full repaint), so only use a
        // ScrollRender when the delta is strictly smaller than the focused
        // pane's content height; otherwise send a FullRender.
        //
        // At the very top of history (viewport_top == 0, i.e. scroll_offset has
        // reached max_offset so the first scrollback line is the top visible
        // row) force a FullRender instead of an incremental ScrollRender. This
        // guarantees the client's on-screen buffer is repainted authoritatively
        // at the scroll boundary rather than relying on an accumulated series of
        // incremental shifts to have landed the earliest lines exactly right.
        let use_scroll_render = scroll_offset > 0
            && prev_so > 0
            && viewport_top > 0
            && abs_delta > 0
            && abs_delta <= 10
            && focused_pane_rect.is_some_and(|r| abs_delta < r.height as usize);

        log::debug!(
            "server: render decision client_id={client_id} scroll_offset={scroll_offset} prev_so={prev_so} delta={delta} use_scroll_render={use_scroll_render}"
        );

        if use_scroll_render {
            let fpr = focused_pane_rect.unwrap();
            let px = fpr.x as usize;
            let py = fpr.y as usize;
            let pw = fpr.width as usize;
            let ph = fpr.height as usize;

            let new_rows: Vec<Vec<RenderCell>> = if delta > 0 {
                // Scrolled UP (deeper into history) — new rows at TOP of pane
                (0..abs_delta)
                    .map(|i| {
                        let y = py + i;
                        if y < cells.len() && px + pw <= cells[y].len() {
                            cells[y][px..px + pw].to_vec()
                        } else {
                            vec![RenderCell::default(); pw]
                        }
                    })
                    .collect()
            } else {
                // Scrolled DOWN (toward live view) — new rows at BOTTOM of pane
                (0..abs_delta)
                    .map(|i| {
                        let y = py + ph - abs_delta + i;
                        if y < cells.len() && px + pw <= cells[y].len() {
                            cells[y][px..px + pw].to_vec()
                        } else {
                            vec![RenderCell::default(); pw]
                        }
                    })
                    .collect()
            };

            let _ = client.tx.send(ServerMessage::ScrollRender {
                pane_x: fpr.x,
                pane_y: fpr.y,
                pane_width: fpr.width,
                pane_height: fpr.height,
                delta: delta as i16,
                new_rows,
                cursor_x,
                cursor_y,
                cursor_visible,
                cursor_style,
                focused_pane_rect: Some(fpr),
                application_cursor_keys,
                viewport_top,
                scroll_offset,
            });
        } else {
            let _ = client.tx.send(ServerMessage::FullRender {
                cells,
                cursor_x,
                cursor_y,
                cursor_visible,
                cursor_style,
                focused_pane_rect,
                application_cursor_keys,
                viewport_top,
                scroll_offset,
            });
        }
    }
}

async fn broadcast_full_render(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    let (cols, rows, mode, selection, si, client_count) = {
        let cls = clients.lock().await;
        let attached: Vec<_> = cls
            .values()
            .filter(|c| c.session_name.as_deref() == Some(session_name))
            .collect();
        if attached.is_empty() {
            return;
        }
        let count = attached.len();
        let cols = attached.iter().map(|c| c.cols).min().unwrap_or(80);
        let rows = attached.iter().map(|c| c.rows).min().unwrap_or(24);
        // Use the mode and selection from the first attached client.
        let first = attached.first();
        let mode = first
            .map(|c| c.mode.clone())
            .unwrap_or_else(|| "NORMAL".to_string());
        let selection = first.and_then(|c| c.mouse_selection.clone());
        let si = first.and_then(|c| c.search_info);
        (cols, rows, mode, selection, si, count)
    };

    log::debug!("server: broadcast_full_render session={session_name:?} clients={client_count}");

    // Update auto-detected pane names before rendering.
    update_auto_pane_names(session_name, state, panes).await;

    let (
        cells,
        cursor_x,
        cursor_y,
        cursor_visible,
        cursor_style,
        focused_pane_rect,
        _hit_regions,
        _pane_rects,
        application_cursor_keys,
        _popup,
    ) = build_composite(
        session_name,
        cols,
        rows,
        &mode,
        state,
        panes,
        config,
        selection.as_ref(),
        si,
        0,
        &config.compositor_theme(),
    )
    .await;

    // Compute viewport_top for live view (scroll_offset=0): first displayed line index.
    let viewport_top = {
        let st = state.lock().await;
        let fp = st
            .sessions
            .get(session_name)
            .and_then(|s| s.tabs.get(s.active_tab))
            .map(|t| t.focused_pane);
        drop(st);
        if let Some(fp) = fp {
            let ps = panes.lock().await;
            ps.get(&fp)
                .map(|p| {
                    // Live view (offset=0): blit reads from grid[0] = line_at(scrollback.len())
                    p.screen.scrollback.len()
                })
                .unwrap_or(0)
        } else {
            0
        }
    };

    // The composite `cells` is identical for every live client (all at the
    // session render size), so it is computed once above. The diff, however, is
    // per-client: each client is diffed against *its own* baseline so a client
    // whose size differs from another's never diffs against a poisoned frame.
    //
    // Collect the eligible (attached, non-scrolled) clients and their senders
    // under the `clients` lock, then drop it before locking `prev_frames`. This
    // keeps lock ordering uniform (never `clients`+`prev_frames` nested) and
    // avoids deadlock against `invalidate_session_baselines`. Scrolled clients
    // are excluded here; they get a FullRender via `send_full_render_to_client`
    // when they scroll and are refreshed when they return to the live view.
    let targets: Vec<(u64, mpsc::UnboundedSender<ServerMessage>, bool)> = {
        let mut cls = clients.lock().await;
        cls.iter_mut()
            .filter(|(_, c)| {
                c.session_name.as_deref() == Some(session_name) && c.scroll_offset == 0
            })
            .map(|(id, c)| {
                let force_full = c.needs_full_render;
                c.needs_full_render = false; // consume it
                (*id, c.tx.clone(), force_full)
            })
            .collect()
    };

    let threshold = cols as usize * rows as usize / 2;
    let mut pf = prev_frames.lock().await;
    for (cid, tx, force_full) in targets {
        // Force a full render when this client has no baseline yet, or when its
        // baseline's dimensions differ from the current frame (a size change,
        // e.g. after another client of a different size attached/detached).
        // compute_diff never clears cells that exist only in a larger prev
        // frame, so a diff across a size change would leave stale content.
        // A client that just returned from scrolling must get a full repaint;
        // its diff baseline may not match its on-screen state.
        let baseline = if force_full { None } else { pf.get(&cid) };
        let size_changed = baseline.is_some_and(|prev| {
            prev.len() != cells.len() || prev.first().map(Vec::len) != cells.first().map(Vec::len)
        });
        let msg = match baseline {
            Some(prev_cells) if !size_changed => {
                let changes = compute_diff(prev_cells, &cells);
                if changes.len() > threshold {
                    log::debug!(
                        "server: broadcast client_id={cid} render=Full (diff {} changes > threshold {})",
                        changes.len(),
                        threshold
                    );
                    ServerMessage::FullRender {
                        cells: cells.clone(),
                        cursor_x,
                        cursor_y,
                        cursor_visible,
                        cursor_style,
                        focused_pane_rect,
                        application_cursor_keys,
                        viewport_top,
                        // Only live-tail clients are targeted here (see the
                        // `scroll_offset == 0` filter above).
                        scroll_offset: 0,
                    }
                } else {
                    log::debug!(
                        "server: broadcast client_id={cid} render=Diff ({} changes, threshold {})",
                        changes.len(),
                        threshold
                    );
                    ServerMessage::RenderDiff {
                        changes,
                        cursor_x,
                        cursor_y,
                        cursor_visible,
                        cursor_style,
                        focused_pane_rect,
                        application_cursor_keys,
                        viewport_top,
                        scroll_offset: 0,
                    }
                }
            }
            _ => {
                log::debug!(
                    "server: broadcast client_id={cid} render=Full (no baseline or size changed)"
                );
                ServerMessage::FullRender {
                    cells: cells.clone(),
                    cursor_x,
                    cursor_y,
                    cursor_visible,
                    cursor_style,
                    focused_pane_rect,
                    application_cursor_keys,
                    viewport_top,
                    scroll_offset: 0,
                }
            }
        };
        let _ = tx.send(msg);
        pf.insert(cid, cells.clone());
    }
}

/// Update display names for panes that don't have a custom name by
/// reading the process name from `/proc/<pid>/comm`.
/// Refresh the auto-detected (process-derived) name of every pane in the
/// session's active tab.
///
/// Also marks the session tree dirty, but ONLY when a name actually changed.
/// This runs on every render and every mouse event, so notifying
/// unconditionally would be a push storm that the coalescing would merely hide;
/// a process name changing (a shell becoming `vim`) is a real change to the
/// pane labels a subscriber's tree displays, and is rare.
async fn update_auto_pane_names(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
) {
    let mut st = state.lock().await;
    let sess = match st.sessions.get_mut(session_name) {
        Some(s) => s,
        None => return,
    };
    let tab = match sess.tabs.get_mut(sess.active_tab) {
        Some(t) => t,
        None => return,
    };

    // Collect pane IDs that need auto-detected names.
    let pane_ids = layout::all_pane_ids(&tab.layout);
    let ps = panes.lock().await;

    // Skip the pane being actively renamed -- its name is managed by the rename flow.
    let renaming_pane = sess.rename_state.as_ref().map(|(pid, _)| *pid);

    let mut changed = false;
    for pane_id in pane_ids {
        if renaming_pane == Some(pane_id) {
            continue;
        }
        // Only update if there's no custom name set.
        let custom = layout::get_pane_custom_name(&tab.layout, pane_id);
        if custom == Some(None) || custom.is_none() {
            // No custom name -- auto-detect from process.
            if let Some(pane_data) = ps.get(&pane_id) {
                let name = get_process_name(pane_data.pty.child_pid.as_raw());
                if layout::get_pane_name(&tab.layout, pane_id).as_deref() != Some(name.as_str()) {
                    changed = true;
                }
                layout::set_pane_name(&mut tab.layout, pane_id, &name);
            }
        }
    }

    // Synchronous, so calling it under the `state`/`panes` guards still held
    // here is safe -- an async broadcast would relock them and deadlock.
    if changed {
        mark_session_tree_dirty();
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_composite(
    session_name: &str,
    cols: u16,
    rows: u16,
    mode: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    _config: &Arc<Config>,
    selection: Option<&MouseSelection>,
    search_info: Option<(usize, usize)>,
    scroll_offset: usize,
    compositor_theme: &crate::config::theme::CompositorTheme,
) -> (
    Vec<Vec<RenderCell>>,
    u16,
    u16,
    bool,
    u8,
    Option<PaneRect>,
    HitRegions,
    Vec<(PaneId, Rect)>,
    bool, // application_cursor_keys
    // The visible popup as `(popup_pane, popup_rect)`. Deliberately NOT folded
    // into `pane_rects`: that stays the layout's own rects (popup-independent),
    // so callers can hit-test the popup FIRST without the overlay ever
    // masquerading as a layout pane.
    Option<(PaneId, Rect)>,
) {
    let st = state.lock().await;
    let sess = match st.sessions.get(session_name) {
        Some(s) => s,
        None => {
            return (
                vec![vec![RenderCell::default(); cols as usize]; rows as usize],
                0,
                0,
                false,
                0,
                None,
                HitRegions::default(),
                Vec::new(),
                false,
                None,
            );
        }
    };
    let tab = match sess.tabs.get(sess.active_tab) {
        Some(t) => t,
        None => {
            return (
                vec![vec![RenderCell::default(); cols as usize]; rows as usize],
                0,
                0,
                false,
                0,
                None,
                HitRegions::default(),
                Vec::new(),
                false,
                None,
            );
        }
    };

    let content_rows = rows.saturating_sub(1);
    let area = Rect {
        x: 0,
        y: 0,
        width: cols,
        height: content_rows,
    };

    let ps = panes.lock().await;
    let mut pane_screens: HashMap<PaneId, &Screen> = HashMap::new();
    let effective_layout = tab.effective_layout();
    let pane_rects = layout::compute_layout(&effective_layout, area, 0);
    for (pane_id, _rect) in &pane_rects {
        if let Some(pane_data) = ps.get(pane_id) {
            pane_screens.insert(*pane_id, &pane_data.screen);
        }
    }

    let status_info = StatusInfo {
        mode: mode.to_string(),
        session_name: session_name.to_string(),
        tabs: sess
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let name = if i == sess.active_tab && t.zoomed_pane.is_some() {
                    format!("{} Z", t.name)
                } else {
                    t.name.clone()
                };
                (name, i == sess.active_tab, t.activity)
            })
            .collect(),
        layout_mode: tab.layout_mode.name().to_string(),
        search_info,
    };

    // The visible popup (if any), resolved once. `popup_rect` derives from
    // `area` alone, so it is independent of the layout AND of the zoom
    // substitution below.
    let popup = sess
        .popup_pane
        .filter(|_| sess.popup_visible)
        .filter(|id| ps.contains_key(id))
        .map(|id| (id, layout::popup_rect(area, sess.popup_size)));

    // Scroll offsets belong to whichever pane currently owns input: while the
    // popup is up it scrolls, and the pane behind it stays put.
    let scroll_target = popup.map(|(id, _)| id).unwrap_or(tab.focused_pane);
    let scroll_offsets = if scroll_offset > 0 {
        let mut offsets = HashMap::new();
        offsets.insert(scroll_target, scroll_offset);
        offsets
    } else {
        HashMap::new()
    };

    let (mut cells, mut hit_regions) = composite(
        &effective_layout,
        &pane_screens,
        area,
        &sess.border_style,
        &status_info,
        cols,
        rows,
        0,
        tab.focused_pane,
        selection,
        &scroll_offsets,
        compositor_theme,
    );

    // -- Popup overlay pass ------------------------------------------------
    //
    // Runs AFTER the normal composite (including the zoom `effective_layout`
    // substitution), painting the popup on top of the finished frame. No pane
    // rect changes, so the popup steals no space and zoom state is untouched.
    // Yields the popup's interior rect, which then drives the reported cursor
    // and the client's focused-pane rect.
    let popup_interior = popup.map(|(popup_id, prect)| {
        let popup_screen = ps
            .get(&popup_id)
            .map(|p| &p.screen)
            .expect("popup pane presence checked above");
        let interior = crate::server::compositor::draw_popup(
            &mut cells,
            prect,
            popup_id,
            popup_screen,
            "popup",
            mode,
            scroll_offsets.get(&popup_id).copied().unwrap_or(0),
            selection,
            compositor_theme,
        );
        // Stack/tab labels the popup covers must stop being clickable, or a
        // click inside the popup could activate a hidden pane behind it.
        hit_regions.stack_regions.retain(|r| {
            !(r.y >= prect.y
                && r.y < prect.y + prect.height
                && r.x_end > prect.x
                && r.x_start < prect.x + prect.width)
        });
        interior
    });

    // If there is an active rename, place the cursor in the pane's border
    // at the end of the typed text instead of inside the shell content.
    let rename_cursor = sess.rename_state.as_ref().and_then(|(rename_pane_id, _)| {
        // Only ZellijStyle has visible borders where we can position the cursor.
        if !matches!(sess.border_style, BorderStyle::ZellijStyle) {
            return None;
        }
        pane_rects
            .iter()
            .find(|(id, _)| id == rename_pane_id)
            .map(|(_, rect)| {
                let name_len = layout::get_pane_name(&tab.layout, *rename_pane_id)
                    .unwrap_or_default()
                    .len() as u16;
                // Cursor goes after "╭ " + name text = x + 1 (corner) + 1 (space) + name_len
                let cx = rect.x + 2 + name_len;
                let cy = rect.y;
                (cx, cy, true)
            })
    });

    // Compute border offsets for the focused pane (shared by rect and cursor).
    let focused_rect_and_offsets = pane_rects
        .iter()
        .find(|(id, _)| *id == tab.focused_pane)
        .map(|(_, rect)| {
            let (x_off, y_off, x_off_end, y_off_end) = match &sess.border_style {
                BorderStyle::ZellijStyle => {
                    if fits_zellij_border(rect.width, rect.height) {
                        (1u16, 1u16, 1u16, 1u16) // 1-cell border on each side
                    } else {
                        (0, 0, 0, 0)
                    }
                }
                BorderStyle::TmuxStyle => {
                    let has_tab_bar = layout::find_stack_for_pane(&tab.layout, tab.focused_pane)
                        .map(|panes| panes.len() > 1)
                        .unwrap_or(false);
                    if has_tab_bar {
                        (0, 1, 0, 0) // tab bar takes 1 row at top
                    } else {
                        (0, 0, 0, 0)
                    }
                }
            };
            (rect, x_off, y_off, x_off_end, y_off_end)
        });

    // Build the focused pane rect for the client (content area, excluding
    // borders). While the popup is up, the content the user is working in IS the
    // popup interior, so report that instead.
    let focused_pane_rect = match popup_interior {
        Some(interior) => Some(PaneRect {
            x: interior.x,
            y: interior.y,
            width: interior.width,
            height: interior.height,
        }),
        None => {
            focused_rect_and_offsets.map(|(rect, x_off, y_off, x_off_end, y_off_end)| PaneRect {
                x: rect.x + x_off,
                y: rect.y + y_off,
                width: rect.width.saturating_sub(x_off + x_off_end),
                height: rect.height.saturating_sub(y_off + y_off_end),
            })
        }
    };

    // Cursor: the popup wins outright when visible (the user is looking at it),
    // ahead of both an in-progress rename and the focused pane -- this replaces
    // the layout cursor wholesale rather than composing with it, so it is
    // correct over a zoomed pane too.
    let (cursor_x, cursor_y, cursor_visible, cursor_style) =
        if let (Some(interior), Some((pid, _))) = (popup_interior, popup) {
            match ps.get(&pid) {
                Some(pane_data) => (
                    interior.x
                        + std::cmp::min(
                            pane_data.screen.cursor_x,
                            interior.width.saturating_sub(1),
                        ),
                    interior.y
                        + std::cmp::min(
                            pane_data.screen.cursor_y,
                            interior.height.saturating_sub(1),
                        ),
                    pane_data.screen.cursor_visible,
                    pane_data.screen.cursor_style,
                ),
                None => (0, 0, false, 0),
            }
        } else if let Some(rc) = rename_cursor {
            (rc.0, rc.1, rc.2, 0u8)
        } else if let Some(pane_data) = ps.get(&tab.focused_pane) {
            if let Some((rect, x_off, y_off, x_off_end, y_off_end)) = focused_rect_and_offsets {
                let content_w = rect.width.saturating_sub(x_off + x_off_end);
                let content_h = rect.height.saturating_sub(y_off + y_off_end);
                (
                    rect.x
                        + x_off
                        + std::cmp::min(pane_data.screen.cursor_x, content_w.saturating_sub(1)),
                    rect.y
                        + y_off
                        + std::cmp::min(pane_data.screen.cursor_y, content_h.saturating_sub(1)),
                    pane_data.screen.cursor_visible,
                    pane_data.screen.cursor_style,
                )
            } else {
                (0, 0, false, 0)
            }
        } else {
            (0, 0, false, 0)
        };

    // DECCKM follows input, so it comes from the popup while it owns input.
    let application_cursor_keys = ps
        .get(&scroll_target)
        .map(|p| p.screen.application_cursor_keys)
        .unwrap_or(false);

    (
        cells,
        cursor_x,
        cursor_y,
        cursor_visible,
        cursor_style,
        focused_pane_rect,
        hit_regions,
        pane_rects,
        application_cursor_keys,
        popup,
    )
}

fn compute_diff(prev: &[Vec<RenderCell>], curr: &[Vec<RenderCell>]) -> Vec<CellChange> {
    let mut changes = Vec::new();
    for (y, row) in curr.iter().enumerate() {
        let prev_row = prev.get(y);
        for (x, cell) in row.iter().enumerate() {
            let prev_cell = prev_row.and_then(|r| r.get(x));
            if prev_cell != Some(cell) {
                // If this is a continuation cell (width 0) following a wide lead,
                // also repaint the lead so the wide glyph covers this column.
                // (The client skips width-0 changes, so a lone continuation change
                // would otherwise leave half a stale glyph.) Only push the lead
                // when it did not already differ this pass, to avoid duplicates.
                if cell.width == 0 && x > 0 {
                    if let Some(lead) = row.get(x - 1) {
                        if lead.width == 2 {
                            let prev_lead = prev_row.and_then(|r| r.get(x - 1));
                            if prev_lead == Some(lead) {
                                changes.push(CellChange {
                                    x: (x - 1) as u16,
                                    y: y as u16,
                                    cell: lead.clone(),
                                });
                            }
                        }
                    }
                }
                changes.push(CellChange {
                    x: x as u16,
                    y: y as u16,
                    cell: cell.clone(),
                });
            }
        }
    }
    changes
}

// ---------------------------------------------------------------------------
// Pane close helper
// ---------------------------------------------------------------------------

/// Close a pane, updating layout and session state. If the pane is the last
/// pane in its tab, the tab is closed. If the last tab closes, the session is
/// left empty.
/// Handle the after-effects of a session being removed (its last tab/pane was
/// closed and the session no longer exists in `state`).
///
/// Picks the next available session; if one exists, every client that was
/// attached to `removed` is re-pointed onto it and given a fresh full render.
/// If no sessions remain, those clients are detached and notified with a
/// `SessionDeleted` event so they can fall back or exit.
///
/// This is the shared post-removal logic used by pane close, tab close, and
/// close-tab-by-index.
async fn handle_session_removed(
    removed: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    let next_session = {
        let st = state.lock().await;
        st.sessions.keys().next().cloned()
    };
    match next_session {
        Some(next) => {
            // Switch all clients that were on the removed session to the next one.
            {
                let mut cls = clients.lock().await;
                for c in cls.values_mut() {
                    if c.session_name.as_deref() == Some(removed) {
                        c.session_name = Some(next.clone());
                    }
                }
            }
            // Clients now display a different session; invalidate their
            // baselines (from the old session) so they get a clean full render.
            invalidate_session_baselines(&next, clients, prev_frames).await;
            let _ = resize_session_panes(&next, state, panes, clients, config).await;
            broadcast_full_render(&next, state, panes, clients, config, prev_frames).await;
        }
        None => {
            // No sessions left -- notify affected clients so they disconnect
            // (or fall back to another server).
            let mut cls = clients.lock().await;
            for c in cls.values_mut() {
                if c.session_name.as_deref() == Some(removed) {
                    c.session_name = None;
                    let _ = c.tx.send(ServerMessage::Event(SessionEvent::SessionDeleted(
                        removed.to_string(),
                    )));
                }
            }
        }
    }
}

/// `exit_code` reported with [`SessionEvent::PaneExited`] when the pane's real
/// exit status is not observable: it was closed by command while its child was
/// still running, or it had already been reaped when a client asked about it.
/// Real codes are `0..=255` (a signalled child reports `128 + signo`), so this
/// can never collide with one.
const EXIT_CODE_UNKNOWN: i32 = -1;

/// Tell every client subscribed to one of `exits` that the pane is gone,
/// dropping the subscription in the same step.
///
/// This is the notification half of pane death. Without it a View cell aliasing
/// the pane cannot tell a dead pane from a quiet one: it sits on `waiting for …`
/// (or a frozen last snapshot painted as if it were live) forever, and every
/// keystroke typed into it vanishes. The event goes ONLY to clients that
/// actually held a subscription -- they are the ones who were being lied to --
/// and the send is fused with the subscription drop so a subscriber can never be
/// forgotten before it is told.
async fn notify_panes_exited(
    exits: &[(PaneId, i32)],
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    if exits.is_empty() {
        return;
    }
    let mut cls = clients.lock().await;
    for conn in cls.values_mut() {
        for &(pane_id, exit_code) in exits {
            conn.pane_scroll.remove(&pane_id);
            conn.pane_selection.remove(&pane_id);
            if conn.subscribed_panes.remove(&pane_id).is_some() {
                let _ = conn.tx.send(ServerMessage::Event(SessionEvent::PaneExited {
                    pane_id,
                    exit_code,
                }));
            }
            // A gesture on a pane that just went away can never make progress;
            // drop it (and its repeat timer) so the ticker stops replaying it.
            if conn.pane_drag.as_ref().map(|d| d.pane_id) == Some(pane_id) {
                conn.pane_drag = None;
                conn.pane_autoscroll_repeat = None;
            }
        }
    }
}

/// Drop `pane_ids` from the pane table and notify their subscribers.
///
/// Every path that destroys a pane (shell exit, `PaneClose`, `PaneCloseById`,
/// tab close, session/folder delete, the popup) funnels through here, so the
/// `PaneExited` notification cannot be forgotten by a new close path.
async fn reap_panes(
    pane_ids: &[PaneId],
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    if pane_ids.is_empty() {
        return;
    }
    // Read each child's status before dropping its `PaneData` (dropping the
    // `Pty` SIGHUPs the child). A pane closed by command still has a live child
    // and reports `EXIT_CODE_UNKNOWN`; nothing consumes the code today, it is
    // carried for the client's benefit.
    let exits: Vec<(PaneId, i32)> = {
        let mut ps = panes.lock().await;
        pane_ids
            .iter()
            .map(|&pane_id| {
                let code = match ps.remove(&pane_id) {
                    Some(pd) => pd
                        .pty
                        .try_wait()
                        .ok()
                        .flatten()
                        .unwrap_or(EXIT_CODE_UNKNOWN),
                    // Already reaped by another path -- still notify, a client
                    // may be holding a subscription to it.
                    None => EXIT_CODE_UNKNOWN,
                };
                (pane_id, code)
            })
            .collect()
    };
    // Taken under `clients` alone (the panes lock above is released) to keep the
    // locks unnested.
    notify_panes_exited(&exits, clients).await;
}

/// Run the NOTIFICATION half of pane death for a pane whose PTY exited but that
/// [`close_pane`] declined to close.
///
/// `close_pane` only knows the session's ACTIVE tab, so a pane living in a
/// background tab is left in the pane table with a dead PTY. Views exist to
/// watch exactly such panes, so their subscribers must still be told -- a cell
/// must not sit on a frozen snapshot just because its pane's tab wasn't in the
/// foreground. Layout state is deliberately untouched here (the pane staying in
/// its tab's `pane_order` is a separate, pre-existing bug); this only stops the
/// lying. A no-op when `close_pane` did reap the pane.
async fn notify_if_close_declined(
    pane_id: PaneId,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) {
    let declined = {
        let ps = panes.lock().await;
        ps.contains_key(&pane_id)
    };
    if declined {
        log::info!(
            "server: pane_id={pane_id} PTY exited but close_pane declined it; notifying subscribers"
        );
        // Deliberately NOT `try_wait`: the `Pty` stays in the table here (that is
        // what "declined" means), and reaping the child would leave `Pty::drop`
        // to SIGHUP -- and `get_pane_cwd` to read /proc for -- a pid the OS may
        // have recycled by then. Nothing consumes the code.
        notify_panes_exited(&[(pane_id, EXIT_CODE_UNKNOWN)], clients).await;
    }
}

async fn close_pane(
    pane_id: PaneId,
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    /// What to do after closing a pane.
    enum CloseAction {
        /// Normal close -- broadcast a full render for the current session.
        Broadcast,
        /// Last pane/tab in the session was closed and the session was removed
        /// from `state`; run the shared post-removal logic (switch clients to
        /// another session, or disconnect them if none remain).
        SessionRemoved,
        /// Nothing to do (pane not found, etc.).
        NoBroadcast,
    }

    // Panes to reap beyond `pane_id` itself (the popup, when the whole session
    // goes away -- it is in no layout tree, so nothing else would reclaim it).
    let mut also_reap: Vec<PaneId> = Vec::new();

    let action = {
        let mut st = state.lock().await;
        let sess = match st.sessions.get_mut(session_name) {
            Some(s) => s,
            None => return,
        };

        // The popup pane first: it is session-scoped and lives in NO tab, so the
        // layout path below would bail out on it (`pane_order` never contains
        // it) and leave a dead PTY behind with the popup still "open". This is
        // both the `PaneClose`-while-popup-open path and the popup shell's own
        // exit path, and it must work from any tab.
        if sess.popup_pane == Some(pane_id) {
            sess.take_popup();
            log::debug!("server: close_pane closed popup pane_id={pane_id}");
            CloseAction::Broadcast
        } else {
            let tab = match sess.tabs.get_mut(sess.active_tab) {
                Some(t) => t,
                None => return,
            };

            // Check if this pane actually belongs to the current tab.
            if !tab.pane_order.contains(&pane_id) {
                return;
            }

            let new_focus = tab.layout.close_pane(pane_id);
            tab.pane_order.retain(|&id| id != pane_id);
            // Closing the zoomed pane un-zooms the tab (tmux does the same):
            // `zoomed_pane` names the pane painted full-area, so a dead id there
            // would paint a pane that no longer exists.
            if tab.zoomed_pane == Some(pane_id) {
                tab.zoomed_pane = None;
            }

            if let Some(nf) = new_focus {
                tab.focus_pane(nf);
                // If in automatic mode, rebuild the tree
                if tab.layout_mode.is_automatic() {
                    tab.layout = tab.layout_mode.build_tree(&tab.pane_order, nf);
                }
                session::debug_check_invariant(sess, "close_pane");
                CloseAction::Broadcast
            } else {
                // Last pane in the tab was closed. Close the tab.
                let tab_idx = sess.active_tab;
                if sess.tabs.len() > 1 {
                    sess.tabs.remove(tab_idx);
                    if sess.active_tab >= sess.tabs.len() {
                        sess.active_tab = sess.tabs.len().saturating_sub(1);
                    }
                    CloseAction::Broadcast
                } else {
                    // Last tab in the session -- remove the session entirely.
                    let session_name_owned = session_name.to_string();
                    also_reap.extend(sess.take_popup());
                    st.sessions.remove(&session_name_owned);
                    CloseAction::SessionRemoved
                }
            }
        }
    };
    // Drop the pane(s) and TELL their subscribers. A subscription to a dead pane
    // must not linger, and a subscriber dropped without a word is exactly the
    // silent failure that left a View cell on `waiting…` forever.
    let mut reaped = Vec::with_capacity(1 + also_reap.len());
    reaped.push(pane_id);
    reaped.extend(also_reap);
    reap_panes(&reaped, panes, clients).await;
    match action {
        CloseAction::Broadcast => {
            let _ = resize_session_panes(session_name, state, panes, clients, config).await;
            broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
        }
        CloseAction::SessionRemoved => {
            handle_session_removed(session_name, state, panes, clients, config, prev_frames).await;
        }
        CloseAction::NoBroadcast => {}
    }
}

// ---------------------------------------------------------------------------
// PTY forwarding
// ---------------------------------------------------------------------------

async fn start_pty_forwarding(
    session_name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) {
    let pane_ids = {
        let st = state.lock().await;
        let sess = match st.sessions.get(session_name) {
            Some(s) => s,
            None => return,
        };
        let tab = match sess.tabs.get(sess.active_tab) {
            Some(t) => t,
            None => return,
        };
        let mut ids = layout::all_pane_ids(&tab.layout);
        // The popup pane is in no layout tree, so it would never get a
        // forwarding task (and never show output) unless added here. Doing it
        // here keeps every existing call site correct; the `forwarding_started`
        // guard below makes the repeat safe.
        ids.extend(sess.popup_pane);
        ids
    };

    log::debug!("server: start_pty_forwarding session={session_name:?} pane_ids={pane_ids:?}");

    let session_name = session_name.to_string();

    for pane_id in pane_ids {
        // Enforce exactly one forwarding task per pane. Check-and-set the
        // guard under the panes lock before spawning: if a task already
        // exists, skip this pane so we don't spawn a competing task that
        // could process PTY chunks out of order.
        {
            let mut ps = panes.lock().await;
            match ps.get_mut(&pane_id) {
                Some(pane_data) => {
                    if pane_data.forwarding_started {
                        continue; // already has its forwarding task
                    }
                    pane_data.forwarding_started = true;
                }
                None => continue,
            }
        }

        let state = Arc::clone(state);
        let panes = Arc::clone(panes);
        let clients = Arc::clone(clients);
        let config = Arc::clone(config);
        let prev_frames = Arc::clone(prev_frames);
        let dormant = Arc::clone(dormant);
        let session_name = session_name.clone();

        tokio::spawn(async move {
            loop {
                let recv_result = {
                    let mut ps = panes.lock().await;
                    if let Some(pane_data) = ps.get_mut(&pane_id) {
                        match pane_data.pty_rx.try_recv() {
                            Ok(data) => Some(Ok(data)),
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                Some(Err(()))
                            }
                        }
                    } else {
                        break;
                    }
                };

                match recv_result {
                    Some(Ok(first_chunk)) => {
                        // Drain all available data from the channel before
                        // processing, so rapid output is batched.
                        let mut chunks = vec![first_chunk];
                        {
                            let mut ps = panes.lock().await;
                            if let Some(pane_data) = ps.get_mut(&pane_id) {
                                while let Ok(more) = pane_data.pty_rx.try_recv() {
                                    chunks.push(more);
                                }
                            }
                        }

                        let (responses, bell, clipboard) = {
                            let mut ps = panes.lock().await;
                            if let Some(pane_data) = ps.get_mut(&pane_id) {
                                for chunk in &chunks {
                                    pane_data.screen.process_output(chunk);
                                }
                                log::debug!(
                                    "pty_forwarding: pane_id={}, cursor=({},{}), screen.rows={}, scroll_bottom={}",
                                    pane_id,
                                    pane_data.screen.cursor_x,
                                    pane_data.screen.cursor_y,
                                    pane_data.screen.rows,
                                    pane_data.screen.scroll_bottom
                                );
                                let bell = pane_data.screen.take_bell();
                                // Always drained, even when the config gate is
                                // off, so a disallowed write cannot sit pending
                                // and land the moment it is switched back on.
                                let clipboard = pane_data.screen.take_clipboard();
                                (pane_data.screen.take_responses(), bell, clipboard)
                            } else {
                                (Vec::new(), false, None)
                            }
                        };
                        // Record background-tab activity (no-op if this pane's
                        // tab is the foreground/active tab). Panes lock is
                        // released above; acquire state alone to preserve the
                        // state-before-panes lock order used elsewhere.
                        {
                            let mut st = state.lock().await;
                            st.record_pane_activity(pane_id, bell, std::time::Instant::now());
                        }
                        // Write any pending responses (e.g., DSR replies) back to the PTY.
                        if !responses.is_empty() {
                            let ps = panes.lock().await;
                            if let Some(pane_data) = ps.get(&pane_id) {
                                for resp in &responses {
                                    let _ = pane_data.pty.write_input(resp);
                                }
                            }
                        }
                        // An application asked for the system clipboard via OSC 52.
                        if let Some(text) = clipboard {
                            if config.general.allow_app_clipboard {
                                deliver_app_clipboard(pane_id, text, &state, &clients).await;
                            }
                        }
                        // Stream to the pane's View-cell subscribers, each at its
                        // own scroll offset and selection. Self-limiting: with no
                        // subscriber it returns before snapshotting, so a pane
                        // nobody watches costs nothing per PTY batch.
                        stream_pane_content(pane_id, &state, &panes, &clients).await;
                        broadcast_full_render(
                            &session_name,
                            &state,
                            &panes,
                            &clients,
                            &config,
                            &prev_frames,
                        )
                        .await;
                    }
                    Some(Err(())) => {
                        // Channel disconnected - process has exited.
                        log::debug!(
                            "server: PTY channel disconnected for pane_id={pane_id} session={session_name:?}"
                        );
                        // Close the pane automatically.
                        close_pane(
                            pane_id,
                            &session_name,
                            &state,
                            &panes,
                            &clients,
                            &config,
                            &prev_frames,
                        )
                        .await;
                        notify_if_close_declined(pane_id, &panes, &clients).await;
                        save_if_enabled(&state, &panes, &config, &dormant).await;
                        // A pane dying on its own removes a row from every
                        // subscriber's tree, and no command ran to say so.
                        mark_session_tree_dirty();
                        break;
                    }
                    None => {
                        // No data available yet, sleep briefly.
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        let ps = panes.lock().await;
                        if !ps.contains_key(&pane_id) {
                            break;
                        }
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Event-driven save helper
// ---------------------------------------------------------------------------

/// Save the server state to disk if `save_sessions` is enabled.
///
/// This captures the current working directory of every pane and writes
/// the full server state (live sessions merged with any still-dormant
/// sessions) to the persistence file. It is called after
/// every structural change (session/tab/pane create/close/rename).
async fn save_if_enabled(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
    dormant: &DormantStore,
) {
    if !config.general.save_sessions {
        log::debug!("server: save_if_enabled skipped (save_sessions=false)");
        return;
    }
    log::debug!("server: save_if_enabled saving state");
    let st = state.lock().await;
    let ps = panes.lock().await;
    let mut pane_cwds = HashMap::new();
    for (&pane_id, pane_data) in ps.iter() {
        if let Some(cwd) = crate::server::persistence::get_pane_cwd(pane_data.pty.child_pid) {
            pane_cwds.insert(pane_id, cwd);
        }
    }
    if let Ok(mut persisted) =
        crate::server::persistence::PersistedState::from_server(&st, &pane_cwds)
    {
        // Persist live + still-dormant sessions so a live-only save never
        // clobbers un-resurrected dormant sessions on disk. No-op (byte-identical
        // to a live-only save) whenever the dormant store is empty, which is the
        // case on the default automatic_restore path and when save_sessions is
        // the only persistence toggle.
        {
            let d = dormant.lock().await;
            merge_dormant_into(&mut persisted, &d);
        }
        if let Err(e) = crate::server::persistence::save_state(&persisted) {
            log::error!("failed to save state: {e}");
        }
    }
}

/// Deep-clone a [`ServerState`] via serde (it doesn't derive `Clone`).
fn clone_server_state(state: &ServerState) -> Option<ServerState> {
    let json = serde_json::to_string(state).ok()?;
    serde_json::from_str(&json).ok()
}

/// Merge the still-dormant sessions/folders/cwds into a live snapshot so a
/// save writes the union of live and dormant state. Live entries win on any
/// name collision; folders are unioned by `session_ids`.
fn merge_dormant_into(target: &mut PersistedState, dormant: &PersistedState) {
    if dormant.state.sessions.is_empty() && dormant.state.folders.is_empty() {
        return;
    }
    let dstate = match clone_server_state(&dormant.state) {
        Some(s) => s,
        None => {
            log::error!("merge_dormant_into: failed to clone dormant state; saving live-only");
            return;
        }
    };
    for (name, session) in dstate.sessions {
        target.state.sessions.entry(name).or_insert(session);
    }
    for (fname, folder) in dstate.folders {
        let entry = target
            .state
            .folders
            .entry(fname.clone())
            .or_insert_with(|| Folder {
                name: fname,
                session_ids: Vec::new(),
            });
        for sid in folder.session_ids {
            if !entry.session_ids.contains(&sid) {
                entry.session_ids.push(sid);
            }
        }
    }
    for (&pid, cwd) in &dormant.pane_cwds {
        target.pane_cwds.entry(pid).or_insert_with(|| cwd.clone());
    }
    // Keep id counters above any merged pane/tab id.
    target.state.ensure_id_counters();
}

// ---------------------------------------------------------------------------
// State restore on startup
// ---------------------------------------------------------------------------

/// Bring a single session (already present in live `ServerState`) to life:
/// spawn a PTY for each of its panes (using saved CWDs) and start PTY
/// forwarding for them. Panes are initially sized 80x24; they are resized when
/// the first client attaches and sends a `Resize`.
///
/// This is the shared per-session materialization path used by both startup
/// `automatic_restore` ([`restore_state`]) and on-demand
/// [`handle_resurrect_session`].
#[allow(clippy::too_many_arguments)]
async fn materialize_session(
    session_name: &str,
    pane_cwds: &HashMap<u64, String>,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    // Collect this session's pane ids across all tabs from live state.
    let pane_ids: Vec<PaneId> = {
        let st = state.lock().await;
        match st.sessions.get(session_name) {
            // `Tab::panes()`, not the layout tree: the two must agree (the
            // structural invariant), and reading the same side as every other
            // consumer is what keeps a restored pane from silently getting no
            // PTY when they ever drift.
            Some(sess) => sess.tabs.iter().flat_map(|t| t.panes().to_vec()).collect(),
            None => {
                log::warn!("materialize_session: session '{session_name}' not in live state");
                return Ok(());
            }
        }
    };

    // Spawn PTYs for all panes. Panes start at 80x24 and are resized on attach.
    let default_cols: u16 = 80;
    let default_rows: u16 = 24;
    for pane_id in &pane_ids {
        let cwd = pane_cwds.get(pane_id).cloned();
        let cwd_path = cwd.as_deref().map(std::path::Path::new);
        if let Err(e) = spawn_pane(
            *pane_id,
            default_cols,
            default_rows,
            None,
            cwd_path,
            panes,
            config,
        )
        .await
        {
            log::warn!("failed to spawn PTY for restored pane {pane_id}: {e}");
        }
    }

    // Start PTY forwarding for every pane.
    for pane_id in pane_ids {
        // Enforce exactly one forwarding task per pane (see start_pty_forwarding).
        {
            let mut ps = panes.lock().await;
            match ps.get_mut(&pane_id) {
                Some(pane_data) => {
                    if pane_data.forwarding_started {
                        continue; // already has its forwarding task
                    }
                    pane_data.forwarding_started = true;
                }
                None => continue,
            }
        }

        let state = Arc::clone(state);
        let panes = Arc::clone(panes);
        let clients = Arc::clone(clients);
        let config = Arc::clone(config);
        let prev_frames = Arc::clone(prev_frames);
        let dormant = Arc::clone(dormant);
        let session_name = session_name.to_string();

        tokio::spawn(async move {
            loop {
                let recv_result = {
                    let mut ps = panes.lock().await;
                    if let Some(pane_data) = ps.get_mut(&pane_id) {
                        match pane_data.pty_rx.try_recv() {
                            Ok(data) => Some(Ok(data)),
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                Some(Err(()))
                            }
                        }
                    } else {
                        break;
                    }
                };

                match recv_result {
                    Some(Ok(data)) => {
                        let (responses, bell, clipboard) = {
                            let mut ps = panes.lock().await;
                            if let Some(pane_data) = ps.get_mut(&pane_id) {
                                pane_data.screen.process_output(&data);
                                let bell = pane_data.screen.take_bell();
                                let clipboard = pane_data.screen.take_clipboard();
                                (pane_data.screen.take_responses(), bell, clipboard)
                            } else {
                                (Vec::new(), false, None)
                            }
                        };
                        {
                            let mut st = state.lock().await;
                            st.record_pane_activity(pane_id, bell, std::time::Instant::now());
                        }
                        if !responses.is_empty() {
                            let ps = panes.lock().await;
                            if let Some(pane_data) = ps.get(&pane_id) {
                                for resp in &responses {
                                    let _ = pane_data.pty.write_input(resp);
                                }
                            }
                        }
                        // An application asked for the system clipboard via OSC 52.
                        if let Some(text) = clipboard {
                            if config.general.allow_app_clipboard {
                                deliver_app_clipboard(pane_id, text, &state, &clients).await;
                            }
                        }
                        // Stream to the pane's View-cell subscribers, each at its
                        // own scroll offset and selection. This resurrect path is a
                        // second per-pane forwarding loop; subscribers to a
                        // materialized session must stream here too.
                        stream_pane_content(pane_id, &state, &panes, &clients).await;
                        broadcast_full_render(
                            &session_name,
                            &state,
                            &panes,
                            &clients,
                            &config,
                            &prev_frames,
                        )
                        .await;
                    }
                    Some(Err(())) => {
                        close_pane(
                            pane_id,
                            &session_name,
                            &state,
                            &panes,
                            &clients,
                            &config,
                            &prev_frames,
                        )
                        .await;
                        notify_if_close_declined(pane_id, &panes, &clients).await;
                        save_if_enabled(&state, &panes, &config, &dormant).await;
                        // A pane dying on its own removes a row from every
                        // subscriber's tree, and no command ran to say so.
                        mark_session_tree_dirty();
                        break;
                    }
                    None => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        let ps = panes.lock().await;
                        if !ps.contains_key(&pane_id) {
                            break;
                        }
                    }
                }
            }
        });
    }

    Ok(())
}

/// Restore server state from a persisted snapshot on startup
/// (`automatic_restore = true`).
///
/// Replaces the server's state with the deserialized state, then materializes
/// every session via the shared [`materialize_session`] path.
async fn restore_state(
    server: &RemuxServer,
    persisted: crate::server::persistence::PersistedState,
) -> Result<()> {
    let mut restored_state = persisted.state;
    restored_state.ensure_id_counters();
    // `pane_order` is `#[serde(default)]`, so a snapshot written before the
    // field existed restores empty alongside a full tree. Repair on the way in,
    // so the structural invariant holds from the first frame and every consumer
    // of the pane set (PTY materialization, View cell titles) sees the same
    // panes.
    for sess in restored_state.sessions.values_mut() {
        for tab in &mut sess.tabs {
            tab.reconcile_pane_order();
        }
    }

    let session_names: Vec<String> = restored_state.sessions.keys().cloned().collect();

    // Replace the server state.
    {
        let mut st = server.state.lock().await;
        *st = restored_state;
    }

    for session_name in &session_names {
        materialize_session(
            session_name,
            &persisted.pane_cwds,
            &server.state,
            &server.panes,
            &server.clients,
            &server.config,
            &server.prev_frames,
            &server.dormant,
        )
        .await?;
    }

    log::info!("restored {} sessions", session_names.len());
    Ok(())
}

/// Move the named session from a dormant snapshot into live `ServerState`,
/// recreating folder membership. Returns the CWDs of the session's panes (for
/// PTY spawning) on success, or `None` if the name isn't dormant or a live
/// session already exists under that name.
///
/// Pure state manipulation with no PTY side effects, so it is unit-testable
/// independently of [`materialize_session`]. Guards on the live collision
/// *before* removing from the dormant snapshot so a name clash never silently
/// drops the dormant session.
fn take_dormant_session(
    dormant: &mut PersistedState,
    live: &mut ServerState,
    name: &str,
) -> Option<HashMap<u64, String>> {
    if live.sessions.contains_key(name) {
        log::warn!("resurrect: live session '{name}' already exists; ignoring");
        return None;
    }
    let mut session = match dormant.state.sessions.remove(name) {
        Some(s) => s,
        None => {
            log::warn!("resurrect: no dormant session '{name}'");
            return None;
        }
    };

    // Same deserialization repair as `restore_state`: a dormant snapshot comes
    // off disk and may predate `pane_order`.
    for tab in &mut session.tabs {
        tab.reconcile_pane_order();
    }

    // Detach from any dormant folder membership and collect its pane CWDs.
    let folder = session.folder.clone();
    if let Some(ref fname) = folder {
        if let Some(f) = dormant.state.folders.get_mut(fname) {
            f.session_ids.retain(|s| s != name);
        }
    }
    let pane_ids: Vec<u64> = session
        .tabs
        .iter()
        .flat_map(|t| layout::all_pane_ids(&t.layout))
        .collect();
    let mut cwds = HashMap::new();
    for pid in &pane_ids {
        if let Some(cwd) = dormant.pane_cwds.remove(pid) {
            cwds.insert(*pid, cwd);
        }
    }

    // Insert into live state, recreating folder membership if needed.
    if let Some(ref fname) = folder {
        let entry = live.folders.entry(fname.clone()).or_insert_with(|| Folder {
            name: fname.clone(),
            session_ids: Vec::new(),
        });
        if !entry.session_ids.iter().any(|s| s == name) {
            entry.session_ids.push(name.to_string());
        }
    }
    live.sessions.insert(name.to_string(), session);
    live.ensure_id_counters();
    Some(cwds)
}

/// Materialize a dormant session into a live session on client request.
///
/// Migrates the session out of the dormant store into live `ServerState` (via
/// [`take_dormant_session`]) and brings it live via the shared
/// [`materialize_session`] path. No-op if the name isn't dormant or a live
/// session already exists under that name.
async fn handle_resurrect_session(
    name: &str,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
    dormant: &DormantStore,
) -> Result<()> {
    // Lock state before dormant to match `save_if_enabled`'s ordering and avoid
    // any lock-order inversion.
    let cwds = {
        let mut st = state.lock().await;
        let mut d = dormant.lock().await;
        match take_dormant_session(&mut d, &mut st, name) {
            Some(c) => c,
            None => return Ok(()),
        }
    };

    log::info!("resurrecting dormant session '{name}'");
    materialize_session(
        name,
        &cwds,
        state,
        panes,
        clients,
        config,
        prev_frames,
        dormant,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        arrow_report, button_report, compute_diff, merge_dormant_into, mouse_route,
        next_layout_mode, saved_custom_is_restorable, take_dormant_session, wheel_report,
        ButtonPhase, MouseGesture, MouseRoute,
    };
    use crate::protocol::RenderCell;
    use crate::screen::Screen;
    use crate::server::layout::{
        BspLayout, CustomLayout, GridLayout, LayoutMode, LayoutNode, MasterLayout, MonocleLayout,
    };
    use crate::server::persistence::PersistedState;
    use crate::server::session::ServerState;
    use std::collections::HashMap;

    /// Build a `ServerState` with the named sessions (each optionally filed
    /// under a folder), returning it alongside the first pane id of the first
    /// listed session (handy for CWD assertions).
    fn state_with_sessions(sessions: &[(&str, Option<&str>)]) -> ServerState {
        let mut st = ServerState::new();
        for (name, folder) in sessions {
            if let Some(f) = folder {
                if !st.folders.contains_key(*f) {
                    st.create_folder(f).unwrap();
                }
            }
            st.create_session(
                name,
                *folder,
                crate::config::BorderStyle::ZellijStyle,
                Default::default(),
                (80, 80),
            )
            .unwrap();
        }
        st
    }

    #[test]
    fn take_dormant_session_migrates_one_session_from_persisted_state() {
        // A dormant snapshot with two sessions, one filed under "work".
        let dstate = state_with_sessions(&[("alpha", Some("work")), ("beta", None)]);
        let alpha_pane =
            crate::server::layout::all_pane_ids(&dstate.sessions["alpha"].tabs[0].layout)[0];
        let mut cwds = HashMap::new();
        cwds.insert(alpha_pane, "/tmp/alpha".to_string());
        let mut dormant = PersistedState {
            state: dstate,
            pane_cwds: cwds,
        };

        let mut live = ServerState::new();
        let got =
            take_dormant_session(&mut dormant, &mut live, "alpha").expect("alpha should resurrect");

        // alpha migrated into live with its folder membership recreated.
        assert!(live.sessions.contains_key("alpha"));
        assert!(live
            .folders
            .get("work")
            .unwrap()
            .session_ids
            .contains(&"alpha".to_string()));
        assert_eq!(got.get(&alpha_pane).unwrap(), "/tmp/alpha");

        // alpha (and its cwd) removed from dormant; beta remains dormant.
        assert!(!dormant.state.sessions.contains_key("alpha"));
        assert!(dormant.state.sessions.contains_key("beta"));
        assert!(dormant.pane_cwds.is_empty());

        // A name already live returns None (no double-resurrect).
        assert!(take_dormant_session(&mut dormant, &mut live, "alpha").is_none());
        // An unknown name returns None.
        assert!(take_dormant_session(&mut dormant, &mut live, "nope").is_none());
    }

    #[test]
    fn merge_dormant_into_unions_live_and_dormant() {
        let mut target = PersistedState {
            state: state_with_sessions(&[("live1", None)]),
            pane_cwds: HashMap::new(),
        };
        let dormant = PersistedState {
            state: state_with_sessions(&[("dorm1", Some("f"))]),
            pane_cwds: HashMap::new(),
        };

        merge_dormant_into(&mut target, &dormant);

        assert!(target.state.sessions.contains_key("live1"));
        assert!(target.state.sessions.contains_key("dorm1"));
        assert!(target
            .state
            .folders
            .get("f")
            .unwrap()
            .session_ids
            .contains(&"dorm1".to_string()));
    }

    #[test]
    fn merge_dormant_into_is_noop_when_dormant_empty() {
        // This is the default automatic_restore path: dormant is empty, so the
        // save must be byte-identical to a live-only save.
        let mut target = PersistedState {
            state: state_with_sessions(&[("live1", None), ("live2", None)]),
            pane_cwds: HashMap::new(),
        };
        let before = serde_json::to_string(&target).unwrap();

        let empty = PersistedState {
            state: ServerState::new(),
            pane_cwds: HashMap::new(),
        };
        merge_dormant_into(&mut target, &empty);

        let after = serde_json::to_string(&target).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn compute_diff_emits_wide_lead_when_only_continuation_differs() {
        let lead = RenderCell {
            c: '中',
            width: 2,
            ..RenderCell::default()
        };
        let continuation = RenderCell {
            c: ' ',
            width: 0,
            ..RenderCell::default()
        };
        let narrow_a = RenderCell {
            c: 'a',
            ..RenderCell::default()
        };

        // Lead unchanged; only the continuation column differs (it used to hold a
        // narrow char, now it is the wide glyph's continuation). The client skips
        // width-0 changes, so the diff must ALSO re-emit the unchanged lead to
        // repaint the whole glyph and cover the continuation column.
        let prev = vec![vec![lead.clone(), narrow_a.clone()]];
        let curr = vec![vec![lead.clone(), continuation.clone()]];

        let changes = compute_diff(&prev, &curr);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].x, 0);
        assert_eq!(changes[0].cell.width, 2);
        assert_eq!(changes[1].x, 1);
        assert_eq!(changes[1].cell.width, 0);

        // When the lead itself also changed, it is emitted once (at x=0) and not
        // duplicated by the continuation handling at x=1.
        let prev2 = vec![vec![narrow_a.clone(), narrow_a.clone()]];
        let curr2 = vec![vec![lead.clone(), continuation.clone()]];
        let changes2 = compute_diff(&prev2, &curr2);
        assert_eq!(changes2.len(), 2);
        assert_eq!(changes2[0].x, 0);
        assert_eq!(changes2[1].x, 1);
    }

    #[test]
    fn compute_diff_emits_cell_when_only_combining_differs() {
        // A base glyph gaining a combining mark differs only in `combining`.
        // `RenderCell` derives `PartialEq` over all fields, so the diff must
        // detect and emit the changed cell.
        let base = RenderCell {
            c: 'e',
            ..RenderCell::default()
        };
        let accented = RenderCell {
            c: 'e',
            combining: vec!['\u{301}'],
            ..RenderCell::default()
        };

        let prev = vec![vec![base.clone()]];
        let curr = vec![vec![accented.clone()]];
        let changes = compute_diff(&prev, &curr);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].x, 0);
        assert_eq!(changes[0].cell.combining, vec!['\u{301}']);

        // Losing the mark is likewise detected.
        let changes_back = compute_diff(&curr, &prev);
        assert_eq!(changes_back.len(), 1);
        assert!(changes_back[0].cell.combining.is_empty());
    }

    #[test]
    fn test_wheel_report_sgr() {
        // Wheel up: button 64, SGR encoding, 1-based coords.
        assert_eq!(wheel_report(true, true, 5, 10), b"\x1b[<64;5;10M".to_vec());
        // Wheel down: button 65.
        assert_eq!(wheel_report(true, false, 1, 1), b"\x1b[<65;1;1M".to_vec());
    }

    #[test]
    fn test_wheel_report_legacy() {
        // Legacy X10: ESC [ M then (32+btn), (32+col), (32+row).
        assert_eq!(
            wheel_report(false, true, 5, 10),
            vec![0x1b, b'[', b'M', 32 + 64, 32 + 5, 32 + 10]
        );
        // Coordinates saturate into a single byte.
        assert_eq!(
            wheel_report(false, false, 300, 400),
            vec![0x1b, b'[', b'M', 32 + 65, 255, 255]
        );
    }

    #[test]
    fn test_arrow_report() {
        assert_eq!(arrow_report(false, true), vec![0x1b, b'[', b'A']);
        assert_eq!(arrow_report(false, false), vec![0x1b, b'[', b'B']);
        assert_eq!(arrow_report(true, true), vec![0x1b, b'O', b'A']);
        assert_eq!(arrow_report(true, false), vec![0x1b, b'O', b'B']);
    }

    #[test]
    fn test_button_report_sgr() {
        // SGR keeps the button number on a release and marks it with a
        // lowercase final byte; motion sets the +32 motion bit.
        assert_eq!(
            button_report(true, ButtonPhase::Press, 10, 5),
            b"\x1b[<0;10;5M".to_vec()
        );
        assert_eq!(
            button_report(true, ButtonPhase::Motion, 10, 6),
            b"\x1b[<32;10;6M".to_vec()
        );
        assert_eq!(
            button_report(true, ButtonPhase::Release, 10, 6),
            b"\x1b[<0;10;6m".to_vec()
        );
    }

    #[test]
    fn test_button_report_legacy() {
        // X10 has no lowercase form: a release is button 3 ("no button").
        assert_eq!(
            button_report(false, ButtonPhase::Press, 5, 10),
            vec![0x1b, b'[', b'M', 32, 32 + 5, 32 + 10]
        );
        assert_eq!(
            button_report(false, ButtonPhase::Motion, 5, 10),
            vec![0x1b, b'[', b'M', 32 + 32, 32 + 5, 32 + 10]
        );
        assert_eq!(
            button_report(false, ButtonPhase::Release, 5, 10),
            vec![0x1b, b'[', b'M', 32 + 3, 32 + 5, 32 + 10]
        );
        // Coordinates saturate into a single byte, as in `wheel_report`.
        assert_eq!(
            button_report(false, ButtonPhase::Press, 300, 400),
            vec![0x1b, b'[', b'M', 32, 255, 255]
        );
    }

    /// The one routing decision every mouse path shares. A plain shell scrolls
    /// and selects in remux; an application that asked for mouse events gets
    /// them; an alt-screen application that did not gets the arrow fallback for
    /// the wheel and, crucially, a `scrollback: false` verdict for buttons --
    /// remux's scrollback there belongs to the primary screen, so an edge drag
    /// must not chase it (the drag-autoscroll spin).
    #[test]
    fn test_mouse_route() {
        let mut s = Screen::new(80, 24, 100);
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, false),
            MouseRoute::Remux { scrollback: true }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::Remux { scrollback: true }
        );

        // Alt screen, no tracking (e.g. `less`): arrows for the wheel, no
        // scrollback for a drag.
        s.process_output(b"\x1b[?1049h");
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, false),
            MouseRoute::AltArrows { app_cursor: false }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::Remux { scrollback: false }
        );
        s.process_output(b"\x1b[?1h");
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, false),
            MouseRoute::AltArrows { app_cursor: true }
        );

        // Tracking wins over everything, and carries the encoding + whether the
        // app asked for motion.
        s.process_output(b"\x1b[?1000h");
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::App {
                sgr: false,
                motion: false
            }
        );
        s.process_output(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, false),
            MouseRoute::App {
                sgr: true,
                motion: true
            }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::App {
                sgr: true,
                motion: true
            }
        );

        // Copy mode (Visual) outranks the application: the user asked for the
        // mouse explicitly. Selection still works, but not the scrolling that
        // would chase the primary screen's history under a full-screen app.
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, true),
            MouseRoute::Remux { scrollback: false }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Wheel, true),
            MouseRoute::Remux { scrollback: false }
        );

        // Leaving the alt screen releases the app's claim on the mouse.
        s.process_output(b"\x1b[?1049l");
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, false),
            MouseRoute::Remux { scrollback: true }
        );
        assert_eq!(
            mouse_route(&s, MouseGesture::Button, true),
            MouseRoute::Remux { scrollback: true }
        );
    }

    // -- Layout cycle (Alt+Space) -------------------------------------------

    #[test]
    fn next_layout_mode_automatic_cycle() {
        // Bsp -> Master -> Monocle -> Grid regardless of the saved-custom flag.
        assert!(matches!(
            next_layout_mode(&LayoutMode::Bsp(BspLayout), false),
            LayoutMode::Master(_)
        ));
        assert!(matches!(
            next_layout_mode(&LayoutMode::Master(MasterLayout::default()), false),
            LayoutMode::Monocle(_)
        ));
        // Monocle now always advances to Grid, even with a saved custom layout.
        assert!(matches!(
            next_layout_mode(&LayoutMode::Monocle(MonocleLayout), false),
            LayoutMode::Grid(_)
        ));
        assert!(matches!(
            next_layout_mode(&LayoutMode::Monocle(MonocleLayout), true),
            LayoutMode::Grid(_)
        ));
    }

    #[test]
    fn next_layout_mode_custom_starts_cycle_at_bsp() {
        assert!(matches!(
            next_layout_mode(&LayoutMode::Custom(CustomLayout), false),
            LayoutMode::Bsp(_)
        ));
    }

    #[test]
    fn next_layout_mode_grid_returns_to_custom_only_when_saved() {
        // Grid is the last automatic before wrap. With a restorable saved custom
        // layout, it wraps back to Custom.
        assert!(matches!(
            next_layout_mode(&LayoutMode::Grid(GridLayout), true),
            LayoutMode::Custom(_)
        ));
        // Without one, Grid wraps to Bsp as usual.
        assert!(matches!(
            next_layout_mode(&LayoutMode::Grid(GridLayout), false),
            LayoutMode::Bsp(_)
        ));
    }

    /// A two-pane custom stack whose traversal order differs from the caller's
    /// `pane_order` (panes `[2, 1]` vs insertion order `[1, 2]`).
    fn custom_stack_2_1() -> LayoutNode {
        LayoutNode::Stack {
            panes: vec![2, 1],
            names: vec![],
            custom_names: vec![],
            active: 0,
        }
    }

    #[test]
    fn saved_custom_restorable_ignores_pane_order() {
        // Same set, different order -> restorable.
        let saved = Some(custom_stack_2_1());
        assert!(saved_custom_is_restorable(&saved, &[1, 2]));
    }

    #[test]
    fn saved_custom_not_restorable_when_pane_added() {
        let saved = Some(custom_stack_2_1());
        assert!(!saved_custom_is_restorable(&saved, &[1, 2, 3]));
    }

    #[test]
    fn saved_custom_not_restorable_when_pane_removed() {
        let saved = Some(custom_stack_2_1());
        assert!(!saved_custom_is_restorable(&saved, &[1]));
    }

    #[test]
    fn saved_custom_not_restorable_when_none() {
        assert!(!saved_custom_is_restorable(&None, &[1, 2]));
    }
}
