//! Which-key style key hint popup.
//!
//! This module renders a popup showing available keybindings when the user
//! is partway through a multi-key sequence in Command mode.

use crossterm::style::Color;

use crate::config::theme::Theme;

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

    /// Render the popup into a list of draw commands.
    ///
    /// The popup is drawn as a bordered box centered horizontally at the
    /// bottom of the screen, with entries in a two-column layout.
    pub fn render(&self, screen_cols: u16, screen_rows: u16, theme: &Theme) -> Vec<DrawCommand> {
        if !self.visible || self.entries.is_empty() {
            return Vec::new();
        }

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

        // Position: centered horizontally, anchored to bottom.
        let start_x = (screen_cols.saturating_sub(popup_width)) / 2;
        let start_y = screen_rows.saturating_sub(popup_height);

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
        let commands = popup.render(80, 24, &theme);
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
        let commands = popup.render(80, 24, &theme);
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
        let commands = popup.render(5, 3, &theme);
        assert!(commands.is_empty());
    }

    #[test]
    fn test_default_is_hidden() {
        let popup = WhichKeyPopup::default();
        assert!(!popup.visible);
    }
}
