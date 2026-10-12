//! Agent mailboxes and names: pure state with injected time.
//!
//! No wire, no lock and no I/O here. The daemon holds one [`Messaging`] behind
//! its own mutex and passes the clock in, so every ruling about ordering,
//! bounds, expiry and names is a unit test.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::protocol::{
    hidden_chars, valid_host, valid_name, Message, MsgId, MsgRef, MsgState, PaneId, Sender, Target,
};

/// The width of the per-sender rate window.
const RATE_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_messages: usize,
    pub max_bytes: usize,
    pub max_body: usize,
    pub rate_per_10s: u32,
    pub ttl_ms: u64,
}

/// Which mailbox cap a send ran into, with what the mailbox holds.
#[derive(Debug, PartialEq, Eq)]
pub enum Full {
    Messages {
        count: usize,
        max: usize,
    },
    Bytes {
        held: usize,
        incoming: usize,
        max: usize,
    },
}

/// Why the store refused a request. [`Refusal::code`] is the wire spelling.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    BadAddress(String),
    BadName(String),
    NameTaken {
        name: String,
        holder: PaneId,
    },
    NoSuchTarget(String),
    MessageTooLarge {
        bytes: usize,
        max: usize,
    },
    MailboxFull {
        pane: PaneId,
        full: Full,
    },
    RateLimited {
        per_10s: u32,
    },
    /// A body carrying a control character other than `\n` and `\t`.
    BadBody {
        character: char,
        at_byte: usize,
    },
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::BadAddress(_) => "bad_address",
            Refusal::BadName(_) => "bad_name",
            Refusal::NameTaken { .. } => "name_taken",
            Refusal::NoSuchTarget(_) => "no_such_target",
            Refusal::MessageTooLarge { .. } => "message_too_large",
            Refusal::MailboxFull { .. } => "mailbox_full",
            Refusal::RateLimited { .. } => "rate_limited",
            Refusal::BadBody { .. } => "bad_body",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Refusal::BadAddress(why) => why.clone(),
            Refusal::BadName(name) => {
                format!(
                    "bad name {name:?}: {}",
                    crate::protocol::bad_name_reason(name)
                )
            }
            Refusal::NameTaken { name, holder } => format!("name {name:?} is held by %{holder}"),
            Refusal::NoSuchTarget(target) => format!("no such target: {target}"),
            Refusal::MessageTooLarge { bytes, max } => {
                format!("message is {bytes} bytes; the limit is {max}")
            }
            Refusal::MailboxFull {
                pane,
                full: Full::Messages { count, max },
            } => format!("the mailbox of %{pane} is full ({count} of {max} messages)"),
            Refusal::MailboxFull {
                pane,
                full: Full::Bytes {
                    held,
                    incoming,
                    max,
                },
            } => format!(
                "the mailbox of %{pane} is full ({held} bytes held, {incoming} more would pass the {max}-byte limit)"
            ),
            Refusal::RateLimited { per_10s } => {
                format!("rate limited: at most {per_10s} sends per 10s")
            }
            Refusal::BadBody { character, at_byte } => format!(
                "the body has a control or invisible character (U+{:04X} at byte {at_byte}); \\n, \\t and a single joiner or variation selector attached to visible text are allowed",
                *character as u32
            ),
        }
    }
}

/// Whose sends a rate window counts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SenderKey {
    Pane(PaneId),
    Remote { host: String, pane_id: PaneId },
}

impl SenderKey {
    fn of(from: &Sender) -> Self {
        match from {
            Sender::Pane { pane_id, .. } => SenderKey::Pane(*pane_id),
            Sender::Remote { host, pane_id, .. } => SenderKey::Remote {
                host: host.clone(),
                pane_id: *pane_id,
            },
        }
    }
}

pub type WaiterId = u64;

/// A read that found nothing and waits for mail: answered by the next send to
/// its pane, by the pane closing, or by its timeout, whichever comes first.
/// It holds the client's sender, never a lock, so a waiting read blocks
/// neither the store nor the client's other requests.
pub struct Waiter {
    pub client_id: u64,
    pub pane: PaneId,
    pub all: bool,
    /// Whether the read marks what it lists, decided as for a read that
    /// answers at once: the pane's own caller, not peeking.
    pub mark: bool,
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::protocol::ServerMessage>,
    /// Held only so that dropping the waiter drops it: its timer waits on the
    /// other end, so every path that removes a waiter ends its timer too.
    pub cancel: tokio::sync::oneshot::Sender<()>,
}

/// Where a parked send is answered: the connection that made it.
pub struct Reply {
    pub client_id: u64,
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::protocol::ServerMessage>,
}

/// The most `agent wait` callers parked at once, across every pane.
pub const MAX_STATE_WAITS: usize = 256;

/// A caller waiting for a pane's agent to reach a state (`agent wait`). The
/// submitter samples the pane each pass until it is there or the deadline
/// passes.
pub struct StateWait {
    pub pane: PaneId,
    pub want: crate::protocol::AgentState,
    pub deadline: Instant,
    pub reply: Reply,
}

/// A message waiting to be typed into a pane, oldest first per pane.
pub struct SubmitEntry {
    pub id: MsgId,
    pub pane: PaneId,
    pub deliver: crate::protocol::Deliver,
    pub body: String,
    /// The sender, for the envelope.
    pub from_pane: PaneId,
    pub from_name: Option<String>,
    pub enqueued_at: Instant,
    /// Waits while the agent is waiting on its user, rather than being
    /// answered `agent_blocked`.
    pub defer: bool,
    /// When it is answered as out of time if it has not been typed, wherever
    /// it is in its queue.
    pub deadline: Instant,
    /// `None` once the sender's connection has closed: the entry is still
    /// typed when it can be, but nobody is told.
    pub reply: Option<Reply>,
    /// The sender was waiting for the answer when this was queued, so a
    /// `reply` of `None` now means it stopped waiting.
    pub had_reply: bool,
    /// How many times it was put back in its queue after its probe was typed.
    pub put_back: u32,
    /// The last reason it was logged as waiting on, so a wait is logged once
    /// per distinct reason rather than on every pass.
    pub waiting_on: Option<&'static str>,
    /// Sent with `--wait`: once submitted, it is answered when the agent's
    /// turn ends, or at this time at the latest.
    pub wait_until: Option<Instant>,
}

/// A submitted message whose sender asked to hear how the agent's turn
/// ended (`--wait`). The submitter samples its pane each pass until the
/// machine answers.
pub struct SubmitWait {
    pub id: MsgId,
    pub pane: PaneId,
    /// The sender's pane, which a sender's share of waits is counted by.
    pub from_pane: PaneId,
    /// How the message was sent, which decides how a stall is answered.
    pub deliver: crate::protocol::Deliver,
    pub reply: Reply,
    pub machine: crate::server::submit::WaitMachine,
}

impl SubmitEntry {
    /// Count a put-back after the probe. Returns whether this is the second,
    /// so that a message going round and round is logged once, not per turn.
    pub fn note_put_back(&mut self) -> bool {
        self.put_back = self.put_back.saturating_add(1);
        self.put_back == 2
    }
}

/// A queue head as the submitter reads it: what it decides on, without the
/// body, which is taken only when the entry is popped to be typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitHead {
    pub id: MsgId,
    pub pane: PaneId,
    pub deliver: crate::protocol::Deliver,
    pub enqueued_at: Instant,
    pub defer: bool,
}

/// Why a submit could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueRefusal {
    /// The sender's connection already has a send waiting for its answer.
    SendParked,
    /// The pane's queue, or the sender's share of it, is full.
    QueueFull,
}

#[derive(Default)]
struct Mailbox {
    /// In id order, which is arrival order.
    messages: VecDeque<Message>,
    /// The sum of the bodies' lengths.
    bytes: usize,
}

impl Mailbox {
    fn drop_expired(&mut self, now_ms: u64) -> usize {
        let before = self.messages.len();
        self.messages.retain(|m| m.expires_ms > now_ms);
        self.bytes = self.messages.iter().map(|m| m.body.len()).sum();
        before - self.messages.len()
    }

    /// Room for a body of `len` bytes. Only Read mail is evicted, oldest
    /// first, because the target already has it, and only if that makes room.
    fn make_room(&mut self, len: usize, limits: &Limits) -> Result<(), Full> {
        let fits = |count: usize, bytes: usize| {
            count < limits.max_messages && bytes + len <= limits.max_bytes
        };
        let (mut count, mut bytes) = (self.messages.len(), self.bytes);
        let mut evict = Vec::new();
        for (i, msg) in self.messages.iter().enumerate() {
            if fits(count, bytes) {
                break;
            }
            if msg.state == MsgState::Read {
                evict.push(i);
                count -= 1;
                bytes -= msg.body.len();
            }
        }
        if count >= limits.max_messages {
            return Err(Full::Messages {
                count: self.messages.len(),
                max: limits.max_messages,
            });
        }
        if bytes + len > limits.max_bytes {
            return Err(Full::Bytes {
                held: self.bytes,
                incoming: len,
                max: limits.max_bytes,
            });
        }
        for i in evict.into_iter().rev() {
            if let Some(msg) = self.messages.remove(i) {
                self.bytes -= msg.body.len();
            }
        }
        Ok(())
    }

    fn live(&self, now_ms: u64) -> impl Iterator<Item = &Message> {
        self.messages.iter().filter(move |m| m.expires_ms > now_ms)
    }
}

pub struct Messaging {
    limits: Limits,
    mailboxes: HashMap<PaneId, Mailbox>,
    names: HashMap<String, PaneId>,
    next_id: MsgId,
    rate: HashMap<SenderKey, VecDeque<Instant>>,
    /// Panes whose mailbox was dropped because they closed. A send racing a
    /// close would otherwise make a fresh mailbox nobody can ever read. Pane
    /// ids are never reused, so this only grows, and it is not persisted: a
    /// restart starts from the restored panes.
    closed: HashSet<PaneId>,
    /// Expired messages dropped since [`Messaging::take_swept`] last asked,
    /// by any path, so the caller can log them once.
    swept: usize,
    /// In the order they parked, which is the order they are answered in.
    waiters: BTreeMap<WaiterId, Waiter>,
    next_waiter: WaiterId,
    /// Something persisted changed since [`Messaging::take_dirty`] last
    /// asked; `changed` wakes whoever writes the file.
    dirty: bool,
    changed: std::sync::Arc<tokio::sync::Notify>,
    /// Messages waiting to be typed, per pane, oldest first. Not persisted: a
    /// restart drops them and they stay in the mailbox, Queued.
    submit_queues: HashMap<PaneId, VecDeque<SubmitEntry>>,
    /// Panes whose head is being typed right now (its Enter is pending), with
    /// the reply still waiting for that answer. No other entry for such a
    /// pane is looked at until it finishes, which is what keeps a pane's
    /// messages in order. The reply is held here, not by the typing task, so
    /// that a disconnect can drop it.
    in_flight: HashMap<PaneId, Option<Reply>>,
    /// Submitted messages whose senders wait to hear how the turn ended,
    /// by message id.
    submit_waits: BTreeMap<MsgId, SubmitWait>,
    /// Callers waiting for an agent to reach a state, by pane, each pane's in
    /// the order they parked, so a pane's sample is checked against its own
    /// waits only.
    state_waits: BTreeMap<PaneId, Vec<StateWait>>,
}

/// What `mailbox.json` holds. Waiters, rate windows and the closed-pane set are
/// deliberately absent: the first two die with their connections, and the
/// closed set is replaced by pruning to the restored panes at load.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailboxFile {
    pub version: u32,
    pub next_id: MsgId,
    /// The highest pane id this file refers to, as a recipient, a sender or
    /// a name. A restart reserves pane ids past it: a surviving message's
    /// sender must never be a new pane's id.
    pub pane_high_water: PaneId,
    pub names: Vec<(PaneId, String)>,
    pub messages: Vec<Message>,
}

pub const MAILBOX_FILE_VERSION: u32 = 1;

/// The largest id a mailbox file may hold, 2^53. Far past any id a server
/// issues, and small enough that the arithmetic done on an id at load cannot
/// overflow. A file beyond it did not come from a server.
pub const MAX_FILE_ID: u64 = 1 << 53;

impl MailboxFile {
    /// Every id in the file within [`MAX_FILE_ID`], or the field that is not.
    pub fn check_ids(&self) -> Result<(), &'static str> {
        let within = |id: u64| id <= MAX_FILE_ID;
        if !within(self.next_id) {
            return Err("next_id");
        }
        if !within(self.pane_high_water) {
            return Err("pane_high_water");
        }
        if !self.names.iter().all(|(pane, _)| within(*pane)) {
            return Err("a name's pane");
        }
        for msg in &self.messages {
            let sender = match &msg.from {
                Sender::Pane { pane_id, .. } | Sender::Remote { pane_id, .. } => *pane_id,
            };
            let reply = msg.reply_to.as_ref().map_or(0, |r| r.id);
            if ![msg.id, msg.to, sender, reply].into_iter().all(within) {
                return Err("a message id");
            }
        }
        Ok(())
    }
}

/// The first control character other than `\n` and `\t`, or invisible
/// character that [`hidden_chars`] does not allow, in `body`. A body is typed
/// into a pane in a later release, and an escape sequence in it (ESC[201~
/// ends a bracketed paste) would reach the pane's program as input rather
/// than as text. An invisible character is refused because it makes a body
/// display as text it does not contain.
fn control_character(body: &str) -> Option<(usize, char)> {
    let control = body
        .char_indices()
        .find(|&(_, c)| c.is_control() && c != '\n' && c != '\t');
    control.into_iter().chain(hidden_chars(body).next()).min()
}

/// The most characters of a session name a sender label keeps.
const MAX_SESSION_LABEL: usize = 128;

/// A session name as a sender carries it: a label, printed wherever the
/// message is shown, so it may hold no control or hidden character.
fn valid_session(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_SESSION_LABEL
        && !name.chars().any(char::is_control)
        && hidden_chars(name).next().is_none()
}

/// `name` made a [`valid_session`] label: each control or hidden character
/// replaced with U+FFFD, and the rest cut to the length limit. A session
/// name is whatever its creator typed, and a message whose label restore
/// would refuse is a message lost at the next restart.
fn session_label(name: &str) -> String {
    // Cut first: cutting after the judgement can leave a joiner that was
    // allowed between two characters with nothing after it.
    let cut: String = name.chars().take(MAX_SESSION_LABEL).collect();
    let hidden: Vec<usize> = hidden_chars(&cut).map(|(at, _)| at).collect();
    let label: String = cut
        .char_indices()
        .map(|(at, c)| {
            if c.is_control() || hidden.binary_search(&at).is_ok() {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    if label.is_empty() {
        '\u{FFFD}'.to_string()
    } else {
        label
    }
}

/// Whether a message read from the file is one a send would have stored.
fn valid_stored(msg: &Message, limits: &Limits) -> bool {
    let sender = match &msg.from {
        Sender::Pane { name, session, .. } => {
            name.as_deref().is_none_or(valid_name) && valid_session(session)
        }
        Sender::Remote { host, name, .. } => {
            valid_host(host) && name.as_deref().is_none_or(valid_name)
        }
    };
    let reply = msg
        .reply_to
        .as_ref()
        .and_then(|r| r.host.as_deref())
        .is_none_or(valid_host);
    sender && reply && msg.body.len() <= limits.max_body && control_character(&msg.body).is_none()
}

/// What a restore left out, for the log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Messages to a pane that was not restored.
    pub dead_pane: usize,
    pub expired: usize,
    /// Names of a pane that was not restored, or not a valid name.
    pub names: usize,
    /// Messages a send would have refused: a duplicate id, a control
    /// character or an oversized body, a sender or host that is not valid.
    pub invalid: usize,
    /// Messages past a mailbox's count or byte cap, the newest dropped.
    pub over_cap: usize,
    /// Messages kept with their sender's session label repaired. A label is
    /// a session's name, which its creator chose, so it is made valid
    /// rather than costing the message.
    pub relabelled: usize,
    /// Messages kept with their sender's name removed, because the name is
    /// now reserved. The mail was sent under a name that was valid then.
    pub unnamed: usize,
}

/// Remove from `queue`, in order, the entries `take` picks, moving nothing
/// when it picks none.
fn take_where(
    queue: &mut VecDeque<SubmitEntry>,
    mut take: impl FnMut(&SubmitEntry) -> bool,
) -> Vec<SubmitEntry> {
    let mut taken = Vec::new();
    let mut i = 0;
    while i < queue.len() {
        if take(&queue[i]) {
            taken.extend(queue.remove(i));
        } else {
            i += 1;
        }
    }
    taken
}

impl Messaging {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            mailboxes: HashMap::new(),
            names: HashMap::new(),
            next_id: 1,
            rate: HashMap::new(),
            closed: HashSet::new(),
            swept: 0,
            waiters: BTreeMap::new(),
            next_waiter: 1,
            dirty: false,
            changed: std::sync::Arc::new(tokio::sync::Notify::new()),
            submit_queues: HashMap::new(),
            submit_waits: BTreeMap::new(),
            state_waits: BTreeMap::new(),
            in_flight: HashMap::new(),
        }
    }

    fn touched(&mut self) {
        self.dirty = true;
        self.changed.notify_one();
    }

    pub fn message_count(&self) -> usize {
        self.mailboxes.values().map(|mb| mb.messages.len()).sum()
    }

    pub fn name_count(&self) -> usize {
        self.names.len()
    }

    /// Whether anything persisted changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Woken once for each change, and holds a wakeup until someone waits,
    /// so a change made while the saver is busy is not lost.
    pub fn changes(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.changed)
    }

    pub fn snapshot(&self) -> MailboxFile {
        let messages: Vec<Message> = self
            .mailboxes
            .values()
            .flat_map(|mb| mb.messages.iter().cloned())
            .collect();
        let mut names: Vec<(PaneId, String)> = self
            .names
            .iter()
            .map(|(name, &pane)| (pane, name.clone()))
            .collect();
        names.sort();
        let senders = messages.iter().map(|m| match &m.from {
            Sender::Pane { pane_id, .. } => *pane_id,
            // A remote sender's id is another server's, which this one never
            // issues.
            Sender::Remote { .. } => 0,
        });
        let pane_high_water = messages
            .iter()
            .map(|m| m.to)
            .chain(senders)
            .chain(names.iter().map(|(pane, _)| *pane))
            .max()
            .unwrap_or(0);
        MailboxFile {
            version: MAILBOX_FILE_VERSION,
            next_id: self.next_id,
            pane_high_water,
            names,
            messages,
        }
    }

    /// A store from `file`, keeping only what belongs to a restored pane
    /// (`is_live`, and not in `closed`) and has not expired by `now_ms`.
    ///
    /// The file is re-validated as a send would validate it, because it is
    /// input: a hand edit or another program can put anything in it. Each
    /// expiry is clamped to one TTL, so no message outlives the configured
    /// lifetime, and ids are expected to have passed
    /// [`MailboxFile::check_ids`], which keeps the arithmetic here in range.
    pub fn restore(
        limits: Limits,
        file: MailboxFile,
        is_live: impl Fn(PaneId) -> bool,
        closed: HashSet<PaneId>,
        now_ms: u64,
    ) -> (Self, Pruned) {
        let mut store = Self::new(limits.clone());
        let mut pruned = Pruned::default();
        let highest = file.messages.iter().map(|m| m.id).max().unwrap_or(0);
        store.next_id = file.next_id.max(highest.saturating_add(1)).max(1);
        let alive = |pane: PaneId| is_live(pane) && !closed.contains(&pane);
        let mut messages = file.messages;
        messages.sort_by_key(|m| m.id);
        let mut seen = HashSet::new();
        for mut msg in messages {
            if let Sender::Pane { session, .. } = &mut msg.from {
                let label = session_label(session);
                if label != *session {
                    *session = label;
                    pruned.relabelled += 1;
                }
            }
            let (Sender::Pane { name, .. } | Sender::Remote { name, .. }) = &mut msg.from;
            let unnamed = name
                .as_deref()
                .is_some_and(|n| crate::protocol::reserved_part(n).is_some());
            if unnamed {
                *name = None;
            }
            if !seen.insert(msg.id) || !valid_stored(&msg, &limits) {
                pruned.invalid += 1;
                continue;
            }
            if unnamed {
                pruned.unnamed += 1;
            }
            if !alive(msg.to) {
                pruned.dead_pane += 1;
                continue;
            }
            msg.expires_ms = msg
                .expires_ms
                .min(msg.created_ms.saturating_add(limits.ttl_ms))
                .min(now_ms.saturating_add(limits.ttl_ms));
            if msg.expires_ms <= now_ms {
                pruned.expired += 1;
                continue;
            }
            let mailbox = store.mailboxes.entry(msg.to).or_default();
            if mailbox.messages.len() >= limits.max_messages
                || mailbox.bytes + msg.body.len() > limits.max_bytes
            {
                pruned.over_cap += 1;
                continue;
            }
            mailbox.bytes += msg.body.len();
            mailbox.messages.push_back(msg);
        }
        store.mailboxes.retain(|_, mb| !mb.messages.is_empty());
        for (pane, name) in file.names {
            if alive(pane) && valid_name(&name) && !store.names.contains_key(&name) {
                store.names.retain(|_, holder| *holder != pane);
                store.names.insert(name, pane);
            } else {
                pruned.names += 1;
            }
        }
        store.closed = closed;
        (store, pruned)
    }

    /// The closed-pane set, for a restore to carry over.
    pub fn closed_panes(&self) -> HashSet<PaneId> {
        self.closed.clone()
    }

    /// Park `waiter`, returning its id and the same client's earlier wait,
    /// which it supersedes: one parked wait per client, never refused, so the
    /// waiters are bounded by the connections. A closed pane is refused here,
    /// under the lock that a close takes to answer its waiters, so a wait can
    /// never park on a pane that has already gone.
    pub fn park(&mut self, waiter: Waiter) -> Result<(WaiterId, Option<Waiter>), Refusal> {
        if self.closed.contains(&waiter.pane) {
            return Err(Refusal::NoSuchTarget(format!("%{}", waiter.pane)));
        }
        let earlier = self
            .waiters
            .iter()
            .find(|(_, w)| w.client_id == waiter.client_id)
            .map(|(&id, _)| id);
        let superseded = earlier.and_then(|id| self.waiters.remove(&id));
        let id = self.next_waiter;
        self.next_waiter += 1;
        self.waiters.insert(id, waiter);
        Ok((id, superseded))
    }

    /// The timeout path: the waiter, if nothing has answered it yet.
    pub fn unpark(&mut self, id: WaiterId) -> Option<Waiter> {
        self.waiters.remove(&id)
    }

    /// Every waiter on `pane`, removed, in the order they parked.
    pub fn take_waiters_for(&mut self, pane: PaneId) -> Vec<Waiter> {
        let ids: Vec<WaiterId> = self
            .waiters
            .iter()
            .filter(|(_, w)| w.pane == pane)
            .map(|(&id, _)| id)
            .collect();
        ids.into_iter()
            .filter_map(|id| self.waiters.remove(&id))
            .collect()
    }

    /// Answer the waiters on `pane` now that mail has arrived. Every peeking
    /// waiter is answered, and only the OLDEST marking one: the owner gets
    /// each message once, as an immediate read would, and a later marking
    /// waiter stays parked for the next message. All answers come from one
    /// snapshot taken before anything is marked, so a waiter answered after
    /// the marking one is not left with an empty list.
    pub fn answer_waiters(&mut self, pane: PaneId, now_ms: u64) -> Vec<(Waiter, Vec<Message>)> {
        let mut consumer_seen = false;
        let ids: Vec<WaiterId> = self
            .waiters
            .iter()
            .filter(|(_, w)| w.pane == pane)
            .filter(|(_, w)| !w.mark || !std::mem::replace(&mut consumer_seen, true))
            .map(|(&id, _)| id)
            .collect();
        let taken: Vec<Waiter> = ids
            .into_iter()
            .filter_map(|id| self.waiters.remove(&id))
            .collect();
        let answers: Vec<(Waiter, Vec<Message>)> = taken
            .into_iter()
            .map(|w| {
                let listed = self.read(pane, w.all, false, now_ms);
                (w, listed)
            })
            .collect();
        for (w, listed) in &answers {
            if w.mark {
                let ids: Vec<MsgId> = listed.iter().map(|m| m.id).collect();
                self.ack(pane, &ids);
            }
        }
        answers
    }

    /// The disconnect path: forget every waiter of `client_id`.
    pub fn drop_waiters_of(&mut self, client_id: u64) -> usize {
        let before = self.waiters.len();
        self.waiters.retain(|_, w| w.client_id != client_id);
        before - self.waiters.len()
    }

    /// Whether connection `client_id` has a request parked: a send queued,
    /// being typed or waiting for its turn to end, or an `agent wait`. A
    /// connection parks at most one, and a second cannot supersede the first:
    /// a send's message is already stored.
    pub fn has_parked_send(&self, client_id: u64) -> bool {
        let is_it =
            |reply: &Option<Reply>| reply.as_ref().is_some_and(|r| r.client_id == client_id);
        self.submit_queues
            .values()
            .flatten()
            .any(|queued| is_it(&queued.reply))
            || self.in_flight.values().any(is_it)
            || self
                .submit_waits
                .values()
                .any(|w| w.reply.client_id == client_id)
            || self
                .state_waits
                .values()
                .flatten()
                .any(|w| w.reply.client_id == client_id)
    }

    /// How many waits pane `from` has, parked or queued to be typed. An
    /// entry being typed is not counted: there is at most one per target.
    fn waits_from(&self, from: PaneId) -> usize {
        let parked = self.submit_waits.values().filter(|w| w.from_pane == from);
        let queued = self
            .submit_queues
            .values()
            .flatten()
            .filter(|e| e.wait_until.is_some() && e.from_pane == from);
        parked.count() + queued.count()
    }

    /// Queue `entry` to be typed into its pane. Refused when its sender
    /// already has a send parked, and when the pane already holds `cap`
    /// entries or the sender already holds half of them: one sender cannot
    /// fill a pane's queue and lock every other sender out of it. A deferred
    /// entry waits as long as its sender asked, so a sender may hold only one
    /// in a pane's queue: two senders holding half a queue each would lock
    /// everyone else out for that long.
    pub fn enqueue_submit(&mut self, entry: SubmitEntry, cap: usize) -> Result<(), EnqueueRefusal> {
        if let Some(client_id) = entry.reply.as_ref().map(|r| r.client_id) {
            if self.has_parked_send(client_id) {
                return Err(EnqueueRefusal::SendParked);
            }
        }
        // A wait lasts as long as its sender asked, so one sender pane, with a
        // connection per send, could otherwise hold any number of them.
        if entry.wait_until.is_some() && self.waits_from(entry.from_pane) >= (cap / 2).max(1) {
            return Err(EnqueueRefusal::QueueFull);
        }
        let queue = self.submit_queues.entry(entry.pane).or_default();
        let own = queue
            .iter()
            .filter(|e| e.from_pane == entry.from_pane)
            .count();
        let deferred = entry.defer
            && queue
                .iter()
                .any(|e| e.defer && e.from_pane == entry.from_pane);
        if queue.len() >= cap || own >= (cap / 2).max(1) || deferred {
            if queue.is_empty() {
                self.submit_queues.remove(&entry.pane);
            }
            return Err(EnqueueRefusal::QueueFull);
        }
        queue.push_back(entry);
        Ok(())
    }

    /// Put an entry popped to be typed back at the front of its queue: it
    /// was not typed, and it must keep its place before the entries behind it.
    pub fn requeue_front(&mut self, entry: SubmitEntry) {
        self.submit_queues
            .entry(entry.pane)
            .or_default()
            .push_front(entry);
    }

    /// The oldest entry of each pane that is not mid-typing. A head that must
    /// wait holds its pane: nothing behind it is returned.
    pub fn submit_heads(&self) -> Vec<SubmitHead> {
        self.submit_queues
            .iter()
            .filter(|(pane, _)| !self.in_flight.contains_key(pane))
            .filter_map(|(_, queue)| queue.front())
            .map(|e| SubmitHead {
                id: e.id,
                pane: e.pane,
                deliver: e.deliver,
                enqueued_at: e.enqueued_at,
                defer: e.defer,
            })
            .collect()
    }

    /// Remove every queued entry whose deadline is past `now`, wherever it
    /// is in its queue. An entry behind a head that waits is never a head,
    /// so a check at the head alone would hold it past its sender's own
    /// deadline, which then reports the outcome as unknown.
    pub fn take_overdue_submits(&mut self, now: Instant) -> Vec<SubmitEntry> {
        let mut overdue = Vec::new();
        for queue in self.submit_queues.values_mut() {
            overdue.extend(take_where(queue, |e| e.deadline <= now));
        }
        if !overdue.is_empty() {
            self.submit_queues.retain(|_, queue| !queue.is_empty());
        }
        overdue
    }

    /// For a pane that is closing: remove the entries it SENT that are still
    /// queued for other panes. Their sender is gone, and a message typed in
    /// its name later, perhaps much later, would say it is from a pane that
    /// no longer exists, or from a new pane given the same id.
    pub fn take_submits_from(&mut self, from: PaneId) -> Vec<SubmitEntry> {
        let mut taken = Vec::new();
        for queue in self.submit_queues.values_mut() {
            taken.extend(take_where(queue, |e| e.from_pane == from));
        }
        if !taken.is_empty() {
            self.submit_queues.retain(|_, queue| !queue.is_empty());
        }
        taken
    }

    /// Remove, in order, the entries behind `pane`'s head `head` that are not
    /// deferred. Called when the head waits for its agent to stop waiting on
    /// its user: an entry that did not ask to wait for that is answered now,
    /// as it would be at the head, rather than when its time runs out.
    pub fn take_undeferred_behind(&mut self, pane: PaneId, head: MsgId) -> Vec<SubmitEntry> {
        let Some(queue) = self
            .submit_queues
            .get_mut(&pane)
            .filter(|q| q.front().is_some_and(|e| e.id == head))
        else {
            return Vec::new();
        };
        let Some(head) = queue.pop_front() else {
            return Vec::new();
        };
        let (deferred, taken): (VecDeque<_>, VecDeque<_>) =
            std::mem::take(queue).into_iter().partition(|e| e.defer);
        *queue = deferred;
        queue.push_front(head);
        taken.into()
    }

    /// Record that `pane`'s head `id` is waiting on `reason`. Returns whether
    /// that is new, so the caller logs each distinct reason once.
    pub fn set_waiting(&mut self, pane: PaneId, id: MsgId, reason: &'static str) -> bool {
        match self
            .submit_queues
            .get_mut(&pane)
            .and_then(|q| q.front_mut())
            .filter(|e| e.id == id)
        {
            Some(entry) if entry.waiting_on != Some(reason) => {
                entry.waiting_on = Some(reason);
                true
            }
            _ => false,
        }
    }

    /// Whether `pane` closed: its mailbox was dropped with it.
    pub fn is_closed(&self, pane: PaneId) -> bool {
        self.closed.contains(&pane)
    }

    /// Remove `pane`'s head if it is still `id`.
    pub fn pop_submit(&mut self, pane: PaneId, id: MsgId) -> Option<SubmitEntry> {
        let queue = self.submit_queues.get_mut(&pane)?;
        if queue.front().map(|e| e.id) != Some(id) {
            return None;
        }
        let entry = queue.pop_front();
        if queue.is_empty() {
            self.submit_queues.remove(&pane);
        }
        entry
    }

    /// Mark `pane` as mid-typing, holding the reply its answer goes to.
    pub fn begin_typing(&mut self, pane: PaneId, reply: Option<Reply>) {
        self.in_flight.insert(pane, reply);
    }

    /// Mark `pane` as done typing, and hand back its reply: `None` when the
    /// sender has gone, or when the pane closed and was answered already.
    pub fn end_typing(&mut self, pane: PaneId) -> Option<Reply> {
        self.in_flight.remove(&pane).flatten()
    }

    /// Whether the submitter has anything to do: a queued entry, a pane
    /// mid-typing, or a turn being waited on.
    pub fn has_submit_work(&self) -> bool {
        !self.submit_queues.is_empty()
            || !self.in_flight.is_empty()
            || !self.submit_waits.is_empty()
            || !self.state_waits.is_empty()
    }

    /// Park a caller waiting for an agent's state. Refused past
    /// [`MAX_STATE_WAITS`] in all: `agent wait` takes callers outside every
    /// pane, so there is no sender to hold to a share, and each wait is a
    /// sample of its pane on every submitter pass.
    pub fn park_state_wait(&mut self, wait: StateWait) -> Result<(), StateWait> {
        let parked: usize = self.state_waits.values().map(Vec::len).sum();
        if parked >= MAX_STATE_WAITS {
            return Err(wait);
        }
        self.state_waits.entry(wait.pane).or_default().push(wait);
        Ok(())
    }

    /// The panes some caller waits on the state of, each once.
    pub fn state_wait_panes(&self) -> Vec<PaneId> {
        self.state_waits.keys().copied().collect()
    }

    /// The waits on `pane` that are over now that its agent is in `state`:
    /// those waiting for `state` (`false`), and those past their deadline
    /// (`true`, timed out). Each is removed.
    pub fn answer_state_waits(
        &mut self,
        pane: PaneId,
        state: crate::protocol::AgentState,
        now: Instant,
    ) -> Vec<(StateWait, bool)> {
        let Some(waits) = self.state_waits.remove(&pane) else {
            return Vec::new();
        };
        let (over, kept): (Vec<StateWait>, Vec<StateWait>) = waits
            .into_iter()
            .partition(|w| w.want == state || now >= w.deadline);
        if !kept.is_empty() {
            self.state_waits.insert(pane, kept);
        }
        over.into_iter()
            .map(|w| {
                let timed_out = w.want != state;
                (w, timed_out)
            })
            .collect()
    }

    /// Every wait on `pane`, removed: it closed, or is no longer an agent.
    pub fn take_state_waits(&mut self, pane: PaneId) -> Vec<StateWait> {
        self.state_waits.remove(&pane).unwrap_or_default()
    }

    /// Drop the state waits of a connection that closed. Returns how many.
    pub fn drop_state_waits_of(&mut self, client_id: u64) -> usize {
        let mut dropped = 0;
        for waits in self.state_waits.values_mut() {
            let before = waits.len();
            waits.retain(|w| w.reply.client_id != client_id);
            dropped += before - waits.len();
        }
        self.state_waits.retain(|_, waits| !waits.is_empty());
        dropped
    }

    /// Watch `wait`'s pane until its agent's turn ends.
    pub fn park_submit_wait(&mut self, wait: SubmitWait) {
        self.submit_waits.insert(wait.id, wait);
    }

    /// The turns being waited on, grouped by pane, so that each pane is
    /// sampled once however many waits it has.
    pub fn submit_waits_by_pane(&self) -> Vec<(PaneId, Vec<MsgId>)> {
        let mut by_pane: BTreeMap<PaneId, Vec<MsgId>> = BTreeMap::new();
        for wait in self.submit_waits.values() {
            by_pane.entry(wait.pane).or_default().push(wait.id);
        }
        by_pane.into_iter().collect()
    }

    /// Step the wait on message `id` with a sample of its pane. A wait that
    /// is over is removed and handed back with how it ended.
    pub fn step_submit_wait(
        &mut self,
        id: MsgId,
        now: Instant,
        last_output: Instant,
        state: crate::protocol::AgentState,
    ) -> Option<(SubmitWait, crate::server::submit::WaitStep)> {
        let wait = self.submit_waits.get_mut(&id)?;
        match wait.machine.step(now, last_output, state) {
            crate::server::submit::WaitStep::Continue => None,
            done => self.submit_waits.remove(&id).map(|wait| (wait, done)),
        }
    }

    /// Take the wait on message `id` away, to be answered now.
    pub fn take_submit_wait(&mut self, id: MsgId) -> Option<SubmitWait> {
        self.submit_waits.remove(&id)
    }

    /// Drop the waits of a connection that closed: nobody is left to tell
    /// how the turn ended. Returns their message ids.
    pub fn drop_submit_waits_of(&mut self, client_id: u64) -> Vec<MsgId> {
        let gone: Vec<MsgId> = self
            .submit_waits
            .values()
            .filter(|w| w.reply.client_id == client_id)
            .map(|w| w.id)
            .collect();
        for id in &gone {
            self.submit_waits.remove(id);
        }
        gone
    }

    /// The waits on a pane that is closing, to be answered.
    pub fn take_submit_waits(&mut self, pane: PaneId) -> Vec<SubmitWait> {
        let closing: Vec<MsgId> = self
            .submit_waits
            .values()
            .filter(|w| w.pane == pane)
            .map(|w| w.id)
            .collect();
        closing
            .iter()
            .filter_map(|id| self.submit_waits.remove(id))
            .collect()
    }

    /// Drop the replies of a connection that closed: `(queued, typing,
    /// dropped)`. Its entries stay and are typed when they can be, and one
    /// being typed finishes: the sender asked for that, and its CLI giving up
    /// must not lose the submit. A DEFERRED entry whose sender was waiting
    /// for it is removed instead (`dropped`), and stays in the mailbox: it
    /// may wait for minutes on its agent's user, and once its sender has gone
    /// nothing is waiting for it to be typed.
    pub fn detach_replies_of(&mut self, client_id: u64) -> (usize, usize, usize) {
        let is_it =
            |reply: &Option<Reply>| reply.as_ref().is_some_and(|r| r.client_id == client_id);
        let mut dropped = 0;
        for queue in self.submit_queues.values_mut() {
            dropped += take_where(queue, |e| e.defer && is_it(&e.reply)).len();
        }
        if dropped > 0 {
            self.submit_queues.retain(|_, queue| !queue.is_empty());
        }
        let mut queued = 0;
        for entry in self.submit_queues.values_mut().flatten() {
            if is_it(&entry.reply) {
                entry.reply = None;
                queued += 1;
            }
        }
        let mut typing = 0;
        for reply in self.in_flight.values_mut() {
            if is_it(reply) {
                *reply = None;
                typing += 1;
            }
        }
        (queued, typing, dropped)
    }

    /// For a pane that is closing: how many messages were waiting to be
    /// typed into it, and every reply still owed an answer, the one being
    /// typed included.
    pub fn take_submits(&mut self, pane: PaneId) -> (usize, Vec<Reply>) {
        let typing = self.in_flight.remove(&pane);
        let queued: Vec<SubmitEntry> = self
            .submit_queues
            .remove(&pane)
            .map(Vec::from)
            .unwrap_or_default();
        let count = queued.len() + usize::from(typing.is_some());
        let replies = typing
            .flatten()
            .into_iter()
            .chain(queued.into_iter().filter_map(|e| e.reply))
            .collect();
        (count, replies)
    }

    /// How many expired messages were dropped since the last call.
    pub fn take_swept(&mut self) -> usize {
        std::mem::take(&mut self.swept)
    }

    pub fn send(
        &mut self,
        from: Sender,
        to: PaneId,
        body: String,
        reply_to: Option<MsgRef>,
        now_ms: u64,
        now: Instant,
    ) -> Result<MsgId, Refusal> {
        let from = match from {
            Sender::Pane {
                pane_id,
                name,
                session,
            } => Sender::Pane {
                pane_id,
                name,
                session: session_label(&session),
            },
            remote @ Sender::Remote { .. } => remote,
        };
        let limits = &self.limits;
        if body.len() > limits.max_body {
            return Err(Refusal::MessageTooLarge {
                bytes: body.len(),
                max: limits.max_body,
            });
        }
        // A body is typed into a pane in a later release, and an escape
        // sequence in it (ESC[201~ ends a bracketed paste) would reach the
        // pane's program as input rather than as text.
        if let Some((at_byte, character)) = control_character(&body) {
            return Err(Refusal::BadBody { character, at_byte });
        }
        if self.closed.contains(&to) {
            return Err(Refusal::NoSuchTarget(format!("%{to}")));
        }

        let key = SenderKey::of(&from);
        let window = self.rate.entry(key).or_default();
        while window
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) >= RATE_WINDOW)
        {
            window.pop_front();
        }
        if window.len() >= limits.rate_per_10s as usize {
            return Err(Refusal::RateLimited {
                per_10s: limits.rate_per_10s,
            });
        }

        let mailbox = self.mailboxes.entry(to).or_default();
        self.swept += mailbox.drop_expired(now_ms);
        mailbox
            .make_room(body.len(), limits)
            .map_err(|full| Refusal::MailboxFull { pane: to, full })?;

        let id = self.next_id;
        self.next_id += 1;
        window.push_back(now);
        mailbox.bytes += body.len();
        mailbox.messages.push_back(Message {
            id,
            from,
            to,
            body,
            created_ms: now_ms,
            expires_ms: now_ms.saturating_add(limits.ttl_ms),
            state: MsgState::Queued,
            reply_to,
        });
        self.touched();
        Ok(id)
    }

    pub fn read(&mut self, pane: PaneId, all: bool, mark: bool, now_ms: u64) -> Vec<Message> {
        self.sweep(now_ms);
        let Some(mailbox) = self.mailboxes.get_mut(&pane) else {
            return Vec::new();
        };
        let mut listed = Vec::new();
        for msg in mailbox.messages.iter_mut() {
            if !all && msg.state != MsgState::Queued {
                continue;
            }
            listed.push(msg.clone());
            if mark {
                msg.state = MsgState::Read;
            }
        }
        if mark && listed.iter().any(|m| m.state != MsgState::Read) {
            self.touched();
        }
        listed
    }

    /// Mark the messages named by `ids` Read. Naming none marks none: "all of
    /// them" is [`Messaging::ack_all`], asked for on purpose.
    pub fn ack(&mut self, pane: PaneId, ids: &[MsgId]) -> usize {
        self.mark_read(pane, |msg| {
            msg.state != MsgState::Read && ids.contains(&msg.id)
        })
    }

    /// Mark message `id` Submitted once it has been typed into `pane`. Only a
    /// Queued message moves: one the pane already read, or that is gone, has
    /// reached its reader, and marking it would say it was typed when it was
    /// not.
    pub fn mark_submitted(&mut self, pane: PaneId, id: MsgId) -> bool {
        let Some(msg) = self
            .mailboxes
            .get_mut(&pane)
            .and_then(|mb| mb.messages.iter_mut().find(|m| m.id == id))
        else {
            return false;
        };
        if msg.state != MsgState::Queued {
            return false;
        }
        msg.state = MsgState::Submitted;
        self.touched();
        true
    }

    /// Undo [`Self::mark_submitted`] when the typing it claimed did not
    /// happen, so the message waits in the mailbox again. Only a message that
    /// is still Submitted moves back.
    pub fn unsubmit(&mut self, pane: PaneId, id: MsgId) -> bool {
        let Some(msg) = self
            .mailboxes
            .get_mut(&pane)
            .and_then(|mb| mb.messages.iter_mut().find(|m| m.id == id))
        else {
            return false;
        };
        if msg.state != MsgState::Submitted {
            return false;
        }
        msg.state = MsgState::Queued;
        self.touched();
        true
    }

    /// Mark every Queued message Read.
    pub fn ack_all(&mut self, pane: PaneId) -> usize {
        self.mark_read(pane, |msg| msg.state == MsgState::Queued)
    }

    fn mark_read(&mut self, pane: PaneId, wanted: impl Fn(&Message) -> bool) -> usize {
        let Some(mailbox) = self.mailboxes.get_mut(&pane) else {
            return 0;
        };
        let mut acked = 0;
        for msg in mailbox.messages.iter_mut().filter(|msg| wanted(msg)) {
            msg.state = MsgState::Read;
            acked += 1;
        }
        if acked > 0 {
            self.touched();
        }
        acked
    }

    pub fn set_name(&mut self, pane: PaneId, name: Option<&str>) -> Result<(), Refusal> {
        let Some(name) = name else {
            let before = self.names.len();
            self.names.retain(|_, holder| *holder != pane);
            if self.names.len() != before {
                self.touched();
            }
            return Ok(());
        };
        if !valid_name(name) {
            return Err(Refusal::BadName(name.to_string()));
        }
        if self.closed.contains(&pane) {
            return Err(Refusal::NoSuchTarget(format!("%{pane}")));
        }
        match self.names.get(name) {
            Some(&holder) if holder == pane => return Ok(()),
            Some(&holder) => {
                return Err(Refusal::NameTaken {
                    name: name.to_string(),
                    holder,
                })
            }
            None => {}
        }
        self.names.retain(|_, holder| *holder != pane);
        self.names.insert(name.to_string(), pane);
        self.touched();
        Ok(())
    }

    pub fn name_of(&self, pane: PaneId) -> Option<&str> {
        self.names
            .iter()
            .find_map(|(name, &holder)| (holder == pane).then_some(name.as_str()))
    }

    pub fn pane_named(&self, name: &str) -> Option<PaneId> {
        self.names.get(name).copied()
    }

    pub fn unread(&self, pane: PaneId, now_ms: u64) -> usize {
        self.mailboxes.get(&pane).map_or(0, |mb| {
            mb.live(now_ms)
                .filter(|m| m.state == MsgState::Queued)
                .count()
        })
    }

    pub fn panes_with_mail_or_name(&self, now_ms: u64) -> Vec<PaneId> {
        let mut panes: Vec<PaneId> = self
            .mailboxes
            .iter()
            .filter(|(_, mb)| mb.live(now_ms).next().is_some())
            .map(|(&pane, _)| pane)
            .chain(self.names.values().copied())
            .collect();
        panes.sort_unstable();
        panes.dedup();
        panes
    }

    /// Forget a closed pane: its mail, its name and its rate window. Returns
    /// how many messages went and the name it held, for the log.
    pub fn drop_pane(&mut self, pane: PaneId) -> (usize, Option<String>) {
        let messages = self
            .mailboxes
            .remove(&pane)
            .map_or(0, |mb| mb.messages.len());
        let named = self.name_of(pane).map(str::to_string);
        self.names.retain(|_, holder| *holder != pane);
        self.rate.remove(&SenderKey::Pane(pane));
        self.closed.insert(pane);
        if messages > 0 || named.is_some() {
            self.touched();
        }
        (messages, named)
    }

    pub fn sweep(&mut self, now_ms: u64) -> usize {
        let mut dropped = 0;
        for mailbox in self.mailboxes.values_mut() {
            dropped += mailbox.drop_expired(now_ms);
        }
        self.swept += dropped;
        self.mailboxes.retain(|_, mb| !mb.messages.is_empty());
        if dropped > 0 {
            self.touched();
        }
        dropped
    }

    pub fn resolve(
        &self,
        target: &Target,
        is_live: impl Fn(PaneId) -> bool,
    ) -> Result<PaneId, Refusal> {
        match target {
            Target::Name(name) => self
                .pane_named(name)
                .filter(|&pane| is_live(pane))
                .ok_or_else(|| Refusal::NoSuchTarget(name.clone())),
            Target::Pane(pane) if is_live(*pane) => Ok(*pane),
            Target::Pane(pane) => Err(Refusal::NoSuchTarget(format!("%{pane}"))),
        }
    }

    #[cfg(test)]
    fn set_state(&mut self, pane: PaneId, id: MsgId, state: MsgState) {
        if let Some(msg) = self
            .mailboxes
            .get_mut(&pane)
            .and_then(|mb| mb.messages.iter_mut().find(|m| m.id == id))
        {
            msg.state = state;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_messages: 256,
            max_bytes: 4 * 1024 * 1024,
            max_body: 64 * 1024,
            rate_per_10s: 1000,
            ttl_ms: 24 * 3_600_000,
        }
    }

    fn pane(id: PaneId) -> Sender {
        Sender::Pane {
            pane_id: id,
            name: None,
            session: "main".into(),
        }
    }

    fn bodies(msgs: &[Message]) -> Vec<&str> {
        msgs.iter().map(|m| m.body.as_str()).collect()
    }

    /// Sends `body` from pane 1 to pane 2 at time zero.
    fn send(m: &mut Messaging, body: &str, t0: Instant) -> Result<MsgId, Refusal> {
        m.send(pane(1), 2, body.into(), None, 0, t0)
    }

    #[test]
    fn ids_are_monotonic_and_order_is_per_mailbox() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        let a = m.send(pane(1), 2, "a".into(), None, 0, t0).unwrap();
        let b = m.send(pane(1), 3, "b".into(), None, 0, t0).unwrap();
        let c = m.send(pane(4), 2, "c".into(), None, 0, t0).unwrap();
        assert_eq!((a, b, c), (1, 2, 3));
        let two = m.read(2, false, false, 0);
        assert_eq!(bodies(&two), ["a", "c"]);
        assert_eq!(two.iter().map(|m| m.id).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(bodies(&m.read(3, false, false, 0)), ["b"]);
    }

    #[test]
    fn a_sent_message_carries_its_sender_reply_and_expiry() {
        let mut m = Messaging::new(limits());
        let reply = Some(MsgRef { host: None, id: 9 });
        let id = m
            .send(
                pane(1),
                2,
                "hi".into(),
                reply.clone(),
                5_000,
                Instant::now(),
            )
            .unwrap();
        let got = m.read(2, false, false, 5_000);
        assert_eq!(
            got,
            [Message {
                id,
                from: pane(1),
                to: 2,
                body: "hi".into(),
                created_ms: 5_000,
                expires_ms: 5_000 + 24 * 3_600_000,
                state: MsgState::Queued,
                reply_to: reply,
            }]
        );
    }

    #[test]
    fn read_lists_queued_only_and_marks_read_when_asked() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        send(&mut m, "a", t0).unwrap();
        send(&mut m, "b", t0).unwrap();

        assert_eq!(bodies(&m.read(2, false, false, 0)), ["a", "b"]);
        assert_eq!(m.unread(2, 0), 2, "a peek changed state");

        assert_eq!(bodies(&m.read(2, false, true, 0)), ["a", "b"]);
        assert_eq!(m.unread(2, 0), 0);
        assert!(m.read(2, false, true, 0).is_empty());

        let all = m.read(2, true, false, 0);
        assert_eq!(bodies(&all), ["a", "b"]);
        assert!(all.iter().all(|m| m.state == MsgState::Read));
    }

    #[test]
    fn submitted_is_not_read() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        let a = send(&mut m, "a", t0).unwrap();
        send(&mut m, "b", t0).unwrap();
        m.set_state(2, a, MsgState::Submitted);

        assert_eq!(bodies(&m.read(2, false, false, 0)), ["b"]);
        assert_eq!(m.unread(2, 0), 1);
        let all = m.read(2, true, false, 0);
        assert_eq!(bodies(&all), ["a", "b"]);
        assert_eq!(all[0].state, MsgState::Submitted);
    }

    #[test]
    fn a_body_over_the_cap_is_refused() {
        let lim = Limits {
            max_body: 4,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        assert!(send(&mut m, "four", t0).is_ok());
        assert_eq!(
            send(&mut m, "five!", t0),
            Err(Refusal::MessageTooLarge { bytes: 5, max: 4 })
        );
        assert_eq!(bodies(&m.read(2, true, false, 0)), ["four"]);
    }

    #[test]
    fn a_full_mailbox_refuses_the_sender_and_drops_nothing() {
        let lim = Limits {
            max_messages: 3,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        for b in ["a", "b", "c"] {
            send(&mut m, b, t0).unwrap();
        }
        assert_eq!(
            send(&mut m, "d", t0),
            Err(Refusal::MailboxFull {
                pane: 2,
                full: Full::Messages { count: 3, max: 3 }
            })
        );
        assert_eq!(bodies(&m.read(2, true, false, 0)), ["a", "b", "c"]);
    }

    #[test]
    fn a_mailbox_full_of_read_mail_evicts_the_oldest_read_to_accept() {
        let lim = Limits {
            max_messages: 3,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        for b in ["a", "b", "c"] {
            send(&mut m, b, t0).unwrap();
        }
        m.read(2, false, true, 0);

        assert!(send(&mut m, "d", t0).is_ok());
        assert_eq!(bodies(&m.read(2, true, false, 0)), ["b", "c", "d"]);
    }

    #[test]
    fn eviction_takes_only_read_mail_and_only_when_it_makes_room() {
        let lim = Limits {
            max_messages: 3,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        let a = send(&mut m, "a", t0).unwrap();
        let b = send(&mut m, "b", t0).unwrap();
        send(&mut m, "c", t0).unwrap();
        m.set_state(2, a, MsgState::Submitted);
        m.ack(2, &[b]);

        assert!(
            send(&mut m, "d", t0).is_ok(),
            "the Read one was not evicted"
        );
        assert_eq!(bodies(&m.read(2, true, false, 0)), ["a", "c", "d"]);
        assert_eq!(
            send(&mut m, "e", t0),
            Err(Refusal::MailboxFull {
                pane: 2,
                full: Full::Messages { count: 3, max: 3 }
            })
        );
    }

    #[test]
    fn a_send_that_eviction_cannot_fit_is_refused_and_evicts_nothing() {
        let lim = Limits {
            max_bytes: 10,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        send(&mut m, "12", t0).unwrap();
        m.read(2, false, true, 0);
        send(&mut m, "12345678", t0).unwrap();

        assert_eq!(
            send(&mut m, "123", t0),
            Err(Refusal::MailboxFull {
                pane: 2,
                full: Full::Bytes {
                    held: 10,
                    incoming: 3,
                    max: 10
                }
            })
        );
        assert_eq!(
            bodies(&m.read(2, true, false, 0)),
            ["12", "12345678"],
            "a refused send cost the target its read mail"
        );
    }

    #[test]
    fn the_byte_cap_is_a_second_full() {
        let lim = Limits {
            max_bytes: 10,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        send(&mut m, "12345", t0).unwrap();
        send(&mut m, "12345", t0).unwrap();
        assert_eq!(
            send(&mut m, "1", t0),
            Err(Refusal::MailboxFull {
                pane: 2,
                full: Full::Bytes {
                    held: 10,
                    incoming: 1,
                    max: 10
                }
            })
        );
        assert_eq!(m.read(2, true, false, 0).len(), 2);
        let message = Refusal::MailboxFull {
            pane: 2,
            full: Full::Bytes {
                held: 10,
                incoming: 1,
                max: 10,
            },
        }
        .message();
        assert!(
            message.contains("10 bytes held") && message.contains("10-byte limit"),
            "{message}"
        );
    }

    #[test]
    fn the_rate_window_is_per_sender_and_slides() {
        let lim = Limits {
            rate_per_10s: 2,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        send(&mut m, "a", t0).unwrap();
        send(&mut m, "b", t0).unwrap();
        assert_eq!(
            send(&mut m, "c", t0),
            Err(Refusal::RateLimited { per_10s: 2 })
        );
        assert!(
            m.send(pane(5), 2, "other".into(), None, 0, t0).is_ok(),
            "another sender shares the first sender's window"
        );
        let later = t0 + Duration::from_millis(10_100);
        assert!(send(&mut m, "d", later).is_ok());
        assert_eq!(bodies(&m.read(2, true, false, 0)), ["a", "b", "other", "d"]);
    }

    #[test]
    fn a_refused_send_does_not_use_up_the_rate() {
        let lim = Limits {
            rate_per_10s: 1,
            max_body: 2,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        assert!(send(&mut m, "big", t0).is_err());
        assert!(send(&mut m, "ok", t0).is_ok());
    }

    #[test]
    fn a_full_mailbox_does_not_use_up_the_rate() {
        let lim = Limits {
            max_messages: 1,
            rate_per_10s: 2,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        send(&mut m, "a", t0).unwrap();
        for _ in 0..3 {
            assert!(matches!(
                send(&mut m, "b", t0),
                Err(Refusal::MailboxFull { .. })
            ));
        }
        m.read(2, false, true, 0);
        assert!(
            send(&mut m, "c", t0).is_ok(),
            "refused sends used up the sender's rate"
        );
    }

    #[test]
    fn expired_mail_is_swept_and_never_read() {
        let lim = Limits {
            ttl_ms: 1_000,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        m.send(pane(1), 2, "old".into(), None, 0, t0).unwrap();
        m.send(pane(1), 2, "new".into(), None, 500, t0).unwrap();

        assert_eq!(bodies(&m.read(2, true, false, 999)), ["old", "new"]);
        assert_eq!(bodies(&m.read(2, true, false, 1_000)), ["new"]);
        assert_eq!(m.unread(2, 1_000), 1);
        assert_eq!(m.sweep(1_500), 1);
        assert!(m.read(2, true, false, 1_500).is_empty());
        assert_eq!(m.sweep(1_500), 0);
    }

    #[test]
    fn every_expired_message_dropped_is_counted_once() {
        let lim = Limits {
            ttl_ms: 1_000,
            max_messages: 1,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        m.send(pane(1), 2, "a".into(), None, 0, t0).unwrap();
        m.send(pane(1), 3, "b".into(), None, 0, t0).unwrap();
        assert_eq!(m.take_swept(), 0);
        assert!(m.read(2, true, false, 1_000).is_empty());
        assert_eq!(m.take_swept(), 2, "read's sweep covers every mailbox");
        assert_eq!(m.take_swept(), 0, "a count was reported twice");
        m.send(pane(1), 2, "c".into(), None, 1_000, t0).unwrap();
        m.send(pane(1), 2, "d".into(), None, 2_000, t0).unwrap();
        assert_eq!(m.take_swept(), 1, "send's own expiry drop is counted too");
    }

    #[test]
    fn expiry_frees_room_in_a_full_mailbox() {
        let lim = Limits {
            max_messages: 1,
            ttl_ms: 1_000,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        m.send(pane(1), 2, "a".into(), None, 0, t0).unwrap();
        assert!(m.send(pane(1), 2, "b".into(), None, 1_000, t0).is_ok());
    }

    #[test]
    fn unread_and_the_listing_ignore_expired_mail_before_any_sweep() {
        let lim = Limits {
            ttl_ms: 1_000,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        m.send(pane(1), 2, "a".into(), None, 0, Instant::now())
            .unwrap();
        assert_eq!(m.unread(2, 999), 1);
        assert_eq!(m.panes_with_mail_or_name(999), [2]);
        assert_eq!(m.unread(2, 1_000), 0);
        assert!(m.panes_with_mail_or_name(1_000).is_empty());
    }

    #[test]
    fn the_rate_window_is_exactly_ten_seconds() {
        let lim = Limits {
            rate_per_10s: 1,
            ..limits()
        };
        let mut m = Messaging::new(lim);
        let t0 = Instant::now();
        send(&mut m, "a", t0).unwrap();
        assert_eq!(
            send(&mut m, "b", t0 + Duration::from_millis(9_999)),
            Err(Refusal::RateLimited { per_10s: 1 })
        );
        assert!(send(&mut m, "c", t0 + Duration::from_millis(10_000)).is_ok());
    }

    #[test]
    fn name_taken_names_the_holder_and_renaming_yourself_is_fine() {
        let mut m = Messaging::new(limits());
        m.set_name(1, Some("alpha")).unwrap();
        assert_eq!(m.set_name(1, Some("alpha")), Ok(()));
        assert_eq!(
            m.set_name(2, Some("alpha")),
            Err(Refusal::NameTaken {
                name: "alpha".into(),
                holder: 1
            })
        );
        assert_eq!(
            m.set_name(2, Some("Alpha")),
            Err(Refusal::BadName("Alpha".into()))
        );

        m.set_name(1, Some("beta")).unwrap();
        assert_eq!(m.name_of(1), Some("beta"));
        assert_eq!(m.pane_named("alpha"), None, "the old name was kept");
        m.set_name(2, Some("alpha")).unwrap();
        assert_eq!(m.pane_named("alpha"), Some(2));

        m.set_name(2, None).unwrap();
        assert_eq!(m.name_of(2), None);
        assert_eq!(m.pane_named("alpha"), None);
    }

    #[test]
    fn a_name_dies_with_the_pane_and_so_does_its_mail() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        m.set_name(2, Some("alpha")).unwrap();
        send(&mut m, "a", t0).unwrap();
        send(&mut m, "b", t0).unwrap();
        m.set_name(3, Some("gamma")).unwrap();
        assert_eq!(m.panes_with_mail_or_name(0), [2, 3]);

        assert_eq!(m.drop_pane(2), (2, Some("alpha".to_string())));
        assert_eq!(m.pane_named("alpha"), None);
        assert!(m.read(2, true, false, 0).is_empty());
        assert_eq!(m.panes_with_mail_or_name(0), [3]);
        assert_eq!(m.drop_pane(2), (0, None));
    }

    #[test]
    fn resolve_is_exact_never_prefix_never_fallback() {
        let mut m = Messaging::new(limits());
        m.set_name(1, Some("alpha")).unwrap();
        let live = |p: PaneId| p == 1 || p == 4;

        assert_eq!(m.resolve(&Target::Name("alpha".into()), live), Ok(1));
        assert_eq!(
            m.resolve(&Target::Name("alp".into()), live),
            Err(Refusal::NoSuchTarget("alp".into()))
        );
        assert_eq!(m.resolve(&Target::Pane(4), live), Ok(4));
        assert_eq!(
            m.resolve(&Target::Pane(9), live),
            Err(Refusal::NoSuchTarget("%9".into()))
        );
        assert_eq!(
            m.resolve(&Target::Name("4".into()), live),
            Err(Refusal::NoSuchTarget("4".into())),
            "a name fell back to a pane id"
        );
    }

    #[test]
    fn ack_marks_exactly_the_ids_it_is_given() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        let a = send(&mut m, "a", t0).unwrap();
        let b = send(&mut m, "b", t0).unwrap();
        send(&mut m, "c", t0).unwrap();

        assert_eq!(m.ack(2, &[b, 999]), 1);
        assert_eq!(bodies(&m.read(2, false, false, 0)), ["a", "c"]);
        assert_eq!(m.ack(2, &[b]), 0, "acking twice counted twice");
        assert_eq!(m.ack(2, &[]), 0, "an ack naming nothing marked something");
        assert_eq!(m.unread(2, 0), 2);
        assert_eq!(m.ack(2, &[a]), 1);
        assert_eq!(m.ack(7, &[a]), 0);
    }

    #[test]
    fn ack_all_marks_every_queued_message_and_nothing_else() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        let a = send(&mut m, "a", t0).unwrap();
        send(&mut m, "b", t0).unwrap();
        send(&mut m, "c", t0).unwrap();
        m.set_state(2, a, MsgState::Submitted);

        assert_eq!(m.ack_all(2), 2, "a Submitted message is not Queued");
        assert_eq!(m.unread(2, 0), 0);
        assert_eq!(m.ack_all(2), 0);
        assert_eq!(m.ack_all(7), 0);
    }

    #[test]
    fn a_closed_pane_is_never_a_target_again() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        m.drop_pane(7);
        assert_eq!(
            m.send(pane(1), 7, "late".into(), None, 0, t0),
            Err(Refusal::NoSuchTarget("%7".into()))
        );
        assert!(
            m.read(7, true, false, 0).is_empty(),
            "a mailbox was made for it"
        );
        assert!(send(&mut m, "fine", t0).is_ok(), "another pane was refused");
    }

    #[test]
    fn a_closed_pane_cannot_be_named() {
        let mut m = Messaging::new(limits());
        m.drop_pane(7);
        assert_eq!(
            m.set_name(7, Some("ghost")),
            Err(Refusal::NoSuchTarget("%7".into()))
        );
        assert_eq!(m.pane_named("ghost"), None);
        assert_eq!(
            m.set_name(8, Some("ghost")),
            Ok(()),
            "a live pane was refused"
        );
    }

    #[test]
    fn a_body_with_a_control_character_is_refused() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        assert!(send(&mut m, "line one\nline\ttwo", t0).is_ok());
        for (body, character, at_byte) in [
            ("paste\u{1b}[201~end", '\u{1b}', 5),
            ("bell\u{7}", '\u{7}', 4),
            ("cr\r", '\r', 2),
            ("del\u{7f}", '\u{7f}', 3),
            ("c1\u{9b}201~", '\u{9b}', 2),
            ("nul\0", '\0', 3),
        ] {
            assert_eq!(
                send(&mut m, body, t0),
                Err(Refusal::BadBody { character, at_byte }),
                "{body:?}"
            );
        }
        assert_eq!(bodies(&m.read(2, true, false, 0)), ["line one\nline\ttwo"]);
    }

    #[test]
    fn a_body_with_a_format_character_is_refused_but_an_emoji_joiner_is_not() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        for character in [
            '\u{34F}',
            '\u{61C}',
            '\u{180E}',
            '\u{200B}',
            '\u{200E}',
            '\u{200F}',
            '\u{2028}',
            '\u{2029}',
            '\u{202A}',
            '\u{202E}',
            '\u{2060}',
            '\u{2066}',
            '\u{206F}',
            '\u{FEFF}',
            '\u{FFF9}',
            '\u{FFFB}',
            '\u{E0001}',
            '\u{E0041}',
            '\u{E007F}',
            '\u{3164}',
            '\u{FE00}',
            '\u{1D173}',
            '\u{E0100}',
        ] {
            let body = format!("ab{character}cd");
            assert_eq!(
                send(&mut m, &body, t0),
                Err(Refusal::BadBody {
                    character,
                    at_byte: 2
                }),
                "{character:?}"
            );
        }
        let emoji = "dev \u{1F469}\u{200D}\u{1F4BB}";
        assert!(send(&mut m, emoji, t0).is_ok());
        assert_eq!(bodies(&m.read(2, true, false, 0)), [emoji]);
        for attached in [
            "\u{43F}\u{435}\u{440}\u{435}\u{AD}\u{43D}\u{43E}\u{441}",
            "\u{645}\u{6CC}\u{200C}\u{62E}",
            "\u{2764}\u{FE0F}",
        ] {
            assert!(send(&mut m, attached, t0).is_ok(), "{attached:?}");
        }
        assert_eq!(
            send(&mut m, "a\u{200D}\u{200D}b", t0),
            Err(Refusal::BadBody {
                character: '\u{200D}',
                at_byte: 1
            })
        );
    }

    /// An entry from sender pane `100 + id`, so entries count against the
    /// per-sender share only where a test says so.
    fn entry(id: MsgId, pane: PaneId, client: Option<u64>) -> SubmitEntry {
        SubmitEntry {
            id,
            pane,
            deliver: crate::protocol::Deliver::Submit,
            body: format!("body {id}"),
            from_pane: 100 + id,
            from_name: None,
            enqueued_at: Instant::now(),
            defer: false,
            deadline: Instant::now() + std::time::Duration::from_secs(60),
            had_reply: client.is_some(),
            reply: client.map(reply),
            put_back: 0,
            waiting_on: None,
            wait_until: None,
        }
    }

    #[test]
    fn a_second_put_back_is_noted_once() {
        let mut e = entry(1, 5, None);
        assert_eq!(
            [e.note_put_back(), e.note_put_back(), e.note_put_back()],
            [false, true, false]
        );
    }

    fn turn_wait(id: MsgId, pane: PaneId, client_id: u64) -> SubmitWait {
        let now = Instant::now();
        let ms = std::time::Duration::from_millis;
        SubmitWait {
            id,
            pane,
            from_pane: 100 + id,
            deliver: crate::protocol::Deliver::Submit,
            reply: reply(client_id),
            machine: crate::server::submit::WaitMachine::new(
                now,
                now + ms(60_000),
                crate::server::submit::WaitCfg {
                    working: ms(500),
                    stall: ms(2000),
                    settle: ms(500),
                },
            ),
        }
    }

    #[test]
    fn a_turn_being_waited_on_is_work_and_holds_its_connections_send() {
        let mut m = Messaging::new(limits());
        assert!(!m.has_submit_work());
        m.park_submit_wait(turn_wait(1, 5, 70));
        assert!(m.has_submit_work(), "the submitter must keep ticking");
        assert!(
            m.has_parked_send(70),
            "a second send on that connection is refused"
        );
        assert!(!m.has_parked_send(71));
        assert_eq!(m.submit_waits_by_pane(), vec![(5, vec![1])]);
        assert_eq!(m.take_submit_wait(1).map(|w| w.id), Some(1));
        assert!(m.take_submit_wait(1).is_none() && !m.has_parked_send(70));
    }

    #[test]
    fn a_wait_is_removed_once_it_has_an_answer_and_not_before() {
        use crate::protocol::AgentState;
        use crate::server::submit::WaitStep;
        let mut m = Messaging::new(limits());
        let w = turn_wait(1, 5, 70);
        let enter = w.machine.enter_at();
        m.park_submit_wait(w);
        let ms = std::time::Duration::from_millis;
        assert!(m
            .step_submit_wait(1, enter + ms(100), enter, AgentState::Working)
            .is_none());
        assert_eq!(m.submit_waits_by_pane(), vec![(5, vec![1])]);
        let (done, step) = m
            .step_submit_wait(1, enter + ms(60_000), enter, AgentState::Idle)
            .expect("the deadline answers it");
        assert_eq!((done.id, step), (1, WaitStep::TimedOut(AgentState::Idle)));
        assert!(m.submit_waits_by_pane().is_empty() && !m.has_submit_work());
        assert!(m
            .step_submit_wait(1, enter + ms(60_100), enter, AgentState::Idle)
            .is_none());
    }

    #[test]
    fn waits_go_with_their_connection_or_their_pane() {
        let mut m = Messaging::new(limits());
        m.park_submit_wait(turn_wait(1, 5, 70));
        m.park_submit_wait(turn_wait(2, 6, 70));
        m.park_submit_wait(turn_wait(3, 6, 71));
        assert_eq!(m.drop_submit_waits_of(70), vec![1, 2]);
        assert!(!m.has_parked_send(70));
        let closing: Vec<MsgId> = m.take_submit_waits(6).iter().map(|w| w.id).collect();
        assert_eq!(closing, vec![3]);
        assert!(m.submit_waits_by_pane().is_empty());
    }

    fn state_wait(pane: PaneId, want: crate::protocol::AgentState, client: u64) -> StateWait {
        StateWait {
            pane,
            want,
            deadline: Instant::now() + std::time::Duration::from_secs(60),
            reply: reply(client),
        }
    }

    #[test]
    fn a_state_wait_ends_on_its_state_or_its_deadline_and_not_another_state() {
        use crate::protocol::AgentState::{Idle, NeedsInput, Working};
        let mut m = Messaging::new(limits());
        for w in [
            state_wait(5, NeedsInput, 70),
            state_wait(5, Idle, 71),
            state_wait(6, Idle, 72),
        ] {
            assert!(m.park_state_wait(w).is_ok());
        }
        assert!(m.has_submit_work() && m.has_parked_send(70));
        assert_eq!(m.state_wait_panes(), vec![5, 6]);
        let now = Instant::now();
        assert!(m.answer_state_waits(5, Working, now).is_empty());
        let over = m.answer_state_waits(5, NeedsInput, now);
        assert_eq!(
            over.iter()
                .map(|(w, t)| (w.reply.client_id, *t))
                .collect::<Vec<_>>(),
            vec![(70, false)],
            "only the wait for that state, on that pane"
        );
        let late = now + std::time::Duration::from_secs(61);
        let over = m.answer_state_waits(5, Working, late);
        assert_eq!(
            over.iter()
                .map(|(w, t)| (w.reply.client_id, *t))
                .collect::<Vec<_>>(),
            vec![(71, true)],
            "past its deadline, timed out"
        );
        assert_eq!(m.state_wait_panes(), vec![6]);
    }

    #[test]
    fn state_waits_are_capped_in_all_and_free_a_place_when_one_ends() {
        use crate::protocol::AgentState::Idle;
        let mut m = Messaging::new(limits());
        for n in 0..MAX_STATE_WAITS {
            let pane = (n % 7) as PaneId;
            assert!(
                m.park_state_wait(state_wait(pane, Idle, n as u64)).is_ok(),
                "{n}"
            );
        }
        assert!(
            m.park_state_wait(state_wait(99, Idle, 9_999)).is_err(),
            "one past the cap, on a pane with none yet"
        );
        assert_eq!(m.drop_state_waits_of(0), 1);
        assert!(
            m.park_state_wait(state_wait(99, Idle, 9_999)).is_ok(),
            "room again"
        );
    }

    #[test]
    fn state_waits_go_with_their_connection_or_their_pane() {
        use crate::protocol::AgentState::Idle;
        let mut m = Messaging::new(limits());
        for w in [
            state_wait(5, Idle, 70),
            state_wait(6, Idle, 70),
            state_wait(6, Idle, 71),
        ] {
            assert!(m.park_state_wait(w).is_ok());
        }
        assert_eq!(m.drop_state_waits_of(70), 2);
        assert!(!m.has_parked_send(70));
        assert_eq!(m.take_state_waits(6).len(), 1);
        assert!(m.state_wait_panes().is_empty() && !m.has_submit_work());
    }

    #[test]
    fn waits_on_one_pane_are_grouped_to_be_sampled_once() {
        let mut m = Messaging::new(limits());
        m.park_submit_wait(turn_wait(3, 6, 70));
        m.park_submit_wait(turn_wait(1, 5, 71));
        m.park_submit_wait(turn_wait(2, 6, 72));
        assert_eq!(
            m.submit_waits_by_pane(),
            vec![(5, vec![1]), (6, vec![2, 3])]
        );
    }

    #[test]
    fn a_sender_pane_holds_at_most_its_share_of_waits() {
        let mut m = Messaging::new(limits());
        let waiting = |id: MsgId, pane: PaneId, client: u64| SubmitEntry {
            from_pane: 40,
            wait_until: Some(Instant::now() + std::time::Duration::from_secs(60)),
            ..entry(id, pane, Some(client))
        };
        // A queue of 4 is a share of 2: one parked and one queued fill it,
        // whichever panes they are for.
        m.park_submit_wait(SubmitWait {
            from_pane: 40,
            ..turn_wait(1, 5, 70)
        });
        assert_eq!(m.enqueue_submit(waiting(2, 6, 71), 4), Ok(()));
        assert_eq!(
            m.enqueue_submit(waiting(3, 7, 72), 4),
            Err(EnqueueRefusal::QueueFull)
        );
        assert_eq!(
            m.enqueue_submit(entry(4, 7, Some(73)), 4),
            Ok(()),
            "a send without --wait is not counted"
        );
        m.take_submit_wait(1);
        assert_eq!(m.enqueue_submit(waiting(5, 7, 74), 4), Ok(()), "room again");
    }

    fn reply(client_id: u64) -> Reply {
        Reply {
            client_id,
            tx: tokio::sync::mpsc::unbounded_channel().0,
        }
    }

    #[test]
    fn enqueue_refuses_past_the_cap() {
        let mut m = Messaging::new(limits());
        assert_eq!(m.enqueue_submit(entry(1, 5, None), 2), Ok(()));
        assert_eq!(m.enqueue_submit(entry(2, 5, None), 2), Ok(()));
        assert_eq!(
            m.enqueue_submit(entry(3, 5, None), 2),
            Err(EnqueueRefusal::QueueFull)
        );
        assert_eq!(
            m.enqueue_submit(entry(4, 6, None), 2),
            Ok(()),
            "another pane"
        );
    }

    #[test]
    fn one_sender_holds_at_most_half_a_panes_queue() {
        let mut m = Messaging::new(limits());
        let from = |id, from_pane| SubmitEntry {
            from_pane,
            ..entry(id, 5, None)
        };
        for id in 1..=4 {
            assert_eq!(m.enqueue_submit(from(id, 7), 8), Ok(()), "entry {id}");
        }
        assert_eq!(
            m.enqueue_submit(from(5, 7), 8),
            Err(EnqueueRefusal::QueueFull),
            "a fifth from one sender with room for eight"
        );
        assert_eq!(m.enqueue_submit(from(6, 8), 8), Ok(()), "another sender");
        assert_eq!(
            m.enqueue_submit(entry(7, 9, None), 8),
            Ok(()),
            "another pane's queue is its own"
        );
        let mut one = Messaging::new(limits());
        assert_eq!(
            one.enqueue_submit(from(1, 7), 1),
            Ok(()),
            "a queue of one still takes one"
        );
    }

    #[test]
    fn one_parked_reply_per_client() {
        let mut m = Messaging::new(limits());
        assert_eq!(m.enqueue_submit(entry(1, 5, Some(9)), 8), Ok(()));
        assert!(m.has_parked_send(9));
        assert_eq!(
            m.enqueue_submit(entry(2, 6, Some(9)), 8),
            Err(EnqueueRefusal::SendParked),
            "a second parked send on one connection, even to another pane"
        );
        assert_eq!(m.enqueue_submit(entry(3, 6, Some(10)), 8), Ok(()));
    }

    #[test]
    fn a_send_being_typed_is_still_parked() {
        let mut m = Messaging::new(limits());
        m.enqueue_submit(entry(1, 5, Some(9)), 8).unwrap();
        let popped = m.pop_submit(5, 1).unwrap();
        m.begin_typing(5, popped.reply);
        assert!(m.has_parked_send(9), "its answer waits for the Enter");
        assert_eq!(
            m.enqueue_submit(entry(2, 6, Some(9)), 8),
            Err(EnqueueRefusal::SendParked)
        );
        assert_eq!(m.detach_replies_of(9), (0, 1, 0));
        assert!(m.end_typing(5).is_none(), "the detached reply was dropped");
        assert_eq!(m.enqueue_submit(entry(3, 6, Some(9)), 8), Ok(()));
    }

    #[test]
    fn heads_are_the_oldest_entry_per_pane_and_in_flight_holds_the_pane() {
        let mut m = Messaging::new(limits());
        m.enqueue_submit(entry(1, 5, None), 8).unwrap();
        m.enqueue_submit(entry(2, 5, None), 8).unwrap();
        m.enqueue_submit(entry(3, 6, None), 8).unwrap();
        let mut heads: Vec<MsgId> = m.submit_heads().iter().map(|h| h.id).collect();
        heads.sort();
        assert_eq!(heads, [1, 3]);
        m.begin_typing(5, None);
        assert_eq!(
            m.submit_heads().iter().map(|h| h.id).collect::<Vec<_>>(),
            [3]
        );
        assert!(m.pop_submit(5, 2).is_none(), "only the head pops");
        assert_eq!(m.pop_submit(5, 1).map(|e| e.id), Some(1));
        m.end_typing(5);
        let mut heads: Vec<MsgId> = m.submit_heads().iter().map(|h| h.id).collect();
        heads.sort();
        assert_eq!(heads, [2, 3]);
    }

    #[test]
    fn a_requeued_entry_keeps_its_place() {
        let mut m = Messaging::new(limits());
        m.enqueue_submit(entry(1, 5, Some(9)), 8).unwrap();
        m.enqueue_submit(entry(2, 5, None), 8).unwrap();
        let popped = m.pop_submit(5, 1).unwrap();
        m.requeue_front(popped);
        assert_eq!(
            m.submit_heads().iter().map(|h| h.id).collect::<Vec<_>>(),
            [1]
        );
        assert!(m.has_parked_send(9), "with its reply");
    }

    #[test]
    fn a_wait_is_logged_once_per_reason() {
        let mut m = Messaging::new(limits());
        m.enqueue_submit(entry(1, 5, None), 8).unwrap();
        assert!(m.set_waiting(5, 1, "human_typing"));
        assert!(!m.set_waiting(5, 1, "human_typing"));
        assert!(m.set_waiting(5, 1, "agent_blocked"));
        assert!(!m.set_waiting(5, 2, "human_typing"), "not the head");
    }

    #[test]
    fn a_waiting_head_holds_its_queue() {
        let mut m = Messaging::new(limits());
        m.enqueue_submit(entry(1, 5, None), 8).unwrap();
        m.enqueue_submit(entry(2, 5, None), 8).unwrap();
        assert!(m.set_waiting(5, 1, "agent_blocked"));
        for _ in 0..3 {
            assert_eq!(
                m.submit_heads().iter().map(|h| h.id).collect::<Vec<_>>(),
                [1],
                "the entry behind a waiting head is never decided"
            );
        }
        assert!(m.pop_submit(5, 2).is_none());
    }

    #[test]
    fn overdue_entries_are_taken_wherever_they_sit_in_their_queue() {
        let mut m = Messaging::new(limits());
        let now = Instant::now();
        let due = |id, pane, secs: i64| SubmitEntry {
            deadline: if secs < 0 {
                now - std::time::Duration::from_secs(secs.unsigned_abs())
            } else {
                now + std::time::Duration::from_secs(secs as u64)
            },
            ..entry(id, pane, None)
        };
        m.enqueue_submit(due(1, 5, 60), 8).unwrap();
        m.enqueue_submit(due(2, 5, -1), 8).unwrap();
        m.enqueue_submit(due(3, 5, 60), 8).unwrap();
        m.enqueue_submit(due(4, 6, -1), 8).unwrap();
        let mut taken: Vec<MsgId> = m.take_overdue_submits(now).iter().map(|e| e.id).collect();
        taken.sort();
        assert_eq!(taken, [2, 4], "behind a head as well as at one");
        assert_eq!(
            m.submit_heads().iter().map(|h| h.id).collect::<Vec<_>>(),
            [1],
            "pane 6's queue is gone with its only entry"
        );
        m.pop_submit(5, 1);
        assert_eq!(
            m.submit_heads().iter().map(|h| h.id).collect::<Vec<_>>(),
            [3],
            "the rest keep their order"
        );
        assert!(m.take_overdue_submits(now).is_empty());
    }

    #[test]
    fn the_undeferred_entries_behind_a_head_are_taken_in_order() {
        let mut m = Messaging::new(limits());
        let deferred = |id| SubmitEntry {
            defer: true,
            ..entry(id, 5, None)
        };
        m.enqueue_submit(deferred(1), 8).unwrap();
        m.enqueue_submit(entry(2, 5, None), 8).unwrap();
        m.enqueue_submit(deferred(3), 8).unwrap();
        m.enqueue_submit(entry(4, 5, None), 8).unwrap();
        m.enqueue_submit(entry(5, 6, None), 8).unwrap();
        assert!(
            m.take_undeferred_behind(5, 3).is_empty(),
            "a head that moved on since it was looked at"
        );
        let taken: Vec<MsgId> = m
            .take_undeferred_behind(5, 1)
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(taken, [2, 4]);
        m.pop_submit(5, 1);
        assert_eq!(
            m.submit_heads()
                .iter()
                .filter(|h| h.pane == 5)
                .map(|h| h.id)
                .collect::<Vec<_>>(),
            [3]
        );
        assert!(m.take_undeferred_behind(7, 1).is_empty(), "no such queue");
        let mut lone = Messaging::new(limits());
        lone.enqueue_submit(entry(1, 5, None), 8).unwrap();
        assert!(
            lone.take_undeferred_behind(5, 1).is_empty(),
            "the head itself is never taken"
        );
    }

    #[test]
    fn detach_keeps_the_entry_and_take_submits_returns_every_reply() {
        let mut m = Messaging::new(limits());
        m.enqueue_submit(entry(1, 5, Some(9)), 8).unwrap();
        m.enqueue_submit(entry(2, 5, None), 8).unwrap();
        m.enqueue_submit(entry(4, 5, Some(11)), 8).unwrap();
        assert_eq!(m.detach_replies_of(9), (1, 0, 0));
        assert_eq!(m.submit_heads().len(), 1, "the entry stays");
        assert!(
            m.enqueue_submit(entry(3, 6, Some(9)), 8).is_ok(),
            "no reply parked now"
        );
        m.begin_typing(5, Some(reply(12)));
        let (count, replies) = m.take_submits(5);
        let mut owed: Vec<u64> = replies.iter().map(|r| r.client_id).collect();
        owed.sort();
        assert_eq!(count, 4, "three queued and one being typed");
        assert_eq!(owed, [11, 12], "the detached one is owed nothing");
        assert!(m.end_typing(5).is_none(), "answered by the close already");
        assert!(m.has_submit_work(), "pane 6 still queued");
        m.take_submits(6);
        assert!(!m.has_submit_work());
    }

    #[test]
    fn a_deferred_entry_whose_sender_went_is_dropped_and_others_are_kept() {
        let mut m = Messaging::new(limits());
        let deferred = |id, client| SubmitEntry {
            defer: true,
            ..entry(id, 5, client)
        };
        m.enqueue_submit(deferred(1, Some(9)), 8).unwrap();
        m.enqueue_submit(deferred(2, None), 8).unwrap();
        m.enqueue_submit(entry(3, 6, Some(10)), 8).unwrap();
        assert_eq!(m.detach_replies_of(9), (0, 0, 1));
        assert_eq!(m.detach_replies_of(10), (1, 0, 0), "not deferred: kept");
        let mut heads: Vec<MsgId> = m.submit_heads().iter().map(|h| h.id).collect();
        heads.sort();
        assert_eq!(heads, [2, 3], "the one with no reply to lose is kept");
    }

    #[test]
    fn an_entry_is_overdue_at_its_deadline_itself() {
        let mut m = Messaging::new(limits());
        let now = Instant::now();
        let at_now = SubmitEntry {
            deadline: now,
            ..entry(1, 5, None)
        };
        m.enqueue_submit(at_now, 8).unwrap();
        assert_eq!(m.take_overdue_submits(now).len(), 1);
    }

    #[test]
    fn a_sender_holds_one_deferred_entry_per_pane() {
        let mut m = Messaging::new(limits());
        let deferred = |id, pane, from| SubmitEntry {
            defer: true,
            from_pane: from,
            ..entry(id, pane, None)
        };
        m.enqueue_submit(deferred(1, 5, 40), 8).unwrap();
        assert_eq!(
            m.enqueue_submit(deferred(2, 5, 40), 8),
            Err(EnqueueRefusal::QueueFull)
        );
        assert_eq!(
            m.enqueue_submit(deferred(3, 6, 40), 8),
            Ok(()),
            "another pane"
        );
        assert_eq!(
            m.enqueue_submit(deferred(4, 5, 41), 8),
            Ok(()),
            "another sender"
        );
        let plain = SubmitEntry {
            from_pane: 40,
            ..entry(5, 5, None)
        };
        assert_eq!(m.enqueue_submit(plain, 8), Ok(()), "not deferred");
    }

    #[test]
    fn a_closed_pane_s_queued_sends_are_taken_from_every_queue() {
        let mut m = Messaging::new(limits());
        let from = |id, pane, from| SubmitEntry {
            from_pane: from,
            ..entry(id, pane, None)
        };
        m.enqueue_submit(from(1, 5, 40), 8).unwrap();
        m.enqueue_submit(from(2, 5, 41), 8).unwrap();
        m.enqueue_submit(from(3, 6, 40), 8).unwrap();
        let mut taken: Vec<MsgId> = m.take_submits_from(40).iter().map(|e| e.id).collect();
        taken.sort();
        assert_eq!(taken, [1, 3]);
        assert_eq!(
            m.submit_heads().iter().map(|h| h.id).collect::<Vec<_>>(),
            [2],
            "pane 6's queue is gone with its only entry"
        );
        assert!(m.take_submits_from(40).is_empty());
    }

    #[test]
    fn mark_submitted_moves_queued_to_submitted_once() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        let typed = send(&mut m, "typed", t0).unwrap();
        let read = send(&mut m, "read", t0).unwrap();
        m.ack(2, &[read]);
        assert!(m.mark_submitted(2, typed));
        assert!(!m.mark_submitted(2, typed), "a second mark");
        assert!(!m.mark_submitted(2, read), "a Read message");
        assert!(!m.mark_submitted(2, 999), "no such id");
        assert!(!m.mark_submitted(3, typed), "another pane");
        let states: Vec<MsgState> = m.read(2, true, false, 0).iter().map(|m| m.state).collect();
        assert_eq!(states, [MsgState::Submitted, MsgState::Read]);
    }

    #[test]
    fn unsubmit_moves_only_a_submitted_message_back() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        let typed = send(&mut m, "typed", t0).unwrap();
        assert!(!m.unsubmit(2, typed), "a Queued message");
        m.mark_submitted(2, typed);
        m.ack_all(2);
        assert!(m.unsubmit(2, typed));
        assert_eq!(m.read(2, false, false, 0).len(), 1, "Queued again");
        m.ack(2, &[typed]);
        assert!(!m.unsubmit(2, typed), "a Read message");
    }

    #[test]
    fn a_submitted_message_is_hidden_from_a_plain_read_and_not_unread() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        let typed = send(&mut m, "typed", t0).unwrap();
        send(&mut m, "waiting", t0).unwrap();
        m.mark_submitted(2, typed);
        assert_eq!(bodies(&m.read(2, false, false, 0)), ["waiting"]);
        assert_eq!(m.unread(2, 0), 1);
        assert_eq!(bodies(&m.read(2, true, false, 0)), ["typed", "waiting"]);
    }

    #[test]
    fn a_hostile_session_name_is_labelled_so_restore_keeps_the_message() {
        let t0 = Instant::now();
        for session in [
            "s\u{1b}[2J".to_string(),
            "main\u{202E}".to_string(),
            "tag\u{E0041}".to_string(),
            "\u{200D}".to_string(),
            String::new(),
            "x".repeat(300),
            format!("{}a\u{200D}b", "x".repeat(126)),
            format!("{}a\u{200C}b", "x".repeat(126)),
            format!("{}a\u{AD}b", "x".repeat(126)),
            format!("{}\u{200D}b", "a".repeat(127)),
            format!("{}\u{1F469}\u{200D}\u{1F4BB}", "x".repeat(126)),
        ] {
            assert!(valid_session(&session_label(&session)), "{session:?}");
            let mut m = Messaging::new(limits());
            let from = Sender::Pane {
                pane_id: 1,
                name: None,
                session: session.clone(),
            };
            m.send(from, 2, "kept".into(), None, 0, t0).unwrap();
            let (mut r, pruned) =
                Messaging::restore(limits(), m.snapshot(), |_| true, HashSet::new(), 0);
            assert_eq!(pruned.invalid, 0, "{session:?}: {pruned:?}");
            assert_eq!(bodies(&r.read(2, true, false, 0)), ["kept"], "{session:?}");
        }
        assert_eq!(
            session_label("dev \u{2764}\u{FE0F}"),
            "dev \u{2764}\u{FE0F}"
        );
        assert_eq!(session_label("a\u{202E}b"), "a\u{FFFD}b");
    }

    type Answers = tokio::sync::mpsc::UnboundedReceiver<crate::protocol::ServerMessage>;
    type Cancel = tokio::sync::oneshot::Receiver<()>;

    fn waiter(client_id: u64, pane: PaneId, mark: bool) -> (Waiter, Answers, Cancel) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        (
            Waiter {
                client_id,
                pane,
                all: false,
                mark,
                tx,
                cancel,
            },
            rx,
            cancelled,
        )
    }

    fn park(m: &mut Messaging, w: Waiter) -> WaiterId {
        m.park(w).expect("parking was refused").0
    }

    #[test]
    fn a_parked_waiter_is_unparked_once() {
        let mut m = Messaging::new(limits());
        let (w, _rx, _c) = waiter(1, 2, false);
        let id = park(&mut m, w);
        assert_eq!(m.unpark(id).map(|w| w.pane), Some(2));
        assert!(m.unpark(id).is_none(), "a waiter was answered twice");
    }

    /// Ruling A: one parked wait per client. A second supersedes the first,
    /// which the caller answers as timed out; it is never refused.
    #[test]
    fn a_second_wait_from_one_client_supersedes_the_first() {
        let mut m = Messaging::new(limits());
        let (first, _a, _c1) = waiter(1, 2, false);
        let (second, _b, _c2) = waiter(1, 3, false);
        let first_id = park(&mut m, first);
        let (_, superseded) = m.park(second).expect("the second wait was refused");
        assert_eq!(superseded.map(|w| w.pane), Some(2));
        assert!(
            m.unpark(first_id).is_none(),
            "the superseded wait is still parked"
        );
        assert_eq!(m.take_waiters_for(3).len(), 1);
        let (other, _c, _c3) = waiter(2, 3, false);
        assert!(
            m.park(other).expect("refused").1.is_none(),
            "another client was superseded"
        );
    }

    #[test]
    fn parking_on_a_closed_pane_is_refused() {
        let mut m = Messaging::new(limits());
        m.drop_pane(7);
        let (w, _rx, _c) = waiter(1, 7, false);
        assert!(matches!(m.park(w), Err(Refusal::NoSuchTarget(_))));
        assert!(m.take_waiters_for(7).is_empty());
    }

    /// Ruling B: peekers all get the snapshot, taken before anything is
    /// marked; only the OLDEST marking waiter consumes the mail, and a later
    /// marking waiter stays parked for the next message. The owner so gets
    /// each message once, as an immediate read would.
    #[test]
    fn peekers_share_the_snapshot_and_only_the_oldest_marking_waiter_consumes() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        let (owner, _a, _c1) = waiter(1, 2, true);
        let (peeker, _b, _c2) = waiter(2, 2, false);
        let (later_owner, _c, _c3) = waiter(3, 2, true);
        let (elsewhere, _d, _c4) = waiter(4, 9, true);
        for w in [owner, peeker, later_owner, elsewhere] {
            park(&mut m, w);
        }
        send(&mut m, "a", t0).unwrap();
        send(&mut m, "b", t0).unwrap();

        let got: Vec<(u64, Vec<String>)> = m
            .answer_waiters(2, 0)
            .iter()
            .map(|(w, msgs)| {
                (
                    w.client_id,
                    bodies(msgs).iter().map(|b| b.to_string()).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (1, vec!["a".to_string(), "b".to_string()]),
                (2, vec!["a".to_string(), "b".to_string()])
            ]
        );
        assert_eq!(m.unread(2, 0), 0, "the oldest marking waiter did not mark");

        send(&mut m, "c", t0).unwrap();
        let next: Vec<(u64, Vec<String>)> = m
            .answer_waiters(2, 0)
            .iter()
            .map(|(w, msgs)| {
                (
                    w.client_id,
                    bodies(msgs).iter().map(|b| b.to_string()).collect(),
                )
            })
            .collect();
        assert_eq!(
            next,
            [(3, vec!["c".to_string()])],
            "the later marking waiter was not kept"
        );
        assert_eq!(
            m.take_waiters_for(9).len(),
            1,
            "another pane's waiter was taken"
        );
    }

    #[test]
    fn peeking_waiters_alone_mark_nothing() {
        let mut m = Messaging::new(limits());
        let (peeker, _rx, _c) = waiter(2, 2, false);
        park(&mut m, peeker);
        send(&mut m, "a", Instant::now()).unwrap();
        assert_eq!(m.answer_waiters(2, 0).len(), 1);
        assert_eq!(m.unread(2, 0), 1);
    }

    #[test]
    fn a_disconnect_drops_only_that_clients_waiters() {
        let mut m = Messaging::new(limits());
        let (a1, _a, _c1) = waiter(1, 2, false);
        let (b1, _c, _c3) = waiter(2, 2, false);
        park(&mut m, a1);
        let kept = park(&mut m, b1);
        assert_eq!(m.drop_waiters_of(1), 1);
        assert_eq!(m.drop_waiters_of(1), 0);
        assert_eq!(m.unpark(kept).map(|w| w.client_id), Some(2));
    }

    /// Every way a waiter leaves the store drops its cancel handle, which is
    /// what ends its timer: answered, its pane closing, its client leaving,
    /// or superseded by the same client's next wait.
    #[test]
    fn every_removal_path_cancels_the_waiters_timer() {
        use tokio::sync::oneshot::error::TryRecvError;
        let cancelled = |c: &mut Cancel| matches!(c.try_recv(), Err(TryRecvError::Closed));
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();

        let (answered, _a, mut c1) = waiter(1, 2, true);
        park(&mut m, answered);
        send(&mut m, "a", t0).unwrap();
        drop(m.answer_waiters(2, 0));
        assert!(cancelled(&mut c1), "answering did not cancel");

        let (closing, _b, mut c2) = waiter(2, 3, false);
        park(&mut m, closing);
        drop(m.take_waiters_for(3));
        assert!(cancelled(&mut c2), "the pane closing did not cancel");

        let (leaving, _c, mut c3) = waiter(3, 4, false);
        park(&mut m, leaving);
        m.drop_waiters_of(3);
        assert!(cancelled(&mut c3), "the client leaving did not cancel");

        let (first, _d, mut c4) = waiter(4, 5, false);
        let (second, _e, _c5) = waiter(4, 5, false);
        park(&mut m, first);
        drop(m.park(second));
        assert!(cancelled(&mut c4), "superseding did not cancel");

        let (parked, _f, mut c6) = waiter(5, 6, false);
        park(&mut m, parked);
        assert!(!cancelled(&mut c6), "a parked waiter's timer was cancelled");
    }

    #[test]
    fn mailbox_file_round_trip_drops_dead_panes_and_expired() {
        let lim = Limits {
            ttl_ms: 1_000,
            ..limits()
        };
        let mut m = Messaging::new(lim.clone());
        let t0 = Instant::now();
        m.send(pane(1), 2, "to-live".into(), None, 0, t0).unwrap();
        m.send(pane(1), 3, "to-dead".into(), None, 0, t0).unwrap();
        m.send(pane(1), 2, "old".into(), None, 0, t0).unwrap();
        m.send(pane(1), 2, "fresh".into(), None, 600, t0).unwrap();
        m.read(2, false, true, 0);
        m.set_name(2, Some("alpha")).unwrap();
        m.set_name(3, Some("gone")).unwrap();

        let file: MailboxFile =
            serde_json::from_str(&serde_json::to_string(&m.snapshot()).unwrap()).unwrap();
        let (r, pruned) = Messaging::restore(lim, file, |p| p == 2, HashSet::new(), 1_000);

        assert_eq!(
            pruned,
            Pruned {
                dead_pane: 1,
                expired: 2,
                names: 1,
                ..Pruned::default()
            }
        );
        let kept = r_read(&r, 2);
        assert_eq!(kept, [("fresh".to_string(), MsgState::Read)]);
        assert_eq!(r.pane_named("alpha"), Some(2));
        assert_eq!(r.pane_named("gone"), None);
    }

    fn r_read(m: &Messaging, pane: PaneId) -> Vec<(String, MsgState)> {
        m.mailboxes
            .get(&pane)
            .map(|mb| {
                mb.messages
                    .iter()
                    .map(|m| (m.body.clone(), m.state))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Ids are never reused, even when every mailbox was empty at the save,
    /// so a reply_to written before a restart cannot name a new message.
    #[test]
    fn restore_keeps_next_id_with_every_mailbox_empty() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        for _ in 0..3 {
            send(&mut m, "x", t0).unwrap();
        }
        m.drop_pane(2);
        let (mut r, _) = Messaging::restore(limits(), m.snapshot(), |_| true, HashSet::new(), 0);
        assert_eq!(send(&mut r, "after", t0), Ok(4));
    }

    /// Every pane id the file refers to (a recipient, a sender, a name) is
    /// below the high-water mark, so a restart can reserve ids past it and
    /// never hand a surviving message's sender id to a new pane.
    #[test]
    fn the_snapshot_records_the_highest_pane_id_it_refers_to() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        m.send(pane(40), 2, "a".into(), None, 0, t0).unwrap();
        m.set_name(7, Some("seven")).unwrap();
        assert_eq!(m.snapshot().pane_high_water, 40);
        m.set_name(55, Some("fiftyfive")).unwrap();
        assert_eq!(m.snapshot().pane_high_water, 55);
        assert_eq!(Messaging::new(limits()).snapshot().pane_high_water, 0);
    }

    #[test]
    fn a_change_marks_the_store_dirty_and_a_look_does_not() {
        let mut m = Messaging::new(limits());
        let t0 = Instant::now();
        assert!(!m.take_dirty());
        send(&mut m, "a", t0).unwrap();
        assert!(m.take_dirty(), "a send");
        assert!(!m.take_dirty(), "dirty was reported twice");
        m.read(2, true, false, 0);
        assert!(!m.take_dirty(), "a peek changed nothing");
        m.read(2, false, true, 0);
        assert!(m.take_dirty(), "a marking read");
        m.set_name(2, Some("alpha")).unwrap();
        assert!(m.take_dirty(), "a name");
        m.drop_pane(2);
        assert!(m.take_dirty(), "a closed pane");
        assert_eq!(m.ack(9, &[1]), 0);
        assert!(!m.take_dirty(), "an ack that marked nothing");
    }

    fn stored(id: MsgId, to: PaneId, body: &str) -> Message {
        Message {
            id,
            from: pane(1),
            to,
            body: body.into(),
            created_ms: 0,
            expires_ms: 1_000,
            state: MsgState::Queued,
            reply_to: None,
        }
    }

    fn file_of(messages: Vec<Message>) -> MailboxFile {
        MailboxFile {
            version: MAILBOX_FILE_VERSION,
            next_id: 1,
            pane_high_water: 0,
            names: Vec::new(),
            messages,
        }
    }

    /// Every id is checked, field by field: one above 2^53 would overflow the
    /// arithmetic done on it at load, and no real file ever holds one.
    #[test]
    fn an_id_beyond_2_to_the_53_makes_the_file_corrupt() {
        let huge = MAX_FILE_ID + 1;
        assert!(file_of(vec![stored(1, 2, "a")]).check_ids().is_ok());
        let mut f = file_of(Vec::new());
        f.next_id = huge;
        assert!(f.check_ids().is_err(), "next_id");
        let mut f = file_of(Vec::new());
        f.pane_high_water = huge;
        assert!(f.check_ids().is_err(), "pane_high_water");
        assert!(
            file_of(vec![stored(huge, 2, "a")]).check_ids().is_err(),
            "message id"
        );
        assert!(
            file_of(vec![stored(1, huge, "a")]).check_ids().is_err(),
            "recipient"
        );
        let mut from_huge = stored(1, 2, "a");
        from_huge.from = pane(huge);
        assert!(file_of(vec![from_huge]).check_ids().is_err(), "sender");
        let mut f = file_of(Vec::new());
        f.names = vec![(huge, "alpha".into())];
        assert!(f.check_ids().is_err(), "a name's pane");
        let mut reply = stored(1, 2, "a");
        reply.reply_to = Some(MsgRef {
            host: None,
            id: huge,
        });
        assert!(file_of(vec![reply]).check_ids().is_err(), "reply_to");
    }

    #[test]
    fn restoring_ids_at_the_limit_does_not_overflow() {
        let mut f = file_of(vec![stored(MAX_FILE_ID, 2, "a")]);
        f.next_id = MAX_FILE_ID;
        let (mut r, _) = Messaging::restore(limits(), f, |_| true, HashSet::new(), 0);
        assert_eq!(send(&mut r, "next", Instant::now()), Ok(MAX_FILE_ID + 1));
    }

    /// The file is re-validated the way a send is: nothing a send would
    /// refuse is restored, and every drop is counted.
    #[test]
    fn restore_revalidates_what_a_send_would_refuse() {
        let lim = Limits {
            max_body: 8,
            max_messages: 2,
            ttl_ms: 1_000,
            ..limits()
        };
        let mut bad_name = stored(4, 2, "c");
        bad_name.from = Sender::Pane {
            pane_id: 1,
            name: Some("x\nFrom: boss".into()),
            session: "main".into(),
        };
        let mut far_future = stored(8, 3, "e");
        far_future.expires_ms = u64::MAX;
        let mut dash_host = stored(9, 2, "g");
        dash_host.from = Sender::Remote {
            host: "-oProxyCommand".into(),
            pane_id: 1,
            name: None,
        };
        let f = file_of(vec![
            stored(1, 2, "ok"),
            stored(2, 2, "esc\u{1b}]52"),
            stored(11, 2, "rlo\u{202E}"),
            dash_host,
            stored(3, 2, "too-long!"),
            bad_name,
            stored(1, 2, "dup-id"),
            stored(6, 2, "fits"),
            stored(7, 2, "over-cap"),
            far_future,
        ]);
        let (r, pruned) = Messaging::restore(lim, f, |_| true, HashSet::new(), 0);
        assert_eq!(pruned.invalid, 6, "{pruned:?}");
        assert_eq!(pruned.over_cap, 1, "{pruned:?}");
        assert_eq!(
            r_read(&r, 2),
            [
                ("ok".to_string(), MsgState::Queued),
                ("fits".to_string(), MsgState::Queued)
            ]
        );
        let clamped = r.mailboxes[&3].messages[0].expires_ms;
        assert_eq!(clamped, 1_000, "expiry was not clamped to created + ttl");
    }

    #[test]
    fn restore_repairs_a_bad_session_label_and_keeps_the_message() {
        let labelled = |id, session: &str| {
            let mut msg = stored(id, 2, "kept");
            msg.from = Sender::Pane {
                pane_id: 1,
                name: None,
                session: session.into(),
            };
            msg
        };
        let f = file_of(vec![
            labelled(1, "s\u{1b}[2J"),
            labelled(2, "main\u{202E}"),
            labelled(3, "main"),
        ]);
        let (r, pruned) = Messaging::restore(limits(), f, |_| true, HashSet::new(), 0);
        assert_eq!((pruned.invalid, pruned.relabelled), (0, 2), "{pruned:?}");
        let sessions: Vec<String> = r.mailboxes[&2]
            .messages
            .iter()
            .map(|m| match &m.from {
                Sender::Pane { session, .. } => session.clone(),
                Sender::Remote { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(sessions, ["s\u{FFFD}[2J", "main\u{FFFD}", "main"]);
    }

    #[test]
    fn restore_drops_a_reserved_name_and_keeps_the_mail() {
        let mut from_user = stored(1, 2, "kept");
        from_user.from = Sender::Pane {
            pane_id: 1,
            name: Some("user".into()),
            session: "main".into(),
        };
        // Invalid for another reason (a control character): counted invalid,
        // not as a name removed from mail that was kept.
        let mut invalid_from_user = stored(3, 2, "bad\u{1b}body");
        invalid_from_user.from = from_user.from.clone();
        let mut f = file_of(vec![
            from_user,
            stored(2, 2, "also kept"),
            invalid_from_user,
        ]);
        f.names = vec![(2, "user".into()), (1, "alpha".into())]
            .into_iter()
            .collect();
        let (r, pruned) = Messaging::restore(limits(), f, |_| true, HashSet::new(), 0);
        assert_eq!(
            (pruned.unnamed, pruned.invalid, pruned.names),
            (1, 1, 1),
            "{pruned:?}"
        );
        assert_eq!(r.mailboxes[&2].messages.len(), 2, "the mail is kept");
        assert!(matches!(
            &r.mailboxes[&2].messages[0].from,
            Sender::Pane { name: None, .. }
        ));
        assert_eq!(r.name_of(2), None, "the reserved name is dropped");
        assert_eq!(r.name_of(1), Some("alpha"));
    }

    #[test]
    fn restore_keeps_the_closed_panes_closed() {
        let f = file_of(vec![stored(1, 7, "late")]);
        let (mut r, pruned) = Messaging::restore(limits(), f, |_| true, HashSet::from([7]), 0);
        assert_eq!(pruned.dead_pane, 1);
        assert_eq!(
            send(&mut r, "x", Instant::now()).map(|_| ()),
            Ok(()),
            "an unrelated pane was refused"
        );
        assert_eq!(
            r.send(pane(1), 7, "y".into(), None, 0, Instant::now()),
            Err(Refusal::NoSuchTarget("%7".into())),
            "the closed set was not carried"
        );
    }

    #[test]
    fn every_refusal_has_its_wire_code() {
        let codes = [
            (Refusal::BadAddress("x".into()), "bad_address"),
            (Refusal::BadName("X".into()), "bad_name"),
            (
                Refusal::NameTaken {
                    name: "a".into(),
                    holder: 1,
                },
                "name_taken",
            ),
            (Refusal::NoSuchTarget("a".into()), "no_such_target"),
            (
                Refusal::MessageTooLarge { bytes: 2, max: 1 },
                "message_too_large",
            ),
            (
                Refusal::MailboxFull {
                    pane: 1,
                    full: Full::Messages { count: 1, max: 1 },
                },
                "mailbox_full",
            ),
            (Refusal::RateLimited { per_10s: 1 }, "rate_limited"),
            (
                Refusal::BadBody {
                    character: '\u{1b}',
                    at_byte: 0,
                },
                "bad_body",
            ),
        ];
        for (r, code) in codes {
            assert_eq!(r.code(), code);
            assert!(!r.message().is_empty());
        }
        assert!(Refusal::NameTaken {
            name: "alpha".into(),
            holder: 3
        }
        .message()
        .contains("%3"));
    }
}
