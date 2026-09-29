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
                format!("bad name {name:?}: use [a-z][a-z0-9_-]{{0,31}}")
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
            if !seen.insert(msg.id) || !valid_stored(&msg, &limits) {
                pruned.invalid += 1;
                continue;
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
