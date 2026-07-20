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
    /// Global Alt shortcuts to display as a secondary section, as
    /// `(notation, label)` pairs (e.g., `("Alt-h", "focus left")`). Only
    /// populated on the root/main which-key page.
    pub shortcuts: Vec<(String, String)>,
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
            shortcuts: Vec::new(),
        }
    }

    /// Show the popup with the given group label, entries, and (optionally) the
    /// global Alt shortcuts section.
    pub fn show(
        &mut self,
        label: String,
        entries: Vec<(char, String)>,
        shortcuts: Vec<(String, String)>,
    ) {
        self.visible = true;
        self.group_label = label;
        self.entries = entries;
        self.shortcuts = shortcuts;
    }

    /// Hide the popup.
    pub fn hide(&mut self) {
        self.visible = false;
        self.entries.clear();
        self.group_label.clear();
        self.shortcuts.clear();
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
        let col_width: u16 = 20; // each column is 20 chars wide
        let popup_width = col_width * 2 + 2; // 2 columns + left/right borders
        let inner_width = (popup_width - 2) as usize;

        let entry_rows = self.entries.len().div_ceil(2);
        let has_shortcuts = !self.shortcuts.is_empty();
        let shortcut_rows_full = self.shortcuts.len().div_ceil(2);

        // The box must fit horizontally, and the entry rows must fit vertically
        // within the borders. If even the entries don't fit, draw nothing (this
        // preserves the historical "too small -> empty" behaviour).
        let border: u16 = 2;
        let max_inner = screen_rows.saturating_sub(border);
        if popup_width > screen_cols || max_inner == 0 || entry_rows as u16 > max_inner {
            return Vec::new();
        }

        // Decide how many shortcut rows fit below the entries. If they overflow,
        // show as many as fit and replace the last visible row with an ellipsis.
        let mut show_sep = false;
        let mut shortcut_rows = 0usize;
        let mut truncated = false;
        if has_shortcuts {
            let remaining = (max_inner - entry_rows as u16) as usize;
            // Need at least a separator row plus one shortcut row.
            if remaining >= 2 {
                show_sep = true;
                let room = remaining - 1;
                if shortcut_rows_full <= room {
                    shortcut_rows = shortcut_rows_full;
                } else {
                    shortcut_rows = room;
                    truncated = true;
                }
            }
        }

        let inner = entry_rows + usize::from(show_sep) + shortcut_rows;
        let popup_height = inner as u16 + 2;

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
        let top_border = format!("\u{256D}{}\u{256E}", "\u{2500}".repeat(inner_width));
        commands.push(DrawCommand {
            x: start_x,
            y: start_y,
            text: top_border,
            fg,
            bg,
        });

        // Entry rows.
        for row in 0..entry_rows {
            let left_entry = self.entries.get(row * 2);
            let right_entry = self.entries.get(row * 2 + 1);
            let y = start_y + 1 + row as u16;

            let mut row_text = String::from("\u{2502}");
            row_text.push_str(&entry_cell(left_entry, col_width));
            row_text.push_str(&entry_cell(right_entry, col_width));
            row_text.push('\u{2502}');
            commands.push(DrawCommand {
                x: start_x,
                y,
                text: row_text,
                fg,
                bg,
            });

            // Separate draw commands for the key chars, for highlight color.
            if let Some((key, _)) = left_entry {
                commands.push(DrawCommand {
                    x: start_x + 2,
                    y,
                    text: key.to_string(),
                    fg: key_fg,
                    bg,
                });
            }
            if let Some((key, _)) = right_entry {
                commands.push(DrawCommand {
                    x: start_x + 2 + col_width,
                    y,
                    text: key.to_string(),
                    fg: key_fg,
                    bg,
                });
            }
        }

        // Alt shortcuts section: separator subheading + shortcut rows.
        if show_sep {
            let sep_y = start_y + 1 + entry_rows as u16;
            commands.push(DrawCommand {
                x: start_x,
                y: sep_y,
                text: separator_line(" Alt ", inner_width),
                fg,
                bg,
            });

            for row in 0..shortcut_rows {
                let y = sep_y + 1 + row as u16;

                // Last visible row is an ellipsis when the list was truncated.
                if truncated && row + 1 == shortcut_rows {
                    commands.push(DrawCommand {
                        x: start_x,
                        y,
                        text: format!(
                            "\u{2502}{:^width$}\u{2502}",
                            "\u{2026}",
                            width = inner_width
                        ),
                        fg,
                        bg,
                    });
                    continue;
                }

                let left = self.shortcuts.get(row * 2);
                let right = self.shortcuts.get(row * 2 + 1);

                let mut row_text = String::from("\u{2502}");
                row_text.push_str(&shortcut_cell(left, col_width));
                row_text.push_str(&shortcut_cell(right, col_width));
                row_text.push('\u{2502}');
                commands.push(DrawCommand {
                    x: start_x,
                    y,
                    text: row_text,
                    fg,
                    bg,
                });

                // Highlight the key notation (drawn after the leading space).
                if let Some((notation, _)) = left {
                    push_notation_highlight(
                        &mut commands,
                        start_x + 2,
                        y,
                        notation,
                        col_width,
                        key_fg,
                        bg,
                    );
                }
                if let Some((notation, _)) = right {
                    push_notation_highlight(
                        &mut commands,
                        start_x + 2 + col_width,
                        y,
                        notation,
                        col_width,
                        key_fg,
                        bg,
                    );
                }
            }
        }

        // Bottom border.
        let bottom_border = format!("\u{2570}{}\u{256F}", "\u{2500}".repeat(inner_width));
        commands.push(DrawCommand {
            x: start_x,
            y: start_y + 1 + inner as u16,
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
        let entry_rows = (self.entries.len() as u16).div_ceil(cols_per_row);
        let has_shortcuts = !self.shortcuts.is_empty();
        let shortcut_rows_full = (self.shortcuts.len() as u16).div_ceil(cols_per_row);

        // Base band: label header row plus the entry rows. Reserve the last
        // screen row for the status bar; the band sits above it.
        let base = entry_rows + 1;
        if base + 1 > screen_rows {
            return Vec::new();
        }

        // Fit the Alt shortcuts below the entries: a small "Alt:" label row
        // plus as many shortcut rows as fit, truncating with an ellipsis row.
        let mut alt_label = false;
        let mut shortcut_rows = 0u16;
        let mut truncated = false;
        if has_shortcuts {
            // Rows available for the shortcut section within the band region.
            let remaining = screen_rows.saturating_sub(1).saturating_sub(base);
            if remaining >= 2 {
                alt_label = true;
                let room = remaining - 1;
                if shortcut_rows_full <= room {
                    shortcut_rows = shortcut_rows_full;
                } else {
                    shortcut_rows = room;
                    truncated = true;
                }
            }
        }

        let band_height = base + u16::from(alt_label) + shortcut_rows;
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

        // Alt shortcuts section.
        if alt_label {
            let alt_y = start_y + base;
            let alt_header = " Alt:"
                .chars()
                .take(screen_cols as usize)
                .collect::<String>();
            commands.push(DrawCommand {
                x: 0,
                y: alt_y,
                text: alt_header,
                fg: key_fg,
                bg,
            });

            let last_row = shortcut_rows.saturating_sub(1);
            for (i, (notation, label)) in self.shortcuts.iter().enumerate() {
                let col = (i as u16) % cols_per_row;
                let row = (i as u16) / cols_per_row;
                if row >= shortcut_rows {
                    break;
                }
                let y = alt_y + 1 + row;

                // The final row becomes an ellipsis when truncated.
                if truncated && row == last_row {
                    commands.push(DrawCommand {
                        x: col * cell_width,
                        y,
                        text: "\u{2026}".to_string(),
                        fg,
                        bg,
                    });
                    continue;
                }

                let x = col * cell_width;
                let avail = (screen_cols - x) as usize;
                let take = (cell_width as usize).min(avail);
                let cell_str = format!(" {} {}", notation, label);
                let cell_str = cell_str.chars().take(take).collect::<String>();
                commands.push(DrawCommand {
                    x,
                    y,
                    text: cell_str,
                    fg,
                    bg,
                });

                // Highlight the notation (after the leading space), clipped to
                // the cell so it never spills past the right edge.
                if take >= 2 {
                    let notation_room = take - 1;
                    let hl: String = notation.chars().take(notation_room).collect();
                    if !hl.is_empty() {
                        commands.push(DrawCommand {
                            x: x + 1,
                            y,
                            text: hl,
                            fg: key_fg,
                            bg,
                        });
                    }
                }
            }
        }

        commands
    }
}

/// Build a two-column entry cell (single-char key) padded to `col_width`.
fn entry_cell(entry: Option<&(char, String)>, col_width: u16) -> String {
    let w = col_width as usize;
    match entry {
        Some((key, label)) => {
            let s = format!(" {} {}", key, label);
            let clipped: String = s.chars().take(w).collect();
            format!("{clipped:<w$}")
        }
        None => " ".repeat(w),
    }
}

/// Build a shortcut cell (`" <notation> <label>"`) clipped and padded to
/// `col_width`.
fn shortcut_cell(entry: Option<&(String, String)>, col_width: u16) -> String {
    let w = col_width as usize;
    match entry {
        Some((notation, label)) => {
            let s = format!(" {notation} {label}");
            let clipped: String = s.chars().take(w).collect();
            format!("{clipped:<w$}")
        }
        None => " ".repeat(w),
    }
}

/// Build a bordered separator/subheading line, e.g. `│──── Alt ────│`, sized to
/// `inner_width` (the box width excluding the two border columns).
fn separator_line(title: &str, inner_width: usize) -> String {
    let title_len = title.chars().count();
    let dashes = inner_width.saturating_sub(title_len);
    let left = dashes / 2;
    let right = dashes - left;
    format!(
        "\u{2502}{}{}{}\u{2502}",
        "\u{2500}".repeat(left),
        title,
        "\u{2500}".repeat(right)
    )
}

/// Push a highlight draw command for a multi-char key notation, clipped so it
/// stays within the cell (which is `col_width` wide, minus the leading space).
fn push_notation_highlight(
    commands: &mut Vec<DrawCommand>,
    x: u16,
    y: u16,
    notation: &str,
    col_width: u16,
    fg: Color,
    bg: Color,
) {
    let max = (col_width as usize).saturating_sub(1);
    let text: String = notation.chars().take(max).collect();
    if !text.is_empty() {
        commands.push(DrawCommand { x, y, text, fg, bg });
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
            vec![("Alt-h".to_string(), "focus left".to_string())],
        );

        assert!(popup.visible);
        assert_eq!(popup.group_label, "Tab");
        assert_eq!(popup.entries.len(), 2);
        assert_eq!(popup.shortcuts.len(), 1);

        popup.hide();
        assert!(!popup.visible);
        assert!(popup.entries.is_empty());
        assert!(popup.shortcuts.is_empty());
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
        );
        popup
    }

    /// A popup with both prefix entries and a full set of Alt shortcuts,
    /// mimicking the root/main which-key page.
    fn sample_popup_with_shortcuts() -> WhichKeyPopup {
        let mut popup = WhichKeyPopup::new();
        let entries = vec![
            ('n', "new".to_string()),
            ('c', "close".to_string()),
            ('r', "rename".to_string()),
            ('p', "prev".to_string()),
            ('x', "next".to_string()),
        ];
        let shortcuts = vec![
            ("Alt-h".to_string(), "focus left".to_string()),
            ("Alt-j".to_string(), "focus down".to_string()),
            ("Alt-k".to_string(), "focus up".to_string()),
            ("Alt-l".to_string(), "focus right".to_string()),
            ("Alt-.".to_string(), "next tab".to_string()),
            ("Alt-,".to_string(), "prev tab".to_string()),
        ];
        popup.show("Remux".to_string(), entries, shortcuts);
        popup
    }

    /// Whether any draw command's text contains `needle`.
    fn any_text_contains(commands: &[DrawCommand], needle: &str) -> bool {
        commands.iter().any(|c| c.text.contains(needle))
    }

    #[test]
    fn test_render_anchored_with_shortcuts_emits_alt_rows() {
        let popup = sample_popup_with_shortcuts();
        let theme = Theme::default();
        let (cols, rows) = (80u16, 24u16);
        let commands = popup.render(cols, rows, &theme, WhichKeyPosition::Anchored);
        assert!(!commands.is_empty());
        assert_within_bounds(&commands, cols, rows);
        // The Alt separator and at least one shortcut row must be present.
        assert!(
            any_text_contains(&commands, "Alt"),
            "expected an Alt heading"
        );
        assert!(
            any_text_contains(&commands, "focus left"),
            "expected a shortcut label in the output"
        );
    }

    #[test]
    fn test_render_centered_with_shortcuts_within_bounds() {
        let popup = sample_popup_with_shortcuts();
        let theme = Theme::default();
        let (cols, rows) = (80u16, 24u16);
        let commands = popup.render(cols, rows, &theme, WhichKeyPosition::Centered);
        assert!(!commands.is_empty());
        assert_within_bounds(&commands, cols, rows);
        assert!(any_text_contains(&commands, "focus left"));
    }

    #[test]
    fn test_render_full_width_with_shortcuts_emits_alt_rows() {
        let popup = sample_popup_with_shortcuts();
        let theme = Theme::default();
        let (cols, rows) = (80u16, 24u16);
        let commands = popup.render(cols, rows, &theme, WhichKeyPosition::FullWidth);
        assert!(!commands.is_empty());
        assert_within_bounds(&commands, cols, rows);
        assert!(
            any_text_contains(&commands, "Alt:"),
            "expected an Alt: header"
        );
        assert!(any_text_contains(&commands, "focus left"));
    }

    /// Build a popup with many shortcuts to force the truncation path.
    fn sample_popup_many_shortcuts() -> WhichKeyPopup {
        let mut popup = WhichKeyPopup::new();
        let entries = vec![('n', "new".to_string()), ('c', "close".to_string())];
        let shortcuts: Vec<(String, String)> = (0..30)
            .map(|i| (format!("Alt-{i}"), format!("action {i}")))
            .collect();
        popup.show("Remux".to_string(), entries, shortcuts);
        popup
    }

    #[test]
    fn test_render_anchored_truncates_with_ellipsis_on_short_screen() {
        let popup = sample_popup_many_shortcuts();
        let theme = Theme::default();
        // Entries fit (1 row) but 30 shortcuts (15 rows) overflow 12 rows.
        let (cols, rows) = (80u16, 12u16);
        let commands = popup.render(cols, rows, &theme, WhichKeyPosition::Anchored);
        assert!(!commands.is_empty());
        assert_within_bounds(&commands, cols, rows);
        assert!(
            any_text_contains(&commands, "\u{2026}"),
            "expected an ellipsis row when shortcuts overflow"
        );
    }

    #[test]
    fn test_render_centered_truncates_with_ellipsis_on_short_screen() {
        let popup = sample_popup_many_shortcuts();
        let theme = Theme::default();
        let (cols, rows) = (80u16, 12u16);
        let commands = popup.render(cols, rows, &theme, WhichKeyPosition::Centered);
        assert!(!commands.is_empty());
        assert_within_bounds(&commands, cols, rows);
        assert!(any_text_contains(&commands, "\u{2026}"));
    }

    #[test]
    fn test_render_full_width_truncates_with_ellipsis_on_short_screen() {
        let popup = sample_popup_many_shortcuts();
        let theme = Theme::default();
        // Narrow + short: one column, so 30 shortcuts overflow the few rows.
        let (cols, rows) = (24u16, 10u16);
        let commands = popup.render(cols, rows, &theme, WhichKeyPosition::FullWidth);
        assert!(!commands.is_empty());
        assert_within_bounds(&commands, cols, rows);
        assert!(any_text_contains(&commands, "\u{2026}"));
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
