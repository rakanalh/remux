//! Client terminal handling and server connection.
//!
//! This module provides:
//! - Terminal raw mode setup/restore
//! - `RemuxClient` for connecting to the server over Unix socket

use anyhow::{Context, Result};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use crate::protocol::{ClientMessage, ServerMessage};
use crate::server::daemon::{read_message, socket_path, write_message};

// ---------------------------------------------------------------------------
// Terminal setup / restore
// ---------------------------------------------------------------------------

/// Put the terminal into raw mode and switch to the alternate screen.
pub fn setup_terminal() -> Result<()> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
        crossterm::cursor::Hide,
    )
    .context("setting up alternate screen")?;
    Ok(())
}

/// Restore the terminal to its normal state.
pub fn restore_terminal() -> Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::cursor::Show,
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
    )
    .context("restoring terminal")?;
    crossterm::terminal::disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// RemuxClient
// ---------------------------------------------------------------------------

/// A client connection to the Remux server.
pub struct RemuxClient {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
}

impl RemuxClient {
    /// Connect to an existing server, or return an error if none is running.
    pub async fn connect() -> Result<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("connecting to server at {}", path.display()))?;

        let (reader, writer) = stream.into_split();
        Ok(Self { reader, writer })
    }

    /// Send a message to the server.
    pub async fn send(&mut self, msg: ClientMessage) -> Result<()> {
        write_message(&mut self.writer, &msg).await
    }

    /// Receive a message from the server.
    ///
    /// Returns `Ok(None)` if the server closed the connection.
    pub async fn recv(&mut self) -> Result<Option<ServerMessage>> {
        read_message::<ServerMessage>(&mut self.reader).await
    }
}
