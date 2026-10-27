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
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use crate::config::{Inbound, RemoteConfig};
use crate::protocol::{
    valid_host, ClientMessage, Deliver, MsgId, MsgRef, Origin, PaneId, ServerMessage, Target,
};

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
    /// A link with nothing in flight and no traffic but pings for this long
    /// is closed; the next send dials again.
    pub idle: Duration,
    /// Where a message the far server sends back over this link goes to be
    /// stored, answered with the ack to write back. `None` refuses every one.
    pub on_inbound: Option<InboundTx>,
}

/// A message that came back over a dialled link, and where its ack goes.
pub type InboundItem = (InboundRequest, oneshot::Sender<InboundAck>);
pub type InboundTx = mpsc::Sender<InboundItem>;
pub type InboundRx = mpsc::Receiver<InboundItem>;

/// How many messages that came back over one link may wait for their acks at
/// once. Past it the far server is answered `queue_full` at once, so a peer
/// pushing faster than this server stores costs a refusal, not a queue.
pub const INBOUND_IN_FLIGHT: usize = 8;

/// A `MsgInbound` that arrived over a link this server dialled, for the
/// daemon to gate by `inbound`, store, and answer.
#[derive(Debug)]
pub struct InboundRequest {
    /// The `[remotes]` key of the link it came over: the sender's host.
    pub link: String,
    pub policy: Inbound,
    pub to: Target,
    pub body: String,
    pub deliver: Deliver,
    pub reply_to: Option<MsgRef>,
    pub origin: Origin,
}

/// What an inbound message came to: stored under `id` in `pane_id`'s
/// mailbox, or refused with `code`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboundAck {
    pub id: Option<MsgId>,
    pub pane_id: Option<PaneId>,
    pub code: Option<String>,
    pub message: Option<String>,
}

impl InboundAck {
    pub fn refused(code: &str, message: impl Into<String>) -> Self {
        InboundAck {
            code: Some(code.to_string()),
            message: Some(message.into()),
            ..Self::default()
        }
    }
}

/// Where a send to `host` goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// Back over the live link `host` holds open to this server: the
    /// connection's client id.
    Inbound(u64),
    /// Over a link this server dials to `[remotes.<host>]`.
    Dial,
    /// Neither exists.
    Neither,
}

/// A live inbound link from `host` wins over a `[remotes.<host>]` entry: the
/// reply goes back over the connection the sender already holds open, and
/// no second ssh is made.
pub fn route_for(inbound: Option<u64>, has_remote: bool) -> Route {
    match (inbound, has_remote) {
        (Some(client_id), _) => Route::Inbound(client_id),
        (None, true) => Route::Dial,
        (None, false) => Route::Neither,
    }
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
    /// When it last pinged, or declared itself.
    pub last_ping: Instant,
    /// Whether this link has already been logged for an `origin.host` that
    /// differs from its declared name, so it is logged once, not per send.
    pub host_mismatch_logged: bool,
}

impl InboundLink {
    /// Whether it has pinged within twice its declared interval of `now`.
    fn alive_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_ping) <= self.ping * 2
    }
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
    /// Every connection that declared a peer link, live or since expired:
    /// one whose entry is gone and which still pings is closed, never kept
    /// alive by a pong while every message on it is refused.
    declared: std::collections::HashSet<u64>,
    /// The queue of each link this server dialled, by `[remotes]` key. A
    /// closed queue is a link whose task ended, which the next forward dials
    /// again.
    outbound: HashMap<String, mpsc::UnboundedSender<Forward>>,
    /// Messages pushed back over an inbound link, waiting for their ack, by
    /// `req`, with the connection they went out on.
    pending: HashMap<u64, (u64, oneshot::Sender<InboundAck>)>,
    next_req: u64,
}

impl Links {
    pub fn new() -> Self {
        Self::default()
    }

    /// The client id of the inbound link named `name`, if it is live at
    /// `now`: one past its expiry and not yet swept is not.
    pub fn live_inbound(&self, name: &str, now: Instant) -> Option<u64> {
        self.inbound
            .get(name)
            .filter(|link| link.alive_at(now))
            .map(|link| link.client_id)
    }

    /// A fresh `req` for a message pushed over `client_id`'s link, and where
    /// its ack will arrive.
    pub fn expect_ack(&mut self, client_id: u64) -> (u64, oneshot::Receiver<InboundAck>) {
        self.next_req += 1;
        let (tx, rx) = oneshot::channel();
        self.pending.insert(self.next_req, (client_id, tx));
        (self.next_req, rx)
    }

    /// The waiter for `req`, when the ack came over the link it was pushed
    /// on: another connection cannot answer it.
    pub fn take_ack(&mut self, client_id: u64, req: u64) -> Option<oneshot::Sender<InboundAck>> {
        match self.pending.get(&req) {
            Some((on, _)) if *on == client_id => self.pending.remove(&req).map(|(_, tx)| tx),
            _ => None,
        }
    }

    /// Stop waiting for `req`'s ack.
    pub fn forget_ack(&mut self, req: u64) {
        self.pending.remove(&req);
    }

    /// The name `client_id` declared, if its connection is a peer link that
    /// is live at `now`. One past its expiry and not yet swept is not: what
    /// it sends is refused as from any other connection.
    pub fn peer_name(&self, client_id: u64, now: Instant) -> Option<&str> {
        self.inbound
            .iter()
            .find(|(_, link)| link.client_id == client_id && link.alive_at(now))
            .map(|(name, _)| name.as_str())
    }

    /// The name `client_id` holds, live or not.
    fn held_name(&self, client_id: u64) -> Option<&str> {
        self.inbound
            .iter()
            .find(|(_, link)| link.client_id == client_id)
            .map(|(name, _)| name.as_str())
    }

    /// Take `name` for `client_id`'s connection, which the caller has already
    /// admitted.
    pub fn declare(
        &mut self,
        name: &str,
        client_id: u64,
        ping: Duration,
    ) -> Result<Option<u64>, Declare> {
        self.declare_at(name, client_id, ping, Instant::now())
    }

    /// [`Self::declare`] at `now`. A name held by an entry that has expired
    /// (no ping for twice its declared interval) is free again, and the
    /// connection that held it is returned for the caller to close; one held
    /// by a live entry is not, and says when it will be.
    pub fn declare_at(
        &mut self,
        name: &str,
        client_id: u64,
        ping: Duration,
        now: Instant,
    ) -> Result<Option<u64>, Declare> {
        if !valid_host(name) {
            return Err(Declare::BadName);
        }
        if let Some(held) = self.held_name(client_id) {
            return Err(Declare::AlreadyPeer(held.to_string()));
        }
        let mut replaced = None;
        if let Some(live) = self.inbound.get(name) {
            let expires = live.last_ping + live.ping * 2;
            if now < expires {
                return Err(Declare::Held {
                    retry_after: expires - now,
                });
            }
            replaced = Some(live.client_id);
        }
        self.declared.insert(client_id);
        self.inbound.insert(
            name.to_string(),
            InboundLink {
                client_id,
                ping,
                last_ping: now,
                host_mismatch_logged: false,
            },
        );
        Ok(replaced)
    }

    /// Whether `client_id` declared a peer link that is not live at `now`:
    /// its entry expired, swept or not yet, or another connection took the
    /// name after it did.
    pub fn lapsed(&self, client_id: u64, now: Instant) -> bool {
        self.declared.contains(&client_id)
            && !self
                .inbound
                .values()
                .any(|link| link.client_id == client_id && link.alive_at(now))
    }

    /// Note a ping on `client_id`'s link: it is alive. A ping that comes
    /// after the entry expired does not revive it, so expiry does not depend
    /// on the sweep running before the late ping arrives.
    pub fn pinged(&mut self, client_id: u64, now: Instant) {
        if let Some(link) = self
            .inbound
            .values_mut()
            .find(|link| link.client_id == client_id && link.alive_at(now))
        {
            link.last_ping = now;
        }
    }

    /// Forget every inbound link that has not pinged for twice the interval
    /// it declared, returning each name, how long it waited and its
    /// connection, which the caller closes: left open, it would answer the
    /// dialler's pings and keep its link Up while refusing every message.
    pub fn expire(&mut self, now: Instant) -> Vec<(String, Duration, u64)> {
        let mut gone = Vec::new();
        self.inbound.retain(|name, link| {
            let alive = link.alive_at(now);
            if !alive {
                gone.push((name.clone(), link.ping * 2, link.client_id));
            }
            alive
        });
        gone
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
        // Its pushes will never be acked: their waiters learn so at once.
        self.pending.retain(|_, (on, _)| *on != client_id);
        self.declared.remove(&client_id);
        let name = self.held_name(client_id)?.to_string();
        self.inbound.remove(&name);
        Some(name)
    }
}

/// The longest message from the far server this server repeats.
const FAR_MESSAGE_MAX: usize = 300;

/// Whether `code` is spelled like a refusal code, `[a-z_]{1,32}`: the only
/// code from the far server this server repeats or logs.
pub fn code_like(code: &str) -> bool {
    (1..=32).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// A refusal's code and message from the far server, as this server repeats
/// them: the code only when it is spelled like one (else `error`), the
/// message escaped and cut to [`FAR_MESSAGE_MAX`] characters. Both are the
/// far server's to write, and land in a reply and a log.
pub fn far_refusal(code: Option<&str>, message: &str) -> (String, String) {
    let code = code.filter(|c| code_like(c)).unwrap_or("error").to_string();
    let message = crate::client::cli_common::escape_bounded(message, FAR_MESSAGE_MAX);
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
    crate::client::cli_common::escape_bounded(line, STDERR_LINE_MAX)
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
    /// Whether an answer naming no forward has been reported on this link.
    stray_seen: bool,
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
    /// forward in flight, or none at all, is dropped: never handed to
    /// another forward, which would answer it with the wrong outcome. Returns
    /// whether it is the link's first such stray, to be logged once.
    fn answer(&mut self, frame: ServerMessage) -> bool {
        let (ServerMessage::MsgSent { req: Some(req), .. }
        | ServerMessage::Error { req: Some(req), .. }) = frame
        else {
            return !std::mem::replace(&mut self.stray_seen, true);
        };
        match self.waiting.remove(&req) {
            Some(reply) => {
                let _ = reply.send(Ok(frame));
                false
            }
            None => !std::mem::replace(&mut self.stray_seen, true),
        }
    }

    fn fail_all(self, reason: &str) {
        for (_, reply) in self.waiting {
            let _ = reply.send(Err(Undelivered::Unknown(reason.to_string())));
        }
    }
}

/// The task behind one outbound link: dial, then carry forwards until the
/// link is lost or idle. A link that ends is never dialled again here: the
/// next forward finds its queue gone and starts a task of its own. Logs on
/// state changes only, never per frame.
pub async fn run_link((spec, mut rx): Dial) {
    let key = spec.key.clone();
    log::info!("link {key}: dialing");
    let up = match dial(&spec).await {
        Ok(up) => up,
        Err(reason) => {
            log::warn!("link {key}: down ({reason})");
            fail_queued(&mut rx, &reason);
            return;
        }
    };
    log::info!(
        "link {key}: up (remux {}, protocol {})",
        crate::client::cli_common::escape(&up.stamp),
        crate::protocol::PROTOCOL_VERSION
    );
    match serve(&spec, up, &mut rx, crate::server::daemon::links()).await {
        Ended::Lost(reason) => {
            log::info!("link {key}: down ({reason})");
            fail_queued(&mut rx, &format!("{key}: {reason}"));
        }
        Ended::Idle => log::info!("link {key}: idle, closed"),
    }
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

/// How many frames from the far server wait for the link task. A bound,
/// not a convenience: a far server that floods the link and stops reading
/// its own input stalls the link task's writes, and only a full queue
/// pushes back through ssh rather than growing without end.
const FRAMES_QUEUE: usize = 64;

/// Hand each frame the far server sends to `tx` until the link ends,
/// waiting while `tx` is full.
async fn pump_frames(
    key: String,
    mut reader: impl tokio::io::AsyncRead + Unpin,
    tx: mpsc::Sender<ServerMessage>,
) {
    let mut skipped = false;
    while let Ok(Some(frame)) = next_frame(&key, &mut reader, &mut skipped).await {
        if tx.send(frame).await.is_err() {
            break;
        }
    }
}

struct Up {
    stamp: String,
    /// ssh, killed when the link ends. `None` only in tests, which play the
    /// far side on an in-memory stream.
    _child: Option<tokio::process::Child>,
    writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    frames: mpsc::Receiver<ServerMessage>,
}

/// ssh, the handshake, and the `PeerLink` declaration, which the far server
/// answers only when it refuses: a `Pong` to the ping sent after it says it
/// was taken. All within `spec.dial`.
async fn dial(spec: &LinkSpec) -> Result<Up, String> {
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
    let declared = tokio::time::timeout(
        spec.dial,
        set_up(key, &spec.host_name, spec.ping_ms, &mut reader, &mut writer),
    )
    .await;
    let stamp = match declared {
        Ok(Ok(stamp)) => stamp,
        Ok(Err(DialError::Refused(why))) => return Err(why),
        failed => {
            let why = match failed {
                Ok(Err(DialError::Failed(why))) => why,
                _ => format!(
                    "dial timed out after {}s",
                    spec.dial.as_millis() as f64 / 1000.0
                ),
            };
            let stderr = tokio::time::timeout(Duration::from_millis(500), kept_rx)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            return Err(dial_failure(
                key,
                &spec.remote,
                &why,
                &ssh_failure_line(&stderr),
            ));
        }
    };
    let (frames_tx, frames) = mpsc::channel(FRAMES_QUEUE);
    tokio::spawn(pump_frames(key.clone(), reader, frames_tx));
    Ok(Up {
        stamp,
        _child: Some(child),
        writer: Box::new(writer),
        frames,
    })
}

/// Why a dial failed when the far side took the handshake and then closed.
/// A far server too old to know `PeerLink` drops the connection on it, but
/// a server that stopped at that moment looks the same from here.
const SETUP_EOF: &str = "the far side closed the link during setup: an older remux that does not know peer links, or a server that went away";

/// The handshake, the `PeerLink` declaration and the barrier ping, on a
/// connection that is ssh's in production and a test's own stream in
/// tests: the remote's build stamp once the far server has taken the link.
async fn set_up<R, W>(
    key: &str,
    host_name: &str,
    ping_ms: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<String, DialError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use crate::server::daemon::write_message;
    let stamp = crate::client::terminal::handshake(reader, writer)
        .await
        .map_err(|e| DialError::from(format!("{e:#}")))?;
    let declare = ClientMessage::PeerLink {
        name: host_name.to_string(),
        ping_ms,
    };
    write_message(writer, &declare)
        .await
        .map_err(|e| DialError::from(format!("{e:#}")))?;
    write_message(writer, &ClientMessage::Ping { nonce: 1 })
        .await
        .map_err(|e| DialError::from(format!("{e:#}")))?;
    let mut skipped = false;
    loop {
        match next_frame(key, reader, &mut skipped).await {
            Ok(Some(ServerMessage::Pong { nonce: 1 })) => return Ok(stamp),
            Ok(Some(ServerMessage::Error { message, .. })) => {
                let (_, message) = far_refusal(None, &message);
                return Err(DialError::Refused(format!(
                    "{key} refused the peer link: {message}"
                )));
            }
            // The link is not up until the Pong: a message pushed before it
            // is refused, never stored or left unanswered.
            Ok(Some(ServerMessage::MsgInbound { req, .. })) => {
                let ack = inbound_ack(
                    req,
                    InboundAck::refused("route_unavailable", "link not up yet"),
                );
                write_message(writer, &ack)
                    .await
                    .map_err(|e| DialError::from(format!("{e:#}")))?;
            }
            Ok(Some(_)) => continue,
            Ok(None) => return Err(SETUP_EOF.into()),
            Err(e) => return Err(format!("{e:#}").into()),
        }
    }
}

/// What a failed dial is answered with: the reason, escaped (part of it is
/// the far side's to write), ssh's first stderr line when it wrote one, and
/// then what to do, since what ssh says on stderr is a prompt or a refusal
/// the server, which has no terminal, cannot answer.
fn dial_failure(key: &str, remote: &RemoteConfig, why: &str, ssh_line: &str) -> String {
    let why = crate::client::cli_common::escape_bounded(why, WHY_MAX);
    if ssh_line.is_empty() {
        return format!("{key}: {why}");
    }
    match ssh_fix(remote, ssh_line) {
        Some(fix) => format!("{key}: {why} (ssh: {ssh_line}); fix: {fix}"),
        None => format!("{key}: {why} (ssh: {ssh_line})"),
    }
}

/// The longest dial failure reason kept. A decode error quotes the far
/// side's bytes, so without a bound the far side sets the reason's length.
const WHY_MAX: usize = 300;

/// What to do about the ssh failure in `ssh_line`, when there is something
/// to do. The ControlMaster and agent advice is for authentication only:
/// for a refused connection or a name that does not resolve it sends the
/// user to fix a key that is not the problem.
fn ssh_fix(remote: &RemoteConfig, ssh_line: &str) -> Option<String> {
    const AUTH: [&str; 4] = [
        "Permission denied",
        "passphrase",
        "publickey",
        "Too many authentication failures",
    ];
    if AUTH.iter().any(|token| ssh_line.contains(token)) {
        Some(
            "connect the remote from the client once with ControlMaster, or start the server where ssh-agent is available (note: panes inherit it)"
                .to_string(),
        )
    } else if ssh_line.contains("Host key verification failed") {
        let port = remote.port.map(|p| format!("-p {p} ")).unwrap_or_default();
        Some(format!(
            "accept the host key once with `ssh {port}{}`",
            crate::client::cli_common::escape(&remote.ssh)
        ))
    } else {
        None
    }
}

/// What a link's ping timer says to do on a tick.
#[derive(Debug, PartialEq, Eq)]
enum Tick {
    /// Send this ping.
    Ping(u64),
    /// The last two pings went unanswered: the link is lost.
    Lost,
    /// Nothing but pings for the idle time, and nothing in flight.
    Idle,
}

/// A dialled link's liveness: the ping it waits for, how many in a row went
/// unanswered, and when anything but a ping last crossed it. A link is only
/// known to be dead by asking it: a far server that died with its relay
/// alive, or a relay that stopped, never closes the connection.
struct Liveness {
    idle: Duration,
    /// The dial's barrier ping was nonce 1.
    next_nonce: u64,
    /// The last two pings sent, newest first: a pong for either counts, so
    /// a round trip longer than the interval is slow, not dead.
    recent: [Option<u64>; 2],
    /// Whether a valid pong arrived since the last tick.
    answered: bool,
    missed: u8,
    last_traffic: Instant,
}

impl Liveness {
    fn new(idle: Duration, now: Instant) -> Self {
        Liveness {
            idle,
            next_nonce: 2,
            recent: [None, None],
            answered: false,
            missed: 0,
            last_traffic: now,
        }
    }

    /// A frame other than a ping or a pong crossed the link.
    fn traffic(&mut self, now: Instant) {
        self.last_traffic = now;
    }

    /// A pong: it counts only when it answers one of the last two pings
    /// sent, never one this link did not send.
    fn pong(&mut self, nonce: u64) {
        if self.recent.contains(&Some(nonce)) {
            self.answered = true;
        }
    }

    /// One ping interval has passed. `busy` is whether anything is in
    /// flight, which holds a link open however quiet it is. `Idle` changes
    /// nothing, so a caller that finds work at the last moment ticks again
    /// as busy and gets this interval's ping.
    fn tick(&mut self, now: Instant, busy: bool) -> Tick {
        if !busy && now.saturating_duration_since(self.last_traffic) >= self.idle {
            return Tick::Idle;
        }
        if self.recent[0].is_some() {
            if self.answered {
                self.missed = 0;
            } else {
                self.missed += 1;
                if self.missed >= 2 {
                    return Tick::Lost;
                }
            }
        }
        self.answered = false;
        let nonce = self.next_nonce;
        self.next_nonce += 1;
        self.recent = [Some(nonce), self.recent[0]];
        Tick::Ping(nonce)
    }
}

/// How a served link ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    Lost(String),
    Idle,
}

/// The ack to write back for the `MsgInbound` `req`.
fn inbound_ack(req: u64, ack: InboundAck) -> ClientMessage {
    ClientMessage::MsgInboundAck {
        req,
        id: ack.id,
        pane_id: ack.pane_id,
        code: ack.code,
        message: ack.message,
    }
}

/// The messages that came back over one link and wait for their acks.
#[derive(Default)]
struct InboundSlots {
    taken: usize,
}

impl InboundSlots {
    /// Hand `request` to `on_inbound` for its ack, or the refusal to write
    /// back at once: no handler, [`INBOUND_IN_FLIGHT`] already waiting on
    /// this link, or the handler's queue full. A taken slot is given back by
    /// [`Self::release`] when its ack is written.
    fn admit(
        &mut self,
        on_inbound: Option<&InboundTx>,
        request: InboundRequest,
    ) -> Result<oneshot::Receiver<InboundAck>, InboundAck> {
        let Some(tx) = on_inbound else {
            return Err(InboundAck::refused(
                "origin_not_allowed",
                "this server takes no messages over its links",
            ));
        };
        if self.taken >= INBOUND_IN_FLIGHT {
            return Err(InboundAck::refused(
                "queue_full",
                format!("{INBOUND_IN_FLIGHT} messages already wait on this link"),
            ));
        }
        let (reply, answered) = oneshot::channel();
        if tx.try_send((request, reply)).is_err() {
            return Err(InboundAck::refused(
                "queue_full",
                "this server is storing too many messages from its links",
            ));
        }
        self.taken += 1;
        Ok(answered)
    }

    fn release(&mut self) {
        self.taken = self.taken.saturating_sub(1);
    }
}

/// Write `frame` to the link, or fail when the far side has not taken it
/// within `limit`: a far side that stopped reading (a stopped relay, a full
/// pipe) would otherwise hold the link task in its write for ever, where no
/// ping timer can see it.
async fn write_within(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    frame: &ClientMessage,
    limit: Duration,
) -> Result<(), String> {
    match tokio::time::timeout(limit, crate::server::daemon::write_message(writer, frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("write failed: {e:#}")),
        Err(_) => Err(format!("write stalled for {}ms", limit.as_millis())),
    }
}

/// Carry forwards until the link is lost or idle, hand each message the far
/// server sends back to `spec.on_inbound`, and ping it every `ping_ms`.
async fn serve(
    spec: &LinkSpec,
    mut up: Up,
    rx: &mut mpsc::UnboundedReceiver<Forward>,
    links: &tokio::sync::Mutex<Links>,
) -> Ended {
    let key = spec.key.as_str();
    // Tokio's clock, which is std's outside a test that pauses time.
    let now = || tokio::time::Instant::now().into_std();
    let interval = crate::server::links::declared_ping(spec.ping_ms);
    // Long enough for a slow far side to drain a frame, short enough that a
    // stopped one is noticed.
    let stall = (interval * 2).max(Duration::from_secs(10));
    let mut liveness = Liveness::new(spec.idle, now());
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut in_flight = InFlight::default();
    let mut slots = InboundSlots::default();
    // Every ack comes from a taken slot, so the slots bound this channel.
    let (acks_tx, mut acks) = mpsc::channel::<ClientMessage>(INBOUND_IN_FLIGHT);
    let ended = loop {
        tokio::select! {
            _ = ticks.tick() => {
                let busy = !in_flight.waiting.is_empty() || slots.taken > 0 || !rx.is_empty();
                let tick = match liveness.tick(now(), busy) {
                    // Decided under the lock that `Links::forward` queues
                    // under, so a forward is either queued before the check
                    // and carried here, or finds no queue after it and dials
                    // a link of its own. Never queued on a link that is
                    // closing, which would answer it unknown once written.
                    Tick::Idle => {
                        let mut links = links.lock().await;
                        if rx.is_empty() {
                            rx.close();
                            links.outbound.remove(key);
                            break Ended::Idle;
                        }
                        liveness.tick(now(), true)
                    }
                    tick => tick,
                };
                match tick {
                    Tick::Lost => break Ended::Lost("2 pongs missed".to_string()),
                    Tick::Idle => break Ended::Idle,
                    Tick::Ping(nonce) => {
                        if let Err(e) = write_within(&mut up.writer, &ClientMessage::Ping { nonce }, stall).await {
                            break Ended::Lost(e);
                        }
                    }
                }
            }
            Some(ack) = acks.recv() => {
                slots.release();
                liveness.traffic(now());
                if let Err(e) = write_within(&mut up.writer, &ack, stall).await {
                    break Ended::Lost(e);
                }
            }
            fwd = rx.recv() => {
                let Some(fwd) = fwd else { break Ended::Lost("closed".to_string()) };
                let Some((req, fwd)) = in_flight.admit(fwd) else { continue };
                liveness.traffic(now());
                // A failed write may still have put part or all of the
                // frame on the link.
                if let Err(e) = write_within(&mut up.writer, &fwd.msg, stall).await {
                    let _ = fwd.reply.send(Err(Undelivered::Unknown(format!("{key}: link lost"))));
                    break Ended::Lost(e);
                }
                in_flight.wait(req, fwd.reply);
            }
            frame = up.frames.recv() => match frame {
                Some(ServerMessage::Pong { nonce }) => liveness.pong(nonce),
                Some(answer @ (ServerMessage::MsgSent { .. } | ServerMessage::Error { .. })) => {
                    liveness.traffic(now());
                    if in_flight.answer(answer) {
                        log::info!("link {key}: dropping an answer with no req, or one naming no send in flight (logged once per link)");
                    }
                }
                Some(ServerMessage::MsgInbound { to, body, deliver, reply_to, origin, req }) => {
                    liveness.traffic(now());
                    let request = InboundRequest {
                        link: key.to_string(),
                        policy: spec.remote.inbound,
                        to,
                        body,
                        deliver,
                        reply_to,
                        origin,
                    };
                    match slots.admit(spec.on_inbound.as_ref(), request) {
                        Ok(answered) => {
                            let acks_tx = acks_tx.clone();
                            tokio::spawn(async move {
                                let ack = answered.await.unwrap_or_else(|_| {
                                    InboundAck::refused("route_unavailable", "the receiving server could not take it")
                                });
                                let _ = acks_tx.send(inbound_ack(req, ack)).await;
                            });
                        }
                        Err(refused) => {
                            if let Err(e) = write_within(&mut up.writer, &inbound_ack(req, refused), stall).await {
                                break Ended::Lost(e);
                            }
                        }
                    }
                }
                // The far server's broadcasts to every connection: dropped,
                // with no line per frame, and not traffic.
                Some(_) => {}
                None => break Ended::Lost("link lost".to_string()),
            },
        }
    };
    let reason = match &ended {
        Ended::Lost(reason) => reason.clone(),
        Ended::Idle => "closed while idle".to_string(),
    };
    in_flight.fail_all(&format!("{key}: {reason}"));
    ended
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
            idle: Duration::from_secs(300),
            on_inbound: None,
        }
    }

    #[test]
    fn route_for_prefers_a_live_inbound_link_over_a_remotes_entry() {
        assert_eq!(route_for(Some(7), true), Route::Inbound(7));
        assert_eq!(route_for(Some(7), false), Route::Inbound(7));
    }

    #[test]
    fn route_for_falls_back_to_remotes() {
        assert_eq!(route_for(None, true), Route::Dial);
    }

    #[test]
    fn route_for_names_both_misses() {
        assert_eq!(route_for(None, false), Route::Neither);
    }

    #[test]
    fn an_ack_is_taken_only_from_the_link_it_was_pushed_on() {
        let mut links = Links::new();
        let (req, _rx) = links.expect_ack(7);
        assert!(links.take_ack(8, req).is_none());
        assert!(links.take_ack(7, req).is_some());
        assert!(links.take_ack(7, req).is_none(), "taken once");
    }

    #[tokio::test]
    async fn a_closed_link_fails_its_pending_acks_at_once() {
        let mut links = Links::new();
        links.declare("laptop", 7, PING).unwrap();
        let (_, rx) = links.expect_ack(7);
        links.drop_client(7);
        assert!(rx.await.is_err());
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
        stderr = vec![0x1b; 1024];
        let line = ssh_failure_line(&stderr);
        assert!(
            line.chars().count() <= STDERR_LINE_MAX && line.ends_with("\\x1b..."),
            "whole escapes only, with no partial one before the cut: {line}"
        );
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
        assert!(
            in_flight.answer(sent(None, 1)),
            "the first stray is reported"
        );
        assert!(!in_flight.answer(sent(Some(4), 2)), "...and only the first");
        assert!(!in_flight.answer(sent(Some(6), 3)));
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
        let (tx, mut rx) = mpsc::channel(4);
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

    #[tokio::test]
    async fn a_full_frame_queue_stops_the_reader_until_it_is_drained() {
        let (mut far, near) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel(1);
        let reader = tokio::spawn(pump_frames("mini".into(), near, tx));
        for nonce in 0..3 {
            write_frame(&mut far, &format!(r#"{{"Pong":{{"nonce":{nonce}}}}}"#)).await;
        }
        drop(far);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!reader.is_finished(), "the reader waits on the full queue");
        let mut got = Vec::new();
        while let Some(frame) = rx.recv().await {
            got.push(frame);
        }
        reader.await.unwrap();
        assert_eq!(got.len(), 3, "nothing was dropped to make room");
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
        let (_, cut) = far_refusal(
            Some("x"),
            &format!("{}{}", "y".repeat(295), "\u{1b}".repeat(5)),
        );
        assert_eq!(
            cut,
            format!("{}...", "y".repeat(295)),
            "an escape that does not fit whole is dropped, never cut to `\\x1`"
        );
    }

    fn inbound_request() -> InboundRequest {
        InboundRequest {
            link: "mini".into(),
            policy: crate::config::Inbound::Replies,
            to: crate::protocol::Target::Pane(1),
            body: "RE".into(),
            deliver: crate::protocol::Deliver::Mailbox,
            reply_to: None,
            origin: Origin {
                host: "mini".into(),
                pane_id: 2,
                name: None,
                session: "main".into(),
                id: 1,
            },
        }
    }

    #[test]
    fn a_link_holds_at_most_its_cap_of_inbound_messages_waiting() {
        let (tx, _rx) = mpsc::channel(64);
        let mut slots = InboundSlots::default();
        let mut waiting = Vec::new();
        for _ in 0..INBOUND_IN_FLIGHT {
            waiting.push(
                slots
                    .admit(Some(&tx), inbound_request())
                    .expect("under the cap"),
            );
        }
        let over = slots.admit(Some(&tx), inbound_request()).unwrap_err();
        assert_eq!(over.code.as_deref(), Some("queue_full"));
        slots.release();
        assert!(
            slots.admit(Some(&tx), inbound_request()).is_ok(),
            "an ack frees a slot"
        );
    }

    #[test]
    fn a_full_handler_queue_is_refused_not_waited_on() {
        let (tx, _rx) = mpsc::channel(1);
        let mut slots = InboundSlots::default();
        assert!(slots.admit(Some(&tx), inbound_request()).is_ok());
        let full = slots.admit(Some(&tx), inbound_request()).unwrap_err();
        assert_eq!(full.code.as_deref(), Some("queue_full"));
        assert_eq!(slots.taken, 1, "a refused one takes no slot");
        let none = slots.admit(None, inbound_request()).unwrap_err();
        assert_eq!(none.code.as_deref(), Some("origin_not_allowed"));
    }

    #[test]
    fn a_code_is_lowercase_letters_and_underscores() {
        assert!(code_like("mailbox_full"));
        for odd in ["", "Mailbox", "a1", "a b", &"a".repeat(33)] {
            assert!(!code_like(odd), "{odd:?}");
        }
    }

    const SEC: Duration = Duration::from_secs(1);

    #[test]
    fn two_missed_pongs_bring_the_link_down() {
        let t0 = Instant::now();
        let mut live = Liveness::new(SEC * 300, t0);
        assert_eq!(live.tick(t0 + SEC, true), Tick::Ping(2));
        assert_eq!(live.tick(t0 + SEC * 2, true), Tick::Ping(3), "one missed");
        assert_eq!(live.tick(t0 + SEC * 3, true), Tick::Lost, "two missed");
    }

    #[test]
    fn a_matching_pong_resets_missed() {
        let t0 = Instant::now();
        let mut live = Liveness::new(SEC * 300, t0);
        assert_eq!(live.tick(t0 + SEC, true), Tick::Ping(2));
        assert_eq!(live.tick(t0 + SEC * 2, true), Tick::Ping(3));
        live.pong(3);
        assert_eq!(live.tick(t0 + SEC * 3, true), Tick::Ping(4));
        assert_eq!(
            live.tick(t0 + SEC * 4, true),
            Tick::Ping(5),
            "one missed again, not two"
        );
    }

    #[test]
    fn a_stale_nonce_is_not_a_pong() {
        let t0 = Instant::now();
        let mut live = Liveness::new(SEC * 300, t0);
        assert_eq!(live.tick(t0 + SEC, true), Tick::Ping(2));
        assert_eq!(live.tick(t0 + SEC * 2, true), Tick::Ping(3));
        live.pong(0);
        live.pong(1);
        live.pong(9);
        assert_eq!(live.tick(t0 + SEC * 3, true), Tick::Lost);
    }

    /// The far server of [`serve`]'s tests: a pong for every ping, noting
    /// when each came in `pings`, and an answer naming its `req` for every
    /// forwarded send.
    async fn far_server(
        stream: tokio::io::DuplexStream,
        frames: mpsc::Sender<ServerMessage>,
        pings: std::sync::Arc<std::sync::Mutex<Vec<tokio::time::Instant>>>,
    ) {
        let (mut reader, _writer) = tokio::io::split(stream);
        while let Ok(Some(frame)) =
            crate::server::daemon::read_message::<ClientMessage>(&mut reader).await
        {
            let answer = match frame {
                ClientMessage::Ping { nonce } => {
                    pings.lock().unwrap().push(tokio::time::Instant::now());
                    ServerMessage::Pong { nonce }
                }
                ClientMessage::MsgSend { req, .. } => ServerMessage::Error {
                    message: "stored".into(),
                    code: Some("stored".into()),
                    req,
                },
                _ => continue,
            };
            if frames.send(answer).await.is_err() {
                return;
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_forward_queued_at_the_idle_tick_is_carried_on_the_same_link() {
        let links = std::sync::Arc::new(tokio::sync::Mutex::new(Links::new()));
        let spec = LinkSpec {
            idle: Duration::from_secs(2),
            ..spec("mini")
        };
        let (first, first_answer) = oneshot::channel();
        let forward = |reply| Forward {
            msg: send_msg(),
            reply,
        };
        let Some((spec, mut rx)) = links.lock().await.forward(spec, forward(first)) else {
            panic!("the first forward dials");
        };
        let (near, far) = tokio::io::duplex(64 * 1024);
        let (frames_tx, frames) = mpsc::channel(16);
        let pings = std::sync::Arc::default();
        tokio::spawn(far_server(far, frames_tx, std::sync::Arc::clone(&pings)));
        let up = Up {
            stamp: String::new(),
            _child: None,
            writer: Box::new(near),
            frames,
        };
        let serving = {
            let (links, spec) = (std::sync::Arc::clone(&links), spec.clone());
            tokio::spawn(async move { serve(&spec, up, &mut rx, &links).await })
        };
        assert!(first_answer.await.unwrap().is_ok(), "the first is answered");

        // Held across the idle tick and one interval past it, so that the
        // link decides it is idle while a forward is being queued on it.
        let interval = declared_ping(spec.ping_ms);
        let mut held = links.lock().await;
        tokio::time::sleep(spec.idle + interval + Duration::from_millis(100)).await;
        let silent_since = pings.lock().unwrap().last().copied();
        assert!(
            silent_since.is_some_and(|t| t.elapsed() > interval),
            "no ping for over an interval: the link is parked on the lock at its idle tick"
        );
        let (second, second_answer) = oneshot::channel();
        assert!(
            held.forward(spec.clone(), forward(second)).is_none(),
            "the second forward is queued on the live link, not given a new dial"
        );
        let released = tokio::time::Instant::now();
        drop(held);
        assert!(
            matches!(second_answer.await, Ok(Ok(ServerMessage::Error { .. }))),
            "the second forward is written and answered by the far server, never failed"
        );
        assert!(!serving.is_finished(), "the link stays up for it");

        // The kept tick pings at once, and every interval after: a skipped
        // one leaves the far server two intervals without a ping, which is
        // when it expires the link.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let after: Vec<_> = pings
            .lock()
            .unwrap()
            .iter()
            .filter(|&&t| t >= released)
            .copied()
            .collect();
        let widest = after.windows(2).map(|w| w[1] - w[0]).max();
        assert!(
            after.first() == Some(&released) && after.len() >= 3 && widest < Some(interval * 2),
            "the kept tick's ping, then one every interval: {after:?} from {released:?}"
        );
    }

    #[test]
    fn an_idle_tick_changes_nothing_so_a_kept_link_counts_no_phantom_miss() {
        let t0 = Instant::now();
        let mut live = Liveness::new(SEC * 2, t0);
        let Tick::Ping(first) = live.tick(t0 + SEC, false) else {
            panic!("the first tick pings");
        };
        live.pong(first);
        assert_eq!(live.tick(t0 + SEC * 2, false), Tick::Idle);
        assert!(
            matches!(live.tick(t0 + SEC * 2, true), Tick::Ping(_)),
            "kept: the same interval's ping"
        );
        assert!(
            matches!(live.tick(t0 + SEC * 3, true), Tick::Ping(_)),
            "one unanswered ping is one miss, not two"
        );
    }

    #[test]
    fn a_round_trip_longer_than_the_interval_is_slow_not_dead() {
        let t0 = Instant::now();
        let mut live = Liveness::new(SEC * 300, t0);
        let mut sent = vec![];
        for n in 1..=6 {
            match live.tick(t0 + SEC * n, true) {
                Tick::Ping(nonce) => sent.push(nonce),
                other => panic!("tick {n}: {other:?}"),
            }
            // Each pong arrives a tick and a half late: for the ping before
            // this one.
            if sent.len() >= 2 {
                live.pong(sent[sent.len() - 2]);
            }
        }
    }

    #[test]
    fn idle_ignores_pings_and_waits_for_nothing_in_flight() {
        let t0 = Instant::now();
        let mut live = Liveness::new(SEC * 4, t0);
        for n in 1..=3 {
            let Tick::Ping(nonce) = live.tick(t0 + SEC * n, false) else {
                panic!("tick {n}");
            };
            live.pong(nonce);
        }
        assert!(
            matches!(live.tick(t0 + SEC * 4, true), Tick::Ping(_)),
            "busy holds it open"
        );
        live.pong(5);
        assert_eq!(
            live.tick(t0 + SEC * 5, false),
            Tick::Idle,
            "answered pings are not traffic"
        );
        let mut used = Liveness::new(SEC * 4, t0);
        used.traffic(t0 + SEC * 3);
        assert!(matches!(used.tick(t0 + SEC * 5, false), Tick::Ping(_)));
    }

    #[test]
    fn an_inbound_link_expires_without_pings() {
        let mut links = Links::new();
        let t0 = Instant::now();
        links.declare_at("laptop", 7, SEC, t0).unwrap();
        links.pinged(7, t0 + SEC);
        assert!(
            links.expire(t0 + SEC * 3).is_empty(),
            "pinged at 1s, so alive until 3s"
        );
        assert_eq!(
            links.expire(t0 + SEC * 3 + SEC / 10),
            vec![("laptop".to_string(), SEC * 2, 7)]
        );
        assert_eq!(links.peer_name(7, Instant::now()), None);
    }

    #[test]
    fn an_expired_link_not_yet_swept_is_no_peer_and_no_route() {
        let mut links = Links::new();
        let t0 = Instant::now();
        links.declare_at("laptop", 7, SEC, t0).unwrap();
        assert_eq!(links.peer_name(7, t0 + SEC), Some("laptop"));
        assert_eq!(links.live_inbound("laptop", t0 + SEC), Some(7));
        let late = t0 + SEC * 2 + SEC / 2;
        assert_eq!(links.peer_name(7, late), None, "its sends are refused");
        assert_eq!(
            links.live_inbound("laptop", late),
            None,
            "no reply is routed to it"
        );
    }

    #[test]
    fn a_ping_after_expiry_does_not_revive_the_link() {
        let mut links = Links::new();
        let t0 = Instant::now();
        links.declare_at("laptop", 7, SEC, t0).unwrap();
        let late = t0 + SEC * 2 + SEC / 2;
        links.pinged(7, late);
        assert!(
            links.lapsed(7, late),
            "past expiry and not yet swept, it is lapsed"
        );
        assert_eq!(
            links.expire(late + SEC / 100),
            vec![("laptop".to_string(), SEC * 2, 7)],
            "the sweep after the late ping still expires it"
        );
    }

    #[test]
    fn a_held_name_is_refused_until_it_expires_then_free() {
        let mut links = Links::new();
        let t0 = Instant::now();
        links.declare_at("laptop", 7, SEC, t0).unwrap();
        assert_eq!(
            links.declare_at("laptop", 8, SEC, t0 + SEC),
            Err(Declare::Held { retry_after: SEC })
        );
        assert_eq!(
            links.declare_at("laptop", 8, SEC, t0 + SEC * 2),
            Ok(Some(7)),
            "the expired holder's connection is handed back to be closed"
        );
        assert_eq!(links.peer_name(8, Instant::now()), Some("laptop"));
        assert!(
            links.lapsed(7, t0 + SEC * 2),
            "7 declared, and holds nothing now"
        );
        assert!(!links.lapsed(8, t0 + SEC * 2));
        assert!(!links.lapsed(9, t0 + SEC * 2), "9 never declared");
    }

    /// The far side of a handshake: read the Hello, answer a Welcome at
    /// `version`, then read one frame and close.
    async fn far_side(stream: tokio::io::DuplexStream, version: u32) {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let _: Option<crate::protocol::Hello> = crate::server::daemon::read_message(&mut reader)
            .await
            .unwrap();
        let welcome = crate::protocol::Welcome {
            protocol_version: version,
            remux_version: "0.1.0+old\x1bt".into(),
        };
        crate::server::daemon::write_message(&mut writer, &welcome)
            .await
            .unwrap();
        let _ = crate::server::daemon::read_frame(&mut reader).await;
    }

    async fn set_up_against(version: u32) -> Result<String, DialError> {
        let (near, far) = tokio::io::duplex(64 * 1024);
        tokio::spawn(far_side(far, version));
        let (mut reader, mut writer) = tokio::io::split(near);
        set_up("mini", "laptop", 1000, &mut reader, &mut writer).await
    }

    #[tokio::test]
    async fn a_skewed_welcome_names_both_versions() {
        let Err(DialError::Failed(why)) = set_up_against(16).await else {
            panic!("a skewed handshake must fail");
        };
        let message = dial_failure("mini", &remote("box", None), &why, "");
        assert!(
            message.contains(&format!(
                "remote protocol v16, local v{}",
                crate::protocol::PROTOCOL_VERSION
            )) && message.contains("0.1.0+old"),
            "{message}"
        );
        assert!(
            !message.contains('\u{1b}') && message.contains("(remote remux 0.1.0+old\\x1bt)"),
            "the far side's stamp is escaped once, not twice: {message:?}"
        );
    }

    #[tokio::test]
    async fn eof_during_setup_is_named_as_an_old_peer() {
        let Err(DialError::Failed(why)) = set_up_against(crate::protocol::PROTOCOL_VERSION).await
        else {
            panic!("a peer that closes on PeerLink must fail");
        };
        assert_eq!(why, SETUP_EOF);
    }

    fn remote(ssh: &str, port: Option<u16>) -> RemoteConfig {
        RemoteConfig {
            ssh: ssh.into(),
            port,
            ..RemoteConfig::default()
        }
    }

    const AUTH_FIX: &str = "; fix: connect the remote from the client once with ControlMaster, or start the server where ssh-agent is available (note: panes inherit it)";

    #[test]
    fn an_auth_failure_says_to_reach_an_agent() {
        for line in [
            "user@host: Permission denied (publickey,password).",
            "Enter passphrase for key '/home/u/.ssh/id_ed25519':",
            "Received disconnect from 10.0.0.1 port 22:2: Too many authentication failures",
            &ssh_failure_line(b"Permission \x1b[31mdenied\x1b[0m (publickey)\n"),
        ] {
            let message = dial_failure("mini", &remote("box", None), "reading Welcome", line);
            assert!(
                message.ends_with(AUTH_FIX) && message.contains(&format!("(ssh: {line})")),
                "{message}"
            );
        }
    }

    #[test]
    fn a_host_key_failure_says_to_accept_the_key() {
        let line = "Host key verification failed.";
        assert_eq!(
            dial_failure("mini", &remote("me@box\u{1b}", None), "reading Welcome", line),
            "mini: reading Welcome (ssh: Host key verification failed.); fix: accept the host key once with `ssh me@box\\x1b`"
        );
        assert_eq!(
            dial_failure("mini", &remote("me@box", Some(2222)), "reading Welcome", line),
            "mini: reading Welcome (ssh: Host key verification failed.); fix: accept the host key once with `ssh -p 2222 me@box`",
            "the remote's port is part of the command, or it checks the key of another sshd"
        );
    }

    #[test]
    fn any_other_ssh_failure_has_no_fix() {
        for line in [
            "ssh: connect to host box port 22: Connection refused",
            "ssh: Could not resolve hostname box: Name or service not known",
        ] {
            assert_eq!(
                dial_failure("mini", &remote("box", None), "reading Welcome", line),
                format!("mini: reading Welcome (ssh: {line})")
            );
        }
    }

    #[test]
    fn a_dial_failure_reason_is_escaped_and_bounded() {
        let box_ = remote("box", None);
        assert_eq!(dial_failure("mini", &box_, "x\u{1b}", ""), "mini: x\\x1b");
        let long = "é".repeat(WHY_MAX * 2);
        let message = dial_failure("mini", &box_, &long, "");
        assert_eq!(
            message,
            format!("mini: {}...", "é".repeat(WHY_MAX - 3)),
            "cut to WHY_MAX characters, on a character boundary"
        );
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
        assert_eq!(links.declare("laptop", 7, PING), Ok(None));
        assert_eq!(links.peer_name(7, Instant::now()), Some("laptop"));
        assert_eq!(links.peer_name(8, Instant::now()), None);
    }

    #[test]
    fn a_live_link_is_never_replaced() {
        let mut links = Links::new();
        let t0 = Instant::now();
        links.declare_at("laptop", 7, PING, t0).unwrap();
        assert_eq!(
            links.declare_at("laptop", 8, PING, t0),
            Err(Declare::Held {
                retry_after: PING * 2
            })
        );
        assert_eq!(links.peer_name(7, Instant::now()), Some("laptop"));
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
        assert_eq!(links.declare("laptop", 8, PING), Ok(None));
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
        assert_eq!(links.declare("users", 7, PING), Ok(None));
    }
}
