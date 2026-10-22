//! Client-side terminal renderer.
//!
//! Uses crossterm to render the composited screen buffer received from the
//! server. Supports both full renders and incremental diff-based updates.

use std::io::{self, Write};

use anyhow::Result;
use crossterm::cursor::MoveTo;
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::{cursor, queue, terminal};

/// `c`, or a space in its place when it is a control character.
///
/// Every character the client prints goes through this. Cells arrive from a
/// server, and names inside them (session, tab, pane, view) are whatever a
/// client or a program chose; a control character among them would start an
/// escape sequence in the user's OUTER terminal, where remux's emulator and
/// its OSC 52 policy cannot see it. A space keeps the cell's width.
fn printable(c: char) -> char {
    if c.is_control() {
        ' '
    } else {
        c
    }
}

/// `text` with its control characters dropped, for text that is framed inside
/// an escape sequence the client writes itself, such as a hyperlink's URI.
fn printable_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

use crate::client::input::{SelectionMode, VisualState};
use crate::client::popup::HeldLayer;
use crate::client::whichkey::DrawCommand;
use crate::protocol::{CellChange, CellColor, RenderCell};
use crate::server::compositor::{box_bottom_line, box_top_line_titled, BOX_VERTICAL};
use crate::server::layout::Rect;

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// The popup box: a layer the client draws over everything else.
///
/// It is a MASK, not part of the front buffer. The front buffer keeps what
/// belongs under the box (the server frame, the sidebars), and every write that
/// would land inside `rect` is skipped, neither emitted nor stored, so the box
/// is never overdrawn and never has to be repainted from beneath. Dropping the
/// layer lifts the mask; the server answers that with a full frame, which
/// repaints what the box covered.
struct PopupBuf {
    /// The box in window coordinates, frame included.
    rect: Rect,
    /// The box's cells, `rect.height` rows of `rect.width` cells.
    cells: Vec<Vec<RenderCell>>,
    /// The popup's cursor, window-absolute: `(x, y, visible, style)`.
    cursor: (u16, u16, bool, u8),
    /// The server's layer this box was composed from. The next frame's diff is
    /// applied to it, so it lives and dies with the box.
    held: Option<HeldLayer>,
}

/// SGR state for the cells of one row, so a run of cells emits only what
/// changes between them. Starts from the state a row reset leaves.
#[derive(Default)]
struct RowStyle {
    fg: Option<CellColor>,
    bg: Option<CellColor>,
    bold: bool,
    italic: bool,
    underline: bool,
    hyperlink: Option<String>,
}

impl RowStyle {
    fn new() -> Self {
        RowStyle {
            fg: Some(CellColor::Default),
            bg: Some(CellColor::Default),
            ..RowStyle::default()
        }
    }

    /// Queue `cell`'s attributes, its glyph and its combining marks. A
    /// width-0 continuation cell is the caller's to skip.
    fn emit<W: Write>(&mut self, out: &mut W, cell: &RenderCell) -> Result<()> {
        if self.fg.as_ref() != Some(&cell.fg) {
            queue!(out, SetForegroundColor(cell_color_to_crossterm(&cell.fg)))?;
            self.fg = Some(cell.fg.clone());
        }
        if self.bg.as_ref() != Some(&cell.bg) {
            queue!(out, SetBackgroundColor(cell_color_to_crossterm(&cell.bg)))?;
            self.bg = Some(cell.bg.clone());
        }
        if cell.bold != self.bold {
            let attr = if cell.bold {
                Attribute::Bold
            } else {
                Attribute::NormalIntensity
            };
            queue!(out, SetAttribute(attr))?;
            self.bold = cell.bold;
        }
        if cell.italic != self.italic {
            let attr = if cell.italic {
                Attribute::Italic
            } else {
                Attribute::NoItalic
            };
            queue!(out, SetAttribute(attr))?;
            self.italic = cell.italic;
        }
        if cell.underline != self.underline {
            let attr = if cell.underline {
                Attribute::Underlined
            } else {
                Attribute::NoUnderline
            };
            queue!(out, SetAttribute(attr))?;
            self.underline = cell.underline;
        }
        if cell.hyperlink.as_deref() != self.hyperlink.as_deref() {
            match &cell.hyperlink {
                Some(uri) => queue!(out, Print(format!("\x1b]8;;{}\x1b\\", printable_text(uri))))?,
                None => queue!(out, Print("\x1b]8;;\x1b\\"))?,
            }
            self.hyperlink = cell.hyperlink.clone();
        }
        queue!(out, Print(printable(cell.c)))?;
        for m in &cell.combining {
            queue!(out, Print(printable(*m)))?;
        }
        Ok(())
    }

    /// Close an open hyperlink and reset, so nothing carries into the next row.
    fn end_row<W: Write>(&mut self, out: &mut W) -> Result<()> {
        if self.hyperlink.is_some() {
            queue!(out, Print("\x1b]8;;\x1b\\"))?;
        }
        queue!(out, SetAttribute(Attribute::Reset))?;
        Ok(())
    }
}

/// The client-side renderer that maintains a front buffer and uses crossterm
/// to draw changes to the actual terminal.
///
/// # Coordinate contract
///
/// The terminal is split into a *content rect* -- where server frames are
/// drawn -- and the sidebar panels around it. Two coordinate spaces meet here,
/// and which one a method takes is part of its contract:
///
/// - **Server-frame methods** -- [`Renderer::render_full`],
///   [`Renderer::render_diff`], [`Renderer::render_scroll`] and
///   [`Renderer::restore_cursor`] -- take **content-relative** coordinates and
///   apply the content origin internally.
/// - **[`Renderer::paint_panel`]** takes **absolute screen** coordinates. Panel
///   rects come from `chrome::geometry::panel_rects`, which already computes
///   absolutes, so no origin is applied.
///
/// The front buffer is always the FULL terminal in absolute coordinates,
/// panels included. Nothing a server frame draws may write, clear, or move a
/// cell outside the content rect. Every server-frame method clips to it
/// unconditionally, so an in-flight frame -- or a stale pane rect -- built for
/// a previous, larger geometry is truncated rather than allowed to paint over
/// a panel.
pub struct Renderer {
    /// The front buffer: what is currently displayed on screen. Always the
    /// FULL terminal, including sidebar columns -- only the write position of
    /// server content is offset. This is what keeps diff rendering coherent.
    front: Vec<Vec<RenderCell>>,
    cols: u16,
    rows: u16,
    /// Top-left of the content rect. Server frames arrive in content-relative
    /// coordinates and are written here.
    origin_x: u16,
    origin_y: u16,
    /// Size of the content rect. Every clear a server frame performs is bounded
    /// by it: a frame smaller than the content rect must still blank the stale
    /// remainder *inside* the rect, but must never reach past it into a panel.
    /// Defaults to the full terminal, which reproduces the pre-sidebar
    /// behaviour exactly when no sidebars are configured.
    content_cols: u16,
    content_rows: u16,
    /// The last cursor state a SERVER FRAME reported, content-relative:
    /// `(x, y, visible, style)`.
    ///
    /// Remembered so painting a panel can be cursor-neutral: `paint_panel`
    /// hides the cursor while it draws and re-issues this afterwards. Without
    /// it the panel's `Hide` and the frame's `Show` land in the same flush and
    /// the cursor stays hidden for as long as a sidebar is visible.
    ///
    /// The fifth field is whether `(x, y)` is already a WINDOW-absolute position
    /// rather than content-relative. It is while the popup layer is up: the
    /// popup can sit over a sidebar, where no content-relative coordinate
    /// reaches.
    last_cursor: (u16, u16, bool, u8, bool),
    /// The popup box the client draws itself, over everything else, while the
    /// server has a popup visible. See [`PopupBuf`].
    popup: Option<PopupBuf>,
    /// Whether the client draws the status bar itself, in which case a server
    /// frame's LAST row (the server's bar) is never painted. See
    /// `chrome::geometry::status_bar_owned`.
    status_row_owned: bool,
    /// Row count of the last frame [`Renderer::render_full`] was given, so a
    /// diff can tell which of its changes land on that frame's last row.
    frame_rows: usize,
    /// The last row of the most recent server frame, kept up to date by diffs
    /// while [`Renderer::status_row_owned`] withholds it from the screen. A
    /// server too old to describe its status bar leaves the client this row as
    /// the only copy of it.
    frame_status_row: Vec<RenderCell>,
}

impl Renderer {
    /// Create a new renderer with the given dimensions.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            front: vec![vec![RenderCell::default(); cols as usize]; rows as usize],
            cols,
            rows,
            origin_x: 0,
            origin_y: 0,
            content_cols: cols,
            content_rows: rows,
            last_cursor: (0, 0, false, 0, false),
            popup: None,
            status_row_owned: false,
            frame_rows: 0,
            frame_status_row: Vec::new(),
        }
    }

    /// Set whether the client draws the status bar itself. While it does, the
    /// last row of every server frame is withheld from the screen and kept in
    /// [`Renderer::frame_status_row`] instead.
    ///
    /// Withheld rather than clipped by the content size alone: a frame sized to
    /// a smaller client attached to the same session has FEWER rows than the
    /// content rect, and its bar row would then land in the middle of this
    /// terminal, above the client's own bar.
    pub fn set_status_row_owned(&mut self, owned: bool) {
        self.status_row_owned = owned;
    }

    /// The server frame's last row as it stands after the latest frame and
    /// diffs. Only maintained while the status row is owned.
    pub fn frame_status_row(&self) -> &[RenderCell] {
        &self.frame_status_row
    }

    /// Set the top-left of the content rect. Server frames are written here.
    pub fn set_origin(&mut self, x: u16, y: u16) {
        self.origin_x = x;
        self.origin_y = y;
    }

    /// The current content origin.
    pub fn origin(&self) -> (u16, u16) {
        (self.origin_x, self.origin_y)
    }

    /// The origin a server-frame-relative coordinate is offset by to reach the
    /// screen: the content origin, except while the popup layer is up. Then the
    /// coordinates the client holds (the focused pane rect, the visual cursor)
    /// are window-absolute, because the popup can sit over a sidebar, and no
    /// offset applies.
    pub fn rect_origin(&self) -> (u16, u16) {
        if self.popup.is_some() {
            (0, 0)
        } else {
            self.origin()
        }
    }

    /// The popup box, in window coordinates, while the layer is up.
    pub fn popup_rect(&self) -> Option<Rect> {
        self.popup.as_ref().map(|p| p.rect)
    }

    /// Whether a write of a `width`-wide cell at screen `(sx, sy)` would touch
    /// the popup box. A wide glyph straddling the box's edge counts as covered:
    /// printing it would overdraw the box, and skipping only its half would
    /// leave half a glyph.
    fn masked(&self, sx: usize, sy: usize, width: u8) -> bool {
        let Some(popup) = &self.popup else {
            return false;
        };
        let r = popup.rect;
        let (x0, y0) = (r.x as usize, r.y as usize);
        let (x1, y1) = (x0 + r.width as usize, y0 + r.height as usize);
        sy >= y0 && sy < y1 && sx < x1 && sx + (width.max(1) as usize) > x0
    }

    /// The cell on screen at `(sx, sy)`: the popup's while the box covers it,
    /// else the front buffer's. What overlays that read the screen back (the
    /// visual selection, a search highlight, a copy) must ask this instead of
    /// the front buffer, which never holds the box.
    fn cell_at(&self, sx: usize, sy: usize) -> Option<&RenderCell> {
        if let Some(popup) = &self.popup {
            let r = popup.rect;
            let (x0, y0) = (r.x as usize, r.y as usize);
            if sx >= x0 && sy >= y0 {
                if let Some(cell) = popup.cells.get(sy - y0).and_then(|row| row.get(sx - x0)) {
                    return Some(cell);
                }
            }
        }
        self.front.get(sy).and_then(|row| row.get(sx))
    }

    /// Put the popup box on screen and start masking it.
    ///
    /// `rect` and `cursor` are window-absolute and `cells` is the box's own grid.
    /// Against the box already shown at the same rect, only the cells that
    /// changed are emitted; anywhere else the whole box is. Call this BEFORE the
    /// frame the box belongs to is painted: that frame then skips the box.
    pub fn set_popup(
        &mut self,
        rect: Rect,
        cells: Vec<Vec<RenderCell>>,
        cursor: (u16, u16, bool, u8),
    ) -> Result<()> {
        let mut stdout = io::stdout().lock();
        self.set_popup_into(&mut stdout, rect, cells, cursor)
    }

    /// The body of [`Renderer::set_popup`], parameterised over the sink for the
    /// same reason as [`Renderer::paint_panel_into`].
    fn set_popup_into<W: Write>(
        &mut self,
        out: &mut W,
        rect: Rect,
        cells: Vec<Vec<RenderCell>>,
        cursor: (u16, u16, bool, u8),
    ) -> Result<()> {
        let previous = self
            .popup
            .take()
            .filter(|p| p.rect == rect)
            .map(|p| p.cells);
        self.popup = Some(PopupBuf {
            rect,
            cells,
            cursor,
            held: None,
        });
        self.paint_popup_into(out, previous.as_deref())
    }

    /// Emit the popup box: every cell, or only those that differ from `previous`.
    fn paint_popup_into<W: Write>(
        &self,
        out: &mut W,
        previous: Option<&[Vec<RenderCell>]>,
    ) -> Result<()> {
        let Some(popup) = &self.popup else {
            return Ok(());
        };
        queue!(out, Print("\x1b[?2026h"))?;
        queue!(out, cursor::Hide)?;
        for (ry, row) in popup.cells.iter().enumerate() {
            let sy = popup.rect.y as usize + ry;
            if ry >= popup.rect.height as usize || sy >= self.rows as usize {
                break;
            }
            // Same reset discipline as `paint_panel_into`.
            queue!(out, SetAttribute(Attribute::Reset))?;
            let mut style = RowStyle::new();
            let mut at_cursor = false;
            for (rx, cell) in row.iter().enumerate() {
                let sx = popup.rect.x as usize + rx;
                if rx >= popup.rect.width as usize || sx >= self.cols as usize {
                    break;
                }
                let unchanged = previous
                    .and_then(|p| p.get(ry))
                    .and_then(|prow| prow.get(rx))
                    == Some(cell);
                // A continuation cell leaves the terminal's cursor where its
                // lead's two-column print put it.
                if cell.width == 0 {
                    continue;
                }
                if unchanged {
                    at_cursor = false;
                    continue;
                }
                if !at_cursor {
                    queue!(out, MoveTo(sx as u16, sy as u16))?;
                    at_cursor = true;
                }
                style.emit(out, cell)?;
            }
            style.end_row(out)?;
        }
        queue!(out, Print("\x1b[?2026l"))?;
        Ok(())
    }

    /// Remember the server layer the box was composed from.
    pub fn set_held_layer(&mut self, layer: HeldLayer) {
        if let Some(popup) = &mut self.popup {
            popup.held = Some(layer);
        }
    }

    /// The server layer the box was composed from, while the box is up.
    pub fn held_layer(&self) -> Option<&HeldLayer> {
        self.popup.as_ref().and_then(|p| p.held.as_ref())
    }

    /// Drop the popup box. Nothing is painted over the cells it covered: the
    /// server answers a vanished layer with a full frame, which does.
    pub fn clear_popup(&mut self) {
        self.popup = None;
    }

    /// Set the size of the content rect, in cells.
    ///
    /// This bounds every clear a server frame performs. A [`Renderer::resize`]
    /// resets it to the full terminal, so the caller must set it again after
    /// one -- otherwise a frame smaller than the content rect would blank
    /// across a panel.
    pub fn set_content_size(&mut self, cols: u16, rows: u16) {
        self.content_cols = cols;
        self.content_rows = rows;
    }

    /// The current content rect size.
    pub fn content_size(&self) -> (u16, u16) {
        (self.content_cols, self.content_rows)
    }

    /// The terminal size the front buffer is allocated for.
    ///
    /// Panels must be laid out against THIS, not against a fresh
    /// `terminal::size()`: between a SIGWINCH and the `Resize` event the two
    /// disagree, and a panel laid out for the new width would be clipped
    /// against the old buffer.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// The absolute right/bottom edges of the content rect, clipped to the
    /// terminal. Every server-frame write, clear and cursor move is bounded by
    /// these; a panel lives beyond them.
    fn content_edges(&self) -> (usize, usize) {
        (
            (self.origin_x as usize + self.content_cols as usize).min(self.cols as usize),
            (self.origin_y as usize + self.content_rows as usize).min(self.rows as usize),
        )
    }

    /// Translate a content-relative cursor position to an absolute screen
    /// position, clamped to the content rect.
    ///
    /// Clamping to the terminal instead would park the hardware cursor inside a
    /// panel. The content rect is never zero-sized in practice
    /// (`chrome::geometry` guarantees it), so the `saturating_sub` floors here
    /// only matter for a degenerate rect that has no content to draw anyway.
    ///
    /// An `absolute` position is a window position already and is only clamped
    /// to the terminal.
    fn cursor_screen_pos(&self, x: u16, y: u16, absolute: bool) -> (u16, u16) {
        if absolute {
            return (
                x.min(self.cols.saturating_sub(1)),
                y.min(self.rows.saturating_sub(1)),
            );
        }
        let (right, bottom) = self.content_edges();
        (
            x.saturating_add(self.origin_x)
                .min(right.saturating_sub(1) as u16),
            y.saturating_add(self.origin_y)
                .min(bottom.saturating_sub(1) as u16),
        )
    }

    /// Remember the cursor state a server frame reported and queue it.
    ///
    /// Every server-frame method ends here, so [`Renderer::paint_panel`] can
    /// put the cursor back exactly as the last frame left it.
    fn queue_cursor<W: Write>(
        &mut self,
        out: &mut W,
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
    ) -> Result<()> {
        self.last_cursor = match &self.popup {
            // The popup owns the cursor while it is up, wherever the server
            // reported its own.
            Some(popup) => {
                let (x, y, visible, style) = popup.cursor;
                (x, y, visible, style, true)
            }
            None => (cursor_x, cursor_y, cursor_visible, cursor_style, false),
        };
        self.queue_remembered_cursor(out)
    }

    /// Queue the remembered cursor state without changing it.
    ///
    /// The remembered position is content-relative, so it is offset here --
    /// otherwise the hardware cursor lands in a left sidebar.
    fn queue_remembered_cursor<W: Write>(&self, out: &mut W) -> Result<()> {
        let (x, y, visible, style, absolute) = self.last_cursor;
        if visible {
            let (sx, sy) = self.cursor_screen_pos(x, y, absolute);
            queue!(
                out,
                MoveTo(sx, sy),
                cursor_style_command(style),
                cursor::Show,
            )?;
        } else {
            queue!(out, cursor::Hide)?;
        }
        Ok(())
    }

    /// Apply a full render (replace everything).
    pub fn render_full(
        &mut self,
        cells: &[Vec<RenderCell>],
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
    ) -> Result<()> {
        let mut stdout = io::stdout().lock();
        self.render_full_into(
            &mut stdout,
            cells,
            cursor_x,
            cursor_y,
            cursor_visible,
            cursor_style,
        )
    }

    /// The body of [`Renderer::render_full`], parameterised over the sink for
    /// the same reason as [`Renderer::paint_panel_into`].
    fn render_full_into<W: Write>(
        &mut self,
        out: &mut W,
        cells: &[Vec<RenderCell>],
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
    ) -> Result<()> {
        self.frame_rows = cells.len();
        let cells = match cells.split_last() {
            Some((status, body)) if self.status_row_owned => {
                self.frame_status_row = status.clone();
                body
            }
            _ => cells,
        };
        let rows = cells.len();
        let cols = cells.first().map_or(0, |r| r.len());
        log::debug!(
            "renderer: render_full dims={}x{} cursor=({},{}) visible={}",
            rows,
            cols,
            cursor_x,
            cursor_y,
            cursor_visible
        );

        let ox = self.origin_x as usize;
        let oy = self.origin_y as usize;
        // Absolute right/bottom edges of the content rect. EVERY write below is
        // bounded by these, not by the terminal: a frame smaller than the
        // content rect must still blank the stale remainder inside the rect,
        // and a frame LARGER than it (an in-flight frame built for a previous,
        // bigger geometry, e.g. after a resize) must be clipped rather than
        // allowed to paint over a panel and persist that into the front buffer.
        let (content_right, content_bottom) = self.content_edges();

        // Bracket the whole frame in synchronized output (DEC 2026) so the
        // outer terminal displays it atomically instead of tearing.
        queue!(out, Print("\x1b[?2026h"))?;

        // Hide cursor during rendering to avoid flicker.
        queue!(out, cursor::Hide)?;

        for (y, row) in cells.iter().enumerate() {
            let sy = y + oy;
            if sy >= content_bottom {
                break;
            }

            // Full SGR reset (SGR 0) so the terminal's real state matches the
            // per-row assumption that fg/bg are Default and bold/italic/underline
            // are off. `ResetColor` only clears colors, leaving stale
            // bold/italic/underline from a previous row visible on leading cells.
            // Emitted before the MoveTo so the assumption holds at the content
            // origin's column, not just at column 0.
            queue!(out, SetAttribute(Attribute::Reset))?;
            queue!(out, MoveTo(self.origin_x, sy as u16))?;

            let mut last_fg = CellColor::Default;
            let mut last_bg = CellColor::Default;
            let mut last_bold = false;
            let mut last_italic = false;
            let mut last_underline = false;
            let mut last_hyperlink: Option<String> = None;
            // Set after a skipped run, so the next emitted cell is preceded by
            // the move that puts the terminal's cursor back on it.
            let mut need_move = false;

            for (x, cell) in row.iter().enumerate() {
                let sx = x + ox;
                if sx >= content_right {
                    break;
                }

                // Continuation cell of a wide glyph: the preceding width-2 Print
                // already advanced the physical cursor by 2, so skip it.
                if cell.width == 0 {
                    continue;
                }

                // The popup box covers this cell: neither emit nor store it.
                if self.masked(sx, sy, cell.width) {
                    need_move = true;
                    continue;
                }
                if need_move {
                    queue!(out, MoveTo(sx as u16, sy as u16))?;
                    need_move = false;
                }

                // Apply style changes only when needed.
                if cell.fg != last_fg {
                    queue!(out, SetForegroundColor(cell_color_to_crossterm(&cell.fg)))?;
                    last_fg = cell.fg.clone();
                }
                if cell.bg != last_bg {
                    queue!(out, SetBackgroundColor(cell_color_to_crossterm(&cell.bg)))?;
                    last_bg = cell.bg.clone();
                }
                if cell.bold != last_bold {
                    if cell.bold {
                        queue!(out, SetAttribute(Attribute::Bold))?;
                    } else {
                        queue!(out, SetAttribute(Attribute::NormalIntensity))?;
                    }
                    last_bold = cell.bold;
                }
                if cell.italic != last_italic {
                    if cell.italic {
                        queue!(out, SetAttribute(Attribute::Italic))?;
                    } else {
                        queue!(out, SetAttribute(Attribute::NoItalic))?;
                    }
                    last_italic = cell.italic;
                }
                if cell.underline != last_underline {
                    if cell.underline {
                        queue!(out, SetAttribute(Attribute::Underlined))?;
                    } else {
                        queue!(out, SetAttribute(Attribute::NoUnderline))?;
                    }
                    last_underline = cell.underline;
                }
                if cell.hyperlink.as_deref() != last_hyperlink.as_deref() {
                    match &cell.hyperlink {
                        Some(uri) => {
                            queue!(out, Print(format!("\x1b]8;;{}\x1b\\", printable_text(uri))))?
                        }
                        None => queue!(out, Print("\x1b]8;;\x1b\\"))?,
                    }
                    last_hyperlink = cell.hyperlink.clone();
                }

                queue!(out, Print(printable(cell.c)))?;
                // Combining marks are zero-width; the terminal composes them onto
                // the base glyph just printed without advancing the cursor.
                for m in &cell.combining {
                    queue!(out, Print(printable(*m)))?;
                }
            }

            // Close any open hyperlink so links never span rows on the wire.
            if last_hyperlink.is_some() {
                queue!(out, Print("\x1b]8;;\x1b\\"))?;
            }
            queue!(out, ResetColor)?;

            // A row may be shorter than the content area (the frame is sized to
            // the MIN across attached clients, so a larger client would leave
            // stale content to the right of it). Blank the remainder of the
            // CONTENT rect only -- never to the terminal edge, because a right
            // sidebar lives out there and `Clear(ClearType::UntilNewLine)` would
            // erase it on every frame. The cursor is already positioned after
            // the last painted cell and ResetColor above put the default
            // background back, so spaces are equivalent to the clear.
            let painted = (ox + row.len()).min(content_right);
            if painted < content_right {
                if self.popup.is_some() {
                    // A masked tail left the terminal's cursor before the box,
                    // not after the last cell of the row.
                    self.blank_span(out, sy, painted, content_right, !need_move)?;
                } else {
                    for sx in painted..content_right {
                        self.front[sy][sx] = RenderCell::default();
                    }
                    queue!(out, Print(" ".repeat(content_right - painted)))?;
                }
            }
        }

        // Blank any content rows below the frame. When the composite frame has
        // fewer rows than the content area, stale content (e.g. a doubled status
        // bar) would otherwise persist below it. Bounded to the content rect on
        // every side: `Clear(ClearType::FromCursorDown)` would wipe a bottom
        // sidebar entirely, and even a per-row clear-to-EOL would eat a right
        // sidebar's cells on these rows.
        let first_blank = (oy + cells.len()).min(self.rows as usize);
        if first_blank < content_bottom && ox < content_right {
            queue!(out, ResetColor)?;
            for sy in first_blank..content_bottom {
                if self.popup.is_some() {
                    self.blank_span(out, sy, ox, content_right, false)?;
                    continue;
                }
                queue!(out, MoveTo(self.origin_x, sy as u16))?;
                queue!(out, Print(" ".repeat(content_right - ox)))?;
                for sx in ox..content_right {
                    self.front[sy][sx] = RenderCell::default();
                }
            }
        }

        // Update cursor. The reported position is content-relative, so it has
        // to be offset -- otherwise the hardware cursor lands in a left sidebar.
        self.queue_cursor(out, cursor_x, cursor_y, cursor_visible, cursor_style)?;

        // Blit into the front buffer at the origin. NEVER `self.front = cells.to_vec()`
        // -- the front buffer is the FULL terminal, including sidebar columns, and
        // replacing it would both resize it and destroy every panel cell.
        for (y, row) in cells.iter().enumerate() {
            let sy = y + oy;
            if sy >= content_bottom {
                break;
            }
            for (x, cell) in row.iter().enumerate() {
                let sx = x + ox;
                if sx >= content_right {
                    break;
                }
                if self.masked(sx, sy, cell.width) {
                    continue;
                }
                self.front[sy][sx] = cell.clone();
            }
        }

        // End synchronized output.
        queue!(out, Print("\x1b[?2026l"))?;

        Ok(())
    }

    /// Blank `x0..x1` on screen row `sy`, leaving what the popup box covers
    /// alone. `positioned` says the terminal's cursor is already on `x0`.
    fn blank_span<W: Write>(
        &mut self,
        out: &mut W,
        sy: usize,
        x0: usize,
        x1: usize,
        positioned: bool,
    ) -> Result<()> {
        queue!(out, ResetColor)?;
        let mut at_cursor = positioned;
        for sx in x0..x1 {
            if self.masked(sx, sy, 1) {
                at_cursor = false;
                continue;
            }
            if !at_cursor {
                queue!(out, MoveTo(sx as u16, sy as u16))?;
                at_cursor = true;
            }
            queue!(out, Print(' '))?;
            self.front[sy][sx] = RenderCell::default();
        }
        Ok(())
    }

    /// Apply a diff render (only changed cells).
    pub fn render_diff(
        &mut self,
        changes: &[CellChange],
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
    ) -> Result<()> {
        log::debug!(
            "renderer: render_diff changes={} cursor=({},{})",
            changes.len(),
            cursor_x,
            cursor_y
        );
        let mut stdout = io::stdout().lock();

        // Bracket the whole frame in synchronized output (DEC 2026) so the
        // outer terminal displays it atomically instead of tearing.
        queue!(stdout, Print("\x1b[?2026h"))?;

        queue!(stdout, cursor::Hide)?;

        // Same bound as render_full's paint loop and blit: a diff built for a
        // previous, larger geometry must be clipped to the content rect rather
        // than the terminal, or it writes over panel cells and persists that
        // into the front buffer.
        let (content_right, content_bottom) = self.content_edges();

        for change in changes {
            if self.status_row_owned && change.y as usize + 1 == self.frame_rows {
                if let Some(slot) = self.frame_status_row.get_mut(change.x as usize) {
                    *slot = change.cell.clone();
                }
                continue;
            }
            // Change coordinates are content-relative; translate them to the
            // screen and drop anything that falls outside the content rect.
            let sx = change.x as usize + self.origin_x as usize;
            let sy = change.y as usize + self.origin_y as usize;
            if sx >= content_right || sy >= content_bottom {
                continue;
            }
            // The popup box covers this cell: neither emit nor store it.
            if self.masked(sx, sy, change.cell.width) {
                continue;
            }

            // Continuation cells (width 0) must not be drawn: printing a space
            // there would erase half the wide glyph the lead already painted.
            // The front buffer is still updated below so later scrolls carry the
            // correct width and don't re-print stale content.
            if change.cell.width != 0 {
                queue!(
                    stdout,
                    MoveTo(sx as u16, sy as u16),
                    SetForegroundColor(cell_color_to_crossterm(&change.cell.fg)),
                    SetBackgroundColor(cell_color_to_crossterm(&change.cell.bg)),
                )?;

                // Each change repositions the cursor, so there is no reliable
                // previous-cell attribute state to diff against. Emit the attribute
                // state absolutely (on OR explicit off) per change so a non-bold
                // cell following a bold one renders correctly regardless of how the
                // terminal treats the trailing ResetColor.
                if change.cell.bold {
                    queue!(stdout, SetAttribute(Attribute::Bold))?;
                } else {
                    queue!(stdout, SetAttribute(Attribute::NormalIntensity))?;
                }
                if change.cell.italic {
                    queue!(stdout, SetAttribute(Attribute::Italic))?;
                } else {
                    queue!(stdout, SetAttribute(Attribute::NoItalic))?;
                }
                if change.cell.underline {
                    queue!(stdout, SetAttribute(Attribute::Underlined))?;
                } else {
                    queue!(stdout, SetAttribute(Attribute::NoUnderline))?;
                }

                // Each change repositions the cursor, so there is no per-row
                // hyperlink state to diff against. Wrap just this cell: open the
                // link before Print and close it after so it can't leak to the
                // next moved-cursor position.
                if let Some(uri) = &change.cell.hyperlink {
                    queue!(
                        stdout,
                        Print(format!("\x1b]8;;{}\x1b\\", printable_text(uri)))
                    )?;
                }

                queue!(stdout, Print(printable(change.cell.c)))?;
                // Combining marks compose onto the base glyph just printed.
                for m in &change.cell.combining {
                    queue!(stdout, Print(printable(*m)))?;
                }
                if change.cell.hyperlink.is_some() {
                    queue!(stdout, Print("\x1b]8;;\x1b\\"))?;
                }
                queue!(stdout, ResetColor)?;
            }

            // Update front buffer (always, even for skipped continuation cells).
            if sy < self.front.len() && sx < self.front[sy].len() {
                self.front[sy][sx] = change.cell.clone();
            }
        }

        // Update cursor. The reported position is content-relative, so it has
        // to be offset -- otherwise the hardware cursor lands in a left sidebar.
        self.queue_cursor(
            &mut stdout,
            cursor_x,
            cursor_y,
            cursor_visible,
            cursor_style,
        )?;

        // End synchronized output.
        queue!(stdout, Print("\x1b[?2026l"))?;

        Ok(())
    }

    /// Apply a scroll render: shift front buffer within a pane rect and render
    /// only the new rows. Much faster than render_full for scroll events.
    #[allow(clippy::too_many_arguments)]
    pub fn render_scroll(
        &mut self,
        pane_x: u16,
        pane_y: u16,
        pane_width: u16,
        pane_height: u16,
        delta: i16,
        new_rows: &[Vec<RenderCell>],
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
    ) -> Result<()> {
        log::debug!(
            "renderer: render_scroll delta={} pane={}x{} at ({},{})",
            delta,
            pane_width,
            pane_height,
            pane_x,
            pane_y
        );
        // The server withholds scroll renders from a client that holds a
        // popup layer, because the shift would move the pane under the box.
        if self.popup.is_some() {
            log::warn!("renderer: ignoring a scroll render while the popup layer is up");
            return Ok(());
        }
        let mut stdout = io::stdout().lock();
        queue!(stdout, cursor::Hide)?;

        // `pane_x`/`pane_y` arrive content-relative; the front buffer is the
        // full terminal, so translate them to screen coordinates once here.
        let px = pane_x as usize + self.origin_x as usize;
        let py = pane_y as usize + self.origin_y as usize;
        let pw = pane_width as usize;
        let ph = pane_height as usize;
        let abs_delta = delta.unsigned_abs() as usize;
        // Bound every write and emission below to the content rect: a stale
        // pane rect from a pre-resize scroll event must not shift or repaint
        // panel cells. `ph` and `abs_delta` are deliberately NOT clamped --
        // `ph` is load-bearing arithmetic for the `delta < 0` insertion point
        // `py + ph - abs_delta + i` and for the `abs_delta >= ph` guard, so
        // clamping it would move where new rows land.
        let (content_right, content_bottom) = self.content_edges();

        if abs_delta == 0 || abs_delta >= ph {
            // Nothing to shift or entire pane replaced — caller should use render_full
            return Ok(());
        }

        // Bracket the whole frame in synchronized output (DEC 2026) so the
        // outer terminal displays it atomically instead of tearing. Emitted
        // after the early-return guard above so begin/end stay balanced.
        queue!(stdout, Print("\x1b[?2026h"))?;

        // 1. Shift front buffer rows within the pane rect
        if delta > 0 {
            // Content moves UP: shift rows up, new rows appear at top
            for row in (abs_delta..ph).rev() {
                let src_y = py + row - abs_delta;
                let dst_y = py + row;
                if dst_y < content_bottom && src_y < content_bottom {
                    for col in 0..pw {
                        let src_x = px + col;
                        if src_x < content_right {
                            self.front[dst_y][src_x] = self.front[src_y][src_x].clone();
                        }
                    }
                }
            }
        } else {
            // Content moves DOWN: shift rows down, new rows appear at bottom
            for row in 0..ph.saturating_sub(abs_delta) {
                let src_y = py + row + abs_delta;
                let dst_y = py + row;
                if dst_y < content_bottom && src_y < content_bottom {
                    for col in 0..pw {
                        let src_x = px + col;
                        if src_x < content_right {
                            self.front[dst_y][src_x] = self.front[src_y][src_x].clone();
                        }
                    }
                }
            }
        }

        // 2. Write new rows into front buffer
        for (i, row_cells) in new_rows.iter().enumerate() {
            let screen_y = if delta > 0 {
                py + i // New rows at top of pane
            } else {
                py + ph - abs_delta + i // New rows at bottom of pane
            };

            if screen_y >= content_bottom {
                continue;
            }

            for (col, cell) in row_cells.iter().enumerate() {
                // Bound to the pane width, matching step 3's re-render. A server
                // row longer than the pane would otherwise write front cells
                // past the pane that step 3 never repaints -- and at a non-zero
                // origin those cells can belong to a right sidebar.
                if col >= pw {
                    break;
                }
                let screen_x = px + col;
                if screen_x < content_right {
                    self.front[screen_y][screen_x] = cell.clone();
                }
            }
        }

        // 3. Re-render the entire pane rect from the (now correct) front buffer.
        // The front buffer was shifted + new rows inserted, so it has the right
        // content. We must re-render all pane rows because the terminal doesn't
        // know the content shifted.
        for row in 0..ph {
            let screen_y = py + row;
            if screen_y >= content_bottom {
                break;
            }

            queue!(
                stdout,
                MoveTo(
                    px.min(content_right.saturating_sub(1)) as u16,
                    screen_y as u16
                )
            )?;

            let mut last_fg = CellColor::Default;
            let mut last_bg = CellColor::Default;
            let mut last_bold = false;
            let mut last_italic = false;
            let mut last_underline = false;
            let mut last_hyperlink: Option<String> = None;

            // Full SGR reset (SGR 0): see render_full. Matches the per-row
            // last_* = Default/false assumption so a non-underlined leading cell
            // is not left underlined by a previous row's trailing state.
            queue!(stdout, SetAttribute(Attribute::Reset))?;

            for col in 0..pw {
                let screen_x = px + col;
                if screen_x >= content_right {
                    break;
                }
                let cell = if screen_x < self.front[screen_y].len() {
                    &self.front[screen_y][screen_x]
                } else {
                    continue;
                };

                // Continuation cell of a wide glyph: the preceding width-2 Print
                // already advanced the physical cursor by 2, so skip it.
                if cell.width == 0 {
                    continue;
                }

                if cell.fg != last_fg {
                    queue!(
                        stdout,
                        SetForegroundColor(cell_color_to_crossterm(&cell.fg))
                    )?;
                    last_fg = cell.fg.clone();
                }
                if cell.bg != last_bg {
                    queue!(
                        stdout,
                        SetBackgroundColor(cell_color_to_crossterm(&cell.bg))
                    )?;
                    last_bg = cell.bg.clone();
                }
                if cell.bold != last_bold {
                    if cell.bold {
                        queue!(stdout, SetAttribute(Attribute::Bold))?;
                    } else {
                        queue!(stdout, SetAttribute(Attribute::NormalIntensity))?;
                    }
                    last_bold = cell.bold;
                }
                if cell.italic != last_italic {
                    if cell.italic {
                        queue!(stdout, SetAttribute(Attribute::Italic))?;
                    } else {
                        queue!(stdout, SetAttribute(Attribute::NoItalic))?;
                    }
                    last_italic = cell.italic;
                }
                if cell.underline != last_underline {
                    if cell.underline {
                        queue!(stdout, SetAttribute(Attribute::Underlined))?;
                    } else {
                        queue!(stdout, SetAttribute(Attribute::NoUnderline))?;
                    }
                    last_underline = cell.underline;
                }
                if cell.hyperlink.as_deref() != last_hyperlink.as_deref() {
                    match &cell.hyperlink {
                        Some(uri) => queue!(
                            stdout,
                            Print(format!("\x1b]8;;{}\x1b\\", printable_text(uri)))
                        )?,
                        None => queue!(stdout, Print("\x1b]8;;\x1b\\"))?,
                    }
                    last_hyperlink = cell.hyperlink.clone();
                }
                queue!(stdout, Print(printable(cell.c)))?;
                // Combining marks compose onto the base glyph just printed.
                for m in &cell.combining {
                    queue!(stdout, Print(printable(*m)))?;
                }
            }

            // Close any open hyperlink so links never span rows on the wire.
            if last_hyperlink.is_some() {
                queue!(stdout, Print("\x1b]8;;\x1b\\"))?;
            }
            queue!(stdout, ResetColor)?;
        }

        // 3. Update cursor. The reported position is content-relative, so it
        // has to be offset -- otherwise the hardware cursor lands in a left
        // sidebar.
        self.queue_cursor(
            &mut stdout,
            cursor_x,
            cursor_y,
            cursor_visible,
            cursor_style,
        )?;

        // End synchronized output.
        queue!(stdout, Print("\x1b[?2026l"))?;

        Ok(())
    }

    /// Write a panel's grid into the front buffer at `rect` and emit it.
    ///
    /// Panels are NOT overlays: they go into the front buffer so a later
    /// `render_full` or `repaint_all` reproduces them instead of erasing them.
    /// Extra rows/columns in `cells` are ignored; a `cells` smaller than `rect`
    /// simply leaves the remainder untouched.
    pub fn paint_panel(&mut self, rect: Rect, cells: &[Vec<RenderCell>]) -> Result<()> {
        let mut stdout = io::stdout().lock();
        self.paint_panel_into(&mut stdout, rect, cells)
    }

    /// The body of [`Renderer::paint_panel`], parameterised over the sink.
    ///
    /// Splitting it out lets a test capture the emitted bytes and prove that
    /// what is *emitted* matches what is *stored* in the front buffer. That
    /// equality is the whole point: `render_full` honours every attribute it
    /// finds in the front buffer, so any attribute stored but not emitted would
    /// make the panel change appearance the first time `repaint_all` runs.
    fn paint_panel_into<W: Write>(
        &mut self,
        out: &mut W,
        rect: Rect,
        cells: &[Vec<RenderCell>],
    ) -> Result<()> {
        queue!(out, Print("\x1b[?2026h"))?;
        queue!(out, cursor::Hide)?;

        for (ry, row) in cells.iter().enumerate() {
            let sy = rect.y as usize + ry;
            if ry >= rect.height as usize || sy >= self.rows as usize {
                break;
            }
            // Same reset discipline as render_full: without it the previous
            // row's attributes bleed into this panel's leading cells.
            queue!(out, SetAttribute(Attribute::Reset))?;
            queue!(out, MoveTo(rect.x, sy as u16))?;

            let mut last_fg = CellColor::Default;
            let mut last_bg = CellColor::Default;
            let mut last_bold = false;
            let mut last_italic = false;
            let mut last_underline = false;
            let mut last_hyperlink: Option<String> = None;
            // Set after a skipped run, so the next emitted cell is preceded by
            // the move that puts the terminal's cursor back on it.
            let mut need_move = false;

            for (rx, cell) in row.iter().enumerate() {
                let sx = rect.x as usize + rx;
                if rx >= rect.width as usize || sx >= self.cols as usize {
                    break;
                }
                // The popup box covers this cell: neither emit nor store it.
                if self.masked(sx, sy, cell.width) {
                    need_move = true;
                    continue;
                }
                if cell.width == 0 {
                    self.front[sy][sx] = cell.clone();
                    continue;
                }
                if need_move {
                    queue!(out, MoveTo(sx as u16, sy as u16))?;
                    need_move = false;
                }
                if cell.fg != last_fg {
                    queue!(out, SetForegroundColor(cell_color_to_crossterm(&cell.fg)))?;
                    last_fg = cell.fg.clone();
                }
                if cell.bg != last_bg {
                    queue!(out, SetBackgroundColor(cell_color_to_crossterm(&cell.bg)))?;
                    last_bg = cell.bg.clone();
                }
                // The attribute set is emitted exactly as render_full would
                // emit it, because render_full is what replays this row out of
                // the front buffer on the next repaint.
                if cell.bold != last_bold {
                    if cell.bold {
                        queue!(out, SetAttribute(Attribute::Bold))?;
                    } else {
                        queue!(out, SetAttribute(Attribute::NormalIntensity))?;
                    }
                    last_bold = cell.bold;
                }
                if cell.italic != last_italic {
                    if cell.italic {
                        queue!(out, SetAttribute(Attribute::Italic))?;
                    } else {
                        queue!(out, SetAttribute(Attribute::NoItalic))?;
                    }
                    last_italic = cell.italic;
                }
                if cell.underline != last_underline {
                    if cell.underline {
                        queue!(out, SetAttribute(Attribute::Underlined))?;
                    } else {
                        queue!(out, SetAttribute(Attribute::NoUnderline))?;
                    }
                    last_underline = cell.underline;
                }
                if cell.hyperlink.as_deref() != last_hyperlink.as_deref() {
                    match &cell.hyperlink {
                        Some(uri) => {
                            queue!(out, Print(format!("\x1b]8;;{}\x1b\\", printable_text(uri))))?
                        }
                        None => queue!(out, Print("\x1b]8;;\x1b\\"))?,
                    }
                    last_hyperlink = cell.hyperlink.clone();
                }
                queue!(out, Print(printable(cell.c)))?;
                // Combining marks are zero-width; they compose onto the base
                // glyph just printed.
                for m in &cell.combining {
                    queue!(out, Print(printable(*m)))?;
                }
                self.front[sy][sx] = cell.clone();
            }
            // Close any open hyperlink so links never span rows, then reset at
            // end of row so the panel's styling cannot bleed into the first
            // content column.
            if last_hyperlink.is_some() {
                queue!(out, Print("\x1b]8;;\x1b\\"))?;
            }
            queue!(out, SetAttribute(Attribute::Reset))?;
        }

        // Painting a panel must be cursor-neutral. The `Hide` above and the
        // server frame's `Show` are queued into the SAME flush, so leaving the
        // cursor hidden here would hide it permanently for as long as a sidebar
        // is visible -- the headline symptom being a shell with no cursor.
        self.queue_remembered_cursor(out)?;

        queue!(out, Print("\x1b[?2026l"))?;
        Ok(())
    }

    /// Repaint the ENTIRE front buffer from (0, 0), ignoring the content
    /// origin.
    ///
    /// `render_full` writes at the origin, so it can no longer be used to
    /// restore the whole screen. Overlay teardown must call this instead, or
    /// the sidebars are erased.
    pub fn repaint_all(&mut self) -> Result<()> {
        let saved = (
            self.origin_x,
            self.origin_y,
            self.content_cols,
            self.content_rows,
        );
        // `render_full(.., false, ..)` below would otherwise record "hidden" as
        // the server's last reported cursor, and a later `paint_panel` would
        // faithfully restore that. Overlay teardown already re-shows the cursor
        // via `restore_cursor`; this keeps the memory pointing at the truth.
        let saved_cursor = self.last_cursor;
        // The front buffer's last row is whatever is on the terminal's last
        // row, which is exactly what must be put back, not a server bar row to
        // withhold. `frame_rows` is restored too, because the diffs that follow
        // still describe the last server frame.
        let saved_status = (self.status_row_owned, self.frame_rows);
        // The front buffer never holds the popup box, so it is replayed whole
        // and the box is put back on top afterwards.
        let popup = self.popup.take();
        self.status_row_owned = false;
        // The frame here IS the whole terminal, so the content rect must be the
        // whole terminal too -- otherwise the clears would treat the panel
        // columns as stale remainder and blank them.
        self.origin_x = 0;
        self.origin_y = 0;
        self.content_cols = self.cols;
        self.content_rows = self.rows;
        let cells = self.front.clone();
        let res = self.render_full(&cells, 0, 0, false, 0);
        self.origin_x = saved.0;
        self.origin_y = saved.1;
        self.content_cols = saved.2;
        self.content_rows = saved.3;
        self.last_cursor = saved_cursor;
        (self.status_row_owned, self.frame_rows) = saved_status;
        self.popup = popup;
        let mut stdout = io::stdout().lock();
        self.paint_popup_into(&mut stdout, None)?;
        res
    }

    /// Flush all queued render commands to the terminal.
    /// Call this after all render methods for a frame are done.
    pub fn flush(&self) -> Result<()> {
        io::stdout().flush()?;
        Ok(())
    }

    /// Resize the renderer to new dimensions.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        log::debug!("renderer: resize cols={} rows={}", cols, rows);
        self.cols = cols;
        self.rows = rows;
        // The content rect falls back to the whole terminal until the caller
        // recomputes it for the new size; the origin is left alone.
        self.content_cols = cols;
        self.content_rows = rows;
        self.status_row_owned = false;
        // The box was placed for the old size; the next frame brings a layer
        // for the new one.
        self.popup = None;
        self.front = vec![vec![RenderCell::default(); cols as usize]; rows as usize];
        // Clear the terminal to avoid stale content from old layout.
        let mut stdout = io::stdout().lock();
        let _ = crossterm::execute!(stdout, terminal::Clear(terminal::ClearType::All));
    }

    /// Get the current terminal size.
    pub fn terminal_size() -> Result<(u16, u16)> {
        let (cols, rows) = terminal::size()?;
        Ok((cols, rows))
    }

    /// Render a which-key popup overlay on top of the current screen.
    pub fn render_whichkey_overlay(&self, commands: &[DrawCommand]) -> Result<()> {
        let mut stdout = io::stdout().lock();

        queue!(stdout, cursor::Hide)?;

        for cmd in commands {
            if cmd.x >= self.cols || cmd.y >= self.rows {
                continue;
            }
            queue!(
                stdout,
                MoveTo(cmd.x, cmd.y),
                SetForegroundColor(crossterm_color_from_style(cmd.fg)),
                SetBackgroundColor(crossterm_color_from_style(cmd.bg)),
            )?;
            // Truncate text to not exceed screen width.
            let max_chars = (self.cols - cmd.x) as usize;
            let text: String = cmd.text.chars().take(max_chars).map(printable).collect();
            queue!(stdout, Print(text), ResetColor)?;
        }

        Ok(())
    }

    /// Render visual mode selection highlighting and cursor on top of the
    /// current front buffer. All coordinates are offset by the pane's position
    /// in the composited buffer (`pane_offset_x`, `pane_offset_y`) and clamped
    /// to the pane bounds.
    pub fn render_visual_overlay(&self, visual_state: &VisualState) -> Result<()> {
        let mut stdout = io::stdout().lock();

        // Bracket the whole frame in synchronized output (DEC 2026) so the
        // outer terminal displays it atomically instead of tearing.
        queue!(stdout, Print("\x1b[?2026h"))?;

        queue!(stdout, cursor::Hide)?;

        // The pane offset arrives CONTENT-relative (the server composited the
        // frame into the content rect), while `self.front` and every `MoveTo`
        // below are ABSOLUTE. Offsetting here fixes the whole function at once:
        // the front-buffer repaint, the selection highlight, and the block
        // cursor all derive from these two values.
        let (origin_x, origin_y) = self.rect_origin();
        let pane_ox = visual_state.pane_offset_x.saturating_add(origin_x);
        let pane_oy = visual_state.pane_offset_y.saturating_add(origin_y);
        let pane_w = visual_state.visible_cols;
        let pane_h = visual_state.visible_rows;

        // Repaint the pane region from the front buffer before drawing the
        // selection/cursor highlights. This function runs both after a server
        // frame (where render_full/diff/scroll already refreshed the physical
        // screen, so this is a harmless no-op) and standalone on in-view cursor
        // moves (where no server frame refreshes the buffer). Repainting here
        // restores each cell to its true content so the previous cursor cell
        // and any previous selection cells are cleared — the cursor leaves no
        // trail when it moves within the visible area.
        for pane_y in 0..pane_h {
            let screen_y = pane_oy as usize + pane_y;
            if screen_y >= self.front.len() || screen_y >= self.rows as usize {
                break;
            }
            queue!(stdout, MoveTo(pane_ox, screen_y as u16), ResetColor)?;
            let mut last_bold = false;
            let mut last_italic = false;
            let mut last_underline = false;
            let mut last_hyperlink: Option<String> = None;
            for pane_x in 0..pane_w {
                let screen_x = pane_ox as usize + pane_x;
                if screen_x >= self.cols as usize {
                    break;
                }
                let Some(cell) = self.cell_at(screen_x, screen_y) else {
                    break;
                };
                // Skip the continuation cell of a wide glyph: the preceding
                // width-2 Print already advanced the physical cursor by 2.
                if cell.width == 0 {
                    continue;
                }
                queue!(
                    stdout,
                    SetForegroundColor(cell_color_to_crossterm(&cell.fg)),
                    SetBackgroundColor(cell_color_to_crossterm(&cell.bg)),
                )?;
                if cell.bold != last_bold {
                    let attr = if cell.bold {
                        Attribute::Bold
                    } else {
                        Attribute::NormalIntensity
                    };
                    queue!(stdout, SetAttribute(attr))?;
                    last_bold = cell.bold;
                }
                if cell.italic != last_italic {
                    let attr = if cell.italic {
                        Attribute::Italic
                    } else {
                        Attribute::NoItalic
                    };
                    queue!(stdout, SetAttribute(attr))?;
                    last_italic = cell.italic;
                }
                if cell.underline != last_underline {
                    let attr = if cell.underline {
                        Attribute::Underlined
                    } else {
                        Attribute::NoUnderline
                    };
                    queue!(stdout, SetAttribute(attr))?;
                    last_underline = cell.underline;
                }
                if cell.hyperlink.as_deref() != last_hyperlink.as_deref() {
                    match &cell.hyperlink {
                        Some(uri) => queue!(
                            stdout,
                            Print(format!("\x1b]8;;{}\x1b\\", printable_text(uri)))
                        )?,
                        None => queue!(stdout, Print("\x1b]8;;\x1b\\"))?,
                    }
                    last_hyperlink = cell.hyperlink.clone();
                }
                queue!(stdout, Print(printable(cell.c)))?;
                // Combining marks compose onto the base glyph just printed.
                for m in &cell.combining {
                    queue!(stdout, Print(printable(*m)))?;
                }
            }
            // Close any open hyperlink so links never span rows on the wire.
            if last_hyperlink.is_some() {
                queue!(stdout, Print("\x1b]8;;\x1b\\"))?;
            }
            queue!(stdout, ResetColor)?;
        }

        let selection_range = visual_state.selection_range();
        let is_line_mode = visual_state.selection_mode == SelectionMode::Line;

        // Determine which pane-relative rows are selected.
        if let Some(((start_row, start_col), (end_row, end_col))) = selection_range {
            let base = visual_state
                .total_lines
                .saturating_sub(visual_state.scroll_offset + pane_h);

            for pane_y in 0..pane_h {
                let scrollback_row = base + pane_y;
                if scrollback_row < start_row || scrollback_row > end_row {
                    continue;
                }

                // Map pane-relative row to screen row.
                let screen_y = pane_oy as usize + pane_y;
                if screen_y >= self.front.len() || screen_y >= self.rows as usize {
                    continue;
                }

                let col_start = if is_line_mode || scrollback_row > start_row {
                    0
                } else {
                    start_col
                };
                let col_end = if is_line_mode || scrollback_row < end_row {
                    pane_w
                } else {
                    end_col + 1
                };

                for col in col_start..col_end.min(pane_w) {
                    let screen_x = pane_ox as usize + col;
                    if screen_x >= self.cols as usize {
                        break;
                    }
                    let Some(cell) = self.cell_at(screen_x, screen_y) else {
                        break;
                    };
                    // Skip the continuation cell of a wide glyph: the preceding
                    // width-2 lead was printed inverted and already covers both
                    // physical columns, so printing here would misalign the
                    // highlight. Mirrors render_full/render_diff width-0 handling.
                    if cell.width == 0 {
                        continue;
                    }

                    let fg = if cell.bg == CellColor::Default {
                        Color::Black
                    } else {
                        cell_color_to_crossterm(&cell.bg)
                    };
                    let bg = if cell.fg == CellColor::Default {
                        Color::White
                    } else {
                        cell_color_to_crossterm(&cell.fg)
                    };

                    queue!(
                        stdout,
                        MoveTo(screen_x as u16, screen_y as u16),
                        SetForegroundColor(fg),
                        SetBackgroundColor(bg),
                    )?;
                    if cell.bold {
                        queue!(stdout, SetAttribute(Attribute::Bold))?;
                    }
                    queue!(stdout, Print(printable(cell.c)))?;
                    // Combining marks compose onto the base glyph just printed.
                    for m in &cell.combining {
                        queue!(stdout, Print(printable(*m)))?;
                    }
                    queue!(stdout, ResetColor)?;
                }
            }
        }

        // Render cursor as a block highlight at the cursor position (pane-relative).
        let cursor_screen_col = pane_ox + visual_state.cursor_col as u16;
        let cursor_screen_row = pane_oy + visual_state.cursor_row as u16;

        if cursor_screen_row < self.rows && cursor_screen_col < self.cols {
            let row_idx = cursor_screen_row as usize;
            let col_idx = cursor_screen_col as usize;
            if let Some(cell) = self.cell_at(col_idx, row_idx) {
                let is_in_selection = selection_range.is_some_and(|_| true);

                if selection_range.is_none() || !is_in_selection {
                    let fg = if cell.bg == CellColor::Default {
                        Color::Black
                    } else {
                        cell_color_to_crossterm(&cell.bg)
                    };
                    let bg = if cell.fg == CellColor::Default {
                        Color::White
                    } else {
                        cell_color_to_crossterm(&cell.fg)
                    };
                    queue!(
                        stdout,
                        MoveTo(cursor_screen_col, cursor_screen_row),
                        SetForegroundColor(fg),
                        SetBackgroundColor(bg),
                        Print(printable(cell.c)),
                    )?;
                    // Combining marks compose onto the base glyph just printed.
                    for m in &cell.combining {
                        queue!(stdout, Print(printable(*m)))?;
                    }
                    queue!(stdout, ResetColor)?;
                }
            }
        }

        // End synchronized output.
        queue!(stdout, Print("\x1b[?2026l"))?;

        Ok(())
    }

    /// Extract text from the front buffer for the given visual selection.
    ///
    /// Selection coordinates are pane-relative. The front buffer is read at
    /// `(pane_offset_x + col, pane_offset_y + row)` to map from pane-local
    /// coordinates to the composited screen buffer.
    pub fn extract_text(&self, visual_state: &VisualState) -> String {
        let selection = match visual_state.selection_range() {
            Some(range) => range,
            None => return String::new(),
        };
        let ((start_row, start_col), (end_row, end_col)) = selection;
        let is_line_mode = visual_state.selection_mode == SelectionMode::Line;

        // Content-relative in, absolute out: `self.front` is the full terminal,
        // so without the origin a copy would lift text out of a sidebar panel.
        let (origin_x, origin_y) = self.rect_origin();
        let pane_ox = visual_state.pane_offset_x.saturating_add(origin_x) as usize;
        let pane_oy = visual_state.pane_offset_y.saturating_add(origin_y) as usize;
        let pane_h = visual_state.visible_rows;
        let pane_w = visual_state.visible_cols;

        let base = visual_state
            .total_lines
            .saturating_sub(visual_state.scroll_offset + pane_h);

        let mut result = String::new();

        for pane_y in 0..pane_h {
            let scrollback_row = base + pane_y;
            if scrollback_row < start_row || scrollback_row > end_row {
                continue;
            }

            let screen_y = pane_oy + pane_y;
            if screen_y >= self.front.len() {
                continue;
            }
            let row_len = self.front[screen_y].len();

            // Extract only the pane's columns from the composited row.
            let pane_row_len = pane_w.min(row_len.saturating_sub(pane_ox));
            let pane_row: Vec<&RenderCell> = (0..pane_row_len)
                .filter_map(|c| self.cell_at(pane_ox + c, screen_y))
                .collect();

            // Collect glyph chars, skipping the width-0 continuation cell that
            // follows a wide glyph (its char is a blank placeholder). A wide
            // glyph still occupies 2 physical columns, so column slicing uses
            // the physical column indices; only the char collection skips the
            // continuation so `中文` yields "中文" and not "中 文".
            if is_line_mode {
                let line: String = pane_row
                    .iter()
                    .filter(|c| c.width != 0)
                    .flat_map(|c| std::iter::once(c.c).chain(c.combining.iter().copied()))
                    .collect();
                result.push_str(line.trim_end());
                result.push('\n');
            } else if start_row == end_row {
                let cs = start_col.min(pane_row.len());
                let ce = (end_col + 1).min(pane_row.len());
                let text: String = pane_row[cs..ce]
                    .iter()
                    .filter(|c| c.width != 0)
                    .flat_map(|c| std::iter::once(c.c).chain(c.combining.iter().copied()))
                    .collect();
                result.push_str(text.trim_end());
            } else if scrollback_row == start_row {
                let cs = start_col.min(pane_row.len());
                let text: String = pane_row[cs..]
                    .iter()
                    .filter(|c| c.width != 0)
                    .flat_map(|c| std::iter::once(c.c).chain(c.combining.iter().copied()))
                    .collect();
                result.push_str(text.trim_end());
                result.push('\n');
            } else if scrollback_row == end_row {
                let ce = (end_col + 1).min(pane_row.len());
                let text: String = pane_row[..ce]
                    .iter()
                    .filter(|c| c.width != 0)
                    .flat_map(|c| std::iter::once(c.c).chain(c.combining.iter().copied()))
                    .collect();
                result.push_str(text.trim_end());
            } else {
                let text: String = pane_row
                    .iter()
                    .filter(|c| c.width != 0)
                    .flat_map(|c| std::iter::once(c.c).chain(c.combining.iter().copied()))
                    .collect();
                result.push_str(text.trim_end());
                result.push('\n');
            }
        }

        result
    }

    /// Render a rename popup overlay centered on the screen.
    pub fn render_rename_popup(
        &self,
        text: &str,
        target: &str,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        use crossterm::style;

        let mut stdout = io::stdout().lock();

        // Calculate popup dimensions
        let popup_width = 40u16.min(cols.saturating_sub(4));
        let popup_height = 3u16;
        let start_x = (cols.saturating_sub(popup_width)) / 2;
        let start_y = (rows.saturating_sub(popup_height)) / 2;

        // Title like "Rename Tab" or "Rename Pane"
        let title = format!(" Rename {} ", target);

        // Draw top border
        queue!(stdout, MoveTo(start_x, start_y))?;
        let title_len = title.len();
        let border_fill = (popup_width as usize).saturating_sub(title_len + 2);
        let half_left = border_fill / 2;
        let half_right = border_fill - half_left;
        // (A second, titleless copy of this line used to be built here and
        // immediately discarded with `let _ = top_border`. It is gone.)
        let top_with_title = box_top_line_titled(half_left, &title, half_right);
        queue!(stdout, style::SetAttribute(style::Attribute::Bold))?;
        queue!(stdout, Print(&top_with_title))?;
        queue!(stdout, style::SetAttribute(style::Attribute::Reset))?;

        // Draw middle row with text input
        queue!(stdout, MoveTo(start_x, start_y + 1))?;
        let inner_width = popup_width.saturating_sub(4) as usize;
        let display_text = if text.len() > inner_width {
            &text[text.len() - inner_width..]
        } else {
            text
        };
        let padding = inner_width.saturating_sub(display_text.len());
        queue!(
            stdout,
            Print(format!(
                "{BOX_VERTICAL} {}{} {BOX_VERTICAL}",
                printable_text(display_text),
                " ".repeat(padding)
            ))
        )?;

        // Draw bottom border
        queue!(stdout, MoveTo(start_x, start_y + 2))?;
        queue!(
            stdout,
            Print(box_bottom_line(popup_width.saturating_sub(2) as usize))
        )?;

        // Position cursor at end of text
        let cursor_x = start_x + 2 + display_text.len() as u16;
        queue!(stdout, MoveTo(cursor_x, start_y + 1), cursor::Show)?;

        Ok(())
    }

    /// Render a command palette overlay on top of the current screen.
    /// Reuses the same mechanism as `render_whichkey_overlay`.
    pub fn render_command_palette_overlay(&self, commands: &[DrawCommand]) -> Result<()> {
        self.render_whichkey_overlay(commands)
    }

    /// Clear the command palette overlay by re-rendering the front buffer.
    pub fn clear_command_palette_overlay(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.clear_overlay(cols, rows)
    }

    /// Render a search prompt overlay at the bottom of the screen (above the
    /// status bar). Shows `/query_` during prompt phase, `/query (x/y)` during
    /// navigation phase.
    pub fn render_search_prompt(
        &self,
        query: &str,
        phase: crate::client::input::SearchPhase,
        match_info: Option<(usize, usize)>,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        let mut stdout = io::stdout().lock();

        // Draw on the second-to-last row (above status bar).
        let prompt_row = rows.saturating_sub(2);

        // Build the prompt string.
        let prompt = match phase {
            crate::client::input::SearchPhase::Prompt => {
                format!("/{query}")
            }
            crate::client::input::SearchPhase::Navigation => {
                if let Some((current, total)) = match_info {
                    format!("/{query} ({}/{})", current + 1, total)
                } else {
                    format!("/{query}")
                }
            }
        };

        let max_len = cols as usize;
        let display: String = prompt.chars().take(max_len).map(printable).collect();
        let padding = max_len.saturating_sub(display.len());

        queue!(stdout, cursor::Hide)?;
        queue!(stdout, MoveTo(0, prompt_row))?;
        queue!(
            stdout,
            SetForegroundColor(Color::Black),
            SetBackgroundColor(Color::AnsiValue(11)), // Bright yellow
        )?;
        queue!(stdout, Print(&display))?;

        // Fill remaining with spaces in the same bg color.
        if padding > 0 {
            queue!(stdout, Print(" ".repeat(padding)))?;
        }

        queue!(stdout, ResetColor)?;

        // Show cursor at the end of the query during prompt phase.
        if phase == crate::client::input::SearchPhase::Prompt {
            let cursor_x = (display.len() as u16).min(cols.saturating_sub(1));
            queue!(stdout, MoveTo(cursor_x, prompt_row), cursor::Show)?;
        }

        Ok(())
    }

    /// Render search match highlights on top of the current front buffer.
    ///
    /// Highlights all visible matches with a subtle background, and the
    /// current match with a bright background. Match positions are in
    /// scrollback coordinates; only those within the visible area of the
    /// focused pane are drawn.
    #[allow(clippy::too_many_arguments)]
    pub fn render_search_highlight(
        &self,
        matches: &[(usize, usize)],
        current_match: usize,
        query_len: usize,
        viewport_top: usize,
        pane_rect: Option<&crate::protocol::PaneRect>,
        theme: &crate::config::theme::Theme,
    ) -> Result<()> {
        let pr = match pane_rect {
            Some(pr) => pr,
            None => return Ok(()),
        };
        if matches.is_empty() || query_len == 0 {
            return Ok(());
        }

        let pane_h = pr.height as usize;
        if pane_h == 0 {
            return Ok(());
        }
        // `pr` is the server's pane rect, i.e. content-relative; every screen
        // coordinate derived from it below must be absolute. `viewport_top` is
        // deliberately NOT offset -- it is a scrollback LINE index compared
        // against match line numbers, not a screen coordinate.
        let (origin_x, origin_y) = self.rect_origin();
        let pane_x = pr.x.saturating_add(origin_x) as usize;
        let pane_y = pr.y.saturating_add(origin_y) as usize;

        // The visible line range in scrollback coordinates.
        let visible_start = viewport_top;
        let visible_end = viewport_top + pane_h;

        let mut stdout = io::stdout().lock();
        queue!(stdout, cursor::Hide)?;

        for (idx, &(line, col)) in matches.iter().enumerate() {
            if line < visible_start || line >= visible_end {
                continue;
            }

            let screen_y = pane_y + (line - visible_start);
            let screen_x_start = pane_x + col;

            // Choose colors: bright for current match, subtle for others.
            let (hl_fg, hl_bg) = if idx == current_match {
                (theme.search_current_fg, theme.search_current_bg)
            } else {
                (theme.search_match_fg, theme.search_match_bg)
            };

            for offset in 0..query_len {
                let screen_x = screen_x_start + offset;
                if screen_x >= self.cols as usize || screen_y >= self.rows as usize {
                    break;
                }
                // Also clamp to pane content area.
                if screen_x >= pane_x + pr.width as usize || screen_y >= pane_y + pr.height as usize
                {
                    break;
                }

                let cell_char = self.cell_at(screen_x, screen_y).map_or(' ', |c| c.c);

                queue!(
                    stdout,
                    MoveTo(screen_x as u16, screen_y as u16),
                    SetForegroundColor(hl_fg),
                    SetBackgroundColor(hl_bg),
                    Print(printable(cell_char)),
                    ResetColor,
                )?;
            }
        }

        Ok(())
    }

    /// Get a reference to the front buffer (for testing/inspection).
    pub fn front_buffer(&self) -> &[Vec<RenderCell>] {
        &self.front
    }

    /// Clear the overlay by re-rendering the whole front buffer.
    ///
    /// Delegates to `repaint_all`: `render_full` writes at the content origin,
    /// so using it here would re-blit the full-terminal buffer offset by the
    /// origin and erase the sidebars.
    pub fn clear_overlay(&mut self, cols: u16, rows: u16) -> Result<()> {
        let _ = (cols, rows); // suppress unused warnings
        self.repaint_all()
    }

    /// Restore the hardware cursor to a known terminal position and visibility.
    ///
    /// `clear_overlay` re-renders the front buffer via `render_full(.., false, ..)`,
    /// which hardcodes the cursor HIDDEN at (0,0). When an overlay (visual/search)
    /// is torn down and no server frame follows to repaint the cursor, call this
    /// AFTER `clear_overlay` to put the cursor back where the last server frame
    /// reported it. Queues the operation; the caller must `flush()`.
    pub fn restore_cursor(&mut self, x: u16, y: u16, visible: bool) -> Result<()> {
        // The caller is asserting where the cursor now is, so record it: a
        // `paint_panel` after this must restore THIS state, not the one the
        // last server frame reported. The style is left as remembered -- this
        // entry point never carried one.
        //
        // While the popup layer is up the caller holds window-absolute
        // coordinates (see `last_cursor`).
        let absolute = self.popup.is_some();
        self.last_cursor = (x, y, visible, self.last_cursor.3, absolute);
        let mut stdout = io::stdout().lock();
        if visible {
            // Without a popup `x`/`y` are the last server-reported position,
            // i.e. content-relative: offset them or the cursor lands in a
            // sidebar.
            let (sx, sy) = self.cursor_screen_pos(x, y, absolute);
            queue!(stdout, MoveTo(sx, sy), cursor::Show)?;
        } else {
            queue!(stdout, cursor::Hide)?;
        }
        Ok(())
    }
}

/// Convert a crossterm `Color` (from the theme/draw commands) to crossterm `Color`.
/// This is an identity conversion since `DrawCommand` already uses crossterm `Color`.
fn crossterm_color_from_style(color: Color) -> Color {
    color
}

// ---------------------------------------------------------------------------
// Color conversion
// ---------------------------------------------------------------------------

/// Convert a DECSCUSR cursor style number to a crossterm `SetCursorStyle`.
fn cursor_style_command(style: u8) -> crossterm::cursor::SetCursorStyle {
    match style {
        1 => crossterm::cursor::SetCursorStyle::BlinkingBlock,
        2 => crossterm::cursor::SetCursorStyle::SteadyBlock,
        3 => crossterm::cursor::SetCursorStyle::BlinkingUnderScore,
        4 => crossterm::cursor::SetCursorStyle::SteadyUnderScore,
        5 => crossterm::cursor::SetCursorStyle::BlinkingBar,
        6 => crossterm::cursor::SetCursorStyle::SteadyBar,
        _ => crossterm::cursor::SetCursorStyle::DefaultUserShape,
    }
}

/// Convert a protocol `CellColor` to a crossterm `Color`.
pub(crate) fn cell_color_to_crossterm(color: &CellColor) -> Color {
    match color {
        CellColor::Default => Color::Reset,
        CellColor::Indexed(idx) => Color::AnsiValue(*idx),
        CellColor::Rgb(r, g, b) => Color::Rgb {
            r: *r,
            g: *g,
            b: *b,
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_control_character_is_never_printed() {
        assert_eq!(super::printable('\u{1b}'), ' ');
        assert_eq!(super::printable('\u{7}'), ' ');
        assert_eq!(super::printable('\u{9b}'), ' ', "C1 CSI");
        assert_eq!(super::printable('a'), 'a');
        assert_eq!(
            super::printable_text("h\u{1b}]52;c;AA==\u{7}i"),
            "h]52;c;AA==i"
        );
    }

    #[test]
    fn test_new_renderer() {
        let renderer = Renderer::new(80, 24);
        assert_eq!(renderer.cols, 80);
        assert_eq!(renderer.rows, 24);
        assert_eq!(renderer.front.len(), 24);
        assert_eq!(renderer.front[0].len(), 80);
    }

    #[test]
    fn test_resize() {
        let mut renderer = Renderer::new(80, 24);
        renderer.resize(120, 40);
        assert_eq!(renderer.cols, 120);
        assert_eq!(renderer.rows, 40);
        assert_eq!(renderer.front.len(), 40);
        assert_eq!(renderer.front[0].len(), 120);
    }

    #[test]
    fn test_cell_color_conversion() {
        assert!(matches!(
            cell_color_to_crossterm(&CellColor::Default),
            Color::Reset
        ));
        assert!(matches!(
            cell_color_to_crossterm(&CellColor::Indexed(5)),
            Color::AnsiValue(5)
        ));
        assert!(matches!(
            cell_color_to_crossterm(&CellColor::Rgb(10, 20, 30)),
            Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            }
        ));
    }

    /// Helper to create a renderer with text content in the front buffer.
    fn renderer_with_text(lines: &[&str], cols: u16, rows: u16) -> Renderer {
        let mut renderer = Renderer::new(cols, rows);
        for (y, line) in lines.iter().enumerate() {
            if y >= rows as usize {
                break;
            }
            for (x, ch) in line.chars().enumerate() {
                if x >= cols as usize {
                    break;
                }
                renderer.front[y][x] = RenderCell {
                    c: ch,
                    ..RenderCell::default()
                };
            }
        }
        renderer
    }

    #[test]
    fn test_extract_text_no_selection() {
        let renderer = renderer_with_text(&["hello", "world"], 10, 5);
        let vs = VisualState::new(5, 5);
        // No selection active.
        let text = renderer.extract_text(&vs);
        assert_eq!(text, "");
    }

    #[test]
    fn test_extract_text_char_single_line() {
        let renderer = renderer_with_text(&["hello world"], 20, 5);
        let mut vs = VisualState::with_cols(5, 5, 20);
        // Position cursor at row 0, col 0.
        vs.cursor_row = 0;
        vs.cursor_col = 0;
        vs.start_char_selection();
        // Move cursor to col 4 (select "hello").
        vs.cursor_col = 4;
        let text = renderer.extract_text(&vs);
        assert_eq!(text, "hello");
    }

    #[test]
    fn test_extract_text_line_mode() {
        let renderer = renderer_with_text(&["line one", "line two", "line three"], 20, 5);
        let mut vs = VisualState::with_cols(5, 5, 20);
        // Position at row 0.
        vs.cursor_row = 0;
        vs.cursor_col = 0;
        vs.start_line_selection();
        // Move to row 1 to select 2 lines.
        vs.cursor_row = 1;
        let text = renderer.extract_text(&vs);
        assert_eq!(text, "line one\nline two\n");
    }

    #[test]
    fn test_extract_text_wide_glyph_no_interior_space() {
        // Build a front buffer containing two wide glyphs "中文". Each wide
        // glyph is a width-2 lead cell followed by a width-0 continuation cell
        // (a blank placeholder). Layout: col0='中'(w2) col1=' '(w0)
        // col2='文'(w2) col3=' '(w0), remaining cells are default spaces.
        let mut renderer = Renderer::new(10, 1);
        renderer.front[0][0] = RenderCell {
            c: '中',
            width: 2,
            ..RenderCell::default()
        };
        renderer.front[0][1] = RenderCell {
            c: ' ',
            width: 0,
            ..RenderCell::default()
        };
        renderer.front[0][2] = RenderCell {
            c: '文',
            width: 2,
            ..RenderCell::default()
        };
        renderer.front[0][3] = RenderCell {
            c: ' ',
            width: 0,
            ..RenderCell::default()
        };

        let mut vs = VisualState::with_cols(1, 1, 10);
        // Select physical columns 0..=3, which span both wide glyphs and their
        // continuation cells.
        vs.cursor_row = 0;
        vs.cursor_col = 0;
        vs.start_char_selection();
        vs.cursor_col = 3;

        let text = renderer.extract_text(&vs);
        // The continuation cells must be skipped: no stray interior/trailing
        // space between the glyphs.
        assert_eq!(text, "中文");
    }

    #[test]
    fn test_extract_text_char_multi_line() {
        let renderer = renderer_with_text(&["AAABBB", "CCCDDD", "EEEFFFGGG"], 10, 5);
        let mut vs = VisualState::with_cols(5, 5, 10);
        // Start at row 0, col 3.
        vs.cursor_row = 0;
        vs.cursor_col = 3;
        vs.start_char_selection();
        // End at row 1, col 2 (select "BBB\nCCC").
        vs.cursor_row = 1;
        vs.cursor_col = 2;
        let text = renderer.extract_text(&vs);
        assert_eq!(text, "BBB\nCCC");
    }
}

#[cfg(test)]
mod origin_tests {
    use super::*;
    use crate::protocol::CellColor;
    use crate::server::layout::Rect;

    fn cell(c: char) -> RenderCell {
        RenderCell {
            c,
            fg: CellColor::Default,
            bg: CellColor::Default,
            bold: false,
            italic: false,
            underline: false,
            width: 1,
            combining: Vec::new(),
            hyperlink: None,
        }
    }

    fn grid(text: &str, rows: usize) -> Vec<Vec<RenderCell>> {
        (0..rows)
            .map(|_| text.chars().map(cell).collect::<Vec<_>>())
            .collect()
    }

    /// A 20x6 terminal with a left sidebar 5 wide: the content rect is 15x6,
    /// and the client draws the status bar, so the frame's last row is not
    /// painted and the content rows end one row short.
    fn owned() -> Renderer {
        let mut r = Renderer::new(20, 6);
        r.set_origin(5, 0);
        r.set_content_size(15, 5);
        r.set_status_row_owned(true);
        r
    }

    #[test]
    fn an_owned_status_row_is_withheld_from_the_screen_and_kept() {
        let mut r = owned();
        let mut frame = grid("pane", 5);
        frame.push("BAR".chars().map(cell).collect());
        r.render_full(&frame, 0, 0, false, 0).unwrap();
        assert_eq!(r.front_buffer()[4][5].c, 'p', "the panes are painted");
        assert!(
            r.front_buffer()[5].iter().all(|c| c.c == ' '),
            "the server's bar must not reach the last row"
        );
        let kept: String = r.frame_status_row().iter().map(|c| c.c).collect();
        assert_eq!(kept, "BAR");
    }

    #[test]
    fn a_diff_to_an_owned_status_row_updates_the_kept_row_only() {
        let mut r = owned();
        let mut frame = grid("pane", 5);
        frame.push("BAR".chars().map(cell).collect());
        r.render_full(&frame, 0, 0, false, 0).unwrap();
        r.render_diff(
            &[CellChange {
                y: 5,
                x: 1,
                cell: cell('X'),
            }],
            0,
            0,
            false,
            0,
        )
        .unwrap();
        let kept: String = r.frame_status_row().iter().map(|c| c.c).collect();
        assert_eq!(kept, "BXR");
        assert!(r.front_buffer()[5].iter().all(|c| c.c == ' '));
    }

    /// A frame sized to a smaller client on the same session is shorter than
    /// the content rect. Its bar row must not land mid-screen.
    #[test]
    fn a_short_frames_bar_row_is_withheld_too() {
        let mut r = owned();
        let mut frame = grid("pane", 2);
        frame.push("BAR".chars().map(cell).collect());
        r.render_full(&frame, 0, 0, false, 0).unwrap();
        assert_eq!(r.front_buffer()[1][5].c, 'p');
        assert!(
            r.front_buffer()[2].iter().all(|c| c.c == ' '),
            "row 2 is where the short frame's bar would have been painted"
        );
    }

    #[test]
    fn without_ownership_the_frames_last_row_is_painted_as_before() {
        let mut r = Renderer::new(20, 6);
        let mut frame = grid("pane", 5);
        frame.push("BAR".chars().map(cell).collect());
        r.render_full(&frame, 0, 0, false, 0).unwrap();
        let last: String = r.front_buffer()[5][..3].iter().map(|c| c.c).collect();
        assert_eq!(last, "BAR");
    }

    /// `repaint_all` replays the front buffer, whose last row is the client's
    /// own bar. Withholding it there would blank the bar on every overlay
    /// teardown.
    #[test]
    fn repaint_all_puts_the_terminals_last_row_back() {
        let mut r = owned();
        let bar = Rect {
            x: 0,
            y: 5,
            width: 20,
            height: 1,
        };
        r.paint_panel(bar, &grid("CLIENTBAR", 1)).unwrap();
        r.repaint_all().unwrap();
        let last: String = r.front_buffer()[5][..9].iter().map(|c| c.c).collect();
        assert_eq!(last, "CLIENTBAR");
        let mut frame = grid("pane", 5);
        frame.push("BAR".chars().map(cell).collect());
        r.render_full(&frame, 0, 0, false, 0).unwrap();
        assert_eq!(
            r.front_buffer()[5][0].c,
            'C',
            "ownership survives the repaint"
        );
    }

    #[test]
    fn origin_defaults_to_zero() {
        let r = Renderer::new(80, 24);
        assert_eq!(r.origin(), (0, 0));
    }

    #[test]
    fn render_full_writes_the_front_buffer_at_the_origin() {
        let mut r = Renderer::new(20, 4);
        r.set_origin(5, 1);
        r.render_full(&grid("abc", 2), 0, 0, false, 0).unwrap();
        let front = r.front_buffer();
        // Columns 0..5 of row 1 are untouched; the content starts at column 5.
        assert_eq!(front[1][4].c, ' ');
        assert_eq!(front[1][5].c, 'a');
        assert_eq!(front[1][6].c, 'b');
        assert_eq!(front[1][7].c, 'c');
        // Row 0 is above the content origin and stays blank.
        assert!(front[0].iter().all(|c| c.c == ' '));
    }

    #[test]
    fn render_full_clips_content_that_would_overflow_the_terminal() {
        let mut r = Renderer::new(10, 3);
        r.set_origin(8, 2);
        // 5 wide x 3 tall at origin (8,2) in a 10x3 terminal: only 2 columns
        // and 1 row fit. Must clip, not panic.
        r.render_full(&grid("vwxyz", 3), 0, 0, false, 0).unwrap();
        let front = r.front_buffer();
        assert_eq!(front[2][8].c, 'v');
        assert_eq!(front[2][9].c, 'w');
    }

    #[test]
    fn render_diff_applies_the_origin_to_change_coordinates() {
        let mut r = Renderer::new(20, 4);
        r.set_origin(5, 1);
        r.render_full(&grid("abc", 2), 0, 0, false, 0).unwrap();
        let changes = vec![CellChange {
            y: 0,
            x: 1,
            cell: cell('Z'),
        }];
        r.render_diff(&changes, 0, 0, false, 0).unwrap();
        // Server-relative (1,0) is screen (6,1).
        assert_eq!(r.front_buffer()[1][6].c, 'Z');
    }

    #[test]
    fn render_diff_drops_changes_that_fall_outside_the_terminal() {
        let mut r = Renderer::new(10, 3);
        r.set_origin(8, 2);
        let changes = vec![CellChange {
            y: 5,
            x: 5,
            cell: cell('Q'),
        }];
        // Must not panic and must not write anywhere.
        r.render_diff(&changes, 0, 0, false, 0).unwrap();
        assert!(r
            .front_buffer()
            .iter()
            .all(|row| row.iter().all(|c| c.c != 'Q')));
    }

    #[test]
    fn paint_panel_writes_into_the_front_buffer_so_it_survives_a_repaint() {
        let mut r = Renderer::new(20, 4);
        r.set_origin(5, 0);
        let panel = grid("SIDE", 4);
        r.paint_panel(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            &panel,
        )
        .unwrap();
        assert_eq!(r.front_buffer()[0][0].c, 'S');
        assert_eq!(r.front_buffer()[3][3].c, 'E');
    }

    #[test]
    fn a_content_render_does_not_erase_a_painted_panel() {
        // The bug this whole design exists to avoid: overlay-style painting
        // would be wiped by the next full render.
        let mut r = Renderer::new(20, 4);
        r.set_origin(5, 0);
        let panel = grid("SIDE", 4);
        r.paint_panel(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            &panel,
        )
        .unwrap();
        r.render_full(&grid("xyz", 4), 0, 0, false, 0).unwrap();
        assert_eq!(
            r.front_buffer()[0][0].c,
            'S',
            "panel was erased by render_full"
        );
        assert_eq!(r.front_buffer()[0][5].c, 'x');
    }

    #[test]
    fn paint_panel_clips_a_rect_that_runs_past_the_terminal() {
        let mut r = Renderer::new(6, 2);
        let panel = grid("abcdefgh", 4);
        r.paint_panel(
            Rect {
                x: 4,
                y: 1,
                width: 8,
                height: 4,
            },
            &panel,
        )
        .unwrap();
        assert_eq!(r.front_buffer()[1][4].c, 'a');
        assert_eq!(r.front_buffer()[1][5].c, 'b');
    }

    #[test]
    fn paint_panel_ignores_a_grid_smaller_than_its_rect() {
        let mut r = Renderer::new(20, 4);
        // A misbehaving plugin returning too few rows must not panic.
        r.paint_panel(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            &grid("ab", 1),
        )
        .unwrap();
        assert_eq!(r.front_buffer()[0][0].c, 'a');
    }

    #[test]
    fn repaint_all_preserves_both_panel_and_content_cells() {
        let mut r = Renderer::new(20, 4);
        r.set_origin(5, 0);
        r.paint_panel(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            &grid("SIDE", 4),
        )
        .unwrap();
        r.render_full(&grid("xyz", 4), 0, 0, false, 0).unwrap();
        r.repaint_all().unwrap();
        assert_eq!(r.front_buffer()[0][0].c, 'S');
        assert_eq!(r.front_buffer()[0][5].c, 'x');
    }

    #[test]
    fn render_full_does_not_shrink_the_front_buffer() {
        // Guards hazard (a): `self.front = cells.to_vec()` would resize the
        // front buffer to the content frame and destroy the sidebar columns.
        let mut r = Renderer::new(20, 4);
        r.set_origin(5, 0);
        r.render_full(&grid("xyz", 4), 0, 0, false, 0).unwrap();
        assert_eq!(r.front_buffer().len(), 4, "front buffer lost rows");
        assert_eq!(r.front_buffer()[0].len(), 20, "front buffer lost columns");
    }

    #[test]
    fn a_narrow_frame_does_not_clear_a_right_sidebar() {
        // Guards hazard (b): the end-of-row clear must stop at the content
        // rect's right edge, not the terminal's.
        let mut r = Renderer::new(20, 2);
        // Content is columns 0..14; a right sidebar occupies 14..20.
        r.paint_panel(
            Rect {
                x: 14,
                y: 0,
                width: 6,
                height: 2,
            },
            &grid("RIGHT!", 2),
        )
        .unwrap();
        r.set_origin(0, 0);
        r.set_content_size(14, 2);
        // A frame narrower than the content rect: 3 columns of a 14-wide area.
        r.render_full(&grid("abc", 2), 0, 0, false, 0).unwrap();
        assert_eq!(r.front_buffer()[0][14].c, 'R', "right sidebar was cleared");
        assert_eq!(r.front_buffer()[0][19].c, '!', "right sidebar was cleared");
    }

    #[test]
    fn a_short_frame_does_not_clear_a_bottom_sidebar() {
        // Guards hazard (c): the below-frame clear must stop at the content
        // rect's bottom edge, not the terminal's.
        let mut r = Renderer::new(10, 6);
        r.paint_panel(
            Rect {
                x: 0,
                y: 4,
                width: 10,
                height: 2,
            },
            &grid("BOTTOMBOTT", 2),
        )
        .unwrap();
        r.set_origin(0, 0);
        r.set_content_size(10, 4);
        // Frame is 2 rows in a 4-row content area, terminal is 6 rows.
        r.render_full(&grid("abcdefghij", 2), 0, 0, false, 0)
            .unwrap();
        assert_eq!(r.front_buffer()[4][0].c, 'B', "bottom sidebar was cleared");
        assert_eq!(r.front_buffer()[5][9].c, 'T', "bottom sidebar was cleared");
    }

    #[test]
    fn render_diff_drops_changes_that_fall_outside_the_content_rect() {
        // Same class as the oversized-frame case, on the diff path: a change
        // built for a previous, larger geometry must not write into a panel.
        let mut r = Renderer::new(20, 4);
        r.paint_panel(
            Rect {
                x: 12,
                y: 0,
                width: 8,
                height: 4,
            },
            &grid("PANELPAN", 4),
        )
        .unwrap();
        r.set_origin(0, 0);
        r.set_content_size(12, 4);
        let changes = vec![CellChange {
            y: 0,
            x: 15,
            cell: cell('Q'),
        }];
        r.render_diff(&changes, 0, 0, false, 0).unwrap();
        assert_eq!(r.front_buffer()[0][15].c, 'E', "diff wrote into the panel");
    }

    #[test]
    fn an_oversized_frame_is_clipped_to_the_content_rect_not_the_terminal() {
        // Guards Important 1. A frame LARGER than the content rect is
        // reachable: a resize race can deliver an in-flight server frame built
        // for the previous, bigger geometry. Bounding the paint loop and the
        // blit by the terminal would let it paint straight over the panels and
        // persist that corruption into the front buffer, where `repaint_all`
        // would reproduce it faithfully.
        let mut r = Renderer::new(20, 6);
        // Left panel on columns 0..4, right panel on 16..20, bottom on rows 4..6.
        r.paint_panel(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            &grid("LEFT", 4),
        )
        .unwrap();
        r.paint_panel(
            Rect {
                x: 16,
                y: 0,
                width: 4,
                height: 4,
            },
            &grid("RGHT", 4),
        )
        .unwrap();
        r.paint_panel(
            Rect {
                x: 0,
                y: 4,
                width: 20,
                height: 2,
            },
            &grid(&"B".repeat(20), 2),
        )
        .unwrap();
        // Content rect is columns 4..16, rows 0..4.
        r.set_origin(4, 0);
        r.set_content_size(12, 4);
        // A frame from the pre-resize geometry: 20x6 into a 12x4 content rect.
        r.render_full(&grid(&"X".repeat(20), 6), 0, 0, false, 0)
            .unwrap();
        let front = r.front_buffer();
        assert_eq!(front[0][0].c, 'L', "left panel was painted over");
        assert_eq!(front[0][16].c, 'R', "right panel was painted over");
        assert_eq!(front[0][19].c, 'T', "right panel was painted over");
        assert_eq!(front[4][0].c, 'B', "bottom panel was painted over");
        assert_eq!(front[5][19].c, 'B', "bottom panel was painted over");
        // The content rect itself is fully painted.
        assert_eq!(front[0][4].c, 'X');
        assert_eq!(front[3][15].c, 'X');
    }

    #[test]
    fn a_scroll_whose_pane_runs_past_the_content_rect_leaves_panels_intact() {
        // A stale pane rect from a pre-resize scroll event must be clipped to
        // the content rect. Every write and emission is bounded, while `ph`,
        // `abs_delta` and the delta arithmetic -- the `abs_delta >= ph`
        // early-return and the `delta < 0` insertion point -- are untouched.
        let mut r = Renderer::new(10, 6);
        // Right panel on columns 6..10, bottom panel on rows 4..6.
        r.paint_panel(
            Rect {
                x: 6,
                y: 0,
                width: 4,
                height: 4,
            },
            &grid("RGHT", 4),
        )
        .unwrap();
        r.paint_panel(
            Rect {
                x: 0,
                y: 4,
                width: 10,
                height: 2,
            },
            &grid("BBBBBBBBBB", 2),
        )
        .unwrap();
        // Content rect is columns 0..6, rows 0..4.
        r.set_origin(0, 0);
        r.set_content_size(6, 4);
        // Seed the content area with DISTINCT rows. A uniform fill (this test
        // used to seed "CCCCCC" on every row) cannot tell a correct one-row
        // shift from no shift at all: every post-shift assertion would hold
        // even if the rows never moved.
        let seeded: Vec<Vec<RenderCell>> = ["000000", "111111", "222222", "333333"]
            .iter()
            .map(|row| row.chars().map(cell).collect())
            .collect();
        r.render_full(&seeded, 0, 0, false, 0).unwrap();
        // A stale pane rect: 10x6, the whole pre-resize terminal.
        let new_row = grid("NNNNNNNNNN", 1);
        r.render_scroll(0, 0, 10, 6, 1, &new_row, 0, 0, false, 0)
            .unwrap();
        let front = r.front_buffer();
        assert_eq!(front[0][6].c, 'R', "scroll wrote into the right panel");
        assert_eq!(front[0][9].c, 'T', "scroll wrote into the right panel");
        assert_eq!(front[4][0].c, 'B', "scroll shifted into the bottom panel");
        assert_eq!(front[5][9].c, 'B', "scroll shifted into the bottom panel");
        // The content rect still scrolled: the new row landed at its top AND
        // every seeded row moved down exactly one. These are the assertions the
        // uniform seed could not make.
        assert_eq!(front[0][0].c, 'N', "new row at the top of the content rect");
        assert_eq!(front[0][5].c, 'N');
        assert_eq!(front[1][0].c, '0', "row 0 shifted down to row 1");
        assert_eq!(front[2][0].c, '1', "row 1 shifted down to row 2");
        assert_eq!(front[3][0].c, '2', "row 2 shifted down to row 3");
        assert_eq!(front[3][5].c, '2', "the shift covers the content width");
    }

    #[test]
    fn render_scroll_does_not_write_new_row_cells_past_the_pane() {
        // Guards Minor 3. Step 2 wrote the full length of each new row while
        // step 3's re-render stops at the pane width, so a server row longer
        // than the pane left front cells past it that were never repainted --
        // and at a non-zero origin those cells belong to a right sidebar.
        let mut r = Renderer::new(10, 4);
        r.paint_panel(
            Rect {
                x: 4,
                y: 0,
                width: 6,
                height: 4,
            },
            &grid("PANEL!", 4),
        )
        .unwrap();
        r.set_content_size(4, 4);
        // The pane is 4 columns wide; the new row is 10 cells long.
        let new_row = grid("NNNNNNNNNN", 1);
        r.render_scroll(0, 0, 4, 4, 1, &new_row, 0, 0, false, 0)
            .unwrap();
        let front = r.front_buffer();
        assert_eq!(front[0][0].c, 'N');
        assert_eq!(front[0][3].c, 'N');
        assert_eq!(front[0][4].c, 'P', "wrote past the pane into the panel");
        assert_eq!(front[0][9].c, '!', "wrote past the pane into the panel");
    }

    #[test]
    fn paint_panel_emits_every_attribute_it_stores() {
        // Guards Important 2. `render_full` honours bold/italic/underline/
        // hyperlink out of the front buffer, so an attribute that `paint_panel`
        // stores but never emits makes the panel silently change appearance the
        // first time an overlay teardown calls `repaint_all`. Capture the bytes
        // and prove emitted == stored.
        let mut r = Renderer::new(10, 1);
        let styled = RenderCell {
            c: 'P',
            bold: true,
            underline: true,
            hyperlink: Some("https://example.invalid".to_string()),
            ..RenderCell::default()
        };
        let mut out: Vec<u8> = Vec::new();
        r.paint_panel_into(
            &mut out,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &[vec![styled.clone()]],
        )
        .unwrap();

        let emitted = String::from_utf8(out).unwrap();
        assert!(
            emitted.contains("\x1b[1m"),
            "bold was stored but not emitted"
        );
        assert!(
            emitted.contains("\x1b[4m"),
            "underline was stored but not emitted"
        );
        assert!(
            emitted.contains("https://example.invalid"),
            "hyperlink was stored but not emitted"
        );

        // ...and the stored cell carries exactly those attributes, so the
        // repaint reproduces the paint rather than restyling it.
        let stored = &r.front_buffer()[0][0];
        assert_eq!(stored, &styled);
    }

    #[test]
    fn repaint_all_keeps_the_cursor_the_last_frame_reported() {
        // `repaint_all` replays the front buffer with `show_cursor = false`,
        // which runs through `queue_cursor` and would otherwise record "hidden"
        // as the server's last reported cursor. The next `paint_panel`
        // faithfully restores whatever is remembered, so without the save/
        // restore in `repaint_all` a panel painted after an overlay teardown
        // hides the cursor -- and with a sidebar configured a panel is painted
        // after every frame, so the shell is left with no cursor.
        //
        // The PTY harness cannot discriminate this: every overlay teardown in
        // `main.rs` is followed either by `restore_cursor` (which rewrites the
        // memory) or by a server frame (ditto) before any panel paints. Pinned
        // here instead, where the emitted bytes are visible.
        let mut r = Renderer::new(10, 2);
        r.render_full(&grid("AAAAAAAAAA", 2), 3, 1, true, 0)
            .unwrap();
        r.clear_overlay(10, 2).unwrap();

        let mut out: Vec<u8> = Vec::new();
        r.paint_panel_into(
            &mut out,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &[vec![RenderCell::default()]],
        )
        .unwrap();

        let emitted = String::from_utf8_lossy(&out).to_string();
        assert!(
            emitted.contains("\x1b[?25h"),
            "a panel painted after an overlay teardown left the cursor hidden: {emitted:?}"
        );
        assert!(
            emitted.contains("\x1b[2;4H"),
            "the restored cursor is not where the last frame put it: {emitted:?}"
        );
    }

    #[test]
    fn a_smaller_frame_still_clears_stale_content_at_the_default_content_size() {
        // The today-behaviour gate. With no sidebars configured -- default
        // content size, origin (0, 0) -- a frame narrower AND shorter than the
        // terminal must still blank the stale region beyond it. That is what
        // `Clear(UntilNewLine)` / `Clear(FromCursorDown)` did for the
        // min-across-clients case (the composite frame is sized to the MIN
        // across attached clients, so a larger client sees a smaller frame),
        // and bounding the clears must not regress it.
        let mut r = Renderer::new(10, 4);
        r.render_full(&grid("SSSSSSSSSS", 4), 0, 0, false, 0)
            .unwrap();
        r.render_full(&grid("abc", 2), 0, 0, false, 0).unwrap();
        let front = r.front_buffer();
        assert_eq!(front[0][0].c, 'a');
        assert_eq!(
            front[0][5].c, ' ',
            "stale content to the right of the frame was not cleared"
        );
        assert_eq!(
            front[3][9].c, ' ',
            "stale content below the frame was not cleared"
        );
    }

    #[test]
    fn resize_resets_the_front_buffer_and_content_rect_but_keeps_the_origin() {
        let mut r = Renderer::new(20, 4);
        r.set_origin(5, 1);
        r.resize(30, 10);
        assert_eq!(r.origin(), (5, 1));
        // The content rect falls back to the whole terminal; the caller must
        // recompute it for the new size.
        assert_eq!(r.content_size(), (30, 10));
        assert_eq!(r.front_buffer().len(), 10);
        assert_eq!(r.front_buffer()[0].len(), 30);
    }

    // -- The popup box --------------------------------------------------------

    fn boxed(width: u16, height: u16, c: char) -> Vec<Vec<RenderCell>> {
        vec![vec![cell(c); width as usize]; height as usize]
    }

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// A 10x4 terminal showing `AAAAAAAAAA` on every row, with a 4x2 popup box
    /// of `P` at (2, 1).
    fn with_box() -> Renderer {
        let mut r = Renderer::new(10, 4);
        r.render_full(&grid("AAAAAAAAAA", 4), 0, 0, false, 0)
            .unwrap();
        let mut sink: Vec<u8> = Vec::new();
        r.set_popup_into(
            &mut sink,
            rect(2, 1, 4, 2),
            boxed(4, 2, 'P'),
            (3, 2, true, 2),
        )
        .unwrap();
        r
    }

    fn front_row(r: &Renderer, y: usize) -> String {
        r.front_buffer()[y].iter().map(|c| c.c).collect()
    }

    /// A terminal fed everything a renderer emits, to check what the user
    /// would see rather than what the renderer believes it painted.
    struct Term(crate::screen::Screen);

    impl Term {
        fn new(cols: u16, rows: u16) -> Term {
            Term(crate::screen::Screen::new(cols, rows, 0))
        }

        fn feed(&mut self, bytes: &[u8]) {
            self.0.process_output(bytes);
        }

        fn row(&self, y: usize) -> String {
            self.0.grid[y]
                .cells
                .iter()
                .filter(|c| c.width != 0)
                .map(|c| c.c)
                .collect()
        }
    }

    /// [`with_box`], with everything it emitted already on a [`Term`].
    fn with_box_on_screen() -> (Renderer, Term) {
        let mut r = Renderer::new(10, 4);
        let mut term = Term::new(10, 4);
        let mut out: Vec<u8> = Vec::new();
        r.render_full_into(&mut out, &grid("AAAAAAAAAA", 4), 0, 0, false, 0)
            .unwrap();
        r.set_popup_into(
            &mut out,
            rect(2, 1, 4, 2),
            boxed(4, 2, 'P'),
            (3, 2, true, 2),
        )
        .unwrap();
        term.feed(&out);
        (r, term)
    }

    #[test]
    fn a_rows_blank_remainder_after_a_masked_tail_lands_past_the_box() {
        let (mut r, mut term) = with_box_on_screen();
        // The frame ends exactly at the box's right edge, so its last cells on
        // rows 1 and 2 are masked and the remainder starts just past the box.
        let mut out: Vec<u8> = Vec::new();
        r.render_full_into(&mut out, &grid("BBBBBB", 4), 0, 0, false, 0)
            .unwrap();
        term.feed(&out);
        assert_eq!(term.row(1), "BBPPPP    ");
        assert_eq!(term.row(2), "BBPPPP    ");
        assert_eq!(term.row(3), "BBBBBB    ");
    }

    #[test]
    fn a_full_render_skips_the_cells_the_box_covers() {
        let mut r = with_box();
        r.render_full(&grid("BBBBBBBBBB", 4), 0, 0, false, 0)
            .unwrap();
        assert_eq!(front_row(&r, 0), "BBBBBBBBBB");
        // Inside the box the front buffer keeps what was there before.
        assert_eq!(front_row(&r, 1), "BBAAAABBBB");
        assert_eq!(front_row(&r, 2), "BBAAAABBBB");
        assert_eq!(front_row(&r, 3), "BBBBBBBBBB");
    }

    #[test]
    fn a_diff_skips_the_cells_the_box_covers() {
        let mut r = with_box();
        let at = |x, y| CellChange {
            x,
            y,
            cell: cell('Z'),
        };
        r.render_diff(&[at(3, 1), at(0, 1), at(6, 2)], 0, 0, false, 0)
            .unwrap();
        assert_eq!(front_row(&r, 1), "ZAAAAAAAAA", "(3,1) is under the box");
        assert_eq!(front_row(&r, 2), "AAAAAAZAAA");
    }

    #[test]
    fn a_panel_paint_skips_the_box_and_moves_the_cursor_past_it() {
        let mut r = with_box();
        let mut out: Vec<u8> = Vec::new();
        r.paint_panel_into(
            &mut out,
            rect(0, 1, 10, 1),
            &[grid("0123456789", 1)[0].clone()],
        )
        .unwrap();
        let emitted = String::from_utf8_lossy(&out).to_string();
        assert_eq!(
            front_row(&r, 1),
            "01AAAA6789",
            "the box's cells are not stored"
        );
        assert!(emitted.contains("01"), "{emitted:?}");
        assert!(
            !emitted.contains("234") && !emitted.contains("345"),
            "{emitted:?}"
        );
        // The terminal's cursor is moved to the first cell after the box
        // (row 2, column 7 in 1-based terms).
        assert!(emitted.contains("\x1b[2;7H6789"), "{emitted:?}");
    }

    #[test]
    fn a_wide_glyph_straddling_the_edge_counts_as_covered() {
        let mut r = with_box();
        let wide = RenderCell {
            c: '\u{4e2d}',
            width: 2,
            ..RenderCell::default()
        };
        let cont = RenderCell {
            c: ' ',
            width: 0,
            ..RenderCell::default()
        };
        // The lead is just left of the box (x = 1), its continuation inside it.
        r.render_diff(
            &[
                CellChange {
                    x: 1,
                    y: 1,
                    cell: wide,
                },
                CellChange {
                    x: 2,
                    y: 1,
                    cell: cont,
                },
            ],
            0,
            0,
            false,
            0,
        )
        .unwrap();
        assert_eq!(front_row(&r, 1), "AAAAAAAAAA");
    }

    #[test]
    fn the_box_owns_the_cursor_in_window_coordinates() {
        let mut r = with_box();
        // A content origin that would shift a content-relative cursor.
        r.set_origin(4, 0);
        r.set_content_size(6, 4);
        r.render_full(&grid("BBBBBB", 4), 50, 50, true, 1).unwrap();
        assert_eq!(r.last_cursor, (3, 2, true, 2, true));
        let mut out: Vec<u8> = Vec::new();
        r.queue_remembered_cursor(&mut out).unwrap();
        let emitted = String::from_utf8_lossy(&out).to_string();
        assert!(
            emitted.contains("\x1b[3;4H"),
            "no origin offset applies: {emitted:?}"
        );
    }

    #[test]
    fn an_absolute_cursor_is_clamped_to_the_terminal() {
        let mut r = Renderer::new(10, 4);
        let mut sink: Vec<u8> = Vec::new();
        r.set_popup_into(
            &mut sink,
            rect(0, 0, 4, 2),
            boxed(4, 2, 'P'),
            (99, 99, true, 0),
        )
        .unwrap();
        r.render_full(&grid("AAAAAAAAAA", 4), 0, 0, false, 0)
            .unwrap();
        let mut out: Vec<u8> = Vec::new();
        r.queue_remembered_cursor(&mut out).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("\x1b[4;10H"),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn dropping_the_layer_lifts_the_mask() {
        let mut r = with_box();
        r.clear_popup();
        assert_eq!(r.popup_rect(), None);
        r.render_full(&grid("BBBBBBBBBB", 4), 0, 0, false, 0)
            .unwrap();
        assert_eq!(front_row(&r, 1), "BBBBBBBBBB");
        assert_eq!(r.rect_origin(), r.origin());
        // The cursor is the frame's again.
        r.render_full(&grid("BBBBBBBBBB", 4), 7, 2, true, 0)
            .unwrap();
        assert_eq!(r.last_cursor, (7, 2, true, 0, false));
    }

    #[test]
    fn a_resize_drops_the_layer() {
        let mut r = with_box();
        r.resize(12, 5);
        assert_eq!(r.popup_rect(), None);
    }

    #[test]
    fn rect_origin_is_zero_while_the_layer_is_up() {
        let mut r = with_box();
        r.set_origin(4, 1);
        assert_eq!(r.origin(), (4, 1));
        assert_eq!(r.rect_origin(), (0, 0));
    }

    #[test]
    fn repaint_all_keeps_the_layer_and_the_front_buffer_intact() {
        let mut r = with_box();
        r.repaint_all().unwrap();
        assert_eq!(r.popup_rect(), Some(rect(2, 1, 4, 2)));
        assert_eq!(front_row(&r, 1), "AAAAAAAAAA", "the box is never stored");
    }

    #[test]
    fn the_box_is_emitted_whole_and_then_only_where_it_changed() {
        let mut r = Renderer::new(10, 4);
        let mut first: Vec<u8> = Vec::new();
        r.set_popup_into(
            &mut first,
            rect(2, 1, 4, 2),
            boxed(4, 2, 'P'),
            (0, 0, false, 0),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&first).matches('P').count(),
            8,
            "the first paint covers the box"
        );
        let mut next = boxed(4, 2, 'P');
        next[1][2] = cell('Q');
        let mut second: Vec<u8> = Vec::new();
        r.set_popup_into(
            &mut second,
            rect(2, 1, 4, 2),
            next.clone(),
            (0, 0, false, 0),
        )
        .unwrap();
        let emitted = String::from_utf8_lossy(&second).to_string();
        assert_eq!(emitted.matches('Q').count(), 1);
        assert_eq!(emitted.matches('P').count(), 0, "unchanged cells stay put");
        // A box that moved is emitted whole again.
        let mut third: Vec<u8> = Vec::new();
        r.set_popup_into(&mut third, rect(3, 1, 4, 2), next, (0, 0, false, 0))
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&third).matches('P').count(), 7);
    }

    #[test]
    fn a_scroll_render_is_ignored_while_the_layer_is_up() {
        let mut r = with_box();
        let before = r.front_buffer().to_vec();
        r.render_scroll(
            0,
            0,
            10,
            4,
            1,
            &[grid("ZZZZZZZZZZ", 1)[0].clone()],
            0,
            0,
            false,
            0,
        )
        .unwrap();
        assert_eq!(r.front_buffer(), before.as_slice());
    }

    #[test]
    fn overlays_that_read_the_screen_read_the_box_where_it_covers() {
        let r = with_box();
        assert_eq!(r.cell_at(3, 1).map(|c| c.c), Some('P'));
        assert_eq!(r.cell_at(0, 1).map(|c| c.c), Some('A'));
        // A copy out of the popup returns the popup's text.
        let mut vs = VisualState::with_cols(2, 2, 4);
        vs.pane_offset_x = 2;
        vs.pane_offset_y = 1;
        vs.cursor_row = 0;
        vs.cursor_col = 0;
        vs.start_char_selection();
        vs.cursor_col = 3;
        assert_eq!(r.extract_text(&vs), "PPPP");
    }

    #[test]
    fn a_blank_fill_under_a_box_leaves_the_box_alone() {
        let mut r = with_box();
        // A frame narrower than the content rect blanks its remainder.
        r.render_full(&grid("BB", 4), 0, 0, false, 0).unwrap();
        assert_eq!(front_row(&r, 0), "BB        ");
        assert_eq!(
            front_row(&r, 1),
            "BBAAAA    ",
            "the box's cells are untouched"
        );
    }
}
