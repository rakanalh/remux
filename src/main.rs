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
use crate::client::renderer::Renderer;
use crate::client::session_manager::{NodeType, SessionManagerAction};
use crate::client::terminal::{restore_terminal, setup_terminal, RemuxClient};
use crate::client::whichkey::WhichKeyPopup;
use crate::config::Config;
use crate::protocol::{ClientMessage, RemuxCommand, ServerMessage};
use crate::server::daemon::{socket_path, RemuxServer};

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

    /// Internal: run the server (not for direct use)
    #[command(hide = true)]
    Server,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Server) => {
            let config = Config::load()?;
            RemuxServer::run(config).await?;
        }
        None => {
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

            match response {
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
                    } else {
                        // Attach to the first session
                        let session_name = sessions[0].name.clone();
                        client.send(ClientMessage::Attach { session_name }).await?;
                    }
                }
                _ => {
                    anyhow::bail!("unexpected response from server");
                }
            }

            client_event_loop(&mut client, &config).await?;
        }
        Some(Commands::New { session, folder }) => {
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
                    session_name: session,
                })
                .await?;

            client_event_loop(&mut client, &config).await?;
        }
        Some(Commands::Attach { name }) => {
            ensure_server_running().await?;
            let mut client = RemuxClient::connect().await?;
            let config = Config::load()?;

            client
                .send(ClientMessage::Attach { session_name: name })
                .await?;

            client_event_loop(&mut client, &config).await?;
        }
        Some(Commands::Ls) => {
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
        Some(Commands::Kill { name }) => {
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
    if socket_path().exists() {
        // Try connecting to verify the socket is live
        match RemuxClient::connect().await {
            Ok(_) => return Ok(()),
            Err(_) => {
                // Stale socket file, remove it
                let _ = std::fs::remove_file(socket_path());
            }
        }
    }

    let exe = std::env::current_exe().context("finding current executable")?;
    std::process::Command::new(exe)
        .arg("server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning server process")?;

    // Wait for the socket to appear
    for _ in 0..50 {
        if socket_path().exists() {
            // Give the server a moment to start accepting connections
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!("timed out waiting for server to start")
}

// ---------------------------------------------------------------------------
// Client event loop
// ---------------------------------------------------------------------------

/// Run the client event loop with terminal setup/restore.
async fn client_event_loop(client: &mut RemuxClient, config: &Config) -> Result<()> {
    setup_terminal()?;

    let result = run_client_loop(client, config).await;

    restore_terminal()?;
    result
}

/// The inner client event loop.
async fn run_client_loop(client: &mut RemuxClient, config: &Config) -> Result<()> {
    use crossterm::event::EventStream;

    config.validate();

    let mut event_stream = EventStream::new();
    let keybindings = config.keybinding_tree();
    let leader_key = config.leader_key();
    let shortcut_bindings = config.shortcut_bindings();
    let mut input = InputHandler::new(keybindings, leader_key, shortcut_bindings);
    let (cols, rows) = crossterm::terminal::size()?;
    let mut renderer = Renderer::new(cols, rows);
    let mut whichkey = WhichKeyPopup::new();
    let theme = config.theme();
    // Last known focused pane rect from the server, and cursor position.
    let mut focused_pane_rect: Option<crate::protocol::PaneRect> = None;
    let mut last_cursor_x: u16 = 0;
    let mut last_cursor_y: u16 = 0;

    // Scroll offset for the focused pane (0 = live view, >0 = scrolled back).
    let mut scroll_offset: usize = 0;

    // Mouse drag state for coalescing drag events (~60fps throttle).
    let mut drag_start: Option<(u16, u16)> = None;
    let mut last_drag_send: Instant = Instant::now();
    /// Minimum interval between drag event sends (~16ms = ~60fps).
    const DRAG_THROTTLE: Duration = Duration::from_millis(16);

    // Tell server our terminal size
    client.send(ClientMessage::Resize { cols, rows }).await?;

    loop {
        tokio::select! {
            // Keyboard events
            event = event_stream.next() => {
                match event {
                    Some(Ok(crossterm::event::Event::Key(key)))
                        if key.kind == KeyEventKind::Press =>
                    {
                        // Reset scroll offset on any keyboard input in Normal mode
                        if scroll_offset > 0 && input.mode == Mode::Normal {
                            scroll_offset = 0;
                            client.send(ClientMessage::ScrollOffset { offset: 0 }).await?;
                        }

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
                                client.send(ClientMessage::Input { data }).await?;
                            }
                            InputAction::Execute(cmd) => {
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
                                    client.send(ClientMessage::Input { data: bytes.clone() }).await?;
                                } else {
                                    client.send(ClientMessage::Command(cmd)).await?;
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
                                client
                                    .send(ClientMessage::ModeChanged {
                                        mode: mode_str.to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::ExecuteChain(cmds) => {
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
                                        client.send(ClientMessage::Input { data: bytes.clone() }).await?;
                                    } else {
                                        client.send(ClientMessage::Command(cmd)).await?;
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
                                client
                                    .send(ClientMessage::ModeChanged {
                                        mode: mode_str.to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::ModeChanged(mode) => {
                                let mode_str = match mode {
                                    Mode::Normal => "NORMAL",
                                    Mode::Command => "COMMAND",
                                    Mode::Visual => "VISUAL",
                                    Mode::CommandPalette => "PALETTE",
                                    Mode::Search => "SEARCH",
                                    Mode::SessionManager => "SESSION_MANAGER",
                                };
                                client
                                    .send(ClientMessage::ModeChanged {
                                        mode: mode_str.to_string(),
                                    })
                                    .await?;
                                // Reset scroll offset when returning to normal mode.
                                if mode == Mode::Normal && scroll_offset > 0 {
                                    scroll_offset = 0;
                                    client.send(ClientMessage::ScrollOffset { offset: 0 }).await?;
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
                                    // Request scrollback info to get accurate total_lines.
                                    client.send(ClientMessage::RequestScrollbackInfo).await?;
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
                                client
                                    .send(ClientMessage::ModeChanged {
                                        mode: "COMMAND".to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::ShowWhichKey(label, entries) => {
                                let (c, r) = crossterm::terminal::size()?;
                                whichkey.show(label, entries);
                                renderer.clear_overlay(c, r)?;
                                let commands = whichkey.render(c, r, &theme);
                                renderer.render_whichkey_overlay(&commands)?;
                                renderer.flush()?;
                            }
                            InputAction::HideWhichKey => {
                                whichkey.hide();
                                renderer.clear_overlay(cols, rows)?;
                                renderer.flush()?;
                            }
                            InputAction::EditInEditor => {
                                input.pending_editor_open = true;
                                client.send(ClientMessage::RequestScrollback).await?;
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
                                input.mode = Mode::Normal;
                                if scroll_offset > 0 {
                                    scroll_offset = 0;
                                    client.send(ClientMessage::ScrollOffset { offset: 0 }).await?;
                                }
                                client
                                    .send(ClientMessage::ModeChanged {
                                        mode: "NORMAL".to_string(),
                                    })
                                    .await?;
                                // Re-render to clear selection highlighting.
                                renderer.clear_overlay(cols, rows)?;
                                renderer.flush()?;
                            }
                            InputAction::VisualScroll { .. } => {
                                // Send scroll offset to server so compositor renders scrollback.
                                if let Some(ref vs) = input.visual_state {
                                    scroll_offset = vs.scroll_offset;
                                    client.send(ClientMessage::ScrollOffset { offset: vs.scroll_offset }).await?;
                                }
                                // The server will send a new render with the scrolled content.
                                // We'll overlay visual mode on top when the render arrives.
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
                                client
                                    .send(ClientMessage::ModeChanged {
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
                                client
                                    .send(ClientMessage::ModeChanged {
                                        mode: mode_str.to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::SearchPrompt => {
                                // Re-render the search prompt overlay.
                                if let Some(ref ss) = input.search_state {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.render_search_prompt(&ss.query_buffer, ss.phase, None, c, r)?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::SearchConfirm(ref query) => {
                                // Request scrollback from server.
                                client.send(ClientMessage::RequestScrollback).await?;
                                // Re-render prompt with confirmed query.
                                if let Some(ref ss) = input.search_state {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.render_search_prompt(query, ss.phase, None, c, r)?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::SearchCancel => {
                                // Clear search info on server.
                                client.send(ClientMessage::SearchInfo { current: 0, total: 0 }).await?;
                                // Send mode changed to NORMAL.
                                client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                // Clear overlay.
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                // Reset scroll offset when exiting search mode.
                                if scroll_offset > 0 {
                                    scroll_offset = 0;
                                    client.send(ClientMessage::ScrollOffset { offset: 0 }).await?;
                                }
                                renderer.flush()?;
                            }
                            InputAction::SearchNavigate => {
                                // Update search info on server and re-render prompt.
                                if let Some(ref ss) = input.search_state {
                                    client.send(ClientMessage::SearchInfo {
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
                                        ss.scrollback_line_count,
                                        scroll_offset,
                                        focused_pane_rect.as_ref(),
                                        &theme,
                                    )?;

                                    // Scroll to match if it's in scrollback (not visible).
                                    if !ss.matches.is_empty() {
                                        let (match_line, _match_col) = ss.matches[ss.current_match];
                                        let pane_height = focused_pane_rect
                                            .map(|pr| pr.height as usize)
                                            .unwrap_or(24);
                                        let total = ss.scrollback_line_count;
                                        // Calculate the scroll offset needed to center the match
                                        let visible_bottom_line = total.saturating_sub(scroll_offset);
                                        let visible_top_line = visible_bottom_line.saturating_sub(pane_height);
                                        if match_line < visible_top_line || match_line >= visible_bottom_line {
                                            // Match is not visible, scroll to center it
                                            let target_offset = total.saturating_sub(match_line + pane_height / 2);
                                            scroll_offset = target_offset;
                                            client.send(ClientMessage::ScrollOffset { offset: scroll_offset }).await?;
                                        }
                                    }
                                }
                            }
                            InputAction::SessionManagerOpen => {
                                // Hide which-key when opening session manager.
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                // Request session tree from server.
                                client.send(ClientMessage::ListSessionTree).await?;
                                // Notify server of mode change.
                                client
                                    .send(ClientMessage::ModeChanged {
                                        mode: "SESSION_MANAGER".to_string(),
                                    })
                                    .await?;
                            }
                            InputAction::SessionManagerClose => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                // Notify server of mode change.
                                client
                                    .send(ClientMessage::ModeChanged {
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
                            }
                            InputAction::SessionManagerAction(ref sm_action) => {
                                match sm_action {
                                    SessionManagerAction::SwitchSession(name) => {
                                        input.session_manager = None;
                                        input.mode = Mode::Normal;
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.clear_overlay(c, r)?;
                                        client.send(ClientMessage::Attach { session_name: name.clone() }).await?;
                                        client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                    }
                                    SessionManagerAction::SwitchTab { session, tab_index } => {
                                        input.session_manager = None;
                                        input.mode = Mode::Normal;
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.clear_overlay(c, r)?;
                                        client.send(ClientMessage::Command(RemuxCommand::SessionSwitchTab {
                                            session: session.clone(),
                                            tab_index: *tab_index,
                                        })).await?;
                                        client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                    }
                                    SessionManagerAction::SwitchPane { session, tab_index, pane_id } => {
                                        input.session_manager = None;
                                        input.mode = Mode::Normal;
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.clear_overlay(c, r)?;
                                        client.send(ClientMessage::Command(RemuxCommand::SessionSwitchPane {
                                            session: session.clone(),
                                            tab_index: *tab_index,
                                            pane_id: *pane_id,
                                        })).await?;
                                        client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                                    }
                                    SessionManagerAction::CreateFolder(name) => {
                                        client.send(ClientMessage::Command(RemuxCommand::FolderNew(name.clone()))).await?;
                                        // Refresh tree.
                                        client.send(ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::CreateSession { name, folder } => {
                                        client.send(ClientMessage::CreateSession {
                                            name: name.clone(),
                                            folder: folder.clone(),
                                        }).await?;
                                        // Wait for creation event before refreshing tree.
                                        // The refresh will happen when we receive SessionTree.
                                        client.send(ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::MoveSession { session, folder } => {
                                        client.send(ClientMessage::Command(RemuxCommand::FolderMoveSession {
                                            session: session.clone(),
                                            folder: folder.clone(),
                                        })).await?;
                                        client.send(ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::DeleteSession(name) => {
                                        client.send(ClientMessage::KillSession { name: name.clone() }).await?;
                                        client.send(ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::DeleteFolder(name) => {
                                        client.send(ClientMessage::Command(RemuxCommand::FolderDelete(name.clone()))).await?;
                                        client.send(ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::CloseTab { session, tab_index } => {
                                        client.send(ClientMessage::Command(RemuxCommand::TabCloseByIndex {
                                            session: session.clone(),
                                            tab_index: *tab_index,
                                        })).await?;
                                        client.send(ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::RefreshTree => {
                                        client.send(ClientMessage::ListSessionTree).await?;
                                    }
                                    SessionManagerAction::Close => {
                                        let has_sessions = input.session_manager.as_ref()
                                            .map(|sm| sm.rows.iter().any(|r| matches!(r.node_type, NodeType::Session(_))))
                                            .unwrap_or(false);
                                        input.session_manager = None;
                                        input.mode = Mode::Normal;
                                        let (c, r) = crossterm::terminal::size()?;
                                        renderer.clear_overlay(c, r)?;
                                        if !has_sessions {
                                            break;
                                        }
                                        client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
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
                                client.send(ClientMessage::ListSessionTree).await?;
                                // Set mode to Command to block normal input
                                input.mode = Mode::Command;
                                // Initialize with a loading placeholder
                                input.folder_select = Some(FolderSelectOverlay {
                                    folders: vec!["Loading...".to_string()],
                                    selected: 0,
                                    session_name: String::new(),
                                });
                                client.send(ClientMessage::ModeChanged { mode: "COMMAND".to_string() }).await?;
                            }
                            InputAction::FolderSelectUpdate => {
                                if let Some(ref fs) = input.folder_select {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = fs.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                }
                            }
                            InputAction::FolderSelectConfirm { ref session, ref folder } => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                // Send the move command
                                client.send(ClientMessage::Command(RemuxCommand::FolderMoveSession {
                                    session: session.clone(),
                                    folder: folder.clone(),
                                })).await?;
                                client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                            }
                            InputAction::FolderSelectClose => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                            }
                            InputAction::SessionSwitchOpen => {
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                                client.send(ClientMessage::ListSessionTree).await?;
                                input.mode = Mode::Command;
                                input.session_switch = Some(SessionSwitchOverlay {
                                    sessions: vec!["Loading...".to_string()],
                                    selected: 0,
                                });
                                client.send(ClientMessage::ModeChanged { mode: "COMMAND".to_string() }).await?;
                            }
                            InputAction::SessionSwitchUpdate => {
                                if let Some(ref ss) = input.session_switch {
                                    let (c, r) = crossterm::terminal::size()?;
                                    renderer.clear_overlay(c, r)?;
                                    let draw_cmds = ss.render(c, r, &theme);
                                    renderer.render_whichkey_overlay(&draw_cmds)?;
                                }
                            }
                            InputAction::SessionSwitchConfirm(ref session_name) => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                client.send(ClientMessage::Attach { session_name: session_name.clone() }).await?;
                                input.session_switch = None;
                                input.mode = Mode::Normal;
                                client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                            }
                            InputAction::SessionSwitchClose => {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                input.session_switch = None;
                                input.mode = Mode::Normal;
                                client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                            }
                            InputAction::NewSession(ref name) => {
                                // Create the session and then attach to it.
                                client.send(ClientMessage::CreateSession {
                                    name: name.clone(),
                                    folder: None,
                                }).await?;
                                client.send(ClientMessage::Attach { session_name: name.clone() }).await?;
                                client.send(ClientMessage::ModeChanged { mode: "NORMAL".to_string() }).await?;
                            }
                            InputAction::None => {}
                        }
                    }
                    Some(Ok(crossterm::event::Event::Mouse(mouse))) => {
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                drag_start = Some((mouse.column, mouse.row));
                                // Send click immediately.
                                client
                                    .send(ClientMessage::MouseClick {
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
                                        client
                                            .send(ClientMessage::MouseDrag {
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
                                        client
                                            .send(ClientMessage::MouseDrag {
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
                                let old = scroll_offset;
                                if input.mode == Mode::Visual {
                                    if let Some(ref mut vs) = input.visual_state {
                                        vs.scroll_up(3);
                                        scroll_offset = vs.scroll_offset;
                                    }
                                } else {
                                    scroll_offset = scroll_offset.saturating_add(3);
                                }
                                if scroll_offset != old {
                                    client.send(ClientMessage::ScrollOffset { offset: scroll_offset }).await?;
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                let old = scroll_offset;
                                if input.mode == Mode::Visual {
                                    if let Some(ref mut vs) = input.visual_state {
                                        vs.scroll_down(3);
                                        scroll_offset = vs.scroll_offset;
                                    }
                                } else {
                                    scroll_offset = scroll_offset.saturating_sub(3);
                                }
                                if scroll_offset != old {
                                    client.send(ClientMessage::ScrollOffset { offset: scroll_offset }).await?;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(crossterm::event::Event::Resize(new_cols, new_rows))) => {
                        renderer.resize(new_cols, new_rows);
                        client.send(ClientMessage::Resize { cols: new_cols, rows: new_rows }).await?;
                    }
                    Some(Ok(crossterm::event::Event::Paste(text))) => {
                        // Wrap pasted text in bracketed paste sequences.
                        let mut data = Vec::new();
                        data.extend_from_slice(b"\x1b[200~");
                        data.extend_from_slice(text.as_bytes());
                        data.extend_from_slice(b"\x1b[201~");
                        client.send(ClientMessage::Input { data }).await?;
                    }
                    Some(Err(e)) => {
                        log::error!("Event error: {}", e);
                    }
                    None => break,
                    _ => {}
                }
            }
            // Server messages
            msg = client.recv() => {
                match msg? {
                    Some(ServerMessage::FullRender { cells, cursor_x, cursor_y, cursor_visible, cursor_style, focused_pane_rect: fpr, application_cursor_keys: ack }) => {
                        focused_pane_rect = fpr;
                        input.application_cursor_keys = ack;
                        last_cursor_x = cursor_x;
                        last_cursor_y = cursor_y;
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
                                ss.scrollback_line_count,
                                scroll_offset,
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
                            let commands = whichkey.render(cols, rows, &theme);
                            renderer.render_whichkey_overlay(&commands)?;
                        }
                    }
                    Some(ServerMessage::RenderDiff { changes, cursor_x, cursor_y, cursor_visible, cursor_style, focused_pane_rect: fpr, application_cursor_keys: ack }) => {
                        focused_pane_rect = fpr;
                        input.application_cursor_keys = ack;
                        last_cursor_x = cursor_x;
                        last_cursor_y = cursor_y;
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
                                ss.scrollback_line_count,
                                scroll_offset,
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
                            let commands = whichkey.render(cols, rows, &theme);
                            renderer.render_whichkey_overlay(&commands)?;
                        }
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
                            client.send(ClientMessage::Resize { cols, rows }).await?;
                        } else if let Some(ref mut ss) = input.search_state {
                            if let Some(ref query) = ss.confirmed_query {
                                ss.scrollback_line_count = lines.len();
                                ss.matches = crate::client::input::SearchState::compute_matches(&lines, query);
                                ss.current_match = 0;
                                // Send search info to server.
                                client.send(ClientMessage::SearchInfo {
                                    current: ss.current_match,
                                    total: ss.matches.len(),
                                }).await?;
                                // Re-render prompt with match info.
                                let match_info = if ss.matches.is_empty() {
                                    None
                                } else {
                                    Some((ss.current_match, ss.matches.len()))
                                };
                                let q = query.clone();
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.render_search_prompt(&q, ss.phase, match_info, c, r)?;
                                // Highlight matches in the terminal content.
                                renderer.render_search_highlight(
                                    &ss.matches,
                                    ss.current_match,
                                    q.len(),
                                    ss.scrollback_line_count,
                                    scroll_offset,
                                    focused_pane_rect.as_ref(),
                                    &theme,
                                )?;
                            }
                        }
                    }
                    Some(ServerMessage::SessionTree { folders, unfiled }) => {
                        // If session switch overlay is active, populate it
                        if input.session_switch.is_some() {
                            let mut session_names: Vec<String> = Vec::new();
                            for f in &folders {
                                for s in &f.sessions {
                                    if !s.is_current {
                                        session_names.push(s.name.clone());
                                    }
                                }
                            }
                            for s in &unfiled {
                                if !s.is_current {
                                    session_names.push(s.name.clone());
                                }
                            }
                            input.update_session_switch_list(session_names);
                            // Render the popup
                            if let Some(ref ss) = input.session_switch {
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                let draw_cmds = ss.render(c, r, &theme);
                                renderer.render_whichkey_overlay(&draw_cmds)?;
                            }
                        }
                        // If folder select overlay is active, populate it
                        else if input.folder_select.is_some() {
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
                        } else {
                            let has_any_sessions = folders.iter().any(|f| !f.sessions.is_empty()) || !unfiled.is_empty();
                            if !has_any_sessions && input.session_manager.is_some() {
                                input.session_manager = None;
                                input.mode = Mode::Normal;
                                break;
                            }
                            if let Some(ref mut sm) = input.session_manager {
                                sm.update_tree(folders, unfiled);
                                let (c, r) = crossterm::terminal::size()?;
                                renderer.clear_overlay(c, r)?;
                                let draw_cmds = sm.render(c, r, &theme);
                                renderer.render_whichkey_overlay(&draw_cmds)?;
                            }
                        }
                    }
                    Some(ServerMessage::Event(event)) => {
                        log::debug!("server event: {:?}", event);
                        if matches!(event, crate::protocol::SessionEvent::SessionDeleted(_)) {
                            // If session manager is open, just refresh the tree
                            // instead of breaking out of the event loop.
                            if input.session_manager.is_some() {
                                client.send(ClientMessage::ListSessionTree).await?;
                            } else {
                                break;
                            }
                        }
                    }
                    Some(ServerMessage::ScrollbackInfo { total_lines }) => {
                        // Update visual state with accurate total line count.
                        if let Some(ref mut vs) = input.visual_state {
                            vs.total_lines = total_lines;
                        }
                    }
                    None => {
                        // Server disconnected
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
