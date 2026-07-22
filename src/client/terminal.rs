//! Client terminal handling and server connection.
//!
//! This module provides:
//! - Terminal raw mode setup/restore
//! - `RemuxClient` for connecting to the server over Unix socket

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use crate::protocol::{ClientMessage, Hello, ServerMessage, Welcome, PROTOCOL_VERSION};
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
///
/// The transport is intentionally abstract: the reader/writer are boxed trait
/// objects so the same client works over a local Unix socket or an SSH pipe.
/// The steady-state render/input loop only ever calls `send`/`recv`, so it is
/// unaffected by which transport backs the connection.
pub struct RemuxClient {
    reader: Box<dyn AsyncRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    /// Keeps the ssh child process alive for remote connections. `None` for
    /// local (Unix socket) connections.
    _child: Option<Child>,
    /// The `remux_version` the server reported in its `Welcome` during the
    /// handshake. Used to detect version skew against this binary's own
    /// [`crate::protocol::build_version`].
    server_version: String,
}

impl RemuxClient {
    /// Connect to an existing local server, or return an error if none is
    /// running.
    pub async fn connect() -> Result<Self> {
        Self::connect_local().await
    }

    /// Connect to the local server over the Unix domain socket.
    pub async fn connect_local() -> Result<Self> {
        let path = socket_path();
        log::debug!("terminal: connect_local socket_path={}", path.display());
        let stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("connecting to server at {}", path.display()))?;

        log::debug!("terminal: connect_local success");
        let (reader, writer) = stream.into_split();
        let mut reader: Box<dyn AsyncRead + Unpin + Send> = Box::new(reader);
        let mut writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(writer);
        let server_version = handshake(&mut reader, &mut writer)
            .await
            .context("handshake with local server")?;
        Ok(Self {
            reader,
            writer,
            _child: None,
            server_version,
        })
    }

    /// Connect to a remote server over SSH by spawning `remux relay` on the
    /// remote host and pumping the wire protocol through its stdio.
    ///
    /// `dest` is any SSH destination (`user@host`, or a `~/.ssh/config` alias);
    /// `remux_path` is where the `remux` binary lives on the remote. Optional
    /// `port`/`identity` map to `ssh -p`/`-i`; `extra_args` are inserted before
    /// the destination so `~/.ssh/config`-style options can be passed.
    pub async fn connect_ssh(
        dest: &str,
        port: Option<u16>,
        identity: Option<&str>,
        extra_args: &[String],
        remux_path: &str,
    ) -> Result<Self> {
        log::debug!(
            "terminal: connect_ssh dest={dest} port={port:?} identity={identity:?} \
             extra_args={extra_args:?} remux_path={remux_path}"
        );
        // stderr is inherited so ssh host-key / password prompts are visible;
        // this runs before the terminal enters raw mode.
        let mut cmd = Command::new("ssh");
        if let Some(port) = port {
            cmd.arg("-p").arg(port.to_string());
        }
        if let Some(identity) = identity {
            cmd.arg("-i").arg(identity);
        }
        cmd.args(extra_args)
            .arg(dest)
            .arg(remux_path)
            .arg("relay")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning ssh to {dest}"))?;

        let stdout = child
            .stdout
            .take()
            .context("ssh child produced no stdout pipe")?;
        let stdin = child
            .stdin
            .take()
            .context("ssh child produced no stdin pipe")?;

        let mut reader: Box<dyn AsyncRead + Unpin + Send> = Box::new(stdout);
        let mut writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(stdin);
        let server_version = handshake(&mut reader, &mut writer)
            .await
            .with_context(|| format!("handshake with remote server via {dest}"))?;

        log::debug!("terminal: connect_ssh success dest={dest}");
        Ok(Self {
            reader,
            writer,
            _child: Some(child),
            server_version,
        })
    }

    /// Send a message to the server.
    pub async fn send(&mut self, msg: ClientMessage) -> Result<()> {
        log::debug!("terminal: send {}", client_message_summary(&msg));
        write_message(&mut self.writer, &msg).await
    }

    /// The `remux_version` the server reported during the handshake. Compared
    /// against [`crate::protocol::build_version`] to surface version skew.
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// Receive a message from the server.
    ///
    /// Returns `Ok(None)` if the server closed the connection.
    pub async fn recv(&mut self) -> Result<Option<ServerMessage>> {
        read_message::<ServerMessage>(&mut self.reader).await
    }

    /// Decompose the client into its owned reader, writer, and (for SSH
    /// connections) the child process handle. Used by the connection registry
    /// to store the writer and spawn a dedicated reader task per connection.
    #[allow(clippy::type_complexity)]
    pub fn into_split(
        self,
    ) -> (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
        Option<Child>,
    ) {
        (self.reader, self.writer, self._child)
    }
}

/// Perform the version handshake: write `Hello`, read `Welcome`, and abort if
/// the peer speaks an incompatible protocol version.
///
/// This is the first exchange on every connection, sent via the normal framed
/// message helpers, before any `ClientMessage`/`ServerMessage` traffic. On
/// success returns the server's reported `remux_version` so callers can detect
/// build skew against this binary's own [`crate::protocol::build_version`].
async fn handshake<R, W>(reader: &mut R, writer: &mut W) -> Result<String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        remux_version: crate::protocol::build_version(),
    };
    write_message(writer, &hello)
        .await
        .context("sending Hello")?;

    let welcome: Welcome = read_message(reader)
        .await
        .context("reading Welcome")?
        .context("server closed connection during handshake")?;
    log::info!(
        "terminal: handshake peer remux {} protocol v{}",
        welcome.remux_version,
        welcome.protocol_version
    );

    if welcome.protocol_version != PROTOCOL_VERSION {
        anyhow::bail!(
            "incompatible remux: remote protocol v{}, local v{} (remote remux {})",
            welcome.protocol_version,
            PROTOCOL_VERSION,
            welcome.remux_version
        );
    }
    Ok(welcome.remux_version)
}

/// Produce a concise summary of a `ClientMessage` for debug logging.
fn client_message_summary(msg: &ClientMessage) -> String {
    match msg {
        ClientMessage::Input { data } => format!("Input({} bytes)", data.len()),
        other => format!("{:?}", other),
    }
}
