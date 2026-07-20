// Allow dead code during early development -- modules are defined but not yet
// wired into the binary entry point.
#![allow(dead_code)]

mod client;
mod config;
mod protocol;
mod screen;
mod server;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{KeyEventKind, MouseButton, MouseEventKind};
use futures::StreamExt;

use crate::client::editor::copy_to_clipboard;
use crate::client::input::{
    FolderSelectOverlay, InputAction, InputHandler, Mode, RenameTarget, SessionSwitchOverlay,
};
use crate::client::registry::{ConnId, ConnectionManager, Incoming, RemoteState};
use crate::client::renderer::Renderer;
use crate::client::session_manager::{NodeType, SessionManagerAction};
use crate::client::terminal::{restore_terminal, setup_terminal, RemuxClient};
use crate::client::whichkey::WhichKeyPopup;
use crate::config::{Config, RemoteConfig};
use crate::protocol::{ClientMessage, RemuxCommand, ServerMessage};
use crate::server::daemon::{socket_path, RemuxServer};

/// Data captured while computing search matches, used to transition from
/// Search into Visual mode positioned at the current match.
struct SearchToVisual {
    matches: Vec<(usize, usize)>,
    current_match: usize,
    total_lines: usize,
    match_line: usize,
    match_col: usize,
}

#[derive(Parser)]
#[command(name = "remux", version, about = "A terminal multiplexer")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new session
    New {
        /// Session name
        #[arg(short, long)]
        session: String,

        /// Working directory for the session
        #[arg(short, long)]
        folder: Option<String>,
    },

    /// Attach to an existing session
    Attach {
        /// Session name to attach to
        name: String,
    },

    /// List all sessions
    Ls,

    /// Kill a session
    Kill {
        /// Session name to kill
        name: String,
    },

    /// Attach to a session on a remote machine over SSH
    AttachRemote {
        /// SSH destination (e.g. user@host); relies on ~/.ssh/config
        dest: String,
        /// Session name on the remote to attach to
        name: String,
        /// Path to the remux binary on the remote
        #[arg(long, default_value = "remux")]
        remux_path: String,
    },

    /// Internal: run the server (not for direct use)
    #[command(hide = true)]
    Server,

    /// Internal: relay stdio to the local server socket (used over SSH)
    #[command(hide = true)]
    Relay,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Determine log file based on command: server.log vs relay.log vs client.log.
    let log_filename = match cli.command {
        Some(Commands::Server) => "server.log",
        Some(Commands::Relay) => "relay.log",
        _ => "client.log",
    };
    let log_dir = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("remux");
    std::fs::create_dir_all(&log_dir).expect("failed to create log directory");
    let log_path = log_dir.join(log_filename);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("failed to open log file");
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Debug)
        .format_timestamp_millis()
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .init();

    let role = match cli.command {
        Some(Commands::Server) => "server",
        Some(Commands::Relay) => "relay",
        _ => "client",
    };
    log::info!("remux starting as {role}, log={}", log_path.display());

    match cli.command {
        Some(Commands::Server) => {
            log::debug!("launching server daemon");
            let config = Config::load()?;
            RemuxServer::run(config).await?;
        }
        None => {
            log::debug!("no subcommand: default attach/create flow");
            // Try to connect to existing server, start if needed
            ensure_server_running().await?;
            let mut client = RemuxClient::connect().await?;
            let config = Config::load()?;

            // Create a default session or attach to existing one
            // First, ask the server for existing sessions
            client.send(ClientMessage::ListSessions).await?;
            let response = client
                .recv()
                .await?
                .context("server disconnected unexpectedly")?;

            let attached_session = match response {
                ServerMessage::SessionList { sessions } => {
                    if sessions.is_empty() {
                        // No sessions exist, create a default one
                        client
                            .send(ClientMessage::CreateSession {
                                name: "main".to_string(),
                                folder: None,
                            })
                            .await?;
                        // Wait for session creation event
                        let _ = client.recv().await?;
                        client
                            .send(ClientMessage::Attach {
                                session_name: "main".to_string(),
                            })
                            .await?;
                        "main".to_string()
                    } else {
                        // Attach to the first session
                        let session_name = sessions[0].name.clone();
                        client
                            .send(ClientMessage::Attach {
                                session_name: session_name.clone(),
                            })
                            .await?;
                        session_name
                    }
                }
                _ => {
                    anyhow::bail!("unexpected response from server");
                }
            };

            let mut mgr = ConnectionManager::new(client, &config.remotes);
            client_event_loop(&mut mgr, &config, Some(attached_session)).await?;
        }
        Some(Commands::New { session, folder }) => {
            log::debug!("cmd: new session={session:?} folder={folder:?}");
            ensure_server_running().await?;
            let mut client = RemuxClient::connect().await?;
            let config = Config::load()?;

            client
                .send(ClientMessage::CreateSession {
                    name: session.clone(),
                    folder,
                })
                .await?;
            // Wait for creation event
            let _ = client.recv().await?;
            client
                .send(ClientMessage::Attach {
                    session_name: session.clone(),
                })
                .await?;

            let mut mgr = ConnectionManager::new(client, &config.remotes);
            client_event_loop(&mut mgr, &config, Some(session)).await?;
        }
        Some(Commands::Attach { name }) => {
            log::debug!("cmd: attach session={name:?}");
            ensure_server_running().await?;
            let mut client = RemuxClient::connect().await?;
            let config = Config::load()?;

            client
                .send(ClientMessage::Attach {
                    session_name: name.clone(),
                })
                .await?;

            let mut mgr = ConnectionManager::new(client, &config.remotes);
            client_event_loop(&mut mgr, &config, Some(name)).await?;
        }
        Some(Commands::Ls) => {
            log::debug!("cmd: list sessions");
            if !socket_path().exists() {
                println!("No server running. No sessions.");
                return Ok(());
            }
            let mut client = RemuxClient::connect().await?;
            client.send(ClientMessage::ListSessions).await?;
            let response = client
                .recv()
                .await?
                .context("server disconnected unexpectedly")?;

            match response {
                ServerMessage::SessionList { sessions } => {
                    if sessions.is_empty() {
                        println!("No sessions.");
                    } else {
                        println!(
                            "{:<20} {:<15} {:<6} {:<8}",
                            "NAME", "FOLDER", "TABS", "CLIENTS"
                        );
                        for s in &sessions {
                            println!(
                                "{:<20} {:<15} {:<6} {:<8}",
                                s.name,
                                s.folder.as_deref().unwrap_or("-"),
                                s.tab_count,
                                s.client_count,
                            );
                        }
                    }
                }
                ServerMessage::Error { message } => {
                    eprintln!("Error: {}", message);
                }
                _ => {
                    eprintln!("Unexpected response from server.");
                }
            }
        }
        Some(Commands::AttachRemote {
            dest,
            name,
            remux_path,
        }) => {
            log::debug!(
                "cmd: attach-remote dest={dest:?} session={name:?} remux_path={remux_path:?}"
            );
            // The server we want is the remote one; the relay starts it there,
            // so we deliberately do NOT call ensure_server_running() locally.
            let mut client = RemuxClient::connect_ssh(&dest, None, None, &[], &remux_path).await?;
            let config = Config::load()?;

            client
                .send(ClientMessage::Attach { session_name: name })
                .await?;

            // Wrap in a manager with a synthetic remote foreground so the loop's
            // multi-connection routing applies uniformly; no `[remotes]` involved.
            let mut mgr = ConnectionManager::new_foreground_remote(&dest, client);
            client_event_loop(&mut mgr, &config, None).await?;
        }
        Some(Commands::Relay) => {
            log::info!("cmd: relay starting");
            // Make sure this machine's own server is up, then become a dumb
            // transparent byte pump between our stdio and the local socket.
            // We do NOT use RemuxClient and perform NO handshake here: the real
            // handshake flows through end-to-end between the far client and this
            // machine's server.
            ensure_server_running().await?;
            let sock = tokio::net::UnixStream::connect(socket_path())
                .await
                .context("relay: connecting to local server socket")?;
            let (mut srd, mut swr) = sock.into_split();
            let mut stdin = tokio::io::stdin();
            let mut stdout = tokio::io::stdout();

            // Exit as soon as EITHER direction hits EOF.
            tokio::select! {
                r = tokio::io::copy(&mut stdin, &mut swr) => {
                    log::debug!("relay: stdin->socket ended: {:?}", r);
                }
                r = tokio::io::copy(&mut srd, &mut stdout) => {
                    log::debug!("relay: socket->stdout ended: {:?}", r);
                }
            }
            log::info!("cmd: relay exiting");
        }
        Some(Commands::Kill { name }) => {
            log::debug!("cmd: kill session={name:?}");
            if !socket_path().exists() {
                eprintln!("No server running.");
                return Ok(());
            }
            let mut client = RemuxClient::connect().await?;
            client
                .send(ClientMessage::KillSession { name: name.clone() })
                .await?;

            // Wait for confirmation
            match client.recv().await? {
                Some(ServerMessage::Event(crate::protocol::SessionEvent::SessionDeleted(
                    deleted,
                ))) => {
                    println!("Killed session '{}'.", deleted);
                }
                Some(ServerMessage::Error { message }) => {
                    eprintln!("Error: {}", message);
                }
                _ => {
                    println!("Killed session '{}'.", name);
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

/// Ensure a server is running, starting one in the background if needed.
async fn ensure_server_running() -> Result<()> {
    let sock = socket_path();
    log::debug!(
        "ensure_server_running: checking socket at {}",
        sock.display()
    );
    if sock.exists() {
        // Try connecting to verify the socket is live
        match RemuxClient::connect().await {
            Ok(_) => {
                log::debug!("ensure_server_running: server already running");
                return Ok(());
            }
            Err(_) => {
                // Stale socket file, remove it
                log::debug!("ensure_server_running: stale socket detected, removing");
                let _ = std::fs::remove_file(&sock);
            }
        }
    }

    let exe = std::env::current_exe().context("finding current executable")?;
    log::debug!(
        "ensure_server_running: spawning server from {}",
        exe.display()
    );
    std::process::Command::new(exe)
        .arg("server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning server process")?;

    // Wait for the socket to appear
    for i in 0..50 {
        if sock.exists() {
            log::debug!("ensure_server_running: socket ready after {} iterations", i);
            // Give the server a moment to start accepting connections
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    log::debug!("ensure_server_running: timed out waiting for socket");
    anyhow::bail!("timed out waiting for server to start")
}

// ---------------------------------------------------------------------------
// Client event loop
// ---------------------------------------------------------------------------

/// Run the client event loop with terminal setup/restore.
///
/// `initial_local_session` is the local session attached before the loop
/// started (if any); it seeds `last_local_session` so a foreground-remote drop
/// can fall back to it.
async fn client_event_loop(
    mgr: &mut ConnectionManager,
    config: &Config,
    initial_local_session: Option<String>,
) -> Result<()> {
    log::debug!("client_event_loop: setting up terminal");
    setup_terminal()?;

    let result = run_client_loop(mgr, config, initial_local_session).await;

    log::debug!(
        "client_event_loop: restoring terminal, result={}",
        result.is_ok()
    );
    restore_terminal()?;
    result
}

/// Hand the foreground over to `target`: connect it if it is a not-yet-connected
/// remote, detach the current foreground so it stops streaming, make `target`
/// the foreground, and resize it to the current terminal.
///
/// No-op (and crucially no `Detach`) when `target` is already the foreground, so
/// same-server switches behave exactly as before. The detach-before-attach step
/// is mandatory on a cross-server handoff: skipping it leaves the old server
/// streaming `RenderDiff`s into a socket nobody drains (backpressure).
async fn switch_to_server(
    mgr: &mut ConnectionManager,
    target: &ConnId,
    cols: u16,
    rows: u16,
) -> Result<()> {
    if mgr.is_foreground(target) {
        return Ok(());
    }
    // Ensure a remote target is connected before handing off (it usually already
    // is, from expanding its node, but be safe).
    if let ConnId::Remote(name) = target {
        if mgr.remote_state(name) != RemoteState::Connected {
            mgr.connect_remote(name).await?;
        }
    }
    let _ = mgr.send_foreground(ClientMessage::Detach).await;
    mgr.set_foreground(target.clone());
    mgr.send(target, ClientMessage::Resize { cols, rows })
        .await?;
    Ok(())
}

/// Record a foreground switch to `(server, session)` for the "last session"
/// toggle (`Alt-o`). When the new target differs from the current attachment,
/// the current attachment becomes the previous one and the new target becomes
/// current. Switching to the session that is already current is ignored, so the
/// toggle never records a self-switch that would strand `previous_attached`.
fn record_switch(
    current: &mut Option<(ConnId, String)>,
    previous: &mut Option<(ConnId, String)>,
    server: ConnId,
    session: String,
) {
    let new_attached = (server, session);
    if current.as_ref() == Some(&new_attached) {
        // Same session as the current foreground: nothing to toggle.
        return;
    }
    *previous = current.take();
    *current = Some(new_attached);
}

/// The inner client event loop.
async fn run_client_loop(
    mgr: &mut ConnectionManager,
    config: &Config,
    initial_local_session: Option<String>,
) -> Result<()> {
    use crossterm::event::EventStream;

    config.validate();

    let mut event_stream = EventStream::new();
    let keybindings = config.keybinding_tree();
    let leader_key = config.leader_key();
    log::debug!("run_client_loop: leader_key={:?}", leader_key);
    let shortcut_bindings = config.shortcut_bindings();
    let mut input = InputHandler::new(keybindings, leader_key, shortcut_bindings);
    let (cols, rows) = crossterm::terminal::size()?;
    log::debug!("run_client_loop: terminal size={}x{}", cols, rows);
    let mut renderer = Renderer::new(cols, rows);
    let mut whichkey = WhichKeyPopup::new();
    let mut theme = config.theme();
    let mut which_key_position = config.appearance.which_key_position.clone();

    // Spawn the config-file watcher for live hot-reload. This is best-effort:
    // if it fails to start we log and continue without hot-reload rather than
    // failing the client. We keep a spare sender (`_cfg_keepalive`) alive for
    // the loop's duration so `cfg_rx.recv()` stays Pending (never returns
    // `None`) even if the watcher never starts or its handle is dropped —
    // otherwise the select! branch below would busy-spin. `_cfg_watch` is bound
    // so the watcher isn't dropped for the loop's lifetime.
    let (cfg_tx, mut cfg_rx) = tokio::sync::mpsc::unbounded_channel::<Config>();
    let _cfg_keepalive = cfg_tx.clone();
    let _cfg_watch = match crate::config::watcher::watch_config(cfg_tx) {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::warn!("client: config watcher failed to start: {e:#}");
            None
        }
    };
    // Last known focused pane rect from the server, and cursor position.
    let mut focused_pane_rect: Option<crate::protocol::PaneRect> = None;
    let mut last_cursor_x: u16 = 0;
    let mut last_cursor_y: u16 = 0;
    // Last known hardware cursor visibility from a server render frame. Used to
    // restore the real cursor when tearing down a visual/search overlay (the
    // overlay clear hides it and no server frame may follow to bring it back).
    let mut last_cursor_visible: bool = true;

    // Scroll offset for the focused pane (0 = live view, >0 = scrolled back).
    // Used by visual mode and search. Normal mode scrolling uses server-owned offset.
    let mut scroll_offset: usize = 0;
    // The true server-owned viewport top (absolute index of the first visible
    // scrollback line). Updated ONLY from server render frames, so it stays a
    // stable coordinate for drawing search-match highlights even when
    // `scroll_offset` is transiently repurposed by in-view visual moves.
    let mut viewport_top: usize = 0;
    // Whether the client is currently scrolled back (server owns the actual offset).
    let mut is_scrolled: bool = false;
    // Baseline for computing VisualScroll deltas, in VisualState's own units
    // (lines-from-bottom). Re-synced to `vs.scroll_offset` at every point the
    // visual view moves without a VisualScroll delta being emitted: every visual
    // entry (keybinding, palette command, search landing) and mouse-wheel scroll.
    // The delta sent to the server is the CHANGE in this value, so an in-view
    // cursor move (which leaves `vs.scroll_offset` unchanged) yields delta 0.
    let mut last_visual_scroll: usize = 0;

    // The last local session we attached to; the fallback target if a
    // foreground-remote connection drops. Seeded from the pre-loop attach.
    let mut last_local_session: Option<String> = initial_local_session;

    // Track the current and previously-attached (server, session) for the
    // "last session" toggle (Alt-o). Seeded with the initial local session as
    // `(ConnId::Local, name)` when known; `previous` starts empty so the first
    // Alt-o before any switch is a no-op.
    let mut current_attached: Option<(ConnId, String)> = last_local_session
        .as_ref()
        .map(|name| (ConnId::Local, name.clone()));
    let mut previous_attached: Option<(ConnId, String)> = None;

    // Mouse drag state for coalescing drag events (~60fps throttle).
    let mut drag_start: Option<(u16, u16)> = None;
    let mut last_drag_send: Instant = Instant::now();
    /// Minimum interval between drag event sends (~16ms = ~60fps).
    const DRAG_THROTTLE: Duration = Duration::from_millis(16);

    // Tell server our terminal size
    log::debug!("run_client_loop: sending initial resize {}x{}", cols, rows);
    mgr.send_foreground(ClientMessage::Resize { cols, rows })
        .await?;

    loop {
        tokio::select! {
            // Keyboard events
            event = event_stream.next() => {
                match event {
                    Some(Ok(crossterm::event::Event::Key(key)))
                        if key.kind == KeyEventKind::Press =>
                    {
                        let was_renaming = input.rename_overlay.is_some();
                        let was_in_palette = input.command_palette.is_some();
                        let action = input.handle_key(key);

                        // If rename popup was dismissed, clear overlay
                        if was_renaming && input.rename_overlay.is_none() && !matches!(action, InputAction::RenameUpdate(_)) {
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.clear_overlay(c, r)?;
                            renderer.flush()?;
                        }
                        match action {
                            InputAction::SendToPty(data) => {
                                log::debug!("input: SendToPty {} bytes", data.len());
                                // Reset scroll when user types (sends PTY input)
                                if is_scrolled {
                                    scroll_offset = 0;
                                    is_scrolled = false;
                                    mgr.send_foreground(ClientMessage::ScrollReset).await?;
                                }
                                mgr.send_foreground(ClientMessage::Input { data }).await?;
                            }
                            InputAction::Execute(cmd) => {
                                log::debug!("input: Execute cmd={:?}", cmd);
                                // Hide which-key popup when executing a command
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // Clear command palette overlay if it was just closed.
                                if was_in_palette && input.command_palette.is_none() {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_command_palette_overlay(c, r)?;
                                }
                                renderer.flush()?;
                                if matches!(cmd, RemuxCommand::SessionDetach) {
                                    return Ok(());
                                }
                                // Handle SendKey: forward raw bytes to PTY.
                                if let RemuxCommand::SendKey(ref bytes) = cmd {
                                    mgr.send_foreground(ClientMessage::Input { data: bytes.clone() }).await?;
                                } else {
                                    mgr.send_foreground(ClientMessage::Command(cmd)).await?;
                                }
                                // Notify server of current mode if it changed.
                                let mode_str = match input.mode {
                                    Mode::Normal => "NORMAL",
                                    Mode::Command => "COMMAND",
                                    Mode::Visual => "VISUAL",
                                    Mode::CommandPalette => "PALETTE",
                                    Mode::Search => "SEARCH",
                                    Mode::SessionManager => "SESSION_MANAGER",
                                };
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: mode_str.to_string(),
                                    })
                                    .await?;
                                // A palette command may have just entered Visual mode
                                // (e.g. `:visual`). Baseline the delta tracker to the
                                // fresh state so a stale value from a prior visual
                                // session can't produce a bogus first-move scroll.
                                if input.mode == Mode::Visual {
                                    if let Some(ref vs) = input.visual_state {
                                        last_visual_scroll = vs.scroll_offset;
                                    }
                                }
                            }
                            InputAction::ExecuteChain(cmds) => {
                                log::debug!("input: ExecuteChain count={} cmds={:?}", cmds.len(), cmds);
                                // Hide which-key popup when executing commands
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                    renderer.flush()?;
                                }
                                for cmd in cmds {
                                    if matches!(cmd, RemuxCommand::SessionDetach) {
                                        return Ok(());
                                    }
                                    if let RemuxCommand::SendKey(ref bytes) = cmd {
                                        mgr.send_foreground(ClientMessage::Input { data: bytes.clone() }).await?;
                                    } else {
                                        mgr.send_foreground(ClientMessage::Command(cmd)).await?;
                                    }
                                }
                                // Notify server of current mode.
                                let mode_str = match input.mode {
                                    Mode::Normal => "NORMAL",
                                    Mode::Command => "COMMAND",
                                    Mode::Visual => "VISUAL",
                                    Mode::CommandPalette => "PALETTE",
                                    Mode::Search => "SEARCH",
                                    Mode::SessionManager => "SESSION_MANAGER",
                                };
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: mode_str.to_string(),
                                    })
                                    .await?;
                                // A chained command may have just entered Visual mode.
                                // Baseline the delta tracker to the fresh state (see the
                                // Execute arm above).
                                if input.mode == Mode::Visual {
                                    if let Some(ref vs) = input.visual_state {
                                        last_visual_scroll = vs.scroll_offset;
                                    }
                                }
                            }
                            InputAction::ModeChanged(mode) => {
                                log::debug!("input: ModeChanged to {:?}", mode);
                                let mode_str = match mode {
                                    Mode::Normal => "NORMAL",
                                    Mode::Command => "COMMAND",
                                    Mode::Visual => "VISUAL",
                                    Mode::CommandPalette => "PALETTE",
                                    Mode::Search => "SEARCH",
                                    Mode::SessionManager => "SESSION_MANAGER",
                                };
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: mode_str.to_string(),
                                    })
                                    .await?;
                                // Reset scroll offset when returning to normal mode.
                                if mode == Mode::Normal && (scroll_offset > 0 || is_scrolled) {
                                    log::debug!("input: resetting scroll on mode change, old offset={}", scroll_offset);
                                    scroll_offset = 0;
                                    is_scrolled = false;
                                    mgr.send_foreground(ClientMessage::ScrollReset).await?;
                                }
                                // Returning to Normal: erase any lingering
                                // search-match highlight / visual overlay
                                // (mirrors SearchCancel). Not gated on
                                // scroll/whichkey — Escape at the bottom sends no
                                // ScrollReset, so nothing else would repaint the
                                // highlights away.
                                if mode == Mode::Normal {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                }
                                // When entering Visual mode, scope to the
                                // focused pane's bounds instead of the full
                                // terminal dimensions.
                                if mode == Mode::Visual {
                                    if let Some(ref mut vs) = input.visual_state {
                                        if let Some(pr) = focused_pane_rect {
                                            vs.visible_rows = pr.height as usize;
                                            vs.visible_cols = pr.width as usize;
                                            vs.pane_offset_x = pr.x;
                                            vs.pane_offset_y = pr.y;
                                            // Place cursor at the pane's actual
                                            // cursor position (relative to pane).
                                            vs.cursor_row = (last_cursor_y.saturating_sub(pr.y)) as usize;
                                            vs.cursor_col = (last_cursor_x.saturating_sub(pr.x)) as usize;
                                            // Clamp to pane bounds.
                                            if vs.cursor_row >= vs.visible_rows {
                                                vs.cursor_row = vs.visible_rows.saturating_sub(1);
                                            }
                                            if vs.cursor_col >= vs.visible_cols {
                                                vs.cursor_col = vs.visible_cols.saturating_sub(1);
                                            }
                                        } else {
                                            // Fallback: use full terminal dims.
                                            let (tc, tr) = crossterm::terminal::size()?;
                                            vs.visible_rows = tr as usize;
                                            vs.visible_cols = tc as usize;
                                            vs.cursor_row = vs.visible_rows.saturating_sub(1);
                                        }
                                        // total_lines is at least visible_rows
                                        // (the front buffer is all we have).
                                        if vs.total_lines < vs.visible_rows {
                                            vs.total_lines = vs.visible_rows;
                                        }
                                    }
                                    // Baseline the VisualScroll delta tracker to this
                                    // fresh state (scroll_offset 0 at the bottom) so the
                                    // first cursor move measures from the right origin.
                                    if let Some(ref vs) = input.visual_state {
                                        last_visual_scroll = vs.scroll_offset;
                                    }
                                    // Request scrollback info to get accurate total_lines.
                                    mgr.send_foreground(ClientMessage::RequestScrollbackInfo).await?;
                                }
                                // When entering Search mode, render the prompt.
                                if mode == Mode::Search {
                                    if let Some(ref ss) = input.search_state {
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.render_search_prompt(&ss.query_buffer, ss.phase, None, c, r)?;
                                    }
                                }
                                // Hide which-key when mode changes
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // Returning to Normal from a visual/search
                                // overlay: clear_overlay above hid the hardware
                                // cursor. Restore it to the terminal's real
                                // position (Escape at the bottom sends no
                                // server frame that would otherwise bring it
                                // back).
                                if mode == Mode::Normal {
                                    renderer.restore_cursor(
                                        last_cursor_x,
                                        last_cursor_y,
                                        last_cursor_visible,
                                    )?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::ActivateRenameOverlay => {
                                // Hide which-key when rename overlay activates
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // Show the rename popup with empty text
                                let target_str = match input.rename_overlay.as_ref().map(|o| &o.target) {
                                    Some(RenameTarget::Tab) => "Tab",
                                    Some(RenameTarget::Pane) => "Pane",
                                    Some(RenameTarget::Session) => "Session",
                                    Some(RenameTarget::NewSession) => "New Session",
                                    None => "Pane",
                                };
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.render_rename_popup("", target_str, c, r)?;
                                renderer.flush()?;
                                // Notify server we're in a rename state
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: "COMMAND".to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::ShowWhichKey(label, entries) => {
                                let (c, r) = crossterm::terminal::size()?;
                                whichkey.show(label, entries);
                                renderer.clear_overlay(c, r)?;
                                let commands = whichkey.render(c, r, &theme, which_key_position.clone());
                                renderer.render_whichkey_overlay(&commands)?;
                                renderer.flush()?;
                            }
                            InputAction::HideWhichKey => {
                                whichkey.hide();
                                renderer.clear_overlay(cols, rows)?;
                                renderer.flush()?;
                            }
                            InputAction::EditInEditor => {
                                log::debug!("input: EditInEditor requested");
                                input.pending_editor_open = true;
                                mgr.send_foreground(ClientMessage::RequestScrollback).await?;
                            }
                            InputAction::RenameUpdate(ref text) => {
                                // Re-render the rename popup with updated text.
                                let target = input.rename_overlay.as_ref()
                                    .map(|o| o.target.clone())
                                    .unwrap_or(RenameTarget::Pane);
                                let target_str = match &target {
                                    RenameTarget::Tab => "Tab",
                                    RenameTarget::Pane => "Pane",
                                    RenameTarget::Session => "Session",
                                    RenameTarget::NewSession => "New Session",
                                };
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.render_rename_popup(text, target_str, c, r)?;
                                renderer.flush()?;
                                // Don't send intermediate updates to server -
                                // only the final rename command is sent on Enter.
                            }
                            InputAction::YankToClipboard(_) => {
                                log::debug!("input: YankToClipboard");
                                // Extract selected text from the front buffer
                                // using the visual state.
                                if let Some(ref vs) = input.visual_state {
                                    let text = renderer.extract_text(vs);
                                    if !text.is_empty() {
                                        if let Err(e) = copy_to_clipboard(&text) {
                                            log::error!("Failed to copy to clipboard: {}", e);
                                        }
                                    }
                                }
                                // Exit visual mode after yanking.
                                if let Some(vs) = input.visual_state.as_mut() {
                                    vs.reset();
                                }
                                input.visual_state = None;
                                // Clear any search state carried in from a
                                // search-to-visual transition so its match
                                // highlights / search status bar don't linger
                                // in Normal mode (mirrors the Escape path).
                                input.search_state = None;
                                input.mode = Mode::Normal;
                                if scroll_offset > 0 || is_scrolled {
                                    scroll_offset = 0;
                                    is_scrolled = false;
                                    mgr.send_foreground(ClientMessage::ScrollReset).await?;
                                }
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: "NORMAL".to_string(),
                                    })
                                    .await?;
                                // Re-render to clear selection highlighting.
                                renderer.clear_overlay(cols, rows)?;
                                // clear_overlay hides the hardware cursor; no
                                // server frame necessarily follows the yank, so
                                // put the real cursor back at its last position.
                                renderer.restore_cursor(last_cursor_x, last_cursor_y, last_cursor_visible)?;
                                renderer.flush()?;
                            }
                            InputAction::VisualScroll { .. } => {
                                // Send scroll delta to server so compositor renders scrollback.
                                if let Some(ref vs) = input.visual_state {
                                    // Delta is the CHANGE in vs.scroll_offset (its own
                                    // lines-from-bottom units), measured against the
                                    // baseline set when the visual view last moved.
                                    // vs.scroll_offset increasing = scrolling up/back,
                                    // which matches ScrollDelta's positive = up/back.
                                    // An in-view cursor move leaves vs.scroll_offset
                                    // unchanged, so delta == 0 and nothing is sent.
                                    let delta = vs.scroll_offset as i32 - last_visual_scroll as i32;
                                    log::debug!("input: VisualScroll offset={} last={} delta={}", vs.scroll_offset, last_visual_scroll, delta);
                                    last_visual_scroll = vs.scroll_offset;
                                    if delta != 0 {
                                        mgr.send_foreground(ClientMessage::ScrollDelta { delta }).await?;
                                    }
                                }
                                // Always repaint the visual overlay so an in-view
                                // cursor/selection move (offset unchanged) shows up
                                // immediately. When the offset changed, the server
                                // frame triggered by the ScrollDelta above also
                                // repaints the overlay — this extra paint is harmless.
                                if let Some(ref vs) = input.visual_state {
                                    renderer.render_visual_overlay(vs)?;
                                    // The overlay repaints the pane from the front
                                    // buffer, which does not contain the search-match
                                    // highlights (they are drawn on top). Redraw them
                                    // so they survive an in-view cursor move.
                                    if let Some(ref ss) = input.search_state {
                                        let query =
                                            ss.confirmed_query.as_deref().unwrap_or(&ss.query_buffer);
                                        renderer.render_search_highlight(
                                            &ss.matches,
                                            ss.current_match,
                                            query.len(),
                                            viewport_top,
                                            focused_pane_rect.as_ref(),
                                            &theme,
                                        )?;
                                    }
                                    renderer.flush()?;
                                }
                            }
                            InputAction::VisualMatchNav => {
                                // handle_visual_key already advanced the visual
                                // state's current_match. Move the cursor to that
                                // match and, if it is off-screen, scroll to it using
                                // a viewport_top-based delta (mirroring the search
                                // flow) rather than the VisualScroll delta path.
                                let pane_h = focused_pane_rect
                                    .map(|pr| pr.height as usize)
                                    .unwrap_or(24);
                                let target = input.visual_state.as_ref().and_then(|vs| {
                                    vs.search_matches.get(vs.current_match).copied()
                                });
                                if let Some((match_line, match_col)) = target {
                                    // Keep the search highlight's current match in
                                    // sync with the visual cursor.
                                    if let Some(ref mut ss) = input.search_state {
                                        let cur = input
                                            .visual_state
                                            .as_ref()
                                            .map(|vs| vs.current_match)
                                            .unwrap_or(0);
                                        ss.current_match =
                                            cur.min(ss.matches.len().saturating_sub(1));
                                        mgr.send_foreground(ClientMessage::SearchInfo {
                                            current: ss.current_match,
                                            total: ss.matches.len(),
                                        })
                                        .await?;
                                    }
                                    // scroll_offset holds viewport_top (absolute
                                    // scrollback line index of the first visible line).
                                    let visible_top = scroll_offset;
                                    let visible_bottom = scroll_offset + pane_h;
                                    let mut sent_scroll = false;
                                    if match_line < visible_top || match_line >= visible_bottom {
                                        let target_vt = match_line.saturating_sub(pane_h / 2);
                                        let delta = scroll_offset as i32 - target_vt as i32;
                                        scroll_offset = target_vt;
                                        is_scrolled = scroll_offset > 0;
                                        if delta != 0 {
                                            mgr.send_foreground(ClientMessage::ScrollDelta { delta })
                                                .await?;
                                            sent_scroll = true;
                                        }
                                    }
                                    // Cursor is pane-relative: row = line - viewport_top.
                                    if let Some(vs) = input.visual_state.as_mut() {
                                        vs.cursor_row = match_line
                                            .saturating_sub(scroll_offset)
                                            .min(vs.visible_rows.saturating_sub(1));
                                        vs.cursor_col =
                                            match_col.min(vs.visible_cols.saturating_sub(1));
                                    }
                                    // Repaint now for the on-screen case. When a
                                    // ScrollDelta was sent, the resulting server frame
                                    // repaints the overlay at the new position (and the
                                    // front buffer will then hold the scrolled content).
                                    if !sent_scroll {
                                        if let Some(ref vs) = input.visual_state {
                                            renderer.render_visual_overlay(vs)?;
                                            // Redraw the match highlights on top of the
                                            // pane repaint (see the VisualScroll arm).
                                            if let Some(ref ss) = input.search_state {
                                                let query = ss
                                                    .confirmed_query
                                                    .as_deref()
                                                    .unwrap_or(&ss.query_buffer);
                                                renderer.render_search_highlight(
                                                    &ss.matches,
                                                    ss.current_match,
                                                    query.len(),
                                                    viewport_top,
                                                    focused_pane_rect.as_ref(),
                                                    &theme,
                                                )?;
                                            }
                                            renderer.flush()?;
                                        }
                                    }
                                }
                            }
                            InputAction::CommandPaletteOpen => {
                                // Hide which-key when opening palette.
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // Render the palette overlay.
                                if let Some(ref palette) = input.command_palette {
                                    let (c, r) = crossterm::terminal::size()?;
                                    let draw_cmds = palette.render(c, r, &theme);
                                    renderer.render_command_palette_overlay(&draw_cmds)?;
                                }
                                renderer.flush()?;
                                // Notify server of mode change.
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: "PALETTE".to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::CommandPaletteUpdate
                            | InputAction::CommandPaletteComplete => {
                                // Re-render the palette overlay with updated state.
                                if let Some(ref palette) = input.command_palette {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_command_palette_overlay(c, r)?;
                                    let draw_cmds = palette.render(c, r, &theme);
                                    renderer.render_command_palette_overlay(&draw_cmds)?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::CommandPaletteExecute => {
                                // Already handled via Execute action path.
                            }
                            InputAction::CommandPaletteClose => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_command_palette_overlay(c, r)?;
                                renderer.flush()?;
                                // Notify server of mode change.
                                let mode_str = match input.mode {
                                    Mode::Normal => "NORMAL",
                                    Mode::Command => "COMMAND",
                                    Mode::Visual => "VISUAL",
                                    Mode::CommandPalette => "PALETTE",
                                    Mode::Search => "SEARCH",
                                    Mode::SessionManager => "SESSION_MANAGER",
                                };
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: mode_str.to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::SearchPrompt => {
                                log::debug!("input: SearchPrompt query={:?}", input.search_state.as_ref().map(|s| &s.query_buffer));
                                // Re-render the search prompt overlay.
                                if let Some(ref ss) = input.search_state {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.render_search_prompt(&ss.query_buffer, ss.phase, None, c, r)?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::SearchConfirm(ref query) => {
                                log::debug!("input: SearchConfirm query={query:?}");
                                // Keep current scroll position — search starts from where user is.
                                // Request scrollback from server.
                                mgr.send_foreground(ClientMessage::RequestScrollback).await?;
                                // Re-render prompt with confirmed query.
                                if let Some(ref ss) = input.search_state {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.render_search_prompt(query, ss.phase, None, c, r)?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::SearchCancel => {
                                log::debug!("input: SearchCancel");
                                // Clear search info on server.
                                mgr.send_foreground(ClientMessage::SearchInfo { current: 0, total: 0 }).await?;
                                // Send mode changed to NORMAL.
                                mgr.send_foreground(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                // Clear overlay.
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                // clear_overlay hides the hardware cursor. This
                                // search->Normal exit does not flow through the
                                // ModeChanged(Normal) handler and, when
                                // unscrolled, sends no follow-up frame, so
                                // restore the real cursor here too.
                                renderer.restore_cursor(last_cursor_x, last_cursor_y, last_cursor_visible)?;
                                // Reset scroll offset when exiting search mode.
                                if scroll_offset > 0 || is_scrolled {
                                    scroll_offset = 0;
                                    is_scrolled = false;
                                    mgr.send_foreground(ClientMessage::ScrollReset).await?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::SearchNavigate => {
                                log::debug!("input: SearchNavigate current={} total={}",
                                    input.search_state.as_ref().map(|s| s.current_match).unwrap_or(0),
                                    input.search_state.as_ref().map(|s| s.matches.len()).unwrap_or(0));
                                // Update search info on server and re-render prompt.
                                if let Some(ref ss) = input.search_state {
                                    mgr.send_foreground(ClientMessage::SearchInfo {
                                        current: ss.current_match,
                                        total: ss.matches.len(),
                                    }).await?;
                                    let match_info = if ss.matches.is_empty() {
                                        None
                                    } else {
                                        Some((ss.current_match, ss.matches.len()))
                                    };
                                    let query = ss.confirmed_query.as_deref().unwrap_or("");
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.render_search_prompt(query, ss.phase, match_info, c, r)?;
                                    // Re-render highlights with updated current match.
                                    renderer.render_search_highlight(
                                        &ss.matches,
                                        ss.current_match,
                                        query.len(),
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                        &theme,
                                    )?;

                                    // Scroll to match if it's in scrollback (not visible).
                                    if !ss.matches.is_empty() {
                                        let (match_line, _match_col) = ss.matches[ss.current_match];
                                        let pane_height = focused_pane_rect
                                            .map(|pr| pr.height as usize)
                                            .unwrap_or(24);
                                        // Calculate the scroll offset needed to center the match
                                        let visible_top_line = scroll_offset;
                                        let visible_bottom_line = scroll_offset + pane_height;
                                        if match_line < visible_top_line || match_line >= visible_bottom_line {
                                            // Match is not visible, scroll to center it
                                            let target_vt = match_line.saturating_sub(pane_height / 2);
                                            let delta = scroll_offset as i32 - target_vt as i32;
                                            scroll_offset = target_vt;
                                            if delta != 0 {
                                                mgr.send_foreground(ClientMessage::ScrollDelta { delta }).await?;
                                            }
                                        }
                                    }
                                }
                                renderer.flush()?;
                            }
                            InputAction::SessionManagerOpen => {
                                log::debug!("input: SessionManagerOpen");
                                // Hide which-key when opening session manager.
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // Seed the freshly-opened manager with the server
                                // roster and current foreground.
                                if let Some(sm) = input.session_manager.as_mut() {
                                    sm.set_foreground(mgr.foreground().clone());
                                    sm.set_roster(mgr.server_roster());
                                }
                                // Refresh every connected server's subtree.
                                for id in mgr.connected_ids() {
                                    mgr.send(&id, ClientMessage::ListSessionTree).await?;
                                }
                                // Notify the foreground server of the mode change.
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: "SESSION_MANAGER".to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::RemoteConnect(dest) => {
                                log::debug!("input: RemoteConnect dest={dest}");
                                // Hide which-key when opening the session manager.
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // This command usually arrives from the palette;
                                // clear its overlay so it doesn't render underneath.
                                if was_in_palette && input.command_palette.is_none() {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_command_palette_overlay(c, r)?;
                                }
                                // Resolve the connection name. If the arg is not a
                                // configured remote, register an ad-hoc (session-only)
                                // entry using the arg as the SSH destination.
                                let name = dest.clone();
                                if !mgr.has_remote(&name) {
                                    mgr.add_remote(
                                        name.clone(),
                                        RemoteConfig {
                                            ssh: dest.clone(),
                                            ..Default::default()
                                        },
                                    );
                                }
                                // Seed the freshly-opened manager with the roster and
                                // foreground, then refresh every connected subtree.
                                if let Some(sm) = input.session_manager.as_mut() {
                                    sm.set_foreground(mgr.foreground().clone());
                                    sm.set_roster(mgr.server_roster());
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = sm.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                    renderer.flush()?;
                                }
                                for id in mgr.connected_ids() {
                                    mgr.send(&id, ClientMessage::ListSessionTree).await?;
                                }
                                // Notify the foreground server of the mode change.
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: "SESSION_MANAGER".to_string(),
                                    })
                                    .await?;
                                // Connect the remote. A failure must NOT exit the
                                // client -- it surfaces as a Failed node in the tree.
                                match mgr.connect_remote(&name).await {
                                    Ok(()) => {
                                        mgr.send(&ConnId::Remote(name.clone()), ClientMessage::ListSessionTree).await?;
                                    }
                                    Err(e) => {
                                        log::warn!("RemoteConnect '{name}' failed: {e}");
                                    }
                                }
                                // Refresh the roster/rows to reflect the new node's
                                // state (Connected or Failed) and redraw.
                                if let Some(sm) = input.session_manager.as_mut() {
                                    sm.set_roster(mgr.server_roster());
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = sm.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                    renderer.flush()?;
                                }
                            }
                            InputAction::SessionManagerClose => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                renderer.flush()?;
                                // Notify server of mode change.
                                mgr
                                    .send_foreground(ClientMessage::ModeChanged {
                                        mode: "NORMAL".to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::SessionManagerUpdate => {
                                // Re-render the session manager overlay.
                                if let Some(ref sm) = input.session_manager {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = sm.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::SessionManagerAction(ref sm_action) => {
                                log::debug!("input: SessionManagerAction {:?}", sm_action);
                                // Clone out of the borrow so we can mutate `input`/`mgr` freely.
                                let sm_action = sm_action.clone();
                                match sm_action {
                                    SessionManagerAction::ConnectRemote(name) => {
                                        // Lazily connect the remote server node, then
                                        // list its tree and refresh the roster/rows.
                                        match mgr.connect_remote(&name).await {
                                            Ok(()) => {
                                                mgr.send(&ConnId::Remote(name.clone()), ClientMessage::ListSessionTree).await?;
                                            }
                                            Err(e) => {
                                                log::warn!("connect remote '{name}' failed: {e:#}");
                                            }
                                        }
                                        // Reflect new state (Connected or Failed) on the node.
                                        if let Some(sm) = input.session_manager.as_mut() {
                                            sm.set_foreground(mgr.foreground().clone());
                                            sm.set_roster(mgr.server_roster());
                                            let (c, r) = crossterm::terminal::size()?;
                                            renderer.clear_overlay(c, r)?;
                                            let draw_cmds = sm.render(c, r, &theme);
                                            renderer.render_whichkey_overlay(&draw_cmds)?;
                                            renderer.flush()?;
                                        }
                                    }
                                    SessionManagerAction::SwitchSession { server, session } => {
                                        input.session_manager = None;
                                        input.mode = Mode::Normal;
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.clear_overlay(c, r)?;
                                        renderer.flush()?;
                                        switch_to_server(mgr, &server, c, r).await?;
                                        mgr.send(&server, ClientMessage::Attach { session_name: session.clone() }).await?;
                                        mgr.send(&server, ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                        if server == ConnId::Local {
                                            last_local_session = Some(session.clone());
                                        }
                                        record_switch(&mut current_attached, &mut previous_attached, server, session);
                                    }
                                    SessionManagerAction::SwitchTab { server, session, tab_index } => {
                                        input.session_manager = None;
                                        input.mode = Mode::Normal;
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.clear_overlay(c, r)?;
                                        renderer.flush()?;
                                        switch_to_server(mgr, &server, c, r).await?;
                                        // The server's handle_command ignores commands from a
                                        // client with no attached session, so a remote tab switch
                                        // must attach first (harmless re-attach for local).
                                        mgr.send(&server, ClientMessage::Attach { session_name: session.clone() }).await?;
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::SessionSwitchTab {
                                            session: session.clone(),
                                            tab_index,
                                        })).await?;
                                        mgr.send(&server, ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                        if server == ConnId::Local {
                                            last_local_session = Some(session.clone());
                                        }
                                        record_switch(&mut current_attached, &mut previous_attached, server, session);
                                    }
                                    SessionManagerAction::SwitchPane { server, session, tab_index, pane_id } => {
                                        input.session_manager = None;
                                        input.mode = Mode::Normal;
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.clear_overlay(c, r)?;
                                        renderer.flush()?;
                                        switch_to_server(mgr, &server, c, r).await?;
                                        // The server's handle_command ignores commands from a
                                        // client with no attached session, so a remote pane switch
                                        // must attach first (harmless re-attach for local).
                                        mgr.send(&server, ClientMessage::Attach { session_name: session.clone() }).await?;
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::SessionSwitchPane {
                                            session: session.clone(),
                                            tab_index,
                                            pane_id,
                                        })).await?;
                                        mgr.send(&server, ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                        if server == ConnId::Local {
                                            last_local_session = Some(session.clone());
                                        }
                                        record_switch(&mut current_attached, &mut previous_attached, server, session);
                                    }
                                    // Structural edits are Local-only (guarded in the
                                    // session manager) and always target the Local server,
                                    // regardless of which connection is foreground.
                                    SessionManagerAction::CreateFolder(name) => {
                                        mgr.send(&ConnId::Local, ClientMessage::Command(RemuxCommand::FolderNew(name.clone()))).await?;
                                        mgr.send(&ConnId::Local, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::CreateSession { name, folder } => {
                                        mgr.send(&ConnId::Local, ClientMessage::CreateSession {
                                            name: name.clone(),
                                            folder: folder.clone(),
                                        }).await?;
                                        mgr.send(&ConnId::Local, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::MoveSession { session, folder } => {
                                        mgr.send(&ConnId::Local, ClientMessage::Command(RemuxCommand::FolderMoveSession {
                                            session: session.clone(),
                                            folder: folder.clone(),
                                        })).await?;
                                        mgr.send(&ConnId::Local, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::DeleteSession(name) => {
                                        mgr.send(&ConnId::Local, ClientMessage::KillSession { name: name.clone() }).await?;
                                        mgr.send(&ConnId::Local, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::DeleteFolder(name) => {
                                        mgr.send(&ConnId::Local, ClientMessage::Command(RemuxCommand::FolderDelete(name.clone()))).await?;
                                        mgr.send(&ConnId::Local, ClientMessage::ListSessionTree).await?;
                                    }
                                    // Resurrecting a dormant session is Local-only: it
                                    // materializes the saved session on the local server,
                                    // then refreshes the tree so it moves from Saved to live.
                                    SessionManagerAction::ResurrectSession(name) => {
                                        mgr.send(&ConnId::Local, ClientMessage::ResurrectSession { name: name.clone() }).await?;
                                        mgr.send(&ConnId::Local, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::CloseTab { session, tab_index } => {
                                        mgr.send(&ConnId::Local, ClientMessage::Command(RemuxCommand::TabCloseByIndex {
                                            session: session.clone(),
                                            tab_index,
                                        })).await?;
                                        mgr.send(&ConnId::Local, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::RefreshTree => {
                                        for id in mgr.connected_ids() {
                                            mgr.send(&id, ClientMessage::ListSessionTree).await?;
                                        }
                                    }
                                    SessionManagerAction::Close => {
                                        let has_sessions = input.session_manager.as_ref()
                                            .map(|sm| sm.rows.iter().any(|r| matches!(r.node_type, NodeType::Session { .. })))
                                            .unwrap_or(false);
                                        input.session_manager = None;
                                        input.mode = Mode::Normal;
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.clear_overlay(c, r)?;
                                        renderer.flush()?;
                                        if !has_sessions {
                                            break;
                                        }
                                        mgr.send_foreground(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                    }
                                    SessionManagerAction::None => {}
                                }
                            }
                            InputAction::FolderSelectOpen => {
                                // Hide which-key popup
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // Request session tree to get folder list
                                mgr.send_foreground(ClientMessage::ListSessionTree).await?;
                                // Set mode to Command to block normal input
                                input.mode = Mode::Command;
                                // Initialize with a loading placeholder
                                input.folder_select = Some(FolderSelectOverlay {
                                    folders: vec!["Loading...".to_string()],
                                    selected: 0,
                                    session_name: String::new(),
                                });
                                mgr.send_foreground(ClientMessage::ModeChanged { mode: "COMMAND".to_string() }).await?;
                            }
                            InputAction::FolderSelectUpdate => {
                                if let Some(ref fs) = input.folder_select {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = fs.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::FolderSelectConfirm { ref session, ref folder } => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                renderer.flush()?;
                                // Send the move command
                                mgr.send_foreground(ClientMessage::Command(RemuxCommand::FolderMoveSession {
                                    session: session.clone(),
                                    folder: folder.clone(),
                                })).await?;
                                mgr.send_foreground(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                            }
                            InputAction::FolderSelectClose => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                renderer.flush()?;
                                mgr.send_foreground(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                            }
                            InputAction::SessionSwitchOpen => {
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // Query every connected server (local + remotes)
                                // so the switcher aggregates all their sessions.
                                for id in mgr.connected_ids() {
                                    mgr.send(&id, ClientMessage::ListSessionTree).await?;
                                }
                                input.mode = Mode::Command;
                                input.session_switch = Some(SessionSwitchOverlay::new());
                                mgr.send_foreground(ClientMessage::ModeChanged { mode: "COMMAND".to_string() }).await?;
                            }
                            InputAction::SessionSwitchUpdate => {
                                if let Some(ref ss) = input.session_switch {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = ss.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::SessionSwitchConfirm { server, session } => {
                                input.session_switch = None;
                                input.mode = Mode::Normal;
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                renderer.flush()?;
                                // Hand off to the target server (no-op when it is
                                // already foreground) and attach. Re-attaching to
                                // the current session is harmless. Mirrors the
                                // session-manager SwitchSession path.
                                switch_to_server(mgr, &server, c, r).await?;
                                mgr.send(&server, ClientMessage::Attach { session_name: session.clone() }).await?;
                                mgr.send(&server, ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                if server == ConnId::Local {
                                    last_local_session = Some(session.clone());
                                }
                                record_switch(&mut current_attached, &mut previous_attached, server, session);
                            }
                            InputAction::SessionSwitchLast => {
                                // Toggle to the previously-attached session. Reset
                                // mode and tear down any which-key popup first so
                                // the leader `x o` path can't leave it lingering,
                                // then either switch (when a previous exists) or
                                // just re-sync the server's mode (no-op switch).
                                input.mode = Mode::Normal;
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                renderer.flush()?;
                                if let Some((server, session)) = previous_attached.clone() {
                                    // Mirror the SessionSwitchConfirm path.
                                    switch_to_server(mgr, &server, c, r).await?;
                                    mgr.send(&server, ClientMessage::Attach { session_name: session.clone() }).await?;
                                    mgr.send(&server, ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                    if server == ConnId::Local {
                                        last_local_session = Some(session.clone());
                                    }
                                    // Record so repeated Alt-o toggles back and forth.
                                    record_switch(&mut current_attached, &mut previous_attached, server, session);
                                } else {
                                    // No previous session: keep the server's mode in sync.
                                    mgr.send_foreground(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                }
                            }
                            InputAction::SessionSwitchClose => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                renderer.flush()?;
                                input.session_switch = None;
                                input.mode = Mode::Normal;
                                mgr.send_foreground(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                            }
                            InputAction::NewSession(ref name) => {
                                // Create the session and then attach to it.
                                mgr.send_foreground(ClientMessage::CreateSession {
                                    name: name.clone(),
                                    folder: None,
                                }).await?;
                                mgr.send_foreground(ClientMessage::Attach { session_name: name.clone() }).await?;
                                mgr.send_foreground(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                // Creating-and-attaching is a foreground switch too:
                                // record it so the Alt-o toggle baseline stays in
                                // sync with the actual foreground session.
                                let fg = mgr.foreground().clone();
                                record_switch(&mut current_attached, &mut previous_attached, fg, name.clone());
                            }
                            InputAction::None => {}
                        }
                    }
                    Some(Ok(crossterm::event::Event::Mouse(mouse))) => {
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                log::debug!("mouse: click at ({}, {})", mouse.column, mouse.row);
                                drag_start = Some((mouse.column, mouse.row));
                                // Send click immediately.
                                mgr
                                    .send_foreground(ClientMessage::MouseClick {
                                        x: mouse.column,
                                        y: mouse.row,
                                    })
                                    .await?;
                            }
                            MouseEventKind::Drag(MouseButton::Left) => {
                                // Throttle drag events to ~60fps.
                                let now = Instant::now();
                                if now.duration_since(last_drag_send) >= DRAG_THROTTLE {
                                    if let Some((sx, sy)) = drag_start {
                                        mgr
                                            .send_foreground(ClientMessage::MouseDrag {
                                                start_x: sx,
                                                start_y: sy,
                                                end_x: mouse.column,
                                                end_y: mouse.row,
                                                is_final: false,
                                            })
                                            .await?;
                                        last_drag_send = now;
                                    }
                                }
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                // Send final drag on release.
                                if let Some((sx, sy)) = drag_start.take() {
                                    if sx != mouse.column || sy != mouse.row {
                                        mgr
                                            .send_foreground(ClientMessage::MouseDrag {
                                                start_x: sx,
                                                start_y: sy,
                                                end_x: mouse.column,
                                                end_y: mouse.row,
                                                is_final: true,
                                            })
                                            .await?;
                                    }
                                }
                            }
                            MouseEventKind::ScrollUp => {
                                log::debug!("mouse: scroll up, is_scrolled={}", is_scrolled);
                                if input.mode == Mode::Visual {
                                    // Visual mode is remux's copy-mode: wheel scrolls the
                                    // local copy view and is never forwarded to the app.
                                    if let Some(ref mut vs) = input.visual_state {
                                        vs.scroll_up(3);
                                        scroll_offset = vs.scroll_offset;
                                        // Keep the VisualScroll delta baseline in sync so
                                        // a following cursor-move key doesn't re-send this
                                        // wheel scroll as a bogus delta.
                                        last_visual_scroll = vs.scroll_offset;
                                    }
                                } else {
                                    // Server decides: forward to the app (mouse/alt screen)
                                    // or scroll remux scrollback. It replies with a render
                                    // that re-syncs scroll_offset/is_scrolled.
                                    mgr.send_foreground(ClientMessage::MouseScroll {
                                        x: mouse.column,
                                        y: mouse.row,
                                        up: true,
                                    })
                                    .await?;
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                log::debug!("mouse: scroll down, is_scrolled={}", is_scrolled);
                                if input.mode == Mode::Visual {
                                    // Visual mode is remux's copy-mode: wheel scrolls the
                                    // local copy view and is never forwarded to the app.
                                    if let Some(ref mut vs) = input.visual_state {
                                        vs.scroll_down(3);
                                        scroll_offset = vs.scroll_offset;
                                        // Keep the VisualScroll delta baseline in sync so
                                        // a following cursor-move key doesn't re-send this
                                        // wheel scroll as a bogus delta.
                                        last_visual_scroll = vs.scroll_offset;
                                    }
                                } else {
                                    // Server decides: forward to the app (mouse/alt screen)
                                    // or scroll remux scrollback. It replies with a render
                                    // that re-syncs scroll_offset/is_scrolled.
                                    mgr.send_foreground(ClientMessage::MouseScroll {
                                        x: mouse.column,
                                        y: mouse.row,
                                        up: false,
                                    })
                                    .await?;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(crossterm::event::Event::Resize(new_cols, new_rows))) => {
                        log::debug!("resize: {}x{}", new_cols, new_rows);
                        renderer.resize(new_cols, new_rows);
                        mgr.send_foreground(ClientMessage::Resize { cols: new_cols, rows: new_rows }).await?;
                    }
                    Some(Ok(crossterm::event::Event::Paste(text))) => {
                        // Wrap pasted text in bracketed paste sequences.
                        let mut data = Vec::new();
                        data.extend_from_slice(b"\x1b[200~");
                        data.extend_from_slice(text.as_bytes());
                        data.extend_from_slice(b"\x1b[201~");
                        mgr.send_foreground(ClientMessage::Input { data }).await?;
                    }
                    Some(Err(e)) => {
                        log::error!("Event error: {}", e);
                    }
                    None => break,
                    _ => {}
                }
            }
            // Server messages
            maybe_incoming = mgr.recv() => {
                // Decode the routed message and its source connection. A `Closed`
                // is handled here (foreground-drop fallback / background cleanup);
                // otherwise we get the source id and an owned `ServerMessage`.
                let incoming = match maybe_incoming {
                    Some(i) => i,
                    // Every connection (including local) is gone — nothing left to
                    // drive the loop.
                    None => return Ok(()),
                };
                let (src, msg) = match incoming {
                    Incoming::Message(src, m) => (src, Some(m)),
                    Incoming::Closed(src) => {
                        log::debug!("srv: connection closed src={:?}", src);
                        if mgr.is_foreground(&src) {
                            match &src {
                                // Local foreground drop: exit the client (unchanged).
                                ConnId::Local => return Ok(()),
                                // Foreground remote drop: fall back to local; MUST
                                // NOT exit the client.
                                ConnId::Remote(name) => {
                                    log::warn!("foreground remote '{name}' dropped; falling back to local");
                                    mgr.fail_remote(name, "connection lost".to_string());
                                    // The standalone `attach-remote` flow has no local
                                    // connection to fall back to — exit gracefully.
                                    if !mgr.connected_ids().contains(&ConnId::Local) {
                                        log::warn!("no local connection to fall back to; exiting");
                                        return Ok(());
                                    }
                                    mgr.set_foreground(ConnId::Local);
                                    let (c, r) = crossterm::terminal::size()?;
                                    mgr.send(&ConnId::Local, ClientMessage::Resize { cols: c, rows: r }).await?;
                                    if let Some(session) = last_local_session.clone() {
                                        // Reattach; the server responds with a fresh FullRender.
                                        mgr.send(&ConnId::Local, ClientMessage::Attach { session_name: session.clone() }).await?;
                                        record_switch(&mut current_attached, &mut previous_attached, ConnId::Local, session);
                                    } else {
                                        // Nothing to fall back to: open the session manager.
                                        input.mode = Mode::SessionManager;
                                        input.session_manager = Some(
                                            crate::client::session_manager::SessionManagerState::new(None),
                                        );
                                        if let Some(sm) = input.session_manager.as_mut() {
                                            sm.set_foreground(mgr.foreground().clone());
                                            sm.set_roster(mgr.server_roster());
                                        }
                                        for id in mgr.connected_ids() {
                                            mgr.send(&id, ClientMessage::ListSessionTree).await?;
                                        }
                                        mgr.send(&ConnId::Local, ClientMessage::ModeChanged { mode: "SESSION_MANAGER".to_string() }).await?;
                                    }
                                    // If the session manager was open when the remote
                                    // dropped, refresh it so the node stops showing
                                    // Connected and reflects the new foreground.
                                    if let Some(sm) = input.session_manager.as_mut() {
                                        sm.set_foreground(mgr.foreground().clone());
                                        sm.set_roster(mgr.server_roster());
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.clear_overlay(c, r)?;
                                        let draw_cmds = sm.render(c, r, &theme);
                                        renderer.render_whichkey_overlay(&draw_cmds)?;
                                        renderer.flush()?;
                                    }
                                }
                            }
                        } else {
                            // A background remote dropped: mark it Failed and, if the
                            // session manager is open, refresh its roster/rows.
                            if let ConnId::Remote(name) = &src {
                                mgr.fail_remote(name, "connection lost".to_string());
                            }
                            if let Some(sm) = input.session_manager.as_mut() {
                                sm.set_foreground(mgr.foreground().clone());
                                sm.set_roster(mgr.server_roster());
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                let draw_cmds = sm.render(c, r, &theme);
                                renderer.render_whichkey_overlay(&draw_cmds)?;
                                renderer.flush()?;
                            }
                        }
                        continue;
                    }
                };
                // Background connections' renders are dropped: only the foreground
                // streams to the screen. This preserves the tuned render hot path.
                if matches!(
                    msg,
                    Some(ServerMessage::FullRender { .. })
                        | Some(ServerMessage::RenderDiff { .. })
                        | Some(ServerMessage::ScrollRender { .. })
                ) && !mgr.is_foreground(&src)
                {
                    continue;
                }
                match msg {
                    Some(ServerMessage::FullRender { cells, cursor_x, cursor_y, cursor_visible, cursor_style, focused_pane_rect: fpr, application_cursor_keys: ack, viewport_top: so }) => {
                        log::debug!("srv: FullRender rows={} cols={} cursor=({},{}) visible={} scroll_offset={}",
                            cells.len(), if cells.is_empty() { 0 } else { cells[0].len() }, cursor_x, cursor_y, cursor_visible, so);
                        focused_pane_rect = fpr;
                        input.application_cursor_keys = ack;
                        scroll_offset = so;
                        // Server render is authoritative for the viewport top;
                        // keep the dedicated highlight coordinate in sync.
                        viewport_top = so;
                        is_scrolled = so > 0;
                        last_cursor_x = cursor_x;
                        last_cursor_y = cursor_y;
                        last_cursor_visible = cursor_visible;
                        renderer.render_full(&cells, cursor_x, cursor_y, cursor_visible, cursor_style)?;
                        // Re-render visual overlay on top if in visual mode
                        if let Some(ref vs) = input.visual_state {
                            renderer.render_visual_overlay(vs)?;
                        }
                        // Re-render rename popup on top if active
                        if let Some(ref overlay) = input.rename_overlay {
                            let target_str = match overlay.target {
                                RenameTarget::Tab => "Tab",
                                RenameTarget::Pane => "Pane",
                                RenameTarget::Session => "Session",
                                RenameTarget::NewSession => "New Session",
                            };
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.render_rename_popup(&overlay.buffer, target_str, c, r)?;
                        }
                        // Re-render command palette on top if active
                        else if let Some(ref palette) = input.command_palette {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = palette.render(c, r, &theme);
                            renderer.render_command_palette_overlay(&draw_cmds)?;
                        }
                        // Re-render search prompt and highlights on top if in search mode
                        else if let Some(ref ss) = input.search_state {
                            let query = ss.confirmed_query.as_deref().unwrap_or(&ss.query_buffer);
                            let match_info = if ss.matches.is_empty() { None } else { Some((ss.current_match, ss.matches.len())) };
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.render_search_highlight(
                                &ss.matches,
                                ss.current_match,
                                query.len(),
                                viewport_top,
                                focused_pane_rect.as_ref(),
                                &theme,
                            )?;
                            renderer.render_search_prompt(query, ss.phase, match_info, c, r)?;
                        }
                        // Re-render session switch overlay on top if active
                        else if let Some(ref ss) = input.session_switch {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = ss.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        // Re-render folder select overlay on top if active
                        else if let Some(ref fs) = input.folder_select {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = fs.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        // Re-render session manager on top if active
                        else if let Some(ref sm) = input.session_manager {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = sm.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        // Re-render popup on top if visible
                        else if whichkey.visible {
                            let commands = whichkey.render(cols, rows, &theme, which_key_position.clone());
                            renderer.render_whichkey_overlay(&commands)?;
                        }
                        renderer.flush()?;
                    }
                    Some(ServerMessage::RenderDiff { changes, cursor_x, cursor_y, cursor_visible, cursor_style, focused_pane_rect: fpr, application_cursor_keys: ack, viewport_top: so }) => {
                        log::debug!("srv: RenderDiff changes={} cursor=({},{}) scroll_offset={}", changes.len(), cursor_x, cursor_y, so);
                        focused_pane_rect = fpr;
                        input.application_cursor_keys = ack;
                        scroll_offset = so;
                        // Server render is authoritative for the viewport top;
                        // keep the dedicated highlight coordinate in sync.
                        viewport_top = so;
                        is_scrolled = so > 0;
                        last_cursor_x = cursor_x;
                        last_cursor_y = cursor_y;
                        last_cursor_visible = cursor_visible;
                        renderer.render_diff(&changes, cursor_x, cursor_y, cursor_visible, cursor_style)?;
                        // Re-render visual overlay on top if in visual mode
                        if let Some(ref vs) = input.visual_state {
                            renderer.render_visual_overlay(vs)?;
                        }
                        // Re-render rename popup on top if active
                        if let Some(ref overlay) = input.rename_overlay {
                            let target_str = match overlay.target {
                                RenameTarget::Tab => "Tab",
                                RenameTarget::Pane => "Pane",
                                RenameTarget::Session => "Session",
                                RenameTarget::NewSession => "New Session",
                            };
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.render_rename_popup(&overlay.buffer, target_str, c, r)?;
                        }
                        // Re-render command palette on top if active
                        else if let Some(ref palette) = input.command_palette {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = palette.render(c, r, &theme);
                            renderer.render_command_palette_overlay(&draw_cmds)?;
                        }
                        // Re-render search prompt and highlights on top if in search mode
                        else if let Some(ref ss) = input.search_state {
                            let query = ss.confirmed_query.as_deref().unwrap_or(&ss.query_buffer);
                            let match_info = if ss.matches.is_empty() { None } else { Some((ss.current_match, ss.matches.len())) };
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.render_search_highlight(
                                &ss.matches,
                                ss.current_match,
                                query.len(),
                                viewport_top,
                                focused_pane_rect.as_ref(),
                                &theme,
                            )?;
                            renderer.render_search_prompt(query, ss.phase, match_info, c, r)?;
                        }
                        // Re-render session switch overlay on top if active
                        else if let Some(ref ss) = input.session_switch {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = ss.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        // Re-render folder select overlay on top if active
                        else if let Some(ref fs) = input.folder_select {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = fs.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        // Re-render session manager on top if active
                        else if let Some(ref sm) = input.session_manager {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = sm.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        // Re-render popup on top if visible
                        else if whichkey.visible {
                            let commands = whichkey.render(cols, rows, &theme, which_key_position.clone());
                            renderer.render_whichkey_overlay(&commands)?;
                        }
                        renderer.flush()?;
                    }
                    Some(ServerMessage::ScrollRender { pane_x, pane_y, pane_width, pane_height, delta, new_rows, cursor_x, cursor_y, cursor_visible, cursor_style, focused_pane_rect: fpr, application_cursor_keys: ack, viewport_top: so }) => {
                        log::debug!("srv: ScrollRender delta={} pane=({},{} {}x{}) scroll_offset={}", delta, pane_x, pane_y, pane_width, pane_height, so);
                        focused_pane_rect = fpr;
                        input.application_cursor_keys = ack;
                        scroll_offset = so;
                        // Server render is authoritative for the viewport top;
                        // keep the dedicated highlight coordinate in sync.
                        viewport_top = so;
                        is_scrolled = so > 0;
                        last_cursor_x = cursor_x;
                        last_cursor_y = cursor_y;
                        last_cursor_visible = cursor_visible;
                        renderer.render_scroll(pane_x, pane_y, pane_width, pane_height, delta, &new_rows, cursor_x, cursor_y, cursor_visible, cursor_style)?;
                        // Re-render visual overlay on top if in visual mode
                        if let Some(ref vs) = input.visual_state {
                            renderer.render_visual_overlay(vs)?;
                        }
                        // Re-render rename popup on top if active
                        if let Some(ref overlay) = input.rename_overlay {
                            let target_str = match overlay.target {
                                RenameTarget::Tab => "Tab",
                                RenameTarget::Pane => "Pane",
                                RenameTarget::Session => "Session",
                                RenameTarget::NewSession => "New Session",
                            };
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.render_rename_popup(&overlay.buffer, target_str, c, r)?;
                        }
                        // Re-render command palette on top if active
                        else if let Some(ref palette) = input.command_palette {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = palette.render(c, r, &theme);
                            renderer.render_command_palette_overlay(&draw_cmds)?;
                        }
                        // Re-render search prompt and highlights on top if in search mode
                        else if let Some(ref ss) = input.search_state {
                            let query = ss.confirmed_query.as_deref().unwrap_or(&ss.query_buffer);
                            let match_info = if ss.matches.is_empty() { None } else { Some((ss.current_match, ss.matches.len())) };
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.render_search_highlight(
                                &ss.matches,
                                ss.current_match,
                                query.len(),
                                viewport_top,
                                focused_pane_rect.as_ref(),
                                &theme,
                            )?;
                            renderer.render_search_prompt(query, ss.phase, match_info, c, r)?;
                        }
                        // Re-render session switch overlay on top if active
                        else if let Some(ref ss) = input.session_switch {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = ss.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        // Re-render folder select overlay on top if active
                        else if let Some(ref fs) = input.folder_select {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = fs.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        // Re-render session manager on top if active
                        else if let Some(ref sm) = input.session_manager {
                            let (c, r) = crossterm::terminal::size()?;
                            let draw_cmds = sm.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        // Re-render popup on top if visible
                        else if whichkey.visible {
                            let commands = whichkey.render(cols, rows, &theme, which_key_position.clone());
                            renderer.render_whichkey_overlay(&commands)?;
                        }
                        renderer.flush()?;
                    }
                    Some(ServerMessage::SessionList { sessions }) => {
                        log::debug!("received session list with {} sessions", sessions.len());
                    }
                    Some(ServerMessage::Error { message }) => {
                        log::error!("Server error: {}", message);
                    }
                    Some(ServerMessage::CopyToClipboard { data }) => {
                        if let Err(e) = copy_to_clipboard(&data) {
                            log::error!("Failed to copy to clipboard: {}", e);
                        }
                    }
                    Some(ServerMessage::ScrollbackContent { lines }) => {
                        log::debug!("srv: ScrollbackContent line_count={}", lines.len());
                        // Data captured for a Search -> Visual transition once the
                        // search-state borrow below is released.
                        let mut enter_visual_at_match: Option<SearchToVisual> = None;
                        if input.pending_editor_open {
                            input.pending_editor_open = false;
                            let content = lines.join("\n");
                            // Temporarily restore terminal for editor
                            restore_terminal()?;
                            if let Err(e) = crate::client::editor::open_in_editor(&content) {
                                log::error!("Failed to open editor: {}", e);
                            }
                            setup_terminal()?;
                            // Re-send resize in case terminal changed
                            let (cols, rows) = crossterm::terminal::size()?;
                            renderer.resize(cols, rows);
                            mgr.send_foreground(ClientMessage::Resize { cols, rows }).await?;
                        } else if let Some(ref mut ss) = input.search_state {
                            if let Some(ref query) = ss.confirmed_query {
                                let pane_height = focused_pane_rect
                                    .map(|pr| pr.height as usize)
                                    .unwrap_or(24);

                                ss.scrollback_line_count = lines.len();
                                ss.matches = crate::client::input::SearchState::compute_matches(&lines, query);

                                // Search behaves like scrollback: land on the
                                // bottom-most (most recent) match. From there,
                                // 'n' moves up (older) and 'p' moves down (newer).
                                ss.current_match = ss.matches.len().saturating_sub(1);

                                // Send search info to server.
                                mgr.send_foreground(ClientMessage::SearchInfo {
                                    current: ss.current_match,
                                    total: ss.matches.len(),
                                }).await?;

                                // Scroll to the current match if it's not in the visible area.
                                if !ss.matches.is_empty() {
                                    let (match_line, _) = ss.matches[ss.current_match];
                                    let visible_top = scroll_offset;
                                    let visible_bottom = scroll_offset + pane_height;

                                    if match_line < visible_top || match_line >= visible_bottom {
                                        // Scroll to center the match
                                        let target_vt = match_line.saturating_sub(pane_height / 2);
                                        let delta = scroll_offset as i32 - target_vt as i32;
                                        scroll_offset = target_vt;
                                        is_scrolled = true;
                                        if delta != 0 {
                                            mgr.send_foreground(ClientMessage::ScrollDelta { delta }).await?;
                                        }
                                    }
                                }

                                // If we found a match, capture the data needed to
                                // switch into Visual mode at the match (applied
                                // below, after the search-state borrow is dropped).
                                if !ss.matches.is_empty() {
                                    let (match_line, match_col) = ss.matches[ss.current_match];
                                    enter_visual_at_match = Some(SearchToVisual {
                                        matches: ss.matches.clone(),
                                        current_match: ss.current_match,
                                        total_lines: ss.scrollback_line_count,
                                        match_line,
                                        match_col,
                                    });
                                }

                                // Render highlights at current display offset (0 if at bottom,
                                // or wherever the server has scrolled to).
                                // NOTE: Don't render highlights here — the server will send a
                                // render response (FullRender/ScrollRender) which triggers the
                                // overlay re-render with correct positions. Just render the prompt.
                                let match_info = if ss.matches.is_empty() {
                                    None
                                } else {
                                    Some((ss.current_match, ss.matches.len()))
                                };
                                let q = query.clone();
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.render_search_prompt(&q, ss.phase, match_info, c, r)?;
                                renderer.flush()?;
                            }
                        }

                        // Search found a match: leave the user in Visual mode at
                        // the match so they can hjkl-move and select around it.
                        // search_state is kept alongside so the all-match highlight
                        // and prompt keep rendering; n/p/N keep both indices in sync.
                        if let Some(SearchToVisual {
                            matches,
                            current_match,
                            total_lines,
                            match_line,
                            match_col,
                        }) = enter_visual_at_match
                        {
                            let match_total = matches.len();
                            let mut vs = crate::client::input::VisualState::new(
                                focused_pane_rect.map(|pr| pr.height as usize).unwrap_or(24),
                                total_lines,
                            );
                            if let Some(pr) = focused_pane_rect {
                                vs.visible_rows = pr.height as usize;
                                vs.visible_cols = pr.width as usize;
                                vs.pane_offset_x = pr.x;
                                vs.pane_offset_y = pr.y;
                            }
                            vs.total_lines = total_lines.max(vs.visible_rows);
                            vs.search_matches = matches;
                            vs.current_match = current_match;
                            // vs.scroll_offset is lines-from-bottom (used by its own
                            // selection math); scroll_offset here is viewport_top.
                            vs.scroll_offset = vs
                                .total_lines
                                .saturating_sub(vs.visible_rows + scroll_offset);
                            // Cursor is pane-relative: row = line - viewport_top.
                            vs.cursor_row = match_line
                                .saturating_sub(scroll_offset)
                                .min(vs.visible_rows.saturating_sub(1));
                            vs.cursor_col = match_col.min(vs.visible_cols.saturating_sub(1));
                            input.visual_state = Some(vs);
                            // Baseline the VisualScroll delta tracker to the landing
                            // position so the first cursor move (in view) yields delta 0
                            // instead of a bogus jump. This is the off-screen-match fix.
                            if let Some(ref vs) = input.visual_state {
                                last_visual_scroll = vs.scroll_offset;
                            }
                            input.mode = Mode::Visual;
                            // Notify the server (also triggers a fresh frame that
                            // repaints the visual overlay at the match).
                            mgr.send_foreground(ClientMessage::ModeChanged {
                                mode: "VISUAL".to_string(),
                            })
                            .await?;
                            // Re-assert the match count (ModeChanged clears the
                            // server-side search info for non-SEARCH modes).
                            mgr.send_foreground(ClientMessage::SearchInfo {
                                current: current_match,
                                total: match_total,
                            })
                            .await?;
                        }
                    }
                    Some(ServerMessage::SessionTree { folders, unfiled, dormant }) => {
                        log::debug!("srv: SessionTree src={:?} folders={} unfiled={} dormant={}", src, folders.len(), unfiled.len(), dormant.len());
                        // The session-switch popup aggregates every connected
                        // server's tree, so it accepts trees from ANY source
                        // (not just the foreground) and tags each with `src`,
                        // including the current session (marked, not filtered).
                        if input.session_switch.is_some() {
                            let mut sessions: Vec<(String, bool)> = Vec::new();
                            for f in &folders {
                                for s in &f.sessions {
                                    sessions.push((s.name.clone(), s.is_current));
                                }
                            }
                            for s in &unfiled {
                                sessions.push((s.name.clone(), s.is_current));
                            }
                            // Replace this server's rows (a re-received tree for
                            // the same `src` overwrites rather than duplicates).
                            input.merge_session_switch(src.clone(), sessions);
                            // Render the popup
                            if let Some(ref ss) = input.session_switch {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                let draw_cmds = ss.render(c, r, &theme);
                                renderer.render_whichkey_overlay(&draw_cmds)?;
                            }
                        }
                        // If folder select overlay is active, populate it
                        else if mgr.is_foreground(&src) && input.folder_select.is_some() {
                            let folder_names: Vec<String> = folders.iter().map(|f| f.name.clone()).collect();
                            // Find current session name and folder from the tree
                            let mut current_session_name = String::new();
                            let mut current_folder: Option<String> = None;
                            for f in &folders {
                                for s in &f.sessions {
                                    if s.is_current {
                                        current_session_name = s.name.clone();
                                        current_folder = Some(f.name.clone());
                                    }
                                }
                            }
                            if current_session_name.is_empty() {
                                for s in &unfiled {
                                    if s.is_current {
                                        current_session_name = s.name.clone();
                                    }
                                }
                            }
                            input.update_folder_list(folder_names, current_folder, current_session_name);
                            // Render the popup
                            if let Some(ref fs) = input.folder_select {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                let draw_cmds = fs.render(c, r, &theme);
                                renderer.render_whichkey_overlay(&draw_cmds)?;
                            }
                        }
                        // Otherwise route the tree into the session manager,
                        // updating the source server's subtree.
                        else if let Some(sm) = input.session_manager.as_mut() {
                            sm.set_foreground(mgr.foreground().clone());
                            sm.set_roster(mgr.server_roster());
                            sm.update_tree(src, folders, unfiled, dormant);
                            // If, after merging, no server has any session and the
                            // foreground is local, the last session was closed —
                            // exit as before. A remote-only empty tree must not
                            // exit the client.
                            let has_any = sm
                                .rows
                                .iter()
                                .any(|r| matches!(r.node_type, NodeType::Session { .. }));
                            if !has_any && mgr.is_foreground(&ConnId::Local) {
                                input.session_manager = None;
                                input.mode = Mode::Normal;
                                break;
                            }
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.clear_overlay(c, r)?;
                            let draw_cmds = sm.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                        }
                        renderer.flush()?;
                    }
                    Some(ServerMessage::Event(event)) => {
                        log::debug!("server event: src={:?} {:?}", src, event);
                        // Events are foreground-scoped: a background remote's
                        // SessionDeleted must not drive the local loop.
                        if mgr.is_foreground(&src)
                            && matches!(event, crate::protocol::SessionEvent::SessionDeleted(_))
                        {
                            // If session manager is open, just refresh the tree
                            // instead of breaking out of the event loop.
                            if input.session_manager.is_some() {
                                for id in mgr.connected_ids() {
                                    mgr.send(&id, ClientMessage::ListSessionTree).await?;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                    Some(ServerMessage::ScrollbackInfo { total_lines }) => {
                        log::debug!("srv: ScrollbackInfo total_lines={}", total_lines);
                        // Update visual state with accurate total line count.
                        if let Some(ref mut vs) = input.visual_state {
                            vs.total_lines = total_lines;
                        }
                    }
                    // Unreachable: `Closed` is handled in the preamble above, so
                    // `msg` is always `Some` here.
                    None => {}
                }
            }
            // Config hot-reload (rare, low-priority). Applies client-side
            // settings live so edits to ~/.config/remux/config.toml don't
            // require a restart. The channel is kept open by `_cfg_keepalive`,
            // so `None` only appears on a genuine full teardown.
            maybe_cfg = cfg_rx.recv() => {
                if let Some(new_config) = maybe_cfg {
                    // Revalidate cross-references (logs on bad refs, like startup).
                    new_config.validate();

                    // Swap keybindings/leader/shortcuts and reset any stale chord.
                    input.reload_keybindings(
                        new_config.keybinding_tree(),
                        new_config.leader_key(),
                        new_config.shortcut_bindings(),
                    );

                    // Update theme before any re-render so overlays repaint with
                    // the new colors.
                    theme = new_config.theme();

                    // Update which-key placement so it changes live too.
                    which_key_position = new_config.appearance.which_key_position.clone();

                    // Reconcile the remotes roster (update in place / add new /
                    // drop idle config-removed remotes).
                    mgr.update_remotes(&new_config.remotes);

                    // If the session-manager overlay is open, repaint it so the
                    // new theme takes effect immediately.
                    if input.session_manager.is_some() {
                        if let Some(sm) = input.session_manager.as_ref() {
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.clear_overlay(c, r)?;
                            let draw_cmds = sm.render(c, r, &theme);
                            renderer.render_whichkey_overlay(&draw_cmds)?;
                            renderer.flush()?;
                        }
                    }

                    log::info!("client: config reloaded");
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str) -> (ConnId, String) {
        (ConnId::Local, name.to_string())
    }

    #[test]
    fn record_switch_sets_previous_on_change() {
        let mut current = Some(local("a"));
        let mut previous = None;

        record_switch(&mut current, &mut previous, ConnId::Local, "b".to_string());

        assert_eq!(current, Some(local("b")));
        assert_eq!(previous, Some(local("a")));
    }

    #[test]
    fn record_switch_ignores_same_session() {
        let mut current = Some(local("a"));
        let mut previous = Some(local("z"));

        record_switch(&mut current, &mut previous, ConnId::Local, "a".to_string());

        // No self-switch: current unchanged and previous is NOT clobbered.
        assert_eq!(current, Some(local("a")));
        assert_eq!(previous, Some(local("z")));
    }

    #[test]
    fn record_switch_from_empty_seeds_current() {
        let mut current: Option<(ConnId, String)> = None;
        let mut previous: Option<(ConnId, String)> = None;

        record_switch(&mut current, &mut previous, ConnId::Local, "a".to_string());

        assert_eq!(current, Some(local("a")));
        assert_eq!(previous, None);
    }

    #[test]
    fn record_switch_toggles_back_and_forth() {
        let mut current = Some(local("a"));
        let mut previous = None;

        // a -> b
        record_switch(&mut current, &mut previous, ConnId::Local, "b".to_string());
        assert_eq!(current, Some(local("b")));
        assert_eq!(previous, Some(local("a")));

        // Toggle back to previous (b -> a): repeated Alt-o must ping-pong.
        record_switch(&mut current, &mut previous, ConnId::Local, "a".to_string());
        assert_eq!(current, Some(local("a")));
        assert_eq!(previous, Some(local("b")));

        record_switch(&mut current, &mut previous, ConnId::Local, "b".to_string());
        assert_eq!(current, Some(local("b")));
        assert_eq!(previous, Some(local("a")));
    }

    #[test]
    fn record_switch_tracks_remote_server() {
        let mut current = Some(local("a"));
        let mut previous = None;
        let remote = (ConnId::Remote("mini".to_string()), "build".to_string());

        record_switch(
            &mut current,
            &mut previous,
            ConnId::Remote("mini".to_string()),
            "build".to_string(),
        );

        assert_eq!(current, Some(remote));
        assert_eq!(previous, Some(local("a")));
    }
}
