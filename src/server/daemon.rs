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
    composite, hit_test, ClickTarget, HitRegions, MouseSelection, StatusInfo,
};
use crate::server::layout::{self, CustomLayout, LayoutMode, PaneId, Rect};
use crate::server::pty::{self, Pty};
use crate::server::session::ServerState;

/// Type alias for the per-session previous-frame cache used for diff rendering.
pub type PrevFrameCache = Arc<Mutex<HashMap<String, Vec<Vec<RenderCell>>>>>;

/// Read the process name from `/proc/<pid>/comm`.
///
/// Falls back to `"shell"` if the file is unreadable.
fn get_process_name(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "shell".to_string())
}

/// Return the path to the Unix domain socket used for client-server
/// communication.
pub fn socket_path() -> PathBuf {
    let runtime_dir = dirs::runtime_dir()
        .or_else(|| std::env::var("XDG_RUNTIME_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime_dir.join("remux.sock")
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
}

// MouseSelection is imported from compositor.

/// A connected client with metadata about which session it is attached to.
struct ClientConnection {
    session_name: Option<String>,
    /// Sender to push `ServerMessage`s to this client's writer task.
    tx: mpsc::UnboundedSender<ServerMessage>,
    cols: u16,
    rows: u16,
    /// The client's current input mode (e.g. "INSERT", "NORMAL", "VISUAL").
    mode: String,
    /// Active mouse selection, if any.
    mouse_selection: Option<MouseSelection>,
    /// Search match info: (current_match, total_matches).
    search_info: Option<(usize, usize)>,
}

/// The Remux server.
pub struct RemuxServer {
    state: Arc<Mutex<ServerState>>,
    panes: Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: Arc<Config>,
    clients: Arc<Mutex<HashMap<u64, ClientConnection>>>,
    /// Monotonically increasing counter for stable client IDs.
    next_client_id: Arc<AtomicU64>,
    /// Previous composite frame per session, for diff computation.
    prev_frames: Arc<Mutex<HashMap<String, Vec<Vec<RenderCell>>>>>,
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
        }
    }

    /// Start the server: bind socket, accept connections, run the event loop.
    pub async fn run(config: Config) -> Result<()> {
        let server = Self::new(config);

        // Restore persisted state before accepting connections.
        if server.config.general.automatic_restore {
            match crate::server::persistence::load_state() {
                Ok(Some(persisted)) => {
                    log::info!("restoring persisted state");
                    if let Err(e) = restore_state(&server, persisted).await {
                        log::warn!("failed to restore state: {e}, starting fresh");
                    }
                }
                Ok(None) => {
                    log::info!("no persisted state found, starting fresh");
                }
                Err(e) => {
                    log::warn!("failed to load persisted state: {e}, starting fresh");
                }
            }
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

        // Set up signal handlers for graceful shutdown.
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("registering SIGTERM handler")?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("registering SIGINT handler")?;

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

        if let Ok(persisted) =
            crate::server::persistence::PersistedState::from_server(&state, &pane_cwds)
        {
            if let Err(e) = crate::server::persistence::save_state(&persisted) {
                log::error!("failed to save state on shutdown: {e}");
            } else {
                log::info!("state saved successfully");
            }
        }

        // Drop locks before cleanup.
        drop(state);
        drop(panes);

        // Remove socket file.
        let _ = std::fs::remove_file(socket_path);
        log::info!("shutdown complete");
    }

    /// Handle a newly accepted client connection.
    async fn handle_new_connection(&self, stream: tokio::net::UnixStream) {
        let (read_half, write_half) = stream.into_split();

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
                    mode: "INSERT".to_string(),
                    mouse_selection: None,
                    search_info: None,
                },
            );
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

        // Spawn reader task.
        let state = Arc::clone(&self.state);
        let panes = Arc::clone(&self.panes);
        let clients = Arc::clone(&self.clients);
        let config = Arc::clone(&self.config);
        let prev_frames = Arc::clone(&self.prev_frames);

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
                        handle_client_disconnect(client_id, &clients).await;
                        break;
                    }
                    Err(e) => {
                        log::error!("error reading from client {client_id}: {e}");
                        handle_client_disconnect(client_id, &clients).await;
                        break;
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
) -> Result<()> {
    match msg {
        ClientMessage::Attach { session_name } => {
            handle_attach(
                client_id,
                &session_name,
                state,
                panes,
                clients,
                config,
                prev_frames,
            )
            .await
        }
        ClientMessage::Detach => {
            handle_detach(client_id, clients).await;
            Ok(())
        }
        ClientMessage::Input { data } => {
            handle_input(client_id, &data, state, panes, clients).await
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
            handle_command(client_id, cmd, state, panes, clients, config, prev_frames).await
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
            save_if_enabled(state, panes, config).await;
            result
        }
        ClientMessage::ListSessions => handle_list_sessions(client_id, state, clients).await,
        ClientMessage::KillSession { name } => {
            let result = handle_kill_session(&name, state, panes, clients).await;
            save_if_enabled(state, panes, config).await;
            result
        }
        ClientMessage::ListSessionTree => {
            handle_list_session_tree(client_id, state, panes, clients).await
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
        ClientMessage::MouseClick { x, y } => {
            handle_mouse_click(client_id, x, y, state, panes, clients, config, prev_frames).await
        }
        ClientMessage::MouseDrag {
            start_x,
            start_y,
            end_x,
            end_y,
            is_final,
        } => {
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

    // Resize panes to match the attaching client's terminal dimensions.
    resize_session_panes(session_name, cols, rows, state, panes, config).await?;

    send_full_render_to_client(
        client_id,
        session_name,
        cols,
        rows,
        state,
        panes,
        clients,
        config,
        prev_frames,
    )
    .await;

    start_pty_forwarding(session_name, state, panes, clients, config, prev_frames).await;
    Ok(())
}

async fn handle_detach(client_id: u64, clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>) {
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

    let active_pane = {
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

    let ps = panes.lock().await;
    if let Some(pane_data) = ps.get(&active_pane) {
        pane_data.pty.write_input(data)?;
    }
    Ok(())
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

    if let Some(session_name) = session_name {
        resize_session_panes(&session_name, cols, rows, state, panes, config).await?;
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
) -> Result<()> {
    let (session_name, cols, rows) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => (c.session_name.clone(), c.cols, c.rows),
            None => return Ok(()),
        }
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    match cmd {
        RemuxCommand::TabNew => {
            let pane_id = {
                let mut st = state.lock().await;
                st.create_tab(&session_name, "shell", LayoutMode::default())?
            };
            spawn_pane(pane_id, cols, rows, None, None, panes, config).await?;
            start_pty_forwarding(&session_name, state, panes, clients, config, prev_frames).await;
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
            let (pane_ids, _deleted) = {
                let mut st = state.lock().await;
                st.close_tab(&session_name, tab_idx)?
            };
            {
                let mut ps = panes.lock().await;
                for pid in pane_ids {
                    ps.remove(&pid);
                }
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabGoto(idx) => {
            {
                let mut st = state.lock().await;
                st.goto_tab(&session_name, idx)?;
            }
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
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::TabRename(name) => {
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
        RemuxCommand::PaneSplitVertical => {
            let new_pane_id = {
                let mut st = state.lock().await;
                let new_pane_id = st.next_pane_id();
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // Eject to Custom mode on manual split
                if tab.layout_mode.is_automatic() {
                    tab.layout_mode = LayoutMode::Custom(CustomLayout);
                }
                let focused = tab.focused_pane;
                tab.layout.split_vertical(focused, new_pane_id);
                tab.focused_pane = new_pane_id;
                tab.pane_order.push(new_pane_id);
                new_pane_id
            };
            spawn_pane(new_pane_id, cols / 2, rows, None, None, panes, config).await?;
            start_pty_forwarding(&session_name, state, panes, clients, config, prev_frames).await;
            resize_session_panes(&session_name, cols, rows, state, panes, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneSplitHorizontal => {
            let new_pane_id = {
                let mut st = state.lock().await;
                let new_pane_id = st.next_pane_id();
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                // Eject to Custom mode on manual split
                if tab.layout_mode.is_automatic() {
                    tab.layout_mode = LayoutMode::Custom(CustomLayout);
                }
                let focused = tab.focused_pane;
                tab.layout.split_horizontal(focused, new_pane_id);
                tab.focused_pane = new_pane_id;
                tab.pane_order.push(new_pane_id);
                new_pane_id
            };
            spawn_pane(new_pane_id, cols, rows / 2, None, None, panes, config).await?;
            start_pty_forwarding(&session_name, state, panes, clients, config, prev_frames).await;
            resize_session_panes(&session_name, cols, rows, state, panes, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneClose => {
            let closed_pane = {
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
                if let Some(neighbor) =
                    layout::find_neighbor(&tab.layout, area, tab.focused_pane, direction, 0)
                {
                    tab.focused_pane = neighbor;
                }
            }
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::PaneStackAdd => {
            let new_pane_id = {
                let mut st = state.lock().await;
                let new_pane_id = st.next_pane_id();
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let focused = tab.focused_pane;
                tab.layout.add_to_stack(focused, new_pane_id);
                tab.focused_pane = new_pane_id;
                tab.pane_order.push(new_pane_id);
                new_pane_id
            };
            spawn_pane(new_pane_id, cols, rows, None, None, panes, config).await?;
            start_pty_forwarding(&session_name, state, panes, clients, config, prev_frames).await;
            resize_session_panes(&session_name, cols, rows, state, panes, config).await?;
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
                    tab.focused_pane = next;
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
                    tab.focused_pane = prev;
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
            resize_session_panes(&session_name, cols, rows, state, panes, config).await?;
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
            }
            resize_session_panes(&session_name, cols, rows, state, panes, config).await?;
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        RemuxCommand::SessionDetach => {
            handle_detach(client_id, clients).await;
        }
        RemuxCommand::SessionList => {
            handle_list_sessions(client_id, state, clients).await?;
        }
        RemuxCommand::SessionRename(new_name) => {
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
            let new_pane_id = {
                let mut st = state.lock().await;
                let new_pane_id = st.next_pane_id();
                let sess = match st.sessions.get_mut(&session_name) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let tab = match sess.tabs.get_mut(sess.active_tab) {
                    Some(t) => t,
                    None => return Ok(()),
                };
                tab.pane_order.push(new_pane_id);
                if tab.layout_mode.is_automatic() {
                    // Rebuild tree from layout mode
                    tab.layout = tab.layout_mode.build_tree(&tab.pane_order, new_pane_id);
                    tab.focused_pane = new_pane_id;
                } else {
                    // Custom mode: split at focused pane (default vertical)
                    let focused = tab.focused_pane;
                    tab.layout.split_vertical(focused, new_pane_id);
                    tab.focused_pane = new_pane_id;
                }
                new_pane_id
            };
            spawn_pane(new_pane_id, cols, rows, None, None, panes, config).await?;
            start_pty_forwarding(&session_name, state, panes, clients, config, prev_frames).await;
            resize_session_panes(&session_name, cols, rows, state, panes, config).await?;
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
                tab.layout_mode = tab.layout_mode.next();
                if tab.layout_mode.is_automatic() {
                    tab.layout = tab
                        .layout_mode
                        .build_tree(&tab.pane_order, tab.focused_pane);
                }
            }
            resize_session_panes(&session_name, cols, rows, state, panes, config).await?;
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
                if let LayoutMode::Master(ref mut master_layout) = tab.layout_mode {
                    if let Some(idx) = tab.pane_order.iter().position(|&id| id == tab.focused_pane)
                    {
                        master_layout.master_idx = idx;
                        tab.layout = tab
                            .layout_mode
                            .build_tree(&tab.pane_order, tab.focused_pane);
                    }
                }
                // No-op if not in Master mode
            }
            resize_session_panes(&session_name, cols, rows, state, panes, config).await?;
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
            start_pty_forwarding(&session, state, panes, clients, config, prev_frames).await;
            resize_session_panes(&session, cols, rows, state, panes, config).await?;
            broadcast_full_render(&session, state, panes, clients, config, prev_frames).await;
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
                        tab.focused_pane = pane_id;
                    }
                }
            }
            start_pty_forwarding(&session, state, panes, clients, config, prev_frames).await;
            resize_session_panes(&session, cols, rows, state, panes, config).await?;
            broadcast_full_render(&session, state, panes, clients, config, prev_frames).await;
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
                    {
                        let mut ps = panes.lock().await;
                        for pid in pane_ids {
                            ps.remove(&pid);
                        }
                    }
                    if session_deleted {
                        let mut cls = clients.lock().await;
                        for client in cls.values_mut() {
                            if client.session_name.as_deref() == Some(&target_session) {
                                client.session_name = None;
                                let _ = client.tx.send(ServerMessage::Event(
                                    SessionEvent::SessionDeleted(target_session.clone()),
                                ));
                            }
                        }
                    } else {
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
        _ => {
            log::debug!("unhandled command: {cmd:?}");
        }
    }

    // Persist state after every command that may have changed structure.
    save_if_enabled(state, panes, config).await;

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
        st.create_session(name, folder, border_style, layout_mode)?
    };
    spawn_pane(pane_id, cols, rows, None, None, panes, config).await?;

    let cls = clients.lock().await;
    if let Some(client) = cls.get(&client_id) {
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

async fn handle_list_session_tree(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let current_session = {
        let cls = clients.lock().await;
        cls.get(&client_id).and_then(|c| c.session_name.clone())
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

    let (folders, unfiled) =
        st.build_session_tree(current_session.as_deref(), &client_counts, &pane_names);

    if let Some(client) = cls.get(&client_id) {
        let _ = client
            .tx
            .send(ServerMessage::SessionTree { folders, unfiled });
    }
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
    {
        let mut ps = panes.lock().await;
        for pid in pane_ids {
            ps.remove(&pid);
        }
    }
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
) {
    let mut cls = clients.lock().await;
    cls.remove(&client_id);
}

async fn handle_request_scrollback(
    client_id: u64,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
) -> Result<()> {
    let (session_name, _cols, _rows) = {
        let cls = clients.lock().await;
        match cls.get(&client_id) {
            Some(c) => (c.session_name.clone(), c.cols, c.rows),
            None => return Ok(()),
        }
    };
    let session_name = match session_name {
        Some(s) => s,
        None => return Ok(()),
    };

    // Find the active pane for this client's session.
    let active_pane_id = {
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

#[allow(clippy::too_many_arguments)]
async fn handle_mouse_click(
    client_id: u64,
    x: u16,
    y: u16,
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
        // Clear any active selection on click.
        client.mouse_selection = None;
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
    let (_cells, _cx, _cy, _cv, _cs, _fpr, hit_regions, pane_rects) = build_composite(
        &session_name,
        cols,
        rows,
        &mode,
        state,
        panes,
        config,
        None,
        None,
        &config.compositor_theme(),
    )
    .await;

    let target = hit_test(x, y, &hit_regions, &pane_rects);

    match target {
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
                tab.focused_pane = pane_id;
                drop(st);
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
            tab.focused_pane = pane_id;
            drop(st);
            broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;
        }
        ClickTarget::None => {}
    }

    Ok(())
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

    // Build composite to get pane rects for coordinate mapping.
    update_auto_pane_names(&session_name, state, panes).await;
    let (_cells, _cx, _cy, _cv, _cs, _fpr, hit_regions, pane_rects) = build_composite(
        &session_name,
        cols,
        rows,
        &mode,
        state,
        panes,
        config,
        None,
        None,
        &config.compositor_theme(),
    )
    .await;

    // Find which pane the drag started in.
    let start_target = hit_test(start_x, start_y, &hit_regions, &pane_rects);
    let target_pane = match start_target {
        ClickTarget::Pane(id) => id,
        _ => return Ok(()),
    };

    // Find the pane's rect for coordinate mapping.
    let pane_rect = match pane_rects.iter().find(|(id, _)| *id == target_pane) {
        Some((_, r)) => *r,
        None => return Ok(()),
    };

    // Compute border offset based on style.
    let border_offset: u16 = match config.appearance.border_style {
        BorderStyle::ZellijStyle if pane_rect.width >= 3 && pane_rect.height >= 3 => 1,
        _ => 0,
    };
    let content_width = pane_rect.width.saturating_sub(border_offset * 2);
    let content_height = pane_rect.height.saturating_sub(border_offset * 2);

    // Map screen coordinates to pane-local coordinates (relative to content area).
    let local_start_x = start_x.saturating_sub(pane_rect.x + border_offset);
    let local_start_y = start_y.saturating_sub(pane_rect.y + border_offset);
    let local_end_x = end_x
        .saturating_sub(pane_rect.x + border_offset)
        .min(content_width.saturating_sub(1));
    let local_end_y = end_y
        .saturating_sub(pane_rect.y + border_offset)
        .min(content_height.saturating_sub(1));

    // Always update selection state so highlighting renders during drag.
    {
        let mut cls = clients.lock().await;
        if let Some(client) = cls.get_mut(&client_id) {
            client.mouse_selection = Some(MouseSelection {
                pane_id: target_pane,
                start: (local_start_x, local_start_y),
                end: (local_end_x, local_end_y),
            });
        }
    }

    if is_final {
        // Mouse button released -- decide based on mouse_auto_yank config.
        if config.general.mouse_auto_yank {
            // Extract text from the pane's screen.
            let selected_text = {
                let ps = panes.lock().await;
                if let Some(pane_data) = ps.get(&target_pane) {
                    extract_selection_text(
                        &pane_data.screen,
                        local_start_x,
                        local_start_y,
                        local_end_x,
                        local_end_y,
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
    }

    // Trigger re-render to show/update/clear selection highlighting.
    broadcast_full_render(&session_name, state, panes, clients, config, prev_frames).await;

    Ok(())
}

/// Extract text from a pane's screen buffer within the given pane-local
/// coordinate range.
fn extract_selection_text(
    screen: &Screen,
    start_x: u16,
    start_y: u16,
    end_x: u16,
    end_y: u16,
) -> String {
    // Normalize so start <= end.
    let (sy, sx, ey, ex) = if (start_y, start_x) <= (end_y, end_x) {
        (start_y, start_x, end_y, end_x)
    } else {
        (end_y, end_x, start_y, start_x)
    };

    let mut result = String::new();
    for row in sy..=ey {
        let r = row as usize;
        if r >= screen.grid.len() {
            break;
        }
        let row_data = &screen.grid[r];
        let col_start = if row == sy { sx as usize } else { 0 };
        let col_end = if row == ey {
            (ex as usize + 1).min(row_data.len())
        } else {
            row_data.len()
        };

        let text: String = row_data[col_start..col_end].iter().map(|c| c.c).collect();
        if row > sy {
            result.push('\n');
        }
        result.push_str(text.trim_end());
    }
    result
}

// ---------------------------------------------------------------------------
// Pane management helpers
// ---------------------------------------------------------------------------

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
        },
    );
    Ok(())
}

async fn resize_session_panes(
    session_name: &str,
    cols: u16,
    rows: u16,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    _config: &Arc<Config>,
) -> Result<()> {
    let rects = {
        let st = state.lock().await;
        let sess = match st.sessions.get(session_name) {
            Some(s) => s,
            None => return Ok(()),
        };
        let tab = match sess.tabs.get(sess.active_tab) {
            Some(t) => t,
            None => return Ok(()),
        };
        let content_rows = rows.saturating_sub(1);
        let area = Rect {
            x: 0,
            y: 0,
            width: cols,
            height: content_rows,
        };
        layout::compute_layout(&tab.layout, area, 0)
    };

    let mut ps = panes.lock().await;
    for (pane_id, rect) in rects {
        if let Some(pane_data) = ps.get_mut(&pane_id) {
            let inner_cols = rect.width.max(1);
            let inner_rows = rect.height.max(1);
            let _ = pane_data.pty.resize(inner_cols, inner_rows);
            pane_data.screen.resize(inner_cols, inner_rows);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn send_full_render_to_client(
    client_id: u64,
    session_name: &str,
    cols: u16,
    rows: u16,
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    clients: &Arc<Mutex<HashMap<u64, ClientConnection>>>,
    config: &Arc<Config>,
    prev_frames: &PrevFrameCache,
) {
    let (mode, selection, client_search_info) = {
        let cls = clients.lock().await;
        let client = cls.get(&client_id);
        let mode = client
            .map(|c| c.mode.clone())
            .unwrap_or_else(|| "INSERT".to_string());
        let selection = client.and_then(|c| c.mouse_selection.clone());
        let si = client.and_then(|c| c.search_info);
        (mode, selection, si)
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
        &config.compositor_theme(),
    )
    .await;
    {
        let mut pf = prev_frames.lock().await;
        pf.insert(session_name.to_string(), cells.clone());
    }
    let cls = clients.lock().await;
    if let Some(client) = cls.get(&client_id) {
        let _ = client.tx.send(ServerMessage::FullRender {
            cells,
            cursor_x,
            cursor_y,
            cursor_visible,
            cursor_style,
            focused_pane_rect,
        });
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
    let (cols, rows, mode, selection, si) = {
        let cls = clients.lock().await;
        let attached: Vec<_> = cls
            .values()
            .filter(|c| c.session_name.as_deref() == Some(session_name))
            .collect();
        if attached.is_empty() {
            return;
        }
        let cols = attached.iter().map(|c| c.cols).min().unwrap_or(80);
        let rows = attached.iter().map(|c| c.rows).min().unwrap_or(24);
        // Use the mode and selection from the first attached client.
        let first = attached.first();
        let mode = first
            .map(|c| c.mode.clone())
            .unwrap_or_else(|| "INSERT".to_string());
        let selection = first.and_then(|c| c.mouse_selection.clone());
        let si = first.and_then(|c| c.search_info);
        (cols, rows, mode, selection, si)
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
        &config.compositor_theme(),
    )
    .await;

    let msg = {
        let prev_frames_map = prev_frames.lock().await;
        let prev = prev_frames_map.get(session_name);
        if let Some(prev_cells) = prev {
            let changes = compute_diff(prev_cells, &cells);
            if changes.len() > (cols as usize * rows as usize / 2) {
                ServerMessage::FullRender {
                    cells: cells.clone(),
                    cursor_x,
                    cursor_y,
                    cursor_visible,
                    cursor_style,
                    focused_pane_rect,
                }
            } else {
                ServerMessage::RenderDiff {
                    changes,
                    cursor_x,
                    cursor_y,
                    cursor_visible,
                    cursor_style,
                    focused_pane_rect,
                }
            }
        } else {
            ServerMessage::FullRender {
                cells: cells.clone(),
                cursor_x,
                cursor_y,
                cursor_visible,
                cursor_style,
                focused_pane_rect,
            }
        }
    };

    {
        let mut pf = prev_frames.lock().await;
        pf.insert(session_name.to_string(), cells);
    }

    let cls = clients.lock().await;
    for client in cls.values() {
        if client.session_name.as_deref() == Some(session_name) {
            let _ = client.tx.send(msg.clone());
        }
    }
}

/// Update display names for panes that don't have a custom name by
/// reading the process name from `/proc/<pid>/comm`.
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
                layout::set_pane_name(&mut tab.layout, pane_id, &name);
            }
        }
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
    let pane_rects = layout::compute_layout(&tab.layout, area, 0);
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
            .map(|(i, t)| (t.name.clone(), i == sess.active_tab))
            .collect(),
        layout_mode: tab.layout_mode.name().to_string(),
        search_info,
    };

    let (cells, hit_regions) = composite(
        &tab.layout,
        &pane_screens,
        area,
        &sess.border_style,
        &status_info,
        cols,
        rows,
        0,
        tab.focused_pane,
        selection,
        compositor_theme,
    );

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
                    if rect.width >= 3 && rect.height >= 3 {
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

    // Build the focused pane rect for the client (content area, excluding borders).
    let focused_pane_rect =
        focused_rect_and_offsets.map(|(rect, x_off, y_off, x_off_end, y_off_end)| PaneRect {
            x: rect.x + x_off,
            y: rect.y + y_off,
            width: rect.width.saturating_sub(x_off + x_off_end),
            height: rect.height.saturating_sub(y_off + y_off_end),
        });

    let (cursor_x, cursor_y, cursor_visible, cursor_style) = if let Some(rc) = rename_cursor {
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

    (
        cells,
        cursor_x,
        cursor_y,
        cursor_visible,
        cursor_style,
        focused_pane_rect,
        hit_regions,
        pane_rects,
    )
}

fn compute_diff(prev: &[Vec<RenderCell>], curr: &[Vec<RenderCell>]) -> Vec<CellChange> {
    let mut changes = Vec::new();
    for (y, row) in curr.iter().enumerate() {
        let prev_row = prev.get(y);
        for (x, cell) in row.iter().enumerate() {
            let prev_cell = prev_row.and_then(|r| r.get(x));
            if prev_cell != Some(cell) {
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
        /// Last pane/tab in the session was closed; switch clients to another
        /// session.
        SwitchSession(String),
        /// Last session was removed; disconnect affected clients.
        Disconnect,
        /// Nothing to do (pane not found, etc.).
        NoBroadcast,
    }

    let action = {
        let mut st = state.lock().await;
        let sess = match st.sessions.get_mut(session_name) {
            Some(s) => s,
            None => return,
        };
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

        if let Some(nf) = new_focus {
            tab.focused_pane = nf;
            // If in automatic mode, rebuild the tree
            if tab.layout_mode.is_automatic() {
                tab.layout = tab.layout_mode.build_tree(&tab.pane_order, nf);
            }
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
                st.sessions.remove(&session_name_owned);

                // Find the next available session to switch to.
                let next_session = st.sessions.keys().next().cloned();
                match next_session {
                    Some(next) => CloseAction::SwitchSession(next),
                    None => CloseAction::Disconnect,
                }
            }
        }
    };
    {
        let mut ps = panes.lock().await;
        ps.remove(&pane_id);
    }
    match action {
        CloseAction::Broadcast => {
            broadcast_full_render(session_name, state, panes, clients, config, prev_frames).await;
        }
        CloseAction::SwitchSession(ref next) => {
            // Switch all clients that were on this session to the next one.
            {
                let mut cls = clients.lock().await;
                for c in cls.values_mut() {
                    if c.session_name.as_deref() == Some(session_name) {
                        c.session_name = Some(next.clone());
                    }
                }
            }
            broadcast_full_render(next, state, panes, clients, config, prev_frames).await;
        }
        CloseAction::Disconnect => {
            // No sessions left -- notify affected clients so they disconnect.
            let mut cls = clients.lock().await;
            for c in cls.values_mut() {
                if c.session_name.as_deref() == Some(session_name) {
                    c.session_name = None;
                    let _ = c.tx.send(ServerMessage::Event(SessionEvent::SessionDeleted(
                        session_name.to_string(),
                    )));
                }
            }
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
        layout::all_pane_ids(&tab.layout)
    };

    let session_name = session_name.to_string();

    for pane_id in pane_ids {
        let state = Arc::clone(state);
        let panes = Arc::clone(panes);
        let clients = Arc::clone(clients);
        let config = Arc::clone(config);
        let prev_frames = Arc::clone(prev_frames);
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
                    Some(Ok(data)) => {
                        let responses = {
                            let mut ps = panes.lock().await;
                            if let Some(pane_data) = ps.get_mut(&pane_id) {
                                pane_data.screen.process_output(&data);
                                pane_data.screen.take_responses()
                            } else {
                                Vec::new()
                            }
                        };
                        // Write any pending responses (e.g., DSR replies) back to the PTY.
                        if !responses.is_empty() {
                            let ps = panes.lock().await;
                            if let Some(pane_data) = ps.get(&pane_id) {
                                for resp in &responses {
                                    let _ = pane_data.pty.write_input(resp);
                                }
                            }
                        }
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
                        save_if_enabled(&state, &panes, &config).await;
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

/// Save the server state to disk if automatic_restore is enabled.
///
/// This captures the current working directory of every pane and writes
/// the full server state to the persistence file. It is called after
/// every structural change (session/tab/pane create/close/rename).
async fn save_if_enabled(
    state: &Arc<Mutex<ServerState>>,
    panes: &Arc<Mutex<HashMap<PaneId, PaneData>>>,
    config: &Arc<Config>,
) {
    if !config.general.automatic_restore {
        return;
    }
    let st = state.lock().await;
    let ps = panes.lock().await;
    let mut pane_cwds = HashMap::new();
    for (&pane_id, pane_data) in ps.iter() {
        if let Some(cwd) = crate::server::persistence::get_pane_cwd(pane_data.pty.child_pid) {
            pane_cwds.insert(pane_id, cwd);
        }
    }
    if let Ok(persisted) = crate::server::persistence::PersistedState::from_server(&st, &pane_cwds)
    {
        if let Err(e) = crate::server::persistence::save_state(&persisted) {
            log::error!("failed to save state: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// State restore on startup
// ---------------------------------------------------------------------------

/// Restore server state from a persisted snapshot.
///
/// Replaces the server's state with the deserialized state, spawns new PTYs
/// for every pane (using saved CWDs), and starts PTY forwarding. Panes are
/// initially sized 80x24; they will be resized when the first client attaches
/// and sends a `Resize` message.
async fn restore_state(
    server: &RemuxServer,
    persisted: crate::server::persistence::PersistedState,
) -> Result<()> {
    let mut restored_state = persisted.state;
    restored_state.ensure_id_counters();

    // Collect all (session_name, pane_id, cwd) triples for PTY spawning.
    let mut pane_spawns: Vec<(String, PaneId, Option<String>)> = Vec::new();
    for (session_name, session) in &restored_state.sessions {
        for tab in &session.tabs {
            let pane_ids = layout::all_pane_ids(&tab.layout);
            for pane_id in pane_ids {
                let cwd = persisted.pane_cwds.get(&pane_id).cloned();
                pane_spawns.push((session_name.clone(), pane_id, cwd));
            }
        }
    }

    // Replace the server state.
    {
        let mut st = server.state.lock().await;
        *st = restored_state;
    }

    // Spawn PTYs for all restored panes.
    let default_cols: u16 = 80;
    let default_rows: u16 = 24;

    for (_session_name, pane_id, cwd) in &pane_spawns {
        let cwd_path = cwd.as_deref().map(std::path::Path::new);
        if let Err(e) = spawn_pane(
            *pane_id,
            default_cols,
            default_rows,
            None,
            cwd_path,
            &server.panes,
            &server.config,
        )
        .await
        {
            log::warn!("failed to spawn PTY for restored pane {pane_id}: {e}");
        }
    }

    // Start PTY forwarding for all sessions and all tabs.
    let session_names: Vec<String> = {
        let st = server.state.lock().await;
        st.sessions.keys().cloned().collect()
    };
    for session_name in &session_names {
        let all_pane_ids: Vec<PaneId> = {
            let st = server.state.lock().await;
            if let Some(sess) = st.sessions.get(session_name) {
                sess.tabs
                    .iter()
                    .flat_map(|t| layout::all_pane_ids(&t.layout))
                    .collect()
            } else {
                Vec::new()
            }
        };

        let sn = session_name.clone();
        for pane_id in all_pane_ids {
            let state = Arc::clone(&server.state);
            let panes = Arc::clone(&server.panes);
            let clients = Arc::clone(&server.clients);
            let config = Arc::clone(&server.config);
            let prev_frames = Arc::clone(&server.prev_frames);
            let session_name = sn.clone();

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
                            let responses = {
                                let mut ps = panes.lock().await;
                                if let Some(pane_data) = ps.get_mut(&pane_id) {
                                    pane_data.screen.process_output(&data);
                                    pane_data.screen.take_responses()
                                } else {
                                    Vec::new()
                                }
                            };
                            if !responses.is_empty() {
                                let ps = panes.lock().await;
                                if let Some(pane_data) = ps.get(&pane_id) {
                                    for resp in &responses {
                                        let _ = pane_data.pty.write_input(resp);
                                    }
                                }
                            }
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
                            save_if_enabled(&state, &panes, &config).await;
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
    }

    log::info!(
        "restored {} sessions with {} panes",
        session_names.len(),
        pane_spawns.len()
    );
    Ok(())
}
