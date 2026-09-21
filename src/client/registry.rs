//! Client-side connection registry.
//!
//! Holds the local connection plus a map of remote connections. Exactly one
//! connection is the **foreground** — it streams renders and receives input.
//! All connections funnel their decoded [`ServerMessage`]s through a single
//! in-process `mpsc` channel as [`Incoming`] values, tagged by source, so the
//! hot render/input loop can select over one receiver while routing messages
//! by origin.
//!
//! Idle remote connections have only ever sent `ListSessionTree` (never
//! `Attach`), so per the server's existing logic they never stream renders —
//! they stay quiet until made foreground.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio::io::AsyncWrite;
use tokio::process::Child;
use tokio::sync::mpsc;

use crate::client::terminal::RemuxClient;
use crate::config::RemoteConfig;
use crate::protocol::{ClientMessage, ServerMessage};
use crate::server::daemon::{read_message, write_message};

// ---------------------------------------------------------------------------
// Identifiers and state
// ---------------------------------------------------------------------------

/// Identifies a connection: the local server or a named remote.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ConnId {
    Local,
    Remote(String),
}

impl ConnId {
    /// Stable string key used to namespace expansion state, etc.
    pub fn key(&self) -> String {
        match self {
            ConnId::Local => "local".to_string(),
            ConnId::Remote(name) => format!("remote:{name}"),
        }
    }
}

/// Lifecycle state of a remote connection.
#[derive(Clone, Debug, PartialEq)]
pub enum RemoteState {
    NotConnected,
    Connecting,
    Connected,
    Failed(String),
    /// The user disconnected this remote in this client run
    /// ([`ConnectionManager::disconnect_remote`]). Distinct from
    /// `NotConnected`, which is "never dialled yet" and therefore eligible for
    /// the lazy dial a View cell starts: this one must NOT be redialled behind
    /// the user's back.
    ///
    /// The paths allowed to redial it are the ones the user drives: Enter or `l`
    /// on its session-manager row (`handle_enter` / `handle_expand` ->
    /// `ConnectRemote`), a jump that needs it (`switch_to_server`), and the
    /// `RemoteConnect` command. Everything else must leave it alone, which is why
    /// `begin_connect_remote` matches `NotConnected` rather than excluding a list
    /// of states.
    Disconnected,
}

/// The generation every `ConnId::Local` reader is tagged with.
///
/// The local connection is never reinstalled: losing it exits the client, so
/// there is no second transport for a stale `Closed` to belong to. Pinning it at
/// one fixed value makes `is_current_generation` answer `true` for Local without
/// a special case at each call site.
const LOCAL_GENERATION: u64 = 0;

/// A message routed in from one of the connections' reader tasks.
pub enum Incoming {
    /// A decoded server message from the given connection.
    Message(ConnId, ServerMessage),
    /// The given connection's reader hit EOF or an error, tagged with the
    /// [`RemoteEntry::generation`] the reader was spawned for.
    ///
    /// The tag is what tells a CURRENT drop from a stale one. Disconnecting and
    /// immediately reconnecting (`d`, `y`, Enter with no pause) installs a new
    /// transport while the old reader's EOF is still in flight, and an untagged
    /// `Closed` would then tear down the connection that just came up. Purely
    /// client-internal: nothing here crosses the wire, so it is not a protocol
    /// change.
    Closed(ConnId, u64),
    /// A background dial started by [`ConnectionManager::begin_connect_remote`]
    /// finished: the named remote either handed back a connected client or an
    /// error message. Delivered through the same channel as everything else so
    /// the SSH dial never blocks the client's event loop; the loop hands it to
    /// [`ConnectionManager::finish_remote_dial`].
    RemoteDialed(String, Result<Box<RemuxClient>, String>),
}

// ---------------------------------------------------------------------------
// RemoteEntry
// ---------------------------------------------------------------------------

/// Per-remote bookkeeping held by the [`ConnectionManager`].
struct RemoteEntry {
    config: RemoteConfig,
    state: RemoteState,
    /// The `remux_version` this remote's server reported during the handshake,
    /// set when the entry becomes `Connected`. Compared against this binary's
    /// [`crate::protocol::build_version`] to detect version skew.
    server_version: Option<String>,
    /// Keeps the ssh child alive; dropping it triggers `kill_on_drop`.
    _child: Option<Child>,
    /// Whether this entry originates from the `[remotes]` config map (vs. an
    /// ad-hoc remote added at runtime via `RemoteConnect`/`add_remote`). Only
    /// config-derived entries are eligible for removal by [`update_remotes`]
    /// when they disappear from a reloaded config.
    from_config: bool,
    /// Which transport this entry is on, incremented by [`install_remote`] each
    /// time a new one is adopted. Every reader task is spawned tagged with the
    /// generation it reads for, so an [`Incoming::Closed`] from a superseded
    /// transport is recognised and ignored instead of tearing down the live one.
    generation: u64,
}

// ---------------------------------------------------------------------------
// ConnectionManager
// ---------------------------------------------------------------------------

type BoxWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// Owns every open connection and the shared incoming-message channel.
pub struct ConnectionManager {
    /// Per-connection writer, keyed by connection id.
    writers: HashMap<ConnId, BoxWriter>,
    /// Remote bookkeeping (config + state + child), keyed by name.
    remotes: HashMap<String, RemoteEntry>,
    /// The connection that currently streams renders / receives input.
    foreground: ConnId,
    /// Cloned into each reader task to funnel decoded messages back.
    tx: mpsc::UnboundedSender<Incoming>,
    /// The paired receiver drained by [`ConnectionManager::recv`].
    rx: mpsc::UnboundedReceiver<Incoming>,
    /// The `remux_version` the local server reported during the handshake. The
    /// local `ConnId::Local` connection has no [`RemoteEntry`], so its reported
    /// version is tracked here for version-skew detection.
    local_server_version: Option<String>,
    /// The shared-view registry snapshot captured from the local client during
    /// its startup handshake (see
    /// [`RemuxClient::take_captured_views`](crate::client::terminal::RemuxClient::take_captured_views)).
    /// The steady-state loop takes it once at startup to seed its view cache, so
    /// a terminal that connects after a shared view already exists lists it
    /// immediately. `None` once taken (or when nothing was captured).
    initial_view_infos: Option<Vec<crate::protocol::ViewInfo>>,
}

impl ConnectionManager {
    /// Build a manager around a connected local client, seeding the remotes map
    /// (all `NotConnected`) from config.
    pub fn new(mut local: RemuxClient, remotes: &HashMap<String, RemoteConfig>) -> Self {
        let mut mgr = Self::empty(remotes, ConnId::Local);
        // Capture the reported version + the startup ViewList snapshot before
        // `into_split` consumes the client.
        mgr.local_server_version = Some(local.server_version().to_string());
        mgr.initial_view_infos = local.take_captured_views();
        let (reader, writer, _child) = local.into_split();
        mgr.writers.insert(ConnId::Local, writer);
        mgr.spawn_reader(ConnId::Local, LOCAL_GENERATION, reader);
        mgr
    }

    /// Build a manager whose foreground is a synthetic remote id, used by the
    /// direct `attach-remote` CLI flow (no `[remotes]` map involved).
    pub fn new_foreground_remote(name: &str, client: RemuxClient) -> Self {
        let id = ConnId::Remote(name.to_string());
        let mut mgr = Self::empty(&HashMap::new(), id.clone());
        // Capture the reported version before `into_split` consumes the client.
        let server_version = Some(client.server_version().to_string());
        // Record the remote as already Connected so the roster reflects it.
        mgr.remotes.insert(
            name.to_string(),
            RemoteEntry {
                config: RemoteConfig::default(),
                state: RemoteState::Connected,
                server_version,
                _child: None,
                from_config: false,
                generation: 1,
            },
        );
        let (reader, writer, child) = client.into_split();
        mgr.writers.insert(id.clone(), writer);
        if let Some(entry) = mgr.remotes.get_mut(name) {
            entry._child = child;
        }
        mgr.spawn_reader(id, 1, reader);
        mgr
    }

    /// Construct an empty manager (no connections wired up) with the remotes map
    /// seeded to `NotConnected`. Shared by [`ConnectionManager::new`] and tests.
    fn empty(remotes: &HashMap<String, RemoteConfig>, foreground: ConnId) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let remotes = remotes
            .iter()
            .map(|(name, config)| {
                (
                    name.clone(),
                    RemoteEntry {
                        config: config.clone(),
                        state: RemoteState::NotConnected,
                        server_version: None,
                        _child: None,
                        from_config: true,
                        generation: 0,
                    },
                )
            })
            .collect();
        Self {
            writers: HashMap::new(),
            remotes,
            foreground,
            tx,
            rx,
            local_server_version: None,
            initial_view_infos: None,
        }
    }

    /// Take the startup shared-view registry snapshot captured from the local
    /// client's handshake, leaving `None` behind. Called once by the steady-state
    /// loop to seed its view cache.
    pub fn take_initial_view_infos(&mut self) -> Option<Vec<crate::protocol::ViewInfo>> {
        self.initial_view_infos.take()
    }

    /// Spawn a reader task that pumps decoded messages from `reader` into the
    /// shared channel as [`Incoming`] values, emitting `Closed` on EOF/error.
    fn spawn_reader(
        &self,
        id: ConnId,
        generation: u64,
        mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    ) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            loop {
                match read_message::<ServerMessage>(&mut reader).await {
                    Ok(Some(msg)) => {
                        if tx.send(Incoming::Message(id.clone(), msg)).is_err() {
                            // Receiver gone; nothing more to do.
                            break;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let _ = tx.send(Incoming::Closed(id.clone(), generation));
                        break;
                    }
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // Connecting remotes
    // -----------------------------------------------------------------------

    /// Whether `name` is a key in the remotes map (configured or ad-hoc).
    pub fn has_remote(&self, name: &str) -> bool {
        self.remotes.contains_key(name)
    }

    /// Add a remote entry in state `NotConnected` with the given config, mirroring
    /// how [`ConnectionManager::new`] seeds config remotes. Idempotent: if `name`
    /// is already present it is left unchanged. Does NOT connect.
    pub fn add_remote(&mut self, name: String, config: RemoteConfig) {
        self.remotes.entry(name).or_insert_with(|| RemoteEntry {
            config,
            state: RemoteState::NotConnected,
            server_version: None,
            _child: None,
            from_config: false,
            generation: 0,
        });
    }

    /// Reconcile the remotes map against a reloaded config's `[remotes]` map.
    ///
    /// For each `(name, config)` in `new_remotes`:
    /// - if an entry already exists, its `config` is updated in place and it is
    ///   marked config-derived (`from_config = true`); its `state` and live
    ///   connection are left untouched, so a `Connected` remote keeps running
    ///   and a `NotConnected`/`Failed` remote picks up the new config on its
    ///   next connect;
    /// - otherwise a fresh `NotConnected` entry is inserted (like
    ///   [`add_remote`]) marked config-derived.
    ///
    /// Entries no longer present in `new_remotes` are removed ONLY when they are
    /// config-derived (`from_config == true`), currently idle (`NotConnected`,
    /// `Failed` or `Disconnected`), and not the foreground. Ad-hoc remotes (added via
    /// `RemoteConnect`/`add_remote`) and any `Connected`/`Connecting` remote are
    /// never dropped here.
    ///
    /// Note: an existing ad-hoc entry that now appears in the config becomes
    /// config-derived (`from_config` flips `false -> true`), so it is eligible
    /// for a future config-removal like any other configured remote.
    pub fn update_remotes(&mut self, new_remotes: &HashMap<String, RemoteConfig>) {
        // Update existing / insert new.
        for (name, config) in new_remotes {
            match self.remotes.get_mut(name) {
                Some(entry) => {
                    entry.config = config.clone();
                    entry.from_config = true;
                }
                None => {
                    self.remotes.insert(
                        name.clone(),
                        RemoteEntry {
                            config: config.clone(),
                            state: RemoteState::NotConnected,
                            server_version: None,
                            _child: None,
                            from_config: true,
                            generation: 0,
                        },
                    );
                }
            }
        }

        // Remove config-derived entries that dropped out of the config, but only
        // if they are idle and not the foreground.
        let to_remove: Vec<String> = self
            .remotes
            .iter()
            .filter(|(name, entry)| {
                entry.from_config
                    && !new_remotes.contains_key(*name)
                    && matches!(
                        entry.state,
                        RemoteState::NotConnected
                            | RemoteState::Failed(_)
                            | RemoteState::Disconnected
                    )
                    && self.foreground != ConnId::Remote((*name).clone())
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in to_remove {
            self.remotes.remove(&name);
            self.writers.remove(&ConnId::Remote(name));
        }
    }

    /// Lazily connect to a named remote over SSH. No-op if already `Connected`.
    /// On success spawns a reader task and marks the remote `Connected`; on
    /// failure or timeout marks it `Failed(msg)` and returns the error.
    pub async fn connect_remote(&mut self, name: &str) -> Result<()> {
        let config = match self.remotes.get(name) {
            Some(entry) => {
                if entry.state == RemoteState::Connected {
                    return Ok(());
                }
                entry.config.clone()
            }
            None => anyhow::bail!("unknown remote '{name}'"),
        };

        self.set_state(name, RemoteState::Connecting);
        log::info!("registry: connecting to remote '{name}' ({})", config.ssh);

        let connect = RemuxClient::connect_ssh(
            &config.ssh,
            config.port,
            config.identity.as_deref(),
            &config.extra_args,
            &config.remux_path,
        );
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), connect).await;

        match result {
            Ok(Ok(client)) => {
                self.install_remote(name, client);
                Ok(())
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                log::warn!("registry: remote '{name}' failed: {msg}");
                self.fail_remote(name, msg.clone());
                Err(e)
            }
            Err(_) => {
                let msg = "connection timed out".to_string();
                log::warn!("registry: remote '{name}' timed out");
                self.fail_remote(name, msg.clone());
                anyhow::bail!("connecting to remote '{name}': {msg}")
            }
        }
    }

    /// Adopt a freshly dialed remote client as the transport for `name`: store
    /// its writer, keep the ssh child alive, record the reported version, mark
    /// the entry `Connected`, advance its generation and spawn its reader task.
    /// Shared by the blocking [`connect_remote`](Self::connect_remote) and the
    /// background [`finish_remote_dial`](Self::finish_remote_dial).
    fn install_remote(&mut self, name: &str, client: RemuxClient) {
        let id = ConnId::Remote(name.to_string());
        // Capture the reported version before `into_split` consumes it.
        let server_version = client.server_version().to_string();
        let (reader, writer, child) = client.into_split();
        self.writers.insert(id.clone(), writer);
        // The reader is tagged with the generation this transport is installed
        // AT, so the bump must happen before it is spawned.
        let mut generation = LOCAL_GENERATION;
        if let Some(entry) = self.remotes.get_mut(name) {
            entry._child = child;
            entry.state = RemoteState::Connected;
            entry.server_version = Some(server_version);
            entry.generation += 1;
            generation = entry.generation;
        }
        self.spawn_reader(id, generation, reader);
        log::info!("registry: remote '{name}' connected (generation {generation})");
    }

    /// Start connecting to a named remote **in the background**, reporting the
    /// outcome as an [`Incoming::RemoteDialed`] on the shared channel.
    ///
    /// Unlike [`connect_remote`](Self::connect_remote) this never awaits the SSH
    /// dial, so a caller on the client's event loop (a View subscribing a cell
    /// that names a remote this terminal has not connected) stays responsive
    /// while the connection is established. The entry is marked `Connecting`
    /// immediately; the caller must feed the resulting `RemoteDialed` to
    /// [`finish_remote_dial`](Self::finish_remote_dial).
    ///
    /// No-op (returning `false`) unless the remote is known and `NotConnected`,
    /// so repeated calls — the View subscribe pass runs on every layout/focus
    /// change — never pile up dials, and no other state is dialled behind the
    /// user's back (see [`RemoteState::Disconnected`]).
    pub fn begin_connect_remote(&mut self, name: &str) -> bool {
        let config = match self.remotes.get(name) {
            Some(entry) if entry.state == RemoteState::NotConnected => entry.config.clone(),
            _ => return false,
        };
        self.set_state(name, RemoteState::Connecting);
        log::info!(
            "registry: background-connecting to remote '{name}' ({})",
            config.ssh
        );
        let tx = self.tx.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let connect = RemuxClient::connect_ssh(
                &config.ssh,
                config.port,
                config.identity.as_deref(),
                &config.extra_args,
                &config.remux_path,
            );
            let result =
                match tokio::time::timeout(std::time::Duration::from_secs(10), connect).await {
                    Ok(Ok(client)) => Ok(Box::new(client)),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(_) => Err("connection timed out".to_string()),
                };
            let _ = tx.send(Incoming::RemoteDialed(name, result));
        });
        true
    }

    /// Apply the outcome of a [`begin_connect_remote`](Self::begin_connect_remote)
    /// dial: adopt the connection on success, mark the remote `Failed` on error.
    /// Returns whether the remote is now connected.
    ///
    /// A dial that lands after the remote was connected by some other path (the
    /// session manager's blocking connect) is dropped, so a duplicate transport
    /// and a second reader task can never be installed for one remote.
    pub fn finish_remote_dial(
        &mut self,
        name: &str,
        result: Result<Box<RemuxClient>, String>,
    ) -> bool {
        if self.remote_state(name) == RemoteState::Connected {
            log::debug!("registry: dropping late dial for already-connected '{name}'");
            return true;
        }
        // The user disconnected this remote while the dial was in flight.
        // Installing it now would hand them back the connection they just ended
        // -- and `Disconnected` exists precisely to mean "not without asking".
        if self.remote_state(name) == RemoteState::Disconnected {
            log::debug!("registry: dropping dial for user-disconnected '{name}'");
            return false;
        }
        match result {
            Ok(client) => {
                self.install_remote(name, *client);
                true
            }
            Err(msg) => {
                log::warn!("registry: background connect to remote '{name}' failed: {msg}");
                self.fail_remote(name, msg);
                false
            }
        }
    }

    /// Set a remote's state (internal).
    fn set_state(&mut self, name: &str, state: RemoteState) {
        if let Some(entry) = self.remotes.get_mut(name) {
            entry.state = state;
        }
    }

    /// Disconnect a remote AT THE USER'S REQUEST: tear its transport down like
    /// [`fail_remote`](Self::fail_remote) and record [`RemoteState::Disconnected`],
    /// which is what keeps the lazy dial paths from bringing it straight back.
    ///
    /// Only a live entry (`Connected`, or `Connecting` with a dial in flight) is
    /// touched. An unknown, `NotConnected`, `Failed` or already-`Disconnected`
    /// remote is left exactly as it is: there was no connection to end, and
    /// overwriting `Failed` would throw away the reason it failed.
    pub fn disconnect_remote(&mut self, name: &str) {
        match self.remotes.get(name).map(|e| &e.state) {
            Some(RemoteState::Connected) | Some(RemoteState::Connecting) => {}
            _ => {
                log::debug!("registry: disconnect_remote('{name}') -- nothing live to end");
                return;
            }
        }
        // Dropping the writer and the child is the whole teardown, because
        // `_child` is `kill_on_drop`. For a `Connected` remote that kills the ssh
        // (and with it the far relay) here. For a `Connecting` one it does not:
        // the child is still inside the dial task's `RemuxClient`, so it dies when
        // `finish_remote_dial` drops that, which can be as late as the 10s dial
        // timeout. The user-visible state changes immediately either way.
        self.writers.remove(&ConnId::Remote(name.to_string()));
        if let Some(entry) = self.remotes.get_mut(name) {
            entry.state = RemoteState::Disconnected;
            entry._child = None;
            // Same reason `fail_remote` would: a stamp from the connection that
            // just ended would be compared against on the next reconnect before
            // the new handshake has replaced it.
            entry.server_version = None;
        }
        log::info!("registry: remote '{name}' disconnected by the user");
    }

    /// Mark a remote `Failed` and tear down its transport (writer + child) so
    /// `kill_on_drop` fires on the ssh process.
    pub fn fail_remote(&mut self, name: &str, msg: String) {
        self.writers.remove(&ConnId::Remote(name.to_string()));
        if let Some(entry) = self.remotes.get_mut(name) {
            entry.state = RemoteState::Failed(msg);
            entry._child = None;
        }
    }

    // -----------------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------------

    /// Send a message to a specific connection.
    pub async fn send(&mut self, id: &ConnId, msg: ClientMessage) -> Result<()> {
        let writer = self
            .writers
            .get_mut(id)
            .with_context(|| format!("no open connection for {id:?}"))?;
        write_message(writer, &msg).await
    }

    /// Send a message to the current foreground connection.
    pub async fn send_foreground(&mut self, msg: ClientMessage) -> Result<()> {
        let id = self.foreground.clone();
        self.send(&id, msg).await
    }

    // -----------------------------------------------------------------------
    // Foreground bookkeeping
    // -----------------------------------------------------------------------

    pub fn set_foreground(&mut self, id: ConnId) {
        self.foreground = id;
    }

    pub fn foreground(&self) -> &ConnId {
        &self.foreground
    }

    pub fn is_foreground(&self, id: &ConnId) -> bool {
        &self.foreground == id
    }

    // -----------------------------------------------------------------------
    // Receiving
    // -----------------------------------------------------------------------

    /// Await the next incoming message from any connection.
    pub async fn recv(&mut self) -> Option<Incoming> {
        self.rx.recv().await
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// Whether an [`Incoming::Closed`] tagged `generation` describes the
    /// transport this connection is on NOW.
    ///
    /// `false` means the reader that sent it has been superseded by a reconnect,
    /// so the drop it reports is already over and acting on it would tear down a
    /// live connection. Local is always current (see [`LOCAL_GENERATION`]), and so
    /// is an unknown remote, whose `Closed` has no live transport to damage.
    pub fn is_current_generation(&self, id: &ConnId, generation: u64) -> bool {
        match id {
            ConnId::Local => true,
            ConnId::Remote(name) => match self.remotes.get(name) {
                Some(entry) => entry.generation == generation,
                None => true,
            },
        }
    }

    /// The generation the named connection's current transport is on, for a
    /// caller that has to report a drop it is causing itself.
    pub fn current_generation(&self, id: &ConnId) -> u64 {
        match id {
            ConnId::Local => LOCAL_GENERATION,
            ConnId::Remote(name) => self
                .remotes
                .get(name)
                .map_or(LOCAL_GENERATION, |e| e.generation),
        }
    }

    /// Current state of a named remote (or `NotConnected` if unknown).
    pub fn remote_state(&self, name: &str) -> RemoteState {
        self.remotes
            .get(name)
            .map(|e| e.state.clone())
            .unwrap_or(RemoteState::NotConnected)
    }

    /// All currently-open connection ids: `Local` plus every `Connected` remote.
    pub fn connected_ids(&self) -> Vec<ConnId> {
        let mut ids = Vec::new();
        if self.writers.contains_key(&ConnId::Local) {
            ids.push(ConnId::Local);
        }
        let mut names: Vec<&String> = self
            .remotes
            .iter()
            .filter(|(_, e)| e.state == RemoteState::Connected)
            .map(|(n, _)| n)
            .collect();
        names.sort();
        for name in names {
            ids.push(ConnId::Remote(name.clone()));
        }
        // For the synthetic foreground-remote flow the remote may be Connected
        // yet not present in the `remotes` map's connected filter path above;
        // ensure the foreground is always included if it has a writer.
        if let ConnId::Remote(_) = &self.foreground {
            if self.writers.contains_key(&self.foreground) && !ids.contains(&self.foreground) {
                ids.push(self.foreground.clone());
            }
        }
        ids
    }

    /// Version skew for a connected server: `Some(server_version)` when the
    /// connected server reported a `remux_version` that differs from this
    /// binary's own [`crate::protocol::build_version`], else `None`.
    ///
    /// Only `Connected` servers are considered — a `NotConnected`/`Failed`
    /// remote (or one whose version was never captured) reports `None`. The
    /// local server is always treated as connected.
    ///
    /// Comparison is [`crate::protocol::stamps_match`], NOT string equality: the
    /// `-dirty` suffix is build-environment noise (`cargo install --git` touches
    /// `Cargo.lock` in its own checkout and stamps every install `-dirty`) and
    /// comparing it made a correctly-installed remote report skew for ever. The
    /// string returned for display is still the server's RAW stamp — the
    /// normalisation decides whether to warn, and never launders what is shown.
    pub fn version_mismatch(&self, id: &ConnId) -> Option<String> {
        let ours = crate::protocol::build_version();
        let reported = match id {
            ConnId::Local => self.local_server_version.as_ref(),
            ConnId::Remote(name) => match self.remotes.get(name) {
                Some(entry) if entry.state == RemoteState::Connected => {
                    entry.server_version.as_ref()
                }
                _ => None,
            },
        };
        reported
            .filter(|v| !crate::protocol::stamps_match(v, &ours))
            .cloned()
    }

    /// Ordered roster of servers for the session-manager tree: Local first
    /// (labelled `"local"`), then remotes sorted by name with their state and
    /// any version-skew (`Some(server_version)` when the server is outdated
    /// relative to this binary, else `None`).
    pub fn server_roster(&self) -> Vec<(ConnId, String, RemoteState, Option<String>)> {
        let mut roster = vec![(
            ConnId::Local,
            "local".to_string(),
            RemoteState::Connected,
            self.version_mismatch(&ConnId::Local),
        )];
        let mut names: Vec<&String> = self.remotes.keys().collect();
        names.sort();
        for name in names {
            let entry = &self.remotes[name];
            let id = ConnId::Remote(name.clone());
            let mismatch = self.version_mismatch(&id);
            roster.push((id, name.clone(), entry.state.clone(), mismatch));
        }
        roster
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_remotes() -> HashMap<String, RemoteConfig> {
        let mut m = HashMap::new();
        m.insert(
            "zulu".to_string(),
            RemoteConfig {
                ssh: "user@zulu".to_string(),
                ..Default::default()
            },
        );
        m.insert(
            "alpha".to_string(),
            RemoteConfig {
                ssh: "user@alpha".to_string(),
                ..Default::default()
            },
        );
        m
    }

    #[test]
    fn roster_is_local_first_then_sorted_remotes() {
        let mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        let roster = mgr.server_roster();
        assert_eq!(roster.len(), 3);
        assert_eq!(roster[0].0, ConnId::Local);
        assert_eq!(roster[0].1, "local");
        assert_eq!(roster[1].0, ConnId::Remote("alpha".to_string()));
        assert_eq!(roster[2].0, ConnId::Remote("zulu".to_string()));
        // Remotes start NotConnected.
        assert_eq!(roster[1].2, RemoteState::NotConnected);
        assert_eq!(roster[2].2, RemoteState::NotConnected);
    }

    #[test]
    fn state_transitions() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        assert_eq!(mgr.remote_state("alpha"), RemoteState::NotConnected);

        mgr.set_state("alpha", RemoteState::Connecting);
        assert_eq!(mgr.remote_state("alpha"), RemoteState::Connecting);

        mgr.set_state("alpha", RemoteState::Connected);
        assert_eq!(mgr.remote_state("alpha"), RemoteState::Connected);

        mgr.fail_remote("alpha", "boom".to_string());
        assert_eq!(
            mgr.remote_state("alpha"),
            RemoteState::Failed("boom".to_string())
        );
        // Failing tears the writer down (there was none, but must not panic).
        assert!(!mgr
            .writers
            .contains_key(&ConnId::Remote("alpha".to_string())));
    }

    #[test]
    fn unknown_remote_state_is_not_connected() {
        let mgr = ConnectionManager::empty(&HashMap::new(), ConnId::Local);
        assert_eq!(mgr.remote_state("ghost"), RemoteState::NotConnected);
    }

    #[test]
    fn connected_ids_only_counts_connected_remotes() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        // No local writer wired up in the empty test manager, and no remotes
        // connected yet.
        assert!(mgr.connected_ids().is_empty());

        mgr.set_state("alpha", RemoteState::Connected);
        assert_eq!(
            mgr.connected_ids(),
            vec![ConnId::Remote("alpha".to_string())]
        );
    }

    #[test]
    fn add_remote_is_idempotent_and_has_remote_reports_presence() {
        let mut mgr = ConnectionManager::empty(&HashMap::new(), ConnId::Local);
        assert!(!mgr.has_remote("adhoc"));

        // First insert seeds a NotConnected entry with the given config.
        mgr.add_remote(
            "adhoc".to_string(),
            RemoteConfig {
                ssh: "user@adhoc".to_string(),
                ..Default::default()
            },
        );
        assert!(mgr.has_remote("adhoc"));
        assert_eq!(mgr.remote_state("adhoc"), RemoteState::NotConnected);

        // Mutate state, then re-add: the existing entry must be left unchanged.
        mgr.set_state("adhoc", RemoteState::Connected);
        mgr.add_remote(
            "adhoc".to_string(),
            RemoteConfig {
                ssh: "different@host".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(mgr.remote_state("adhoc"), RemoteState::Connected);
    }

    #[test]
    fn update_remotes_updates_existing_config_without_disturbing_connection() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        // Bring "alpha" up as if it were live.
        mgr.set_state("alpha", RemoteState::Connected);
        assert_eq!(mgr.remotes["alpha"].config.ssh, "user@alpha");

        // Reload with a new ssh target for "alpha" (and "zulu" unchanged).
        let mut new_map = sample_remotes();
        new_map.insert(
            "alpha".to_string(),
            RemoteConfig {
                ssh: "user@alpha-new".to_string(),
                ..Default::default()
            },
        );
        mgr.update_remotes(&new_map);

        // Config was updated in place, state left untouched.
        assert_eq!(mgr.remotes["alpha"].config.ssh, "user@alpha-new");
        assert_eq!(mgr.remote_state("alpha"), RemoteState::Connected);
    }

    #[test]
    fn update_remotes_adds_new_entry() {
        let mut mgr = ConnectionManager::empty(&HashMap::new(), ConnId::Local);
        assert!(!mgr.has_remote("beta"));

        let mut new_map = HashMap::new();
        new_map.insert(
            "beta".to_string(),
            RemoteConfig {
                ssh: "user@beta".to_string(),
                ..Default::default()
            },
        );
        mgr.update_remotes(&new_map);

        assert!(mgr.has_remote("beta"));
        assert_eq!(mgr.remote_state("beta"), RemoteState::NotConnected);
        assert_eq!(mgr.remotes["beta"].config.ssh, "user@beta");
        assert!(mgr.remotes["beta"].from_config);
    }

    #[test]
    fn update_remotes_removes_idle_config_absent_entry() {
        // Both start config-derived and NotConnected.
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        assert!(mgr.has_remote("alpha"));
        assert!(mgr.has_remote("zulu"));

        // New config drops "zulu".
        let mut new_map = HashMap::new();
        new_map.insert(
            "alpha".to_string(),
            RemoteConfig {
                ssh: "user@alpha".to_string(),
                ..Default::default()
            },
        );
        mgr.update_remotes(&new_map);

        // Idle + config-derived + config-absent => removed.
        assert!(mgr.has_remote("alpha"));
        assert!(!mgr.has_remote("zulu"));
    }

    #[test]
    fn update_remotes_keeps_connected_config_absent_entry() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        mgr.set_state("zulu", RemoteState::Connected);

        // New config drops "zulu", but it's live -> must be kept.
        let mut new_map = HashMap::new();
        new_map.insert(
            "alpha".to_string(),
            RemoteConfig {
                ssh: "user@alpha".to_string(),
                ..Default::default()
            },
        );
        mgr.update_remotes(&new_map);

        assert!(mgr.has_remote("zulu"));
        assert_eq!(mgr.remote_state("zulu"), RemoteState::Connected);
    }

    #[test]
    fn update_remotes_keeps_adhoc_config_absent_entry() {
        let mut mgr = ConnectionManager::empty(&HashMap::new(), ConnId::Local);
        // Ad-hoc remote (from_config = false), idle.
        mgr.add_remote(
            "adhoc".to_string(),
            RemoteConfig {
                ssh: "user@adhoc".to_string(),
                ..Default::default()
            },
        );
        assert!(!mgr.remotes["adhoc"].from_config);

        // Reload with an unrelated config; the ad-hoc entry is absent from it but
        // must NOT be removed (it wasn't config-derived).
        let mut new_map = HashMap::new();
        new_map.insert(
            "beta".to_string(),
            RemoteConfig {
                ssh: "user@beta".to_string(),
                ..Default::default()
            },
        );
        mgr.update_remotes(&new_map);

        assert!(mgr.has_remote("adhoc"));
    }

    #[test]
    fn version_mismatch_reports_only_for_differing_connected_servers() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        let ours = crate::protocol::build_version();

        // Local: no version captured yet -> no mismatch.
        assert_eq!(mgr.version_mismatch(&ConnId::Local), None);

        // Local reporting an identical version -> no mismatch.
        mgr.local_server_version = Some(ours.clone());
        assert_eq!(mgr.version_mismatch(&ConnId::Local), None);

        // Local reporting a different version -> mismatch surfaced.
        mgr.local_server_version = Some("0.1.0+deadbeef".to_string());
        assert_eq!(
            mgr.version_mismatch(&ConnId::Local),
            Some("0.1.0+deadbeef".to_string())
        );

        // Remote connected with a differing version -> mismatch.
        let alpha = ConnId::Remote("alpha".to_string());
        mgr.set_state("alpha", RemoteState::Connected);
        mgr.remotes.get_mut("alpha").unwrap().server_version = Some("0.1.0+stale".to_string());
        assert_eq!(
            mgr.version_mismatch(&alpha),
            Some("0.1.0+stale".to_string())
        );

        // Remote connected with the same version -> no mismatch.
        mgr.remotes.get_mut("alpha").unwrap().server_version = Some(ours.clone());
        assert_eq!(mgr.version_mismatch(&alpha), None);

        // A differing version but NotConnected -> no mismatch.
        mgr.set_state("alpha", RemoteState::NotConnected);
        mgr.remotes.get_mut("alpha").unwrap().server_version = Some("0.1.0+stale".to_string());
        assert_eq!(mgr.version_mismatch(&alpha), None);

        // A differing version but Failed -> no mismatch.
        mgr.fail_remote("alpha", "boom".to_string());
        mgr.remotes.get_mut("alpha").unwrap().server_version = Some("0.1.0+stale".to_string());
        assert_eq!(mgr.version_mismatch(&alpha), None);

        // Unknown remote -> no mismatch.
        assert_eq!(mgr.version_mismatch(&ConnId::Remote("ghost".into())), None);
    }

    /// The 10.0.0.2 report, at the level the user actually sees it: a remote
    /// installed with `cargo install --git` at THIS commit stamps `-dirty`
    /// (cargo touches `Cargo.lock` in its own checkout) and must not be flagged.
    ///
    /// `build_version()` is the right input here, unlike in `stamps_match`'s own
    /// tests: the point is that the manager compares against this binary's real
    /// stamp. Both arms are derived from it, so the assertion holds whether the
    /// tree the test runs in is clean or dirty.
    #[test]
    fn a_dirty_install_of_this_commit_is_not_flagged() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        let ours = crate::protocol::build_version();
        let clean = ours.strip_suffix("-dirty").unwrap_or(&ours).to_string();

        mgr.local_server_version = Some(format!("{clean}-dirty"));
        assert_eq!(
            mgr.version_mismatch(&ConnId::Local),
            None,
            "a `cargo install --git` build of this very commit was reported as skew"
        );

        mgr.local_server_version = Some(clean);
        assert_eq!(mgr.version_mismatch(&ConnId::Local), None);
    }

    #[test]
    fn server_roster_carries_version_mismatch() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        mgr.local_server_version = Some("0.1.0+stale".to_string());
        let roster = mgr.server_roster();
        // Local is first and now carries the mismatch as the 4th tuple element.
        assert_eq!(roster[0].0, ConnId::Local);
        assert_eq!(roster[0].3, Some("0.1.0+stale".to_string()));
        // Idle remotes never report a mismatch.
        assert_eq!(roster[1].3, None);
        assert_eq!(roster[2].3, None);
    }

    /// `disconnect_remote` is the USER's "stop talking to this remote": the
    /// transport goes away like a failure, but the state records that the user
    /// asked, so nothing dials it again behind their back.
    #[test]
    fn disconnect_remote_tears_down_and_records_the_user_choice() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        mgr.set_state("alpha", RemoteState::Connected);
        mgr.remotes.get_mut("alpha").unwrap().server_version = Some("0.1.0+x".to_string());
        mgr.writers.insert(
            ConnId::Remote("alpha".to_string()),
            Box::new(tokio::io::sink()),
        );
        assert_eq!(
            mgr.connected_ids(),
            vec![ConnId::Remote("alpha".to_string())]
        );

        mgr.disconnect_remote("alpha");

        assert_eq!(mgr.remote_state("alpha"), RemoteState::Disconnected);
        // The transport is gone, so the remote is no longer a send target and
        // drops out of every `connected_ids()` consumer (the switcher included).
        assert!(!mgr
            .writers
            .contains_key(&ConnId::Remote("alpha".to_string())));
        assert!(mgr.connected_ids().is_empty());
        // Same bookkeeping `fail_remote` clears: a stale version would be
        // reported as skew the moment the entry looked connected again.
        assert_eq!(mgr.remotes["alpha"].server_version, None);
        assert_eq!(mgr.version_mismatch(&ConnId::Remote("alpha".into())), None);
    }

    /// Disconnecting an unknown or already-idle remote is a no-op, not a panic:
    /// the command palette can name anything.
    #[test]
    fn disconnect_remote_on_an_unknown_or_idle_remote_is_inert() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        mgr.disconnect_remote("ghost");
        assert_eq!(mgr.remote_state("ghost"), RemoteState::NotConnected);
        // An idle entry is left where it was rather than being moved to
        // `Disconnected`: the user never had a connection to end.
        mgr.disconnect_remote("alpha");
        assert_eq!(mgr.remote_state("alpha"), RemoteState::NotConnected);
    }

    /// A config reload must not resurrect a disconnected remote. `update_remotes`
    /// refreshes config in place for every state, and `auto_connect` is honoured
    /// once at startup -- so the thing to pin is that the state survives.
    #[test]
    fn update_remotes_leaves_a_disconnected_entry_disconnected() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        mgr.set_state("alpha", RemoteState::Connected);
        mgr.disconnect_remote("alpha");

        let mut new_map = sample_remotes();
        new_map.insert(
            "alpha".to_string(),
            RemoteConfig {
                ssh: "user@alpha".to_string(),
                auto_connect: true,
                ..Default::default()
            },
        );
        mgr.update_remotes(&new_map);

        assert_eq!(mgr.remote_state("alpha"), RemoteState::Disconnected);
        assert!(mgr.remotes["alpha"].config.auto_connect);
    }

    /// A disconnected remote is IDLE, so a config that no longer mentions it
    /// drops it, exactly as it drops a `NotConnected`/`Failed` one. Otherwise
    /// the roster would carry a row nothing can ever reach again.
    #[test]
    fn update_remotes_removes_a_disconnected_config_absent_entry() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        mgr.set_state("zulu", RemoteState::Connected);
        mgr.disconnect_remote("zulu");

        let mut new_map = HashMap::new();
        new_map.insert(
            "alpha".to_string(),
            RemoteConfig {
                ssh: "user@alpha".to_string(),
                ..Default::default()
            },
        );
        mgr.update_remotes(&new_map);

        assert!(!mgr.has_remote("zulu"));
        assert!(mgr.has_remote("alpha"));
    }

    /// Stand in for [`install_remote`] in a test: adopt a transport for `name`
    /// exactly as it does, minus the `RemuxClient` a unit test cannot build.
    ///
    /// The bump and the writer are what matter to the generation gate, so they are
    /// reproduced here rather than mocked away. Returns the new generation.
    fn fake_install(mgr: &mut ConnectionManager, name: &str) -> u64 {
        mgr.writers.insert(
            ConnId::Remote(name.to_string()),
            Box::new(tokio::io::sink()),
        );
        let entry = mgr.remotes.get_mut(name).expect("a known remote");
        entry.state = RemoteState::Connected;
        entry.generation += 1;
        entry.generation
    }

    /// The stale-`Closed` race: `d`, `y`, Enter with no pause reconnects the remote
    /// while the OLD reader's EOF is still in flight. That `Closed` carries the
    /// generation it was spawned for, so it is recognised as superseded.
    ///
    /// This pins the GATE. That the gate is consulted before the cleanup runs is
    /// pinned by `tests/pty/session_manager_disconnect.py` case 8, which sends the
    /// three keys with nothing between them against the real client.
    #[test]
    fn a_closed_from_a_superseded_transport_is_not_current() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        let remote = ConnId::Remote("alpha".to_string());

        let first = fake_install(&mut mgr, "alpha");
        assert!(mgr.is_current_generation(&remote, first));

        mgr.disconnect_remote("alpha");
        // Disconnecting does not advance the generation: the reader that is about
        // to report EOF is still the current one, so its `Closed` must be acted on
        // (that is what lets `mark_dropped` see `Disconnected` and leave it alone).
        assert_eq!(mgr.current_generation(&remote), first);
        assert!(mgr.is_current_generation(&remote, first));

        let second = fake_install(&mut mgr, "alpha");
        assert_eq!(second, first + 1);
        assert!(
            !mgr.is_current_generation(&remote, first),
            "the old reader's Closed would tear down the reconnected transport"
        );
        assert!(mgr.is_current_generation(&remote, second));
        // What the gate protects: the entry is live again.
        assert_eq!(mgr.remote_state("alpha"), RemoteState::Connected);
        assert_eq!(mgr.connected_ids(), vec![remote]);
    }

    /// Local is always current: it is never reinstalled (losing it exits the
    /// client), so there is no second transport for a stale `Closed` to belong to.
    /// An unknown remote is current too -- its `Closed` has nothing live to damage.
    #[test]
    fn local_and_unknown_connections_are_always_current() {
        let mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        assert!(mgr.is_current_generation(&ConnId::Local, LOCAL_GENERATION));
        assert!(mgr.is_current_generation(&ConnId::Local, 99));
        assert!(mgr.is_current_generation(&ConnId::Remote("ghost".into()), 7));
    }

    /// Disconnecting mid-dial: the entry is live enough to end (an ssh child is
    /// running), and the dial that lands afterwards must not install itself.
    #[test]
    fn disconnect_remote_cancels_an_in_flight_dial() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        mgr.set_state("alpha", RemoteState::Connecting);

        mgr.disconnect_remote("alpha");
        assert_eq!(mgr.remote_state("alpha"), RemoteState::Disconnected);

        // The late dial's FAILURE must not overwrite the user's choice with
        // `Failed` either -- the state is the record of what they asked for, and
        // an error about a connection they cancelled is noise.
        assert!(!mgr.finish_remote_dial("alpha", Err("connection timed out".to_string())));
        assert_eq!(mgr.remote_state("alpha"), RemoteState::Disconnected);
    }

    /// The View subscribe pass runs on every layout/focus change, so the lazy
    /// dial must refuse a user-disconnected remote structurally -- not because
    /// some caller remembered to check.
    #[test]
    fn begin_connect_remote_refuses_a_disconnected_remote() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        mgr.set_state("alpha", RemoteState::Connected);
        mgr.disconnect_remote("alpha");

        assert!(!mgr.begin_connect_remote("alpha"));
        assert_eq!(mgr.remote_state("alpha"), RemoteState::Disconnected);
    }

    #[test]
    fn foreground_helpers() {
        let mut mgr = ConnectionManager::empty(&sample_remotes(), ConnId::Local);
        assert!(mgr.is_foreground(&ConnId::Local));
        assert_eq!(mgr.foreground(), &ConnId::Local);

        let remote = ConnId::Remote("zulu".to_string());
        mgr.set_foreground(remote.clone());
        assert!(mgr.is_foreground(&remote));
        assert!(!mgr.is_foreground(&ConnId::Local));
    }
}
