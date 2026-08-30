//! Sidebar plugins: the panels that live inside the client's chrome.
//!
//! A plugin renders a `RenderCell` grid -- the same primitive the server
//! compositor produces -- rather than `DrawCommand`s, because panels are
//! written INTO the renderer's front buffer rather than painted over it. See
//! the design doc, section 6.

use std::time::Duration;

use crossterm::event::{KeyEvent, MouseEventKind};
use unicode_width::UnicodeWidthChar;

use crate::client::registry::ConnId;
use crate::client::tree_model::JumpTarget;
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
    /// Move focus out of the sidebar, with no direction to offer.
    ///
    /// Distinct from [`PluginAction::LeaveTo`] on purpose. A panel that has been
    /// asked to leave BECAUSE it did something -- `files` opening a file in a
    /// split -- has no direction to give: it does not know which edge it is
    /// docked to, and the pane it wants the user in is the one the server just
    /// created, not a spatial neighbour. The two happen to be handled
    /// identically today (the `LeaveTo` handler ignores its direction), and that
    /// is exactly why this is a separate variant rather than a fabricated
    /// direction: the day the direction starts being honoured, a made-up one
    /// would send focus somewhere nobody chose.
    Leave,
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
    /// The contents of a directory on `conn`, as that server reported them.
    ///
    /// Broadcast rather than panel-targeted, and correlated by `(conn, path)`
    /// instead: a panel claims the listing it asked for and ignores the rest.
    /// That is cheaper AND more honest than a correlation queue -- two `files`
    /// panels sitting in the same directory both genuinely want this answer, and
    /// a queue would hand it to one of them.
    DirectoryListing {
        conn: ConnId,
        path: String,
        entries: Vec<crate::protocol::DirEntry>,
        /// Why the directory could not be listed. Shown, never swallowed: an
        /// empty panel that might mean "empty" and might mean "denied" is the
        /// same ambiguity the agents panel's `supported` exists to remove.
        error: Option<String>,
        /// Whether the server hit its entry cap, so this listing is incomplete.
        truncated: bool,
    },
}

/// Something a plugin needs the client to do on its behalf.
///
/// Panels do not speak to servers: a panel says what it wants and the client
/// sends it. What the client is allowed to RESOLVE, though, is narrower than it
/// looks, and the difference is a correctness boundary rather than a style
/// preference: **the connection is NOT the client's to pick.** A panel driven by
/// [`PluginEvent::FocusedCwd`] knows exactly which machine the path it is
/// holding came from, and the client's `foreground()` is a DIFFERENT fact that
/// moves first. Between a foreground switch and the `FocusedCwd` derived from
/// the new connection's first tree push, the two disagree -- and a request
/// routed by `foreground()` then carries one machine's path to another machine.
/// For `OpenInSplit` that means an editor opening at a path that does not exist
/// there, silently creating an empty file with a plausible name.
///
/// So both [`ListDirectory`](PluginRequest::ListDirectory) and
/// [`OpenInSplit`](PluginRequest::OpenInSplit) carry the conn the PANEL is
/// looking at, and the client routes where it is told. Those are both of them;
/// the class is closed, so a third reader of `foreground()` appearing here
/// should be read as a bug rather than as a decision. (It was three until the
/// two file panels merged: the deleted `Spawn` carried a conn for the identical
/// reason.)
///
/// This was reasoned away once, in a comment claiming the two were equal "by
/// construction". They are equal in the steady state; the construction has a
/// window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRequest {
    /// List this directory on `conn`. Answered by a
    /// [`PluginEvent::DirectoryListing`] broadcast, which the panel claims by
    /// `(conn, path)` -- so this must be the SAME conn the panel will correlate
    /// on, or the reply is never claimed and the panel waits for ever.
    ///
    /// `path` must already be absolute and normalised -- the server echoes it
    /// back verbatim, so a panel that sent `/a/b/..` would be looking for a
    /// reply about `/a`.
    ListDirectory { conn: ConnId, path: String },
    /// Open this file in a split running an editor, on `conn`.
    ///
    /// `conn` is the machine the panel believes `path` is on, not the client's
    /// foreground -- see this enum's note. Getting it wrong here is the worst
    /// case in the whole plugin surface: an editor opening a nonexistent path on
    /// the wrong machine creates a file rather than reporting anything.
    ///
    /// `command` is the panel's configured editor override, or `None` to let the
    /// SERVER decide -- which is the point: the editor must exist where the file
    /// is, and the file is on the server.
    OpenInSplit {
        conn: ConnId,
        path: String,
        command: Option<String>,
        vertical: bool,
    },
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
    /// This is where a panel that needs to ACT on being visible does so:
    /// `render` takes `&self`, so nothing can be started from there. A panel
    /// dropped for being below its minimum is not called at all, so one that has
    /// never been laid out never starts anything -- and a hidden sidebar stops
    /// the calls, which is what makes a hidden panel free.
    fn on_size(&mut self, _cols: u16, _rows: u16) {}

    /// How long until this panel wants [`SidebarPlugin::tick`] called, or
    /// `None` if it never does. The default is `None`: a panel is event-driven
    /// unless it says otherwise.
    ///
    /// The client arms ONE timer, for the soonest answer across the panels the
    /// chrome actually PLACED -- so a panel in a hidden sidebar is not asked,
    /// and a client whose panels all answer `None` arms no timer at all. That is
    /// what makes "it costs nothing when the sidebar is closed" true rather than
    /// merely cheap.
    ///
    /// It must be computed from an ANCHOR the panel stores (a `last_*: Instant`)
    /// rather than returned as a fresh interval each call. The client recomputes
    /// this on every pass through its event loop, and a pass happens on every
    /// keystroke and every frame from the server; a panel answering "two seconds
    /// from now" each time would have its deadline pushed back by a busy pane
    /// and never fire.
    fn poll_after(&self) -> Option<Duration> {
        None
    }

    /// The deadline from [`SidebarPlugin::poll_after`] has passed.
    ///
    /// Called on the same pass as [`SidebarPlugin::on_size`], for the same
    /// panels -- the ones the chrome placed -- and before
    /// [`SidebarPlugin::take_requests`] drains whatever it produced. A panel is
    /// called whenever ANY panel's timer fires, not only its own, so it must
    /// check its own deadline rather than assume it is due.
    fn tick(&mut self) {}

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
/// from it -- and because two of its fields are now MIGRATIONS, which is a
/// decision this function is the only place to make.
///
/// ## `plugin = "browser"` is aliased; `command` is not
///
/// The two file panels merged: the built-in browser took the `files` name, and
/// the plugin that hosted `nnn`/`yazi`/`ranger` in an auxiliary pane is gone.
/// Two things in an existing config point at the old world, and they are
/// treated in OPPOSITE ways on purpose -- alias where the old spelling maps to
/// the right behaviour, ignore where it maps to the wrong one:
///
/// * **`plugin = "browser"` is accepted**, with a warning. It named this exact
///   panel, so honouring it is correct; and refusing it would fall through to
///   the unknown-plugin rule, which SKIPS the panel -- the user's sidebar would
///   quietly come back missing a panel with only a log line to say why.
/// * **`command` is accepted and then IGNORED**, with a warning naming `editor`.
///   Aliasing it to `editor` was the obvious move and is wrong in both
///   directions. On an old `browser` panel `command` was already the editor, and
///   it is precisely the field that made a `command = "nnn"` copied from a
///   `files` panel open every file in `nnn` -- the reported bug, preserved. On
///   an old `files` panel it named a FILE MANAGER, and calling that an editor is
///   nonsense. Ignored, both configs fall back to the server's `$EDITOR`, which
///   is what the user wanted in each case.
///
/// The field is still declared in [`PanelConfig`] rather than deleted: serde
/// ignores unknown keys silently, so deleting it would take the warning with it
/// and leave the user with a config line that does nothing and says nothing.
pub fn make_plugin(cfg: &PanelConfig) -> Option<Box<dyn SidebarPlugin>> {
    if cfg.command.is_some() {
        log::warn!(
            "sidebar: `command` is no longer read (panel plugin = {:?}). It meant the file \
manager to the old `files` plugin, which has been removed, and the editor to `browser`, \
which is now `files` -- one field, two meanings, which is why a `command` copied between \
them opened files in a file manager. Use `editor` to override the editor; delete it to use \
the server's $EDITOR.",
            cfg.plugin
        );
    }
    match cfg.plugin.as_str() {
        "placeholder" => Some(Box::new(placeholder::PlaceholderPlugin::new())),
        "sessions" => Some(Box::new(sessions::SessionsPlugin::new())),
        "agents" => Some(Box::new(agents::AgentsPlugin::new())),
        "files" => Some(Box::new(files::FilesPlugin::new(cfg.editor.clone()))),
        "browser" => {
            log::warn!(
                "sidebar: the `browser` plugin has been renamed to `files` (and the old \
`files` plugin, which hosted nnn/yazi/ranger, has been removed). Loading it as `files`; \
rename it in your config."
            );
            Some(Box::new(files::FilesPlugin::new(cfg.editor.clone())))
        }
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

/// Fit a path into `width` columns, keeping the END of it -- the leaf directory
/// is what identifies where you are, and it is the part a left-truncating
/// header would throw away first.
///
/// Used by the `files` panel's header, and kept general for anything else whose
/// header is a directory.
pub fn shorten_path(path: &str, width: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= width || width == 0 {
        return path.to_string();
    }
    if width == 1 {
        return "\u{2026}".to_string();
    }
    let tail: String = chars[chars.len() - (width - 1)..].iter().collect();
    format!("\u{2026}{tail}")
}
