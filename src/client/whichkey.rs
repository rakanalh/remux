//! Which-key style key hint popup.
//!
//! This module renders a popup showing available keybindings when the user
//! is partway through a multi-key sequence in Command mode.

use crossterm::style::Color;

use crate::config::theme::Theme;
use crate::config::WhichKeyPosition;

/// A which-key popup that displays available keybindings in a bordered box.
#[derive(Debug)]
pub struct WhichKeyPopup {
    /// Whether the popup is currently visible.
    pub visible: bool,
    /// The label for the current keybinding group (e.g., "Tab").
    pub group_label: String,
    /// The key-label pairs to display (e.g., `('n', "new")`).
    pub entries: Vec<(char, String)>,
}

/// A single rendering command for drawing the popup.
#[derive(Debug, Clone)]
pub struct DrawCommand {
    pub x: u16,
    pub y: u16,
    pub text: String,
    pub fg: Color,
    pub bg: Color,
}

impl WhichKeyPopup {
    /// Create a new hidden popup.
    pub fn new() -> Self {
        Self {
            visible: false,
            group_label: String::new(),
            entries: Vec::new(),
        }
    }

    /// Show the popup with the given group label and entries.
    pub fn show(&mut self, label: String, entries: Vec<(char, String)>) {
        self.visible = true;
        self.group_label = label;
        self.entries = entries;
    }

    /// Hide the popup.
    pub fn hide(&mut self) {
        self.visible = false;
        self.entries.clear();
        self.group_label.clear();
    }

    /// Render the popup into a list of draw commands, using the requested
    /// placement `position`.
    ///
    /// - [`WhichKeyPosition::Anchored`] draws a bordered box centered
    ///   horizontally at the bottom of the screen (the historical default).
    /// - [`WhichKeyPosition::Centered`] draws the same box centered both
    ///   horizontally and vertically.
    /// - [`WhichKeyPosition::FullWidth`] draws an emacs/ivy-like panel spanning
    ///   the full terminal width, anchored above the status bar row.
    pub fn render(
        &self,
        screen_cols: u16,
        screen_rows: u16,
        theme: &Theme,
        position: WhichKeyPosition,
    ) -> Vec<DrawCommand> {
        if !self.visible || self.entries.is_empty() {
            return Vec::new();
        }

        match position {
            WhichKeyPosition::Anchored => self.render_box(screen_cols, screen_rows, theme, false),
            WhichKeyPosition::Centered => self.render_box(screen_cols, screen_rows, theme, true),
            WhichKeyPosition::FullWidth => self.render_full_width(screen_cols, screen_rows, theme),
        }
    }

    /// Render the bordered two-column box. When `centered` is false the box is
    /// anchored to the bottom of the screen (the historical Anchored layout);
    /// when true it is centered vertically as well.
    fn render_box(
        &self,
        screen_cols: u16,
        screen_rows: u16,
        theme: &Theme,
        centered: bool,
    ) -> Vec<DrawCommand> {
        let mut commands = Vec::new();

        // Calculate layout: two columns of entries.
        let num_entries = self.entries.len();
        let rows_needed = num_entries.div_ceil(2);
        let col_width: u16 = 20; // each column is 20 chars wide
        let popup_width = col_width * 2 + 2; // 2 columns + left/right borders
        let popup_height = rows_needed as u16 + 2; // entries + top/bottom border

        // Clamp to screen size.
        if popup_width > screen_cols || popup_height > screen_rows {
            return Vec::new();
        }

        // Position: centered horizontally; vertically centered or anchored to
        // the bottom depending on `centered`.
        let start_x = (screen_cols.saturating_sub(popup_width)) / 2;
        let start_y = if centered {
            (screen_rows.saturating_sub(popup_height)) / 2
        } else {
            screen_rows.saturating_sub(popup_height)
        };

        let fg = theme.whichkey_fg;
        let bg = theme.whichkey_bg;
        let key_fg = theme.whichkey_key_fg;

        // Top border.
        let top_border = format!(
            "\u{256D}{}\u{256E}",
            "\u{2500}".repeat((popup_width - 2) as usize)
        );
        commands.push(DrawCommand {
            x: start_x,
            y: start_y,
            text: top_border,
            fg,
            bg,
        });

        // Entry rows.
        for row in 0..rows_needed {
            let left_idx = row * 2;
            let right_idx = row * 2 + 1;

            let left_entry = self.entries.get(left_idx);
            let right_entry = self.entries.get(right_idx);

            let mut row_text = String::from("\u{2502}");

            // Left column.
            if let Some((key, label)) = left_entry {
                let entry_str = format!(" {} {}", key, label);
                let padded = format!("{:<width$}", entry_str, width = col_width as usize);
                row_text.push_str(&padded);
            } else {
                row_text.push_str(&" ".repeat(col_width as usize));
            }

            // Right column.
            if let Some((key, label)) = right_entry {
                let entry_str = format!(" {} {}", key, label);
                let padded = format!("{:<width$}", entry_str, width = col_width as usize);
                row_text.push_str(&padded);
            } else {
                row_text.push_str(&" ".repeat(col_width as usize));
            }

            row_text.push('\u{2502}');

            // We render the whole row as one draw command with the base colors.
            // The key highlighting would need per-character rendering in a real
            // terminal, but for the data model we store the full text.
            commands.push(DrawCommand {
                x: start_x,
                y: start_y + 1 + row as u16,
                text: row_text,
                fg,
                bg,
            });

            // Add separate draw commands for the key characters so the renderer
            // can apply the highlight color.
            if let Some((key, _)) = left_entry {
                commands.push(DrawCommand {
                    x: start_x + 2,
                    y: start_y + 1 + row as u16,
                    text: key.to_string(),
                    fg: key_fg,
                    bg,
                });
            }
            if let Some((key, _)) = right_entry {
                commands.push(DrawCommand {
                    x: start_x + 2 + col_width,
                    y: start_y + 1 + row as u16,
                    text: key.to_string(),
                    fg: key_fg,
                    bg,
                });
            }
        }

        // Bottom border.
        let bottom_border = format!(
            "\u{2570}{}\u{256F}",
            "\u{2500}".repeat((popup_width - 2) as usize)
        );
        commands.push(DrawCommand {
            x: start_x,
            y: start_y + 1 + rows_needed as u16,
            text: bottom_border,
            fg,
            bg,
        });

        commands
    }

    /// Render the full-width, bottom-anchored panel. Entries flow left-to-right
    /// across as many columns as fit in the terminal width, wrapping onto
    /// additional rows as needed. The group label occupies the top band row.
    fn render_full_width(
        &self,
        screen_cols: u16,
        screen_rows: u16,
        theme: &Theme,
    ) -> Vec<DrawCommand> {
        if screen_cols == 0 {
            return Vec::new();
        }

        let fg = theme.whichkey_fg;
        let bg = theme.whichkey_bg;
        let key_fg = theme.whichkey_key_fg;

        // Each entry occupies a fixed-width cell: " <key> \u{2192} <label>".
        let cell_width: u16 = 22;
        let cols_per_row = (screen_cols / cell_width).max(1);
        let num_entries = self.entries.len() as u16;
        let entry_rows = num_entries.div_ceil(cols_per_row);
        // One row for the label header plus the entry rows.
        let band_height = entry_rows + 1;

        // Reserve the last screen row for the status bar; the band sits above it.
        if band_height + 1 > screen_rows {
            return Vec::new();
        }
        let start_y = screen_rows - 1 - band_height;

        let mut commands = Vec::new();

        // Full-width background band.
        for row in 0..band_height {
            commands.push(DrawCommand {
                x: 0,
                y: start_y + row,
                text: " ".repeat(screen_cols as usize),
                fg,
                bg,
            });
        }

        // Label header on the top band row.
        let label_text = if self.group_label.is_empty() {
            " which-key ".to_string()
        } else {
            format!(" {} ", self.group_label)
        };
        let label_text = label_text
            .chars()
            .take(screen_cols as usize)
            .collect::<String>();
        commands.push(DrawCommand {
            x: 0,
            y: start_y,
            text: label_text,
            fg: key_fg,
            bg,
        });

        // Entry cells, flowing across columns then wrapping to new rows.
        for (i, (key, label)) in self.entries.iter().enumerate() {
            let col = (i as u16) % cols_per_row;
            let row = (i as u16) / cols_per_row;
            let x = col * cell_width;
            let y = start_y + 1 + row;

            // Never draw past the right edge: cap the cell to the remaining
            // width (matters on very narrow screens where a cell would spill).
            let avail = (screen_cols - x) as usize;
            let take = (cell_width as usize).min(avail);
            let entry_str = format!(" {} \u{2192} {}", key, label);
            let entry_str = entry_str.chars().take(take).collect::<String>();
            commands.push(DrawCommand {
                x,
                y,
                text: entry_str,
                fg,
                bg,
            });

            // Highlight the key char (drawn after the leading space). Only when
            // the cell is wide enough for it to fall within screen bounds.
            if take >= 2 {
                commands.push(DrawCommand {
                    x: x + 1,
                    y,
                    text: key.to_string(),
                    fg: key_fg,
                    bg,
                });
            }
        }

        commands
    }
}

impl Default for WhichKeyPopup {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_popup_is_hidden() {
        let popup = WhichKeyPopup::new();
        assert!(!popup.visible);
        assert!(popup.entries.is_empty());
    }

    #[test]
    fn test_show_and_hide() {
        let mut popup = WhichKeyPopup::new();
        popup.show(
            "Tab".to_string(),
            vec![('n', "new".to_string()), ('c', "close".to_string())],
        );

        assert!(popup.visible);
        assert_eq!(popup.group_label, "Tab");
        assert_eq!(popup.entries.len(), 2);

        popup.hide();
        assert!(!popup.visible);
        assert!(popup.entries.is_empty());
    }

    #[test]
    fn test_render_hidden_returns_empty() {
        let popup = WhichKeyPopup::new();
        let theme = Theme::default();
        let commands = popup.render(80, 24, &theme, WhichKeyPosition::Anchored);
        assert!(commands.is_empty());
    }

    #[test]
    fn test_render_visible_returns_commands() {
        let mut popup = WhichKeyPopup::new();
        popup.show(
            "Tab".to_string(),
            vec![
                ('n', "new".to_string()),
                ('c', "close".to_string()),
                ('r', "rename".to_string()),
            ],
        );

        let theme = Theme::default();
        let commands = popup.render(80, 24, &theme, WhichKeyPosition::Anchored);
        assert!(!commands.is_empty());
    }

    #[test]
    fn test_render_too_small_screen_returns_empty() {
        let mut popup = WhichKeyPopup::new();
        popup.show(
            "Tab".to_string(),
            vec![('n', "new".to_string()), ('c', "close".to_string())],
        );

        let theme = Theme::default();
        // Screen too small to fit popup.
        let commands = popup.render(5, 3, &theme, WhichKeyPosition::Anchored);
        assert!(commands.is_empty());
    }

    #[test]
    fn test_default_is_hidden() {
        let popup = WhichKeyPopup::default();
        assert!(!popup.visible);
    }

    /// Assert every draw command sits within the given screen bounds. `x` plus
    /// the char-length of the text must not exceed `cols`, and `y` must be
    /// within `rows`.
    fn assert_within_bounds(commands: &[DrawCommand], cols: u16, rows: u16) {
        for cmd in commands {
            assert!(cmd.y < rows, "y={} out of rows={}", cmd.y, rows);
            let end_x = cmd.x as usize + cmd.text.chars().count();
            assert!(
                end_x <= cols as usize,
                "x={} + len={} exceeds cols={}",
                cmd.x,
                cmd.text.chars().count(),
                cols
            );
        }
    }

    fn sample_popup() -> WhichKeyPopup {
        let mut popup = WhichKeyPopup::new();
        popup.show(
            "Tab".to_string(),
            vec![
                ('n', "new".to_string()),
                ('c', "close".to_string()),
                ('r', "rename".to_string()),
                ('p', "prev".to_string()),
                ('x', "next".to_string()),
            ],
        );
        popup
    }

    #[test]
    fn test_render_anchored_within_bounds() {
        let popup = sample_popup();
        let theme = Theme::default();
        let (cols, rows) = (80u16, 24u16);
        let commands = popup.render(cols, rows, &theme, WhichKeyPosition::Anchored);
        assert!(!commands.is_empty());
        assert_within_bounds(&commands, cols, rows);
    }

    #[test]
    fn test_render_centered_is_offset_and_within_bounds() {
        let popup = sample_popup();
        let theme = Theme::default();
        let (cols, rows) = (80u16, 24u16);
        let anchored = popup.render(cols, rows, &theme, WhichKeyPosition::Anchored);
        let centered = popup.render(cols, rows, &theme, WhichKeyPosition::Centered);
        assert!(!centered.is_empty());
        assert_within_bounds(&centered, cols, rows);
        // Centered should be vertically offset upward relative to the
        // bottom-anchored layout (its top border sits at a smaller y).
        assert!(
            centered[0].y < anchored[0].y,
            "centered top y={} should be above anchored top y={}",
            centered[0].y,
            anchored[0].y
        );
    }

    #[test]
    fn test_render_full_width_spans_width_and_within_bounds() {
        let popup = sample_popup();
        let theme = Theme::default();
        let (cols, rows) = (80u16, 24u16);
        let commands = popup.render(cols, rows, &theme, WhichKeyPosition::FullWidth);
        assert!(!commands.is_empty());
        assert_within_bounds(&commands, cols, rows);
        // At least one band row spans the full terminal width.
        assert!(
            commands
                .iter()
                .any(|c| c.x == 0 && c.text.chars().count() == cols as usize),
            "expected a full-width background band row"
        );
    }

    #[test]
    fn test_render_full_width_narrow_screen_within_bounds() {
        let popup = sample_popup();
        let theme = Theme::default();
        // Very narrow screen: cell width forces a single column.
        let (cols, rows) = (10u16, 24u16);
        let commands = popup.render(cols, rows, &theme, WhichKeyPosition::FullWidth);
        assert!(!commands.is_empty());
        assert_within_bounds(&commands, cols, rows);
    }
}
