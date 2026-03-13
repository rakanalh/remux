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
            queue!(stdout, MoveTo(cursor_x, cursor_y), cursor::Show)?;
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
            queue!(stdout, MoveTo(cursor_x, cursor_y), cursor::Show)?;
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

    /// Clear the overlay by re-rendering the front buffer rows that might
    /// have been affected (bottom portion of screen).
    pub fn clear_overlay(&mut self, cols: u16, rows: u16) -> Result<()> {
        // Re-render the current front buffer to clear any overlay.
        let cells = self.front.clone();
        // Determine cursor position from existing state (place at 0,0 hidden).
        self.render_full(&cells, 0, 0, false)?;
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
}
