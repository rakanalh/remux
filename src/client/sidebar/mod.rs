//! Sidebar plugins: the panels that live inside the client's chrome.
//!
//! A plugin renders a `RenderCell` grid -- the same primitive the server
//! compositor produces -- rather than `DrawCommand`s, because panels are
//! written INTO the renderer's front buffer rather than painted over it. See
//! the design doc, section 6.

use crossterm::event::{KeyEvent, MouseEventKind};
use unicode_width::UnicodeWidthChar;

use crate::client::registry::ConnId;
use crate::client::tree_model::JumpTarget;
use crate::client::view::PaneSnapshot;
use crate::config::sidebar::PanelConfig;
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, RenderCell};
use crate::server::layout::FocusDirection;

pub mod agents;
pub mod files;
pub mod nav;
pub mod placeholder;
pub mod sessions;

/// What the client should do after handing an event to a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginAction {
    /// Nothing happened; do not repaint.
    None,
    /// Internal state changed; repaint this panel.
    Redraw,
    /// Move focus out of the sidebar in this direction.
    LeaveTo(FocusDirection),
    /// Go to a session, a tab, or a pane, on any connected server. The client
    /// routes this through the same jump path the session manager uses.
    ///
    /// A node identity rather than a resolved pane: above pane level the target
    /// is "that node's current focus", and the server is the one that knows
    /// what that is.
    JumpTo(JumpTarget),
}

/// Data pushed to plugins by the client. One variant per plugin family; later
/// phases add variants without touching the framework.
#[derive(Debug, Clone)]
pub enum PluginEvent {
    SessionTree {
        conn: ConnId,
        folders: Vec<crate::protocol::FolderTreeEntry>,
        unfiled: Vec<crate::protocol::SessionTreeEntry>,
        dormant: Vec<String>,
    },
    /// The panes running an AI coding agent on `conn`, and their states. A
    /// full list, replacing whatever that connection last reported.
    Agents {
        conn: ConnId,
        agents: Vec<crate::protocol::AgentEntry>,
        /// Whether that server can detect agents at all; see
        /// [`crate::protocol::ServerMessage::AgentList`]. `false` means "cannot
        /// know", which is not the same as "none", and the panel says so.
        supported: bool,
    },
    /// A connection went away; drop anything scoped to it.
    ConnectionLost { conn: ConnId },
    /// The working directory of the pane the user is focused on, as the
    /// FOREGROUND server reports it, or `None` when it is not known (no
    /// attachment, or a server too old to send one).
    ///
    /// Resolved by the client rather than by the plugin, even though the raw
    /// material is in [`PluginEvent::SessionTree`]: picking the focused pane
    /// means knowing which connection is in the foreground and which of its
    /// sessions the client is attached to, and that is the client's knowledge,
    /// not a panel's. Broadcast, since more than one panel may want to follow it.
    ///
    /// `conn` is part of the target, not decoration. Two machines routinely have
    /// a pane in the same-named directory (`/home/you`, `/srv/app`), and a panel
    /// comparing the path alone would decide nothing had changed when the
    /// foreground moved to a remote -- and go on showing the OLD machine's
    /// directory.
    FocusedCwd { conn: ConnId, cwd: Option<String> },
    /// The auxiliary pane this panel asked for exists. Delivered ONLY to the
    /// panel that requested it; the pane id itself stays with the client, which
    /// is what addresses the pane on the panel's behalf.
    AuxPaneReady,
    /// A fresh snapshot of this panel's auxiliary pane. Panel-targeted.
    AuxPaneContent { snapshot: Box<PaneSnapshot> },
    /// This panel's auxiliary pane is gone -- its program exited, or the
    /// connection carrying it dropped. Panel-targeted.
    AuxPaneExited,
}

/// Something a plugin needs the client to do on its behalf.
///
/// Panels do not speak to servers. A panel says what it wants; the client
/// resolves WHICH connection and WHICH pane id that means, because a panel has
/// no way to know either -- the foreground connection can change under it, and
/// its aux pane's id is assigned by whichever server answered. Keeping that
/// knowledge on one side is what lets [`PluginEvent::AuxPaneReady`] and friends
/// carry no addressing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRequest {
    /// Spawn this panel's auxiliary pane. Replaces any pane the panel already
    /// has (the client kills the old one first), which is how a re-target to a
    /// new directory works.
    Spawn {
        cols: u16,
        rows: u16,
        command: String,
        cwd: Option<String>,
    },
    /// (Re-)subscribe to this panel's aux pane at this size. Sent on every size
    /// change, exactly as a View cell re-subscribes: the size demand is folded
    /// into the pane's min-across-viewers effective size, so the pane reflows to
    /// the panel with no second sizing policy.
    Subscribe { cols: u16, rows: u16 },
    /// Raw bytes for this panel's aux pane.
    Input { data: Vec<u8> },
    /// Kill this panel's aux pane and forget it.
    Kill,
}

/// A panel that can live inside a sidebar.
pub trait SidebarPlugin: Send {
    /// Short name shown in the panel header.
    fn title(&self) -> &str;

    /// The smallest `(cols, rows)` this panel can usefully render into. The
    /// chrome drops the panel for a frame rather than rendering it smaller.
    fn min_size(&self) -> (u16, u16);

    /// Render into a `rows` x `cols` grid. MUST return exactly `rows` rows of
    /// exactly `cols` cells (or an empty vec when either is 0).
    fn render(
        &self,
        cols: u16,
        rows: u16,
        focused: bool,
        theme: &CompositorTheme,
    ) -> Vec<Vec<RenderCell>>;

    /// Handle a key while this panel has focus.
    fn on_key(&mut self, key: KeyEvent) -> PluginAction;

    /// Handle a mouse event at panel-local coordinates.
    fn on_mouse(&mut self, x: u16, y: u16, kind: MouseEventKind) -> PluginAction;

    /// Receive pushed data. Called regardless of focus.
    fn on_event(&mut self, ev: &PluginEvent);

    /// The panel has been laid out at this size. Called once per repaint pass
    /// for every panel the chrome actually placed, before anything is drawn.
    ///
    /// This is where a panel that owns a resource learns it has somewhere to put
    /// it: `render` takes `&self`, so a lazily-spawned aux pane cannot be
    /// started from there. A panel dropped for being below its minimum is not
    /// called at all, so one that has never been laid out never starts anything.
    ///
    /// That is NOT the same as "a hidden panel costs nothing", and `files` is
    /// where the difference shows. Hiding the sidebar stops the `on_size` calls
    /// but changes nothing else: the aux pane stays spawned and subscribed, and
    /// every `PaneContent` it produces still drives a paint. Deliberate --
    /// keeping the pane is exactly what makes un-hiding instant, and killing it
    /// would cost the user their place in the file manager every time the
    /// sidebar was toggled.
    fn on_size(&mut self, _cols: u16, _rows: u16) {}

    /// Hand over anything the plugin needs the client to do. Drained once per
    /// pass; a plugin that wants nothing returns an empty vec (the default).
    fn take_requests(&mut self) -> Vec<PluginRequest> {
        Vec::new()
    }

    /// Whether this panel needs `PluginEvent::SessionTree`.
    ///
    /// The client subscribes to the server's session-tree push only when some
    /// configured panel says yes -- a client with no such panel must put no
    /// extra traffic on the wire at all.
    fn wants_session_tree(&self) -> bool {
        false
    }

    /// Whether this panel needs [`PluginEvent::Agents`].
    ///
    /// Separate from [`SidebarPlugin::wants_session_tree`] for the same reason
    /// the server has two subscriptions: the payloads are dirtied by different
    /// things, and a client with only a sessions panel must put no agent
    /// traffic on the wire at all (nor the reverse).
    fn wants_agents(&self) -> bool {
        false
    }
}

/// Resolve a `[[sidebar.panel]]` entry to a plugin instance.
///
/// Returns `None` for an unknown name; the caller logs a warning and skips the
/// panel, so a config naming a not-yet-implemented plugin still loads.
///
/// The whole entry rather than just the name, because a plugin may need options
/// from it -- and may REFUSE the entry. `files` does: it has no default command,
/// so a panel that names no program is skipped with a warning rather than
/// spawning an arbitrary one in a PTY the user then has to hunt down.
pub fn make_plugin(cfg: &PanelConfig) -> Option<Box<dyn SidebarPlugin>> {
    match cfg.plugin.as_str() {
        "placeholder" => Some(Box::new(placeholder::PlaceholderPlugin::new())),
        "sessions" => Some(Box::new(sessions::SessionsPlugin::new())),
        "agents" => Some(Box::new(agents::AgentsPlugin::new())),
        "files" => match &cfg.command {
            Some(command) => Some(Box::new(files::FilesPlugin::new(command.clone()))),
            None => {
                log::warn!(
                    "sidebar: the `files` plugin requires a `command` (e.g. command = \"yazi\"); skipping this panel"
                );
                None
            }
        },
        _ => None,
    }
}

/// A `rows` x `cols` grid of spaces on `bg`.
pub fn blank_grid(cols: u16, rows: u16, bg: CellColor) -> Vec<Vec<RenderCell>> {
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    let cell = RenderCell {
        c: ' ',
        fg: CellColor::Default,
        bg,
        bold: false,
        italic: false,
        underline: false,
        width: 1,
        combining: Vec::new(),
        hyperlink: None,
    };
    vec![vec![cell; cols as usize]; rows as usize]
}

/// Write `text` into `grid` at `(x, y)`, clipping at the right edge.
///
/// Wide glyphs emit a `width: 2` lead followed by a `width: 0` continuation,
/// matching what the server compositor produces -- the renderer skips
/// continuation cells, so omitting them would shift every following column.
///
/// Zero-width combining marks attach to the cell they modify (`RenderCell`'s
/// `combining` vec) rather than taking a column of their own: panels render
/// arbitrary user text -- session, folder, tab and pane names -- and a
/// decomposed (NFD) accent would otherwise be laid out as a stray spacing cell
/// that shifts everything after it. A control character (no width at all) is
/// dropped for the same reason.
pub fn draw_text(
    grid: &mut [Vec<RenderCell>],
    x: u16,
    y: u16,
    text: &str,
    fg: CellColor,
    bg: CellColor,
) {
    let Some(row) = grid.get_mut(y as usize) else {
        return;
    };
    let width = row.len();
    let mut cx = x as usize;
    // The column of the last spacing cell written, so a combining mark knows
    // what it modifies. A mark with nothing before it (`text` opening with one,
    // or `x` at column 0) has no base and is dropped.
    let mut last_base: Option<usize> = None;
    for ch in text.chars() {
        let w = match UnicodeWidthChar::width(ch) {
            Some(0) | None if ch.is_control() => continue,
            Some(0) | None => {
                // A combining mark: hang it off the base cell.
                if let Some(base) = last_base {
                    row[base].combining.push(ch);
                }
                continue;
            }
            Some(w) => w,
        };
        if cx + w > width {
            break;
        }
        row[cx] = RenderCell {
            c: ch,
            fg: fg.clone(),
            bg: bg.clone(),
            bold: false,
            italic: false,
            underline: false,
            width: w as u8,
            combining: Vec::new(),
            hyperlink: None,
        };
        for k in 1..w {
            row[cx + k] = RenderCell {
                c: ' ',
                fg: fg.clone(),
                bg: bg.clone(),
                bold: false,
                italic: false,
                underline: false,
                width: 0,
                combining: Vec::new(),
                hyperlink: None,
            };
        }
        last_base = Some(cx);
        cx += w;
    }
}
