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
use crate::protocol::{ClientMessage, ConnDescriptor, RemuxCommand, ServerMessage, ViewId};
use crate::server::daemon::{self, socket_path, RemuxServer};

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

    /// Stop the running server, saving session state first
    Stop,

    /// Restart the server (stop, then start), preserving saved sessions
    Restart,

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

    // Refuse to launch an interactive client inside an existing remux pane
    // (mirrors tmux's $TMUX guard). Only the interactive attach/create flows
    // are guarded; management subcommands (ls, kill, stop, restart, and the
    // internal server/relay) are unaffected. Override with REMUX_ALLOW_NESTED.
    if matches!(
        cli.command,
        None | Some(Commands::New { .. })
            | Some(Commands::Attach { .. })
            | Some(Commands::AttachRemote { .. })
    ) && std::env::var_os("REMUX").is_some()
        && std::env::var_os("REMUX_ALLOW_NESTED").is_none()
    {
        eprintln!(
            "remux: refusing to launch inside an existing remux session.\n\
             Detach first, use a different terminal, or set REMUX_ALLOW_NESTED=1 to override."
        );
        std::process::exit(1);
    }

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
                .recv_skip_views()
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
                        let _ = client.recv_skip_views().await?;
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
            let _ = client.recv_skip_views().await?;
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
                .recv_skip_views()
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
            match client.recv_skip_views().await? {
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
        Some(Commands::Stop) => {
            log::debug!("cmd: stop server");
            stop_server().await?;
        }
        Some(Commands::Restart) => {
            log::debug!("cmd: restart server");
            let _ = stop_server().await?;
            ensure_server_running().await?;
            println!("Server restarted.");
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

/// Gracefully stop the running server via SIGTERM.
///
/// Returns `Ok(true)` if a running server was stopped, `Ok(false)` if none was
/// running (or only stale files existed). The SIGTERM triggers the server's
/// existing graceful path, which saves session state before removing the socket
/// and pid files.
async fn stop_server() -> Result<bool> {
    let sock = socket_path();
    let pid_file = daemon::pid_path();

    if !sock.exists() && !pid_file.exists() {
        println!("No server running.");
        return Ok(false);
    }

    // Read and parse the PID. Without a valid pid file we can't signal a live
    // server, and we must not force-remove a live socket out from under it.
    let pid = match std::fs::read_to_string(&pid_file) {
        Ok(contents) => match contents.trim().parse::<i32>() {
            Ok(pid) => pid,
            Err(_) => {
                println!(
                    "Server socket exists but pid file is unreadable; cannot signal the server."
                );
                return Ok(false);
            }
        },
        Err(_) => {
            println!("Server socket exists but pid file is missing; cannot signal the server.");
            return Ok(false);
        }
    };

    // Send SIGTERM to trigger the graceful save-and-exit path.
    match nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    ) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => {
            // No such process: the pid file is stale. Clean up leftover files.
            let _ = std::fs::remove_file(&sock);
            let _ = std::fs::remove_file(&pid_file);
            println!("No server running (removed stale files).");
            return Ok(false);
        }
        Err(e) => {
            return Err(e).context("sending SIGTERM to server");
        }
    }

    // Poll for the socket to disappear (the server removes it on shutdown).
    for _ in 0..50 {
        if !sock.exists() {
            println!("Server stopped.");
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    println!("Warning: server did not exit within 5s.");
    Ok(false)
}

// ---------------------------------------------------------------------------
// Client event loop
// ---------------------------------------------------------------------------

/// Connect any remotes configured with `auto_connect = true` at startup.
///
/// Runs before raw mode so SSH prompts are visible. A failed auto-connect is
/// non-fatal: it logs a warning and continues so client startup is never
/// aborted by an unreachable remote. Names are visited in sorted order for
/// stable output.
async fn connect_auto_remotes(mgr: &mut ConnectionManager, config: &Config) {
    let mut names: Vec<&String> = config
        .remotes
        .iter()
        .filter(|(_, rc)| rc.auto_connect)
        .map(|(name, _)| name)
        .collect();
    names.sort();

    for name in names {
        log::info!("auto-connecting remote '{name}'");
        match mgr.connect_remote(name).await {
            Ok(()) => log::info!("auto-connect to remote '{name}' succeeded"),
            Err(e) => {
                log::warn!("auto-connect to remote '{name}' failed: {e}");
                eprintln!("remux: auto-connect to remote '{name}' failed: {e}");
            }
        }
    }
}

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
    // Auto-connect any remotes flagged `auto_connect` before entering raw mode,
    // so SSH host-key/password prompts render normally.
    connect_auto_remotes(mgr, config).await;

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

/// Re-render whichever transient overlay is currently active on top of the
/// freshly-painted base frame. Extracted from the (previously triplicated)
/// FullRender/RenderDiff/ScrollRender arms so the View compositor (PaneContent
/// arm) can reuse the exact same overlay layering. The caller paints the base
/// frame and flushes; this only lays the overlays between those two steps.
///
/// Note the deliberate asymmetry preserved from the original arms: the
/// which-key popup is drawn at the loop's cached `cols`/`rows` while every other
/// overlay re-queries `terminal::size()`. Kept verbatim to avoid changing
/// behavior during the extraction.
#[allow(clippy::too_many_arguments)]
fn relay_overlays(
    renderer: &mut Renderer,
    input: &InputHandler,
    whichkey: &WhichKeyPopup,
    theme: &crate::config::theme::Theme,
    which_key_position: &crate::config::WhichKeyPosition,
    viewport_top: usize,
    focused_pane_rect: Option<&crate::protocol::PaneRect>,
    cols: u16,
    rows: u16,
) -> Result<()> {
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
            RenameTarget::NewView => "New View",
            RenameTarget::ViewRename => "Rename View",
        };
        let (c, r) = crossterm::terminal::size()?;
        renderer.render_rename_popup(&overlay.buffer, target_str, c, r)?;
    }
    // Re-render command palette on top if active
    else if let Some(ref palette) = input.command_palette {
        let (c, r) = crossterm::terminal::size()?;
        let draw_cmds = palette.render(c, r, theme);
        renderer.render_command_palette_overlay(&draw_cmds)?;
    }
    // Re-render search prompt and highlights on top if in search mode
    else if let Some(ref ss) = input.search_state {
        let query = ss.confirmed_query.as_deref().unwrap_or(&ss.query_buffer);
        let match_info = if ss.matches.is_empty() {
            None
        } else {
            Some((ss.current_match, ss.matches.len()))
        };
        let (c, r) = crossterm::terminal::size()?;
        renderer.render_search_highlight(
            &ss.matches,
            ss.current_match,
            query.len(),
            viewport_top,
            focused_pane_rect,
            theme,
        )?;
        renderer.render_search_prompt(query, ss.phase, match_info, c, r)?;
    }
    // Re-render session switch overlay on top if active
    else if let Some(ref ss) = input.session_switch {
        let (c, r) = crossterm::terminal::size()?;
        let draw_cmds = ss.render(c, r, theme);
        renderer.render_whichkey_overlay(&draw_cmds)?;
    }
    // Re-render view picker overlay on top if active
    else if let Some(ref vp) = input.view_picker {
        let (c, r) = crossterm::terminal::size()?;
        let draw_cmds = vp.render(c, r, theme);
        renderer.render_whichkey_overlay(&draw_cmds)?;
    }
    // Re-render folder select overlay on top if active
    else if let Some(ref fs) = input.folder_select {
        let (c, r) = crossterm::terminal::size()?;
        let draw_cmds = fs.render(c, r, theme);
        renderer.render_whichkey_overlay(&draw_cmds)?;
    }
    // Re-render session manager on top if active
    else if let Some(ref sm) = input.session_manager {
        let (c, r) = crossterm::terminal::size()?;
        let draw_cmds = sm.render(c, r, theme);
        renderer.render_whichkey_overlay(&draw_cmds)?;
    }
    // Re-render popup on top if visible
    else if whichkey.visible {
        let commands = whichkey.render(cols, rows, theme, which_key_position.clone());
        renderer.render_whichkey_overlay(&commands)?;
    }
    Ok(())
}

/// Flip a border style, exactly as the server's `ToggleStyle` handler flips a
/// session's. Used to advance the client-local view border style
/// (`view_border_style` in [`run_client_loop`]) whenever `ToggleStyle` runs --
/// in a view or in a normal tab -- so `Ctrl-a g` reads as a single toggle and a
/// view opens in the style the user last chose.
fn toggled_border_style(style: &crate::config::BorderStyle) -> crate::config::BorderStyle {
    match style {
        crate::config::BorderStyle::ZellijStyle => crate::config::BorderStyle::TmuxStyle,
        crate::config::BorderStyle::TmuxStyle => crate::config::BorderStyle::ZellijStyle,
    }
}

/// Composite the given active View into a full-screen frame and paint it,
/// re-laying any active overlay on top. Used by every code path that changes
/// what a live view shows (a fresh `PaneContent` snapshot, focus movement,
/// layout change, add/remove cell, terminal resize).
///
/// The focused cell's source cursor is shown at its mapped position (or hidden
/// when unavailable). The bottom row is reserved for a client-side status bar
/// (view name, focused cell title, input mode, layout name); cells are laid out
/// above it so they never overwrite it.
///
/// `border_style` is the client-local view border style (see
/// `view_border_style` in [`run_client_loop`]): the cells are framed with it
/// exactly as a normal tab's panes are framed with the session's style.
#[allow(clippy::too_many_arguments)]
fn paint_view(
    renderer: &mut Renderer,
    view: &crate::client::view::ClientView,
    input: &InputHandler,
    whichkey: &WhichKeyPopup,
    theme: &crate::config::theme::Theme,
    compositor_theme: &crate::config::theme::CompositorTheme,
    border_style: &crate::config::BorderStyle,
    which_key_position: &crate::config::WhichKeyPosition,
    viewport_top: usize,
    focused_pane_rect: Option<&crate::protocol::PaneRect>,
) -> Result<()> {
    let (c, r) = crossterm::terminal::size()?;
    let area = crate::server::layout::Rect {
        x: 0,
        y: 0,
        width: c,
        height: r,
    };
    // Draw the client-side status bar on the reserved bottom row, mirroring the
    // normal (server) bar's left/right layout with the same theme colors.
    let mode = match input.mode {
        Mode::Normal => "NORMAL",
        Mode::Command => "COMMAND",
        Mode::Visual => "VISUAL",
        Mode::CommandPalette => "PALETTE",
        Mode::Search => "SEARCH",
        Mode::SessionManager => "SESSION_MANAGER",
    };
    let mut composed =
        crate::client::view::composite(view, area, compositor_theme, mode, border_style);
    let cell_title = view
        .cells
        .get(view.focused)
        .and_then(|c| c.title.as_deref());
    // Mirror a normal tab's zoom marker (`format!("{} Z", name)` in the server
    // status bar): append ` Z` to the view name while a cell is zoomed.
    let view_name = if view.zoomed {
        format!("{} Z", view.name)
    } else {
        view.name.clone()
    };
    crate::client::view::draw_status_bar(
        &mut composed,
        area,
        mode,
        &view_name,
        cell_title,
        view.layout_name(),
        compositor_theme,
    );
    // Show the terminal cursor at the focused cell's source cursor position (if
    // visible and in view); unfocused cells and hidden/clipped cursors leave it
    // off. This lets a mirrored interactive app (vim, claude) show a live cursor.
    let (cur_x, cur_y, cur_vis) =
        match crate::client::view::focused_cursor(view, area, border_style) {
            Some((x, y)) => (x, y, true),
            None => (0, 0, false),
        };
    renderer.render_full(&composed, cur_x, cur_y, cur_vis, 0)?;
    relay_overlays(
        renderer,
        input,
        whichkey,
        theme,
        which_key_position,
        viewport_top,
        focused_pane_rect,
        c,
        r,
    )?;
    renderer.flush()?;
    Ok(())
}

/// Subscribe every cell of `view` to its source pane, sizing each subscription
/// to the cell's interior (the box border steals one row/column on each side).
/// Idempotent: re-calling updates each subscription on the server, so it doubles
/// as the "cells changed" / "focus moved" re-subscribe. Cells whose interior
/// collapses to zero in either axis are skipped.
///
/// A cell sizes its pane to itself: every cell the current layout actually
/// SHOWS carries a real size demand (`size_demand = true`), so its source pane
/// reflows to the cell's interior whether or not it is focused. A pane added to
/// a view therefore fits the space it is given -- the earlier focus-only rule
/// left every other cell's pane at its home session's allotment, which a
/// reflowing shell hid but a full-screen app (neovim in a quarter-height pane)
/// exposed as a tiny render inside a big cell.
///
/// Two cells impose NO demand:
/// - a cell HIDDEN by the current layout (Monocle's unfocused cells, and the
///   non-zoomed cells while the view is zoomed): it is still subscribed -- at
///   the full cell area, so a cell that has just been focused releases its old
///   size clamp instead of staying pinned to it forever -- but a cell nobody
///   sees must not clamp the pane to a size nothing is drawing;
/// - a SESSION-VISIBLE cell: its pane is driven full-size by its real session
///   and the cell paints the "Active in session" placeholder, so it must not
///   shrink it (the server force-ignores such demands too, see
///   `recompute_pane_size`).
///
/// Several viewers of one pane still fold via min-across-subscribers on the
/// server, so the pane ends up small enough for every cell showing it.
///
/// Views are SHARED, so a cell can name a remote *this* terminal has never
/// connected (another terminal composed it) -- there is then no transport to
/// subscribe over. Such a cell is lazily dialed in the BACKGROUND
/// ([`ConnectionManager::begin_connect_remote`], never awaited here so the TUI
/// stays responsive) and meanwhile labelled with the honest reason via
/// [`ViewCell::unavailable`]; the `Incoming::RemoteDialed` arm re-subscribes once
/// the dial lands. Without this the cell sat on `waiting…` forever.
async fn subscribe_view_cells(
    mgr: &mut ConnectionManager,
    view: &mut crate::client::view::ClientView,
    border_style: &crate::config::BorderStyle,
) -> Result<()> {
    let (c, r) = crossterm::terminal::size()?;
    let area = crate::server::layout::Rect {
        x: 0,
        y: 0,
        width: c,
        height: r,
    };
    let inner = crate::client::view::cells_area(area);
    let rects = crate::client::view::cell_rects(view, area);
    for (i, cell) in view.cells.iter_mut().enumerate() {
        // The source pane is gone (the server reported `PaneExited`). Nothing can
        // ever stream again, so skip it: re-subscribing would only make the
        // server repeat the event, and the cell keeps its honest `pane closed`
        // label instead of flickering back to `waiting…`.
        if cell.exited {
            continue;
        }
        // Visible cells subscribe at their rect's interior. Cells hidden by the
        // current layout (e.g. Monocle's unfocused cells) still get a
        // subscription -- at the full cell area with NO size demand -- so a
        // cell that was just focused releases its old size clamp instead of
        // staying pinned to the old cell size forever.
        {
            // `None` here IS "hidden by the current layout": `cell_rects` places
            // only the cells the layout draws.
            let placed = rects.get(i).copied().flatten();
            let rect = placed.unwrap_or(inner);
            // The demanded size is the cell's CONTENT region, which depends on
            // the border style (zellij loses a row/column to each border edge,
            // tmux is edge-to-edge). Sharing `cell_content_size` with the
            // compositor keeps the reflowed pane exactly the size that is drawn,
            // instead of leaving two blank columns in tmux style.
            let (cols, rows) = crate::client::view::cell_content_size(rect, border_style);
            if cols > 0 && rows > 0 {
                // No transport for this cell's server: start/observe a lazy dial
                // and label the cell honestly instead of subscribing into the
                // void. Returning early keeps `unavailable` set for exactly as
                // long as the cell really is unreachable.
                if let Some(reason) = reach_conn(mgr, &cell.conn) {
                    log::info!(
                        "view: cell pane {} on {:?} unreachable: {reason}",
                        cell.pane_id,
                        cell.conn
                    );
                    cell.unavailable = Some(reason);
                    continue;
                }
                // Best-effort: a torn-down connection must not abort the whole
                // subscribe pass (nor exit the client). Log and move on -- the
                // cell simply never receives snapshots and, on the next
                // keystroke/close, is marked disconnected.
                if let Err(e) = mgr
                    .send(
                        &cell.conn,
                        ClientMessage::SubscribePane {
                            pane_id: cell.pane_id,
                            cols,
                            rows,
                            // Every cell the layout SHOWS sizes its pane to
                            // itself, focused or not -- that is what makes a pane
                            // added to a view fit the space it is given. A hidden
                            // cell (`placed.is_none()`) demands nothing: nothing
                            // draws it, so it must not clamp the pane. Nor does a
                            // session-visible cell: that pane is driven full-size
                            // by its real session and the cell shows the "Active in
                            // session" placeholder (matches the server, which
                            // ignores the demand for such a pane anyway).
                            size_demand: placed.is_some() && !cell.is_session_visible(),
                        },
                    )
                    .await
                {
                    log::warn!(
                        "view: SubscribePane to {:?} pane {} failed: {e}",
                        cell.conn,
                        cell.pane_id
                    );
                } else {
                    // The subscription is on its way: whatever made the cell
                    // unreachable is over.
                    cell.unavailable = None;
                }
            }
        }
    }
    Ok(())
}

/// Whether `conn` can be subscribed over right now, and if not, the short label
/// a view cell on it should show ([`ViewCell::unavailable`]).
///
/// `None` means "reachable, go ahead". Otherwise a remote this terminal has not
/// connected is dialed in the BACKGROUND (never awaited: an SSH dial takes
/// seconds and this runs on the event loop) and reported as `connecting to x…`;
/// a remote whose dial already failed, or one absent from this terminal's
/// config, reports `not connected: x`. The local connection is always reachable
/// -- losing it exits the client.
fn reach_conn(mgr: &mut ConnectionManager, conn: &ConnId) -> Option<String> {
    let name = match conn {
        ConnId::Local => return None,
        ConnId::Remote(name) => name,
    };
    match mgr.remote_state(name) {
        RemoteState::Connected => None,
        // Idle and configured: kick off the dial. `begin_connect_remote` is a
        // no-op for an unknown remote, which is exactly the `not connected` case.
        RemoteState::NotConnected if mgr.begin_connect_remote(name) => {
            Some(format!("connecting to {name}…"))
        }
        RemoteState::Connecting => Some(format!("connecting to {name}…")),
        _ => Some(format!("not connected: {name}")),
    }
}

/// Unsubscribe every cell of `view` from its source pane (view leave / cell
/// removal). Best-effort per cell.
async fn unsubscribe_view_cells(
    mgr: &mut ConnectionManager,
    view: &crate::client::view::ClientView,
) -> Result<()> {
    for cell in &view.cells {
        // Best-effort per cell: a gone connection must not abort the leave/close
        // path or exit the client.
        if let Err(e) = mgr
            .send(
                &cell.conn,
                ClientMessage::UnsubscribePane {
                    pane_id: cell.pane_id,
                },
            )
            .await
        {
            log::warn!(
                "view: UnsubscribePane to {:?} pane {} failed: {e}",
                cell.conn,
                cell.pane_id
            );
        }
    }
    Ok(())
}

/// Map the client's [`ConnId`] to the wire [`ConnDescriptor`] carried in a
/// view-management intent's cell list. This is the intent-side half of the
/// descriptor mapping; the reverse (`ConnDescriptor → ConnId`, used when a
/// `ViewInfo` is synced into the cache) is
/// [`conn_from_descriptor`](crate::client::view::conn_from_descriptor).
fn descriptor_of_conn(conn: &ConnId) -> ConnDescriptor {
    match conn {
        ConnId::Local => ConnDescriptor::Local,
        ConnId::Remote(name) => ConnDescriptor::Remote(name.clone()),
    }
}

/// Enter (start displaying) the view at cache index `target_idx`, mirroring the
/// steps every enter path shares: leave any *other* currently-active view
/// (unsubscribe its cells), record the active index + id, `Detach` the
/// foreground session so its panes don't self-count as session-visible (bug4,
/// commit 8af74aa), then subscribe the target's cells and paint it.
///
/// Phase 2: membership/layout/focus/zoom already live in `views[target_idx]`
/// (synced from the server's `ViewList`); this only wires up *display* +
/// subscriptions, which are per-terminal.
#[allow(clippy::too_many_arguments)]
async fn enter_view(
    mgr: &mut ConnectionManager,
    views: &mut [crate::client::view::ClientView],
    active_view: &mut Option<usize>,
    active_view_id: &mut Option<ViewId>,
    target_idx: usize,
    current_attached: &Option<(ConnId, String)>,
    renderer: &mut Renderer,
    input: &InputHandler,
    whichkey: &WhichKeyPopup,
    theme: &crate::config::theme::Theme,
    compositor_theme: &crate::config::theme::CompositorTheme,
    border_style: &crate::config::BorderStyle,
    which_key_position: &crate::config::WhichKeyPosition,
    viewport_top: usize,
    focused_pane_rect: Option<&crate::protocol::PaneRect>,
) -> Result<()> {
    if let Some(av) = *active_view {
        if av != target_idx {
            // Bounds-guard: `active_view` is always re-resolved against the
            // current `views` before this runs, but never index blindly.
            if let Some(v) = views.get(av) {
                unsubscribe_view_cells(mgr, v).await?;
            }
        }
    }
    *active_view = Some(target_idx);
    *active_view_id = Some(views[target_idx].id);
    // bug4: detach the foreground session so a cell aliasing one of its
    // active-tab panes streams content instead of showing the "Active in
    // session" placeholder. Guarded on a known session so the leave-to-session
    // exit path can always re-attach.
    if current_attached.is_some() {
        mgr.send_foreground(ClientMessage::Detach).await?;
    }
    let (c, r) = crossterm::terminal::size()?;
    renderer.resize(c, r);
    subscribe_view_cells(mgr, &mut views[target_idx], border_style).await?;
    paint_view(
        renderer,
        &views[target_idx],
        input,
        whichkey,
        theme,
        compositor_theme,
        border_style,
        which_key_position,
        viewport_top,
        focused_pane_rect,
    )?;
    Ok(())
}

/// Route raw bytes to the focused cell of `view`, addressed by pane identity
/// (never to the foreground: a client showing a view is detached, so anything
/// sent to the foreground is dropped by the server).
///
/// Cells that have `exited`, are `disconnected`, or are `is_session_visible()`
/// take no input: the first two have no pane left to write to, and the third
/// paints a placeholder while its real session drives the pane.
///
/// Crash-safe by contract: a failed send (a torn-down remote writer) is NOT
/// propagated -- an input event must never exit the client. The cell is marked
/// disconnected instead and `true` is returned, meaning "the view changed, the
/// caller must repaint". `label` names the input path in the warning it logs.
async fn send_to_focused_cell(
    mgr: &mut ConnectionManager,
    view: &mut crate::client::view::ClientView,
    data: Vec<u8>,
    label: &str,
) -> bool {
    let focused = view.focused;
    let target = view
        .cells
        .get(focused)
        .filter(|c| !c.exited && !c.disconnected && !c.is_session_visible())
        .map(|c| (c.conn.clone(), c.pane_id));
    let (conn, pane_id) = match target {
        Some(t) => t,
        None => return false,
    };
    if let Err(e) = mgr
        .send(&conn, ClientMessage::InputToPane { pane_id, data })
        .await
    {
        log::warn!(
            "view: {label} InputToPane to {:?} pane {} failed: {e}; marking cell disconnected",
            conn,
            pane_id
        );
        if let Some(cell) = view.cells.get_mut(focused) {
            cell.disconnected = true;
        }
        return true;
    }
    false
}

/// While a view is active, decide what a `RemuxCommand` does and apply it,
/// returning `true` when the command was consumed (the caller should `continue`
/// and forward nothing) or `false` when it must fall through to the normal path.
///
/// This is the single interception point used by both `InputAction::Execute`
/// and each command of `InputAction::ExecuteChain`. It fixes a crash: without
/// it, a structural command like `PaneClose` (`Prefix p x`) was forwarded to the
/// masked foreground server, which is not the pane the user sees.
///
/// Phase 2: view-management commands no longer mutate the local view. They send
/// an **intent** to the local server (which owns the shared-view registry); the
/// resulting `ViewList` broadcast drives the repaint on every terminal. The one
/// local decision made here is directional focus: which cell is the neighbor
/// depends on this terminal's geometry, so it is resolved locally (a clone probe
/// of `move_focus`) into a target `cell_id` and sent as `ViewSetFocus`.
///
/// - `PaneFocus{Left,Right,Up,Down}` -> resolve the neighbor cell locally, send
///   `ViewSetFocus { cell_id }`.
/// - `LayoutNext` -> `ViewCycleLayout`.
/// - `Resize{Left,Right,Up,Down}` -> `ViewResizeCell { dir, amount }`.
/// - `PaneMove{Left,Right,Up,Down}` -> `ViewMoveCell { dir }`.
/// - `PaneToggleZoom` -> `ViewToggleZoom`.
/// - `PaneClose` -> eject the focused cell: `ViewRemoveCell { cell_id }` (the
///   real pane is untouched; the resync unsubscribes it).
/// - `ToggleStyle` -> repaint the view's cells in the (already flipped, see
///   `view_border_style`) border style. Client-local; nothing is forwarded.
/// - `SessionDetach` -> NOT consumed (returns `false`) so the caller's detach
///   path runs and the client exits.
/// - `SendKey(bytes)` -> route the raw bytes to the focused cell's pane by
///   identity (best-effort), never to the foreground (per-terminal, unchanged).
/// - every other structural / server command -> NO-OP: consumed, nothing sent.
#[allow(clippy::too_many_arguments)]
async fn handle_view_command(
    cmd: &RemuxCommand,
    mgr: &mut ConnectionManager,
    views: &mut [crate::client::view::ClientView],
    av: usize,
    renderer: &mut Renderer,
    input: &InputHandler,
    whichkey: &mut WhichKeyPopup,
    theme: &crate::config::theme::Theme,
    compositor_theme: &crate::config::theme::CompositorTheme,
    border_style: &crate::config::BorderStyle,
    which_key_position: &crate::config::WhichKeyPosition,
    viewport_top: usize,
    focused_pane_rect: Option<&crate::protocol::PaneRect>,
    cols: u16,
    rows: u16,
) -> Result<bool> {
    use crate::server::layout::FocusDirection;

    // Common "hide the which-key popup" step shared by the consuming branches.
    macro_rules! hide_whichkey {
        () => {
            if whichkey.visible {
                whichkey.hide();
                renderer.clear_overlay(cols, rows)?;
            }
        };
    }
    // Repaint the (still-stale) active view so hiding the popup leaves no
    // artifacts; the ensuing `ViewList` from the intent repaints authoritatively.
    macro_rules! repaint {
        () => {
            paint_view(
                renderer,
                &views[av],
                input,
                whichkey,
                theme,
                compositor_theme,
                border_style,
                which_key_position,
                viewport_top,
                focused_pane_rect,
            )?;
        };
    }

    let view_id = views[av].id;

    let dir = match cmd {
        RemuxCommand::PaneFocusLeft => Some(FocusDirection::Left),
        RemuxCommand::PaneFocusRight => Some(FocusDirection::Right),
        RemuxCommand::PaneFocusUp => Some(FocusDirection::Up),
        RemuxCommand::PaneFocusDown => Some(FocusDirection::Down),
        _ => None,
    };
    if let Some(dir) = dir {
        hide_whichkey!();
        // Resolve the neighbor cell with THIS terminal's geometry (a clone probe
        // so the cache isn't mutated), then intent the shared focus change. The
        // repaint arrives via the resulting `ViewList`.
        let cells = crate::client::view::cells_area(crate::server::layout::Rect {
            x: 0,
            y: 0,
            width: cols,
            height: rows,
        });
        let mut probe = views[av].clone();
        if probe.move_focus(dir, cells) {
            let cell_id = probe.focused_id();
            mgr.send(
                &ConnId::Local,
                ClientMessage::ViewSetFocus {
                    id: view_id,
                    cell_id,
                },
            )
            .await?;
        }
        repaint!();
        return Ok(true);
    }

    match cmd {
        RemuxCommand::LayoutNext => {
            hide_whichkey!();
            mgr.send(
                &ConnId::Local,
                ClientMessage::ViewCycleLayout { id: view_id },
            )
            .await?;
            repaint!();
            Ok(true)
        }
        RemuxCommand::ResizeLeft(amount)
        | RemuxCommand::ResizeRight(amount)
        | RemuxCommand::ResizeUp(amount)
        | RemuxCommand::ResizeDown(amount) => {
            hide_whichkey!();
            let dir = match cmd {
                RemuxCommand::ResizeLeft(_) => FocusDirection::Left,
                RemuxCommand::ResizeRight(_) => FocusDirection::Right,
                RemuxCommand::ResizeUp(_) => FocusDirection::Up,
                RemuxCommand::ResizeDown(_) => FocusDirection::Down,
                _ => unreachable!(),
            };
            if !views[av].cells.is_empty() {
                let cell_id = views[av].focused_id();
                mgr.send(
                    &ConnId::Local,
                    ClientMessage::ViewResizeCell {
                        id: view_id,
                        cell_id,
                        dir,
                        amount: *amount,
                    },
                )
                .await?;
            }
            repaint!();
            Ok(true)
        }
        RemuxCommand::PaneMoveLeft
        | RemuxCommand::PaneMoveRight
        | RemuxCommand::PaneMoveUp
        | RemuxCommand::PaneMoveDown => {
            hide_whichkey!();
            let dir = match cmd {
                RemuxCommand::PaneMoveLeft => FocusDirection::Left,
                RemuxCommand::PaneMoveRight => FocusDirection::Right,
                RemuxCommand::PaneMoveUp => FocusDirection::Up,
                RemuxCommand::PaneMoveDown => FocusDirection::Down,
                _ => unreachable!(),
            };
            if !views[av].cells.is_empty() {
                let cell_id = views[av].focused_id();
                mgr.send(
                    &ConnId::Local,
                    ClientMessage::ViewMoveCell {
                        id: view_id,
                        cell_id,
                        dir,
                    },
                )
                .await?;
            }
            repaint!();
            Ok(true)
        }
        RemuxCommand::PaneToggleZoom => {
            hide_whichkey!();
            if !views[av].cells.is_empty() {
                mgr.send(
                    &ConnId::Local,
                    ClientMessage::ViewToggleZoom { id: view_id },
                )
                .await?;
            }
            repaint!();
            Ok(true)
        }
        RemuxCommand::ToggleStyle => {
            // Border style is a display preference, not a structural change, so
            // it is NOT a no-op in a view: the caller has already flipped the
            // client-local `view_border_style` (so `border_style` here is the NEW
            // one and `repaint!` shows it) and the cells are simply redrawn in it.
            //
            // Deliberately NOT forwarded to the server. `Session::border_style`
            // is PER-SESSION state, and a view's cells alias panes across several
            // sessions and machines that each own their own style -- so there is
            // no single "the session" whose style a view could mirror. (A forward
            // would also be dead anyway: entering a view detaches the foreground
            // session, and the server drops any command from a client with no
            // attached session.) The view's frame is client-local, exactly as a
            // view's geometry already is.
            hide_whichkey!();
            // The interior a cell paints changed size (a border was gained or
            // lost on every edge), so re-demand the new content size.
            subscribe_view_cells(mgr, &mut views[av], border_style).await?;
            repaint!();
            Ok(true)
        }
        RemuxCommand::PaneClose => {
            // Eject the focused cell from the view -- do NOT close the real pane.
            // The resync (ViewList) removes the cell and unsubscribes it.
            hide_whichkey!();
            if !views[av].cells.is_empty() {
                let cell_id = views[av].focused_id();
                mgr.send(
                    &ConnId::Local,
                    ClientMessage::ViewRemoveCell {
                        id: view_id,
                        cell_id,
                    },
                )
                .await?;
            }
            repaint!();
            Ok(true)
        }
        // Let the caller's detach path run (it exits the client).
        RemuxCommand::SessionDetach => Ok(false),
        RemuxCommand::SendKey(bytes) => {
            // Route raw bytes to the focused cell by identity (never the
            // foreground). Best-effort, matching the crash-safe input path.
            // A session-visible cell shows a placeholder, not the pane's live
            // content, so raw input is suppressed (the pane is driven by its real
            // session); view-management shortcuts still act on the view because
            // they are separate commands handled above, not `SendKey`.
            if send_to_focused_cell(mgr, &mut views[av], bytes.clone(), "SendKey").await {
                repaint!();
            }
            Ok(true)
        }
        // Every other structural / server command is a NO-OP while in a view:
        // consume it, forward nothing. Hide the which-key popup (as the normal
        // path does) and repaint so the view is left clean.
        _ => {
            if whichkey.visible {
                whichkey.hide();
                renderer.clear_overlay(cols, rows)?;
                repaint!();
            }
            Ok(true)
        }
    }
}

/// Tear down the active view (if any) before switching the foreground
/// session/server. A live view's `paint_view` overrides the screen and masks
/// the switched-to session, so every switch entry point must leave the view
/// first: unsubscribe its cells and clear `active_view`. A no-op when no view is
/// active.
///
/// Unlike the `ViewClose` handler, this deliberately does NOT resize/re-render:
/// the switch that follows sends its own `Resize` (via `switch_to_server`) or
/// `Attach`, and the resulting server `FullRender` repaints the screen.
async fn leave_active_view(
    mgr: &mut ConnectionManager,
    views: &[crate::client::view::ClientView],
    active_view: &mut Option<usize>,
    active_view_id: &mut Option<ViewId>,
) -> Result<()> {
    if let Some(av) = *active_view {
        unsubscribe_view_cells(mgr, &views[av]).await?;
        *active_view = None;
        *active_view_id = None;
    }
    Ok(())
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
    let session_manager_bindings = config.session_manager_bindings();
    let mut input = InputHandler::new(
        keybindings,
        leader_key,
        shortcut_bindings,
        session_manager_bindings,
    );
    let (cols, rows) = crossterm::terminal::size()?;
    log::debug!("run_client_loop: terminal size={}x{}", cols, rows);
    let mut renderer = Renderer::new(cols, rows);
    let mut whichkey = WhichKeyPopup::new();
    let mut theme = config.theme();
    // `CompositorTheme` (CellColor) mirror of the client `Theme`, used to draw
    // the client-side view status bar with the same colors as the normal bar.
    let mut compositor_theme = config.compositor_theme();
    let mut which_key_position = config.appearance.which_key_position.clone();
    // Border style used to frame a VIEW's cells. This is client-local, and has
    // to be: `Session::border_style` is PER-SESSION server state, while a view's
    // cells alias panes across several sessions and machines at once, so there
    // is no single session style a view could inherit or mirror. (A view's
    // *geometry* is already per-terminal for the same reason, and staying
    // client-local needs no `PROTOCOL_VERSION` bump.)
    //
    // Seeded from `appearance.border_style` -- the same value each session is
    // seeded with, so a view opens in the style the user configured -- and
    // flipped by `ToggleStyle` wherever it executes, in a view or in a normal
    // tab. That last part is why `Ctrl-a g` reads as one toggle: whichever
    // surface the user pressed it on, the next view they open shows the style
    // they last chose.
    let mut view_border_style = config.appearance.border_style.clone();

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
    // Whether the client is currently scrolled back (server owns the actual
    // offset). Read from the render frames' `scroll_offset`, NEVER from
    // `viewport_top`: the latter is an absolute line index, so at maximum scroll
    // (the first line of history on the top row) it is exactly 0 -- identical to
    // the live tail. Deriving this flag from it made the client believe it was
    // unscrolled at precisely the maximum, and every path that returns to the
    // live tail (typing, Escape to Normal, leaving Visual, cancelling Search) is
    // gated on this flag, so none of them fired and the session looked dead.
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

    // Set when a foreground-remote session was deleted (e.g. its last tab was
    // closed) and we've asked the local server for its session list to decide
    // whether to fall back to a local session or exit. Consumed by the
    // `SessionList` handler (gated on `src == Local`) to complete the fallback.
    let mut pending_local_fallback = false;

    // Set when a foreground *local* session was deleted while the session
    // manager is open. It arms a one-shot check: when the LOCAL server's tree
    // reply comes back (gated on `src == Local`), the client exits iff no local
    // sessions remain — mirroring the manager-closed "last session closed ->
    // exit" behavior. Only a genuine deletion arms it, so merely opening or
    // refreshing the manager (including with a remote connected) never exits.
    let mut pending_manager_exit_check = false;

    // Track the current and previously-attached (server, session) for the
    // "last session" toggle (Alt-o). Seeded with the initial local session as
    // `(ConnId::Local, name)` when known; `previous` starts empty so the first
    // Alt-o before any switch is a no-op.
    let mut current_attached: Option<(ConnId, String)> = last_local_session
        .as_ref()
        .map(|name| (ConnId::Local, name.clone()));
    let mut previous_attached: Option<(ConnId, String)> = None;

    // Views: virtual tabs whose cells alias real panes and are composited from
    // streamed `PaneContent` snapshots. Phase 2: the shared-view registry is
    // SERVER-OWNED; `views` is a per-terminal CACHE rebuilt from every `ViewList`
    // broadcast (see the `ViewList` arm), and all view-management actions send
    // intents to the local server rather than mutating this list.
    //
    // `active_view` is the cache index this terminal is DISPLAYING (None = the
    // normal server-driven frame is shown); `active_view_id` is the same view's
    // stable [`ViewId`], used to re-resolve `active_view` after a `ViewList`
    // rebuild shuffles indices and to detect a displayed view being deleted.
    let mut views: Vec<crate::client::view::ClientView> = Vec::new();
    let mut active_view: Option<usize> = None;
    let mut active_view_id: Option<ViewId> = None;
    // Seed the cache from the startup `ViewList` the server pushed on connect
    // (captured during the handshake). Nothing is displayed yet, so this only
    // populates the cache — enough for the switcher to list pre-existing shared
    // views immediately.
    if let Some(infos) = mgr.take_initial_view_infos() {
        views = infos
            .iter()
            .map(|info| crate::client::view::ClientView::from_info(info, None))
            .collect();
    }
    // A view this terminal created (via `w n` / `w a`→new) and should enter as
    // soon as the resulting `ViewList` carries it. Set from the `ViewCreated`
    // ack (creator-only), consumed by the `ViewList` sync.
    let mut pending_enter_view: Option<ViewId> = None;
    // Cells to add to a just-created view once its `ViewCreated` ack arrives (the
    // `w a`/compose → new-view path: create, then add, then enter).
    let mut pending_add_cells: Option<Vec<(ConnDescriptor, crate::protocol::PaneId)>> = None;
    // Pending "add focused pane to a view" flow (see the `w a` handler): we ask
    // the foreground server for its session tree, resolve the focused pane into
    // `pending_panes` when the tree arrives, and complete the add once the user
    // picks a target view in the picker overlay.
    let mut pending_view_add = false;
    // Panes waiting to be added to a view once the user picks a target in the
    // picker. The `w a` path resolves exactly one (the focused pane, via a tree
    // round-trip); the session-manager "add to view" path fills it directly with
    // the marked/highlighted panes.
    let mut pending_panes: Vec<(ConnId, crate::protocol::PaneId)> = Vec::new();

    // Mouse drag state for coalescing drag events (~60fps throttle).
    let mut drag_start: Option<(u16, u16)> = None;
    // An in-progress drag-selection inside a VIEW cell: the cell's connection,
    // its source pane, and the press point in that pane's own content
    // coordinates. Kept separate from `drag_start` (screen coordinates in the
    // foreground session) because a client displaying a view is detached: the
    // gesture is routed by pane identity, and it stays bound to the cell it
    // started in even as the pointer leaves that cell.
    let mut view_drag: Option<(ConnId, protocol::PaneId, u16, u16)> = None;
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
                        // Inside a view, arrow/nav keys must be encoded with the
                        // FOCUSED cell's DECCKM (application-cursor-keys) state, not
                        // the foreground session's -- so arrows/Home/End work inside
                        // a mirrored interactive app. Override the field just around
                        // `handle_key` (which does the encoding), then restore the
                        // foreground value so leaving the view is unaffected.
                        let saved_ack = input.application_cursor_keys;
                        if let Some(av) = active_view {
                            let v = &views[av];
                            if let Some(ack) = v
                                .cells
                                .get(v.focused)
                                .and_then(|c| c.snapshot.as_ref())
                                .map(|s| s.application_cursor_keys)
                            {
                                input.application_cursor_keys = ack;
                            }
                        }
                        let action = input.handle_key(key);
                        input.application_cursor_keys = saved_ack;

                        // If rename popup was dismissed, clear overlay
                        if was_renaming && input.rename_overlay.is_none() && !matches!(action, InputAction::RenameUpdate(_)) {
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.clear_overlay(c, r)?;
                            renderer.flush()?;
                        }
                        // If the palette was dismissed by this key, tear its
                        // overlay down here -- once, for EVERY action arm.
                        // Confirming a palette entry runs the same action chain
                        // a keybinding does, so Enter can now land on any arm
                        // (a rename prompt, the pane picker, the switcher);
                        // clearing per-arm would leave the palette painted over
                        // whichever arm was forgotten.
                        if was_in_palette && input.command_palette.is_none() {
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.clear_command_palette_overlay(c, r)?;
                            renderer.flush()?;
                        }
                        match action {
                            InputAction::SendToPty(data) => {
                                log::debug!("input: SendToPty {} bytes", data.len());
                                if let Some(av) = active_view {
                                    // In view mode, route keystrokes to the focused
                                    // cell's pane by identity (independent of the
                                    // server's foreground focus) -- see
                                    // `send_to_focused_cell` for which cells accept
                                    // input and why a failed send is swallowed.
                                    if send_to_focused_cell(mgr, &mut views[av], data, "key")
                                        .await
                                    {
                                        paint_view(
                                            &mut renderer,
                                            &views[av],
                                            &input,
                                            &whichkey,
                                            &theme,
                                            &compositor_theme,
                                            &view_border_style,
                                            &which_key_position,
                                            viewport_top,
                                            focused_pane_rect.as_ref(),
                                        )?;
                                    }
                                } else {
                                    // Reset scroll when user types (sends PTY input)
                                    if is_scrolled {
                                        scroll_offset = 0;
                                        is_scrolled = false;
                                        mgr.send_foreground(ClientMessage::ScrollReset).await?;
                                    }
                                    mgr.send_foreground(ClientMessage::Input { data }).await?;
                                }
                            }
                            InputAction::Execute(cmd) => {
                                log::debug!("input: Execute cmd={:?}", cmd);
                                // Border style is a display preference honored by
                                // BOTH renderers: flip the client-local view style
                                // here -- before the view interception, so the
                                // repaint below already uses it -- whether a view
                                // or a normal tab is on screen. Doing it in one
                                // place per action arm is what stops the two from
                                // drifting apart.
                                if matches!(cmd, RemuxCommand::ToggleStyle) {
                                    view_border_style = toggled_border_style(&view_border_style);
                                }
                                // (The palette overlay was already torn down
                                // above, before this arm and so before the view
                                // interception -- a command run from `:` while
                                // in a view is consumed by the interception and
                                // would otherwise leave the palette painted.)
                                // While a view is active, structural pane/tab
                                // commands are intercepted client-side (focus
                                // move, layout cycle, eject cell, or no-op) and
                                // NEVER forwarded to the (masked) foreground
                                // server -- forwarding e.g. PaneClose there would
                                // crash the client. `SessionDetach` is the one
                                // command allowed to fall through (to exit).
                                if let Some(av) = active_view {
                                    if handle_view_command(
                                        &cmd,
                                        mgr,
                                        &mut views,
                                        av,
                                        &mut renderer,
                                        &input,
                                        &mut whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                        cols,
                                        rows,
                                    )
                                    .await?
                                    {
                                        continue;
                                    }
                                }
                                // Hide which-key popup when executing a command
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
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
                                    // See the `Execute` arm: keep the client-local
                                    // view border style in step with the server's.
                                    // `Prefix g` arrives here (a chain of
                                    // `ToggleStyle` + `EnterNormal`).
                                    if matches!(cmd, RemuxCommand::ToggleStyle) {
                                        view_border_style =
                                            toggled_border_style(&view_border_style);
                                    }
                                    // While a view is active, intercept structural
                                    // commands client-side (focus / layout / eject
                                    // / no-op); nothing structural is forwarded to
                                    // the masked foreground server. See the
                                    // single-command `Execute` path above.
                                    if let Some(av) = active_view {
                                        if handle_view_command(
                                            &cmd,
                                            mgr,
                                            &mut views,
                                            av,
                                            &mut renderer,
                                            &input,
                                            &mut whichkey,
                                            &theme,
                                            &compositor_theme,
                                            &view_border_style,
                                            &which_key_position,
                                            viewport_top,
                                            focused_pane_rect.as_ref(),
                                            cols,
                                            rows,
                                        )
                                        .await?
                                        {
                                            continue;
                                        }
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
                                    // A VIEW is scoped to its FOCUSED CELL, from
                                    // the view's own geometry. `focused_pane_rect`
                                    // describes the server's foreground session,
                                    // and entering a view detaches -- so in a view
                                    // it is a stale rect from a layout that is not
                                    // on screen, which is what put the cursor in
                                    // the wrong cell at a nonsensical offset.
                                    let view_scope = active_view.and_then(|av| {
                                        let (c, r) = crossterm::terminal::size().ok()?;
                                        crate::client::view::focused_cell_visual_scope(
                                            &views[av],
                                            crate::server::layout::Rect {
                                                x: 0,
                                                y: 0,
                                                width: c,
                                                height: r,
                                            },
                                            &view_border_style,
                                        )
                                    });
                                    if let (Some((ox, oy, vc, vr, cc, cr)), Some(vs)) =
                                        (view_scope, input.visual_state.as_mut())
                                    {
                                        vs.pane_offset_x = ox;
                                        vs.pane_offset_y = oy;
                                        vs.visible_cols = vc as usize;
                                        vs.visible_rows = vr as usize;
                                        vs.cursor_col = cc as usize;
                                        vs.cursor_row = cr as usize;
                                        // Pin the scrollback extent to what the cell
                                        // paints. The client has no line count for a
                                        // cell's source pane (`RequestScrollbackInfo`
                                        // is session-scoped, hence dead while
                                        // detached), so leaving `total_lines` larger
                                        // would let `k` scroll the copy view into
                                        // coordinates the extraction cannot address.
                                        // Visual mode in a view therefore covers the
                                        // cell's VISIBLE content; the mouse wheel
                                        // still pages the cell through its history.
                                        vs.scroll_offset = 0;
                                        vs.total_lines = vs.visible_rows;
                                    } else if let Some(ref mut vs) = input.visual_state {
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
                                    // Request scrollback info to get accurate
                                    // total_lines -- session-scoped, so skipped in a
                                    // view (nothing is attached to answer it, and the
                                    // reply would un-pin the cell's total_lines).
                                    if active_view.is_none() {
                                        mgr.send_foreground(ClientMessage::RequestScrollbackInfo).await?;
                                    }
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
                                // A view has to repaint itself. In a normal tab
                                // the `ModeChanged` above makes the server send a
                                // frame carrying the new mode, and painting that
                                // frame is what redraws the status bar and lays
                                // the visual overlay back on top. A client in a
                                // view is detached, so no such frame ever comes:
                                // without this, entering Visual left the status
                                // bar reading [NORMAL] and drew no copy cursor at
                                // all.
                                if let Some(av) = active_view {
                                    paint_view(
                                        &mut renderer,
                                        &views[av],
                                        &input,
                                        &whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
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
                                    Some(RenameTarget::NewView) => "New View",
                                    Some(RenameTarget::ViewRename) => "Rename View",
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
                            InputAction::ShowWhichKey {
                                label,
                                entries,
                                shortcuts,
                            } => {
                                let (c, r) = crossterm::terminal::size()?;
                                whichkey.show(label, entries, shortcuts);
                                renderer.clear_overlay(c, r)?;
                                let commands = whichkey.render(c, r, &theme, which_key_position.clone());
                                renderer.render_whichkey_overlay(&commands)?;
                                renderer.flush()?;
                            }
                            InputAction::ExecuteAndShowWhichKey {
                                command,
                                label,
                                entries,
                                shortcuts,
                            } => {
                                log::debug!("input: ExecuteAndShowWhichKey cmd={:?}", command);
                                // Sticky-group leaves (e.g. the `p R h/j/k/l`
                                // resize group) arrive here, NOT via `Execute`.
                                // While a view is active they must be intercepted
                                // client-side just like `Execute`/`ExecuteChain`
                                // -- otherwise a resize would be forwarded to the
                                // masked foreground server (resizing the wrong
                                // panes) instead of the view's cells.
                                let (c, r) = crossterm::terminal::size()?;
                                // See the `Execute` arm: keep the client-local view
                                // border style in step with the server's.
                                if matches!(command, RemuxCommand::ToggleStyle) {
                                    view_border_style = toggled_border_style(&view_border_style);
                                }
                                let mut consumed = false;
                                if let Some(av) = active_view {
                                    consumed = handle_view_command(
                                        &command,
                                        mgr,
                                        &mut views,
                                        av,
                                        &mut renderer,
                                        &input,
                                        &mut whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                        cols,
                                        rows,
                                    )
                                    .await?;
                                }
                                if !consumed {
                                    // Not in a view: send to the foreground server.
                                    // These are never SessionDetach/SendKey and the
                                    // mode stays Command, so no ModeChanged is needed.
                                    mgr.send_foreground(ClientMessage::Command(command)).await?;
                                }
                                // Keep the which-key popup open so the user can
                                // keep resizing (re-shown over the repainted view
                                // when a view consumed the command).
                                whichkey.show(label, entries, shortcuts);
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
                                    RenameTarget::NewView => "New View",
                                    RenameTarget::ViewRename => "Rename View",
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
                                        // Entered only when the match is OUTSIDE the
                                        // visible window, so we are scrolling away
                                        // from the tail. Not `target_vt > 0`: a match
                                        // near the top of history centers at
                                        // viewport_top 0, which is maximum scroll, not
                                        // the live tail (see `is_scrolled`'s comment).
                                        is_scrolled = true;
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
                                            // Same as the visual match-nav arm: the
                                            // match was off-screen, so this leaves the
                                            // live tail even when it centers at
                                            // viewport_top 0.
                                            is_scrolled = true;
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
                                // (This command usually arrives from the palette;
                                // its overlay was torn down above.)
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
                                        // A live view masks the switched-to session:
                                        // tear it down before handing off the screen.
                                        leave_active_view(mgr, &views, &mut active_view, &mut active_view_id).await?;
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
                                        // A live view masks the switched-to session:
                                        // tear it down before handing off the screen.
                                        leave_active_view(mgr, &views, &mut active_view, &mut active_view_id).await?;
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
                                        // A live view masks the switched-to session:
                                        // tear it down before handing off the screen.
                                        leave_active_view(mgr, &views, &mut active_view, &mut active_view_id).await?;
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
                                    // Structural edits target the server carried by the
                                    // action (the selected node's connection), so the
                                    // session manager can edit folders/sessions/tabs on
                                    // any connected server, Local or remote.
                                    SessionManagerAction::CreateFolder { server, name } => {
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::FolderNew(name.clone()))).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::CreateSession { server, name, folder } => {
                                        mgr.send(&server, ClientMessage::CreateSession {
                                            name: name.clone(),
                                            folder: folder.clone(),
                                        }).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::MoveSession { server, session, folder } => {
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::FolderMoveSession {
                                            session: session.clone(),
                                            folder: folder.clone(),
                                        })).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::DeleteSession { server, name } => {
                                        mgr.send(&server, ClientMessage::KillSession { name: name.clone() }).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::DeleteFolder { server, name } => {
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::FolderDelete(name.clone()))).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    // Resurrecting a dormant session is Local-only: it
                                    // materializes the saved session on the local server,
                                    // then refreshes the tree so it moves from Saved to live.
                                    SessionManagerAction::ResurrectSession(name) => {
                                        mgr.send(&ConnId::Local, ClientMessage::ResurrectSession { name: name.clone() }).await?;
                                        mgr.send(&ConnId::Local, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::CloseTab { server, session, tab_index } => {
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::TabCloseByIndex {
                                            session: session.clone(),
                                            tab_index,
                                        })).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::TabNew { server, session } => {
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::TabNewInSession {
                                            session: session.clone(),
                                        })).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::TabMove { server, session, tab_index, delta } => {
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::TabMoveByIndex {
                                            session: session.clone(),
                                            tab_index,
                                            delta,
                                        })).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::PaneNew { server, session, tab_index } => {
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::PaneNewInTab {
                                            session: session.clone(),
                                            tab_index,
                                        })).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::PaneClose { server, session, pane_id } => {
                                        mgr.send(&server, ClientMessage::Command(RemuxCommand::PaneCloseById {
                                            session: session.clone(),
                                            pane_id,
                                        })).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::Rename { server, kind, new_name } => {
                                        use crate::client::session_manager::RenameKind;
                                        let cmd = match kind {
                                            RenameKind::Session { name } => RemuxCommand::SessionRenameByName {
                                                old: name.clone(),
                                                new: new_name.clone(),
                                            },
                                            RenameKind::Folder { name } => RemuxCommand::FolderRename {
                                                old: name.clone(),
                                                new: new_name.clone(),
                                            },
                                            RenameKind::Tab { session, tab_index } => RemuxCommand::TabRenameByIndex {
                                                session: session.clone(),
                                                tab_index,
                                                name: new_name.clone(),
                                            },
                                            RenameKind::Pane { session, pane_id } => RemuxCommand::PaneRenameById {
                                                session: session.clone(),
                                                pane_id,
                                                name: new_name.clone(),
                                            },
                                        };
                                        mgr.send(&server, ClientMessage::Command(cmd)).await?;
                                        mgr.send(&server, ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::RefreshTree => {
                                        for id in mgr.connected_ids() {
                                            mgr.send(&id, ClientMessage::ListSessionTree).await?;
                                        }
                                    }
                                    SessionManagerAction::AddToView { panes } => {
                                        // Close the manager and open the view picker
                                        // seeded with the marked/highlighted panes.
                                        // Explicitly clear `pending_view_add` so a
                                        // stale `w a` tree round-trip can't also push
                                        // into `pending_panes` behind our back.
                                        input.session_manager = None;
                                        pending_view_add = false;
                                        pending_panes = panes;
                                        input.mode = Mode::Command;
                                        let names: Vec<String> = views.iter().map(|v| v.name.clone()).collect();
                                        input.view_picker = Some(crate::client::input::ViewPickerOverlay::new(names));
                                        if let Some(ref vp) = input.view_picker {
                                            let (c, r) = crossterm::terminal::size()?;
                                            renderer.clear_overlay(c, r)?;
                                            let draw_cmds = vp.render(c, r, &theme);
                                            renderer.render_whichkey_overlay(&draw_cmds)?;
                                            renderer.flush()?;
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
                                // Fold the client-only views into the switcher:
                                // seed their names ONCE here (they are not
                                // populated asynchronously) so they occupy stable
                                // leading indices while session trees merge in.
                                let mut overlay = SessionSwitchOverlay::new();
                                overlay.set_views(views.iter().map(|v| v.name.clone()).collect());
                                input.session_switch = Some(overlay);
                                // Render immediately so the Views section is
                                // visible before any session tree arrives.
                                // View-aware: over a live view, composite it and
                                // re-lay the switcher on top via `paint_view`.
                                let (c, r) = crossterm::terminal::size()?;
                                if let Some(av) = active_view {
                                    paint_view(
                                        &mut renderer,
                                        &views[av],
                                        &input,
                                        &whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                    )?;
                                } else if let Some(ref ss) = input.session_switch {
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = ss.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                    renderer.flush()?;
                                }
                                mgr.send_foreground(ClientMessage::ModeChanged { mode: "COMMAND".to_string() }).await?;
                            }
                            InputAction::SessionSwitchUpdate => {
                                let (c, r) = crossterm::terminal::size()?;
                                // View-aware: over a live view, `paint_view` both
                                // composites the view and re-lays the switcher.
                                if let Some(av) = active_view {
                                    paint_view(
                                        &mut renderer,
                                        &views[av],
                                        &input,
                                        &whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                    )?;
                                } else {
                                    if let Some(ref ss) = input.session_switch {
                                        renderer.clear_overlay(c, r)?;
                                        let draw_cmds = ss.render(c, r, &theme);
                                        renderer.render_whichkey_overlay(&draw_cmds)?;
                                    }
                                    renderer.flush()?;
                                }
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
                                // A live view masks the switched-to session: tear it
                                // down first so the switch actually shows.
                                leave_active_view(mgr, &views, &mut active_view, &mut active_view_id).await?;
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
                                    // A live view masks the switched-to session: tear
                                    // it down before handing off the screen.
                                    leave_active_view(mgr, &views, &mut active_view, &mut active_view_id).await?;
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
                            // -- Views --------------------------------------------------
                            InputAction::NewView(ref name) => {
                                // Create a new shared view on the local server.
                                // We enter it once its `ViewCreated` ack + the
                                // ensuing `ViewList` arrive (see `pending_enter_view`).
                                let name = if name.trim().is_empty() {
                                    format!("View {}", views.len() + 1)
                                } else {
                                    name.clone()
                                };
                                mgr.send(&ConnId::Local, ClientMessage::ViewCreate { name })
                                    .await?;
                            }
                            InputAction::ViewRename(ref name) => {
                                // Rename the active view for EVERY terminal: intent
                                // the local server; the `ViewList` broadcast applies
                                // it. Empty name = no-op (dismissed without typing).
                                let name = name.trim();
                                if let (Some(av), false) = (active_view, name.is_empty()) {
                                    let id = views[av].id;
                                    mgr.send(
                                        &ConnId::Local,
                                        ClientMessage::ViewRename {
                                            id,
                                            name: name.to_string(),
                                        },
                                    )
                                    .await?;
                                }
                            }
                            InputAction::ViewActivate { index } => {
                                // Selected from the switcher's Views section. The
                                // input handler already cleared the switcher overlay
                                // and reset mode to Normal. Resolve the switcher
                                // index against the (possibly-since-rebuilt) cache and
                                // enter that view by id.
                                let (c, r) = crossterm::terminal::size()?;
                                if index < views.len() {
                                    enter_view(
                                        mgr,
                                        &mut views,
                                        &mut active_view,
                                        &mut active_view_id,
                                        index,
                                        &current_attached,
                                        &mut renderer,
                                        &input,
                                        &whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                    )
                                    .await?;
                                } else {
                                    // Stale index (view removed under us): just
                                    // clear the switcher popup.
                                    renderer.clear_overlay(c, r)?;
                                    renderer.flush()?;
                                }
                            }
                            InputAction::ViewAddPaneOpen => {
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // Resolve the current focused pane asynchronously
                                // (the client doesn't track it): ask the foreground
                                // server for its tree, then open the picker. The
                                // tree round-trip beats the human pressing Enter, so
                                // `pending_panes` is resolved before ViewPickerConfirm.
                                pending_view_add = true;
                                pending_panes.clear();
                                mgr.send_foreground(ClientMessage::ListSessionTree).await?;
                                let names: Vec<String> = views.iter().map(|v| v.name.clone()).collect();
                                input.mode = Mode::Command;
                                input.view_picker = Some(crate::client::input::ViewPickerOverlay::new(names));
                                if let Some(ref vp) = input.view_picker {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = vp.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                    renderer.flush()?;
                                }
                            }
                            InputAction::ViewPickerUpdate => {
                                if let Some(ref vp) = input.view_picker {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = vp.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                    renderer.flush()?;
                                }
                            }
                            InputAction::ViewPickerClose => {
                                pending_view_add = false;
                                pending_panes.clear();
                                let (c, r) = crossterm::terminal::size()?;
                                if let Some(av) = active_view {
                                    paint_view(
                                        &mut renderer,
                                        &views[av],
                                        &input,
                                        &whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                    )?;
                                } else {
                                    renderer.clear_overlay(c, r)?;
                                    renderer.flush()?;
                                }
                            }
                            InputAction::ViewPickerConfirm { view } => {
                                let (c, r) = crossterm::terminal::size()?;
                                pending_view_add = false;
                                let panes = std::mem::take(&mut pending_panes);
                                if panes.is_empty() {
                                    // Nothing resolved (no tree / no focused pane, or
                                    // an empty session-manager selection). Warn and
                                    // repaint whatever was underneath.
                                    log::warn!("view: add-pane confirmed but no panes resolved; ignoring");
                                    if let Some(av) = active_view {
                                        paint_view(
                                            &mut renderer,
                                            &views[av],
                                            &input,
                                            &whichkey,
                                            &theme,
                                            &compositor_theme,
                                            &view_border_style,
                                            &which_key_position,
                                            viewport_top,
                                            focused_pane_rect.as_ref(),
                                        )?;
                                    } else {
                                        renderer.clear_overlay(c, r)?;
                                        renderer.flush()?;
                                    }
                                } else {
                                    // Map each marked pane's connection (Local or a
                                    // named remote) to its wire descriptor for the
                                    // cell list.
                                    let cells: Vec<(ConnDescriptor, crate::protocol::PaneId)> = panes
                                        .iter()
                                        .map(|(conn, pid)| (descriptor_of_conn(conn), *pid))
                                        .collect();
                                    match view {
                                        // Existing view: dedupe against its current
                                        // cells (by conn+pane), then intent an add.
                                        // The resync repaints if it is displayed.
                                        Some(i) if i < views.len() => {
                                            let id = views[i].id;
                                            let to_add: Vec<(ConnDescriptor, crate::protocol::PaneId)> =
                                                panes
                                                    .iter()
                                                    .filter(|(conn, pid)| {
                                                        !views[i].cells.iter().any(|cell| {
                                                            cell.conn == *conn && cell.pane_id == *pid
                                                        })
                                                    })
                                                    .map(|(conn, pid)| {
                                                        (descriptor_of_conn(conn), *pid)
                                                    })
                                                    .collect();
                                            if !to_add.is_empty() {
                                                mgr.send(
                                                    &ConnId::Local,
                                                    ClientMessage::ViewAddCells {
                                                        id,
                                                        cells: to_add,
                                                    },
                                                )
                                                .await?;
                                            }
                                            // Repaint underneath the (now dismissed)
                                            // picker; the ViewList resync follows.
                                            if let Some(av) = active_view {
                                                paint_view(
                                                    &mut renderer,
                                                    &views[av],
                                                    &input,
                                                    &whichkey,
                                                    &theme,
                                                    &compositor_theme,
                                                    &view_border_style,
                                                    &which_key_position,
                                                    viewport_top,
                                                    focused_pane_rect.as_ref(),
                                                )?;
                                            } else {
                                                renderer.clear_overlay(c, r)?;
                                                renderer.flush()?;
                                            }
                                        }
                                        // New view: create it, stash the cells to add
                                        // + enter once the `ViewCreated` ack arrives.
                                        _ => {
                                            let name = format!("View {}", views.len() + 1);
                                            pending_add_cells = Some(cells);
                                            mgr.send(
                                                &ConnId::Local,
                                                ClientMessage::ViewCreate { name },
                                            )
                                            .await?;
                                            renderer.clear_overlay(c, r)?;
                                            renderer.flush()?;
                                        }
                                    }
                                }
                            }
                            InputAction::ViewRemovePane => {
                                // `w x` ejects the focused cell -- the SAME
                                // crash-safe eject as `Prefix p x` in a view, so
                                // route it through the one helper (best-effort
                                // unsubscribe: a dead source must not exit the
                                // client).
                                if let Some(av) = active_view {
                                    handle_view_command(
                                        &RemuxCommand::PaneClose,
                                        mgr,
                                        &mut views,
                                        av,
                                        &mut renderer,
                                        &input,
                                        &mut whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                        cols,
                                        rows,
                                    )
                                    .await?;
                                }
                            }
                            InputAction::ViewLayoutNext => {
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                if let Some(av) = active_view {
                                    // Cycle the SHARED layout: intent the local server;
                                    // the `ViewList` resync repaints every terminal.
                                    let id = views[av].id;
                                    mgr.send(&ConnId::Local, ClientMessage::ViewCycleLayout { id })
                                        .await?;
                                    // Repaint the current (stale) view so hiding the
                                    // popup leaves no artifacts; the resync follows.
                                    paint_view(
                                        &mut renderer,
                                        &views[av],
                                        &input,
                                        &whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                    )?;
                                }
                            }
                            InputAction::ViewClose => {
                                // Leave-to-session for THIS terminal only (the shared
                                // view persists for other terminals — this is not a
                                // ViewDelete). Unsubscribe our cells and hand the
                                // screen back to the foreground session via Resize.
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                if let Some(av) = active_view {
                                    unsubscribe_view_cells(mgr, &views[av]).await?;
                                    active_view = None;
                                    active_view_id = None;
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.resize(c, r);
                                    // Entering the view detached the foreground session
                                    // (bug4 fix); re-attach it now so the server resumes
                                    // rendering it. `handle_resize` is a no-op for an
                                    // unattached client, so without this re-attach the
                                    // screen would stay blank on view exit.
                                    if let Some((_, session)) = current_attached.clone() {
                                        mgr.send_foreground(ClientMessage::Attach {
                                            session_name: session,
                                        }).await?;
                                    }
                                    mgr.send_foreground(ClientMessage::Resize { cols: c, rows: r }).await?;
                                }
                            }
                            InputAction::ViewDelete => {
                                // Delete the active view for EVERYONE. We do NOT touch
                                // `active_view` locally: the resulting `ViewList`
                                // (view gone) drives this terminal's leave-to-session
                                // via the deleted-view branch, and every other terminal
                                // displaying it does the same.
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                if let Some(av) = active_view {
                                    let id = views[av].id;
                                    mgr.send(&ConnId::Local, ClientMessage::ViewDelete { id })
                                        .await?;
                                }
                            }
                            InputAction::None => {}
                        }
                    }
                    Some(Ok(crossterm::event::Event::Mouse(mouse))) => {
                        // A live view owns the screen: mouse events target its
                        // cells, not the (masked) foreground session. Everything
                        // here is routed by CELL GEOMETRY and PANE IDENTITY --
                        // never through the session-scoped `MouseClick`/
                        // `MouseDrag`/`MouseScroll` path below, whose server
                        // handlers resolve the target from the client's attached
                        // session. Entering a view detaches, so those would find
                        // no session and silently do nothing.
                        if let Some(av) = active_view {
                            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                                let (c, r) = crossterm::terminal::size()?;
                                let area = crate::server::layout::Rect {
                                    x: 0,
                                    y: 0,
                                    width: c,
                                    height: r,
                                };
                                if let Some(idx) = crate::client::view::cell_at(
                                    &views[av],
                                    area,
                                    mouse.column,
                                    mouse.row,
                                    &view_border_style,
                                ) {
                                    // Clicking a cell focuses it for EVERY terminal:
                                    // intent the shared focus change; the resync
                                    // repaints. (No local mutation.)
                                    if views[av].focused != idx {
                                        if let Some(cell_id) =
                                            views[av].cells.get(idx).map(|c| c.id)
                                        {
                                            let id = views[av].id;
                                            mgr.send(
                                                &ConnId::Local,
                                                ClientMessage::ViewSetFocus { id, cell_id },
                                            )
                                            .await?;
                                        }
                                    }
                                }
                                // A press on a cell's CONTENT (not its border or
                                // the Monocle strip) also anchors a drag-selection
                                // there. The click itself clears any previous
                                // selection on that pane, so a plain click
                                // dismisses a highlight exactly like it does in a
                                // normal pane.
                                view_drag = None;
                                if let Some((idx, cx, cy)) =
                                    crate::client::view::cell_content_at(
                                        &views[av],
                                        area,
                                        mouse.column,
                                        mouse.row,
                                        &view_border_style,
                                    )
                                {
                                    if let Some(cell) = views[av].cells.get(idx) {
                                        view_drag =
                                            Some((cell.conn.clone(), cell.pane_id, cx, cy));
                                        mgr.send(
                                            &cell.conn,
                                            ClientMessage::MouseClick {
                                                x: cx,
                                                y: cy,
                                                pane_id: Some(cell.pane_id),
                                                release: false,
                                            },
                                        )
                                        .await?;
                                    }
                                }
                            } else if let MouseEventKind::Up(MouseButton::Left) = mouse.kind {
                                // Release commits the gesture: the server yanks
                                // over the absolute selection range and honors
                                // `mouse_auto_yank`, replying with
                                // `CopyToClipboard` just as it does for a pane.
                                if let Some((conn, pane_id, sx, sy)) = view_drag.take() {
                                    let (c, r) = crossterm::terminal::size()?;
                                    let area = crate::server::layout::Rect {
                                        x: 0,
                                        y: 0,
                                        width: c,
                                        height: r,
                                    };
                                    let idx = views[av]
                                        .cells
                                        .iter()
                                        .position(|cell| cell.pane_id == pane_id);
                                    if let Some((ex, ey)) = idx.and_then(|i| {
                                        crate::client::view::cell_content_pos(
                                            &views[av],
                                            area,
                                            i,
                                            mouse.column,
                                            mouse.row,
                                            &view_border_style,
                                        )
                                    }) {
                                        if (ex, ey) != (sx, sy) {
                                            mgr.send(
                                                &conn,
                                                ClientMessage::MouseDrag {
                                                    start_x: sx,
                                                    start_y: sy,
                                                    end_x: ex,
                                                    end_y: ey,
                                                    is_final: true,
                                                    pane_id: Some(pane_id),
                                                },
                                            )
                                            .await?;
                                        } else {
                                            // Released where it began: a click,
                                            // not a selection -- so nothing is
                                            // yanked. Still tell the server, or a
                                            // gesture that wandered to a content
                                            // edge and came back would leave its
                                            // repeat timer armed and the cell
                                            // would keep scrolling after release.
                                            // `release` marks it as the button
                                            // coming UP, which is what a
                                            // mouse-tracking application needs to
                                            // see after the press it already got.
                                            mgr.send(
                                                &conn,
                                                ClientMessage::MouseClick {
                                                    x: ex,
                                                    y: ey,
                                                    pane_id: Some(pane_id),
                                                    release: true,
                                                },
                                            )
                                            .await?;
                                        }
                                    }
                                }
                            } else if let MouseEventKind::ScrollUp | MouseEventKind::ScrollDown =
                                mouse.kind
                            {
                                // The wheel scrolls the cell under the pointer (or the
                                // focused cell if the pointer is off any cell) through
                                // its source pane's scrollback, by identity -- NOT the
                                // masked foreground session. The server keeps a
                                // per-(client, pane) offset and streams a fresh
                                // `PaneContent` rendered at it.
                                let (c, r) = crossterm::terminal::size()?;
                                let area = crate::server::layout::Rect {
                                    x: 0,
                                    y: 0,
                                    width: c,
                                    height: r,
                                };
                                let target = crate::client::view::cell_at(
                                    &views[av],
                                    area,
                                    mouse.column,
                                    mouse.row,
                                    &view_border_style,
                                )
                                .unwrap_or(views[av].focused);
                                if let Some(cell) = views[av].cells.get(target) {
                                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                                    // The wheel position in the cell's content
                                    // coordinates: the server needs it to build a
                                    // mouse report when the pane's application has
                                    // tracking on. A pointer that is off the cell
                                    // (the focused-cell fallback above) has no
                                    // position of its own, so it reports the
                                    // top-left content cell.
                                    let (wx, wy) = crate::client::view::cell_content_pos(
                                        &views[av],
                                        area,
                                        target,
                                        mouse.column,
                                        mouse.row,
                                        &view_border_style,
                                    )
                                    .unwrap_or((0, 0));
                                    mgr.send(
                                        &cell.conn,
                                        ClientMessage::ScrollPane {
                                            pane_id: cell.pane_id,
                                            up,
                                            lines: 3,
                                            x: wx,
                                            y: wy,
                                        },
                                    )
                                    .await?;
                                }
                            } else if let MouseEventKind::Drag(MouseButton::Left) = mouse.kind {
                                // Extend the selection anchored by the press. The
                                // point is mapped into the ANCHOR cell's content
                                // coordinates and clamped there, so dragging out
                                // of the cell keeps growing that cell's selection
                                // instead of jumping to a neighbour -- and landing
                                // on its top/bottom content row is what the server
                                // turns into an edge auto-scroll step (which also
                                // extends the selection, since the anchor is held
                                // in eviction-stable absolute coordinates).
                                if let Some((conn, pane_id, sx, sy)) = view_drag.clone() {
                                    let now = Instant::now();
                                    if now.duration_since(last_drag_send) >= DRAG_THROTTLE {
                                        let (c, r) = crossterm::terminal::size()?;
                                        let area = crate::server::layout::Rect {
                                            x: 0,
                                            y: 0,
                                            width: c,
                                            height: r,
                                        };
                                        let idx = views[av]
                                            .cells
                                            .iter()
                                            .position(|cell| cell.pane_id == pane_id);
                                        if let Some((ex, ey)) = idx.and_then(|i| {
                                            crate::client::view::cell_content_pos(
                                                &views[av],
                                                area,
                                                i,
                                                mouse.column,
                                                mouse.row,
                                                &view_border_style,
                                            )
                                        }) {
                                            mgr.send(
                                                &conn,
                                                ClientMessage::MouseDrag {
                                                    start_x: sx,
                                                    start_y: sy,
                                                    end_x: ex,
                                                    end_y: ey,
                                                    is_final: false,
                                                    pane_id: Some(pane_id),
                                                },
                                            )
                                            .await?;
                                            last_drag_send = now;
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                log::debug!("mouse: click at ({}, {})", mouse.column, mouse.row);
                                drag_start = Some((mouse.column, mouse.row));
                                // Send click immediately.
                                mgr
                                    .send_foreground(ClientMessage::MouseClick {
                                        x: mouse.column,
                                        y: mouse.row,
                                        pane_id: None,
                                        release: false,
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
                                                pane_id: None,
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
                                                pane_id: None,
                                            })
                                            .await?;
                                    } else {
                                        // Released where it began: no drag to
                                        // finalize, but a mouse-tracking
                                        // application still needs the button-up
                                        // that follows the press it already got,
                                        // or it latches the button down. (Mirrors
                                        // the view path's release-click.)
                                        mgr
                                            .send_foreground(ClientMessage::MouseClick {
                                                x: mouse.column,
                                                y: mouse.row,
                                                pane_id: None,
                                                release: true,
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
                        // A live view owns the screen; recomposite it at the new
                        // size so it doesn't stay blank until the next snapshot.
                        // Every cell's rect changed with the terminal, so re-demand
                        // first: the cells' panes must follow the new cell sizes
                        // (the other geometry changes -- layout/resize/move/zoom --
                        // arrive as `ViewList` and re-subscribe there).
                        if let Some(av) = active_view {
                            subscribe_view_cells(mgr, &mut views[av], &view_border_style).await?;
                            paint_view(
                                &mut renderer,
                                &views[av],
                                &input,
                                &whichkey,
                                &theme,
                                &compositor_theme,
                                &view_border_style,
                                &which_key_position,
                                viewport_top,
                                focused_pane_rect.as_ref(),
                            )?;
                        }
                    }
                    Some(Ok(crossterm::event::Event::Paste(text))) => {
                        // Wrap pasted text in bracketed paste sequences.
                        let mut data = Vec::new();
                        data.extend_from_slice(b"\x1b[200~");
                        data.extend_from_slice(text.as_bytes());
                        data.extend_from_slice(b"\x1b[201~");
                        if let Some(av) = active_view {
                            // A client showing a view is DETACHED, so a paste sent
                            // to the foreground would be dropped by the server.
                            // Route it to the focused cell's pane by identity,
                            // exactly as a keystroke is.
                            if send_to_focused_cell(mgr, &mut views[av], data, "paste")
                                .await
                            {
                                paint_view(
                                    &mut renderer,
                                    &views[av],
                                    &input,
                                    &whichkey,
                                    &theme,
                                    &compositor_theme,
                                    &view_border_style,
                                    &which_key_position,
                                    viewport_top,
                                    focused_pane_rect.as_ref(),
                                )?;
                            }
                        } else {
                            mgr.send_foreground(ClientMessage::Input { data }).await?;
                        }
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
                    // A lazy dial started for a view cell on a remote this
                    // terminal had not connected (see `subscribe_view_cells`)
                    // finished. Adopt or fail it, then re-subscribe the displayed
                    // view so the cell starts streaming (or shows the honest
                    // `not connected` label the failed state now yields).
                    Incoming::RemoteDialed(name, result) => {
                        let connected = mgr.finish_remote_dial(&name, result);
                        log::debug!("srv: RemoteDialed '{name}' connected={connected}");
                        if let Some(av) = active_view {
                            subscribe_view_cells(mgr, &mut views[av], &view_border_style).await?;
                            paint_view(
                                &mut renderer,
                                &views[av],
                                &input,
                                &whichkey,
                                &theme,
                                &compositor_theme,
                                &view_border_style,
                                &which_key_position,
                                viewport_top,
                                focused_pane_rect.as_ref(),
                            )?;
                        }
                        continue;
                    }
                    Incoming::Closed(src) => {
                        log::debug!("srv: connection closed src={:?}", src);
                        // Mark every view cell (across ALL views) that aliases a
                        // pane on the dropped connection as disconnected. Done at
                        // the very top of the arm, before the several early
                        // returns/continues below, so no drop path skips it. If
                        // the active view was touched, repaint so the label shows.
                        let mut active_view_touched = false;
                        for (vi, view) in views.iter_mut().enumerate() {
                            for cell in view.cells.iter_mut() {
                                if cell.conn == src && !cell.disconnected {
                                    cell.disconnected = true;
                                    if active_view == Some(vi) {
                                        active_view_touched = true;
                                    }
                                }
                            }
                        }
                        if active_view_touched {
                            if let Some(av) = active_view {
                                paint_view(
                                    &mut renderer,
                                    &views[av],
                                    &input,
                                    &whichkey,
                                    &theme,
                                    &compositor_theme,
                                    &view_border_style,
                                    &which_key_position,
                                    viewport_top,
                                    focused_pane_rect.as_ref(),
                                )?;
                            }
                        }
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
                                        input.session_manager = Some(input.new_session_manager(None));
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
                    Some(ServerMessage::FullRender { cells, cursor_x, cursor_y, cursor_visible, cursor_style, focused_pane_rect: fpr, application_cursor_keys: ack, viewport_top: so, scroll_offset: srv_so }) => {
                        log::debug!("srv: FullRender rows={} cols={} cursor=({},{}) visible={} viewport_top={} scroll_offset={}",
                            cells.len(), if cells.is_empty() { 0 } else { cells[0].len() }, cursor_x, cursor_y, cursor_visible, so, srv_so);
                        focused_pane_rect = fpr;
                        input.application_cursor_keys = ack;
                        scroll_offset = so;
                        // Server render is authoritative for the viewport top;
                        // keep the dedicated highlight coordinate in sync.
                        viewport_top = so;
                        is_scrolled = srv_so > 0;
                        last_cursor_x = cursor_x;
                        last_cursor_y = cursor_y;
                        last_cursor_visible = cursor_visible;
                        // A View owns the screen while active: keep all the
                        // bookkeeping above (so state is fresh when the view
                        // closes) but skip painting the server's frame.
                        if active_view.is_none() {
                            renderer.render_full(&cells, cursor_x, cursor_y, cursor_visible, cursor_style)?;
                            relay_overlays(
                                &mut renderer,
                                &input,
                                &whichkey,
                                &theme,
                                &which_key_position,
                                viewport_top,
                                focused_pane_rect.as_ref(),
                                cols,
                                rows,
                            )?;
                            renderer.flush()?;
                        }
                    }
                    Some(ServerMessage::RenderDiff { changes, cursor_x, cursor_y, cursor_visible, cursor_style, focused_pane_rect: fpr, application_cursor_keys: ack, viewport_top: so, scroll_offset: srv_so }) => {
                        log::debug!("srv: RenderDiff changes={} cursor=({},{}) viewport_top={} scroll_offset={}", changes.len(), cursor_x, cursor_y, so, srv_so);
                        focused_pane_rect = fpr;
                        input.application_cursor_keys = ack;
                        scroll_offset = so;
                        // Server render is authoritative for the viewport top;
                        // keep the dedicated highlight coordinate in sync.
                        viewport_top = so;
                        is_scrolled = srv_so > 0;
                        last_cursor_x = cursor_x;
                        last_cursor_y = cursor_y;
                        last_cursor_visible = cursor_visible;
                        // See the FullRender arm: a View suppresses the paint.
                        if active_view.is_none() {
                            renderer.render_diff(&changes, cursor_x, cursor_y, cursor_visible, cursor_style)?;
                            relay_overlays(
                                &mut renderer,
                                &input,
                                &whichkey,
                                &theme,
                                &which_key_position,
                                viewport_top,
                                focused_pane_rect.as_ref(),
                                cols,
                                rows,
                            )?;
                            renderer.flush()?;
                        }
                    }
                    Some(ServerMessage::ScrollRender { pane_x, pane_y, pane_width, pane_height, delta, new_rows, cursor_x, cursor_y, cursor_visible, cursor_style, focused_pane_rect: fpr, application_cursor_keys: ack, viewport_top: so, scroll_offset: srv_so }) => {
                        log::debug!("srv: ScrollRender delta={} pane=({},{} {}x{}) viewport_top={} scroll_offset={}", delta, pane_x, pane_y, pane_width, pane_height, so, srv_so);
                        focused_pane_rect = fpr;
                        input.application_cursor_keys = ack;
                        scroll_offset = so;
                        // Server render is authoritative for the viewport top;
                        // keep the dedicated highlight coordinate in sync.
                        viewport_top = so;
                        is_scrolled = srv_so > 0;
                        last_cursor_x = cursor_x;
                        last_cursor_y = cursor_y;
                        last_cursor_visible = cursor_visible;
                        // See the FullRender arm: a View suppresses the paint.
                        if active_view.is_none() {
                            renderer.render_scroll(pane_x, pane_y, pane_width, pane_height, delta, &new_rows, cursor_x, cursor_y, cursor_visible, cursor_style)?;
                            relay_overlays(
                                &mut renderer,
                                &input,
                                &whichkey,
                                &theme,
                                &which_key_position,
                                viewport_top,
                                focused_pane_rect.as_ref(),
                                cols,
                                rows,
                            )?;
                            renderer.flush()?;
                        }
                    }
                    Some(ServerMessage::SessionList { sessions }) => {
                        log::debug!("received session list with {} sessions", sessions.len());
                        // Complete a pending local fallback: a foreground remote
                        // session was deleted and we asked the local server for its
                        // sessions to decide where to land.
                        if pending_local_fallback && src == ConnId::Local {
                            pending_local_fallback = false;
                            if sessions.is_empty() {
                                // No local sessions to fall back to -- exit.
                                break;
                            }
                            // Prefer the last local session we were on if it still
                            // exists; otherwise take the first available one.
                            let target = last_local_session
                                .as_ref()
                                .filter(|name| sessions.iter().any(|s| &s.name == *name))
                                .cloned()
                                .unwrap_or_else(|| sessions[0].name.clone());
                            // Standard switch dance (mirrors the session-manager
                            // SwitchSession path) targeting the local server.
                            input.session_manager = None;
                            input.mode = Mode::Normal;
                            let (c, r) = crossterm::terminal::size()?;
                            renderer.clear_overlay(c, r)?;
                            renderer.flush()?;
                            switch_to_server(mgr, &ConnId::Local, c, r).await?;
                            mgr.send(&ConnId::Local, ClientMessage::Attach {
                                session_name: target.clone(),
                            }).await?;
                            mgr.send(&ConnId::Local, ClientMessage::ModeChanged {
                                mode: "NORMAL".to_string(),
                            }).await?;
                            last_local_session = Some(target.clone());
                            record_switch(&mut current_attached, &mut previous_attached, ConnId::Local, target);
                        }
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
                        // Resolve a pending "add focused pane to a view" request
                        // BEFORE `folders`/`unfiled` are moved into the session
                        // manager below. `is_focused` is per-tab (every tab reports
                        // its focused pane), and this message carries no active-tab
                        // marker, so within the current session we take the first
                        // focused pane found (multi-tab active-tab disambiguation is
                        // a known limitation).
                        if pending_view_add && mgr.is_foreground(&src) {
                            let want = current_attached.as_ref().map(|(_, s)| s.clone());
                            let mut found: Option<crate::protocol::PaneId> = None;
                            'find: for s in folders
                                .iter()
                                .flat_map(|f| f.sessions.iter())
                                .chain(unfiled.iter())
                            {
                                let is_target = match &want {
                                    Some(name) => &s.name == name,
                                    None => s.is_current,
                                };
                                if !is_target {
                                    continue;
                                }
                                for tab in &s.tabs {
                                    for p in &tab.panes {
                                        if p.is_focused {
                                            found = Some(p.id);
                                            break 'find;
                                        }
                                    }
                                }
                            }
                            if let Some(pid) = found {
                                pending_panes.push((src.clone(), pid));
                            }
                            // Consumed: don't let a later tree re-resolve it.
                            pending_view_add = false;
                        }
                        // The session-switch popup aggregates every connected
                        // server's tree, so it accepts trees from ANY source
                        // (not just the foreground) and tags each with `src`,
                        // including the current session (marked, not filtered).
                        if input.session_switch.is_some() {
                            let mut sessions: Vec<(String, bool, Option<String>)> = Vec::new();
                            for f in &folders {
                                for s in &f.sessions {
                                    sessions.push((
                                        s.name.clone(),
                                        s.is_current,
                                        Some(f.name.clone()),
                                    ));
                                }
                            }
                            for s in &unfiled {
                                sessions.push((s.name.clone(), s.is_current, None));
                            }
                            // Replace this server's rows (a re-received tree for
                            // the same `src` overwrites rather than duplicates).
                            input.merge_session_switch(src.clone(), sessions);
                            // Render the popup. View-aware: over a live view,
                            // `paint_view` composites the view AND re-lays the
                            // switcher overlay (via `relay_overlays`), so the
                            // popup draws on top of the composite, not a stale
                            // server frame.
                            if let Some(av) = active_view {
                                paint_view(
                                    &mut renderer,
                                    &views[av],
                                    &input,
                                    &whichkey,
                                    &theme,
                                    &compositor_theme,
                                    &view_border_style,
                                    &which_key_position,
                                    viewport_top,
                                    focused_pane_rect.as_ref(),
                                )?;
                            } else if let Some(ref ss) = input.session_switch {
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
                            // Whether this is the LOCAL server's reply, and whether
                            // it reports zero sessions. Computed from the just-
                            // received tree (before it is moved into `update_tree`)
                            // and scoped to the local server, so a remote's (possibly
                            // collapsed/empty) subtree never counts. Dormant sessions
                            // are intentionally ignored — they never kept the client
                            // alive before either.
                            let is_local = matches!(src, ConnId::Local);
                            let local_now_empty = is_local
                                && unfiled.is_empty()
                                && folders.iter().all(|f| f.sessions.is_empty());
                            sm.update_tree(src, folders, unfiled, dormant);
                            // A plain tree refresh only updates and re-renders — it
                            // must never exit the client (an earlier version broke
                            // out here on an empty aggregate, which misfired when a
                            // remote reply was processed before the local one and
                            // silently exited the client on `Prefix x m`). The one
                            // exception is the last-local-session lifecycle: a
                            // foreground-local `SessionDeleted` arms
                            // `pending_manager_exit_check`, and only the ensuing LOCAL
                            // tree reply resolves it — exiting iff local now has no
                            // sessions and the foreground is still local. This is
                            // event-driven and order-independent (remote replies are
                            // ignored), so opening/refreshing the manager — even with
                            // a remote connected — can never trip it.
                            if pending_manager_exit_check && is_local {
                                pending_manager_exit_check = false;
                                if local_now_empty && mgr.is_foreground(&ConnId::Local) {
                                    input.session_manager = None;
                                    input.mode = Mode::Normal;
                                    break;
                                }
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
                        // A pane the SERVER reports dead is deliberately NOT
                        // foreground-scoped: a view cell can alias a pane on any
                        // connected server, and the whole point of the event is
                        // that the client cannot otherwise tell a dead pane from a
                        // quiet one -- a healthy connection to a server whose pane
                        // died never trips `disconnected`, so the cell sat on
                        // `waiting…` (or frozen content) forever and swallowed
                        // every keystroke.
                        if let crate::protocol::SessionEvent::PaneExited { pane_id, .. } = &event {
                            let mut active_touched = false;
                            for (vi, view) in views.iter_mut().enumerate() {
                                for cell in view.cells.iter_mut() {
                                    if cell.conn == src && cell.pane_id == *pane_id && !cell.exited {
                                        cell.exited = true;
                                        // Drop the last snapshot with it: keeping it
                                        // would keep painting stale content that
                                        // looks live.
                                        cell.snapshot = None;
                                        if active_view == Some(vi) {
                                            active_touched = true;
                                        }
                                    }
                                }
                            }
                            if active_touched {
                                if let Some(av) = active_view {
                                    paint_view(
                                        &mut renderer,
                                        &views[av],
                                        &input,
                                        &whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                    )?;
                                }
                            }
                        }
                        // A session created elsewhere (another terminal on this
                        // server, or any connected remote) must appear in an OPEN
                        // session manager: every tree refresh is event-driven with
                        // no timer, so without this the tree stayed stale until
                        // some unrelated action happened to refresh it. Symmetric
                        // with the `SessionDeleted` refresh below, minus the exit
                        // bookkeeping -- a creation can never be the last-session
                        // shutdown, so it must never arm `pending_manager_exit_check`.
                        if matches!(event, crate::protocol::SessionEvent::SessionCreated(_))
                            && input.session_manager.is_some()
                        {
                            for id in mgr.connected_ids() {
                                mgr.send(&id, ClientMessage::ListSessionTree).await?;
                            }
                        }
                        // Events are foreground-scoped: a background remote's
                        // SessionDeleted must not drive the local loop.
                        if mgr.is_foreground(&src)
                            && matches!(event, crate::protocol::SessionEvent::SessionDeleted(_))
                        {
                            // If session manager is open, refresh the tree instead
                            // of breaking out of the event loop. A foreground
                            // *local* deletion additionally arms a one-shot check
                            // so the refreshed LOCAL tree can exit iff it was the
                            // last local session (see `pending_manager_exit_check`).
                            if input.session_manager.is_some() {
                                if matches!(src, ConnId::Local) {
                                    pending_manager_exit_check = true;
                                }
                                for id in mgr.connected_ids() {
                                    mgr.send(&id, ClientMessage::ListSessionTree).await?;
                                }
                            } else if matches!(src, ConnId::Remote(_)) {
                                // A foreground *remote* session was deleted. Don't
                                // exit -- fall back to a local session instead. Ask
                                // the local server for its sessions; the reply is
                                // handled in the `SessionList` arm (gated on the
                                // flag + src == Local) to complete the switch.
                                //
                                // The standalone `attach-remote` flow has no local
                                // connection to fall back to -- exit gracefully
                                // rather than send to a connection that isn't there.
                                if mgr.connected_ids().contains(&ConnId::Local) {
                                    pending_local_fallback = true;
                                    mgr.send(&ConnId::Local, ClientMessage::ListSessions).await?;
                                } else {
                                    break;
                                }
                            } else {
                                // A foreground *local* session was deleted. The
                                // server already switched us to another local
                                // session if one remained, so a SessionDeleted here
                                // means none are left -- shut down.
                                break;
                            }
                        }
                    }
                    Some(ServerMessage::ScrollbackInfo { total_lines }) => {
                        log::debug!("srv: ScrollbackInfo total_lines={}", total_lines);
                        // Update visual state with accurate total line count.
                        // Never while a view is up: this counts the FOREGROUND
                        // session's scrollback, but there Visual mode is scoped to
                        // a view cell whose `total_lines` is pinned to the rows it
                        // paints. A reply still in flight from before the view was
                        // entered would otherwise un-pin it and let the copy view
                        // scroll into lines it cannot extract.
                        if active_view.is_none() {
                            if let Some(ref mut vs) = input.visual_state {
                                vs.total_lines = total_lines;
                            }
                        }
                    }
                    Some(ServerMessage::PaneContent {
                        pane_id,
                        cols: pane_cols,
                        rows: pane_rows,
                        cells: pane_cells,
                        cursor_x,
                        cursor_y,
                        cursor_visible,
                        application_cursor_keys,
                        session_name: pc_session,
                        tab_name: pc_tab,
                        session_visible,
                    }) => {
                        // Note: `pane_cols`/`pane_rows` are the PANE's size, not
                        // the terminal's; do not confuse them with the loop's
                        // `cols`/`rows`. Fold this snapshot into every view cell
                        // that aliases (src, pane_id), then repaint if the change
                        // touched the currently-active view.
                        log::debug!("srv: PaneContent pane_id={pane_id} {pane_cols}x{pane_rows}");
                        let snap = crate::client::view::PaneSnapshot {
                            cols: pane_cols,
                            rows: pane_rows,
                            cells: pane_cells,
                            cursor_x,
                            cursor_y,
                            cursor_visible,
                            application_cursor_keys,
                            session_visible,
                        };
                        // Cell title = `session / tab`, host-prefixed for a remote
                        // source (`host: session / tab`). Empty session ⇒ the pane
                        // couldn't be resolved server-side; leave the title unset so
                        // the cell keeps showing `waiting…`.
                        let title = if pc_session.is_empty() {
                            None
                        } else {
                            let base = format!("{pc_session} / {pc_tab}");
                            Some(match &src {
                                ConnId::Remote(host) => format!("{host}: {base}"),
                                ConnId::Local => base,
                            })
                        };
                        let mut active_touched = false;
                        // A cell in the active view whose session-visibility just
                        // flipped needs a re-subscribe: entering visibility drops
                        // its size demand, and LEAVING it must re-assert the demand
                        // so the pane reflows to the cell (else the cell would show
                        // clipped full-size content instead of cell-sized content).
                        let mut active_visibility_flipped = false;
                        for (vi, view) in views.iter_mut().enumerate() {
                            for cell in view.cells.iter_mut() {
                                // An exited cell is terminal: an in-flight snapshot
                                // that raced the `PaneExited` event must not
                                // resurrect it into painting content for a pane
                                // that no longer exists.
                                if cell.conn == src && cell.pane_id == pane_id && !cell.exited {
                                    let was_visible = cell.is_session_visible();
                                    // Clone per match: the same pane can be
                                    // aliased by more than one cell/view. A fresh
                                    // snapshot means the source is live again.
                                    cell.snapshot = Some(snap.clone());
                                    cell.disconnected = false;
                                    cell.unavailable = None;
                                    if title.is_some() {
                                        cell.title = title.clone();
                                    }
                                    if active_view == Some(vi) {
                                        active_touched = true;
                                        if was_visible != session_visible {
                                            active_visibility_flipped = true;
                                        }
                                    }
                                }
                            }
                        }
                        if active_visibility_flipped {
                            if let Some(av) = active_view {
                                // Re-subscribe the whole active view so the flipped
                                // cell's size_demand is recomputed (see
                                // `subscribe_view_cells`).
                                subscribe_view_cells(mgr, &mut views[av], &view_border_style).await?;
                            }
                        }
                        if active_touched {
                            if let Some(av) = active_view {
                                paint_view(
                                    &mut renderer,
                                    &views[av],
                                    &input,
                                    &whichkey,
                                    &theme,
                                    &compositor_theme,
                                    &view_border_style,
                                    &which_key_position,
                                    viewport_top,
                                    focused_pane_rect.as_ref(),
                                )?;
                            }
                        }
                    }
                    // Direct ack to THIS client's `ViewCreate`. Finish the compose
                    // -> new-view flow (add any queued cells) and arm the enter that
                    // fires when the ensuing `ViewList` carries the new view.
                    Some(ServerMessage::ViewCreated { id }) => {
                        log::debug!("srv: ViewCreated id={id}");
                        if matches!(src, ConnId::Local) {
                            if let Some(cells) = pending_add_cells.take() {
                                mgr.send(
                                    &ConnId::Local,
                                    ClientMessage::ViewAddCells { id, cells },
                                )
                                .await?;
                            }
                            pending_enter_view = Some(id);
                        }
                    }
                    // The shared-view registry snapshot. Rebuild the per-terminal
                    // cache from it (preserving each view's render state), then
                    // reconcile what this terminal is displaying: enter a
                    // just-created view, mirror focus/layout/zoom + cell adds/removes
                    // into the live view, or leave to a session if the displayed view
                    // was deleted. This is what makes views shared + live.
                    Some(ServerMessage::ViewList { views: view_infos }) => {
                        // Only the local server owns the registry this client drives.
                        if matches!(src, ConnId::Local) {
                            log::debug!("srv: ViewList {} view(s)", view_infos.len());
                            // The displayed view's pane set BEFORE the rebuild, for
                            // the subscribe/unsubscribe diff. Keyed by (conn, pane):
                            // a pane still aliased by any surviving cell must keep its
                            // subscription even if one aliasing cell was removed.
                            let old_active_panes: Vec<(ConnId, crate::protocol::PaneId)> =
                                active_view
                                    .and_then(|av| views.get(av))
                                    .map(|v| {
                                        v.cells
                                            .iter()
                                            .map(|c| (c.conn.clone(), c.pane_id))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                            // Rebuild the cache, carrying each view's per-terminal
                            // render state forward from the matching old entry.
                            let old_views = std::mem::take(&mut views);
                            views = view_infos
                                .iter()
                                .map(|info| {
                                    let prev = old_views.iter().find(|v| v.id == info.id);
                                    crate::client::view::ClientView::from_info(info, prev)
                                })
                                .collect();
                            // Re-resolve the displayed cache index by stable id
                            // IMMEDIATELY, so no stale (pre-rebuild) index can index
                            // the new cache in any branch below (enter/leave/diff).
                            active_view =
                                active_view_id.and_then(|id| views.iter().position(|v| v.id == id));

                            // A view this terminal just created: enter it now that the
                            // snapshot carries it (takes precedence over plain sync).
                            let entered = match pending_enter_view {
                                Some(pid) => {
                                    match views.iter().position(|v| v.id == pid) {
                                        Some(idx) => {
                                            pending_enter_view = None;
                                            enter_view(
                                                mgr,
                                                &mut views,
                                                &mut active_view,
                                                &mut active_view_id,
                                                idx,
                                                &current_attached,
                                                &mut renderer,
                                                &input,
                                                &whichkey,
                                                &theme,
                                                &compositor_theme,
                                                &view_border_style,
                                                &which_key_position,
                                                viewport_top,
                                                focused_pane_rect.as_ref(),
                                            )
                                            .await?;
                                            true
                                        }
                                        None => false,
                                    }
                                }
                                None => false,
                            };

                            if !entered {
                                if active_view_id.is_some() && active_view.is_none() {
                                    // The displayed view was deleted for everyone:
                                    // leave to the foreground session (unsubscribe our
                                    // cells, re-attach, resize). No `Detach`/`Attach`
                                    // dance beyond the re-attach.
                                    for (conn, pid) in &old_active_panes {
                                        let _ = mgr
                                            .send(
                                                conn,
                                                ClientMessage::UnsubscribePane {
                                                    pane_id: *pid,
                                                },
                                            )
                                            .await;
                                    }
                                    active_view = None;
                                    active_view_id = None;
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.resize(c, r);
                                    if let Some((_, session)) = current_attached.clone() {
                                        mgr.send_foreground(ClientMessage::Attach {
                                            session_name: session,
                                        })
                                        .await?;
                                    }
                                    mgr.send_foreground(ClientMessage::Resize {
                                        cols: c,
                                        rows: r,
                                    })
                                    .await?;
                                } else if let Some(av) = active_view {
                                    // Diff the pane set: unsubscribe panes fully gone,
                                    // (re)subscribe the current set (adds the new ones,
                                    // refreshes sizes for a layout/focus change). This
                                    // never sends Detach/Attach — the bug4 fix belongs
                                    // to the enter path only.
                                    let new_panes: Vec<(ConnId, crate::protocol::PaneId)> = views
                                        [av]
                                        .cells
                                        .iter()
                                        .map(|c| (c.conn.clone(), c.pane_id))
                                        .collect();
                                    for (conn, pid) in &old_active_panes {
                                        if !new_panes.iter().any(|(c, p)| c == conn && p == pid) {
                                            let _ = mgr
                                                .send(
                                                    conn,
                                                    ClientMessage::UnsubscribePane {
                                                        pane_id: *pid,
                                                    },
                                                )
                                                .await;
                                        }
                                    }
                                    subscribe_view_cells(mgr, &mut views[av], &view_border_style).await?;
                                    paint_view(
                                        &mut renderer,
                                        &views[av],
                                        &input,
                                        &whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                    )?;
                                }
                            }

                            // Live-update an OPEN switcher's Views section so another
                            // terminal's create/rename shows without reopening it.
                            if input.session_switch.is_some() {
                                if let Some(ss) = input.session_switch.as_mut() {
                                    ss.set_views(views.iter().map(|v| v.name.clone()).collect());
                                }
                                let (c, r) = crossterm::terminal::size()?;
                                if let Some(av) = active_view {
                                    paint_view(
                                        &mut renderer,
                                        &views[av],
                                        &input,
                                        &whichkey,
                                        &theme,
                                        &compositor_theme,
                                        &view_border_style,
                                        &which_key_position,
                                        viewport_top,
                                        focused_pane_rect.as_ref(),
                                    )?;
                                } else {
                                    renderer.clear_overlay(c, r)?;
                                }
                                if let Some(ref ss) = input.session_switch {
                                    let draw_cmds = ss.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                    renderer.flush()?;
                                }
                            }
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
                        new_config.session_manager_bindings(),
                    );

                    // Update theme before any re-render so overlays repaint with
                    // the new colors.
                    theme = new_config.theme();
                    compositor_theme = new_config.compositor_theme();

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
