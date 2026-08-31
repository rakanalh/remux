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

use crate::protocol::{ClientMessage, Hello, ServerMessage, ViewInfo, Welcome, PROTOCOL_VERSION};
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
    /// The most recent shared-view registry snapshot seen while
    /// [`recv_skip_views`](Self::recv_skip_views) skipped `ViewList` broadcasts
    /// during the synchronous startup handshake. The server pushes an initial
    /// `ViewList` on connect, which would otherwise be discarded before the
    /// steady-state loop's reader task exists — so it is *captured* here and
    /// replayed into the loop's view cache via [`take_captured_views`](Self::take_captured_views).
    captured_views: Option<Vec<ViewInfo>>,
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
            captured_views: None,
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
            captured_views: None,
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

    /// Receive the next message, transparently skipping the shared-view
    /// broadcasts (`ViewList` / `ViewCreated`).
    ///
    /// The server pushes an initial `ViewList` to every client as soon as it
    /// connects (and re-broadcasts on every view mutation). That message can
    /// therefore arrive in the middle of the client's *synchronous* startup
    /// request/response handshake (ListSessions → SessionList, etc.), which runs
    /// before the async reader task is spawned. Those sites expect a specific
    /// reply, so they use this to ignore the unsolicited view broadcast rather
    /// than mis-consume it during the handshake.
    ///
    /// Phase 2: the initial `ViewList` is not discarded but *captured* into
    /// `captured_views` (last one wins). The steady-state loop replays it into
    /// its view cache via [`take_captured_views`](Self::take_captured_views), so a
    /// client that connects after another terminal created a shared view still
    /// sees it — its switcher lists it — without waiting for the next mutation.
    pub async fn recv_skip_views(&mut self) -> Result<Option<ServerMessage>> {
        loop {
            match self.recv().await? {
                Some(ServerMessage::ViewList { views }) => {
                    log::debug!(
                        "terminal: capturing startup ViewList ({} view(s))",
                        views.len()
                    );
                    self.captured_views = Some(views);
                    continue;
                }
                Some(ServerMessage::ViewCreated { .. }) => {
                    log::debug!("terminal: skipping ViewCreated during startup handshake");
                    continue;
                }
                other => return Ok(other),
            }
        }
    }

    /// Take the shared-view registry snapshot captured during the startup
    /// handshake (see [`recv_skip_views`](Self::recv_skip_views)), leaving `None`
    /// behind. The steady-state loop calls this once to seed its view cache.
    pub fn take_captured_views(&mut self) -> Option<Vec<ViewInfo>> {
        self.captured_views.take()
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
        // Every keystroke into a VIEW cell goes out as `InputToPane`, so
        // without this arm a passphrase typed into a view was written to
        // `client.log` -- as `[99, 111, 114, ...]`, which is full fidelity and
        // trivially decoded. `Input` had the arm from the start; the newer
        // variant never got the same treatment.
        ClientMessage::InputToPane { pane_id, data } => {
            format!("InputToPane(pane_id={pane_id}, {} bytes)", data.len())
        }
        // `remux split -- psql postgresql://user:pass@host/db`: the PROGRAM is
        // the diagnostic, the ARGUMENTS are where the credential is.
        ClientMessage::CliSpawn {
            session,
            placement,
            argv,
            cwd,
        } => format!(
            "CliSpawn(session={session:?}, placement={placement:?}, program={:?}, \
             {} more arg(s), cwd={})",
            argv.first(),
            argv.len().saturating_sub(1),
            if cwd.is_some() { "set" } else { "none" }
        ),
        other => format!("{:?}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`client.log` must not contain what the user typed.** It is plaintext,
    /// unrotated and never pruned, and `main.rs` pins the logger at `Debug`
    /// with no `RUST_LOG`, so nothing a user can set turns it off.
    ///
    /// Tested here rather than in a harness because `InputToPane` is only ever
    /// sent from a VIEW cell, which no frame-level harness can drive -- and a
    /// summary function is a pure function, so this is the layer that can
    /// assert on it directly.
    #[test]
    fn a_keystroke_is_summarised_by_length_and_never_printed() {
        let secret = b"correct-horse-battery-staple".to_vec();
        for msg in [
            ClientMessage::Input {
                data: secret.clone(),
            },
            ClientMessage::InputToPane {
                pane_id: 7,
                data: secret.clone(),
            },
        ] {
            let s = client_message_summary(&msg);
            assert!(
                !s.contains("correct-horse"),
                "the text itself must not appear: {s}"
            );
            // The form the leak actually took: `{:?}` prints a `Vec<u8>` as
            // decimal bytes, so a check for the STRING alone passes against a
            // leaking build.
            assert!(!s.contains("99, 111, 114"), "nor the byte list: {s}");
            assert!(
                s.contains(&format!("{} bytes", secret.len())),
                "but the length must, or the line is useless: {s}"
            );
        }
    }

    /// The same, for the command line `remux split` forwards.
    #[test]
    fn cli_spawn_names_the_program_but_not_its_arguments() {
        let s = client_message_summary(&ClientMessage::CliSpawn {
            session: "main".to_string(),
            placement: crate::protocol::CliPlacement::SplitBelow,
            argv: vec![
                "psql".to_string(),
                "postgresql://sam:hunter2@db/prod".to_string(),
            ],
            cwd: None,
        });
        assert!(
            !s.contains("hunter2"),
            "the credential must not appear: {s}"
        );
        assert!(s.contains("psql"), "but the program must: {s}");
        assert!(s.contains("1 more arg"), "and how many were dropped: {s}");
    }

    /// Drive [`handshake`] against a scripted peer that answers with `welcome`.
    /// The client's Hello is read (and returned) so the test can also assert
    /// what this binary announces.
    async fn handshake_against(welcome: Welcome) -> (Hello, Result<String>) {
        let (mut client_side, mut peer) = tokio::io::duplex(4096);
        let peer_task = tokio::spawn(async move {
            let hello: Hello = read_message(&mut peer)
                .await
                .unwrap()
                .expect("client sent no Hello");
            write_message(&mut peer, &welcome).await.unwrap();
            hello
        });
        let (mut r, mut w) = tokio::io::split(&mut client_side);
        let result = handshake(&mut r, &mut w).await;
        (peer_task.await.unwrap(), result)
    }

    /// The client announces the current protocol version and accepts a peer
    /// that speaks it, returning the peer's build stamp for skew display.
    #[tokio::test]
    async fn handshake_accepts_a_matching_protocol_version() {
        let (hello, result) = handshake_against(Welcome {
            protocol_version: PROTOCOL_VERSION,
            remux_version: "0.1.0+peer".to_string(),
        })
        .await;
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        assert_eq!(result.unwrap(), "0.1.0+peer");
    }

    /// Version skew is HARD-rejected here, on the client. The server is
    /// deliberately lenient (it logs the mismatch and answers anyway), so this
    /// bail is the whole enforcement -- a `PROTOCOL_VERSION` bump only keeps
    /// old peers out because of it. The frozen `Hello`/`Welcome` shape is what
    /// lets the rejection be a clean error rather than a decode failure.
    #[tokio::test]
    async fn handshake_rejects_an_older_protocol_version() {
        let (_, result) = handshake_against(Welcome {
            protocol_version: PROTOCOL_VERSION - 1,
            remux_version: "0.1.0+old".to_string(),
        })
        .await;
        let err = result
            .expect_err("a skewed peer must be rejected")
            .to_string();
        assert!(
            err.contains("incompatible remux") && err.contains("0.1.0+old"),
            "the rejection must name the peer it refused: {err}"
        );
    }
}
