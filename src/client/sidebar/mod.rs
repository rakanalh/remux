//! Sidebar plugins: the panels that live inside the client's chrome.
//!
//! A plugin renders a `RenderCell` grid -- the same primitive the server
//! compositor produces -- rather than `DrawCommand`s, because panels are
//! written INTO the renderer's front buffer rather than painted over it. See
//! the design doc, section 6.

use crossterm::event::{KeyEvent, MouseEventKind};
use unicode_width::UnicodeWidthChar;

use crate::client::registry::ConnId;
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, PaneId, RenderCell};
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
    /// Jump to a pane, anywhere. The client routes this through the existing
    /// session-manager jump path.
    JumpTo {
        conn: ConnId,
        session: String,
        tab_index: usize,
        pane_id: PaneId,
    },
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
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
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
        cx += w;
    }
}
