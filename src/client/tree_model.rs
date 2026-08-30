//! The session tree model.
//!
//! The flattened node list, expansion state, selection, and the search filter
//! that the session-manager overlay and the sidebar's session-tree panel both
//! read from. Extracted from `session_manager.rs` so the two surfaces share one
//! implementation of *what the tree is* while each keeps its own rendering
//! (their chrome and their widths differ) and its own key handling.

use std::collections::{HashMap, HashSet};

use crate::client::registry::{ConnId, RemoteState};
use crate::protocol::{FolderTreeEntry, PaneId, SessionTreeEntry};

// ---------------------------------------------------------------------------
// NodeType / TreeRow
// ---------------------------------------------------------------------------

/// The type of a node in the flattened tree view.
///
/// The tree is now two-level at the top: a `Server` node per connection, whose
/// folders/sessions/tabs/panes nest beneath it. Every non-server node carries
/// the `ConnId` of the server it belongs to so actions can be routed and
/// remote-only guards applied.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeType {
    Server {
        id: ConnId,
        state: RemoteState,
    },
    Folder {
        server: ConnId,
        name: String,
    },
    Session {
        server: ConnId,
        name: String,
    },
    Tab {
        server: ConnId,
        session: String,
        tab_index: usize,
    },
    Pane {
        server: ConnId,
        session: String,
        tab_index: usize,
        pane_id: u64,
    },
    /// Header row for the "Saved (resurrect)" group of dormant sessions. Local
    /// server only for now.
    SavedGroup {
        server: ConnId,
    },
    /// A dormant (saved-but-not-live) session that can be resurrected. Pressing
    /// Enter on it materializes the session on the server.
    DormantSession {
        server: ConnId,
        name: String,
    },
}

impl NodeType {
    /// The connection this node belongs to.
    pub fn server(&self) -> ConnId {
        match self {
            NodeType::Server { id, .. } => id.clone(),
            NodeType::Folder { server, .. }
            | NodeType::Session { server, .. }
            | NodeType::Tab { server, .. }
            | NodeType::Pane { server, .. }
            | NodeType::SavedGroup { server }
            | NodeType::DormantSession { server, .. } => server.clone(),
        }
    }
}

/// Where activating a tree row goes.
///
/// A node identity, not a resolved pane: the three levels are exactly the three
/// the server can already be told to go to (`Attach`, `SessionSwitchTab`,
/// `SessionSwitchPane`), so "that node's current focus" stays the server's
/// answer rather than a client-side guess at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpTarget {
    Session {
        conn: ConnId,
        session: String,
    },
    Tab {
        conn: ConnId,
        session: String,
        tab_index: usize,
    },
    Pane {
        conn: ConnId,
        session: String,
        tab_index: usize,
        pane_id: PaneId,
    },
}

impl JumpTarget {
    /// The connection this target lives on.
    pub fn conn(&self) -> &ConnId {
        match self {
            JumpTarget::Session { conn, .. }
            | JumpTarget::Tab { conn, .. }
            | JumpTarget::Pane { conn, .. } => conn,
        }
    }

    /// The session this target lives in.
    pub fn session(&self) -> &str {
        match self {
            JumpTarget::Session { session, .. }
            | JumpTarget::Tab { session, .. }
            | JumpTarget::Pane { session, .. } => session,
        }
    }
}

/// A single row in the flattened session manager tree.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub indent: usize,
    pub node_type: NodeType,
    pub display_name: String,
    pub is_expanded: bool,
    pub is_current: bool,
}
/// A per-rebuild view of what the current query allows to render.
///
/// Computed fresh on every [`TreeModel::rebuild_rows`] and thrown
/// away afterwards. `force_expand` deliberately does NOT touch
/// `TreeModel::expanded`: auto-expanding a match path must not
/// permanently rewrite the user's expansion state, or clearing the query would
/// leave the tree splayed open.
#[derive(Debug, Default)]
struct Filter {
    /// Keys of the nodes the query permits to render.
    visible: HashSet<String>,
    /// Keys of the nodes that must be treated as expanded so matches nested
    /// beneath them are reachable. Only ever the *ancestors* of a match -- a
    /// matching node's own subtree stays governed by `expanded`.
    force_expand: HashSet<String>,
    /// Keys of the nodes whose OWN name matched the query -- the *direct hits*,
    /// as opposed to the rows that only render because an ancestor or a
    /// descendant matched. This is what the selection must land on: the first
    /// visible row is typically an ancestor (a server, then a folder), and
    /// activating an ancestor is not what the user typed.
    hits: HashSet<String>,
}

/// Whether `filter` permits the node with `key` to render. No filter (an empty
/// query) permits everything.
fn filter_allows(filter: Option<&Filter>, key: &str) -> bool {
    filter.is_none_or(|f| f.visible.contains(key))
}

/// Expansion key for a server node.
fn server_key(id: &ConnId) -> String {
    format!("server:{}", id.key())
}

/// Expansion key for a folder node (namespaced by server).
fn folder_key(server: &ConnId, name: &str) -> String {
    format!("folder:{}:{}", server.key(), name)
}

/// Expansion key for a session node (namespaced by server).
fn session_key(server: &ConnId, name: &str) -> String {
    format!("session:{}:{}", server.key(), name)
}

/// Expansion key for a tab node (namespaced by server).
fn tab_key(server: &ConnId, session: &str, tab_index: usize) -> String {
    format!("tab:{}:{}:{}", server.key(), session, tab_index)
}

/// Expansion key for the "Saved (resurrect)" group node (namespaced by server).
fn saved_key(server: &ConnId) -> String {
    format!("saved:{}", server.key())
}

/// Filter key for a pane node. Panes never expand, so this key only ever
/// appears in [`Filter::visible`].
fn pane_key(server: &ConnId, session: &str, pane_id: u64) -> String {
    format!("pane:{}:{}:{}", server.key(), session, pane_id)
}

/// Filter key for a dormant (saved) session row. Dormant sessions never expand,
/// so this key only ever appears in [`Filter::visible`].
fn dormant_key(server: &ConnId, name: &str) -> String {
    format!("dormant:{}:{}", server.key(), name)
}

// ---------------------------------------------------------------------------
// ConnTrees / TreeModel
// ---------------------------------------------------------------------------

/// One connection's slice of the tree, mirroring the three fields of the
/// `SessionTree` server message.
#[derive(Debug, Clone, Default)]
pub struct ConnTrees {
    pub folders: Vec<FolderTreeEntry>,
    pub unfiled: Vec<SessionTreeEntry>,
    pub dormant: Vec<String>,
}

/// The tree itself: the raw per-server data, what is expanded, what is
/// selected, and the flattened rows those three imply.
///
/// A consumer feeds it a roster ([`TreeModel::set_roster`]) and per-connection
/// data ([`TreeModel::update_tree`] or [`TreeModel::rebuild`]), then reads
/// [`TreeModel::rows`] and [`TreeModel::selected`] to render. Expansion state
/// and the selection index survive a rebuild with unchanged data -- the sidebar
/// panel refreshes on every server push, so a model that reset either would
/// collapse under the user whenever anything changed anywhere.
#[derive(Debug, Clone)]
pub struct TreeModel {
    /// Flattened tree rows currently displayed.
    pub rows: Vec<TreeRow>,
    /// Index of the selected row.
    pub selected: usize,
    /// Set of expanded node keys (namespaced by server, e.g.
    /// "server:local", "folder:local:work", "session:remote:pi:proj").
    pub expanded: HashSet<String>,
    /// The search query. Filters the tree to matching nodes plus their ancestor
    /// chain (see [`TreeModel::compute_filter`]). Empty means "no filtering" --
    /// the tree flattens exactly as it did before the search bar existed.
    ///
    /// Deliberately a plain field rather than a session-manager sub-mode: that
    /// overlay's `sub_mode` holds a single value, and the query must survive
    /// while a delete/create/move/rename sub-mode is active (otherwise deleting
    /// a filtered session would wipe the query out from under the user).
    pub query: String,
    /// The foreground connection -- a session row is "current" only when it is
    /// the attached session of the foreground server.
    foreground: ConnId,
    /// Ordered server roster: `(id, label, state, version_mismatch)`. The last
    /// element is `Some(server_version)` when the server is outdated relative to
    /// this client (drives the "outdated" suffix), else `None`.
    roster: Vec<(ConnId, String, RemoteState, Option<String>)>,
    /// Per-server raw tree data: `(folders, unfiled)`.
    trees: HashMap<ConnId, (Vec<FolderTreeEntry>, Vec<SessionTreeEntry>)>,
    /// Every folder/session key this model has EVER been shown, per server.
    /// Drives the auto-expand-what-is-new rule in [`TreeModel::update_tree`];
    /// cumulative on purpose, so a tree that empties and refills does not read
    /// as a first load. Never shrinks: forgetting a key would re-open a node
    /// the user collapsed the moment it reappeared.
    seen_keys: HashMap<ConnId, HashSet<String>>,
    /// Names of the Local server's dormant (saved-but-not-live) sessions,
    /// rendered as a "Saved (resurrect)" group. Dormant sessions are a
    /// Local-server concept for now.
    dormant: Vec<String>,
    /// Direct hits from the most recent [`TreeModel::rebuild_rows`] (see
    /// [`Filter::hits`]); empty when no query is active. Refreshed on every
    /// rebuild and read only by [`TreeModel::on_query_changed`] to place the
    /// selection -- it is derived state, never user-visible, and must not leak
    /// into `expanded`.
    filter_hits: HashSet<String>,
}

impl Default for TreeModel {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeModel {
    /// A model holding just the local server node, expanded so local sessions
    /// show immediately.
    pub fn new() -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(server_key(&ConnId::Local));
        // Expand the Saved group by default so dormant sessions are discoverable.
        expanded.insert(saved_key(&ConnId::Local));
        Self {
            rows: Vec::new(),
            selected: 0,
            expanded,
            query: String::new(),
            foreground: ConnId::Local,
            roster: vec![(
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            )],
            trees: HashMap::new(),
            seen_keys: HashMap::new(),
            dormant: Vec::new(),
            filter_hits: HashSet::new(),
        }
    }

    /// Feed every connection's slice at once. Routed through
    /// [`TreeModel::update_tree`] so the auto-expand-new-entries and
    /// first-load semantics are one implementation, not two.
    ///
    /// Server rows come from the roster, not from this data: a consumer that
    /// shows remotes must also call [`TreeModel::set_roster`].
    pub fn rebuild(&mut self, per_conn: &[(ConnId, ConnTrees)]) {
        for (conn, trees) in per_conn {
            self.update_tree(
                conn.clone(),
                trees.folders.clone(),
                trees.unfiled.clone(),
                trees.dormant.clone(),
            );
        }
    }

    /// The selected row, if any.
    pub fn selected_row(&self) -> Option<&TreeRow> {
        self.rows.get(self.selected)
    }

    /// Which node activating the selected row should go to.
    ///
    /// Names the *node*; it does not resolve a session or a tab down to some
    /// pane. That resolution is the server's -- `Attach` lands on a session's
    /// current tab and pane, `SessionSwitchTab` on a tab's focused pane -- so a
    /// client-side second implementation of "which pane is current" would only
    /// be able to disagree with it.
    ///
    /// Server, folder and saved-group rows return `None`: they expand, they do
    /// not jump. So does a dormant session, which has to be resurrected before
    /// there is anything to jump to.
    pub fn jump_target(&self) -> Option<JumpTarget> {
        match &self.selected_row()?.node_type {
            NodeType::Session { server, name } => Some(JumpTarget::Session {
                conn: server.clone(),
                session: name.clone(),
            }),
            NodeType::Tab {
                server,
                session,
                tab_index,
            } => Some(JumpTarget::Tab {
                conn: server.clone(),
                session: session.clone(),
                tab_index: *tab_index,
            }),
            NodeType::Pane {
                server,
                session,
                tab_index,
                pane_id,
            } => Some(JumpTarget::Pane {
                conn: server.clone(),
                session: session.clone(),
                tab_index: *tab_index,
                pane_id: *pane_id,
            }),
            NodeType::Server { .. }
            | NodeType::Folder { .. }
            | NodeType::SavedGroup { .. }
            | NodeType::DormantSession { .. } => None,
        }
    }

    /// Force a server node open (used when a lazy connect is kicked off, so its
    /// children appear as soon as the tree arrives).
    pub fn force_expand_server(&mut self, id: &ConnId) {
        self.expanded.insert(server_key(id));
        self.rebuild_rows();
    }

    /// Whether `server` is present in the roster and currently connected.
    pub fn is_connected(&self, server: &ConnId) -> bool {
        self.roster
            .iter()
            .any(|(id, _, state, _)| id == server && matches!(state, RemoteState::Connected))
    }

    /// A given server's folder names.
    pub fn folder_names_for(&self, server: &ConnId) -> Vec<String> {
        self.trees
            .get(server)
            .map(|(folders, _)| folders.iter().map(|f| f.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Move the selection down one row (alias of [`TreeModel::select_next`]).
    pub fn move_down(&mut self) {
        self.select_next();
    }

    /// Move the selection up one row (alias of [`TreeModel::select_prev`]).
    pub fn move_up(&mut self) {
        self.select_prev();
    }

    /// Append a char to the search query and refilter.
    pub fn push_query_char(&mut self, c: char) {
        self.query.push(c);
        self.on_query_changed();
    }

    /// Remove the last char of the search query and refilter.
    pub fn pop_query_char(&mut self) {
        self.query.pop();
        self.on_query_changed();
    }

    /// Clear the search query and refilter (Ctrl-U).
    pub fn clear_query(&mut self) {
        if self.query.is_empty() {
            return;
        }
        self.query.clear();
        self.on_query_changed();
    }

    /// Re-filter after the query changed. The previous selection index is
    /// meaningless against a new result set, so it is re-placed against the
    /// rows the filter just produced.
    ///
    /// The filtered tree shows a match's whole ancestor chain, so the topmost
    /// rows are ancestors: always a `Server` row (servers render regardless of
    /// the query), then usually the enclosing folder. Activating an ancestor
    /// only toggles its expansion -- not what the user typed. The selection
    /// therefore lands on the first *direct hit* (a row whose own name matched,
    /// per [`Filter::hits`]).
    ///
    /// Two fallbacks, for rows that are visible without being hits: the first
    /// non-server row, then the top. An empty query means no filtering at all,
    /// so it keeps the plain "back to the top" behaviour.
    fn on_query_changed(&mut self) {
        self.rebuild_rows();
        let next = if self.query.is_empty() {
            0
        } else {
            self.rows
                .iter()
                .position(|r| self.filter_hits.contains(&self.row_key(r)))
                .or_else(|| {
                    self.rows
                        .iter()
                        .position(|r| !matches!(r.node_type, NodeType::Server { .. }))
                })
                .unwrap_or(0)
        };
        self.selected = next;
    }

    /// Set the foreground connection (drives which server's sessions render as
    /// "current"). Does not rebuild rows on its own; callers pair this with
    /// `set_roster`/`update_tree`.
    pub fn set_foreground(&mut self, foreground: ConnId) {
        self.foreground = foreground;
    }

    /// Replace the server roster (order + labels + states) and rebuild rows.
    pub fn set_roster(&mut self, roster: Vec<(ConnId, String, RemoteState, Option<String>)>) {
        // Ensure Local is always expanded by default the first time we see it.
        for (id, _, _, _) in &roster {
            if matches!(id, ConnId::Local) {
                self.expanded.insert(server_key(id));
            }
            self.trees.entry(id.clone()).or_default();
        }
        self.roster = roster;
        self.rebuild_rows();
    }

    /// Update a single server's slice of the tree and rebuild rows.
    pub fn update_tree(
        &mut self,
        server: ConnId,
        folders: Vec<FolderTreeEntry>,
        unfiled: Vec<SessionTreeEntry>,
        dormant: Vec<String>,
    ) {
        log::debug!(
            "tree_model: update_tree server={:?} folders={} unfiled={} dormant={}",
            server,
            folders.len(),
            unfiled.len(),
            dormant.len()
        );
        // Dormant sessions are a Local-server concept for now.
        if server == ConnId::Local {
            self.dormant = dormant;
        }
        // Auto-expand only entries this model has NEVER seen on this server.
        //
        // The memory is cumulative (`seen_keys`) rather than derived from the
        // previously stored tree: a tree that goes empty and comes back --
        // a remote whose last session closes, a server that connects with
        // nothing running -- would otherwise look brand new all over again and
        // re-open every folder the user had collapsed. The sidebar panel
        // refreshes on every server push, so that boundary is reachable there
        // in a way it never really was in an overlay opened on demand.
        //
        // The deliberate cost: a session deleted and recreated under the same
        // name is not treated as new, so it does not auto-expand.
        let known_keys = self.seen_keys.entry(server.clone()).or_default();
        let expanded = &mut self.expanded;
        let mut first_sighting = |key: String| {
            if known_keys.insert(key.clone()) {
                expanded.insert(key);
            }
        };

        for f in &folders {
            first_sighting(folder_key(&server, &f.name));
            for s in &f.sessions {
                first_sighting(session_key(&server, &s.name));
            }
        }
        for s in &unfiled {
            first_sighting(session_key(&server, &s.name));
        }

        self.trees.insert(server, (folders, unfiled));
        self.rebuild_rows();
    }

    /// Compute the visibility / auto-expansion sets implied by the current
    /// query, or `None` when the query is empty (no filtering at all).
    ///
    /// A node is visible when it matches, when any descendant matches, or when
    /// any ancestor matches -- so a hit on a tab name drags its session and
    /// folder into view, and a hit on a session name keeps that session's tabs
    /// and panes browsable. Matching is a case-insensitive substring test
    /// against the *entity* name, not the decorated `display_name` (which
    /// carries client-count suffixes, the dormant `💤` prefix and the focused
    /// pane's `*`).
    ///
    /// Server rows are always visible: the roster is tiny, and hiding an
    /// offline remote would remove the only affordance to connect it.
    fn compute_filter(&self) -> Option<Filter> {
        if self.query.is_empty() {
            return None;
        }
        let needle = self.query.to_lowercase();
        let hit = |name: &str| name.to_lowercase().contains(&needle);

        let mut f = Filter::default();
        for (id, _, _, _) in &self.roster {
            let skey = server_key(id);
            f.visible.insert(skey.clone());

            if let Some((folders, unfiled)) = self.trees.get(id) {
                // Unfiled sessions hang directly off the server node.
                for session in unfiled {
                    if self.filter_session(&mut f, id, session, false, &hit) {
                        f.force_expand.insert(skey.clone());
                    }
                }
                for folder in folders {
                    let fkey = folder_key(id, &folder.name);
                    let folder_hit = hit(&folder.name);
                    let mut child_hit = false;
                    for session in &folder.sessions {
                        if self.filter_session(&mut f, id, session, folder_hit, &hit) {
                            child_hit = true;
                        }
                    }
                    if folder_hit {
                        f.hits.insert(fkey.clone());
                    }
                    if folder_hit || child_hit {
                        f.visible.insert(fkey.clone());
                        f.force_expand.insert(skey.clone());
                    }
                    if child_hit {
                        f.force_expand.insert(fkey);
                    }
                }
            }

            // The "Saved (resurrect)" group is Local-only; it shows when any
            // dormant session name matches.
            if *id == ConnId::Local {
                let mut any_dormant = false;
                for name in &self.dormant {
                    if hit(name) {
                        let dkey = dormant_key(id, name);
                        f.visible.insert(dkey.clone());
                        f.hits.insert(dkey);
                        any_dormant = true;
                    }
                }
                if any_dormant {
                    f.visible.insert(saved_key(id));
                    f.force_expand.insert(saved_key(id));
                    f.force_expand.insert(skey);
                }
            }
        }
        Some(f)
    }

    /// Filter one session subtree into `f`. `ancestor_hit` is true when an
    /// enclosing folder already matched (which makes the whole subtree visible).
    /// Returns whether this session or anything beneath it matched -- the caller
    /// uses that to decide whether to force-expand the session's parent.
    fn filter_session(
        &self,
        f: &mut Filter,
        server: &ConnId,
        session: &SessionTreeEntry,
        ancestor_hit: bool,
        hit: &impl Fn(&str) -> bool,
    ) -> bool {
        let skey = session_key(server, &session.name);
        let session_hit = hit(&session.name);
        let mut child_hit = false;

        for (tab_idx, tab) in session.tabs.iter().enumerate() {
            let tkey = tab_key(server, &session.name, tab_idx);
            let tab_hit = hit(&tab.name);
            let mut pane_hit = false;
            for pane in &tab.panes {
                let this_pane_hit = hit(&pane.name);
                pane_hit |= this_pane_hit;
                // A pane renders when it matches or when anything above it did.
                if this_pane_hit || tab_hit || session_hit || ancestor_hit {
                    let pkey = pane_key(server, &session.name, pane.id);
                    if this_pane_hit {
                        f.hits.insert(pkey.clone());
                    }
                    f.visible.insert(pkey);
                }
            }
            if tab_hit {
                f.hits.insert(tkey.clone());
            }
            if tab_hit || pane_hit || session_hit || ancestor_hit {
                f.visible.insert(tkey.clone());
            }
            if pane_hit {
                // Only a match *below* the tab forces it open; a tab that
                // merely matched keeps its own panes under `expanded`.
                f.force_expand.insert(tkey);
                child_hit = true;
            }
            if tab_hit {
                child_hit = true;
            }
        }

        if session_hit {
            f.hits.insert(skey.clone());
        }
        if session_hit || child_hit || ancestor_hit {
            f.visible.insert(skey.clone());
        }
        if child_hit {
            f.force_expand.insert(skey);
        }
        session_hit || child_hit
    }

    /// Whether `key` renders as expanded: either the user expanded it, or the
    /// active filter is holding it open so a match beneath it is reachable.
    fn is_expanded(&self, key: &str, filter: Option<&Filter>) -> bool {
        self.expanded.contains(key) || filter.is_some_and(|f| f.force_expand.contains(key))
    }

    /// Rebuild the flat row list from the roster + per-server tree data.
    fn rebuild_rows(&mut self) {
        // What the selection NAMES, before the rows it indexes into are thrown
        // away. See the re-resolve at the end of this function.
        let previously_selected = self.selected_row().map(|r| self.row_key(r));
        let mut rows = Vec::new();
        let filter = self.compute_filter();
        let filter = filter.as_ref();

        for (id, label, state, mismatch) in &self.roster {
            let skey = server_key(id);
            let server_expanded = self.is_expanded(&skey, filter);
            let connected = matches!(state, RemoteState::Connected);
            let mut suffix = match state {
                RemoteState::Connected => String::new(),
                RemoteState::NotConnected => " (offline)".to_string(),
                RemoteState::Connecting => " (connecting…)".to_string(),
                RemoteState::Failed(msg) => format!(" (failed: {msg})"),
            };
            // A connected server built from different source than this client
            // is flagged so the user knows before silently hitting version skew.
            //
            // NOT "outdated: restart remux server". Nothing here knows WHICH
            // side is behind -- a stale daemon that has not been restarted since
            // a rebuild, and a client older than the machine it dialled, produce
            // the identical mismatch -- and "restart the server" is a lie in the
            // second case AND in the case this comparison used to invent, where
            // the server was already running the right build. Both halves must
            // be true whichever way round it is: to match, the two must be built
            // from the same commit (rebuild), and a rebuilt server binary only
            // takes effect once the running process is replaced (restart).
            if mismatch.is_some() {
                suffix.push_str(" (build mismatch: rebuild + restart)");
            }
            rows.push(TreeRow {
                indent: 0,
                node_type: NodeType::Server {
                    id: id.clone(),
                    state: state.clone(),
                },
                display_name: format!("{label}{suffix}"),
                is_expanded: server_expanded,
                is_current: false,
            });

            if server_expanded && connected {
                if let Some((folders, unfiled)) = self.trees.get(id) {
                    for folder in folders {
                        let fkey = folder_key(id, &folder.name);
                        if !filter_allows(filter, &fkey) {
                            continue;
                        }
                        let folder_expanded = self.is_expanded(&fkey, filter);
                        rows.push(TreeRow {
                            indent: 1,
                            node_type: NodeType::Folder {
                                server: id.clone(),
                                name: folder.name.clone(),
                            },
                            display_name: folder.name.clone(),
                            is_expanded: folder_expanded,
                            is_current: false,
                        });

                        if folder_expanded {
                            for session in &folder.sessions {
                                self.add_session_rows(&mut rows, id, session, 2, filter);
                            }
                        }
                    }

                    for session in unfiled {
                        self.add_session_rows(&mut rows, id, session, 1, filter);
                    }
                }

                // Render the "Saved (resurrect)" group at the bottom of the
                // Local server's children. Dormant sessions are Local-only.
                let gkey = saved_key(id);
                if *id == ConnId::Local && !self.dormant.is_empty() && filter_allows(filter, &gkey)
                {
                    let group_expanded = self.is_expanded(&gkey, filter);
                    rows.push(TreeRow {
                        indent: 1,
                        node_type: NodeType::SavedGroup { server: id.clone() },
                        display_name: "Saved (resurrect)".to_string(),
                        is_expanded: group_expanded,
                        is_current: false,
                    });
                    if group_expanded {
                        for name in &self.dormant {
                            if !filter_allows(filter, &dormant_key(id, name)) {
                                continue;
                            }
                            rows.push(TreeRow {
                                indent: 2,
                                node_type: NodeType::DormantSession {
                                    server: id.clone(),
                                    name: name.clone(),
                                },
                                display_name: format!("\u{1F4A4} {}", name),
                                is_expanded: false,
                                is_current: false,
                            });
                        }
                    }
                }
            }
        }

        self.rows = rows;
        // Unconditionally refreshed: `rebuild_rows` also runs for expand /
        // collapse / tree updates, and leaving a previous query's hits behind
        // would let a stale set outlive the query that produced it.
        self.filter_hits = filter.map(|f| f.hits.clone()).unwrap_or_default();

        // Re-resolve the selection by IDENTITY, not by index.
        //
        // Expansion survives a rebuild by key and the manager's marks survive by
        // identity; the selection used to survive only as an index, which is not
        // the same thing once the rows above it can change. Deleting a session
        // above the selection shifts every row up and the highlight silently
        // lands on a different node -- and the sidebar panel rebuilds on EVERY
        // server push, so that is a wrong-jump bug there rather than a cosmetic
        // one (Enter is its primary action).
        //
        // A node that is genuinely gone falls back to the clamp: the index stays
        // put, which is the usual "the cursor does not move when the thing under
        // it is deleted" behaviour.
        //
        // Caveat: a tab's identity is its INDEX within its session
        // (`NodeType::Tab` carries `tab_index`; the wire's tab ids never reach
        // the model), so reordering tabs still retargets a tab-row selection.
        if let Some(key) = previously_selected {
            if let Some(idx) = self.rows.iter().position(|r| self.row_key(r) == key) {
                self.selected = idx;
            }
        }
        if !self.rows.is_empty() && self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
    }

    fn add_session_rows(
        &self,
        rows: &mut Vec<TreeRow>,
        server: &ConnId,
        session: &SessionTreeEntry,
        indent: usize,
        filter: Option<&Filter>,
    ) {
        let skey = session_key(server, &session.name);
        if !filter_allows(filter, &skey) {
            return;
        }
        let session_expanded = self.is_expanded(&skey, filter);
        let client_suffix = if session.client_count > 0 {
            format!(" ({})", session.client_count)
        } else {
            String::new()
        };
        // "Current" only for the foreground server's attached session.
        let is_current = server == &self.foreground && session.is_current;
        rows.push(TreeRow {
            indent,
            node_type: NodeType::Session {
                server: server.clone(),
                name: session.name.clone(),
            },
            display_name: format!("{}{}", session.name, client_suffix),
            is_expanded: session_expanded,
            is_current,
        });

        if session_expanded {
            for (tab_idx, tab) in session.tabs.iter().enumerate() {
                let tkey = tab_key(server, &session.name, tab_idx);
                if !filter_allows(filter, &tkey) {
                    continue;
                }
                let tab_expanded = self.is_expanded(&tkey, filter);
                rows.push(TreeRow {
                    indent: indent + 1,
                    node_type: NodeType::Tab {
                        server: server.clone(),
                        session: session.name.clone(),
                        tab_index: tab_idx,
                    },
                    display_name: tab.name.clone(),
                    is_expanded: tab_expanded,
                    is_current: false,
                });

                if tab_expanded {
                    for pane in &tab.panes {
                        if !filter_allows(filter, &pane_key(server, &session.name, pane.id)) {
                            continue;
                        }
                        let focus_marker = if pane.is_focused { "*" } else { "" };
                        rows.push(TreeRow {
                            indent: indent + 2,
                            node_type: NodeType::Pane {
                                server: server.clone(),
                                session: session.name.clone(),
                                tab_index: tab_idx,
                                pane_id: pane.id,
                            },
                            display_name: format!("{}{}", pane.name, focus_marker),
                            is_expanded: false,
                            is_current: false,
                        });
                    }
                }
            }
        }
    }

    /// Move selection down, wrapping to the top.
    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.rows.len();
        log::debug!("tree_model: select_next selected={}", self.selected);
    }

    /// Move selection up, wrapping to the bottom.
    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.rows.len() - 1;
        } else {
            self.selected -= 1;
        }
        log::debug!("tree_model: select_prev selected={}", self.selected);
    }

    /// Toggle the expand/collapse state of the selected node.
    pub fn toggle_expand(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            let key = self.node_key(&row.node_type);
            if self.expanded.contains(&key) {
                self.expanded.remove(&key);
            } else {
                self.expanded.insert(key);
            }
            self.rebuild_rows();
        }
    }

    /// Expand the selected node.
    pub fn expand_selected(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            let key = self.node_key(&row.node_type);
            if !self.expanded.contains(&key) {
                self.expanded.insert(key);
                self.rebuild_rows();
            }
        }
    }

    /// Collapse the selected node.
    pub fn collapse_selected(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            let key = self.node_key(&row.node_type);
            if self.expanded.contains(&key) {
                self.expanded.remove(&key);
                self.rebuild_rows();
            }
        }
    }

    pub fn node_key(&self, node_type: &NodeType) -> String {
        match node_type {
            NodeType::Server { id, .. } => server_key(id),
            NodeType::Folder { server, name } => folder_key(server, name),
            NodeType::Session { server, name } => session_key(server, name),
            NodeType::Tab {
                server,
                session,
                tab_index,
            } => tab_key(server, session, *tab_index),
            NodeType::SavedGroup { server } => saved_key(server),
            // Panes and dormant sessions don't expand.
            NodeType::Pane { .. } | NodeType::DormantSession { .. } => String::new(),
        }
    }

    /// The filter key of a row, for hit-testing against [`Filter::hits`].
    ///
    /// Distinct from [`TreeModel::node_key`], which deliberately
    /// returns an empty string for panes and dormant sessions because they have
    /// no expansion state. Search matching has no such exemption -- either can
    /// be the thing the user typed -- so those two get their real filter keys
    /// here and everything else defers to `node_key`.
    fn row_key(&self, row: &TreeRow) -> String {
        match &row.node_type {
            NodeType::Pane {
                server,
                session,
                pane_id,
                ..
            } => pane_key(server, session, *pane_id),
            NodeType::DormantSession { server, name } => dormant_key(server, name),
            other => self.node_key(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PaneTreeEntry, TabTreeEntry};

    fn remote(name: &str) -> ConnId {
        ConnId::Remote(name.to_string())
    }

    fn pane(id: u64, name: &str) -> PaneTreeEntry {
        PaneTreeEntry {
            cwd: None,
            id,
            name: name.to_string(),
            is_focused: false,
        }
    }

    fn tab(id: u64, name: &str, panes: Vec<PaneTreeEntry>) -> TabTreeEntry {
        TabTreeEntry {
            is_active: true,
            id,
            name: name.to_string(),
            panes,
        }
    }

    fn session(name: &str, tabs: Vec<TabTreeEntry>) -> SessionTreeEntry {
        SessionTreeEntry {
            name: name.to_string(),
            tabs,
            client_count: 0,
            is_current: false,
        }
    }

    /// Two connections: `local` has a folder `work` holding session `alpha`
    /// (one tab, two panes); `pi` has one unfiled session `beta` (one tab, one
    /// pane). Both servers are connected, so both subtrees render.
    fn two_conn_fixture() -> (TreeModel, Vec<(ConnId, ConnTrees)>) {
        let mut model = TreeModel::new();
        model.set_roster(vec![
            (
                ConnId::Local,
                "local".to_string(),
                RemoteState::Connected,
                None,
            ),
            (remote("pi"), "pi".to_string(), RemoteState::Connected, None),
        ]);
        let per_conn = vec![
            (
                ConnId::Local,
                ConnTrees {
                    folders: vec![FolderTreeEntry {
                        name: "work".to_string(),
                        sessions: vec![session(
                            "alpha",
                            vec![tab(1, "editor", vec![pane(10, "sh"), pane(11, "top")])],
                        )],
                    }],
                    unfiled: Vec::new(),
                    dormant: Vec::new(),
                },
            ),
            (
                remote("pi"),
                ConnTrees {
                    folders: Vec::new(),
                    unfiled: vec![session("beta", vec![tab(2, "shell", vec![pane(20, "sh")])])],
                    dormant: Vec::new(),
                },
            ),
        ];
        model.rebuild(&per_conn);
        (model, per_conn)
    }

    fn labels(model: &TreeModel) -> Vec<(usize, String)> {
        model
            .rows
            .iter()
            .map(|r| (r.indent, r.display_name.clone()))
            .collect()
    }

    fn row_of(model: &TreeModel, name: &str) -> usize {
        model
            .rows
            .iter()
            .position(|r| r.display_name == name)
            .unwrap_or_else(|| panic!("no row {name:?} in {:?}", labels(model)))
    }

    /// Expand the node whose row label is `name`.
    fn expand(model: &mut TreeModel, name: &str) {
        model.selected = row_of(model, name);
        model.expand_selected();
    }

    #[test]
    fn rebuild_flattens_two_connections() {
        let (model, _) = two_conn_fixture();
        // Servers at indent 0; the local folder at 1 and its session at 2, both
        // auto-expanded by the first load, with that session's tab at 3. Tabs
        // are NOT auto-expanded, so no pane rows; and only the local server
        // starts expanded, so the remote renders as a single collapsed row.
        assert_eq!(
            labels(&model),
            vec![
                (0, "local".to_string()),
                (1, "work".to_string()),
                (2, "alpha".to_string()),
                (3, "editor".to_string()),
                (0, "pi".to_string()),
            ]
        );
    }

    #[test]
    fn expanding_a_tab_and_a_remote_server_reveals_their_children() {
        let (mut model, _) = two_conn_fixture();
        expand(&mut model, "editor");
        expand(&mut model, "pi");
        expand(&mut model, "shell");
        assert_eq!(
            labels(&model),
            vec![
                (0, "local".to_string()),
                (1, "work".to_string()),
                (2, "alpha".to_string()),
                (3, "editor".to_string()),
                (4, "sh".to_string()),
                (4, "top".to_string()),
                (0, "pi".to_string()),
                (1, "beta".to_string()),
                (2, "shell".to_string()),
                (3, "sh".to_string()),
            ]
        );
    }

    #[test]
    fn rebuild_with_the_same_data_keeps_expansion_and_selection() {
        let (mut model, per_conn) = two_conn_fixture();
        // Open a tab (tabs are not auto-expanded, so this is purely the user's
        // doing), collapse the folder that holds it, open the remote server,
        // and park the selection on the remote's session.
        expand(&mut model, "editor");
        expand(&mut model, "pi");
        model.selected = row_of(&model, "work");
        model.toggle_expand();
        let selected_before = row_of(&model, "beta");
        model.selected = selected_before;
        let rows_before = labels(&model);

        // The sidebar panel refreshes on EVERY server push. A refresh carrying
        // unchanged data must not re-open what the user collapsed, nor move the
        // selection out from under them.
        model.rebuild(&per_conn);

        assert_eq!(labels(&model), rows_before, "refresh re-flattened the tree");
        assert_eq!(
            model.selected, selected_before,
            "refresh moved the selection"
        );
        assert!(
            !model.expanded.contains(&folder_key(&ConnId::Local, "work")),
            "refresh re-expanded a collapsed folder"
        );
        assert!(
            model
                .expanded
                .contains(&tab_key(&ConnId::Local, "alpha", 0)),
            "refresh collapsed a tab the user had opened"
        );
    }

    #[test]
    fn toggle_expand_round_trips_without_losing_the_selection() {
        let (mut model, _) = two_conn_fixture();
        let before = labels(&model);
        let folder = row_of(&model, "work");
        model.selected = folder;

        model.toggle_expand();
        assert_eq!(model.selected, folder, "collapse moved the selection");
        assert!(
            !model.rows.iter().any(|r| r.display_name == "alpha"),
            "collapse left the folder's children rendered"
        );

        model.toggle_expand();
        assert_eq!(model.selected, folder, "re-expand moved the selection");
        assert_eq!(labels(&model), before, "re-expand did not restore the tree");
    }

    #[test]
    fn move_down_and_move_up_wrap_at_the_ends() {
        // The brief specified clamping; the shipped session manager WRAPS
        // (`test_navigation_wraps`), and this model is the same code, so
        // wrapping is what it must do -- clamping here would silently change
        // the overlay.
        let (mut model, _) = two_conn_fixture();
        let last = model.rows.len() - 1;

        model.selected = last;
        model.move_down();
        assert_eq!(
            model.selected, 0,
            "moving down off the end must wrap to the top"
        );

        model.move_up();
        assert_eq!(
            model.selected, last,
            "moving up off the top must wrap to the end"
        );
    }

    #[test]
    fn move_down_and_move_up_are_noops_on_an_empty_tree() {
        let mut model = TreeModel::new();
        model.set_roster(Vec::new());
        assert!(model.rows.is_empty());
        model.move_down();
        model.move_up();
        assert_eq!(model.selected, 0);
    }

    #[test]
    fn jump_target_resolves_session_tab_and_pane_rows() {
        let (mut model, _) = two_conn_fixture();
        expand(&mut model, "editor");
        expand(&mut model, "pi");
        expand(&mut model, "shell");

        // Rows that expand rather than jump.
        model.selected = row_of(&model, "local");
        assert_eq!(
            model.jump_target(),
            None,
            "a server row is not a jump target"
        );
        model.selected = row_of(&model, "work");
        assert_eq!(
            model.jump_target(),
            None,
            "a folder row is not a jump target"
        );

        // The three levels the server can be told to go to.
        model.selected = row_of(&model, "alpha");
        assert_eq!(
            model.jump_target(),
            Some(JumpTarget::Session {
                conn: ConnId::Local,
                session: "alpha".to_string()
            }),
        );
        model.selected = row_of(&model, "editor");
        assert_eq!(
            model.jump_target(),
            Some(JumpTarget::Tab {
                conn: ConnId::Local,
                session: "alpha".to_string(),
                tab_index: 0
            }),
        );
        model.selected = row_of(&model, "top");
        assert_eq!(
            model.jump_target(),
            Some(JumpTarget::Pane {
                conn: ConnId::Local,
                session: "alpha".to_string(),
                tab_index: 0,
                pane_id: 11
            }),
            "a pane row names the pane, not merely its session"
        );

        // ... including a pane on a remote connection.
        let remote_pane = model
            .rows
            .iter()
            .position(|r| matches!(&r.node_type, NodeType::Pane { server, .. } if *server == remote("pi")))
            .unwrap();
        model.selected = remote_pane;
        assert_eq!(
            model.jump_target(),
            Some(JumpTarget::Pane {
                conn: remote("pi"),
                session: "beta".to_string(),
                tab_index: 0,
                pane_id: 20
            })
        );
    }

    #[test]
    fn a_dormant_session_row_is_not_a_jump_target() {
        // It has to be resurrected before there is anything to jump to, so the
        // panel must be able to tell it apart from a live session row.
        let mut model = TreeModel::new();
        model.set_roster(vec![(
            ConnId::Local,
            "local".to_string(),
            RemoteState::Connected,
            None,
        )]);
        model.update_tree(
            ConnId::Local,
            Vec::new(),
            Vec::new(),
            vec!["archived".to_string()],
        );
        model.selected = model
            .rows
            .iter()
            .position(|r| matches!(r.node_type, NodeType::DormantSession { .. }))
            .expect("a dormant row");
        assert_eq!(model.jump_target(), None);
    }

    #[test]
    fn a_tree_that_empties_and_refills_does_not_re_expand_a_collapsed_folder() {
        // The sidebar panel refreshes on EVERY server push, so "this server's
        // stored tree is empty" is a reachable state mid-session -- a remote
        // whose last session closes, say. If that read as a first load, every
        // folder the user collapsed would spring back open.
        let (mut model, per_conn) = two_conn_fixture();
        let local = per_conn[0].1.clone();

        model.selected = row_of(&model, "work");
        model.collapse_selected();
        assert!(!model.rows.iter().any(|r| r.display_name == "alpha"));

        // The server goes quiet, then comes back with exactly what it had.
        model.update_tree(ConnId::Local, Vec::new(), Vec::new(), Vec::new());
        model.update_tree(
            ConnId::Local,
            local.folders.clone(),
            local.unfiled.clone(),
            Vec::new(),
        );

        assert!(
            model.rows.iter().any(|r| r.display_name == "work"),
            "the folder itself must come back: {:?}",
            labels(&model)
        );
        assert!(
            !model.rows.iter().any(|r| r.display_name == "alpha"),
            "the collapsed folder re-opened itself: {:?}",
            labels(&model)
        );
    }

    #[test]
    fn a_genuinely_new_session_still_auto_expands() {
        // The other half of the rule: only NEVER-seen entries auto-expand, and
        // a brand new session is one.
        let (mut model, per_conn) = two_conn_fixture();
        let mut local = per_conn[0].1.clone();
        local
            .unfiled
            .push(session("gamma", vec![tab(9, "gtab", vec![pane(90, "sh")])]));
        model.update_tree(ConnId::Local, local.folders, local.unfiled, Vec::new());
        assert!(
            model.rows.iter().any(|r| r.display_name == "gtab"),
            "a new session did not auto-expand: {:?}",
            labels(&model)
        );
    }

    #[test]
    fn the_selection_follows_its_node_when_a_row_above_it_disappears() {
        // The panel rebuilds on EVERY server push, so a session deleted above
        // the selection shifts every row up. Preserved by index, the highlight
        // would silently land on a different node -- and Enter jumps.
        let mut model = TreeModel::new();
        model.set_roster(vec![(
            ConnId::Local,
            "local".to_string(),
            RemoteState::Connected,
            None,
        )]);
        let alpha = session("alpha", vec![tab(1, "atab", vec![pane(10, "sh")])]);
        let beta = session("beta", vec![tab(2, "btab", vec![pane(20, "sh")])]);
        model.update_tree(
            ConnId::Local,
            Vec::new(),
            vec![alpha, beta.clone()],
            Vec::new(),
        );

        model.selected = row_of(&model, "beta");
        let before = model.selected;

        // "alpha" (and its tab row) go away above the selection.
        model.update_tree(ConnId::Local, Vec::new(), vec![beta], Vec::new());

        assert_ne!(
            model.selected, before,
            "the fixture must actually shift the row, or this proves nothing"
        );
        assert_eq!(
            model.selected_row().map(|r| r.display_name.as_str()),
            Some("beta"),
            "the selection retargeted: {:?}",
            labels(&model)
        );
    }

    #[test]
    fn a_selection_whose_node_is_gone_falls_back_to_the_index() {
        // The usual "the cursor does not move when the thing under it is
        // deleted" behaviour, and the clamp still applies at the end.
        let (mut model, _) = two_conn_fixture();
        model.selected = row_of(&model, "alpha");
        let before = model.selected;
        model.update_tree(ConnId::Local, Vec::new(), Vec::new(), Vec::new());
        assert!(model.selected <= before);
        assert!(model.selected < model.rows.len());
    }

    #[test]
    fn jump_target_is_none_with_no_rows() {
        let mut model = TreeModel::new();
        model.set_roster(Vec::new());
        assert_eq!(model.jump_target(), None);
    }
}
