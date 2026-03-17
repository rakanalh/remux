//! VTE-based terminal screen buffer.
//!
//! This module implements a virtual terminal screen that processes VT100/xterm
//! escape sequences via the `vte` crate. Each pane in the multiplexer owns its
//! own `Screen` instance.

use std::collections::VecDeque;

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
    pub attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            attrs: CellAttrs::default(),
        }
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
    pub grid: Vec<Vec<Cell>>,
    pub cursor_x: u16,
    pub cursor_y: u16,
    /// The current text attributes applied to newly printed characters.
    pub current_attrs: CellAttrs,
    /// Lines that have scrolled off the top of the visible area.
    pub scrollback: VecDeque<Vec<Cell>>,
    /// Maximum number of scrollback lines to retain.
    pub scrollback_limit: usize,
    /// Top of the scroll region (0-based, inclusive).
    pub scroll_top: u16,
    /// Bottom of the scroll region (0-based, inclusive).
    pub scroll_bottom: u16,
    /// The VTE parser instance.
    parser: vte::Parser,
    /// Saved primary screen state for alternate screen buffer switching.
    saved_grid: Option<Vec<Vec<Cell>>>,
    saved_cursor_x: u16,
    saved_cursor_y: u16,
    saved_attrs: CellAttrs,
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
    /// Saved cursor position for CSI s/u (separate from alt screen save).
    scp_cursor_x: u16,
    scp_cursor_y: u16,
    /// Whether renders are locked (Synchronized Update mode, CSI ? 2026 h/l).
    pub lock_renders: bool,
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
            scrollback: VecDeque::new(),
            scrollback_limit,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            parser: vte::Parser::new(),
            saved_grid: None,
            saved_cursor_x: 0,
            saved_cursor_y: 0,
            saved_attrs: CellAttrs::default(),
            saved_scroll_top: 0,
            saved_scroll_bottom: rows.saturating_sub(1),
            alt_screen_active: false,
            cursor_visible: true,
            cursor_style: 0,
            pty_responses: Vec::new(),
            scp_cursor_x: 0,
            scp_cursor_y: 0,
            lock_renders: false,
        }
    }

    /// Resize the screen to new dimensions.
    ///
    /// Content is preserved where possible. New cells are filled with defaults.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let mut new_grid = Self::make_grid(cols, rows);

        let copy_rows = std::cmp::min(self.rows as usize, rows as usize);
        let copy_cols = std::cmp::min(self.cols as usize, cols as usize);

        for (r, new_row) in new_grid.iter_mut().enumerate().take(copy_rows) {
            for (c, new_cell) in new_row.iter_mut().enumerate().take(copy_cols) {
                *new_cell = self.grid[r][c].clone();
            }
        }

        self.cols = cols;
        self.rows = rows;
        self.grid = new_grid;
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

    /// Feed raw terminal output bytes through the VTE parser, updating the
    /// screen state.
    pub fn process_output(&mut self, data: &[u8]) {
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

    // -- Private helpers ----------------------------------------------------

    /// Create an empty grid filled with default cells.
    fn make_grid(cols: u16, rows: u16) -> Vec<Vec<Cell>> {
        vec![vec![Cell::default(); cols as usize]; rows as usize]
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
        // scrollback.
        if top == 0 {
            let evicted = self.grid[0].clone();
            self.scrollback.push_back(evicted);
            while self.scrollback.len() > self.scrollback_limit {
                self.scrollback.pop_front();
            }
        }

        // Shift lines up within the region.
        for r in top..bottom {
            self.grid[r] = self.grid[r + 1].clone();
        }
        self.grid[bottom] = vec![Cell::default(); self.cols as usize];
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
        self.grid[top] = vec![Cell::default(); self.cols as usize];
    }

    /// Write a character at the current cursor position, then advance the
    /// cursor. Wraps to the next line at the end of a row and scrolls if
    /// necessary.
    fn put_char(&mut self, c: char) {
        if self.cursor_x >= self.cols {
            self.cursor_x = 0;
            self.cursor_y += 1;
            if self.cursor_y > self.scroll_bottom {
                self.cursor_y = self.scroll_bottom;
                self.scroll_up_region();
            }
        }

        let row = self.cursor_y as usize;
        let col = self.cursor_x as usize;

        if row < self.grid.len() && col < self.grid[row].len() {
            self.grid[row][col] = Cell {
                c,
                attrs: self.current_attrs.clone(),
            };
        }

        self.cursor_x += 1;
    }

    /// Handle the SGR (Select Graphic Rendition) escape sequence.
    fn handle_sgr(&mut self, params: &[&[u16]]) {
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
                4 => self.current_attrs.underline = true,
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

                // Extended color: 38;5;N (256-color fg) or 38;2;R;G;B (truecolor fg)
                38 => {
                    if i + 1 < params.len() {
                        let sub = params[i + 1].first().copied().unwrap_or(0);
                        if sub == 5 && i + 2 < params.len() {
                            let n = params[i + 2].first().copied().unwrap_or(0);
                            self.current_attrs.fg = Color::Indexed(n as u8);
                            i += 2;
                        } else if sub == 2 && i + 4 < params.len() {
                            let r = params[i + 2].first().copied().unwrap_or(0) as u8;
                            let g = params[i + 3].first().copied().unwrap_or(0) as u8;
                            let b = params[i + 4].first().copied().unwrap_or(0) as u8;
                            self.current_attrs.fg = Color::Rgb(r, g, b);
                            i += 4;
                        }
                    }
                }

                // Extended color: 48;5;N (256-color bg) or 48;2;R;G;B (truecolor bg)
                48 => {
                    if i + 1 < params.len() {
                        let sub = params[i + 1].first().copied().unwrap_or(0);
                        if sub == 5 && i + 2 < params.len() {
                            let n = params[i + 2].first().copied().unwrap_or(0);
                            self.current_attrs.bg = Color::Indexed(n as u8);
                            i += 2;
                        } else if sub == 2 && i + 4 < params.len() {
                            let r = params[i + 2].first().copied().unwrap_or(0) as u8;
                            let g = params[i + 3].first().copied().unwrap_or(0) as u8;
                            let b = params[i + 4].first().copied().unwrap_or(0) as u8;
                            self.current_attrs.bg = Color::Rgb(r, g, b);
                            i += 4;
                        }
                    }
                }

                _ => {} // Ignore unknown SGR codes
            }
            i += 1;
        }
    }

    /// Take any pending PTY responses (e.g., DSR replies).
    pub fn take_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pty_responses)
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
                self.cursor_x = 0;
            }
            // Line feed / vertical tab / form feed
            0x0A..=0x0C => {
                if self.cursor_y >= self.scroll_bottom {
                    self.scroll_up_region();
                } else {
                    self.cursor_y += 1;
                }
            }
            // Backspace
            0x08 => {
                if self.cursor_x > 0 {
                    self.cursor_x -= 1;
                }
            }
            // Horizontal tab
            0x09 => {
                // Advance to the next tab stop (every 8 columns).
                let next_tab = ((self.cursor_x / 8) + 1) * 8;
                self.cursor_x = std::cmp::min(next_tab, self.cols.saturating_sub(1));
            }
            // Bell
            0x07 => {
                // Ignore bell for now.
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
                let n = Self::csi_param(params, 0, 1);
                self.cursor_y = self.cursor_y.saturating_sub(n);
                if self.cursor_y < self.scroll_top {
                    self.cursor_y = self.scroll_top;
                }
            }
            // CUD - Cursor Down
            'B' => {
                let n = Self::csi_param(params, 0, 1);
                self.cursor_y = std::cmp::min(self.cursor_y + n, self.scroll_bottom);
            }
            // CUF - Cursor Forward (Right)
            'C' => {
                let n = Self::csi_param(params, 0, 1);
                self.cursor_x = std::cmp::min(self.cursor_x + n, self.cols.saturating_sub(1));
            }
            // CUB - Cursor Backward (Left)
            'D' => {
                let n = Self::csi_param(params, 0, 1);
                self.cursor_x = self.cursor_x.saturating_sub(n);
            }
            // CNL - Cursor Next Line (move down n lines, column 1)
            'E' => {
                let n = Self::csi_param(params, 0, 1);
                self.cursor_y = std::cmp::min(self.cursor_y + n, self.scroll_bottom);
                self.cursor_x = 0;
            }
            // CPL - Cursor Previous Line (move up n lines, column 1)
            'F' => {
                let n = Self::csi_param(params, 0, 1);
                self.cursor_y = self.cursor_y.saturating_sub(n);
                if self.cursor_y < self.scroll_top {
                    self.cursor_y = self.scroll_top;
                }
                self.cursor_x = 0;
            }
            // CUP - Cursor Position (row;col, 1-based)
            'H' | 'f' => {
                let row = Self::csi_param(params, 0, 1).saturating_sub(1);
                let col = Self::csi_param(params, 1, 1).saturating_sub(1);
                self.cursor_y = std::cmp::min(row, self.rows.saturating_sub(1));
                self.cursor_x = std::cmp::min(col, self.cols.saturating_sub(1));
            }
            // VPA - Vertical Position Absolute (1-based row)
            'd' => {
                let row = Self::csi_param(params, 0, 1).saturating_sub(1);
                self.cursor_y = std::cmp::min(row, self.rows.saturating_sub(1));
            }
            // HPA / CHA - Horizontal Position Absolute (1-based column)
            'G' => {
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
                        if row < self.grid.len() {
                            for c in col..self.grid[row].len() {
                                self.grid[row][c] = Cell::default();
                            }
                            for r in (row + 1)..self.grid.len() {
                                self.grid[r] = vec![Cell::default(); self.cols as usize];
                            }
                        }
                    }
                    1 => {
                        // Erase from start of display to cursor.
                        let row = self.cursor_y as usize;
                        let col = self.cursor_x as usize;
                        for r in 0..row {
                            self.grid[r] = vec![Cell::default(); self.cols as usize];
                        }
                        if row < self.grid.len() {
                            for c in 0..=std::cmp::min(col, self.grid[row].len().saturating_sub(1))
                            {
                                self.grid[row][c] = Cell::default();
                            }
                        }
                    }
                    2 | 3 => {
                        // Erase entire display.
                        self.grid = Self::make_grid(self.cols, self.rows);
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
                        for c in (self.cursor_x as usize)..cols {
                            self.grid[row][c] = Cell::default();
                        }
                    }
                    1 => {
                        // Erase from start of line to cursor.
                        for c in 0..=std::cmp::min(self.cursor_x as usize, cols.saturating_sub(1)) {
                            self.grid[row][c] = Cell::default();
                        }
                    }
                    2 => {
                        // Erase entire line.
                        self.grid[row] = vec![Cell::default(); self.cols as usize];
                    }
                    _ => {}
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
                        self.grid
                            .insert(row, vec![Cell::default(); self.cols as usize]);
                    }
                }
            }
            // DL - Delete Lines
            'M' => {
                let n = Self::csi_param(params, 0, 1) as usize;
                let row = self.cursor_y as usize;
                let bottom = self.scroll_bottom as usize;

                for _ in 0..n {
                    if row <= bottom && row < self.grid.len() {
                        self.grid.remove(row);
                        self.grid
                            .insert(bottom, vec![Cell::default(); self.cols as usize]);
                    }
                }
            }
            // ICH - Insert Characters
            '@' => {
                let n = Self::csi_param(params, 0, 1) as usize;
                let row = self.cursor_y as usize;
                let col = self.cursor_x as usize;
                if row < self.grid.len() {
                    let line = &mut self.grid[row];
                    for _ in 0..n {
                        if col < line.len() {
                            line.insert(col, Cell::default());
                            line.truncate(self.cols as usize);
                        }
                    }
                }
            }
            // DCH - Delete Characters
            'P' => {
                let n = Self::csi_param(params, 0, 1) as usize;
                let row = self.cursor_y as usize;
                let col = self.cursor_x as usize;
                if row < self.grid.len() {
                    let line = &mut self.grid[row];
                    for _ in 0..n {
                        if col < line.len() {
                            line.remove(col);
                            line.push(Cell::default());
                        }
                    }
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
                self.cursor_x = self.scp_cursor_x;
                self.cursor_y = self.scp_cursor_y;
            }
            // SGR - Select Graphic Rendition
            'm' => {
                self.handle_sgr(params);
            }
            // SM/DECSET - Set Mode
            'h' => {
                let is_private = _intermediates.first() == Some(&b'?');
                if is_private {
                    for param_slice in params {
                        let mode = param_slice[0];
                        match mode {
                            25 => self.cursor_visible = true,
                            2026 => self.lock_renders = true,
                            1049 => {
                                // Save cursor and switch to alternate screen
                                if !self.alt_screen_active {
                                    self.saved_grid = Some(self.grid.clone());
                                    self.saved_cursor_x = self.cursor_x;
                                    self.saved_cursor_y = self.cursor_y;
                                    self.saved_attrs = self.current_attrs.clone();
                                    self.saved_scroll_top = self.scroll_top;
                                    self.saved_scroll_bottom = self.scroll_bottom;
                                    self.grid = Self::make_grid(self.cols, self.rows);
                                    self.cursor_x = 0;
                                    self.cursor_y = 0;
                                    self.scroll_top = 0;
                                    self.scroll_bottom = self.rows.saturating_sub(1);
                                    self.alt_screen_active = true;
                                }
                            }
                            1047 => {
                                // Switch to alternate screen (no cursor save)
                                if !self.alt_screen_active {
                                    self.saved_grid = Some(self.grid.clone());
                                    self.saved_scroll_top = self.scroll_top;
                                    self.saved_scroll_bottom = self.scroll_bottom;
                                    self.grid = Self::make_grid(self.cols, self.rows);
                                    self.cursor_x = 0;
                                    self.cursor_y = 0;
                                    self.scroll_top = 0;
                                    self.scroll_bottom = self.rows.saturating_sub(1);
                                    self.alt_screen_active = true;
                                }
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
                            25 => self.cursor_visible = false,
                            2026 => self.lock_renders = false,
                            1049 => {
                                // Restore primary screen and cursor
                                if self.alt_screen_active {
                                    if let Some(grid) = self.saved_grid.take() {
                                        self.grid = grid;
                                    }
                                    self.cursor_x = self.saved_cursor_x;
                                    self.cursor_y = self.saved_cursor_y;
                                    self.current_attrs = self.saved_attrs.clone();
                                    self.scroll_top = self.saved_scroll_top;
                                    self.scroll_bottom = self.saved_scroll_bottom;
                                    self.alt_screen_active = false;
                                }
                            }
                            1047 => {
                                // Switch back to primary screen
                                if self.alt_screen_active {
                                    if let Some(grid) = self.saved_grid.take() {
                                        self.grid = grid;
                                    }
                                    self.scroll_top = self.saved_scroll_top;
                                    self.scroll_bottom = self.saved_scroll_bottom;
                                    self.alt_screen_active = false;
                                }
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
                if self.cursor_y >= self.scroll_bottom {
                    self.scroll_up_region();
                } else {
                    self.cursor_y += 1;
                }
            }
            // DECSC - Save Cursor Position
            b'7' => {
                self.scp_cursor_x = self.cursor_x;
                self.scp_cursor_y = self.cursor_y;
            }
            // DECRC - Restore Cursor Position
            b'8' => {
                self.cursor_x = self.scp_cursor_x;
                self.cursor_y = self.scp_cursor_y;
            }
            // RI - Reverse Index (move cursor up, scroll if at top)
            b'M' => {
                if self.cursor_y <= self.scroll_top {
                    self.scroll_down_region();
                } else {
                    self.cursor_y -= 1;
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // OSC sequences (e.g. title setting) are acknowledged but currently
        // ignored. A future version may store the window title.
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
}
