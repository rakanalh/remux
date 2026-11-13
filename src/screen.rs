//! VTE-based terminal screen buffer.
//!
//! This module implements a virtual terminal screen that processes VT100/xterm
//! escape sequences via the `vte` crate. Each pane in the multiplexer owns its
//! own `Screen` instance.

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};

use unicode_width::UnicodeWidthChar;

/// Largest base64 payload accepted from an `OSC 52` clipboard write. Anything
/// bigger is logged and dropped, so a runaway or hostile program cannot balloon
/// the server's memory or wedge the render loop by "copying" a huge blob.
/// 256 KiB of base64 is ~192 KiB of text — far more than any plausible copy.
const MAX_OSC52_BASE64_LEN: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Cell types
// ---------------------------------------------------------------------------

/// A terminal color.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Color {
    /// Use the terminal default foreground or background color.
    #[default]
    Default,
    /// A 256-color palette index (0..=255).
    Indexed(u8),
    /// A 24-bit true color value.
    Rgb(u8, u8, u8),
}

/// Text attributes for a single cell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellAttrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub fg: Color,
    pub bg: Color,
}

/// A single character cell in the terminal grid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    /// Display width in columns: `1` normal, `2` wide lead (CJK/emoji), `0`
    /// continuation cell placed in the column after a wide lead.
    pub width: u8,
    pub attrs: CellAttrs,
    /// Zero-width combining marks (e.g. U+0301 COMBINING ACUTE ACCENT) that
    /// compose onto the base glyph `c`. Empty for the overwhelmingly common
    /// case of a plain glyph.
    pub combining: Vec<char>,
    /// OSC 8 hyperlink target URI for this cell, if any. `None` for the common
    /// case of a non-linked cell.
    pub hyperlink: Option<String>,
    /// Filled by an erase or a scroll rather than written. Such a blank may
    /// carry the current background (back-colour erase). Reflow treats it as
    /// padding when it decides where a line ends, then puts it back: counting
    /// a coloured erase as text would make every erased row as long as the old
    /// width and wrap it when the pane narrows.
    pub erased: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            width: 1,
            attrs: CellAttrs::default(),
            combining: Vec::new(),
            hyperlink: None,
            erased: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------------

/// A single grid row: its cells plus whether the line *continues* onto the next
/// row.
///
/// `wrapped` is set **only** by the DECAWM autowrap path in [`Screen::put_char`]
/// -- i.e. the row physically ran out of columns and the text spilled over. An
/// explicit `LF`/`CR`/`IND` never sets it. That distinction is the whole point:
/// [`Screen::reflow`] rejoins consecutive rows into one logical line while
/// `wrapped` is true, so a stale flag would splice two *unrelated* lines
/// together. Every path that blanks, inserts, deletes or scrolls a row must
/// therefore clear it (`Row::new` makes a default blank row, `Screen::blank_row`
/// one in the current background).
///
/// `Deref`/`DerefMut` to the cell vector keep the (many) `row[c]`, `row.len()`,
/// `row.iter()` call sites reading exactly as they did when a row was a plain
/// `Vec<Cell>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    /// A blank row of `cols` default cells, explicitly *not* wrapped.
    pub fn new(cols: u16) -> Self {
        Row {
            cells: vec![Cell::default(); cols as usize],
            wrapped: false,
        }
    }
}

impl Deref for Row {
    type Target = Vec<Cell>;

    fn deref(&self) -> &Self::Target {
        &self.cells
    }
}

impl DerefMut for Row {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.cells
    }
}

// `for cell in &row` / `&mut row`: `Deref` alone does not cover `for` loops,
// which need `IntoIterator` on the reference itself.
impl<'a> IntoIterator for &'a Row {
    type Item = &'a Cell;
    type IntoIter = std::slice::Iter<'a, Cell>;

    fn into_iter(self) -> Self::IntoIter {
        self.cells.iter()
    }
}

impl<'a> IntoIterator for &'a mut Row {
    type Item = &'a mut Cell;
    type IntoIter = std::slice::IterMut<'a, Cell>;

    fn into_iter(self) -> Self::IntoIter {
        self.cells.iter_mut()
    }
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

/// A virtual terminal screen buffer with scrollback.
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    /// The visible grid, indexed as `grid[row][col]`.
    pub grid: Vec<Row>,
    pub cursor_x: u16,
    pub cursor_y: u16,
    /// The current text attributes applied to newly printed characters.
    pub current_attrs: CellAttrs,
    /// The current OSC 8 hyperlink applied to newly printed characters, if a
    /// link is open. Independent of SGR (not affected by `handle_sgr`).
    pub current_hyperlink: Option<String>,
    /// Lines that have scrolled off the top of the visible area.
    pub scrollback: VecDeque<Row>,
    /// Maximum number of scrollback lines to retain.
    pub scrollback_limit: usize,
    /// Total number of lines that have been permanently evicted off the top of
    /// scrollback (via `scrollback.pop_front()`) over the screen's lifetime.
    /// Combined with the current combined-buffer index it yields an
    /// eviction-stable absolute line id, so a captured selection anchor keeps
    /// pointing at the same content line even as older lines are dropped. See
    /// [`Screen::abs_of_row`] / [`Screen::row_of_abs`].
    pub lines_evicted: usize,
    /// How many of the newest scrollback lines a taller screen may pull back
    /// into view: the lines that scrolled or were resized off the top since the
    /// screen was last cleared. tmux keeps the same count (`hscrolled`), which
    /// is what stops a resize after `clear` from bringing the cleared history
    /// back above the prompt.
    resize_scrolled: usize,
    /// Top of the scroll region (0-based, inclusive).
    pub scroll_top: u16,
    /// Bottom of the scroll region (0-based, inclusive).
    pub scroll_bottom: u16,
    /// The VTE parser instance.
    parser: vte::Parser,
    /// Saved primary screen state for alternate screen buffer switching.
    saved_grid: Option<Vec<Row>>,
    /// The primary screen's scrollback, set aside while the alternate screen is
    /// up. The alternate screen is historyless (tmux and zellij both model it
    /// that way): its own redraws never become scrollback, and the primary
    /// screen's history is not addressable from it -- scrolling a pane running
    /// a full-screen application used to walk into text that has nothing to do
    /// with what is on the screen. Handed back untouched on the way out.
    saved_scrollback: Option<VecDeque<Row>>,
    saved_cursor_x: u16,
    saved_cursor_y: u16,
    saved_attrs: CellAttrs,
    /// Saved hyperlink for alternate-screen switching (mode 1049), mirroring
    /// `saved_attrs`.
    saved_hyperlink: Option<String>,
    saved_scroll_top: u16,
    saved_scroll_bottom: u16,
    /// Whether we are currently on the alternate screen.
    pub alt_screen_active: bool,
    /// Whether the cursor is visible.
    pub cursor_visible: bool,
    /// Cursor style set by the application (DECSCUSR). 0 = default.
    pub cursor_style: u8,
    /// Responses to be written back to the PTY (e.g., DSR cursor position replies).
    pub pty_responses: Vec<Vec<u8>>,
    /// Saved cursor position for CSI s/u and DECSC/DECRC (separate from alt screen save).
    scp_cursor_x: u16,
    scp_cursor_y: u16,
    /// Saved graphic rendition for DECSC/DECRC (ESC 7 / ESC 8). Not touched by
    /// the SCO save/restore (CSI s/u), which is position-only by convention.
    scp_attrs: CellAttrs,
    /// Saved hyperlink for DECSC/DECRC (ESC 7 / ESC 8), mirroring `scp_attrs`.
    scp_hyperlink: Option<String>,
    /// Whether renders are locked (Synchronized Update mode, CSI ? 2026 h/l).
    pub lock_renders: bool,
    /// DECCKM: application cursor keys mode (CSI ? 1 h/l).
    pub application_cursor_keys: bool,
    /// Bracketed paste mode (CSI ? 2004 h/l): whether the program in this pane
    /// has ASKED to have pasted text wrapped in `ESC[200~` / `ESC[201~`.
    ///
    /// Bracketed paste is a handshake, and this is the half remux was missing.
    /// The client used to wrap unconditionally, so a raw-mode reader that never
    /// asked got 12 bytes of garbage around the text -- vim read the leading
    /// `ESC` as the Escape key and ran `[200~` as normal-mode commands.
    ///
    /// It lives here beside DECCKM rather than beside the saved grid because
    /// 2004 is a TERMINAL-level mode -- what the terminal does to input -- not a
    /// property of the screen buffer, so it deliberately does not follow the
    /// alt-screen switch (1047/1049).
    pub bracketed_paste: bool,
    /// Whether the application has enabled mouse tracking (modes 1000/1002/1003).
    pub mouse_tracking: bool,
    /// Whether the application asked for MOTION reports too (modes 1002
    /// button-event and 1003 any-event), as opposed to plain 1000 which reports
    /// presses and releases only. A drag is forwarded as motion reports only
    /// when this is set, so a 1000-only application is not fed events it never
    /// asked for.
    pub mouse_motion: bool,
    /// Whether the application requested SGR mouse encoding (mode 1006).
    pub mouse_sgr: bool,
    /// Set when a BEL (`0x07`) is received; consumed via [`Screen::take_bell`].
    /// Used by the server for per-tab bell activity monitoring.
    pub bell_pending: bool,
    /// Text an application asked the terminal to put on the system clipboard
    /// with `OSC 52`; consumed via [`Screen::take_clipboard`]. Last write wins —
    /// a batch of PTY output holding several clipboard writes leaves the last
    /// one, exactly as a real terminal would.
    pub clipboard_pending: Option<String>,
    /// The latest `OSC 0` / `OSC 2` window title, sanitised; consumed via
    /// [`Screen::take_title`]. Last write wins within one batch of output.
    title_pending: Option<String>,
    /// Deferred line-wrap (DECAWM "last column") state. When a printable char
    /// fills the last column, the cursor stays parked on that column and this
    /// flag is set; the actual wrap to the next row happens only when the NEXT
    /// printable char arrives. Any explicit cursor move cancels it.
    pending_wrap: bool,
}

impl Screen {
    /// Create a new screen with the given dimensions and scrollback limit.
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        let grid = Self::make_grid(cols, rows);
        Screen {
            cols,
            rows,
            grid,
            cursor_x: 0,
            cursor_y: 0,
            current_attrs: CellAttrs::default(),
            current_hyperlink: None,
            scrollback: VecDeque::new(),
            scrollback_limit,
            lines_evicted: 0,
            resize_scrolled: 0,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            parser: vte::Parser::new(),
            saved_grid: None,
            saved_scrollback: None,
            saved_cursor_x: 0,
            saved_cursor_y: 0,
            saved_attrs: CellAttrs::default(),
            saved_hyperlink: None,
            saved_scroll_top: 0,
            saved_scroll_bottom: rows.saturating_sub(1),
            alt_screen_active: false,
            cursor_visible: true,
            cursor_style: 0,
            pty_responses: Vec::new(),
            scp_cursor_x: 0,
            scp_cursor_y: 0,
            scp_attrs: CellAttrs::default(),
            scp_hyperlink: None,
            lock_renders: false,
            application_cursor_keys: false,
            bracketed_paste: false,
            mouse_tracking: false,
            mouse_motion: false,
            mouse_sgr: false,
            bell_pending: false,
            clipboard_pending: None,
            title_pending: None,
            pending_wrap: false,
        }
    }

    /// Consume the pending-bell flag, returning whether a BEL was received
    /// since the last call and clearing it.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell_pending)
    }

    /// Consume any pending `OSC 52` clipboard write, clearing it.
    ///
    /// The server drains this after every batch of PTY output and hands the text
    /// to the clients that should act on it as a `CopyToClipboard`.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard_pending.take()
    }

    /// Consume the title set since the last call, if any. An empty string is a
    /// real answer: programs clear the title that way when they exit.
    pub fn take_title(&mut self) -> Option<String> {
        self.title_pending.take()
    }

    /// Handle `OSC 52` — an application asking the terminal to put text on the
    /// system clipboard: `OSC 52 ; <targets> ; <base64> ST`.
    ///
    /// Only clipboard **writes** are honored. A payload of `?` is a clipboard
    /// *read* request: the terminal is expected to answer on the PTY with the
    /// clipboard's current contents. Answering it would let any program — or any
    /// file run through `cat` — exfiltrate whatever the user last copied, so
    /// reads are deliberately ignored and never replied to.
    ///
    /// `targets` is xterm's selection list. Empty means "the default selection";
    /// `c` is the clipboard, `p` the primary selection and `s` the configured
    /// selection. Any of those is accepted — they all mean "the thing the user
    /// pastes with", and Remux has exactly one clipboard to offer. A request
    /// aimed only at the numbered cut buffers `0`-`7` has no system-clipboard
    /// equivalent and is ignored.
    fn osc52_clipboard(&mut self, params: &[&[u8]]) {
        let targets = params.get(1).copied().unwrap_or(b"");
        if !targets.is_empty() && !targets.iter().any(|b| matches!(b, b'c' | b'p' | b's')) {
            return;
        }
        let payload = params.get(2).copied().unwrap_or(b"");
        if payload == b"?" {
            // Clipboard READ — see the note above. Never answered.
            log::debug!("screen: ignoring OSC 52 clipboard read request");
            return;
        }
        if payload.is_empty() {
            // `OSC 52 ; c ; ST` clears the selection; there is nothing to copy.
            return;
        }
        if payload.len() > MAX_OSC52_BASE64_LEN {
            log::warn!(
                "screen: dropping oversized OSC 52 clipboard write ({} bytes of base64)",
                payload.len()
            );
            return;
        }
        match base64_decode(payload) {
            Some(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if !text.is_empty() {
                    self.clipboard_pending = Some(text);
                }
            }
            None => log::debug!("screen: ignoring OSC 52 write with undecodable base64"),
        }
    }

    /// Resize the screen to new dimensions.
    ///
    /// When the width changes on the primary screen the buffer is *reflowed*:
    /// every logical line (a run of rows joined by [`Row::wrapped`]) is rejoined
    /// and re-split at the new width, so text that was hard-wrapped narrow fills
    /// the pane again when it is widened -- and re-wraps when it is narrowed.
    ///
    /// Only the **alternate screen** skips reflow: full-screen apps repaint from
    /// scratch on `SIGWINCH`, and rewrapping their private buffer underneath
    /// them only races that redraw and produces garbage. tmux, zellij,
    /// Alacritty, kitty and WezTerm all skip it too.
    ///
    /// A *pure vertical* resize on the primary screen used to skip it as well,
    /// on the reasoning that nothing can rewrap when the width is unchanged.
    /// Nothing rewraps -- but the row count still changes, and that is not a
    /// block copy. [`Screen::resize_clamp`] keeps the **top** `rows` of the grid,
    /// so shrinking the height *deleted the bottom rows outright*: the cursor,
    /// the prompt and the newest output, dropped rather than scrolled into
    /// scrollback. Zooming a full-width pane changes only the height, so a
    /// `Prefix+f` in and back out silently ate every line that no longer fit and
    /// left `max_scroll_offset()` frozen -- history that could never be scrolled
    /// to because it was never recorded. Reflow already re-derives the split
    /// correctly in both directions (the last `rows` lines are the grid, the
    /// rest is scrollback), so the primary screen always takes it; with the
    /// width unchanged the rejoin/re-split is an identity on the wrapping.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        log::debug!(
            "screen: resize old={}x{}, new={}x{}",
            self.cols,
            self.rows,
            cols,
            rows
        );

        self.resize_saved_screen(cols, rows);

        if self.alt_screen_active {
            self.resize_clamp(cols, rows);
        } else {
            // Height first, then width, the order tmux's screen_resize uses.
            self.resize_height(rows);
            // A height-only resize skips the re-split of every scrollback line,
            // which would change nothing at an unchanged width.
            if cols != self.cols {
                self.reflow(cols, rows);
            }
        }

        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);

        // Clamp cursor position.
        if self.cursor_x >= cols {
            self.cursor_x = cols.saturating_sub(1);
        }
        if self.cursor_y >= rows {
            self.cursor_y = rows.saturating_sub(1);
        }
    }

    /// The whole screen was cleared, so none of the scrollback belongs above
    /// what is on it any more: a taller screen must not pull it back into view.
    /// tmux resets `hscrolled` on the same clears. The alternate screen's
    /// clears leave the primary screen's count alone.
    fn history_cleared_from_view(&mut self) {
        if !self.alt_screen_active {
            self.resize_scrolled = 0;
            // A line that wrapped from the scrollback onto the cleared screen
            // ends at the clear, or reflow would join the prompt onto it.
            if let Some(last) = self.scrollback.back_mut() {
                last.wrapped = false;
            }
        }
    }

    /// Change the primary screen's height at its current width, as tmux does.
    ///
    /// Shrinking removes the blank rows below the cursor first and only then
    /// moves rows off the top into the scrollback, so a prompt at the top of a
    /// cleared screen stays there. Rows that hold anything are never removed.
    /// Growing pulls back at most `resize_scrolled` lines from the scrollback,
    /// so history cleared from view stays out of it, and adds blank rows at the
    /// bottom for the rest.
    fn resize_height(&mut self, rows: u16) {
        let old = self.grid.len();
        let new = rows as usize;
        if new < old {
            let mut need = old - new;
            while need > 0
                && self.grid.len() > self.cursor_y as usize + 1
                && self
                    .grid
                    .last()
                    .is_some_and(|row| row.cells.iter().all(Self::is_padding))
            {
                self.grid.pop();
                need -= 1;
            }
            // Rows below the cursor that hold anything are kept, so when there
            // are more of them than the new height leaves room for, the rows
            // pushed off the top include the cursor's own row. That is the
            // price of never removing text, and the old resize did the same.
            for row in self.grid.drain(..need) {
                self.scrollback.push_back(row);
            }
            self.resize_scrolled += need;
            self.cursor_y = (self.cursor_y as usize).saturating_sub(need) as u16;
            while self.scrollback.len() > self.scrollback_limit {
                self.scrollback.pop_front();
                self.lines_evicted += 1;
            }
            self.resize_scrolled = self.resize_scrolled.min(self.scrollback.len());
        } else if new > old {
            let pull = (new - old)
                .min(self.resize_scrolled)
                .min(self.scrollback.len());
            for _ in 0..pull {
                if let Some(row) = self.scrollback.pop_back() {
                    self.grid.insert(0, row);
                }
            }
            self.resize_scrolled -= pull;
            self.cursor_y += pull as u16;
            self.grid.resize(new, Row::new(self.cols));
        }
        self.rows = rows;
    }

    /// Non-reflowing resize: copy the top-left `min(rows) x min(cols)` block
    /// into a fresh grid, padding the rest with blanks.
    fn resize_clamp(&mut self, cols: u16, rows: u16) {
        let new_grid = Self::clamp_grid(&self.grid, self.cols, cols, rows);
        self.cols = cols;
        self.rows = rows;
        self.grid = new_grid;
    }

    /// Copy the top-left `min(rows) x min(cols)` block of `old` (whose rows are
    /// `old_cols` wide) into a fresh grid of `cols x rows`.
    fn clamp_grid(old: &[Row], old_cols: u16, cols: u16, rows: u16) -> Vec<Row> {
        let mut new_grid = Self::make_grid(cols, rows);

        let copy_rows = old.len().min(rows as usize);
        let copy_cols = (old_cols as usize).min(cols as usize);
        // A row whose tail was cut off no longer continues into the next one, so
        // the wrap flag only survives when the width is untouched.
        let keep_wrap = cols == old_cols;

        for (r, new_row) in new_grid.iter_mut().enumerate().take(copy_rows) {
            for (c, new_cell) in new_row.cells.iter_mut().enumerate().take(copy_cols) {
                if let Some(cell) = old[r].cells.get(c) {
                    *new_cell = cell.clone();
                }
            }
            new_row.wrapped = keep_wrap && old[r].wrapped;
        }

        new_grid
    }

    /// Bring the saved *primary* screen (the snapshot taken when an application
    /// switched to the alternate screen, modes 1047/1049) along to the new
    /// dimensions.
    ///
    /// Without this, resizing while a full-screen app is running leaves a
    /// snapshot of the old size behind; restoring it on exit hands back a grid
    /// whose row count disagrees with `rows`/`scroll_bottom`, and the next
    /// `DL`/`IL` then indexes past the end of the grid and panics.
    ///
    /// Must run *before* `self.cols`/`self.rows` are updated.
    fn resize_saved_screen(&mut self, cols: u16, rows: u16) {
        let last_row = rows.saturating_sub(1);

        if let Some(saved) = self.saved_grid.take() {
            let saved = self.spill_saved_rows(saved, rows);
            self.saved_grid = Some(Self::clamp_grid(&saved, self.cols, cols, rows));
        }

        // A saved region that spanned the whole old screen keeps spanning the
        // whole screen; anything narrower is just clamped into range.
        self.saved_scroll_bottom = if self.saved_scroll_bottom >= self.rows.saturating_sub(1) {
            last_row
        } else {
            self.saved_scroll_bottom.min(last_row)
        };
        self.saved_scroll_top = self.saved_scroll_top.min(self.saved_scroll_bottom);
        self.saved_cursor_x = self.saved_cursor_x.min(cols.saturating_sub(1));
        self.saved_cursor_y = self.saved_cursor_y.min(last_row);
    }

    /// Scroll the rows a shrinking primary snapshot can no longer hold off its
    /// **top** and into the parked primary scrollback, returning what remains.
    ///
    /// [`Screen::clamp_grid`] keeps the top `rows`, so clamping the snapshot on
    /// its own deleted the *bottom* of the primary screen -- the cursor, the
    /// prompt and the newest output -- and deleted it silently, without even the
    /// consolation of it landing in scrollback. That is the same defect
    /// [`Screen::resize`] no longer has on the live screen, one indirection
    /// further out: it fires when a pane is resized (a zoom, a split, a client
    /// attaching at a smaller size) *while a full-screen application is up*, and
    /// only shows itself much later, when the application exits and hands back a
    /// primary screen with its last few dozen lines missing.
    fn spill_saved_rows(&mut self, mut saved: Vec<Row>, rows: u16) -> Vec<Row> {
        let overflow = saved.len().saturating_sub(rows as usize);
        if overflow == 0 {
            return saved;
        }
        let limit = self.scrollback_limit;
        // The two snapshots are taken and released together, so a parked grid
        // always has a parked scrollback to spill into -- asserted directly by
        // `the_scrollback_park_is_held_exactly_while_the_alt_screen_is_up`. The
        // `else` is unreachable defensive cover: without somewhere to put them,
        // dropping the rows is the only option, as before.
        if let Some(scrollback) = self.saved_scrollback.as_mut() {
            scrollback.extend(saved.drain(..overflow));
            while scrollback.len() > limit {
                scrollback.pop_front();
                self.lines_evicted += 1;
            }
            // These rows may come back when the primary screen grows again,
            // as rows `resize_height` moves off the top do.
            self.resize_scrolled = (self.resize_scrolled + overflow).min(scrollback.len());
        } else {
            saved.drain(..overflow);
        }
        saved
    }

    /// Feed raw terminal output bytes through the VTE parser, updating the
    /// screen state.
    pub fn process_output(&mut self, data: &[u8]) {
        log::trace!("screen: process_output bytes={}", data.len());
        for &byte in data {
            // We need to temporarily take ownership of the parser to satisfy
            // the borrow checker (advance requires &mut Parser and &mut Perform).
            let mut parser = std::mem::replace(&mut self.parser, vte::Parser::new());
            parser.advance(self, byte);
            self.parser = parser;
        }
    }

    /// Return all scrollback lines plus the visible grid as plain text.
    pub fn scrollback_content(&self) -> String {
        let mut result = String::new();

        for line in &self.scrollback {
            let text: String = line.iter().map(|c| c.c).collect();
            result.push_str(text.trim_end());
            result.push('\n');
        }

        for row in &self.grid {
            let text: String = row.iter().map(|c| c.c).collect();
            result.push_str(text.trim_end());
            result.push('\n');
        }

        result
    }

    /// Return scrollback + only the first `visible_rows` grid rows as plain text.
    /// Use this instead of `scrollback_content()` when the grid is larger than
    /// the displayed content area (e.g., due to borders).
    pub fn scrollback_content_visible(&self, visible_rows: usize) -> String {
        let mut result = String::new();

        for line in &self.scrollback {
            let text: String = line.iter().map(|c| c.c).collect();
            result.push_str(text.trim_end());
            result.push('\n');
        }

        for row in self.grid.iter().take(visible_rows) {
            let text: String = row.iter().map(|c| c.c).collect();
            result.push_str(text.trim_end());
            result.push('\n');
        }

        result
    }

    /// Total number of lines: scrollback + visible grid.
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.grid.len()
    }

    /// Get a row from the combined scrollback+grid buffer by absolute index.
    /// Index 0 = oldest scrollback line.
    pub fn line_at(&self, index: usize) -> Option<&Row> {
        if index < self.scrollback.len() {
            self.scrollback.get(index)
        } else {
            self.grid.get(index - self.scrollback.len())
        }
    }

    /// Maximum scroll offset: how many lines the view can be scrolled back
    /// before the oldest scrollback line sits on the top row. Clamps against the
    /// inner grid height (`rows`), exactly the number of content rows a pane
    /// draws — using the client terminal rows would over-subtract and leave the
    /// earliest scrollback lines unreachable at max scroll.
    pub fn max_scroll_offset(&self) -> usize {
        self.total_lines().saturating_sub(self.rows as usize)
    }

    /// Map a viewport row (`0..rows`, top-down) at a given `scroll_offset` to a
    /// stable absolute line id. The id survives scrollback eviction: as older
    /// lines are dropped, `lines_evicted` grows while `scrollback.len()` shrinks,
    /// so the id of a fixed content line is unchanged. Inverse of
    /// [`Screen::row_of_abs`].
    pub fn abs_of_row(&self, scroll_offset: usize, row: u16) -> usize {
        self.lines_evicted + self.scrollback.len().saturating_sub(scroll_offset) + row as usize
    }

    /// Map a stable absolute line id back to a viewport row at the given
    /// `scroll_offset`. The result may be `< 0` (the line is scrolled above the
    /// viewport) or `>= rows` (below it). Inverse of [`Screen::abs_of_row`].
    pub fn row_of_abs(&self, scroll_offset: usize, abs: usize) -> i64 {
        abs as i64 - self.lines_evicted as i64 - self.scrollback.len() as i64 + scroll_offset as i64
    }

    /// Convert a stable absolute line id into an index for [`Screen::line_at`]
    /// (an index into the combined scrollback+grid buffer). An id older than the
    /// current eviction watermark saturates to 0 (the oldest retained line).
    pub fn array_index_of_abs(&self, abs: usize) -> usize {
        abs.saturating_sub(self.lines_evicted)
    }

    // -- Private helpers ----------------------------------------------------

    /// Create an empty grid filled with default cells.
    fn make_grid(cols: u16, rows: u16) -> Vec<Row> {
        vec![Row::new(cols); rows as usize]
    }

    // -- Reflow -------------------------------------------------------------

    /// Whether a cell is padding rather than text, for deciding where a logical
    /// line ends.
    ///
    /// Only such cells may be trimmed off the end of a logical line. A space
    /// with a non-default background or under reverse video is *real content*
    /// (colored prompts, `ls` output, status bars) and has to survive reflow.
    ///
    /// An erased blank counts as padding whatever its background, because it
    /// says where nothing was written, not what was. Reflow uses that to decide
    /// where a line ends and whether it wraps, and then puts the trimmed tail
    /// back on the line's last row so its colour survives (see `pad_template`).
    /// A continuation cell is never padding: it belongs to the glyph before it.
    fn is_padding(cell: &Cell) -> bool {
        cell.c == ' '
            && cell.width == 1
            && cell.combining.is_empty()
            && (cell.erased || (cell.attrs.bg == Color::Default && !cell.attrs.reverse))
    }

    /// The cell that continues a trimmed tail past its own end: its last cell
    /// when that came from an erase, so a line erased in colour to the edge
    /// stays coloured to the new edge, and a default blank otherwise.
    fn pad_template(tail: &[Cell]) -> Cell {
        match tail.last() {
            Some(cell) if cell.erased => cell.clone(),
            _ => Cell::default(),
        }
    }

    /// The cell every erase, insert, delete and scroll fills with. It takes the
    /// current background and nothing else, as tmux and xterm do: the pane's
    /// TERM is xterm-256color, which declares `bce`, so programs clear a
    /// coloured area with an erase and expect the colour to stay.
    fn blank(&self) -> Cell {
        Cell {
            attrs: CellAttrs {
                bg: self.current_attrs.bg.clone(),
                ..CellAttrs::default()
            },
            erased: true,
            ..Cell::default()
        }
    }

    /// A whole row of `blank()` cells.
    fn blank_row(&self) -> Row {
        Row {
            cells: vec![self.blank(); self.cols as usize],
            wrapped: false,
        }
    }

    /// Pad a row under construction out to exactly `cols` cells and stamp its
    /// wrap flag. Every row handed back by reflow is full width so that
    /// `grid[r][c]` indexing (here and in the compositor) stays in bounds.
    fn finish_row(mut cells: Vec<Cell>, cols: u16, wrapped: bool) -> Row {
        cells.resize(cols as usize, Cell::default());
        Row { cells, wrapped }
    }

    /// Re-split one logical line into physical rows of `cols` columns.
    ///
    /// Accumulation is by DISPLAY width (`Cell::width`), never by cell index, so
    /// a width-2 glyph is never cut in half: if it does not fit in the columns
    /// left on the current row it starts the next one, leaving the straddled
    /// column blank -- exactly what `put_char` does when printing live. A glyph
    /// wider than the entire screen can never be placed and is dropped, which
    /// also guarantees the loop makes progress.
    ///
    /// `cursor_at` is an index into `cells` (or `cells.len()` for "just past the
    /// end of the line"). The matching `(row, col)` within the produced rows is
    /// returned so the caller can put the cursor back on the same character.
    ///
    /// `tail` is the padding reflow trimmed off the line. It goes back on the
    /// last row as far as it fits, never onto a row of its own, and the row is
    /// then filled out with `pad_template(tail)`.
    fn split_logical(
        cells: &[Cell],
        cols: u16,
        cursor_at: Option<usize>,
        tail: &[Cell],
    ) -> (Vec<Row>, Option<(usize, usize)>) {
        let width = cols as usize;
        let mut out: Vec<Row> = Vec::new();
        let mut row: Vec<Cell> = Vec::with_capacity(width);
        let mut used = 0usize;
        let mut cursor_pos: Option<(usize, usize)> = None;
        let mut i = 0usize;

        while i < cells.len() {
            let cell = &cells[i];
            let w = if cell.width == 2 { 2 } else { 1 };
            if w > width {
                i += 1;
                continue;
            }
            if used + w > width {
                out.push(Self::finish_row(std::mem::take(&mut row), cols, true));
                used = 0;
            }

            let start = i;
            if cell.width == 2 {
                row.push(cell.clone());
                // Reuse the original continuation spacer when there is one so
                // its background survives; synthesize one otherwise.
                let spacer = match cells.get(i + 1) {
                    Some(next) if next.width == 0 => {
                        i += 1;
                        next.clone()
                    }
                    _ => Cell {
                        c: ' ',
                        width: 0,
                        attrs: cell.attrs.clone(),
                        combining: Vec::new(),
                        hyperlink: cell.hyperlink.clone(),
                        erased: false,
                    },
                };
                row.push(spacer);
            } else {
                let mut copy = cell.clone();
                if copy.width == 0 {
                    // A continuation cell whose wide lead is gone (overwritten
                    // earlier): demote it to a plain blank so widths stay sane.
                    copy.c = ' ';
                    copy.width = 1;
                }
                row.push(copy);
            }
            i += 1;

            if let Some(t) = cursor_at {
                if cursor_pos.is_none() && t >= start && t < i {
                    cursor_pos = Some((out.len(), used + (t - start)));
                }
            }
            used += w;
        }

        // Cursor parked just past the last character of the line. `used` may
        // equal `cols` here; the caller turns that into the deferred-wrap state.
        if cursor_at == Some(cells.len()) {
            cursor_pos = Some((out.len(), used));
        }
        let room = width.saturating_sub(used);
        row.extend(tail.iter().take(room).cloned());
        row.resize(width, Self::pad_template(tail));
        out.push(Row {
            cells: row,
            wrapped: false,
        });

        (out, cursor_pos)
    }

    /// Rebuild `scrollback` + `grid` so that every logical line is wrapped at
    /// `cols` instead of the old width.
    ///
    /// The buffer is rebuilt into fresh storage rather than mutated in place;
    /// the cursor is carried across as a *logical* position (which logical line,
    /// and how far into it) and re-derived afterwards.
    fn reflow(&mut self, cols: u16, rows: u16) {
        // 1. Treat scrollback and grid as ONE stream, oldest row first. They can
        //    never be reflowed independently: a logical line that straddles the
        //    boundary would be torn into two unrelated lines.
        let sb_len = self.scrollback.len();
        let mut stream: Vec<Row> = Vec::with_capacity(sb_len + self.grid.len());
        stream.extend(self.scrollback.drain(..));
        stream.append(&mut std::mem::take(&mut self.grid));

        let cursor_abs = (sb_len + self.cursor_y as usize).min(stream.len().saturating_sub(1));

        // 2. Drop the trailing run of blank rows BELOW the cursor. They hold no
        //    information, and keeping them means every shrink shoves real
        //    content off into the scrollback -- so a zoom/unzoom cycle would
        //    permanently inflate it with blanks.
        //    The dropped rows come back in step 6 as the padding rows at the
        //    bottom of the grid, with their own cells cut or extended the way a
        //    line's tail is, so an erase's colour stays where it was.
        let mut end = stream.len();
        while end > cursor_abs + 1 && stream[end - 1].cells.iter().all(Self::is_padding) {
            end -= 1;
        }
        let dropped: Vec<Vec<Cell>> = stream.drain(end..).map(|row| row.cells).collect();

        // 3. Join rows into logical lines: a row starts a new one unless the row
        //    before it is flagged `wrapped`. The cursor's *logical* coordinates
        //    are captured on the way past. A cursor parked awaiting a deferred
        //    wrap sits one column further along the logical line than its
        //    physical column suggests, so fold `pending_wrap` in here.
        let cursor_x_eff = self.cursor_x as usize + usize::from(self.pending_wrap);
        let mut logicals: Vec<Vec<Cell>> = Vec::new();
        let mut current: Vec<Cell> = Vec::new();
        let mut cursor_logical = 0usize;
        let mut cursor_off = 0usize;
        // Where the old screen's top row sits in its logical line, so step 6
        // can find that row again after re-splitting. Likewise the first row
        // after the last clear: the scrollback before it was cleared from view
        // and must stay out of it.
        let mut top_logical = 0usize;
        let mut top_off = 0usize;
        let boundary = sb_len - self.resize_scrolled.min(sb_len);
        let mut boundary_logical = 0usize;
        let mut boundary_off = 0usize;

        for (idx, row) in stream.iter().enumerate() {
            if idx == boundary {
                boundary_logical = logicals.len();
                boundary_off = current.len();
            }
            if idx == sb_len {
                top_logical = logicals.len();
                top_off = current.len();
            }
            if idx == cursor_abs {
                cursor_logical = logicals.len();
                cursor_off = current.len() + cursor_x_eff;
            }
            current.extend(row.cells.iter().cloned());
            if !row.wrapped {
                logicals.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            logicals.push(current);
        }
        if logicals.is_empty() {
            logicals.push(Vec::new());
        }

        // 4. Trim the padding off the end of each logical line. The cursor's own
        //    line is never trimmed past the cursor: it usually sits just after
        //    the text (shell prompt), and clipping there would drag it left.
        //    The trimmed padding is kept as the line's tail for step 5.
        let mut tails: Vec<Vec<Cell>> = Vec::with_capacity(logicals.len());
        for (li, logical) in logicals.iter_mut().enumerate() {
            let keep = if li == cursor_logical {
                cursor_off.min(logical.len())
            } else {
                0
            };
            let mut cut = logical.len();
            while cut > keep && Self::is_padding(&logical[cut - 1]) {
                cut -= 1;
            }
            tails.push(logical.split_off(cut));
        }
        cursor_off = cursor_off.min(logicals[cursor_logical].len());

        // 5. Re-split every logical line at the new width.
        let mut new_lines: Vec<Row> = Vec::new();
        let mut cursor_target: Option<(usize, usize)> = None;
        let mut old_top = 0usize;
        let mut new_boundary = 0usize;
        for (li, logical) in logicals.iter().enumerate() {
            let row_of = |off: usize| {
                let (_, pos) =
                    Self::split_logical(logical, cols, Some(off.min(logical.len())), &tails[li]);
                new_lines.len() + pos.map_or(0, |(r, _)| r)
            };
            if li == top_logical {
                old_top = row_of(top_off);
            }
            if li == boundary_logical {
                new_boundary = row_of(boundary_off);
            }
            let at = if li == cursor_logical {
                Some(cursor_off)
            } else {
                None
            };
            let (chunks, pos) = Self::split_logical(logical, cols, at, &tails[li]);
            if let Some((r, c)) = pos {
                cursor_target = Some((new_lines.len() + r, c));
            }
            new_lines.extend(chunks);
        }

        // 6. Re-derive the split between scrollback and grid. The blank rows
        //    that were below the cursor stay below it, as in tmux, so a prompt
        //    on a cleared screen keeps its place instead of the scrollback being
        //    pulled down above it.
        //    - Text that now takes fewer rows pulls the scrollback into view
        //      above it, keeping the cursor's distance from the bottom.
        //    - Text that now takes more rows uses up the blank rows first, and
        //      only then pushes the top of the screen into the scrollback.
        let rows_us = rows as usize;
        let total = new_lines.len();
        let keep_distance = (total + dropped.len()).saturating_sub(rows_us);
        //    - Either way the grid never starts above the last clear, so the
        //      history cleared from view stays out of it. That cannot hide the
        //      cursor: the clear's row is at or above the old top row.
        let grid_start = if keep_distance <= old_top {
            keep_distance
        } else {
            old_top.max(total.saturating_sub(rows_us))
        }
        .max(new_boundary);
        let mut grid = new_lines.split_off(grid_start);
        let mut scrollback: VecDeque<Row> = VecDeque::from(new_lines);
        let mut dropped = dropped.into_iter();
        while grid.len() < rows_us {
            let mut cells = dropped.next().unwrap_or_default();
            let pad = Self::pad_template(&cells);
            cells.truncate(cols as usize);
            cells.resize(cols as usize, pad);
            grid.push(Row {
                cells,
                wrapped: false,
            });
        }
        while scrollback.len() > self.scrollback_limit {
            scrollback.pop_front();
            self.lines_evicted += 1;
        }
        // The scrollback rows after the clear are the ones a taller screen may
        // pull back; re-splitting changed how many rows they take.
        self.resize_scrolled = (grid_start - new_boundary).min(scrollback.len());

        self.cols = cols;
        self.rows = rows;
        self.grid = grid;
        self.scrollback = scrollback;

        // 7. Put the cursor back on the same logical character. A column equal
        //    to the new width means the character it followed exactly filled the
        //    row: that is the deferred-wrap state, not an out-of-range column.
        let (abs, col) = cursor_target.unwrap_or((total.saturating_sub(1), 0));
        self.pending_wrap = col >= cols as usize;
        self.cursor_x = if self.pending_wrap {
            cols.saturating_sub(1)
        } else {
            col as u16
        };
        self.cursor_y = abs
            .saturating_sub(grid_start)
            .min(rows_us.saturating_sub(1)) as u16;
    }

    /// Scroll the region between `scroll_top` and `scroll_bottom` up by one
    /// line. The top line of the region is moved to scrollback (if the region
    /// starts at the top of the screen).
    fn scroll_up_region(&mut self) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;

        if top > bottom || bottom >= self.grid.len() {
            return;
        }

        // If the scroll region starts at row 0, the evicted line goes to
        // scrollback -- unless we are on the alternate screen, which has no
        // history at all. A full-screen application's redraws are ephemeral by
        // definition: letting them accumulate grew `max_scroll_offset()`
        // without bound and gave a wheel/`ScrollDelta`/copy-mode gesture
        // something to chase that the user never typed.
        if top == 0 && !self.alt_screen_active {
            let evicted = self.grid[0].clone();
            self.scrollback.push_back(evicted);
            self.resize_scrolled += 1;
            while self.scrollback.len() > self.scrollback_limit {
                self.scrollback.pop_front();
                self.lines_evicted += 1;
            }
            self.resize_scrolled = self.resize_scrolled.min(self.scrollback.len());
        }

        // Shift lines up within the region. The rows keep their `wrapped`
        // flags: a logical line that spills from the region into the scrollback
        // must still read as one line for reflow.
        for r in top..bottom {
            self.grid[r] = self.grid[r + 1].clone();
        }
        self.grid[bottom] = self.blank_row();
        self.break_wrap_above(top);
    }

    /// Scroll the region between `scroll_top` and `scroll_bottom` down by one
    /// line. The bottom line of the region is discarded.
    fn scroll_down_region(&mut self) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;

        if top > bottom || bottom >= self.grid.len() {
            return;
        }

        for r in (top + 1..=bottom).rev() {
            self.grid[r] = self.grid[r - 1].clone();
        }
        self.grid[top] = self.blank_row();
        self.break_wrap_above(top);
    }

    /// Clear the wrap flag of the row directly *above* a region whose contents
    /// just moved (scroll region, IL/DL). Whatever used to continue into `top`
    /// no longer does, and leaving the flag set would make reflow splice the
    /// row above onto unrelated text. Mirrors tmux's `grid_view_*` handling.
    ///
    /// When `top == 0` the row above lives in the scrollback, and it genuinely
    /// still continues into the new `grid[0]` (that is how a logical line
    /// straddling the boundary survives), so nothing is cleared.
    fn break_wrap_above(&mut self, top: usize) {
        if top > 0 && top - 1 < self.grid.len() {
            self.grid[top - 1].wrapped = false;
        }
    }

    /// Blank the cells `start..end` of `row`, together with the other half of
    /// any wide glyph the span cuts through. A lead left over a blanked
    /// continuation, or a continuation left after a blanked lead, would be a
    /// width-0 cell the client never paints, so that column keeps whatever the
    /// terminal last drew there.
    fn erase_cells(&mut self, row: usize, start: usize, end: usize) {
        let blank = self.blank();
        let Some(line) = self.grid.get_mut(row) else {
            return;
        };
        let end = end.min(line.len());
        if start >= end {
            return;
        }
        if start > 0 && line[start].width == 0 {
            line[start - 1] = blank.clone();
        }
        if end < line.len() && line[end].width == 0 {
            line[end] = blank.clone();
        }
        for cell in &mut line.cells[start..end] {
            *cell = blank.clone();
        }
    }

    /// When `x` is the continuation of a wide glyph, blank the whole glyph, so
    /// that an insert or delete splitting it there moves no lone half.
    fn break_wide_at(&mut self, row: usize, x: usize) {
        if self
            .grid
            .get(row)
            .and_then(|line| line.get(x))
            .is_some_and(|c| c.width == 0)
        {
            self.erase_cells(row, x, x + 1);
        }
    }

    /// Perform a DECAWM autowrap: mark the row the cursor is leaving as
    /// continuing onto the next one, then move to column 0 of that next row
    /// (scrolling the region when we were sitting on its last line).
    ///
    /// The flag has to be stamped *before* the cursor moves and before any
    /// scroll, otherwise it lands on the wrong row. This is the ONLY place
    /// (together with its wide-glyph twin, which also routes through here) that
    /// ever sets `Row::wrapped` -- explicit line feeds must never do so.
    fn autowrap_to_next_row(&mut self) {
        let row = self.cursor_y as usize;
        if row < self.grid.len() {
            self.grid[row].wrapped = true;
        }
        self.cursor_x = 0;
        self.cursor_y += 1;
        if self.cursor_y > self.scroll_bottom {
            self.cursor_y = self.scroll_bottom;
            self.scroll_up_region();
        }
    }

    /// Write a character at the current cursor position, then advance the
    /// cursor. Wraps to the next line at the end of a row and scrolls if
    /// necessary.
    fn put_char(&mut self, c: char) {
        // Zero-width / combining characters (e.g. U+0301 COMBINING ACUTE
        // ACCENT): attach them to the base glyph in the cell immediately to the
        // left of the cursor -- the last-written cell -- without advancing the
        // cursor or overwriting any cell. At column 0 there is no preceding base
        // on this row, so the mark is dropped (rare edge case).
        let display_width = UnicodeWidthChar::width(c);
        if display_width == Some(0) {
            if self.cursor_x > 0 {
                let row = self.cursor_y as usize;
                let col = (self.cursor_x - 1) as usize;
                if row < self.grid.len() && col < self.grid[row].len() {
                    self.grid[row][col].combining.push(c);
                }
            }
            return;
        }
        let width: u8 = if display_width == Some(2) { 2 } else { 1 };

        // Deferred (DECAWM) wrap: the previous char filled the last column and
        // left the cursor pending at the row's edge; wrap now that another char
        // arrives.
        if self.pending_wrap {
            self.pending_wrap = false;
            self.autowrap_to_next_row();
        }

        // A wide glyph must never straddle the right edge. If it would not fit,
        // wrap first and leave the last cell of the previous row blank. This is
        // based on the real edge (a wide glyph at the last free column can never
        // be pending), so clear any stale pending-wrap here too.
        if width == 2 && self.cursor_x + 2 > self.cols {
            self.pending_wrap = false;
            self.autowrap_to_next_row();
        }

        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;

        if row < self.grid.len() && col < self.grid[row].len() {
            let old_width = self.grid[row][col].width;

            self.grid[row][col] = Cell {
                c,
                width,
                attrs: self.current_attrs.clone(),
                combining: Vec::new(),
                hyperlink: self.current_hyperlink.clone(),
                erased: false,
            };

            if width == 2 {
                // Write the continuation spacer for the wide lead.
                if col + 1 < self.grid[row].len() {
                    self.grid[row][col + 1] = Cell {
                        c: ' ',
                        width: 0,
                        attrs: self.current_attrs.clone(),
                        combining: Vec::new(),
                        hyperlink: self.current_hyperlink.clone(),
                        erased: false,
                    };
                }
            } else if old_width == 2 && col + 1 < self.grid[row].len() {
                // We overwrote a former wide lead with a narrow char; blank the
                // now-dangling continuation cell so it renders as a space. Clear
                // any marks the in-place path would otherwise leave behind.
                self.grid[row][col + 1].c = ' ';
                self.grid[row][col + 1].width = 1;
                self.grid[row][col + 1].combining.clear();
            }
        }

        self.cursor_x += width as u16;
        if self.cursor_x >= self.cols {
            // At/over the right edge: park on the last column and defer the wrap.
            self.cursor_x = self.cols.saturating_sub(1);
            self.pending_wrap = true;
        }
    }

    /// Handle the SGR (Select Graphic Rendition) escape sequence.
    fn handle_sgr(&mut self, params: &[&[u16]]) {
        if std::env::var("REMUX_DEBUG_SGR").is_ok() {
            log::info!("SGR params={:?}", params);
        }
        if params.is_empty() {
            self.current_attrs = CellAttrs::default();
            return;
        }

        let mut i = 0;
        while i < params.len() {
            let code = params[i].first().copied().unwrap_or(0);
            match code {
                0 => self.current_attrs = CellAttrs::default(),
                1 => self.current_attrs.bold = true,
                3 => self.current_attrs.italic = true,
                4 => {
                    // Extended underline: `4:0` disables, `4:1..=4:5` are styles
                    // (all shown as underline in our bool model), plain `4`
                    // enables. vte groups colon sub-parameters into this slice,
                    // so `params[i].get(1)` is the sub-param when present.
                    self.current_attrs.underline = params[i].get(1).copied() != Some(0);
                }
                7 => self.current_attrs.reverse = true,
                22 => self.current_attrs.bold = false,
                23 => self.current_attrs.italic = false,
                24 => self.current_attrs.underline = false,
                27 => self.current_attrs.reverse = false,

                // Standard foreground colors 30-37
                30..=37 => {
                    self.current_attrs.fg = Color::Indexed((code - 30) as u8);
                }
                // Default foreground
                39 => self.current_attrs.fg = Color::Default,

                // Standard background colors 40-47
                40..=47 => {
                    self.current_attrs.bg = Color::Indexed((code - 40) as u8);
                }
                // Default background
                49 => self.current_attrs.bg = Color::Default,

                // Bright foreground colors 90-97
                90..=97 => {
                    self.current_attrs.fg = Color::Indexed((code - 90 + 8) as u8);
                }
                // Bright background colors 100-107
                100..=107 => {
                    self.current_attrs.bg = Color::Indexed((code - 100 + 8) as u8);
                }

                // Extended foreground color. Two wire forms:
                //   SEMICOLON: `38;5;N` / `38;2;R;G;B` — sub-params are in the
                //     following separate param slices; we advance `i` past them.
                //   COLON: `38:5:N` / `38:2:R:G:B` (or `38:2::R:G:B` with an
                //     empty colorspace field) — vte packs the whole sequence
                //     into this one slice, so nothing extra to advance.
                38 => {
                    if params[i].len() > 1 {
                        if let Some(c) = Self::parse_colon_color(params[i]) {
                            self.current_attrs.fg = c;
                        }
                    } else if let Some((c, adv)) = Self::parse_semicolon_color(params, i) {
                        self.current_attrs.fg = c;
                        i += adv;
                    }
                }

                // Extended background color (see `38` above for the two forms).
                48 => {
                    if params[i].len() > 1 {
                        if let Some(c) = Self::parse_colon_color(params[i]) {
                            self.current_attrs.bg = c;
                        }
                    } else if let Some((c, adv)) = Self::parse_semicolon_color(params, i) {
                        self.current_attrs.bg = c;
                        i += adv;
                    }
                }

                // Underline color set (58) / reset (59): explicitly ignored so
                // they never leak into the `underline` bool. The colon form
                // packs its sub-params into this slice; the semicolon form's
                // trailing params are harmless no-ops on subsequent iterations.
                58 | 59 => {}

                _ => {} // Ignore unknown SGR codes
            }
            i += 1;
        }
    }

    /// Take any pending PTY responses (e.g., DSR replies).
    pub fn take_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pty_responses)
    }

    /// Parse an extended color from the COLON form, where vte has packed the
    /// whole sequence into a single param slice, e.g. `38:5:N` → `[38, 5, N]`
    /// and `38:2:R:G:B` → `[38, 2, R, G, B]` (with `38:2::R:G:B` giving an
    /// empty colorspace field as `[38, 2, 0, R, G, B]`).
    fn parse_colon_color(param: &[u16]) -> Option<Color> {
        match *param.get(1)? {
            5 => param.get(2).map(|&n| Color::Indexed(n as u8)),
            2 => {
                // Trailing components after the `2` selector. Either
                // `[R, G, B]` or `[colorspace, R, G, B]` (possibly with more
                // fields) — in the latter case take the last three as RGB.
                let comps = &param[2..];
                let rgb = match comps.len() {
                    n if n >= 4 => &comps[n - 3..],
                    3 => comps,
                    _ => return None,
                };
                Some(Color::Rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8))
            }
            _ => None,
        }
    }

    /// Parse an extended color from the SEMICOLON form, where the sub-params
    /// live in separate param slices starting at `params[i + 1]` (`38;5;N` or
    /// `38;2;R;G;B`). Returns the color and how many extra params it consumed
    /// so the caller can advance its index.
    fn parse_semicolon_color(params: &[&[u16]], i: usize) -> Option<(Color, usize)> {
        match params.get(i + 1)?.first().copied()? {
            5 => {
                let n = params.get(i + 2)?.first().copied()?;
                Some((Color::Indexed(n as u8), 2))
            }
            2 => {
                let r = params.get(i + 2)?.first().copied()? as u8;
                let g = params.get(i + 3)?.first().copied()? as u8;
                let b = params.get(i + 4)?.first().copied()? as u8;
                Some((Color::Rgb(r, g, b), 4))
            }
            _ => None,
        }
    }

    /// Get the first parameter value from a CSI parameter list, with a default.
    fn csi_param(params: &[&[u16]], idx: usize, default: u16) -> u16 {
        params
            .get(idx)
            .and_then(|p| p.first().copied())
            .map(|v| if v == 0 { default } else { v })
            .unwrap_or(default)
    }
}

// ---------------------------------------------------------------------------
// vte::Perform implementation
// ---------------------------------------------------------------------------

impl vte::Perform for Screen {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // Carriage return
            0x0D => {
                self.pending_wrap = false;
                self.cursor_x = 0;
            }
            // Line feed / vertical tab / form feed
            0x0A..=0x0C => {
                self.pending_wrap = false;
                if self.cursor_y >= self.scroll_bottom {
                    self.scroll_up_region();
                } else {
                    self.cursor_y += 1;
                }
            }
            // Backspace
            0x08 => {
                self.pending_wrap = false;
                if self.cursor_x > 0 {
                    self.cursor_x -= 1;
                }
            }
            // Horizontal tab
            0x09 => {
                self.pending_wrap = false;
                // Advance to the next tab stop (every 8 columns).
                let next_tab = ((self.cursor_x / 8) + 1) * 8;
                self.cursor_x = std::cmp::min(next_tab, self.cols.saturating_sub(1));
            }
            // Bell
            0x07 => {
                // Flag a pending bell for per-tab activity monitoring. The
                // server consumes this via `take_bell()` after processing
                // output for a background tab.
                self.bell_pending = true;
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let params: Vec<&[u16]> = params.iter().collect();
        let params = params.as_slice();
        match action {
            // CUU - Cursor Up
            'A' => {
                self.pending_wrap = false;
                let n = Self::csi_param(params, 0, 1);
                self.cursor_y = self.cursor_y.saturating_sub(n);
                if self.cursor_y < self.scroll_top {
                    self.cursor_y = self.scroll_top;
                }
            }
            // CUD - Cursor Down
            'B' => {
                self.pending_wrap = false;
                let n = Self::csi_param(params, 0, 1);
                self.cursor_y = std::cmp::min(self.cursor_y + n, self.scroll_bottom);
            }
            // CUF - Cursor Forward (Right)
            'C' => {
                self.pending_wrap = false;
                let n = Self::csi_param(params, 0, 1);
                self.cursor_x = std::cmp::min(self.cursor_x + n, self.cols.saturating_sub(1));
            }
            // CUB - Cursor Backward (Left)
            'D' => {
                self.pending_wrap = false;
                let n = Self::csi_param(params, 0, 1);
                self.cursor_x = self.cursor_x.saturating_sub(n);
            }
            // CNL - Cursor Next Line (move down n lines, column 1)
            'E' => {
                self.pending_wrap = false;
                let n = Self::csi_param(params, 0, 1);
                self.cursor_y = std::cmp::min(self.cursor_y + n, self.scroll_bottom);
                self.cursor_x = 0;
            }
            // CPL - Cursor Previous Line (move up n lines, column 1)
            'F' => {
                self.pending_wrap = false;
                let n = Self::csi_param(params, 0, 1);
                self.cursor_y = self.cursor_y.saturating_sub(n);
                if self.cursor_y < self.scroll_top {
                    self.cursor_y = self.scroll_top;
                }
                self.cursor_x = 0;
            }
            // CUP - Cursor Position (row;col, 1-based)
            'H' | 'f' => {
                self.pending_wrap = false;
                let row = Self::csi_param(params, 0, 1).saturating_sub(1);
                let col = Self::csi_param(params, 1, 1).saturating_sub(1);
                self.cursor_y = std::cmp::min(row, self.rows.saturating_sub(1));
                self.cursor_x = std::cmp::min(col, self.cols.saturating_sub(1));
            }
            // VPA - Vertical Position Absolute (1-based row)
            'd' => {
                self.pending_wrap = false;
                let row = Self::csi_param(params, 0, 1).saturating_sub(1);
                self.cursor_y = std::cmp::min(row, self.rows.saturating_sub(1));
            }
            // HPA / CHA - Horizontal Position Absolute (1-based column)
            'G' => {
                self.pending_wrap = false;
                let col = Self::csi_param(params, 0, 1).saturating_sub(1);
                self.cursor_x = std::cmp::min(col, self.cols.saturating_sub(1));
            }
            // ED - Erase in Display
            'J' => {
                let mode = Self::csi_param(params, 0, 0);
                match mode {
                    0 => {
                        // Erase from cursor to end of display.
                        let row = self.cursor_y as usize;
                        let col = self.cursor_x as usize;
                        if row == 0 && col == 0 {
                            self.history_cleared_from_view();
                        }
                        if row < self.grid.len() {
                            self.erase_cells(row, col, self.grid[row].len());
                            // Erased text cannot continue anywhere: drop the
                            // wrap flag on every row we touched, or reflow would
                            // later glue this row onto whatever is printed next.
                            self.grid[row].wrapped = false;
                            for r in (row + 1)..self.grid.len() {
                                self.grid[r] = self.blank_row();
                            }
                        }
                    }
                    1 => {
                        // Erase from start of display to cursor.
                        let row = self.cursor_y as usize;
                        let col = self.cursor_x as usize;
                        for r in 0..row {
                            self.grid[r] = self.blank_row();
                        }
                        if row < self.grid.len() {
                            self.erase_cells(row, 0, col + 1);
                            self.grid[row].wrapped = false;
                        }
                    }
                    2 => {
                        // Erase entire display.
                        self.grid = vec![self.blank_row(); self.rows as usize];
                        self.history_cleared_from_view();
                    }
                    3 => {
                        // xterm ED 3: erase the saved lines (scrollback).
                        // The visible grid is left untouched; only remux's
                        // scrollback buffer is cleared. This matches
                        // xterm/Alacritty/tmux behavior and makes the `clear`
                        // command (which emits \e[3J via the E3 capability)
                        // truly drop remux's scrollback. total_lines() is
                        // computed on demand, so no cached count to invalidate.
                        //
                        // On the alternate screen this clears nothing (that
                        // screen keeps no saved lines) and deliberately leaves
                        // the primary screen's history -- parked in
                        // `saved_scrollback` -- alone: a full-screen app's
                        // redraw must not throw away the user's shell history.
                        self.scrollback.clear();
                        self.history_cleared_from_view();
                    }
                    _ => {}
                }
            }
            // EL - Erase in Line
            'K' => {
                let mode = Self::csi_param(params, 0, 0);
                let row = self.cursor_y as usize;
                if row >= self.grid.len() {
                    return;
                }
                let cols = self.grid[row].len();
                match mode {
                    0 => {
                        // Erase from cursor to end of line.
                        self.erase_cells(row, self.cursor_x as usize, cols);
                    }
                    1 => {
                        // Erase from start of line to cursor.
                        self.erase_cells(row, 0, self.cursor_x as usize + 1);
                    }
                    2 => {
                        // Erase entire line.
                        self.grid[row] = self.blank_row();
                    }
                    _ => {}
                }
                // Any erase within the line ends it here as far as reflow is
                // concerned. Shells redraw their prompt as `CR text EL`, so a
                // row that used to overflow routinely stops overflowing without
                // ever being fully rewritten -- a stale flag here is the classic
                // "two unrelated lines get joined" bug.
                if mode <= 2 {
                    self.grid[row].wrapped = false;
                }
            }
            // ECH - Erase Characters. Neovim ends every line of a window
            // narrower than the screen with ECH, because EL would also clear
            // the window beside it. Neovim on macOS also uses ECH in a
            // full-width window, to erase only the part of the old text that
            // the new line does not cover; on Linux it uses EL there. If ECH is
            // dropped, a line redrawn after a scroll keeps the old row's text
            // past its own end, so the row reads as a copy of the line that was
            // there before.
            'X' => {
                let n = Self::csi_param(params, 0, 1) as usize;
                let row = self.cursor_y as usize;
                if row >= self.grid.len() {
                    return;
                }
                let start = self.cursor_x as usize;
                self.erase_cells(row, start, start.saturating_add(n));
                // As in xterm: an erase cancels a deferred wrap, so the next
                // character lands in the last column instead of on a new row.
                self.pending_wrap = false;
                // Text that ends before the row's last cell no longer runs on
                // into the next row, and reflow would otherwise join them.
                if start.saturating_add(n) >= self.grid[row].len() {
                    self.grid[row].wrapped = false;
                }
            }
            // IL - Insert Lines
            'L' => {
                let n = Self::csi_param(params, 0, 1) as usize;
                let row = self.cursor_y as usize;
                let bottom = self.scroll_bottom as usize;

                for _ in 0..n {
                    if row <= bottom && bottom < self.grid.len() {
                        self.grid.remove(bottom);
                        self.grid.insert(row, self.blank_row());
                    }
                }
                // Blank lines were pushed in at `row`, so the row above no
                // longer continues into it.
                self.break_wrap_above(row);
            }
            // DL - Delete Lines
            'M' => {
                let n = Self::csi_param(params, 0, 1) as usize;
                let row = self.cursor_y as usize;
                let bottom = self.scroll_bottom as usize;

                for _ in 0..n {
                    if row <= bottom && row < self.grid.len() {
                        self.grid.remove(row);
                        self.grid.insert(bottom, self.blank_row());
                    }
                }
                // Lines were pulled up into `row`; whatever the row above used
                // to continue into is gone.
                self.break_wrap_above(row);
            }
            // ICH - Insert Characters
            '@' => {
                // As in xterm, the shifted row no longer ends in the character
                // that left the wrap pending.
                self.pending_wrap = false;
                let n = Self::csi_param(params, 0, 1) as usize;
                let row = self.cursor_y as usize;
                let col = self.cursor_x as usize;
                if row < self.grid.len() && col < self.grid[row].len() {
                    let len = self.grid[row].len();
                    let n = n.min(len - col);
                    // As tmux does: break a wide glyph the cursor sits inside,
                    // and the one the first dropped cell belongs to, so the
                    // shift neither moves a lone half nor drops a continuation
                    // off the row behind a lead left in the last column.
                    self.break_wide_at(row, col);
                    self.break_wide_at(row, len - n);
                    let blank = self.blank();
                    let line = &mut self.grid[row];
                    line.splice(col..col, std::iter::repeat_n(blank, n));
                    line.truncate(len);
                }
            }
            // DCH - Delete Characters
            'P' => {
                self.pending_wrap = false;
                let n = Self::csi_param(params, 0, 1) as usize;
                let row = self.cursor_y as usize;
                let col = self.cursor_x as usize;
                if row < self.grid.len() && col < self.grid[row].len() {
                    let len = self.grid[row].len();
                    let n = n.min(len - col);
                    // Break the glyph the cursor sits inside, and the one whose
                    // continuation the deletion would pull to the cursor.
                    self.break_wide_at(row, col);
                    self.break_wide_at(row, col + n);
                    let blank = self.blank();
                    let line = &mut self.grid[row];
                    line.drain(col..col + n);
                    line.resize(len, blank);
                }
            }
            // SU - Scroll Up
            'S' => {
                let n = Self::csi_param(params, 0, 1);
                for _ in 0..n {
                    self.scroll_up_region();
                }
            }
            // SD - Scroll Down
            'T' => {
                let n = Self::csi_param(params, 0, 1);
                for _ in 0..n {
                    self.scroll_down_region();
                }
            }
            // DECSTBM - Set Scroll Region (top;bottom, 1-based)
            'r' => {
                self.pending_wrap = false;
                let top = Self::csi_param(params, 0, 1).saturating_sub(1);
                let bottom = Self::csi_param(params, 1, self.rows).saturating_sub(1);
                let bottom = std::cmp::min(bottom, self.rows.saturating_sub(1));
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                }
                // Reset cursor to home on scroll region change.
                self.cursor_x = 0;
                self.cursor_y = 0;
            }
            // SCP - Save Cursor Position (CSI s, no intermediates)
            's' if _intermediates.is_empty() => {
                self.scp_cursor_x = self.cursor_x;
                self.scp_cursor_y = self.cursor_y;
            }
            // RCP - Restore Cursor Position (CSI u, no intermediates)
            'u' if _intermediates.is_empty() => {
                self.pending_wrap = false;
                self.cursor_x = self.scp_cursor_x;
                self.cursor_y = self.scp_cursor_y;
            }
            // SGR - Select Graphic Rendition. A private/intermediate prefix (e.g.
            // `CSI > 4 m` = XTMODKEYS/modifyOtherKeys) is NOT SGR; ignore it so its
            // numeric params are never misparsed as graphic attributes (was latching
            // underline on permanently).
            'm' if _intermediates.is_empty() => {
                self.handle_sgr(params);
            }
            // SM/DECSET - Set Mode
            'h' => {
                let is_private = _intermediates.first() == Some(&b'?');
                if is_private {
                    for param_slice in params {
                        let mode = param_slice[0];
                        match mode {
                            1 => self.application_cursor_keys = true,
                            // 2004 = bracketed paste. Until this arm existed the
                            // mode fell into the catch-all below and the client
                            // wrapped every paste regardless.
                            2004 => self.bracketed_paste = true,
                            25 => self.cursor_visible = true,
                            2026 => self.lock_renders = true,
                            // Mouse tracking: 1000=normal, 1002=button-event, 1003=any-event.
                            1000 | 1002 | 1003 => {
                                self.mouse_tracking = true;
                                if mode != 1000 {
                                    self.mouse_motion = true;
                                }
                            }
                            // 1006 = SGR mouse encoding.
                            1006 => self.mouse_sgr = true,
                            1049 => {
                                // Save cursor and switch to alternate screen
                                if !self.alt_screen_active {
                                    self.saved_grid = Some(self.grid.clone());
                                    self.saved_scrollback =
                                        Some(std::mem::take(&mut self.scrollback));
                                    self.saved_cursor_x = self.cursor_x;
                                    self.saved_cursor_y = self.cursor_y;
                                    self.saved_attrs = self.current_attrs.clone();
                                    self.saved_hyperlink = self.current_hyperlink.clone();
                                    self.saved_scroll_top = self.scroll_top;
                                    self.saved_scroll_bottom = self.scroll_bottom;
                                    // As in tmux, the alternate screen starts
                                    // in the default background, not the BCE one.
                                    self.grid = Self::make_grid(self.cols, self.rows);
                                    self.cursor_x = 0;
                                    self.cursor_y = 0;
                                    self.scroll_top = 0;
                                    self.scroll_bottom = self.rows.saturating_sub(1);
                                    self.alt_screen_active = true;
                                }
                            }
                            1047 if !self.alt_screen_active => {
                                // Switch to alternate screen (no cursor save)
                                self.saved_grid = Some(self.grid.clone());
                                self.saved_scrollback = Some(std::mem::take(&mut self.scrollback));
                                self.saved_scroll_top = self.scroll_top;
                                self.saved_scroll_bottom = self.scroll_bottom;
                                // As in tmux, the alternate screen starts in the
                                // default background, not the BCE one.
                                self.grid = Self::make_grid(self.cols, self.rows);
                                self.cursor_x = 0;
                                self.cursor_y = 0;
                                self.scroll_top = 0;
                                self.scroll_bottom = self.rows.saturating_sub(1);
                                self.alt_screen_active = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
            // RM/DECRST - Reset Mode
            'l' => {
                let is_private = _intermediates.first() == Some(&b'?');
                if is_private {
                    for param_slice in params {
                        let mode = param_slice[0];
                        match mode {
                            1 => self.application_cursor_keys = false,
                            2004 => self.bracketed_paste = false,
                            25 => self.cursor_visible = false,
                            2026 => self.lock_renders = false,
                            // Mouse tracking: 1000=normal, 1002=button-event, 1003=any-event.
                            1000 | 1002 | 1003 => {
                                self.mouse_tracking = false;
                                self.mouse_motion = false;
                            }
                            // 1006 = SGR mouse encoding.
                            1006 => self.mouse_sgr = false,
                            1049 => {
                                // Restore primary screen and cursor
                                if self.alt_screen_active {
                                    if let Some(grid) = self.saved_grid.take() {
                                        self.grid = grid;
                                    }
                                    if let Some(scrollback) = self.saved_scrollback.take() {
                                        self.scrollback = scrollback;
                                    }
                                    self.cursor_x = self.saved_cursor_x;
                                    self.cursor_y = self.saved_cursor_y;
                                    self.current_attrs = self.saved_attrs.clone();
                                    self.current_hyperlink = self.saved_hyperlink.clone();
                                    self.scroll_top = self.saved_scroll_top;
                                    self.scroll_bottom = self.saved_scroll_bottom;
                                    self.alt_screen_active = false;
                                    // Safety: mouse-tracking apps are almost always
                                    // alt-screen apps that enable/disable both together.
                                    // Reset the mouse flags when leaving the alt screen
                                    // so a crashed/exited app that never sent the
                                    // matching DECRST doesn't leave the wheel forwarding
                                    // escape sequences to the shell instead of scrolling.
                                    self.mouse_tracking = false;
                                    self.mouse_motion = false;
                                    self.mouse_sgr = false;
                                    // `bracketed_paste` is deliberately NOT reset
                                    // here, and the asymmetry with the mouse flags
                                    // above is the cost of being wrong. A stuck
                                    // mouse flag makes the WHEEL unusable for as
                                    // long as the pane lives, and nothing ever
                                    // clears it. A stuck 2004 costs one paste with
                                    // visible markers, and both readline and zle
                                    // re-emit 2004h/2004l around every line they
                                    // read -- so the next shell prompt overwrites
                                    // whatever a crashed app left behind. Clearing
                                    // it would break the common case outright: a
                                    // shell with paste legitimately armed is then
                                    // one alt-screen command away from silently
                                    // losing it. A real terminal leaves it set.
                                }
                            }
                            1047 if self.alt_screen_active => {
                                // Switch back to primary screen
                                if let Some(grid) = self.saved_grid.take() {
                                    self.grid = grid;
                                }
                                if let Some(scrollback) = self.saved_scrollback.take() {
                                    self.scrollback = scrollback;
                                }
                                self.scroll_top = self.saved_scroll_top;
                                self.scroll_bottom = self.saved_scroll_bottom;
                                self.alt_screen_active = false;
                                // Safety: reset mouse flags on leaving the alt screen
                                // (see mode 1049 above) to recover the common
                                // "app gone, flag stuck" case. `bracketed_paste` is
                                // deliberately excluded, for the reason recorded
                                // there.
                                self.mouse_tracking = false;
                                self.mouse_motion = false;
                                self.mouse_sgr = false;
                            }
                            _ => {}
                        }
                    }
                }
            }
            // DSR - Device Status Report
            'n' => {
                let mode = Self::csi_param(params, 0, 0);
                match mode {
                    6 => {
                        // Report cursor position: ESC [ <row> ; <col> R (1-based)
                        let response =
                            format!("\x1b[{};{}R", self.cursor_y + 1, self.cursor_x + 1,);
                        self.pty_responses.push(response.into_bytes());
                    }
                    5 => {
                        // Device status - report "OK"
                        self.pty_responses.push(b"\x1b[0n".to_vec());
                    }
                    _ => {}
                }
            }
            // DECSCUSR - Set Cursor Style (CSI Ps SP q)
            'q' if _intermediates.first() == Some(&b' ') => {
                let style = Self::csi_param(params, 0, 0);
                self.cursor_style = style as u8;
            }
            _ => {
                // Unknown CSI sequence - ignore.
            }
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            // RIS - Full Reset
            b'c' => {
                let cols = self.cols;
                let rows = self.rows;
                let limit = self.scrollback_limit;
                let parser = std::mem::replace(&mut self.parser, vte::Parser::new());
                *self = Screen::new(cols, rows, limit);
                self.parser = parser;
            }
            // IND - Index (move cursor down, scroll if at bottom)
            b'D' => {
                self.pending_wrap = false;
                if self.cursor_y >= self.scroll_bottom {
                    self.scroll_up_region();
                } else {
                    self.cursor_y += 1;
                }
            }
            // DECSC - Save Cursor (position + graphic rendition)
            b'7' => {
                self.scp_cursor_x = self.cursor_x;
                self.scp_cursor_y = self.cursor_y;
                self.scp_attrs = self.current_attrs.clone();
                self.scp_hyperlink = self.current_hyperlink.clone();
            }
            // DECRC - Restore Cursor (position + graphic rendition)
            b'8' => {
                self.pending_wrap = false;
                self.cursor_x = self.scp_cursor_x;
                self.cursor_y = self.scp_cursor_y;
                self.current_attrs = self.scp_attrs.clone();
                self.current_hyperlink = self.scp_hyperlink.clone();
            }
            // RI - Reverse Index (move cursor up, scroll if at top)
            b'M' => {
                self.pending_wrap = false;
                if self.cursor_y <= self.scroll_top {
                    self.scroll_down_region();
                } else {
                    self.cursor_y -= 1;
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 8 hyperlinks: `OSC 8 ; params ; URI ST`. Empty URI closes the link.
        if params.first().map(|p| *p == b"8").unwrap_or(false) {
            // The URI is everything after the second `;`. vte splits on `;`, so a
            // URI containing `;` arrives as multiple params — rejoin with `;`.
            let uri: Vec<u8> = params
                .get(2..)
                .map(|rest| rest.join(&b';'))
                .unwrap_or_default();
            self.current_hyperlink = if uri.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&uri).into_owned())
            };
        } else if params.first().map(|p| *p == b"52").unwrap_or(false) {
            self.osc52_clipboard(params);
        } else if params.first().is_some_and(|p| *p == b"0" || *p == b"2") {
            // OSC 1 sets only the icon name, which nothing here shows.
            let raw: Vec<u8> = params
                .get(1..)
                .map(|rest| rest.join(&b';'))
                .unwrap_or_default();
            self.title_pending = Some(sanitize_title(&String::from_utf8_lossy(&raw)));
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        // DCS hook - not yet implemented.
    }

    fn unhook(&mut self) {
        // DCS unhook - not yet implemented.
    }

    fn put(&mut self, _byte: u8) {
        // DCS put - not yet implemented.
    }
}

/// `raw` without control or invisible format characters, cut to a label's
/// length. An escape sequence inside a title would otherwise reach every
/// client that draws the pane's name. See
/// [`crate::config::pane_title::sanitize_label`].
fn sanitize_title(raw: &str) -> String {
    crate::config::pane_title::sanitize_label(raw)
}

// ---------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------

/// Decode standard-alphabet base64, returning `None` for anything malformed.
///
/// Hand-rolled (Remux takes no base64 dependency) and deliberately strict: the
/// only caller feeds it untrusted bytes straight off a PTY, so a bad payload
/// must be rejected outright rather than guessed at — and must never panic.
/// Padding is optional, so both `=`-padded and unpadded input decode, but `=`
/// anywhere other than the tail, any byte outside the alphabet (whitespace
/// included), a padding run that does not complete a 4-byte group, or a length
/// that cannot encode a whole byte (`len % 4 == 1`) rejects the whole payload.
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn sextet(b: u8) -> Option<u32> {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        Some(u32::from(v))
    }

    let (body, pad) = if let Some(b) = input.strip_suffix(b"==") {
        (b, 2)
    } else if let Some(b) = input.strip_suffix(b"=") {
        (b, 1)
    } else {
        (input, 0)
    };
    // Padding, when present, must land the payload on a whole 4-byte group, and
    // may only appear at the tail (`sextet` rejects a `=` inside `body`).
    if pad > 0 && (body.len() + pad) % 4 != 0 {
        return None;
    }
    if body.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in body {
        acc = (acc << 6) | sextet(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_screen() -> Screen {
        Screen::new(80, 24, 1000)
    }

    #[test]
    fn test_abs_row_roundtrip() {
        // Build a screen with some scrollback so the mapping is non-trivial.
        let mut s = Screen::new(10, 5, 1000);
        for i in 0..30 {
            s.process_output(format!("line{i}\r\n").as_bytes());
        }
        assert!(
            s.scrollback.len() >= 5,
            "need scrollback for the round-trip"
        );

        // For several scroll offsets, abs_of_row and row_of_abs must invert each
        // other for every in-range viewport row.
        for &scroll in &[0usize, 1, 3, s.max_scroll_offset()] {
            for row in 0..s.rows {
                let abs = s.abs_of_row(scroll, row);
                assert_eq!(
                    s.row_of_abs(scroll, abs),
                    row as i64,
                    "row_of_abs(abs_of_row(r)) must equal r (scroll={scroll}, row={row})"
                );
            }
        }
    }

    #[test]
    fn test_abs_of_row_increases_by_one_per_row() {
        let mut s = Screen::new(10, 5, 1000);
        for i in 0..30 {
            s.process_output(format!("line{i}\r\n").as_bytes());
        }
        for &scroll in &[0usize, 2, s.max_scroll_offset()] {
            for row in 1..s.rows {
                assert_eq!(
                    s.abs_of_row(scroll, row),
                    s.abs_of_row(scroll, row - 1) + 1,
                    "abs id must advance by exactly one per viewport row"
                );
            }
        }
    }

    #[test]
    fn test_eviction_keeps_abs_pointing_at_same_line() {
        // Scrollback limit forces eviction; a captured abs must keep resolving to
        // the same content line via array_index_of_abs after older lines drop.
        // Retained window = scrollback_limit (5) + grid rows (3) = 8 lines, so a
        // line captured then followed by only two more output lines survives.
        let mut s = Screen::new(10, 3, 5);
        for i in 0..8 {
            s.process_output(format!("L{i}\r\n").as_bytes());
        }
        let evicted_before = s.lines_evicted;

        // Find the combined-buffer index of the line reading "L6" and turn it
        // into a stable absolute id.
        let idx = (0..s.total_lines())
            .find(|&i| {
                let text: String = s.line_at(i).unwrap().iter().map(|c| c.c).collect();
                text.trim_end() == "L6"
            })
            .expect("L6 must still be present");
        let abs = s.lines_evicted + idx;

        // Produce two more lines, forcing further eviction from the front while
        // keeping "L6" inside the retained window.
        for i in 8..10 {
            s.process_output(format!("L{i}\r\n").as_bytes());
        }
        assert!(
            s.lines_evicted > evicted_before,
            "further output must evict lines and bump lines_evicted"
        );

        // The same abs id must still resolve to the same content line.
        let after: String = s
            .line_at(s.array_index_of_abs(abs))
            .unwrap()
            .iter()
            .map(|c| c.c)
            .collect();
        assert_eq!(
            after.trim_end(),
            "L6",
            "a captured abs id must keep pointing at the same content line across eviction"
        );
    }

    #[test]
    fn test_pending_wrap_defers() {
        // Fill an entire 5-column row. The cursor must stay parked on the last
        // column of row 0 (deferred wrap), NOT advance to row 1.
        let mut s = Screen::new(5, 3, 100);
        s.process_output(b"ABCDE");
        assert_eq!(s.cursor_y, 0, "cursor must stay on row 0 after filling it");
        assert_eq!(
            s.cursor_x, 4,
            "cursor must be parked on the last column, not out of bounds"
        );
        let row0: String = s.grid[0].iter().map(|c| c.c).collect();
        assert_eq!(row0, "ABCDE", "row 0 should hold all 5 chars");

        // The NEXT printable char triggers the deferred wrap: it lands at the
        // start of row 1.
        s.process_output(b"F");
        assert_eq!(s.cursor_y, 1, "next char must wrap to row 1");
        assert_eq!(s.grid[1][0].c, 'F', "wrapped char lands at row 1, col 0");

        // A cursor-move between the full row and the next char CANCELS the
        // pending wrap: no spurious blank line / no wrap.
        let mut s = Screen::new(5, 3, 100);
        s.process_output(b"ABCDE");
        assert_eq!(s.cursor_y, 0);
        s.process_output(b"\x1b[5G"); // CHA to column 5 (last col) — cancels pending wrap
        s.process_output(b"X");
        assert_eq!(
            s.cursor_y, 0,
            "cursor-move must cancel pending wrap; char must not wrap to row 1"
        );
        assert_eq!(
            s.grid[0][4].c, 'X',
            "char lands on row 0 last column after the cursor-move cancelled the wrap"
        );
    }

    #[test]
    fn test_new_screen() {
        let s = make_screen();
        assert_eq!(s.cols, 80);
        assert_eq!(s.rows, 24);
        assert_eq!(s.grid.len(), 24);
        assert_eq!(s.grid[0].len(), 80);
        assert_eq!(s.cursor_x, 0);
        assert_eq!(s.cursor_y, 0);
        assert_eq!(s.scroll_top, 0);
        assert_eq!(s.scroll_bottom, 23);
    }

    #[test]
    fn test_print_char() {
        let mut s = make_screen();
        s.process_output(b"A");
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.cursor_x, 1);
    }

    #[test]
    fn test_bell_sets_pending_flag() {
        let mut s = make_screen();
        assert!(!s.bell_pending);
        // BEL (0x07) should raise the pending-bell flag.
        s.process_output(b"\x07");
        assert!(s.bell_pending);
    }

    #[test]
    fn test_take_bell_consumes_flag() {
        let mut s = make_screen();
        s.process_output(b"hi\x07there");
        // First take returns true and clears the flag.
        assert!(s.take_bell());
        assert!(!s.bell_pending);
        // Second take returns false (already consumed).
        assert!(!s.take_bell());
        // BEL should not disturb normal text output.
        assert_eq!(s.grid[0][0].c, 'h');
    }

    // -----------------------------------------------------------------------
    // OSC 52 clipboard + base64
    // -----------------------------------------------------------------------

    /// Encode with the same alphabet `base64_decode` accepts, so the round-trip
    /// tests do not depend on a hand-written literal being right.
    fn b64(input: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(A[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn test_base64_decode_padded_and_unpadded() {
        // Two `=`, one `=`, and no padding needed.
        assert_eq!(base64_decode(b"YQ==").unwrap(), b"a");
        assert_eq!(base64_decode(b"YWI=").unwrap(), b"ab");
        assert_eq!(base64_decode(b"YWJj").unwrap(), b"abc");
        // The same payloads with the padding stripped decode identically.
        assert_eq!(base64_decode(b"YQ").unwrap(), b"a");
        assert_eq!(base64_decode(b"YWI").unwrap(), b"ab");
        // Empty in, empty out (the caller treats this as "nothing to copy").
        assert_eq!(base64_decode(b"").unwrap(), b"");
    }

    #[test]
    fn test_base64_decode_rejects_malformed() {
        // A length of 4n+1 cannot encode a whole byte.
        assert!(base64_decode(b"YWJjZ").is_none());
        // Bytes outside the alphabet -- including whitespace, which some encoders
        // wrap lines with but an OSC payload must never contain.
        assert!(base64_decode(b"YW*j").is_none());
        assert!(base64_decode(b"YW Jj").is_none());
        assert!(base64_decode(b"YWJj\n").is_none());
        // `=` anywhere but the tail.
        assert!(base64_decode(b"Y=Jj").is_none());
        // Padding that does not complete a 4-byte group.
        assert!(base64_decode(b"YWJ==").is_none());
        // A clipboard READ request is not base64 and must not decode.
        assert!(base64_decode(b"?").is_none());
    }

    #[test]
    fn osc_0_and_2_set_the_title_with_either_terminator() {
        let mut s = make_screen();
        s.process_output(b"\x1b]0;bel form\x07");
        assert_eq!(s.take_title().as_deref(), Some("bel form"));
        assert_eq!(s.take_title(), None);
        s.process_output(b"\x1b]2;st form\x1b\\");
        assert_eq!(s.take_title().as_deref(), Some("st form"));
    }

    #[test]
    fn osc_1_sets_no_title() {
        let mut s = make_screen();
        s.process_output(b"\x1b]1;icon\x07");
        assert_eq!(s.take_title(), None);
    }

    #[test]
    fn a_title_keeps_its_semicolons_and_the_last_write_wins() {
        let mut s = make_screen();
        s.process_output(b"\x1b]2;first\x07\x1b]2;a;b;c\x07");
        assert_eq!(s.take_title().as_deref(), Some("a;b;c"));
    }

    #[test]
    fn an_empty_title_is_reported_as_a_clear() {
        let mut s = make_screen();
        s.process_output(b"\x1b]0;\x07");
        assert_eq!(s.take_title().as_deref(), Some(""));
    }

    #[test]
    fn sanitize_title_drops_controls_and_caps_length() {
        assert_eq!(sanitize_title("a\u{1b}b\tc\u{85}d"), "abcd");
        assert_eq!(
            sanitize_title("Ex\u{202e}erolp\u{202c}\u{200b}\u{feff}\u{2066}!"),
            "Exerolp!",
            "bidi controls and zero-width characters go"
        );
        assert_eq!(sanitize_title("a\u{200d}b\u{200c}c"), "a\u{200d}b\u{200c}c");
        let max = crate::config::pane_title::MAX_LABEL_CHARS;
        let long = "x".repeat(max + 10);
        assert_eq!(sanitize_title(&long).chars().count(), max);
    }

    #[test]
    fn test_osc52_write_reaches_clipboard() {
        let mut s = make_screen();
        s.process_output(format!("\x1b]52;c;{}\x07", b64(b"REMUX_CLIP_7788")).as_bytes());
        assert_eq!(s.take_clipboard().as_deref(), Some("REMUX_CLIP_7788"));
        // Consumed: a second take yields nothing.
        assert_eq!(s.take_clipboard(), None);
    }

    #[test]
    fn test_osc52_accepts_st_terminator_and_selection_targets() {
        // ST-terminated rather than BEL-terminated.
        let mut s = make_screen();
        s.process_output(format!("\x1b]52;c;{}\x1b\\", b64(b"st-form")).as_bytes());
        assert_eq!(s.take_clipboard().as_deref(), Some("st-form"));

        // Primary selection, the combined list, and an empty (default) target all
        // mean "the thing the user pastes with".
        for targets in ["p", "s", "cp", ""] {
            let mut s = make_screen();
            s.process_output(format!("\x1b]52;{targets};{}\x07", b64(b"sel")).as_bytes());
            assert_eq!(
                s.take_clipboard().as_deref(),
                Some("sel"),
                "targets {targets:?} should be honored"
            );
        }

        // A request aimed only at the numbered cut buffers has no clipboard
        // meaning here and is ignored.
        let mut s = make_screen();
        s.process_output(format!("\x1b]52;0;{}\x07", b64(b"cutbuf")).as_bytes());
        assert_eq!(s.take_clipboard(), None);
    }

    #[test]
    fn test_osc52_read_request_is_ignored() {
        // `?` asks the terminal to hand the clipboard BACK to the application.
        // Serving it would let any program exfiltrate what the user copied, so it
        // must set no clipboard and, critically, queue no PTY response.
        let mut s = make_screen();
        s.process_output(b"\x1b]52;c;?\x07");
        assert_eq!(s.take_clipboard(), None);
        assert!(s.take_responses().is_empty());
    }

    #[test]
    fn test_osc52_invalid_and_oversized_payloads_are_dropped() {
        // Garbage base64: ignored, no panic.
        let mut s = make_screen();
        s.process_output(b"\x1b]52;c;!!!not base64!!!\x07");
        assert_eq!(s.take_clipboard(), None);

        // An empty payload clears the selection; there is nothing to copy.
        let mut s = make_screen();
        s.process_output(b"\x1b]52;c;\x07");
        assert_eq!(s.take_clipboard(), None);

        // Over the cap: dropped rather than decoded.
        let mut s = make_screen();
        let huge = "A".repeat(MAX_OSC52_BASE64_LEN + 4);
        s.process_output(format!("\x1b]52;c;{huge}\x07").as_bytes());
        assert_eq!(s.take_clipboard(), None);
    }

    #[test]
    fn test_osc52_multi_kilobyte_payload_round_trips() {
        // The reported case is a TUI app copying a real block of text, not a
        // 15-byte marker: 8 KiB has to survive the OSC parser's buffering intact
        // or the clipboard silently gets a truncated fragment.
        let mut s = make_screen();
        let text: String = (0..512)
            .map(|i| format!("line {i:04} of copied text\n"))
            .collect();
        assert!(
            text.len() > 8 * 1024,
            "payload must exceed the parser's buffer"
        );
        s.process_output(format!("\x1b]52;c;{}\x07", b64(text.as_bytes())).as_bytes());
        assert_eq!(s.take_clipboard().as_deref(), Some(text.as_str()));
    }

    #[test]
    fn test_osc52_last_write_wins_within_one_batch() {
        // A batch of PTY output holding several clipboard writes leaves the last,
        // as a real terminal would -- not a queue that thrashes the clipboard.
        let mut s = make_screen();
        s.process_output(
            format!(
                "\x1b]52;c;{}\x07\x1b]52;c;{}\x07",
                b64(b"first"),
                b64(b"second")
            )
            .as_bytes(),
        );
        assert_eq!(s.take_clipboard().as_deref(), Some("second"));
    }

    #[test]
    fn test_osc52_does_not_disturb_osc8_hyperlinks() {
        // The two OSC handlers compose: a clipboard write between a link's open
        // and close must not clear or capture the active hyperlink.
        let mut s = make_screen();
        s.process_output(b"\x1b]8;;https://example.com/a;b\x1b\\");
        s.process_output(format!("\x1b]52;c;{}\x07", b64(b"clip")).as_bytes());
        s.process_output(b"X");
        assert_eq!(s.take_clipboard().as_deref(), Some("clip"));
        assert_eq!(
            s.grid[0][0].hyperlink.as_deref(),
            Some("https://example.com/a;b")
        );
        s.process_output(b"\x1b]8;;\x1b\\");
        assert_eq!(s.current_hyperlink, None);
    }

    #[test]
    fn test_mouse_tracking_decset() {
        let mut s = make_screen();
        assert!(!s.mouse_tracking);
        assert!(!s.mouse_sgr);

        // Enable normal mouse tracking (1000) and SGR encoding (1006).
        s.process_output(b"\x1b[?1000h");
        s.process_output(b"\x1b[?1006h");
        assert!(s.mouse_tracking);
        assert!(s.mouse_sgr);

        // Disable both.
        s.process_output(b"\x1b[?1000l");
        s.process_output(b"\x1b[?1006l");
        assert!(!s.mouse_tracking);
        assert!(!s.mouse_sgr);

        // Button-event (1002) and any-event (1003) also set mouse_tracking, and
        // additionally ask for MOTION reports -- which plain 1000 does not, so a
        // drag over a 1000-only app is not forwarded as motion.
        s.process_output(b"\x1b[?1000h");
        assert!(s.mouse_tracking);
        assert!(!s.mouse_motion);
        s.process_output(b"\x1b[?1000l");

        s.process_output(b"\x1b[?1002h");
        assert!(s.mouse_tracking);
        assert!(s.mouse_motion);
        s.process_output(b"\x1b[?1002l");
        assert!(!s.mouse_tracking);
        assert!(!s.mouse_motion);
        s.process_output(b"\x1b[?1003h");
        assert!(s.mouse_tracking);
        assert!(s.mouse_motion);
    }

    #[test]
    fn test_leaving_alt_screen_resets_mouse_tracking() {
        let mut s = make_screen();

        // Enter alt screen (1049) and enable mouse tracking (1000) + SGR (1006),
        // simulating a TUI app.
        s.process_output(b"\x1b[?1049h");
        s.process_output(b"\x1b[?1002h");
        s.process_output(b"\x1b[?1006h");
        assert!(s.alt_screen_active);
        assert!(s.mouse_tracking);
        assert!(s.mouse_motion);
        assert!(s.mouse_sgr);

        // The app exits/crashes and only the alt-screen reset (1049l) arrives,
        // without the matching mouse DECRSTs. Leaving the alt screen must clear
        // the stuck mouse flags.
        s.process_output(b"\x1b[?1049l");
        assert!(!s.alt_screen_active);
        assert!(!s.mouse_tracking);
        assert!(!s.mouse_motion);
        assert!(!s.mouse_sgr);
    }

    #[test]
    fn test_bracketed_paste_decset() {
        let mut s = make_screen();
        assert!(!s.bracketed_paste);

        // The handshake half remux was missing: an application asks for
        // bracketed paste with `CSI ? 2004 h` and gives it up with `... l`.
        s.process_output(b"\x1b[?2004h");
        assert!(s.bracketed_paste);
        s.process_output(b"\x1b[?2004l");
        assert!(!s.bracketed_paste);

        // An unrelated private mode must not disturb it in either direction --
        // 2004 used to fall into the catch-all arm with every other unknown
        // mode, so "the flag is untouched" is exactly the old behaviour and
        // needs pinning as the NEW behaviour too.
        s.process_output(b"\x1b[?2004h");
        s.process_output(b"\x1b[?1000h");
        s.process_output(b"\x1b[?25l");
        assert!(s.bracketed_paste);
    }

    #[test]
    fn test_bracketed_paste_is_terminal_state_not_screen_state() {
        let mut s = make_screen();

        // Judgement call 1: 2004 does NOT follow the alt-screen switch. It is a
        // TERMINAL-level mode -- what the terminal does to input -- not a
        // property of the screen buffer, which is why `Screen` keeps it beside
        // DECCKM (mode 1) rather than beside the saved grid. A shell that armed
        // bracketed paste on the primary screen still has it armed when a
        // full-screen app that never touched 2004 exits.
        s.process_output(b"\x1b[?2004h");
        s.process_output(b"\x1b[?1049h");
        assert!(s.alt_screen_active);
        assert!(s.bracketed_paste);
        s.process_output(b"\x1b[?1049l");
        assert!(!s.alt_screen_active);
        assert!(s.bracketed_paste);

        // Judgement call 2: an app that dies with the flag set does NOT get it
        // cleared for it, unlike the mouse flags one arm above (see
        // `test_leaving_alt_screen_resets_mouse_tracking`). The asymmetry is the
        // cost of being wrong. A stuck mouse flag makes the WHEEL unusable for
        // as long as the pane lives and nothing ever resets it; a stuck 2004
        // costs a single paste with visible markers, and both readline and zle
        // re-emit `2004h`/`2004l` around every line they read, so the very next
        // shell prompt overwrites whatever the corpse left behind. Clearing it
        // would instead break the common case outright -- a shell with paste
        // legitimately armed, one alt-screen command away from silently losing
        // it. A real terminal leaves it set; so do we.
        s.process_output(b"\x1b[?2004l");
        assert!(!s.bracketed_paste);
        s.process_output(b"\x1b[?1049h");
        s.process_output(b"\x1b[?2004h");
        s.process_output(b"\x1b[?1049l");
        assert!(s.bracketed_paste);
    }

    #[test]
    fn test_print_string() {
        let mut s = make_screen();
        s.process_output(b"Hello");
        assert_eq!(s.grid[0][0].c, 'H');
        assert_eq!(s.grid[0][1].c, 'e');
        assert_eq!(s.grid[0][2].c, 'l');
        assert_eq!(s.grid[0][3].c, 'l');
        assert_eq!(s.grid[0][4].c, 'o');
        assert_eq!(s.cursor_x, 5);
    }

    #[test]
    fn test_wide_char_placement() {
        let mut s = make_screen();
        // '中' (U+4E2D) is unambiguously display-width 2.
        s.process_output("中".as_bytes());
        assert_eq!(s.grid[0][0].c, '中');
        assert_eq!(s.grid[0][0].width, 2);
        // Continuation spacer in the next column.
        assert_eq!(s.grid[0][1].c, ' ');
        assert_eq!(s.grid[0][1].width, 0);
        // Cursor advanced by two columns.
        assert_eq!(s.cursor_x, 2);
    }

    #[test]
    fn test_combining_mark_attaches_to_base() {
        let mut s = make_screen();
        s.process_output(b"e");
        // U+0301 COMBINING ACUTE ACCENT is display-width 0; it attaches to the
        // base glyph to the left of the cursor without advancing the cursor or
        // consuming a column (so `e` + U+0301 renders as `é`).
        s.process_output("\u{0301}".as_bytes());
        assert_eq!(s.grid[0][0].c, 'e');
        assert_eq!(s.grid[0][0].width, 1);
        assert_eq!(s.grid[0][0].combining, vec!['\u{301}']);
        // Cursor advanced by exactly 1 (the base), not 2.
        assert_eq!(s.cursor_x, 1);
    }

    #[test]
    fn test_combining_mark_at_column_zero_dropped() {
        let mut s = make_screen();
        // A combining mark with no preceding base on the row is dropped.
        s.process_output("\u{0301}".as_bytes());
        assert_eq!(s.cursor_x, 0);
        assert!(s.grid[0][0].combining.is_empty());
        assert_eq!(s.grid[0][0].c, ' ');
    }

    #[test]
    fn test_new_base_clears_previous_combining() {
        let mut s = make_screen();
        // Build 'e' + combining acute at col 0.
        s.process_output(b"e");
        s.process_output("\u{0301}".as_bytes());
        assert_eq!(s.grid[0][0].combining, vec!['\u{301}']);
        // Overwrite the cell with a fresh base char; combining must reset.
        s.process_output(b"\rX");
        assert_eq!(s.grid[0][0].c, 'X');
        assert!(s.grid[0][0].combining.is_empty());
    }

    #[test]
    fn test_multiple_combining_marks_accumulate() {
        let mut s = make_screen();
        s.process_output(b"a");
        // Two combining marks stack onto the same base.
        s.process_output("\u{0301}".as_bytes());
        s.process_output("\u{0308}".as_bytes());
        assert_eq!(s.grid[0][0].c, 'a');
        assert_eq!(s.grid[0][0].combining, vec!['\u{301}', '\u{308}']);
        assert_eq!(s.cursor_x, 1);
    }

    #[test]
    fn test_wide_char_wraps_at_last_column() {
        let mut s = make_screen();
        // Fill the first 79 columns, leaving the cursor at the last column (79).
        for _ in 0..79 {
            s.process_output(b"a");
        }
        assert_eq!(s.cursor_x, 79);
        // A wide glyph cannot fit in the single remaining column, so it wraps to
        // the next row and leaves the last cell of row 0 blank.
        s.process_output("中".as_bytes());
        assert_eq!(s.grid[0][79].c, ' ');
        assert_eq!(s.grid[0][79].width, 1);
        assert_eq!(s.grid[1][0].c, '中');
        assert_eq!(s.grid[1][0].width, 2);
        assert_eq!(s.grid[1][1].width, 0);
        assert_eq!(s.cursor_y, 1);
        assert_eq!(s.cursor_x, 2);
    }

    #[test]
    fn test_narrow_over_wide_blanks_continuation() {
        let mut s = make_screen();
        s.process_output("中".as_bytes()); // cols 0-1: wide lead + continuation
                                           // Return to column 0 and overwrite the lead with a narrow char.
        s.process_output(b"\rX");
        assert_eq!(s.grid[0][0].c, 'X');
        assert_eq!(s.grid[0][0].width, 1);
        // The now-dangling continuation cell is blanked to a normal space.
        assert_eq!(s.grid[0][1].c, ' ');
        assert_eq!(s.grid[0][1].width, 1);
    }

    #[test]
    fn test_carriage_return() {
        let mut s = make_screen();
        s.process_output(b"Hello\rWorld");
        assert_eq!(s.grid[0][0].c, 'W');
        assert_eq!(s.grid[0][1].c, 'o');
        assert_eq!(s.grid[0][2].c, 'r');
        assert_eq!(s.grid[0][3].c, 'l');
        assert_eq!(s.grid[0][4].c, 'd');
    }

    #[test]
    fn test_linefeed() {
        let mut s = make_screen();
        s.process_output(b"A\r\nB");
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.grid[1][0].c, 'B');
        assert_eq!(s.cursor_y, 1);
    }

    #[test]
    fn test_backspace() {
        let mut s = make_screen();
        s.process_output(b"AB\x08C");
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.grid[0][1].c, 'C');
    }

    #[test]
    fn test_tab() {
        let mut s = make_screen();
        s.process_output(b"A\tB");
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.cursor_x, 9); // 'B' at position 8, cursor at 9
        assert_eq!(s.grid[0][8].c, 'B');
    }

    #[test]
    fn test_cursor_up() {
        let mut s = make_screen();
        s.cursor_y = 5;
        s.process_output(b"\x1b[3A"); // CUU 3
        assert_eq!(s.cursor_y, 2);
    }

    #[test]
    fn test_cursor_down() {
        let mut s = make_screen();
        s.process_output(b"\x1b[5B"); // CUD 5
        assert_eq!(s.cursor_y, 5);
    }

    #[test]
    fn test_cursor_forward() {
        let mut s = make_screen();
        s.process_output(b"\x1b[10C"); // CUF 10
        assert_eq!(s.cursor_x, 10);
    }

    #[test]
    fn test_cursor_backward() {
        let mut s = make_screen();
        s.cursor_x = 10;
        s.process_output(b"\x1b[3D"); // CUB 3
        assert_eq!(s.cursor_x, 7);
    }

    #[test]
    fn test_cursor_position() {
        let mut s = make_screen();
        s.process_output(b"\x1b[5;10H"); // CUP row=5, col=10 (1-based)
        assert_eq!(s.cursor_y, 4);
        assert_eq!(s.cursor_x, 9);
    }

    #[test]
    fn test_erase_display_to_end() {
        let mut s = make_screen();
        s.process_output(b"ABCDE");
        s.cursor_x = 2;
        s.process_output(b"\x1b[0J"); // ED 0
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.grid[0][1].c, 'B');
        assert_eq!(s.grid[0][2].c, ' ');
        assert_eq!(s.grid[0][3].c, ' ');
    }

    #[test]
    fn test_erase_display_full() {
        let mut s = make_screen();
        s.process_output(b"Hello");
        s.process_output(b"\x1b[2J"); // ED 2
        for c in &s.grid[0] {
            assert_eq!(c.c, ' ');
        }
    }

    #[test]
    fn test_erase_line_to_end() {
        let mut s = make_screen();
        s.process_output(b"ABCDE");
        s.cursor_x = 2;
        s.process_output(b"\x1b[0K"); // EL 0
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.grid[0][1].c, 'B');
        assert_eq!(s.grid[0][2].c, ' ');
    }

    #[test]
    fn test_erase_line_from_start() {
        let mut s = make_screen();
        s.process_output(b"ABCDE");
        s.cursor_x = 2;
        s.process_output(b"\x1b[1K"); // EL 1
        assert_eq!(s.grid[0][0].c, ' ');
        assert_eq!(s.grid[0][1].c, ' ');
        assert_eq!(s.grid[0][2].c, ' ');
        assert_eq!(s.grid[0][3].c, 'D');
    }

    #[test]
    fn test_scroll_region() {
        let mut s = Screen::new(10, 5, 100);
        // Set scroll region to rows 2-4 (1-based)
        s.process_output(b"\x1b[2;4r");
        assert_eq!(s.scroll_top, 1);
        assert_eq!(s.scroll_bottom, 3);
    }

    #[test]
    fn test_sgr_bold() {
        let mut s = make_screen();
        s.process_output(b"\x1b[1mA");
        assert!(s.grid[0][0].attrs.bold);
        assert_eq!(s.grid[0][0].c, 'A');
    }

    #[test]
    fn test_sgr_reset() {
        let mut s = make_screen();
        s.process_output(b"\x1b[1;3m");
        assert!(s.current_attrs.bold);
        assert!(s.current_attrs.italic);
        s.process_output(b"\x1b[0m");
        assert!(!s.current_attrs.bold);
        assert!(!s.current_attrs.italic);
    }

    #[test]
    fn test_sgr_fg_color() {
        let mut s = make_screen();
        s.process_output(b"\x1b[31mA"); // Red foreground
        assert_eq!(s.grid[0][0].attrs.fg, Color::Indexed(1));
    }

    #[test]
    fn test_sgr_bg_color() {
        let mut s = make_screen();
        s.process_output(b"\x1b[42mA"); // Green background
        assert_eq!(s.grid[0][0].attrs.bg, Color::Indexed(2));
    }

    #[test]
    fn test_sgr_256_color() {
        let mut s = make_screen();
        s.process_output(b"\x1b[38;5;200mA"); // 256-color fg
        assert_eq!(s.grid[0][0].attrs.fg, Color::Indexed(200));
    }

    #[test]
    fn test_sgr_truecolor() {
        let mut s = make_screen();
        s.process_output(b"\x1b[38;2;100;150;200mA"); // Truecolor fg
        assert_eq!(s.grid[0][0].attrs.fg, Color::Rgb(100, 150, 200));
    }

    #[test]
    fn test_sgr_truecolor_colon() {
        let mut s = make_screen();
        s.process_output(b"\x1b[38:2:10:20:30mA"); // colon-form truecolor fg
        assert_eq!(s.grid[0][0].attrs.fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn test_sgr_truecolor_colon_empty_colorspace() {
        let mut s = make_screen();
        s.process_output(b"\x1b[38:2::10:20:30mA"); // empty colorspace field
        assert_eq!(s.grid[0][0].attrs.fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn test_sgr_256_color_colon() {
        let mut s = make_screen();
        s.process_output(b"\x1b[38:5:200mA"); // colon-form 256-color fg
        assert_eq!(s.grid[0][0].attrs.fg, Color::Indexed(200));
    }

    #[test]
    fn test_sgr_underline_color_ignored() {
        let mut s = make_screen();
        // 58 is underline color; it must not set the underline attribute.
        s.process_output(b"\x1b[58:2:1:2:3mA");
        assert!(!s.current_attrs.underline);
        assert!(!s.grid[0][0].attrs.underline);
    }

    #[test]
    fn test_csi_gt_4m_not_treated_as_underline() {
        // `CSI > 4 m` (XTMODKEYS) must NOT enable underline; only real SGR `CSI 4 m` does.
        let mut s = Screen::new(20, 3, 100);
        s.process_output(b"\x1b[>4m");
        s.process_output(b"A");
        assert!(!s.current_attrs.underline, "CSI >4m must not set underline");
        assert!(
            !s.grid[0][0].attrs.underline,
            "char after CSI >4m must not be underlined"
        );
        // Plain SGR 4 still works.
        s.process_output(b"\x1b[4mB");
        assert!(
            s.current_attrs.underline,
            "plain CSI 4m must still set underline"
        );
    }

    #[test]
    fn test_sgr_bright_colors() {
        let mut s = make_screen();
        s.process_output(b"\x1b[91mA"); // Bright red fg
        assert_eq!(s.grid[0][0].attrs.fg, Color::Indexed(9));

        s.process_output(b"\x1b[102mB"); // Bright green bg
        assert_eq!(s.grid[1 - 1][1].attrs.bg, Color::Indexed(10));
    }

    #[test]
    fn test_scroll_up() {
        let mut s = Screen::new(5, 3, 100);
        s.process_output(b"AAAAA\r\nBBBBB\r\nCCCCC");
        // Now scroll up
        s.process_output(b"\x1b[1S");
        assert_eq!(s.grid[0][0].c, 'B');
        assert_eq!(s.grid[1][0].c, 'C');
        assert_eq!(s.grid[2][0].c, ' ');
        assert_eq!(s.scrollback.len(), 1);
    }

    #[test]
    fn test_scroll_down() {
        let mut s = Screen::new(5, 3, 100);
        s.process_output(b"AAAAA\r\nBBBBB\r\nCCCCC");
        s.process_output(b"\x1b[1T");
        assert_eq!(s.grid[0][0].c, ' ');
        assert_eq!(s.grid[1][0].c, 'A');
        assert_eq!(s.grid[2][0].c, 'B');
    }

    #[test]
    fn test_scrollback_limit() {
        let mut s = Screen::new(5, 2, 3);
        // Fill and scroll many times.
        for _ in 0..10 {
            s.process_output(b"XXXXX\n");
        }
        assert!(s.scrollback.len() <= 3);
    }

    #[test]
    fn test_ed3_clears_scrollback_only() {
        // Small screen so LF-scrolled lines spill into scrollback.
        let mut s = Screen::new(5, 2, 100);
        for i in 0..10 {
            s.process_output(format!("L{i:03}\r\n").as_bytes());
        }
        assert!(!s.scrollback.is_empty(), "scrollback should be populated");

        // Capture the visible grid so we can prove ED 3 does not touch it.
        let grid_before = s.grid.clone();

        // ED 3 (\e[3J): erase saved lines (scrollback) only.
        s.process_output(b"\x1b[3J");

        assert!(s.scrollback.is_empty(), "ED 3 must clear scrollback");
        assert_eq!(
            s.grid, grid_before,
            "ED 3 must leave the visible grid untouched"
        );
    }

    #[test]
    fn test_scrollback_content() {
        let mut s = Screen::new(5, 2, 100);
        s.process_output(b"AAAAA\r\nBBBBB\r\nCCCCC");
        let content = s.scrollback_content();
        assert!(content.contains("AAAAA"));
        assert!(content.contains("BBBBB"));
        assert!(content.contains("CCCCC"));
    }

    #[test]
    fn test_resize() {
        let mut s = make_screen();
        s.process_output(b"Hello");
        s.resize(40, 10);
        assert_eq!(s.cols, 40);
        assert_eq!(s.rows, 10);
        assert_eq!(s.grid.len(), 10);
        assert_eq!(s.grid[0].len(), 40);
        assert_eq!(s.grid[0][0].c, 'H');
        assert_eq!(s.grid[0][4].c, 'o');
        assert_eq!(s.scroll_bottom, 9);
    }

    #[test]
    fn test_line_wrap() {
        let mut s = Screen::new(5, 3, 100);
        s.process_output(b"ABCDEFGH");
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.grid[0][4].c, 'E');
        assert_eq!(s.grid[1][0].c, 'F');
        assert_eq!(s.grid[1][2].c, 'H');
    }

    #[test]
    fn test_insert_line() {
        let mut s = Screen::new(5, 3, 100);
        s.process_output(b"AAAAA\r\nBBBBB\r\nCCCCC");
        s.cursor_y = 1;
        s.process_output(b"\x1b[1L"); // Insert 1 line at row 1
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.grid[1][0].c, ' '); // Inserted blank line
        assert_eq!(s.grid[2][0].c, 'B'); // Original row 1 shifted down
    }

    #[test]
    fn test_delete_line() {
        let mut s = Screen::new(5, 3, 100);
        s.process_output(b"AAAAA\r\nBBBBB\r\nCCCCC");
        s.cursor_y = 1;
        s.process_output(b"\x1b[1M"); // Delete 1 line at row 1
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.grid[1][0].c, 'C');
        assert_eq!(s.grid[2][0].c, ' '); // Blank line at bottom
    }

    #[test]
    fn test_insert_char() {
        let mut s = Screen::new(5, 1, 100);
        s.process_output(b"ABCDE");
        s.cursor_x = 1;
        s.process_output(b"\x1b[1@"); // Insert 1 char at col 1
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.grid[0][1].c, ' '); // Inserted blank
        assert_eq!(s.grid[0][2].c, 'B');
        assert_eq!(s.grid[0][4].c, 'D'); // 'E' shifted off
    }

    #[test]
    fn test_delete_char() {
        let mut s = Screen::new(5, 1, 100);
        s.process_output(b"ABCDE");
        s.cursor_x = 1;
        s.process_output(b"\x1b[1P"); // Delete 1 char at col 1
        assert_eq!(s.grid[0][0].c, 'A');
        assert_eq!(s.grid[0][1].c, 'C');
        assert_eq!(s.grid[0][2].c, 'D');
        assert_eq!(s.grid[0][3].c, 'E');
        assert_eq!(s.grid[0][4].c, ' '); // Blank at end
    }

    #[test]
    fn test_vpa() {
        let mut s = make_screen();
        s.cursor_y = 5;
        s.process_output(b"\x1b[3d"); // VPA row 3 (1-based) -> row 2 (0-based)
        assert_eq!(s.cursor_y, 2);
    }

    #[test]
    fn test_hpa() {
        let mut s = make_screen();
        s.cursor_x = 10;
        s.process_output(b"\x1b[5G"); // HPA col 5 (1-based) -> col 4 (0-based)
        assert_eq!(s.cursor_x, 4);
    }

    #[test]
    fn test_sgr_extended_underline_off() {
        // Regression: `ESC[4:0m` (extended-underline OFF) was mishandled because
        // vte groups colon sub-parameters into a single param slice `[4, 0]`, and
        // the SGR handler ignored the sub-param, leaving underline stuck on. This
        // made every subsequent character render underlined under modern TUIs.
        let mut s = make_screen();
        // `4:3` (curly underline) enables underline in our bool model.
        s.process_output(b"\x1b[4:3mX");
        // `4:0` must turn underline back OFF.
        s.process_output(b"\x1b[4:0mY");

        assert!(
            s.grid[0][0].attrs.underline,
            "X should be underlined after ESC[4:3m"
        );
        assert!(
            !s.grid[0][1].attrs.underline,
            "Y should NOT be underlined after ESC[4:0m"
        );
    }

    #[test]
    fn test_sgr_plain_underline_on_off() {
        // Plain-underline path still works: `ESC[4m` on, `ESC[24m` off.
        let mut s = make_screen();
        s.process_output(b"\x1b[4mX");
        s.process_output(b"\x1b[24mY");

        assert!(
            s.grid[0][0].attrs.underline,
            "X should be underlined after ESC[4m"
        );
        assert!(
            !s.grid[0][1].attrs.underline,
            "Y should NOT be underlined after ESC[24m"
        );
    }

    #[test]
    fn test_decsc_decrc_restores_underline() {
        // DECSC (ESC 7) saves attrs, DECRC (ESC 8) must restore them.
        // ESC 7 -> ESC[4m (underline on) -> X -> ESC 8 -> (reposition) -> Y
        // X is underlined; Y must NOT be, because DECRC restores the
        // pre-underline rendition. DECRC also restores the cursor position,
        // so we move to column 11 before writing Y to avoid overwriting X.
        let mut s = make_screen();
        s.process_output(b"\x1b7\x1b[4mX\x1b8\x1b[1;11HY");

        assert!(
            s.grid[0][0].attrs.underline,
            "X should be underlined (written while underline was on)"
        );
        assert!(
            !s.grid[0][10].attrs.underline,
            "Y should NOT be underlined (DECRC restored pre-underline attrs)"
        );
        assert!(
            !s.current_attrs.underline,
            "current_attrs.underline must be false after DECRC"
        );
    }

    #[test]
    fn test_decsc_decrc_restores_bold() {
        // ESC 7 -> ESC[1m (bold on) -> X -> ESC 8 -> (reposition) -> Y
        // X is bold; Y must NOT be after DECRC.
        let mut s = make_screen();
        s.process_output(b"\x1b7\x1b[1mX\x1b8\x1b[1;11HY");

        assert!(
            s.grid[0][0].attrs.bold,
            "X should be bold (written while bold was on)"
        );
        assert!(
            !s.grid[0][10].attrs.bold,
            "Y should NOT be bold (DECRC restored pre-bold attrs)"
        );
        assert!(
            !s.current_attrs.bold,
            "current_attrs.bold must be false after DECRC"
        );
    }

    #[test]
    fn test_decsc_decrc_restores_cursor_position() {
        // ESC 7 saves the cursor position; ESC 8 restores it.
        // Move to (row 3, col 6), save, move elsewhere, restore -> back to saved.
        let mut s = make_screen();
        s.process_output(b"\x1b[3;6H"); // cursor to row 2, col 5 (0-based)
        s.process_output(b"\x1b7"); // DECSC: save position
        s.process_output(b"\x1b[1;1H"); // move to home
        assert_eq!((s.cursor_x, s.cursor_y), (0, 0), "cursor moved to home");
        s.process_output(b"\x1b8"); // DECRC: restore position
        assert_eq!(
            (s.cursor_x, s.cursor_y),
            (5, 2),
            "DECRC must restore the saved cursor position"
        );
    }

    #[test]
    fn test_osc8_hyperlink_sets_cell() {
        let mut s = Screen::new(40, 3, 100);
        s.process_output(b"\x1b]8;;https://example.com\x1b\\LINK\x1b]8;;\x1b\\X");
        // L,I,N,K carry the hyperlink; X does not.
        assert_eq!(
            s.grid[0][0].hyperlink.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            s.grid[0][3].hyperlink.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            s.grid[0][4].hyperlink, None,
            "X after link close must have no hyperlink"
        );
    }

    // -- Reflow -------------------------------------------------------------

    /// Row `r` of the grid as a plain string.
    fn row_text(s: &Screen, r: usize) -> String {
        s.grid[r].iter().map(|c| c.c).collect()
    }

    #[test]
    fn ech_erases_n_cells_from_the_cursor_without_moving_it() {
        let mut s = Screen::new(20, 3, 100);
        s.process_output(b"abcdefghij\x1b[1;3H\x1b[4X");
        assert_eq!(row_text(&s, 0).trim_end(), "ab    ghij");
        assert_eq!((s.cursor_x, s.cursor_y), (2, 0));
    }

    #[test]
    fn ech_defaults_to_one_cell_and_treats_zero_as_one() {
        let mut s = Screen::new(20, 3, 100);
        s.process_output(b"abcdef\x1b[1;2H\x1b[X\x1b[1;4H\x1b[0X");
        assert_eq!(row_text(&s, 0).trim_end(), "a c ef");
    }

    #[test]
    fn ech_blanks_both_halves_of_wide_glyphs_it_cuts_at_either_edge() {
        let mut s = Screen::new(10, 2, 100);
        // 中 at 0-1, 文 at 2-3, 字 at 4-5. Erase cols 1..5: it starts on 中's
        // continuation and ends on 字's lead.
        s.process_output("中文字ab".as_bytes());
        s.process_output(b"\x1b[1;2H\x1b[4X");
        for x in 0..6 {
            let cell = &s.grid[0][x];
            assert_eq!(
                (cell.c, cell.width),
                (' ', 1),
                "col {x} must be a blank narrow cell"
            );
        }
        assert_eq!(row_text(&s, 0).trim_end(), "      ab");
        assert_eq!((s.cursor_x, s.cursor_y), (1, 0));
    }

    #[test]
    fn el_blanks_the_wide_glyph_it_cuts_at_the_cursor() {
        let mut s = Screen::new(10, 2, 100);
        s.process_output("ab中cd".as_bytes());
        // EL 0 from 中's continuation: the lead at col 2 must go too.
        s.process_output(b"\x1b[1;4H\x1b[K");
        assert_eq!((s.grid[0][2].c, s.grid[0][2].width), (' ', 1));
        assert_eq!(row_text(&s, 0).trim_end(), "ab");

        let mut s = Screen::new(10, 2, 100);
        s.process_output("ab中cd".as_bytes());
        // EL 1 up to 中's lead: the continuation at col 3 must go too.
        s.process_output(b"\x1b[1;3H\x1b[1K");
        assert_eq!((s.grid[0][3].c, s.grid[0][3].width), (' ', 1));
        assert_eq!(row_text(&s, 0), "    cd    ");
    }

    #[test]
    fn ech_cancels_a_pending_wrap() {
        let mut s = Screen::new(10, 3, 100);
        s.process_output(b"0123456789");
        assert!(s.pending_wrap, "a full row leaves the wrap pending");
        s.process_output(b"\x1b[XZ");
        assert_eq!(row_text(&s, 0), "012345678Z");
        assert_eq!(row_text(&s, 1).trim_end(), "");
    }

    #[test]
    fn ech_to_the_row_end_clears_the_wrapped_flag() {
        let mut s = Screen::new(10, 3, 100);
        s.process_output(b"0123456789abcde");
        assert!(
            s.grid[0].wrapped,
            "the overflowing row continues onto the next"
        );
        s.process_output(b"\x1b[1;6H\x1b[2X");
        assert!(
            s.grid[0].wrapped,
            "an erase inside the row keeps it continuing"
        );
        s.process_output(b"\x1b[1;6H\x1b[10X");
        assert!(
            !s.grid[0].wrapped,
            "an erase that reaches the row end ends the line"
        );
    }

    /// Every wide lead on row `r` is followed by its continuation, and every
    /// continuation follows a lead: the client never paints a width-0 cell, so
    /// a lone half leaves that column showing whatever was drawn there before.
    fn assert_no_split_glyph(s: &Screen, r: usize, op: &str) {
        let line = &s.grid[r];
        for x in 0..line.len() {
            match line[x].width {
                2 => assert!(
                    line.get(x + 1).is_some_and(|c| c.width == 0),
                    "{op}: the lead at col {x} has no continuation"
                ),
                0 => assert!(
                    x > 0 && line[x - 1].width == 2,
                    "{op}: the continuation at col {x} has no lead"
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn ich_at_a_continuation_blanks_the_glyph_it_splits() {
        let mut s = Screen::new(10, 2, 100);
        s.process_output("a中b".as_bytes());
        s.process_output(b"\x1b[1;3H\x1b[@");
        assert_no_split_glyph(&s, 0, "ICH at a continuation");
        assert_eq!(row_text(&s, 0).trim_end(), "a   b");
    }

    #[test]
    fn dch_at_a_continuation_blanks_the_glyph_it_splits() {
        let mut s = Screen::new(10, 2, 100);
        s.process_output("a中b".as_bytes());
        s.process_output(b"\x1b[1;3H\x1b[P");
        assert_no_split_glyph(&s, 0, "DCH at a continuation");
        assert_eq!(row_text(&s, 0).trim_end(), "a b");
    }

    #[test]
    fn ich_that_pushes_a_lead_into_the_last_column_blanks_it() {
        let mut s = Screen::new(6, 2, 100);
        // 中 sits at cols 3-4. Two inserted cells would leave its lead in the
        // last column and push its continuation off the row.
        s.process_output("abc中d".as_bytes());
        s.process_output(b"\x1b[1;1H\x1b[2@");
        assert_no_split_glyph(&s, 0, "ICH pushing a lead to the margin");
        assert_eq!(row_text(&s, 0), "  abc ");
    }

    #[test]
    fn dch_that_pulls_a_continuation_to_the_cursor_blanks_it() {
        let mut s = Screen::new(10, 2, 100);
        // Deleting 中's lead alone would pull its continuation to col 1.
        s.process_output("a中b".as_bytes());
        s.process_output(b"\x1b[1;2H\x1b[P");
        assert_no_split_glyph(&s, 0, "DCH pulling a continuation");
        assert_eq!(row_text(&s, 0).trim_end(), "a b");

        // The same with a span that ends inside the glyph.
        let mut s = Screen::new(10, 2, 100);
        s.process_output("ab中c".as_bytes());
        s.process_output(b"\x1b[1;2H\x1b[2P");
        assert_no_split_glyph(&s, 0, "DCH ending inside a glyph");
        assert_eq!(row_text(&s, 0).trim_end(), "a c");
    }

    #[test]
    fn ich_and_dch_clamp_a_count_past_the_row_end() {
        // A count far past the row must be clamped to the columns left, or
        // `len - n` underflows (ICH) and the drain range runs off the row (DCH):
        // pane output is untrusted, so either would take the server down.
        for (op, seq) in [
            ("ICH", b"\x1b[1;8H\x1b[500@"),
            ("DCH", b"\x1b[1;8H\x1b[500P"),
        ] {
            let mut s = Screen::new(10, 2, 100);
            s.process_output(b"0123456789");
            s.process_output(seq);
            assert_eq!(row_text(&s, 0), "0123456   ", "{op}");
            assert_eq!(s.grid[0].len(), 10, "{op} keeps the row length");
        }
        // With a wide glyph at cols 7-8 both breaks run with the clamped count.
        for (op, seq) in [
            ("ICH", b"\x1b[1;9H\x1b[500@"),
            ("DCH", b"\x1b[1;9H\x1b[500P"),
        ] {
            let mut s = Screen::new(10, 2, 100);
            s.process_output("0123456中9".as_bytes());
            s.process_output(seq);
            assert_no_split_glyph(&s, 0, op);
            assert_eq!(row_text(&s, 0), "0123456   ", "{op}");
            assert_eq!(s.grid[0].len(), 10, "{op} keeps the row length");
        }
    }

    #[test]
    fn ich_with_a_zero_count_inserts_one_cell() {
        let mut s = Screen::new(10, 2, 100);
        s.process_output(b"abc\x1b[1;2H\x1b[0@");
        assert_eq!(row_text(&s, 0).trim_end(), "a bc");
    }

    #[test]
    fn dch_with_a_zero_count_deletes_one_cell() {
        let mut s = Screen::new(10, 2, 100);
        s.process_output(b"abc\x1b[1;2H\x1b[0P");
        assert_eq!(row_text(&s, 0).trim_end(), "ac");
    }

    #[test]
    fn ich_and_dch_cancel_a_pending_wrap() {
        for (op, seq) in [("ICH", b"\x1b[@X"), ("DCH", b"\x1b[PX")] {
            let mut s = Screen::new(10, 3, 100);
            s.process_output(b"0123456789");
            assert!(s.pending_wrap, "{op}: a full row leaves the wrap pending");
            s.process_output(seq);
            assert_eq!(s.grid[0][9].c, 'X', "{op}: X lands in the last column");
            assert_eq!(row_text(&s, 1).trim_end(), "", "{op}: nothing wrapped");
        }
    }

    #[test]
    fn ich_and_dch_move_whole_wide_glyphs_unchanged() {
        let mut s = Screen::new(10, 2, 100);
        s.process_output("a中b".as_bytes());
        s.process_output(b"\x1b[1;1H\x1b[2@");
        assert_no_split_glyph(&s, 0, "ICH before a glyph");
        assert_eq!((s.grid[0][3].c, s.grid[0][3].width), ('中', 2));
        s.process_output(b"\x1b[1;1H\x1b[2P");
        assert_no_split_glyph(&s, 0, "DCH before a glyph");
        assert_eq!((s.grid[0][1].c, s.grid[0][1].width), ('中', 2));
        // A continuation cell holds a space, so row_text shows one after 中.
        assert_eq!(row_text(&s, 0).trim_end(), "a中 b");
    }

    #[test]
    fn ech_stops_at_the_end_of_the_row() {
        let mut s = Screen::new(10, 2, 100);
        s.process_output(b"0123456789\r\nnext\x1b[1;8H\x1b[500X");
        assert_eq!(row_text(&s, 0), "0123456   ");
        assert_eq!(row_text(&s, 1).trim_end(), "next");
    }

    /// The shape of Neovim's redraw of a window narrower than the screen, as
    /// `nvim --clean +vsplit` emits it: the line number, the text, then ECH over
    /// the rest of the window. An empty file line is the number and ECH alone,
    /// so an ignored ECH shows the text of the line that was there before the
    /// scroll as if it were this line.
    #[test]
    fn ech_clears_stale_text_under_a_neovim_window_redraw() {
        let mut s = Screen::new(40, 2, 100);
        s.process_output(b"  2 line 2 xxxxxxxxxx    \xe2\x94\x82 right");
        s.process_output(b"\x1b[1;1H  3 \x1b[20X");
        assert_eq!(
            row_text(&s, 0).trim_end(),
            format!("  3{}\u{2502} right", " ".repeat(22))
        );
    }

    /// The shape Neovim on macOS emits when a full-width window scrolls by
    /// redrawing its rows: each row is a jump past the gutter, the new text,
    /// and an ECH that ends where the row's old text ended rather than at the
    /// right edge, because Neovim knows the cells past it are already blank. A dropped ECH
    /// leaves the old line's tail after the new, shorter line, so the row reads
    /// as a copy of the line that was there before the scroll.
    #[test]
    fn ech_clears_the_old_tail_when_a_full_width_window_redraws_its_rows() {
        let mut s = Screen::new(40, 5, 100);
        s.process_output(b"\x1b[1;1Hbar");
        s.process_output(b"\x1b[2;1H 21 \xe2\x94\x82 the twenty-first line, long");
        s.process_output(b"\x1b[3;1H 22 \xe2\x94\x82 short one");
        s.process_output(b"\x1b[5;1Hstatus");
        s.process_output(b"\x1b[2;2H22 \xe2\x94\x82 short one\x1b[18X");
        s.process_output(b"\x1b[3;2H23 \xe2\x94\x82 the twenty-third line, long");
        assert_eq!(row_text(&s, 0).trim_end(), "bar");
        assert_eq!(row_text(&s, 1).trim_end(), " 22 \u{2502} short one");
        assert_eq!(
            row_text(&s, 2).trim_end(),
            " 23 \u{2502} the twenty-third line, long"
        );
        assert_eq!(row_text(&s, 4).trim_end(), "status");
    }

    #[test]
    fn test_reflow_widen_rejoins_wrapped_rows() {
        // 100 chars at 40 columns occupy three rows (40/40/20), the first two
        // flagged as continuing.
        let mut s = Screen::new(40, 6, 100);
        s.process_output("X".repeat(100).as_bytes());
        assert!(s.grid[0].wrapped, "row 0 overflowed, so it must be flagged");
        assert!(s.grid[1].wrapped);
        assert!(!s.grid[2].wrapped, "the last row does not continue");

        s.resize(80, 6);

        // Widened: the same logical line now needs only two rows, the first
        // holding a full 80 characters.
        assert_eq!(row_text(&s, 0), "X".repeat(80));
        assert_eq!(row_text(&s, 1).trim_end(), "X".repeat(20));
        assert_eq!(row_text(&s, 2).trim_end(), "");
        assert!(s.grid[0].wrapped);
        assert!(!s.grid[1].wrapped);
        // Rows are always rebuilt at full width.
        assert_eq!(s.grid[0].len(), 80);
        assert_eq!(s.grid[5].len(), 80);
    }

    #[test]
    fn test_reflow_round_trip_is_lossless() {
        let mut s = Screen::new(80, 10, 1000);
        s.process_output(b"first line\r\n");
        s.process_output("Y".repeat(150).as_bytes());
        s.process_output(b"\r\nlast\r\n");

        let before = s.scrollback_content();
        let cursor_before = (s.cursor_x, s.cursor_y);

        s.resize(40, 10);
        s.resize(80, 10);

        assert_eq!(
            s.scrollback_content(),
            before,
            "80 -> 40 -> 80 must restore the original wrapping"
        );
        assert_eq!(
            (s.cursor_x, s.cursor_y),
            cursor_before,
            "the cursor must come back to where it started"
        );
        assert!(s.scrollback.is_empty(), "no blank rows leaked into history");
    }

    #[test]
    fn test_reflow_does_not_join_explicit_newlines() {
        // Two short lines separated by CR/LF are two logical lines; widening
        // must never splice them together.
        let mut s = Screen::new(40, 5, 100);
        s.process_output(b"aaa\r\nbbb\r\n");
        assert!(!s.grid[0].wrapped, "an explicit LF must not set `wrapped`");

        s.resize(80, 5);

        assert_eq!(row_text(&s, 0).trim_end(), "aaa");
        assert_eq!(row_text(&s, 1).trim_end(), "bbb");
    }

    #[test]
    fn test_reflow_never_splits_wide_char() {
        // 9 narrow chars, then a width-2 glyph. At the new width of 10 the glyph
        // would straddle the boundary, so it must move to the next row.
        let mut s = Screen::new(40, 4, 100);
        s.process_output("a".repeat(9).as_bytes());
        s.process_output("中".as_bytes());
        s.process_output("b".repeat(20).as_bytes());

        s.resize(10, 4);

        assert_eq!(row_text(&s, 0), "aaaaaaaaa ", "col 9 left blank");
        assert_eq!(s.grid[0][9].width, 1);
        assert!(s.grid[0].wrapped);
        assert_eq!(s.grid[1][0].c, '中');
        assert_eq!(s.grid[1][0].width, 2);
        assert_eq!(
            s.grid[1][1].width, 0,
            "continuation spacer follows the lead"
        );
        assert_eq!(s.grid[1][2].c, 'b');
    }

    #[test]
    fn test_alt_screen_is_not_reflowed() {
        // Full-screen apps repaint on SIGWINCH; rewrapping their buffer would
        // race that redraw. The alt screen keeps the naive clamp/pad behavior.
        let mut s = Screen::new(10, 4, 100);
        s.process_output(b"\x1b[?1049h");
        s.process_output(b"ABCDEFGHIJKLM");
        assert_eq!(s.grid[1][2].c, 'M');

        s.resize(20, 4);

        assert_eq!(s.grid[0].len(), 20, "rows are padded to the new width");
        assert_eq!(row_text(&s, 0).trim_end(), "ABCDEFGHIJ");
        assert_eq!(
            row_text(&s, 1).trim_end(),
            "KLM",
            "the wrapped tail must stay on its own row"
        );
    }

    #[test]
    fn test_reflow_preserves_cursor_logical_position() {
        // 45 chars at 20 columns: rows of 20/20/5, cursor just past char 45.
        let mut s = Screen::new(20, 4, 100);
        s.process_output("z".repeat(45).as_bytes());
        assert_eq!((s.cursor_y, s.cursor_x), (2, 5));

        s.resize(30, 4);

        // 45 chars at 30 columns: rows of 30/15, cursor still just past char 45.
        assert_eq!((s.cursor_y, s.cursor_x), (1, 15));
        assert_eq!(s.grid[1][14].c, 'z');
        assert_eq!(s.grid[1][15].c, ' ');
    }

    #[test]
    fn test_reflow_carries_pending_wrap_across_resize() {
        // Exactly one row's worth of text leaves the cursor parked on the last
        // column with a deferred wrap outstanding. That state is folded into the
        // logical cursor offset before reflow and re-derived after, so it must
        // survive a resize in both directions.
        let mut s = Screen::new(40, 4, 100);
        s.process_output("q".repeat(40).as_bytes());
        assert_eq!((s.cursor_y, s.cursor_x), (0, 39));
        assert!(s.pending_wrap, "the row is full: wrap is deferred");

        // Widening leaves room, so the cursor is no longer at an edge.
        s.resize(80, 4);
        assert_eq!((s.cursor_y, s.cursor_x), (0, 40));
        assert!(!s.pending_wrap, "there is room again; nothing is deferred");

        // Narrowing to 20 splits the line in two and re-parks the cursor at the
        // end of the second row, deferred wrap restored.
        s.resize(20, 4);
        assert_eq!((s.cursor_y, s.cursor_x), (1, 19));
        assert!(s.pending_wrap, "the cursor is parked at the edge again");

        // And the deferred wrap still behaves: the next char starts row 2.
        s.process_output(b"Z");
        assert_eq!((s.cursor_y, s.cursor_x), (2, 1));
        assert_eq!(s.grid[2][0].c, 'Z');
        assert!(s.grid[1].wrapped);
    }

    #[test]
    fn test_reflow_joins_line_across_scrollback_boundary() {
        // A 45-char line on a 3-row screen: most of it has scrolled off into
        // the scrollback, so the logical line straddles the boundary.
        let mut s = Screen::new(10, 3, 100);
        s.process_output(b"line0\r\n");
        s.process_output("w".repeat(45).as_bytes());
        assert!(!s.scrollback.is_empty(), "content must have scrolled off");

        s.resize(50, 3);

        // Widened past its length, the whole line fits on a single row and the
        // scrollback drains completely.
        assert!(s.scrollback.is_empty());
        assert_eq!(row_text(&s, 0).trim_end(), "line0");
        assert_eq!(row_text(&s, 1).trim_end(), "w".repeat(45));
        assert!(!s.grid[1].wrapped);
    }

    /// A 10x4 screen with every row full of text, then SGR blue background
    /// with underline and reverse, inside an open OSC 8 link, so a blank that
    /// took anything but the background shows it.
    fn bce_screen() -> Screen {
        let mut s = Screen::new(10, 4, 100);
        for r in 1..=4 {
            s.process_output(format!("\x1b[{r};1H0123456789").as_bytes());
        }
        s.process_output(b"\x1b[44;4;7m\x1b]8;;http://x\x1b\\");
        s
    }

    /// The first cell of `cols` on `row` that is not a blank carrying only the
    /// blue background, described for the failure message.
    fn bce_mismatch(s: &Screen, row: usize, cols: std::ops::Range<usize>) -> Option<String> {
        let want = CellAttrs {
            bg: Color::Indexed(4),
            ..CellAttrs::default()
        };
        cols.map(|x| (x, &s.grid[row][x]))
            .find(|(_, cell)| cell.c != ' ' || cell.attrs != want || cell.hyperlink.is_some())
            .map(|(x, cell)| {
                format!(
                    "({row},{x}) = {:?} {:?} {:?}",
                    cell.c, cell.attrs, cell.hyperlink
                )
            })
    }

    /// A row and the columns on it that an operation must have blanked.
    type BceSpan = (usize, std::ops::Range<usize>);

    #[test]
    fn erases_and_scrolls_fill_with_the_current_background() {
        let cases: &[(&str, &[u8], &[BceSpan])] = &[
            ("EL 0", b"\x1b[1;4H\x1b[K", &[(0, 3..10)]),
            ("EL 1", b"\x1b[1;4H\x1b[1K", &[(0, 0..4)]),
            ("EL 2", b"\x1b[1;4H\x1b[2K", &[(0, 0..10)]),
            (
                "ED 0",
                b"\x1b[2;4H\x1b[J",
                &[(1, 3..10), (2, 0..10), (3, 0..10)],
            ),
            ("ED 1", b"\x1b[2;4H\x1b[1J", &[(0, 0..10), (1, 0..4)]),
            (
                "ED 2",
                b"\x1b[2J",
                &[(0, 0..10), (1, 0..10), (2, 0..10), (3, 0..10)],
            ),
            ("ECH", b"\x1b[1;3H\x1b[3X", &[(0, 2..5)]),
            ("ICH", b"\x1b[1;3H\x1b[2@", &[(0, 2..4)]),
            ("DCH", b"\x1b[1;3H\x1b[2P", &[(0, 8..10)]),
            ("IL", b"\x1b[2;1H\x1b[L", &[(1, 0..10)]),
            ("DL", b"\x1b[2;1H\x1b[M", &[(3, 0..10)]),
            ("SU", b"\x1b[S", &[(3, 0..10)]),
            ("SD", b"\x1b[T", &[(0, 0..10)]),
            ("LF at the bottom", b"\x1b[4;1H\n", &[(3, 0..10)]),
            ("IND at the bottom", b"\x1b[4;1H\x1bD", &[(3, 0..10)]),
            ("RI at the top", b"\x1b[1;1H\x1bM", &[(0, 0..10)]),
            ("LF in a region", b"\x1b[2;3r\x1b[3;1H\n", &[(2, 0..10)]),
            ("autowrap scroll", b"\x1b[4;10HXY", &[(3, 1..10)]),
        ];
        let wrong: Vec<String> = cases
            .iter()
            .filter_map(|(op, bytes, spans)| {
                let mut s = bce_screen();
                s.process_output(bytes);
                spans
                    .iter()
                    .find_map(|(row, cols)| bce_mismatch(&s, *row, cols.clone()))
                    .map(|m| format!("{op}: {m}"))
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "blanks without the current background:\n{}",
            wrong.join("\n")
        );
    }

    #[test]
    fn an_erase_with_the_default_background_leaves_a_default_blank() {
        let mut s = Screen::new(10, 2, 100);
        // Bold, underline and reverse are set but the background is default,
        // so the erased cells must carry none of them.
        s.process_output(b"0123456789\x1b[1;4;7m\x1b[1;4H\x1b[K");
        for x in 3..10 {
            let cell = &s.grid[0][x];
            assert_eq!(cell.attrs, CellAttrs::default(), "col {x}");
            assert_eq!(cell.c, ' ', "col {x}");
            assert!(cell.erased, "col {x} comes from an erase");
        }
    }

    #[test]
    fn reflow_keeps_a_coloured_erase_through_narrowing_and_widening() {
        let mut s = Screen::new(20, 3, 100);
        s.process_output(b"ab\x1b[44m\x1b[K\x1b[0m\r\ncd");

        for (cols, when) in [
            (15, "after narrowing to 15"),
            (20, "after widening back to 20"),
        ] {
            s.resize(cols, 3);
            assert_eq!(s.grid[0][5].attrs.bg, Color::Indexed(4), "col 5 {when}");
            assert_eq!(
                s.grid[0][cols as usize - 1].attrs.bg,
                Color::Indexed(4),
                "the last column {when}"
            );
            assert!(!s.grid[0].wrapped, "row 0 does not wrap {when}");
            assert_eq!(row_text(&s, 0).trim_end(), "ab", "{when}");
            assert_eq!(row_text(&s, 1).trim_end(), "cd", "{when}");
        }
    }

    #[test]
    fn reflow_keeps_the_colour_of_erased_rows_below_the_cursor() {
        let mut s = Screen::new(20, 4, 100);
        // ED 0 in blue below the cursor's row, then the cursor goes home.
        s.process_output(b"ab\r\n\x1b[44m\x1b[J\x1b[0m\x1b[H");

        s.resize(15, 4);

        for r in 1..4 {
            assert_eq!(s.grid[r][0].attrs.bg, Color::Indexed(4), "row {r} col 0");
            assert_eq!(
                s.grid[r][14].attrs.bg,
                Color::Indexed(4),
                "row {r} last col"
            );
        }
        assert_eq!(row_text(&s, 0).trim_end(), "ab");
    }

    #[test]
    fn reflow_never_splits_a_trailing_wide_glyph() {
        let mut s = Screen::new(10, 2, 100);
        // The cursor leaves the row, so reflow trims the row's padding.
        s.process_output("ab中\r\n".as_bytes());
        s.resize(8, 2);
        s.resize(10, 2);
        let line = &s.grid[0];
        for x in 0..line.len() {
            match line[x].width {
                2 => assert!(
                    line.get(x + 1).is_some_and(|c| c.width == 0),
                    "the lead at col {x} has no continuation"
                ),
                0 => assert!(
                    x > 0 && line[x - 1].width == 2,
                    "the continuation at col {x} has no lead"
                ),
                _ => {}
            }
        }
        assert_eq!((line[2].c, line[2].width), ('中', 2));
    }

    #[test]
    fn reflow_puts_a_partly_coloured_tail_back_where_it_was() {
        let mut s = Screen::new(20, 3, 100);
        // Default padding from col 2, then a blue erase from col 12. The cursor
        // then leaves the row, so all of cols 2-19 is the line's tail.
        s.process_output(b"zz\x1b[1;13H\x1b[44m\x1b[K\x1b[0m\r\n");
        s.resize(15, 3);
        assert_eq!(s.grid[0][5].attrs.bg, Color::Default, "col 5 stays default");
        assert_eq!(
            s.grid[0][12].attrs.bg,
            Color::Indexed(4),
            "col 12 stays blue"
        );
        assert_eq!(
            s.grid[0][14].attrs.bg,
            Color::Indexed(4),
            "the new edge is blue"
        );
    }

    #[test]
    fn reflow_rebuilds_partly_coloured_rows_below_the_cursor_cell_by_cell() {
        // Row 1 is blue from col 12; row 2 is blue up to col 9 and default after.
        let mut s = Screen::new(20, 4, 100);
        s.process_output(b"ab\x1b[2;13H\x1b[44m\x1b[K\x1b[3;10H\x1b[1K\x1b[0m\x1b[H");

        s.resize(15, 4);

        assert_eq!(s.grid[1][5].attrs.bg, Color::Default, "row 1 col 5");
        assert_eq!(s.grid[1][12].attrs.bg, Color::Indexed(4), "row 1 col 12");
        assert_eq!(s.grid[1][14].attrs.bg, Color::Indexed(4), "row 1 new edge");
        assert_eq!(s.grid[2][0].attrs.bg, Color::Indexed(4), "row 2 col 0");
        assert_eq!(s.grid[2][9].attrs.bg, Color::Indexed(4), "row 2 col 9");
        assert_eq!(s.grid[2][10].attrs.bg, Color::Default, "row 2 col 10");
        assert_eq!(s.grid[2][14].attrs.bg, Color::Default, "row 2 new edge");
    }

    #[test]
    fn reflow_trims_a_coloured_erase_like_padding() {
        let mut s = Screen::new(20, 3, 100);
        s.process_output(b"ab\x1b[44m\x1b[K\x1b[0m\r\ncd");
        assert_eq!(s.grid[0][19].attrs.bg, Color::Indexed(4));

        s.resize(10, 3);

        assert!(
            !s.grid[0].wrapped,
            "an erased tail is not text, so the row must not wrap"
        );
        assert_eq!(row_text(&s, 0).trim_end(), "ab");
        assert_eq!(row_text(&s, 1).trim_end(), "cd");
    }

    #[test]
    fn test_reflow_keeps_colored_trailing_spaces() {
        // Spaces with a non-default background are content, not padding.
        let mut s = Screen::new(20, 3, 100);
        s.process_output(b"ab\x1b[41m    \x1b[0m");

        s.resize(40, 3);

        for c in 2..6 {
            assert_eq!(s.grid[0][c].c, ' ');
            assert_eq!(
                s.grid[0][c].attrs.bg,
                Color::Indexed(1),
                "colored trailing space at col {c} must survive reflow"
            );
        }
        assert_eq!(
            s.grid[0][6].attrs.bg,
            Color::Default,
            "real padding past the content is still trimmed"
        );
    }

    #[test]
    fn test_resize_on_alt_screen_keeps_saved_primary_consistent() {
        // Regression: resizing while an app was on the alternate screen left the
        // saved *primary* screen at the old size. Restoring it handed back a
        // grid whose row count disagreed with `rows`/`scroll_bottom`, and the
        // next DL indexed past the end of the grid and panicked.
        let mut s = Screen::new(14, 9, 100);
        s.process_output(b"\x1b[?1049h");
        s.resize(25, 3);
        s.process_output(b"\x1b[?1049l");

        assert_eq!(s.grid.len(), 3, "restored primary screen must match `rows`");
        assert_eq!(s.grid[0].len(), 25);
        assert!((s.scroll_bottom as usize) < s.grid.len());
        assert!(s.cursor_y < s.rows && s.cursor_x < s.cols);

        // Used to panic here.
        s.process_output(b"\x1b[2J");
        s.process_output(b"\x1b[1M");
    }

    /// Scrollback rows as plain text, for byte-identical round-trip checks.
    fn scrollback_texts(s: &Screen) -> Vec<String> {
        s.scrollback
            .iter()
            .map(|row| row.cells.iter().map(|c| c.c).collect())
            .collect()
    }

    #[test]
    fn test_alt_screen_output_does_not_grow_scrollback() {
        // A full-screen application's own redraws are ephemeral: they must not
        // become history the user can scroll into.
        let mut s = Screen::new(20, 4, 100);
        for i in 0..10 {
            s.process_output(format!("primary {i}\r\n").as_bytes());
        }
        let before = s.scrollback.len();
        assert!(
            before > 0,
            "need primary history for the test to mean anything"
        );

        s.process_output(b"\x1b[?1049h");
        for i in 0..50 {
            s.process_output(format!("alt {i}\r\n").as_bytes());
        }

        assert!(
            s.scrollback.is_empty(),
            "the alternate screen has no scrollback of its own"
        );
        assert_eq!(
            s.max_scroll_offset(),
            0,
            "nothing to scroll while a full-screen app is up"
        );
        assert_eq!(
            s.total_lines(),
            s.grid.len(),
            "only the alt grid is addressable"
        );

        s.process_output(b"\x1b[?1049l");
        assert_eq!(
            s.scrollback.len(),
            before,
            "leaving the alt screen must not have added history"
        );
    }

    #[test]
    fn test_alt_screen_round_trip_preserves_primary_scrollback() {
        // The primary screen's history is the user's; an app switching to the
        // alternate screen and back must hand it back byte-identical.
        let mut s = Screen::new(20, 4, 100);
        for i in 0..30 {
            s.process_output(format!("history {i}\r\n").as_bytes());
        }
        let texts = scrollback_texts(&s);
        let evicted = s.lines_evicted;
        let max_off = s.max_scroll_offset();
        assert!(max_off > 0);

        // No resize here: a width change while on the alt screen legitimately
        // changes what is addressable afterwards.
        s.process_output(b"\x1b[?1049h");
        for i in 0..40 {
            s.process_output(format!("alt {i}\r\n").as_bytes());
        }
        assert_eq!(s.max_scroll_offset(), 0);
        s.process_output(b"\x1b[?1049l");

        assert_eq!(
            scrollback_texts(&s),
            texts,
            "primary history must survive intact"
        );
        assert_eq!(s.lines_evicted, evicted, "nothing may have been evicted");
        assert_eq!(
            s.max_scroll_offset(),
            max_off,
            "the pane scrolls exactly as far as it did before"
        );
    }

    #[test]
    fn test_alt_screen_1047_round_trip_preserves_primary_scrollback() {
        // Mode 1047 takes the same path as 1049 without the cursor save.
        let mut s = Screen::new(20, 4, 100);
        for i in 0..20 {
            s.process_output(format!("history {i}\r\n").as_bytes());
        }
        let texts = scrollback_texts(&s);

        s.process_output(b"\x1b[?1047h");
        for i in 0..30 {
            s.process_output(format!("alt {i}\r\n").as_bytes());
        }
        assert!(s.scrollback.is_empty());
        s.process_output(b"\x1b[?1047l");

        assert_eq!(scrollback_texts(&s), texts);
    }

    #[test]
    fn test_scrolled_back_pane_entering_alt_screen_stays_addressable() {
        // Entering the alt screen while a client sits scrolled back leaves that
        // client's stored offset pointing past the end of what is addressable.
        // Every render path clamps against `max_scroll_offset()`, so the clamp
        // must land on the live view and `line_at` must stay in range.
        let mut s = Screen::new(20, 4, 100);
        for i in 0..30 {
            s.process_output(format!("history {i}\r\n").as_bytes());
        }
        let stale_offset = s.max_scroll_offset();
        s.process_output(b"\x1b[?1049h");

        let clamped = stale_offset.min(s.max_scroll_offset());
        assert_eq!(clamped, 0, "a stale offset clamps to the live alt screen");
        for row in 0..s.rows {
            let abs = s.abs_of_row(clamped, row);
            assert!(
                s.line_at(s.array_index_of_abs(abs)).is_some(),
                "row {row} must resolve to a real line"
            );
        }
    }

    /// Every addressable line, oldest first, as trimmed text.
    fn all_lines(s: &Screen) -> Vec<String> {
        (0..s.total_lines())
            .filter_map(|i| s.line_at(i))
            .map(|row| {
                row.cells
                    .iter()
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn screen_text(s: &Screen) -> Vec<String> {
        (0..s.grid.len())
            .map(|r| row_text(s, r).trim_end().to_string())
            .collect()
    }

    /// Six lines of history on a 4-row screen, then `clear` (ED 2) and a prompt
    /// at the top: the state a shell leaves after the `clear` command.
    fn cleared_with_history() -> Screen {
        cleared_with_history_by(b"\x1b[2J\x1b[H")
    }

    /// `cleared_with_history` with the clear given as bytes, ED 2 or ED 0 from
    /// home.
    fn cleared_with_history_by(clear: &[u8]) -> Screen {
        let mut s = Screen::new(20, 4, 100);
        for i in 0..6 {
            s.process_output(format!("hist{i}\r\n").as_bytes());
        }
        s.process_output(clear);
        s.process_output(b"$ ");
        assert!(!s.scrollback.is_empty(), "the history is in the scrollback");
        s
    }

    /// Ten lines of history `H0`..`H9` on a 20x6 screen, then ED 2 and home.
    fn cleared_after_ten_lines() -> Screen {
        let mut s = Screen::new(20, 6, 100);
        for i in 0..10 {
            s.process_output(format!("H{i}\r\n").as_bytes());
        }
        s.process_output(b"\x1b[2J\x1b[H");
        s
    }

    fn shows_cleared_history(s: &Screen) -> bool {
        screen_text(s).iter().any(|row| row.starts_with('H'))
    }

    #[test]
    fn a_line_wrapped_across_the_clear_does_not_join_the_prompt() {
        let mut s = Screen::new(20, 4, 100);
        // The X line's first row is the last scrollback row and still wraps
        // onto the screen when the screen is cleared.
        s.process_output(format!("a\r\nb\r\nc\r\n{}\r\nd\r\ne\r\nf", "X".repeat(30)).as_bytes());
        s.process_output(b"\x1b[2J\x1b[H$ ");
        s.resize(40, 4);
        assert_eq!(screen_text(&s), ["$", "", "", ""]);
        assert_eq!((s.cursor_x, s.cursor_y), (2, 0));
    }

    #[test]
    fn widening_wrapped_text_after_clear_does_not_pull_the_cleared_history_in() {
        let mut s = cleared_after_ten_lines();
        let a = "A".repeat(30);
        let b = "B".repeat(30);
        s.process_output(format!("{a}\r\n{b}\r\n$ ").as_bytes());
        s.resize(40, 6);
        assert_eq!(screen_text(&s), [a.as_str(), b.as_str(), "$", "", "", ""]);
        assert_eq!(s.cursor_y, 2);
    }

    #[test]
    fn output_that_scrolled_after_clear_comes_back_but_the_cleared_history_does_not() {
        let mut s = cleared_after_ten_lines();
        // Four 30-character lines take eight rows on a six-row screen, so some
        // of the text printed after the clear scrolls into the scrollback.
        for i in 0..4 {
            s.process_output(format!("L{i}{}\r\n", "x".repeat(28)).as_bytes());
        }
        s.process_output(b"$ ");
        s.resize(40, 6);
        assert!(!shows_cleared_history(&s), "widened: {:?}", screen_text(&s));
        s.resize(40, 16);
        assert!(!shows_cleared_history(&s), "grown: {:?}", screen_text(&s));
        let text = screen_text(&s);
        for i in 0..4 {
            let want = format!("L{i}{}", "x".repeat(28));
            assert!(
                text.contains(&want),
                "{want:?} comes back into view: {text:?}"
            );
        }
        assert_eq!(
            text[0],
            format!("L0{}", "x".repeat(28)),
            "the text starts at the top"
        );
    }

    #[test]
    fn growing_pulls_back_the_lines_that_scrolled_off() {
        let mut s = Screen::new(20, 4, 100);
        for i in 0..10 {
            s.process_output(format!("x{i}\r\n").as_bytes());
        }
        s.resize(20, 8);
        assert_eq!(
            row_text(&s, 0).trim_end(),
            "x3",
            "the 4th-newest history line"
        );
        assert_eq!(s.cursor_y, 7);
    }

    #[test]
    fn rows_a_shrink_pushes_off_come_back_when_the_screen_grows() {
        let mut s = Screen::new(20, 4, 100);
        s.process_output(b"\x1b[2J\x1b[Hr0\r\nr1\r\nr2\r\nr3");
        s.resize(20, 2);
        s.resize(20, 4);
        assert_eq!(screen_text(&s), ["r0", "r1", "r2", "r3"]);
        assert_eq!(s.cursor_y, 3);
    }

    #[test]
    fn ed_0_from_home_counts_as_a_clear() {
        let mut s = cleared_with_history_by(b"\x1b[H\x1b[J");
        s.resize(20, 8);
        assert_eq!(
            s.cursor_y, 0,
            "growing does not pull the cleared history back"
        );
        assert_eq!(row_text(&s, 0).trim_end(), "$");
    }

    #[test]
    fn a_clear_on_the_alternate_screen_leaves_the_history_pullable() {
        let mut s = Screen::new(20, 4, 100);
        for i in 0..10 {
            s.process_output(format!("x{i}\r\n").as_bytes());
        }
        s.process_output(b"\x1b[?1049h\x1b[2J\x1b[?1049l");
        s.resize(20, 8);
        assert_eq!(row_text(&s, 0).trim_end(), "x3", "the history comes back");
    }

    #[test]
    fn a_height_only_resize_after_clear_keeps_the_cleared_history_out() {
        let mut s = cleared_after_ten_lines();
        let a = "A".repeat(30);
        s.process_output(format!("{a}\r\n$ ").as_bytes());
        s.resize(20, 10);
        assert!(!shows_cleared_history(&s), "grown: {:?}", screen_text(&s));
        s.resize(20, 3);
        s.resize(20, 6);
        assert!(
            !shows_cleared_history(&s),
            "shrunk and grown: {:?}",
            screen_text(&s)
        );
        assert_eq!(row_text(&s, 0).trim_end(), &a[..20]);
    }

    #[test]
    fn rows_spilled_while_the_alternate_screen_is_up_come_back() {
        let mut s = Screen::new(20, 6, 100);
        s.process_output(b"\x1b[2J\x1b[Hp0\r\np1\r\np2\r\np3\r\n$ ");
        s.process_output(b"\x1b[?1049h");
        s.resize(20, 3);
        s.process_output(b"\x1b[?1049l");
        s.resize(20, 6);
        assert_eq!(screen_text(&s)[..5], ["p0", "p1", "p2", "p3", "$"]);
    }

    #[test]
    fn a_width_change_after_clear_keeps_the_prompt_at_the_top() {
        let mut s = cleared_with_history();
        let history = s.scrollback.len();
        s.resize(15, 4);
        assert_eq!(
            screen_text(&s),
            ["$", "", "", ""],
            "no history comes back into view"
        );
        assert_eq!(
            (s.cursor_x, s.cursor_y),
            (2, 0),
            "the cursor stays on the prompt row"
        );
        assert_eq!(
            s.scrollback.len(),
            history,
            "the history stays where it was"
        );
    }

    #[test]
    fn a_height_change_after_clear_keeps_the_prompt_at_the_top() {
        let mut s = cleared_with_history();
        s.resize(20, 8);
        assert_eq!(s.cursor_y, 0, "growing does not pull cleared history back");
        assert_eq!(row_text(&s, 0).trim_end(), "$");
        s.resize(20, 3);
        assert_eq!(
            s.cursor_y, 0,
            "shrinking eats the blank rows below the cursor"
        );
        assert_eq!(screen_text(&s), ["$", "", ""]);
    }

    #[test]
    fn a_prompt_mid_screen_after_clear_keeps_its_row_through_resizes() {
        let mut s = Screen::new(20, 6, 100);
        for i in 0..10 {
            s.process_output(format!("old output {i}\r\n").as_bytes());
        }
        s.process_output(b"\x1b[H\x1b[2J$ echo hi\r\nhi\r\n$ ");
        let history = s.scrollback.len();
        for (cols, when) in [(12, "narrower"), (30, "wider"), (20, "back")] {
            s.resize(cols, 6);
            assert_eq!(s.cursor_y, 2, "{when}: the cursor keeps its row");
            assert_eq!(
                screen_text(&s)[..3],
                ["$ echo hi", "hi", "$"],
                "{when}: the screen above the cursor is unchanged"
            );
            assert_eq!(
                s.scrollback.len(),
                history,
                "{when}: nothing moves to or from history"
            );
        }
    }

    #[test]
    fn coloured_rows_below_the_cursor_come_back_with_scrollback_present() {
        let mut s = cleared_with_history();
        // Blue from col 12 on row 2, below the prompt.
        s.process_output(b"\x1b[3;13H\x1b[44m\x1b[K\x1b[0m\x1b[1;3H");
        s.resize(15, 4);
        assert_eq!(s.cursor_y, 0);
        assert_eq!(s.grid[2][5].attrs.bg, Color::Default, "row 2 col 5");
        assert_eq!(s.grid[2][12].attrs.bg, Color::Indexed(4), "row 2 col 12");
        assert_eq!(s.grid[2][14].attrs.bg, Color::Indexed(4), "row 2 new edge");
    }

    #[test]
    fn repeated_resizes_after_clear_are_stable() {
        let mut s = cleared_with_history();
        let history = s.scrollback.len();
        for _ in 0..5 {
            for (cols, rows) in [(15, 4), (20, 6), (12, 3), (20, 4)] {
                s.resize(cols, rows);
                assert_eq!(s.cursor_y, 0, "{cols}x{rows}: the prompt stays at the top");
                assert_eq!(row_text(&s, 0).trim_end(), "$", "{cols}x{rows}");
            }
        }
        assert_eq!(
            s.scrollback.len(),
            history,
            "the history neither grows nor shrinks"
        );
    }

    #[test]
    fn a_full_screen_of_output_still_scrolls_into_history_when_narrowed() {
        let mut s = Screen::new(20, 4, 100);
        for i in 0..4 {
            s.process_output(format!("line {i} 1234567890").as_bytes());
            if i < 3 {
                s.process_output(b"\r\n");
            }
        }
        assert!(s.scrollback.is_empty());
        s.resize(10, 4);
        // Every 16-character line now takes two rows, so the first two lines
        // move into the scrollback and the cursor stays on the last row.
        assert_eq!(s.scrollback.len(), 4);
        assert_eq!(s.cursor_y, 3);
        assert_eq!(
            screen_text(&s),
            ["line 2 123", "4567890", "line 3 123", "4567890"]
        );
    }

    #[test]
    fn vertical_resize_scrolls_off_the_top_instead_of_deleting_the_bottom() {
        // Zooming a pane that already spans the full width changes only the
        // height, which used to take the clamp path. `clamp_grid` keeps the TOP
        // `rows`, so unzooming deleted every line that no longer fit -- cursor,
        // prompt and newest output first -- and deleted it outright: the lines
        // never reached scrollback, so nothing could ever scroll back to them.
        let mut s = Screen::new(80, 24, 2000);
        for i in 0..39 {
            s.process_output(format!("early {i}\r\n").as_bytes());
        }
        s.resize(80, 45); // zoom (height only)
        for i in 0..30 {
            s.process_output(format!("zoomed {i}\r\n").as_bytes());
        }
        let total_before = s.total_lines();
        s.resize(80, 24); // unzoom (height only)

        assert_eq!(
            s.total_lines(),
            total_before,
            "a height change may move lines between grid and scrollback, never drop them"
        );
        let lines = all_lines(&s);
        for i in 0..30 {
            let want = format!("zoomed {i}");
            assert!(
                lines.contains(&want),
                "{want:?} was deleted by the unzoom; kept {lines:?}"
            );
        }
        assert!(lines.contains(&"early 0".to_string()), "history intact too");
        // The newest line is still ON the screen, not scrolled past it.
        assert!(
            all_lines(&s)[s.scrollback.len()..].contains(&"zoomed 29".to_string()),
            "the bottom of the screen must survive the shrink"
        );
    }

    #[test]
    fn zoom_cycles_keep_growing_the_scrollback() {
        // The user-visible shape of the bug: `max_scroll_offset()` frozen while
        // real output streams past, so the wheel and Visual mode both stop at
        // the same stale line no matter how much has since been printed.
        let mut s = Screen::new(80, 24, 5000);
        for i in 0..39 {
            s.process_output(format!("early {i}\r\n").as_bytes());
        }
        for cycle in 0..3 {
            let before = s.max_scroll_offset();
            s.resize(80, 45);
            for i in 0..30 {
                s.process_output(format!("cycle {cycle} line {i}\r\n").as_bytes());
            }
            s.resize(80, 24);
            // 30 lines printed at the same height means 30 more reachable lines.
            // The clamp path let the zoom absorb most of them, so the wheel kept
            // stopping at very nearly the line it stopped at last time.
            assert_eq!(
                s.max_scroll_offset(),
                before + 30,
                "cycle {cycle}: 30 lines of output must add 30 scrollable lines"
            );
        }
    }

    /// A float zoom changes both axes at once, unlike a fullscreen zoom of a
    /// full-width pane. Lines are short enough that no resize here wraps them,
    /// so any line that goes missing was dropped, not reflowed.
    fn float_round_trip_keeps_every_line(slot: (u16, u16), float: (u16, u16)) {
        let mut s = Screen::new(slot.0, slot.1, 2000);
        for i in 0..60 {
            s.process_output(format!("early {i}\r\n").as_bytes());
        }
        s.resize(float.0, float.1); // float zoom
        for i in 0..30 {
            s.process_output(format!("floated {i}\r\n").as_bytes());
        }
        let total_before = s.total_lines();
        s.resize(slot.0, slot.1); // snap back

        assert_eq!(
            s.total_lines(),
            total_before,
            "{slot:?} -> {float:?} -> {slot:?}: lines may move between grid and scrollback, never vanish"
        );
        let lines = all_lines(&s);
        for i in 0..30 {
            let want = format!("floated {i}");
            assert!(
                lines.contains(&want),
                "{want:?} was deleted by the snap-back; kept {lines:?}"
            );
        }
        for i in 0..60 {
            let want = format!("early {i}");
            assert!(lines.contains(&want), "{want:?} lost from the history");
        }
        assert!(
            all_lines(&s)[s.scrollback.len()..].contains(&"floated 29".to_string()),
            "the newest line must still be on the screen after the snap-back"
        );
    }

    #[test]
    fn float_zoom_of_a_narrow_pane_into_a_wider_shorter_box_and_back_keeps_every_line() {
        float_round_trip_keeps_every_line((33, 27), (88, 24));
    }

    #[test]
    fn float_zoom_of_a_big_pane_into_a_smaller_box_and_back_keeps_every_line() {
        float_round_trip_keeps_every_line((98, 27), (88, 24));
    }

    #[test]
    fn float_zoom_of_a_short_pane_into_a_taller_narrower_box_and_back_keeps_every_line() {
        float_round_trip_keeps_every_line((100, 12), (60, 30));
    }

    #[test]
    fn float_zoom_cycles_keep_growing_the_scrollback() {
        let mut s = Screen::new(33, 27, 5000);
        for i in 0..60 {
            s.process_output(format!("early {i}\r\n").as_bytes());
        }
        for cycle in 0..3 {
            let before = s.max_scroll_offset();
            s.resize(88, 24);
            for i in 0..30 {
                s.process_output(format!("cycle {cycle} line {i}\r\n").as_bytes());
            }
            s.resize(33, 27);
            assert_eq!(
                s.max_scroll_offset(),
                before + 30,
                "cycle {cycle}: 30 lines of output must add 30 scrollable lines"
            );
        }
    }

    #[test]
    fn float_zoom_round_trip_with_a_full_screen_app_up_keeps_the_primary_screens_lines() {
        for (slot, float) in [((33u16, 27u16), (88u16, 24u16)), ((98, 27), (88, 24))] {
            let mut s = Screen::new(slot.0, slot.1, 2000);
            for i in 0..100 {
                s.process_output(format!("primary {i}\r\n").as_bytes());
            }
            let total_before = s.total_lines();

            s.process_output(b"\x1b[?1049h");
            s.resize(float.0, float.1); // float the pane while the app is up
            for i in 0..10 {
                s.process_output(format!("app {i}\r\n").as_bytes());
            }
            s.resize(slot.0, slot.1); // snap back while the app is still up
            s.process_output(b"\x1b[?1049l");

            // The parked primary screen pads with blank rows when it grows back
            // instead of pulling history down, so the count may rise but must
            // never fall.
            assert!(
                s.total_lines() >= total_before,
                "{slot:?} -> {float:?}: the round trip must not consume primary lines"
            );
            let lines = all_lines(&s);
            for i in 0..100 {
                let want = format!("primary {i}");
                assert!(
                    lines.contains(&want),
                    "{want:?} lost to the float round trip"
                );
            }
        }
    }

    #[test]
    fn resize_while_alt_active_keeps_the_primary_screens_bottom_rows() {
        // A pane resized while a full-screen application is up (zoom, split, a
        // client attaching smaller) shrinks the parked primary snapshot too.
        // Clamping it kept the top rows, so the application's exit handed back a
        // primary screen missing its last few dozen lines -- including the shell
        // prompt the user was looking at before they started the app.
        let mut s = Screen::new(80, 40, 2000);
        for i in 0..100 {
            s.process_output(format!("primary {i}\r\n").as_bytes());
        }
        let total_before = s.total_lines();

        s.process_output(b"\x1b[?1049h");
        s.resize(80, 10); // unzoom while the app is up
        s.process_output(b"\x1b[?1049l");

        assert_eq!(
            s.total_lines(),
            total_before,
            "the round trip must not consume primary lines"
        );
        let lines = all_lines(&s);
        for i in 0..100 {
            let want = format!("primary {i}");
            assert!(
                lines.contains(&want),
                "{want:?} lost to the alt-active resize"
            );
        }
    }

    #[test]
    fn alt_screen_round_trip_survives_a_resize() {
        // The round trip plus a resize in the middle -- the combination the
        // plain round-trip test above deliberately leaves out.
        for (cols, rows) in [(200u16, 50u16), (40, 12), (80, 50), (200, 24)] {
            let mut s = Screen::new(80, 24, 2000);
            for i in 0..200 {
                s.process_output(format!("history {i:03}\r\n").as_bytes());
            }
            assert!(s.max_scroll_offset() > 0);

            s.process_output(b"\x1b[?1049h");
            assert_eq!(s.max_scroll_offset(), 0, "the alt screen is historyless");
            for i in 0..80 {
                s.process_output(format!("alt {i}\r\n").as_bytes());
            }
            s.resize(cols, rows);
            s.process_output(b"\x1b[?1049l");

            // The row count decides how the same content is split between grid
            // and scrollback, so `max_scroll_offset()` legitimately moves. What
            // may not move is the content: every line still addressable.
            let lines = all_lines(&s);
            for i in 0..200 {
                let want = format!("history {i:03}");
                assert!(
                    lines.contains(&want),
                    "{want:?} lost to a {cols}x{rows} resize while alt-active"
                );
            }
            assert!(
                !lines.iter().any(|l| l.starts_with("alt ")),
                "the alt screen's own output must never enter the primary history"
            );
        }
    }

    #[test]
    fn wrapped_history_survives_a_width_change_while_alt_active() {
        // Lines long enough to wrap, so the rejoin/re-split actually has work to
        // do when the pane comes back to its original width.
        let mut s = Screen::new(80, 24, 4000);
        for i in 0..200 {
            s.process_output(format!("line{i:03} {}\r\n", "x".repeat(150)).as_bytes());
        }
        let max_before = s.max_scroll_offset();

        s.process_output(b"\x1b[?1049h");
        s.resize(200, 50);
        s.process_output(b"\x1b[?1049l");
        s.resize(80, 24);

        assert_eq!(
            s.max_scroll_offset(),
            max_before,
            "a width round trip must land back on the same rewrap"
        );
        assert!(all_lines(&s)[0].starts_with("line000 "));
    }

    #[test]
    fn an_app_killed_on_the_alt_screen_does_not_take_the_history_with_it() {
        // No `1049l` ever arrives. The primary history is parked, not lost: the
        // alt screen's own output must not reach it, and the next clean leave --
        // a later application exiting normally -- must hand it back whole.
        let mut s = Screen::new(80, 24, 2000);
        for i in 0..200 {
            s.process_output(format!("history {i:03}\r\n").as_bytes());
        }
        let max_before = s.max_scroll_offset();

        s.process_output(b"\x1b[?1049h");
        for i in 0..500 {
            s.process_output(format!("output after the app died {i}\r\n").as_bytes());
        }
        assert_eq!(
            s.max_scroll_offset(),
            0,
            "still believed to be on the alt screen, so still historyless"
        );

        // A later application starts (a no-op enter) and exits cleanly.
        s.process_output(b"\x1b[?1049h");
        s.process_output(b"\x1b[?1049l");
        assert_eq!(
            s.max_scroll_offset(),
            max_before,
            "the parked history comes back whole, never a truncated or empty one"
        );
        let lines = all_lines(&s);
        assert!(lines.contains(&"history 000".to_string()));
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("output after the app died")),
            "alt-screen output must not have leaked into the primary history"
        );
    }

    #[test]
    fn the_scrollback_park_is_held_exactly_while_the_alt_screen_is_up() {
        // The park and `alt_screen_active` must agree after ANY sequence of
        // enters and leaves, balanced or not: a park held while the primary
        // screen is live would freeze history, and a released one while the alt
        // screen is up would let an application's redraws into it.
        let mut s = Screen::new(80, 24, 2000);
        for i in 0..100 {
            s.process_output(format!("history {i:03}\r\n").as_bytes());
        }
        for seq in [
            &b"\x1b[?1049h"[..],
            &b"\x1b[?1049h"[..],
            &b"\x1b[?1047h"[..],
            &b"\x1b[?1047l"[..],
            &b"\x1b[?1049l"[..],
            &b"\x1b[?1047h"[..],
            &b"\x1b[?1049l"[..],
            &b"\x1b[?1047l"[..],
        ] {
            s.process_output(seq);
            assert_eq!(
                s.saved_scrollback.is_some(),
                s.alt_screen_active,
                "park and alt flag disagree after {:?}",
                String::from_utf8_lossy(seq)
            );
            // The two snapshots are held and released together. `spill_saved_rows`
            // branches on `saved_grid` but spills into `saved_scrollback`, so a
            // grid parked without a scrollback to spill into would silently drop
            // the rows it displaces -- the very defect this file is about.
            assert_eq!(
                s.saved_grid.is_some(),
                s.saved_scrollback.is_some(),
                "the grid and scrollback snapshots disagree after {:?}",
                String::from_utf8_lossy(seq)
            );
            assert_eq!(
                s.total_lines(),
                s.scrollback.len() + s.grid.len(),
                "total_lines must stay consistent after {:?}",
                String::from_utf8_lossy(seq)
            );
        }
        assert!(all_lines(&s).contains(&"history 000".to_string()));
    }

    #[test]
    fn test_osc8_bel_terminated() {
        let mut s = Screen::new(40, 3, 100);
        s.process_output(b"\x1b]8;;http://x\x07A\x1b]8;;\x07B");
        assert_eq!(s.grid[0][0].hyperlink.as_deref(), Some("http://x"));
        assert_eq!(s.grid[0][1].hyperlink, None);
    }
}
