// Allow dead code during early development -- modules are defined but not yet
// wired into the binary entry point.
#![allow(dead_code)]

mod client;
mod config;
mod protocol;
mod screen;
mod server;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{KeyCode, KeyEventKind};
use futures::StreamExt;

use crate::client::input::{InputAction, InputHandler, Mode};
use crate::client::renderer::Renderer;
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

    let mut event_stream = EventStream::new();
    let keybindings = config.keybinding_tree();
    let mode_switch_key = parse_mode_switch_key(&config.general.mode_switch_key);
    let mut input = InputHandler::new(keybindings, mode_switch_key);
    let (cols, rows) = crossterm::terminal::size()?;
    let mut renderer = Renderer::new(cols, rows);
    let mut whichkey = WhichKeyPopup::new();
    let theme = config.theme();

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
                        let action = input.handle_key(key);
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
                                if matches!(cmd, RemuxCommand::SessionDetach) {
                                    return Ok(());
                                }
                                let is_rename_finish = matches!(
                                    cmd,
                                    RemuxCommand::PaneRename(_) | RemuxCommand::PaneRenameCancel
                                );
                                client.send(ClientMessage::Command(cmd)).await?;
                                // After rename confirm/cancel, the client has
                                // already switched to Normal mode -- tell the
                                // server so the status bar and cursor update.
                                if is_rename_finish {
                                    client
                                        .send(ClientMessage::ModeChanged {
                                            mode: "NORMAL".to_string(),
                                        })
                                        .await?;
                                }
                            }
                            InputAction::ModeChanged(mode) => {
                                let mode_str = match mode {
                                    Mode::Insert => "INSERT",
                                    Mode::Normal => "NORMAL",
                                    Mode::Visual => "VISUAL",
                                    Mode::Rename => "RENAME",
                                };
                                client
                                    .send(ClientMessage::ModeChanged {
                                        mode: mode_str.to_string(),
                                    })
                                    .await?;
                                // When entering Rename mode, send an initial
                                // empty update so the server clears the pane
                                // name and stores the original for cancel.
                                if mode == Mode::Rename {
                                    client
                                        .send(ClientMessage::Command(
                                            RemuxCommand::PaneRenameUpdate(String::new()),
                                        ))
                                        .await?;
                                }
                                // Hide which-key when mode changes
                                if whichkey.visible {
                                    whichkey.hide();
                                    renderer.clear_overlay(cols, rows)?;
                                }
                            }
                            InputAction::ShowWhichKey(label, entries) => {
                                whichkey.show(label, entries);
                                let commands = whichkey.render(cols, rows, &theme);
                                renderer.render_whichkey_overlay(&commands)?;
                            }
                            InputAction::HideWhichKey => {
                                whichkey.hide();
                                renderer.clear_overlay(cols, rows)?;
                            }
                            InputAction::EditInEditor => {
                                // Temporarily restore terminal for editor
                                restore_terminal()?;
                                // TODO: get scrollback from server, open in editor
                                setup_terminal()?;
                                // Re-send resize in case terminal changed
                                let (cols, rows) = crossterm::terminal::size()?;
                                renderer.resize(cols, rows);
                                client.send(ClientMessage::Resize { cols, rows }).await?;
                            }
                            InputAction::RenameUpdate(text) => {
                                client
                                    .send(ClientMessage::Command(
                                        RemuxCommand::PaneRenameUpdate(text),
                                    ))
                                    .await?;
                            }
                            InputAction::YankToClipboard(_) | InputAction::SearchPrompt
                            | InputAction::VisualScroll { .. }
                            | InputAction::None => {}
                        }
                    }
                    Some(Ok(crossterm::event::Event::Resize(new_cols, new_rows))) => {
                        renderer.resize(new_cols, new_rows);
                        client.send(ClientMessage::Resize { cols: new_cols, rows: new_rows }).await?;
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
                    Some(ServerMessage::FullRender { cells, cursor_x, cursor_y, cursor_visible }) => {
                        renderer.render_full(&cells, cursor_x, cursor_y, cursor_visible)?;
                        // Re-render popup on top if visible
                        if whichkey.visible {
                            let commands = whichkey.render(cols, rows, &theme);
                            renderer.render_whichkey_overlay(&commands)?;
                        }
                    }
                    Some(ServerMessage::RenderDiff { changes, cursor_x, cursor_y, cursor_visible }) => {
                        renderer.render_diff(&changes, cursor_x, cursor_y, cursor_visible)?;
                        // Re-render popup on top if visible
                        if whichkey.visible {
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
                    Some(ServerMessage::Event(event)) => {
                        log::debug!("server event: {:?}", event);
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

/// Parse the mode switch key string from config into a crossterm KeyCode.
fn parse_mode_switch_key(key: &str) -> KeyCode {
    match key.to_lowercase().as_str() {
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "enter" => KeyCode::Enter,
        s if s.len() == 1 => KeyCode::Char(s.chars().next().unwrap_or(' ')),
        _ => KeyCode::Esc,
    }
}
