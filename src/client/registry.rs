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
}

/// A message routed in from one of the connections' reader tasks.
pub enum Incoming {
    /// A decoded server message from the given connection.
    Message(ConnId, ServerMessage),
    /// The given connection's reader hit EOF or an error.
    Closed(ConnId),
}

// ---------------------------------------------------------------------------
// RemoteEntry
// ---------------------------------------------------------------------------

/// Per-remote bookkeeping held by the [`ConnectionManager`].
struct RemoteEntry {
    config: RemoteConfig,
    state: RemoteState,
    /// Keeps the ssh child alive; dropping it triggers `kill_on_drop`.
    _child: Option<Child>,
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
}

impl ConnectionManager {
    /// Build a manager around a connected local client, seeding the remotes map
    /// (all `NotConnected`) from config.
    pub fn new(local: RemuxClient, remotes: &HashMap<String, RemoteConfig>) -> Self {
        let mut mgr = Self::empty(remotes, ConnId::Local);
        let (reader, writer, _child) = local.into_split();
        mgr.writers.insert(ConnId::Local, writer);
        mgr.spawn_reader(ConnId::Local, reader);
        mgr
    }

    /// Build a manager whose foreground is a synthetic remote id, used by the
    /// direct `attach-remote` CLI flow (no `[remotes]` map involved).
    pub fn new_foreground_remote(name: &str, client: RemuxClient) -> Self {
        let id = ConnId::Remote(name.to_string());
        let mut mgr = Self::empty(&HashMap::new(), id.clone());
        // Record the remote as already Connected so the roster reflects it.
        mgr.remotes.insert(
            name.to_string(),
            RemoteEntry {
                config: RemoteConfig::default(),
                state: RemoteState::Connected,
                _child: None,
            },
        );
        let (reader, writer, child) = client.into_split();
        mgr.writers.insert(id.clone(), writer);
        if let Some(entry) = mgr.remotes.get_mut(name) {
            entry._child = child;
        }
        mgr.spawn_reader(id, reader);
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
                        _child: None,
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
        }
    }

    /// Spawn a reader task that pumps decoded messages from `reader` into the
    /// shared channel as [`Incoming`] values, emitting `Closed` on EOF/error.
    fn spawn_reader(&self, id: ConnId, mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>) {
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
                        let _ = tx.send(Incoming::Closed(id.clone()));
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
            _child: None,
        });
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
                let id = ConnId::Remote(name.to_string());
                let (reader, writer, child) = client.into_split();
                self.writers.insert(id.clone(), writer);
                if let Some(entry) = self.remotes.get_mut(name) {
                    entry._child = child;
                    entry.state = RemoteState::Connected;
                }
                self.spawn_reader(id, reader);
                log::info!("registry: remote '{name}' connected");
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

    /// Set a remote's state (internal).
    fn set_state(&mut self, name: &str, state: RemoteState) {
        if let Some(entry) = self.remotes.get_mut(name) {
            entry.state = state;
        }
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

    /// Ordered roster of servers for the session-manager tree: Local first
    /// (labelled `"local"`), then remotes sorted by name with their state.
    pub fn server_roster(&self) -> Vec<(ConnId, String, RemoteState)> {
        let mut roster = vec![(ConnId::Local, "local".to_string(), RemoteState::Connected)];
        let mut names: Vec<&String> = self.remotes.keys().collect();
        names.sort();
        for name in names {
            let entry = &self.remotes[name];
            roster.push((
                ConnId::Remote(name.clone()),
                name.clone(),
                entry.state.clone(),
            ));
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
