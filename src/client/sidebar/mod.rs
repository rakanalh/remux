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
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, RenderCell};
use crate::server::layout::FocusDirection;

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
    /// A connection went away; drop anything scoped to it.
    ConnectionLost { conn: ConnId },
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

    /// Whether this panel needs `PluginEvent::SessionTree`.
    ///
    /// The client subscribes to the server's session-tree push only when some
    /// configured panel says yes -- a client with no such panel must put no
    /// extra traffic on the wire at all.
    fn wants_session_tree(&self) -> bool {
        false
    }
}

/// Resolve a config `plugin` name to an instance.
///
/// Returns `None` for an unknown name; the caller logs a warning and skips the
/// panel, so a config naming a not-yet-implemented plugin still loads.
pub fn make_plugin(name: &str) -> Option<Box<dyn SidebarPlugin>> {
    match name {
        "placeholder" => Some(Box::new(placeholder::PlaceholderPlugin::new())),
        "sessions" => Some(Box::new(sessions::SessionsPlugin::new())),
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
