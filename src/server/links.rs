//! The server's links to other servers.
//!
//! An OUTBOUND link is one this server dialled to send to a `[remotes.<key>]`:
//! it runs `ssh -o BatchMode=yes ... -- <ssh> <remux_path> relay` itself,
//! declares itself with `PeerLink`, and forwards each send with its `Origin`.
//! A task owns the ssh child and answers each forward from the frame that
//! answers it: every forward is written with a `req` from the link's own
//! counter, the far server echoes it, and the answer goes to the forward
//! with that `req`. An answer naming no forward in flight is dropped.
//!
//! An INBOUND link is a connection that declared itself a link from another
//! server with `PeerLink`, after [`crate::server::identity::peer_link_check`]
//! admitted it. Its messages carry an `Origin` the link vouches for, and are
//! stored from `Sender::Remote` under the name it declared.
//!
//! The map is its own lock, taken alone: never while `panes`, `state`,
//! `clients` or the message store is held.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::config::RemoteConfig;
use crate::protocol::{valid_host, ClientMessage, ServerMessage};

/// What a forward is answered with: the far server's own answer, or why
/// there is none.
pub type Answer = Result<ServerMessage, Undelivered>;

/// Why a forward has no answer from the far server, split by whether the far
/// server can have stored it: a retry after `Unknown` may deliver it twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undelivered {
    /// It was never written to the link.
    NotSent(String),
    /// It was written, or may have been, and no answer came.
    Unknown(String),
}

/// One send to forward, and where its answer goes.
pub struct Forward {
    pub msg: ClientMessage,
    pub reply: oneshot::Sender<Answer>,
}

/// Everything a dial needs, so the task owns it.
#[derive(Debug, Clone)]
pub struct LinkSpec {
    /// The `[remotes]` key, which is how this server names the link.
    pub key: String,
    pub remote: RemoteConfig,
    /// This server's name, declared in `PeerLink`.
    pub host_name: String,
    pub dial: Duration,
    pub ping_ms: u64,
}

/// A link that must be dialled: the caller spawns [`run_link`] with these,
/// outside the lock.
pub type Dial = (LinkSpec, mpsc::UnboundedReceiver<Forward>);

/// How many bytes of ssh's stderr are kept for the error line. The rest is
/// read and dropped, so an ssh that writes more never blocks on the pipe.
const STDERR_KEEP: usize = 4096;

/// The longest error line taken from ssh's stderr.
const STDERR_LINE_MAX: usize = 200;

/// The ping interval a `PeerLink` may declare. A declaration lives for twice
/// its interval past a connection gone quiet, holding its name, so a peer
/// cannot hold one for long by declaring a long interval, nor make a quiet
/// moment look dead by declaring a tiny one.
const DECLARED_PING_MS: std::ops::RangeInclusive<u64> = 1_000..=60_000;

/// The interval a peer declared, clamped to [`DECLARED_PING_MS`].
pub fn declared_ping(ping_ms: u64) -> Duration {
    Duration::from_millis(ping_ms.clamp(*DECLARED_PING_MS.start(), *DECLARED_PING_MS.end()))
}

/// One admitted peer link.
#[derive(Debug, Clone)]
pub struct InboundLink {
    pub client_id: u64,
    /// How often the declaring server pings, which bounds how long a
    /// declaration outlives a connection that went quiet.
    pub ping: Duration,
    /// Whether this link has already been logged for an `origin.host` that
    /// differs from its declared name, so it is logged once, not per send.
    pub host_mismatch_logged: bool,
}

/// Why a `PeerLink` was not taken.
#[derive(Debug, PartialEq, Eq)]
pub enum Declare {
    /// The name is not one a host can be.
    BadName,
    /// This connection already declared a link.
    AlreadyPeer(String),
    /// A live link holds the name. A live link is never replaced: that would
    /// hand its replies to whoever declared the name second.
    Held { retry_after: Duration },
}

#[derive(Debug, Default)]
pub struct Links {
    inbound: HashMap<String, InboundLink>,
    /// The queue of each link this server dialled, by `[remotes]` key. A
    /// closed queue is a link whose task ended, which the next forward dials
    /// again.
    outbound: HashMap<String, mpsc::UnboundedSender<Forward>>,
}

impl Links {
    pub fn new() -> Self {
        Self::default()
    }

    /// The name `client_id` declared, if its connection is a peer link.
    pub fn peer_name(&self, client_id: u64) -> Option<&str> {
        self.inbound
            .iter()
            .find(|(_, link)| link.client_id == client_id)
            .map(|(name, _)| name.as_str())
    }

    /// Take `name` for `client_id`'s connection, which the caller has already
    /// admitted.
    pub fn declare(&mut self, name: &str, client_id: u64, ping: Duration) -> Result<(), Declare> {
        if !valid_host(name) {
            return Err(Declare::BadName);
        }
        if let Some(held) = self.peer_name(client_id) {
            return Err(Declare::AlreadyPeer(held.to_string()));
        }
        if let Some(live) = self.inbound.get(name) {
            return Err(Declare::Held {
                retry_after: live.ping * 2,
            });
        }
        self.inbound.insert(
            name.to_string(),
            InboundLink {
                client_id,
                ping,
                host_mismatch_logged: false,
            },
        );
        Ok(())
    }

    /// Queue `fwd` on the link to `spec.key`: on the live one, or on a new
    /// one, returned as a [`Dial`] for the caller to start. A link being
    /// dialled takes the forward on its queue, so two sends never dial twice.
    pub fn forward(&mut self, spec: LinkSpec, fwd: Forward) -> Option<Dial> {
        let fwd = match self.outbound.get(&spec.key) {
            Some(queue) => match queue.send(fwd) {
                Ok(()) => return None,
                Err(mpsc::error::SendError(fwd)) => fwd,
            },
            None => fwd,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(fwd);
        self.outbound.insert(spec.key.clone(), tx);
        Some((spec, rx))
    }

    /// Whether this is the first `origin.host` mismatch on `client_id`'s
    /// link, marking it so the next one is not.
    pub fn first_host_mismatch(&mut self, client_id: u64) -> bool {
        match self
            .inbound
            .values_mut()
            .find(|link| link.client_id == client_id)
        {
            Some(link) if !link.host_mismatch_logged => {
                link.host_mismatch_logged = true;
                true
            }
            _ => false,
        }
    }

    /// Forget `client_id`'s link, if it had one, when its connection closes:
    /// the name it held is free again.
    pub fn drop_client(&mut self, client_id: u64) -> Option<String> {
        let name = self.peer_name(client_id)?.to_string();
        self.inbound.remove(&name);
        Some(name)
    }
}

/// The longest message from the far server this server repeats.
const FAR_MESSAGE_MAX: usize = 300;

/// A refusal's code and message from the far server, as this server repeats
/// them: the code only when it is spelled like one (else `error`), the
/// message escaped and cut to [`FAR_MESSAGE_MAX`] characters. Both are the
/// far server's to write, and land in a reply and a log.
pub fn far_refusal(code: Option<&str>, message: &str) -> (String, String) {
    let code = code
        .filter(|c| {
            (1..=32).contains(&c.len())
                && c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
        .unwrap_or("error")
        .to_string();
    let message = crate::client::cli_common::escape(message)
        .chars()
        .take(FAR_MESSAGE_MAX)
        .collect();
    (code, message)
}

/// ssh's first non-empty stderr line, escaped (it is the remote's to
/// write, and goes into a log and a reply) and cut to [`STDERR_LINE_MAX`]
/// characters.
pub fn ssh_failure_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    crate::client::cli_common::escape(line)
        .chars()
        .take(STDERR_LINE_MAX)
        .collect()
}

/// Spawn ssh. Off Linux, under [`crate::server::pty::OPEN_TO_FORK`]: a fork
/// between a pane's terminal open and its `FD_CLOEXEC` would hand that
/// terminal to ssh. `spawn` is synchronous, and nothing else is taken under
/// the guard.
fn spawn_ssh(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    #[cfg(not(target_os = "linux"))]
    let _open_to_fork = crate::server::pty::OPEN_TO_FORK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cmd.spawn()
}

/// Answer every forward still queued with `reason`, and close the queue so
/// the next forward dials again.
fn fail_queued(rx: &mut mpsc::UnboundedReceiver<Forward>, reason: &str) {
    rx.close();
    while let Ok(fwd) = rx.try_recv() {
        let _ = fwd
            .reply
            .send(Err(Undelivered::NotSent(reason.to_string())));
    }
}

/// The forwards written to one link and not yet answered, by `req`.
#[derive(Default)]
struct InFlight {
    last_req: u64,
    waiting: HashMap<u64, oneshot::Sender<Answer>>,
}

impl InFlight {
    /// Give `msg` the link's next `req`, returned.
    fn stamp(&mut self, msg: &mut ClientMessage) -> u64 {
        self.last_req += 1;
        if let ClientMessage::MsgSend { req, .. } = msg {
            *req = Some(self.last_req);
        }
        self.last_req
    }

    /// Stamp `fwd` for writing, or `None` when its caller has stopped
    /// waiting: written that late it could be stored after the caller was
    /// told the outcome is unknown, and its retry would deliver it twice.
    /// Forwards in flight whose callers have stopped waiting are dropped too.
    fn admit(&mut self, mut fwd: Forward) -> Option<(u64, Forward)> {
        self.waiting.retain(|_, reply| !reply.is_closed());
        if fwd.reply.is_closed() {
            return None;
        }
        let req = self.stamp(&mut fwd.msg);
        Some((req, fwd))
    }

    fn wait(&mut self, req: u64, reply: oneshot::Sender<Answer>) {
        self.waiting.insert(req, reply);
    }

    /// Hand an answer to the forward its `req` names. One naming no
    /// forward in flight, or none at all, is dropped.
    fn answer(&mut self, frame: ServerMessage) {
        let (ServerMessage::MsgSent { req: Some(req), .. }
        | ServerMessage::Error { req: Some(req), .. }) = frame
        else {
            return;
        };
        if let Some(reply) = self.waiting.remove(&req) {
            let _ = reply.send(Ok(frame));
        }
    }

    fn fail_all(self, reason: &str) {
        for (_, reply) in self.waiting {
            let _ = reply.send(Err(Undelivered::Unknown(reason.to_string())));
        }
    }
}

/// The task behind one outbound link: dial, then carry forwards until the
/// link is lost. Logs on state changes only, never per frame.
pub async fn run_link((spec, mut rx): Dial) {
    let key = spec.key.clone();
    log::info!("link {key}: dialing");
    let up = match dial(&spec).await {
        Ok(up) => up,
        Err(reason) => {
            log::info!("link {key}: down ({reason})");
            fail_queued(&mut rx, &reason);
            return;
        }
    };
    log::info!(
        "link {key}: up (remux {}, protocol {})",
        crate::client::cli_common::escape(&up.stamp),
        crate::protocol::PROTOCOL_VERSION
    );
    let reason = serve(&key, up, &mut rx).await;
    log::info!("link {key}: down ({reason})");
    fail_queued(&mut rx, &format!("{key}: {reason}"));
}

/// Why a dial did not bring a link up.
enum DialError {
    /// The far server answered and refused the peer link: its own reason,
    /// with no ssh line, since ssh did its part.
    Refused(String),
    /// ssh, the handshake or the connection failed.
    Failed(String),
}

impl From<String> for DialError {
    fn from(why: String) -> Self {
        DialError::Failed(why)
    }
}

impl From<&str> for DialError {
    fn from(why: &str) -> Self {
        DialError::Failed(why.to_string())
    }
}

/// The next frame this build can decode from `reader`, skipping (and logging
/// once per link, by `skipped`) any it cannot: the far server broadcasts to
/// the link as to any client, and a newer one has variants this build does
/// not know. Only EOF (`None`) or a read error ends the link.
async fn next_frame(
    key: &str,
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    skipped: &mut bool,
) -> anyhow::Result<Option<ServerMessage>> {
    loop {
        let Some(bytes) = crate::server::daemon::read_frame(reader).await? else {
            return Ok(None);
        };
        match serde_json::from_slice::<ServerMessage>(&bytes) {
            Ok(frame) => return Ok(Some(frame)),
            Err(_) if !*skipped => {
                *skipped = true;
                log::warn!(
                    "link {key}: skipping a {} frame this build cannot decode (the far server may be newer); further ones are skipped silently",
                    frame_variant(&bytes)
                );
            }
            Err(_) => {}
        }
    }
}

/// A frame's variant name for a log line, never its contents.
fn frame_variant(bytes: &[u8]) -> String {
    let name = match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(serde_json::Value::Object(map)) => map.keys().next().cloned(),
        Ok(serde_json::Value::String(name)) => Some(name),
        _ => None,
    };
    name.map_or_else(
        || "malformed".to_string(),
        |name| {
            crate::client::cli_common::escape(&name)
                .chars()
                .take(40)
                .collect()
        },
    )
}

/// Hand each frame the far server sends to `tx` until the link ends.
async fn pump_frames(
    key: String,
    mut reader: impl tokio::io::AsyncRead + Unpin,
    tx: mpsc::UnboundedSender<ServerMessage>,
) {
    let mut skipped = false;
    while let Ok(Some(frame)) = next_frame(&key, &mut reader, &mut skipped).await {
        if tx.send(frame).is_err() {
            break;
        }
    }
}

struct Up {
    stamp: String,
    _child: tokio::process::Child,
    writer: tokio::process::ChildStdin,
    frames: mpsc::UnboundedReceiver<ServerMessage>,
}

/// ssh, the handshake, and the `PeerLink` declaration, which the far server
/// answers only when it refuses: a `Pong` to the ping sent after it says it
/// was taken. All within `spec.dial`.
async fn dial(spec: &LinkSpec) -> Result<Up, String> {
    use crate::server::daemon::write_message;
    use tokio::io::AsyncReadExt;

    let key = &spec.key;
    let mut cmd = spec.remote.ssh_command(true);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = spawn_ssh(&mut cmd).map_err(|e| format!("{key}: cannot run ssh: {e}"))?;
    let (Some(mut reader), Some(mut writer), Some(mut stderr)) =
        (child.stdout.take(), child.stdin.take(), child.stderr.take())
    else {
        return Err(format!("{key}: ssh has no pipes"));
    };
    let (kept_tx, kept_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut tx = Some(kept_tx);
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if kept.len() < STDERR_KEEP {
                        let room = STDERR_KEEP - kept.len();
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                    if kept.contains(&b'\n') {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(kept.clone());
                        }
                    }
                }
            }
        }
        if let Some(tx) = tx.take() {
            let _ = tx.send(kept);
        }
    });
    let declared = tokio::time::timeout(spec.dial, async {
        let stamp = crate::client::terminal::handshake(&mut reader, &mut writer)
            .await
            .map_err(|e| DialError::from(format!("{e:#}")))?;
        let declare = ClientMessage::PeerLink {
            name: spec.host_name.clone(),
            ping_ms: spec.ping_ms,
        };
        write_message(&mut writer, &declare)
            .await
            .map_err(|e| DialError::from(format!("{e:#}")))?;
        write_message(&mut writer, &ClientMessage::Ping { nonce: 1 })
            .await
            .map_err(|e| DialError::from(format!("{e:#}")))?;
        let mut skipped = false;
        loop {
            match next_frame(key, &mut reader, &mut skipped).await {
                Ok(Some(ServerMessage::Pong { nonce: 1 })) => return Ok(stamp),
                Ok(Some(ServerMessage::Error { message, .. })) => {
                    let (_, message) = far_refusal(None, &message);
                    return Err(DialError::Refused(format!(
                        "{key} refused the peer link: {message}"
                    )));
                }
                Ok(Some(_)) => continue,
                Ok(None) => return Err("the connection closed".into()),
                Err(e) => return Err(format!("{e:#}").into()),
            }
        }
    })
    .await;
    let stamp = match declared {
        Ok(Ok(stamp)) => stamp,
        Ok(Err(DialError::Refused(why))) => return Err(why),
        failed => {
            let why = match failed {
                Ok(Err(DialError::Failed(why))) => why,
                _ => format!("no answer in {}ms", spec.dial.as_millis()),
            };
            let stderr = tokio::time::timeout(Duration::from_millis(500), kept_rx)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            let line = ssh_failure_line(&stderr);
            return Err(if line.is_empty() {
                format!("{key}: {why}")
            } else {
                format!("{key}: {why} (ssh: {line})")
            });
        }
    };
    let (frames_tx, frames) = mpsc::unbounded_channel();
    tokio::spawn(pump_frames(key.clone(), reader, frames_tx));
    Ok(Up {
        stamp,
        _child: child,
        writer,
        frames,
    })
}

/// Carry forwards until the link is lost; the reason it was.
async fn serve(key: &str, mut up: Up, rx: &mut mpsc::UnboundedReceiver<Forward>) -> String {
    use crate::server::daemon::write_message;

    let mut in_flight = InFlight::default();
    let reason = loop {
        tokio::select! {
            fwd = rx.recv() => {
                let Some(fwd) = fwd else { break "closed".to_string() };
                let Some((req, fwd)) = in_flight.admit(fwd) else { continue };
                // A failed write may still have put part or all of the
                // frame on the link.
                if let Err(e) = write_message(&mut up.writer, &fwd.msg).await {
                    let _ = fwd.reply.send(Err(Undelivered::Unknown(format!("{key}: link lost"))));
                    break format!("write failed: {e:#}");
                }
                in_flight.wait(req, fwd.reply);
            }
            frame = up.frames.recv() => match frame {
                Some(answer @ (ServerMessage::MsgSent { .. } | ServerMessage::Error { .. })) => {
                    in_flight.answer(answer);
                }
                // Pongs and the far server's broadcasts to every
                // connection: dropped, with no line per frame.
                Some(_) => {}
                None => break "link lost".to_string(),
            },
        }
    };
    in_flight.fail_all(&format!("{key}: {reason}"));
    reason
}

#[cfg(test)]
mod tests {
    use super::*;

    const PING: Duration = Duration::from_secs(15);

    fn spec(key: &str) -> LinkSpec {
        LinkSpec {
            key: key.into(),
            remote: RemoteConfig::default(),
            host_name: "laptop".into(),
            dial: Duration::from_secs(1),
            ping_ms: 1000,
        }
    }

    fn fwd() -> (Forward, oneshot::Receiver<Answer>) {
        let (reply, rx) = oneshot::channel();
        (
            Forward {
                msg: ClientMessage::Ping { nonce: 9 },
                reply,
            },
            rx,
        )
    }

    #[test]
    fn a_dial_in_progress_queues_a_second_forward_rather_than_dialing_twice() {
        let mut links = Links::new();
        let (first, _a) = fwd();
        let (second, _b) = fwd();
        let dial = links.forward(spec("mini"), first);
        assert!(dial.is_some(), "the first forward dials");
        assert!(
            links.forward(spec("mini"), second).is_none(),
            "the second queues"
        );
        let (_, mut rx) = dial.unwrap();
        assert!(rx.try_recv().is_ok() && rx.try_recv().is_ok());
    }

    #[test]
    fn a_link_whose_task_ended_is_dialled_again() {
        let mut links = Links::new();
        let (first, _a) = fwd();
        let (_, rx) = links.forward(spec("mini"), first).unwrap();
        drop(rx);
        let (second, _b) = fwd();
        assert!(links.forward(spec("mini"), second).is_some());
    }

    #[test]
    fn ssh_failure_line_takes_the_first_non_empty_line_escaped_and_bounded() {
        let mut stderr = b"\n  \nEnter passphrase \x1b[31mfor key:\nsecond\n".to_vec();
        assert_eq!(
            ssh_failure_line(&stderr),
            "Enter passphrase \\x1b[31mfor key:"
        );
        stderr = vec![b'a'; 10 * 1024];
        assert_eq!(ssh_failure_line(&stderr).chars().count(), STDERR_LINE_MAX);
        assert_eq!(ssh_failure_line(b""), "");
    }

    #[tokio::test]
    async fn queued_forwards_are_answered_when_a_dial_fails() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (a, ra) = fwd();
        tx.send(a).ok();
        fail_queued(&mut rx, "mini: ssh: denied");
        assert_eq!(
            ra.await.unwrap().unwrap_err(),
            Undelivered::NotSent("mini: ssh: denied".into())
        );
        let (b, _rb) = fwd();
        assert!(
            tx.send(b).is_err(),
            "the queue is closed, so the next forward dials again"
        );
    }

    fn sent(req: Option<u64>, pane_id: u64) -> ServerMessage {
        ServerMessage::MsgSent {
            id: 1,
            to: crate::protocol::Resolved {
                host: None,
                pane_id,
                name: None,
            },
            delivery: crate::protocol::Delivery::Mailbox,
            reason: None,
            timed_out: false,
            final_state: None,
            req,
        }
    }

    fn pane_of(answer: Answer) -> Option<u64> {
        match answer {
            Ok(ServerMessage::MsgSent { to, .. }) => Some(to.pane_id),
            _ => None,
        }
    }

    fn send_msg() -> ClientMessage {
        ClientMessage::MsgSend {
            to: crate::protocol::Address {
                host: None,
                target: crate::protocol::Target::Pane(1),
            },
            body: String::new(),
            deliver: crate::protocol::Deliver::Mailbox,
            reply_to: None,
            pane_hint: None,
            defer: false,
            timeout_ms: None,
            wait: false,
            origin: None,
            req: None,
        }
    }

    #[test]
    fn each_forward_is_stamped_with_the_links_next_req() {
        let mut in_flight = InFlight::default();
        let (mut a, mut b) = (send_msg(), send_msg());
        assert_eq!(in_flight.stamp(&mut a), 1);
        assert_eq!(in_flight.stamp(&mut b), 2);
        assert!(matches!(a, ClientMessage::MsgSend { req: Some(1), .. }));
        assert!(matches!(b, ClientMessage::MsgSend { req: Some(2), .. }));
    }

    #[tokio::test]
    async fn answers_go_to_the_forward_their_req_names_in_any_order() {
        let mut in_flight = InFlight::default();
        let (first, mut first_rx) = oneshot::channel();
        let (second, second_rx) = oneshot::channel();
        in_flight.wait(1, first);
        in_flight.wait(2, second);
        in_flight.answer(sent(Some(2), 20));
        assert!(
            first_rx.try_recv().is_err(),
            "the older forward is not handed the answer to the newer"
        );
        assert_eq!(pane_of(second_rx.await.unwrap()), Some(20));
        in_flight.answer(ServerMessage::Error {
            message: "no".into(),
            code: Some("no_such_target".into()),
            req: Some(1),
        });
        assert!(matches!(
            first_rx.await.unwrap(),
            Ok(ServerMessage::Error { req: Some(1), .. })
        ));
    }

    #[tokio::test]
    async fn a_stray_answer_is_dropped_and_answers_nobody() {
        let mut in_flight = InFlight::default();
        let (reply, mut rx) = oneshot::channel();
        in_flight.wait(5, reply);
        in_flight.answer(sent(None, 1));
        in_flight.answer(sent(Some(4), 2));
        in_flight.answer(sent(Some(6), 3));
        assert!(
            rx.try_recv().is_err(),
            "no stray reached the forward in flight"
        );
        in_flight.answer(sent(Some(5), 4));
        assert_eq!(pane_of(rx.await.unwrap()), Some(4));
        in_flight.answer(sent(Some(5), 5));
        assert!(
            in_flight.waiting.is_empty(),
            "a second answer to one req finds nobody"
        );
    }

    #[tokio::test]
    async fn a_lost_link_leaves_its_written_forwards_unknown() {
        let mut in_flight = InFlight::default();
        let (reply, rx) = oneshot::channel();
        in_flight.wait(1, reply);
        in_flight.fail_all("mini: link lost");
        assert_eq!(
            rx.await.unwrap().unwrap_err(),
            Undelivered::Unknown("mini: link lost".into())
        );
    }

    async fn write_frame(w: &mut (impl tokio::io::AsyncWrite + Unpin), json: &str) {
        use tokio::io::AsyncWriteExt;
        w.write_all(&(json.len() as u32).to_be_bytes())
            .await
            .unwrap();
        w.write_all(json.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn a_frame_this_build_cannot_decode_is_skipped_not_the_end_of_the_link() {
        let (mut far, near) = tokio::io::duplex(4096);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(pump_frames("mini".into(), near, tx));
        write_frame(&mut far, r#"{"SomeVariantFromANewerServer":{"x":1}}"#).await;
        write_frame(&mut far, r#"{"Pong":{"nonce":7}}"#).await;
        write_frame(&mut far, "not json at all").await;
        write_frame(&mut far, r#"{"Pong":{"nonce":8}}"#).await;
        drop(far);
        let mut got = Vec::new();
        while let Some(frame) = rx.recv().await {
            got.push(frame);
        }
        reader.await.unwrap();
        assert!(
            matches!(
                got.as_slice(),
                [
                    ServerMessage::Pong { nonce: 7 },
                    ServerMessage::Pong { nonce: 8 }
                ]
            ),
            "{got:?}"
        );
    }

    #[test]
    fn a_frames_variant_is_named_for_the_log_never_its_contents() {
        assert_eq!(
            frame_variant(br#"{"NewThing":{"body":"secret"}}"#),
            "NewThing"
        );
        assert_eq!(frame_variant(br#""Bare""#), "Bare");
        assert_eq!(frame_variant(b"\xff"), "malformed");
    }

    #[test]
    fn a_forward_whose_caller_stopped_waiting_is_never_written() {
        let mut in_flight = InFlight::default();
        let (gone, rx) = fwd();
        drop(rx);
        assert!(in_flight.admit(gone).is_none());
        let (live, _keep) = fwd();
        assert!(in_flight.admit(live).is_some());
    }

    #[test]
    fn forwards_in_flight_whose_callers_stopped_waiting_are_dropped() {
        let mut in_flight = InFlight::default();
        let (reply, rx) = oneshot::channel();
        in_flight.wait(1, reply);
        drop(rx);
        let (next, _keep) = fwd();
        in_flight.admit(next);
        assert!(in_flight.waiting.is_empty());
    }

    #[test]
    fn a_far_refusal_is_repeated_bounded_and_escaped() {
        assert_eq!(
            far_refusal(Some("mailbox_full"), "full"),
            ("mailbox_full".to_string(), "full".to_string())
        );
        for odd in ["Mailbox", "a b", "x\u{1b}", "", &"a".repeat(33)] {
            assert_eq!(far_refusal(Some(odd), "m").0, "error", "{odd:?}");
        }
        assert_eq!(far_refusal(None, "m").0, "error");
        let (_, long) = far_refusal(Some("x"), &"y".repeat(5000));
        assert_eq!(long.chars().count(), FAR_MESSAGE_MAX);
        let (_, esc) = far_refusal(Some("x"), "a\u{1b}[31mb\nc");
        assert!(!esc.contains('\u{1b}') && !esc.contains('\n'), "{esc:?}");
    }

    #[test]
    fn a_declared_ping_is_clamped_to_one_second_through_a_minute() {
        assert_eq!(declared_ping(0), Duration::from_secs(1));
        assert_eq!(declared_ping(999), Duration::from_secs(1));
        assert_eq!(declared_ping(15_000), Duration::from_secs(15));
        assert_eq!(declared_ping(60_000), Duration::from_secs(60));
        assert_eq!(declared_ping(u64::MAX), Duration::from_secs(60));
    }

    #[test]
    fn a_declared_link_is_found_by_its_connection() {
        let mut links = Links::new();
        assert_eq!(links.declare("laptop", 7, PING), Ok(()));
        assert_eq!(links.peer_name(7), Some("laptop"));
        assert_eq!(links.peer_name(8), None);
    }

    #[test]
    fn a_live_link_is_never_replaced() {
        let mut links = Links::new();
        links.declare("laptop", 7, PING).unwrap();
        assert_eq!(
            links.declare("laptop", 8, PING),
            Err(Declare::Held {
                retry_after: PING * 2
            })
        );
        assert_eq!(links.peer_name(7), Some("laptop"));
    }

    #[test]
    fn a_connection_declares_once_and_frees_its_name_when_it_closes() {
        let mut links = Links::new();
        links.declare("laptop", 7, PING).unwrap();
        assert_eq!(
            links.declare("other", 7, PING),
            Err(Declare::AlreadyPeer("laptop".into()))
        );
        assert_eq!(links.drop_client(7), Some("laptop".into()));
        assert_eq!(links.declare("laptop", 8, PING), Ok(()));
    }

    #[test]
    fn a_host_mismatch_is_reported_once_per_link() {
        let mut links = Links::new();
        links.declare("laptop", 7, PING).unwrap();
        assert!(links.first_host_mismatch(7));
        assert!(!links.first_host_mismatch(7));
        assert!(!links.first_host_mismatch(8), "not a link");
        links.drop_client(7);
        links.declare("laptop", 9, PING).unwrap();
        assert!(
            links.first_host_mismatch(9),
            "a new connection is a new link"
        );
    }

    #[test]
    fn a_name_that_cannot_be_a_host_is_refused() {
        let mut links = Links::new();
        for bad in [
            "-oProxyCommand=x",
            "user",
            "system-reminder",
            "my_claude",
            "",
        ] {
            assert_eq!(links.declare(bad, 7, PING), Err(Declare::BadName), "{bad}");
        }
        assert_eq!(links.declare("users", 7, PING), Ok(()));
    }
}
