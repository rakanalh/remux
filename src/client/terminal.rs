//! Client terminal handling and server connection.
//!
//! This module provides:
//! - Terminal raw mode setup/restore
//! - `RemuxClient` for connecting to the server over Unix socket

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::process::Child;

use crate::config::theme::TabStyle;
use crate::config::Config;
use crate::protocol::{ClientMessage, Hello, ServerMessage, ViewInfo, Welcome, PROTOCOL_VERSION};
use crate::server::daemon::{read_message, socket_path, write_message};

// ---------------------------------------------------------------------------
// Terminal setup / restore
// ---------------------------------------------------------------------------

/// Put the terminal into raw mode and switch to the alternate screen.
pub fn setup_terminal() -> Result<()> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    enable_terminal_modes().context("setting up alternate screen")
}

/// Send the modes the client runs under: the alternate screen, the input
/// modes of [`enable_input_modes`], and a hidden cursor.
///
/// [`setup_terminal`] sends them at startup and a redraw sends them again,
/// because a terminal reset drops all of them. Only call this when a full
/// repaint follows: xterm and most terminals that copy it clear the alternate
/// screen on `CSI ? 1049 h` even when it is already active.
pub fn enable_terminal_modes() -> Result<()> {
    crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
    enable_input_modes()?;
    crossterm::execute!(std::io::stdout(), crossterm::cursor::Hide)?;
    Ok(())
}

/// Send bracketed paste and mouse capture. Both are idempotent and change
/// nothing on screen, so they are safe to send without a repaint.
pub fn enable_input_modes() -> Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
    )?;
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
        crate::server::daemon::prepare_runtime_dir()?;
        let path = socket_path();
        log::debug!("terminal: connect_local socket_path={}", path.display());
        let stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("connecting to server at {}", path.display()))?;

        log::debug!("terminal: connect_local success");
        let (reader, writer) = stream.into_split();
        let mut reader: Box<dyn AsyncRead + Unpin + Send> = Box::new(reader);
        let mut writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(writer);
        let server_version = handshake(&mut reader, &mut writer).await.map_err(|e| {
            if e.is::<ProtocolSkew>() {
                e.context(
                    "the running server is from another remux build; `remux restart` \
                     replaces it, saving and restoring its sessions",
                )
            } else {
                e.context("handshake with local server")
            }
        })?;
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
    /// `label` names the remote in the log. A failed dial, or one that has
    /// not finished within `deadline`, is an error that starts with the last
    /// lines ssh wrote to stderr. `echo` also prints those lines to this
    /// process's stderr until the handshake is done; it is for a dial made
    /// before the terminal enters raw mode.
    pub async fn connect_ssh(
        label: &str,
        remote: &crate::config::RemoteConfig,
        deadline: Option<std::time::Duration>,
        echo: bool,
    ) -> Result<Self> {
        log::debug!(
            "terminal: connect_ssh {label} dest={} port={:?} identity={:?} \
             extra_args={:?} remux_path={}",
            remote.ssh,
            remote.port,
            remote.identity,
            remote.extra_args,
            remote.remux_path
        );
        // stderr is never inherited, because the client is in raw mode for
        // most of the connection and escape codes from the far side would
        // change the terminal's modes. ssh writes its password, passphrase and
        // host-key prompts to /dev/tty, so they still reach the user. It
        // writes keyboard-interactive instructions (a 2FA menu) and the sshd
        // banner to stderr, which is what `echo` is for.
        let mut cmd = remote.ssh_command(false);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning ssh to {}", remote.ssh))?;

        let (Some(stdout), Some(stdin), Some(stderr)) =
            (child.stdout.take(), child.stdin.take(), child.stderr.take())
        else {
            anyhow::bail!("ssh child has no stdio pipes");
        };
        let mut stderr = crate::client::ssh_stderr::SshStderr::spawn(label, stderr, echo);

        let mut reader: Box<dyn AsyncRead + Unpin + Send> = Box::new(stdout);
        let mut writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(stdin);
        let dial = async {
            handshake(&mut reader, &mut writer)
                .await
                .with_context(|| format!("handshake with remote server via {}", remote.ssh))
        };
        let dialed = match deadline {
            Some(deadline) => tokio::time::timeout(deadline, dial)
                .await
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "connection timed out after {}s",
                        deadline.as_secs()
                    ))
                }),
            None => dial.await,
        };
        stderr.stop_echo();
        let server_version = match dialed {
            Ok(version) => version,
            Err(e) => {
                let _ = child.start_kill();
                let tail = stderr.tail(std::time::Duration::from_millis(500)).await;
                return Err(if tail.is_empty() {
                    e
                } else {
                    e.context(format!("ssh stderr: {tail}"))
                });
            }
        };

        log::debug!("terminal: connect_ssh success {label}");
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

    /// Tell the server how this client wants its frames drawn, and `host`,
    /// the name it knows this server by (empty for the local one). Sent
    /// straight after the handshake, before anything is attached, so that
    /// even the first frame is drawn this client's way.
    pub async fn announce(&mut self, look: &Look, host: &str) -> Result<()> {
        for msg in look.messages(host) {
            self.send(msg).await?;
        }
        Ok(())
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

/// The client settings every server it views draws with, whichever machine
/// that server is on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Look {
    /// `[appearance] pane_title`.
    pub pane_title: Option<String>,
    /// `[appearance.theme] tab_style`.
    pub tab_style: TabStyle,
}

impl Look {
    pub fn from_config(config: &Config) -> Self {
        Self {
            pane_title: config.appearance.pane_title.clone(),
            tab_style: config.appearance.theme.tab_style,
        }
    }

    /// The messages that tell a server known as `host` this look.
    pub fn messages(&self, host: &str) -> [ClientMessage; 2] {
        [
            ClientMessage::PaneTitle {
                template: self.pane_title.clone(),
                host: host.to_string(),
            },
            ClientMessage::TabStyle {
                style: self.tab_style,
            },
        ]
    }
}

/// Perform the version handshake: write `Hello`, read `Welcome`, and abort if
/// the peer speaks an incompatible protocol version.
///
/// This is the first exchange on every connection, sent via the normal framed
/// message helpers, before any `ClientMessage`/`ServerMessage` traffic. On
/// success returns the server's reported `remux_version` so callers can detect
/// build skew against this binary's own [`crate::protocol::build_version`].
pub(crate) async fn handshake<R, W>(reader: &mut R, writer: &mut W) -> Result<String>
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
    // The far side writes the stamp, and this function also runs inside the
    // server for a peer link: raw, a newline in it forges a line in
    // server.log and an ESC reaches whatever terminal shows that log.
    let shown = crate::client::cli_common::escape_bounded(&welcome.remux_version, STAMP_MAX);
    log::info!(
        "terminal: handshake peer remux {shown} protocol v{}",
        welcome.protocol_version
    );

    if welcome.protocol_version != PROTOCOL_VERSION {
        return Err(ProtocolSkew {
            remote: welcome.protocol_version,
            remote_version: shown,
        }
        .into());
    }
    Ok(welcome.remux_version.chars().take(STAMP_MAX).collect())
}

/// The most of a peer's build stamp that is kept or shown. A real stamp is a
/// version and a short git hash, far shorter than this.
const STAMP_MAX: usize = 64;

/// The peer answered the handshake with another protocol version: a server is
/// running, and it is not this build's. A type of its own so that a caller can
/// tell it from "nothing is listening".
#[derive(Debug)]
pub struct ProtocolSkew {
    pub remote: u32,
    /// The peer's build stamp, already escaped and bounded for display.
    pub remote_version: String,
}

impl std::fmt::Display for ProtocolSkew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "incompatible remux: remote protocol v{}, local v{} (remote remux {})",
            self.remote, PROTOCOL_VERSION, self.remote_version
        )
    }
}

impl std::error::Error for ProtocolSkew {}

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
            pane,
        } => format!(
            "CliSpawn(session={session:?}, pane={pane:?}, placement={placement:?}, \
             program={:?}, {} more arg(s), cwd={})",
            argv.first(),
            argv.len().saturating_sub(1),
            if cwd.is_some() { "set" } else { "none" }
        ),
        // A message body is user content. Every field is named so that a
        // field added later cannot fall through to the catch-all.
        ClientMessage::MsgSend {
            to,
            body,
            deliver,
            reply_to,
            pane_hint,
            defer,
            timeout_ms,
            wait,
            origin,
            req,
        } => format!(
            "MsgSend(to={:?}, deliver={deliver:?}, {} bytes, reply_to={:?}, pane_hint={pane_hint:?}, \
             defer={defer}, timeout={timeout_ms:?}, wait={wait}, origin={:?}, req={req:?})",
            to.to_string(),
            body.len(),
            reply_to.as_ref().map(|r| r.id),
            origin.as_ref().map(|o| format!("{}:%{}", o.host, o.pane_id))
        ),
        ClientMessage::MsgAck {
            pane,
            ids,
            all,
            pane_hint,
        } => format!(
            "MsgAck(pane={pane:?}, {} id(s), all={all}, pane_hint={pane_hint:?})",
            ids.len()
        ),
        ClientMessage::AgentName {
            name,
            pane,
            pane_hint,
        } => format!("AgentName(name={name:?}, pane={pane:?}, pane_hint={pane_hint:?})"),
        // The id names a transcript on disk and a path names a file in the
        // user's home; neither is a diagnostic. The agent, the kind and the
        // length are.
        ClientMessage::AgentSession {
            agent,
            reference,
            source,
            pane,
            pane_hint,
        } => format!(
            "AgentSession(agent={:?}, {}, {} bytes, source={source:?}, pane={pane:?}, \
             pane_hint={pane_hint:?})",
            crate::server::agent_session::agent_label(agent),
            reference.kind().as_str(),
            reference.value().len()
        ),
        ClientMessage::AgentStatus { target, req } => format!(
            "AgentStatus(target={:?}, req={req:?})",
            target.as_ref().map(|t| t.to_string())
        ),
        ClientMessage::AgentWait {
            target,
            state,
            timeout_ms,
            req,
        } => format!(
            "AgentWait(target={:?}, state={state:?}, timeout={timeout_ms:?}, req={req:?})",
            target.to_string()
        ),
        ClientMessage::MsgRead {
            pane,
            all,
            peek,
            wait_ms,
            pane_hint,
        } => format!(
            "MsgRead(pane={pane:?}, all={all}, peek={peek}, wait_ms={wait_ms:?}, pane_hint={pane_hint:?})"
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
            pane: Some(7),
        });
        assert!(
            !s.contains("hunter2"),
            "the credential must not appear: {s}"
        );
        assert!(s.contains("psql"), "but the program must: {s}");
        assert!(s.contains("1 more arg"), "and how many were dropped: {s}");
        assert!(s.contains("pane=Some(7)"), "and the caller's pane id: {s}");
    }

    #[test]
    fn agent_session_names_the_kind_and_length_but_not_the_value() {
        let id = "7f3c2a9e-51d4-4b8e-9a61-0c2f5d8e4b17";
        let path = "/home/sam/.pi/sessions/secret-project.jsonl";
        for (reference, kind, value) in [
            (crate::protocol::AgentSessionRef::Id(id.into()), "id", id),
            (
                crate::protocol::AgentSessionRef::Path(path.into()),
                "path",
                path,
            ),
        ] {
            let s = client_message_summary(&ClientMessage::AgentSession {
                agent: "claude".to_string(),
                reference,
                source: Some("startup".to_string()),
                pane: None,
                pane_hint: Some(7),
            });
            assert!(!s.contains(value), "the value must not appear: {s}");
            assert!(
                s.contains(&format!("agent=\"claude\", {kind}, {} bytes", value.len())),
                "but the agent, kind and length must: {s}"
            );
        }
    }

    #[test]
    fn msg_send_names_the_target_but_not_the_body() {
        let s = client_message_summary(&ClientMessage::MsgSend {
            origin: None,
            req: None,
            to: "beta".parse().unwrap(),
            body: "the token is hunter2-BODY".to_string(),
            deliver: crate::protocol::Deliver::Auto,
            reply_to: None,
            pane_hint: Some(3),
            defer: true,
            timeout_ms: Some(1500),
            wait: true,
        });
        assert!(!s.contains("hunter2"), "the body must not appear: {s}");
        assert!(s.contains("beta"), "but the target must: {s}");
        assert!(s.contains("25 bytes"), "and the body's size: {s}");
        assert!(
            s.contains("defer=true, timeout=Some(1500), wait=true"),
            "and the flags: {s}"
        );
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
        let err = result.expect_err("a skewed peer must be rejected");
        assert!(
            err.is::<ProtocolSkew>(),
            "typed, so a caller can tell it apart"
        );
        let err = err.to_string();
        assert!(
            err.contains("incompatible remux") && err.contains("0.1.0+old"),
            "the rejection must name the peer it refused: {err}"
        );
    }

    #[tokio::test]
    async fn a_peer_stamp_is_escaped_and_bounded_in_the_rejection() {
        let stamp = format!("forged\nINFO line\u{1b}[31m{}", "x".repeat(500));
        let (_, result) = handshake_against(Welcome {
            protocol_version: PROTOCOL_VERSION - 1,
            remux_version: stamp,
        })
        .await;
        let err = result.expect_err("a skewed peer is rejected").to_string();
        assert!(
            !err.contains('\n') && !err.contains('\u{1b}') && err.contains("forged\\x0aINFO"),
            "{err:?}"
        );
        assert!(err.len() < 200, "the far stamp is cut: {} bytes", err.len());
    }

    #[tokio::test]
    async fn a_peer_stamp_is_returned_cut() {
        let (_, result) = handshake_against(Welcome {
            protocol_version: PROTOCOL_VERSION,
            remux_version: "é".repeat(500),
        })
        .await;
        assert_eq!(result.unwrap(), "é".repeat(STAMP_MAX));
    }
}
