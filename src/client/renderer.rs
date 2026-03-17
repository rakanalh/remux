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

use crate::client::input::{SelectionMode, VisualState};
use crate::client::whichkey::DrawCommand;
use crate::protocol::{CellChange, CellColor, RenderCell};

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// The client-side renderer that maintains a front buffer and uses crossterm
/// to draw changes to the actual terminal.
pub struct Renderer {
    /// The front buffer: what is currently displayed on screen.
    front: Vec<Vec<RenderCell>>,
    cols: u16,
    rows: u16,
}

impl Renderer {
    /// Create a new renderer with the given dimensions.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            front: vec![vec![RenderCell::default(); cols as usize]; rows as usize],
            cols,
            rows,
        }
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

        // Hide cursor during rendering to avoid flicker.
        queue!(stdout, cursor::Hide)?;

        for (y, row) in cells.iter().enumerate() {
            if y as u16 >= self.rows {
                break;
            }
            queue!(stdout, MoveTo(0, y as u16))?;

            let mut last_fg = CellColor::Default;
            let mut last_bg = CellColor::Default;
            let mut last_bold = false;
            let mut last_italic = false;
            let mut last_underline = false;

            queue!(stdout, ResetColor)?;

            for (x, cell) in row.iter().enumerate() {
                if x as u16 >= self.cols {
                    break;
                }

                // Apply style changes only when needed.
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

                queue!(stdout, Print(cell.c))?;
            }

            queue!(stdout, ResetColor)?;
        }

        // Update cursor.
        if cursor_visible {
            queue!(
                stdout,
                MoveTo(cursor_x, cursor_y),
                cursor_style_command(cursor_style),
                cursor::Show,
            )?;
        } else {
            queue!(stdout, cursor::Hide)?;
        }

        stdout.flush()?;

        // Update front buffer.
        self.front = cells.to_vec();

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
        let mut stdout = io::stdout().lock();

        queue!(stdout, cursor::Hide)?;

        for change in changes {
            if change.x >= self.cols || change.y >= self.rows {
                continue;
            }

            queue!(
                stdout,
                MoveTo(change.x, change.y),
                SetForegroundColor(cell_color_to_crossterm(&change.cell.fg)),
                SetBackgroundColor(cell_color_to_crossterm(&change.cell.bg)),
            )?;

            if change.cell.bold {
                queue!(stdout, SetAttribute(Attribute::Bold))?;
            }
            if change.cell.italic {
                queue!(stdout, SetAttribute(Attribute::Italic))?;
            }
            if change.cell.underline {
                queue!(stdout, SetAttribute(Attribute::Underlined))?;
            }

            queue!(stdout, Print(change.cell.c), ResetColor)?;

            // Update front buffer.
            let y = change.y as usize;
            let x = change.x as usize;
            if y < self.front.len() && x < self.front[y].len() {
                self.front[y][x] = change.cell.clone();
            }
        }

        // Update cursor.
        if cursor_visible {
            queue!(
                stdout,
                MoveTo(cursor_x, cursor_y),
                cursor_style_command(cursor_style),
                cursor::Show,
            )?;
        } else {
            queue!(stdout, cursor::Hide)?;
        }

        stdout.flush()?;
        Ok(())
    }

    /// Resize the renderer to new dimensions.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
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
            let text: String = cmd.text.chars().take(max_chars).collect();
            queue!(stdout, Print(text), ResetColor)?;
        }

        stdout.flush()?;
        Ok(())
    }

    /// Render visual mode selection highlighting and cursor on top of the
    /// current front buffer. All coordinates are offset by the pane's position
    /// in the composited buffer (`pane_offset_x`, `pane_offset_y`) and clamped
    /// to the pane bounds.
    pub fn render_visual_overlay(&self, visual_state: &VisualState) -> Result<()> {
        let mut stdout = io::stdout().lock();
        queue!(stdout, cursor::Hide)?;

        let pane_ox = visual_state.pane_offset_x;
        let pane_oy = visual_state.pane_offset_y;
        let pane_w = visual_state.visible_cols;
        let pane_h = visual_state.visible_rows;

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
                    let row = &self.front[screen_y];
                    if screen_x >= row.len() {
                        break;
                    }
                    let cell = &row[screen_x];

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
                    queue!(stdout, Print(cell.c), ResetColor)?;
                }
            }
        }

        // Render cursor as a block highlight at the cursor position (pane-relative).
        let cursor_screen_col = pane_ox + visual_state.cursor_col as u16;
        let cursor_screen_row = pane_oy + visual_state.cursor_row as u16;

        if cursor_screen_row < self.rows && cursor_screen_col < self.cols {
            let row_idx = cursor_screen_row as usize;
            let col_idx = cursor_screen_col as usize;
            if row_idx < self.front.len() && col_idx < self.front[row_idx].len() {
                let cell = &self.front[row_idx][col_idx];
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
                        Print(cell.c),
                        ResetColor,
                    )?;
                }
            }
        }

        stdout.flush()?;
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

        let pane_ox = visual_state.pane_offset_x as usize;
        let pane_oy = visual_state.pane_offset_y as usize;
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
            let row = &self.front[screen_y];

            // Extract only the pane's columns from the composited row.
            let pane_row_len = pane_w.min(row.len().saturating_sub(pane_ox));
            let pane_row: Vec<&RenderCell> = (0..pane_row_len).map(|c| &row[pane_ox + c]).collect();

            if is_line_mode {
                let line: String = pane_row.iter().map(|c| c.c).collect();
                result.push_str(line.trim_end());
                result.push('\n');
            } else if start_row == end_row {
                let cs = start_col.min(pane_row.len());
                let ce = (end_col + 1).min(pane_row.len());
                let text: String = pane_row[cs..ce].iter().map(|c| c.c).collect();
                result.push_str(text.trim_end());
            } else if scrollback_row == start_row {
                let cs = start_col.min(pane_row.len());
                let text: String = pane_row[cs..].iter().map(|c| c.c).collect();
                result.push_str(text.trim_end());
                result.push('\n');
            } else if scrollback_row == end_row {
                let ce = (end_col + 1).min(pane_row.len());
                let text: String = pane_row[..ce].iter().map(|c| c.c).collect();
                result.push_str(text.trim_end());
            } else {
                let text: String = pane_row.iter().map(|c| c.c).collect();
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
        let top_border = format!(
            "\u{256d}{}\u{2500}{}\u{256e}",
            "\u{2500}".repeat(half_left),
            "\u{2500}".repeat(half_right),
        );
        // Build top border with title inserted
        let top_with_title = format!(
            "\u{256d}{}{}{}\u{256e}",
            "\u{2500}".repeat(half_left),
            title,
            "\u{2500}".repeat(half_right),
        );
        let _ = top_border; // unused, we use top_with_title
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
                "\u{2502} {}{} \u{2502}",
                display_text,
                " ".repeat(padding)
            ))
        )?;

        // Draw bottom border
        queue!(stdout, MoveTo(start_x, start_y + 2))?;
        queue!(
            stdout,
            Print(format!(
                "\u{2570}{}\u{256f}",
                "\u{2500}".repeat(popup_width.saturating_sub(2) as usize)
            ))
        )?;

        // Position cursor at end of text
        let cursor_x = start_x + 2 + display_text.len() as u16;
        queue!(stdout, MoveTo(cursor_x, start_y + 1), cursor::Show)?;

        stdout.flush()?;
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
        let display: String = prompt.chars().take(max_len).collect();
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

        stdout.flush()?;
        Ok(())
    }

    /// Render search match highlights on top of the current front buffer.
    ///
    /// Highlights all visible matches with a subtle background, and the
    /// current match with a bright background. Match positions are in
    /// scrollback coordinates; only those within the visible area of the
    /// focused pane are drawn.
    pub fn render_search_highlight(
        &self,
        matches: &[(usize, usize)],
        current_match: usize,
        query_len: usize,
        scrollback_line_count: usize,
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

        // The visible line range in scrollback coordinates.
        let visible_start = scrollback_line_count.saturating_sub(pane_h);
        let visible_end = scrollback_line_count;

        let mut stdout = io::stdout().lock();
        queue!(stdout, cursor::Hide)?;

        for (idx, &(line, col)) in matches.iter().enumerate() {
            if line < visible_start || line >= visible_end {
                continue;
            }

            let screen_y = pr.y as usize + (line - visible_start);
            let screen_x_start = pr.x as usize + col;

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
                if screen_x >= (pr.x + pr.width) as usize || screen_y >= (pr.y + pr.height) as usize
                {
                    break;
                }

                let cell_char =
                    if screen_y < self.front.len() && screen_x < self.front[screen_y].len() {
                        self.front[screen_y][screen_x].c
                    } else {
                        ' '
                    };

                queue!(
                    stdout,
                    MoveTo(screen_x as u16, screen_y as u16),
                    SetForegroundColor(hl_fg),
                    SetBackgroundColor(hl_bg),
                    Print(cell_char),
                    ResetColor,
                )?;
            }
        }

        stdout.flush()?;
        Ok(())
    }

    /// Get a reference to the front buffer (for testing/inspection).
    pub fn front_buffer(&self) -> &[Vec<RenderCell>] {
        &self.front
    }

    /// Clear the overlay by re-rendering the front buffer rows that might
    /// have been affected (bottom portion of screen).
    pub fn clear_overlay(&mut self, cols: u16, rows: u16) -> Result<()> {
        // Re-render the current front buffer to clear any overlay.
        let cells = self.front.clone();
        // Determine cursor position from existing state (place at 0,0 hidden).
        self.render_full(&cells, 0, 0, false, 0)?;
        let _ = (cols, rows); // suppress unused warnings
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
fn cell_color_to_crossterm(color: &CellColor) -> Color {
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
