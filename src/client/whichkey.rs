//! Which-key style key hint popup.
//!
//! This module renders a popup showing available keybindings when the user
//! is partway through a multi-key sequence in Command mode.

use crossterm::style::Color;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::theme::Theme;
use crate::config::WhichKeyPosition;
use crate::server::compositor::{
    box_bottom_line, box_top_line, sharp_box_bottom_line, sharp_box_top_line, BOX_HORIZONTAL,
    BOX_VERTICAL,
};

/// Narrowest cell in the full-width layout, so short labels keep the grid
/// spacing they always had.
const FULL_WIDTH_MIN_CELL: usize = 22;

/// Narrowest column in the bordered box, for the same reason.
const BOX_MIN_COL: usize = 20;

/// Narrowest truncated cell worth drawing: the leading space, one character
/// and the ellipsis, plus the trailing gap.
const MIN_TRUNCATED_CELL: usize = 4;

/// How a section's cells are laid out: `count` columns, each `width` wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Columns {
    count: usize,
    width: usize,
}

/// The cell width that holds every cell in `texts` whole, plus the one-column
/// gap that keeps a label from running into the next column or the border.
///
/// Every width in this module is in terminal COLUMNS, not characters: a CJK
/// ideograph or an emoji is one character and two columns, so sizing by
/// character count let such a label overrun its column.
fn natural_width<'a>(texts: impl IntoIterator<Item = &'a str>, min: usize) -> usize {
    texts
        .into_iter()
        .map(|t| t.width() + 1)
        .fold(min, usize::max)
}

/// The longest prefix of `text` that fits in `cols` columns. A wide character
/// that would straddle the limit is dropped whole, never split.
fn prefix_within(text: &str, cols: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= cols
        })
        .collect()
}

/// Choose the box's columns. Two columns at the natural width are preferred;
/// a terminal too narrow for them drops to one column, and only a terminal too
/// narrow for one whole column truncates. `entry_count` rows must fit within
/// `max_rows` at the chosen column count.
fn box_columns(
    natural: usize,
    entry_count: usize,
    screen_cols: u16,
    max_rows: usize,
) -> Option<Columns> {
    let avail = (screen_cols as usize).saturating_sub(2);
    let rows_fit = |count: usize| entry_count.div_ceil(count) <= max_rows;
    for count in [2, 1] {
        if count * natural <= avail && rows_fit(count) {
            return Some(Columns {
                count,
                width: natural,
            });
        }
    }
    [1, 2]
        .into_iter()
        .map(|count| Columns {
            count,
            width: avail / count,
        })
        .find(|c| c.width >= MIN_TRUNCATED_CELL && rows_fit(c.count))
}

/// Choose the full-width panel's columns: as many `natural`-wide columns as
/// fit in `inner_cols`, or one truncated column when not even one fits.
fn full_width_columns(natural: usize, inner_cols: usize) -> Columns {
    if natural <= inner_cols {
        Columns {
            count: inner_cols / natural,
            width: natural,
        }
    } else {
        Columns {
            count: 1,
            width: inner_cols,
        }
    }
}

/// How many leading columns of a cell text `text_cols` wide survive in a cell
/// `width` wide. The last column of every cell is the gap, so text that needs
/// it is truncated and the column before the gap holds the ellipsis.
fn kept_cols(text_cols: usize, width: usize) -> usize {
    let budget = width.saturating_sub(1);
    if text_cols <= budget {
        text_cols
    } else {
        budget.saturating_sub(1)
    }
}

/// Whether a cell that keeps `kept` columns still shows the whole key, which
/// is drawn after the cell's one leading space.
fn key_survives(kept: usize, key: char) -> bool {
    kept > key.width().unwrap_or(1)
}

/// `text` fitted to exactly `width` columns: padded when it fits, otherwise cut
/// and ended with an ellipsis so the reader can see it was cut.
fn fit_cell(text: &str, width: usize) -> String {
    let text_cols = text.width();
    let kept = kept_cols(text_cols, width);
    let mut out = prefix_within(text, kept);
    if kept < text_cols && width >= 2 {
        out.push('\u{2026}');
    }
    let pad = width.saturating_sub(out.width());
    out.extend(std::iter::repeat_n(' ', pad));
    out
}

fn box_entry_text(key: char, label: &str) -> String {
    format!(" {key} {label}")
}

fn full_width_entry_text(key: char, label: &str) -> String {
    format!(" {key} \u{2192} {label}")
}

fn shortcut_text(notation: &str, label: &str) -> String {
    format!(" {notation} {label}")
}

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

    /// Render the bordered box of one or two columns. When `centered` is false the box is
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

        // Both sections share the box, so its columns are as wide as the
        // widest cell of either section.
        let texts: Vec<String> = self
            .entries
            .iter()
            .map(|(k, l)| box_entry_text(*k, l))
            .chain(self.shortcuts.iter().map(|(n, l)| shortcut_text(n, l)))
            .collect();
        let natural = natural_width(texts.iter().map(String::as_str), BOX_MIN_COL);

        // If even the entries cannot be placed, draw nothing (the historical
        // "too small -> empty" behaviour).
        let border: u16 = 2;
        let max_inner = screen_rows.saturating_sub(border);
        let Some(columns) =
            box_columns(natural, self.entries.len(), screen_cols, max_inner as usize)
        else {
            return Vec::new();
        };
        let col_width = columns.width as u16;
        let inner_width = columns.count * columns.width;
        let popup_width = inner_width as u16 + 2;

        let entry_rows = self.entries.len().div_ceil(columns.count);
        let has_shortcuts = !self.shortcuts.is_empty();
        let shortcut_rows_full = self.shortcuts.len().div_ceil(columns.count);

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
        let top_border = box_top_line(inner_width);
        commands.push(DrawCommand {
            x: start_x,
            y: start_y,
            text: top_border,
            fg,
            bg,
        });

        // Entry rows.
        for row in 0..entry_rows {
            let y = start_y + 1 + row as u16;
            let cells: Vec<Option<&(char, String)>> = (0..columns.count)
                .map(|c| self.entries.get(row * columns.count + c))
                .collect();

            let mut row_text = BOX_VERTICAL.to_string();
            for entry in &cells {
                let text = entry.map_or_else(String::new, |(k, l)| box_entry_text(*k, l));
                row_text.push_str(&fit_cell(&text, columns.width));
            }
            row_text.push(BOX_VERTICAL);
            commands.push(DrawCommand {
                x: start_x,
                y,
                text: row_text,
                fg,
                bg,
            });

            // Separate draw commands for the key chars, for highlight color.
            for (c, entry) in cells.iter().enumerate() {
                if let Some((key, label)) = entry {
                    let text_cols = box_entry_text(*key, label).width();
                    if key_survives(kept_cols(text_cols, columns.width), *key) {
                        commands.push(DrawCommand {
                            x: start_x + 2 + c as u16 * col_width,
                            y,
                            text: key.to_string(),
                            fg: key_fg,
                            bg,
                        });
                    }
                }
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
                            "{BOX_VERTICAL}{:^width$}{BOX_VERTICAL}",
                            "\u{2026}",
                            width = inner_width
                        ),
                        fg,
                        bg,
                    });
                    continue;
                }

                let cells: Vec<Option<&(String, String)>> = (0..columns.count)
                    .map(|c| self.shortcuts.get(row * columns.count + c))
                    .collect();

                let mut row_text = BOX_VERTICAL.to_string();
                for entry in &cells {
                    let text = entry.map_or_else(String::new, |(n, l)| shortcut_text(n, l));
                    row_text.push_str(&fit_cell(&text, columns.width));
                }
                row_text.push(BOX_VERTICAL);
                commands.push(DrawCommand {
                    x: start_x,
                    y,
                    text: row_text,
                    fg,
                    bg,
                });

                // Highlight the key notation (drawn after the leading space).
                for (c, entry) in cells.iter().enumerate() {
                    if let Some((notation, label)) = entry {
                        let text_cols = shortcut_text(notation, label).width();
                        push_notation_highlight(
                            &mut commands,
                            start_x + 1 + c as u16 * col_width,
                            y,
                            notation,
                            kept_cols(text_cols, columns.width),
                            key_fg,
                            bg,
                        );
                    }
                }
            }
        }

        // Bottom border.
        let bottom_border = box_bottom_line(inner_width);
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
    /// additional rows as needed. The group label occupies the top content row.
    ///
    /// The whole band is enclosed in a light box (`\u{250C}\u{2500}\u{2510}`
    /// top, `\u{2502}` side edges, `\u{2514}\u{2500}\u{2518}` bottom) drawn in
    /// the theme frame color, so it is visually separated from the terminal
    /// content above and the status bar below. The box borders occupy two extra
    /// rows and two extra columns, which are reserved in the layout so nothing
    /// is clipped and the band still sits above the status-bar row.
    fn render_full_width(
        &self,
        screen_cols: u16,
        screen_rows: u16,
        theme: &Theme,
    ) -> Vec<DrawCommand> {
        // The box needs a left edge, at least one content column, and a right
        // edge. Anything narrower cannot be framed, so bail gracefully.
        if screen_cols < 3 {
            return Vec::new();
        }

        let fg = theme.whichkey_fg;
        let bg = theme.whichkey_bg;
        let key_fg = theme.whichkey_key_fg;

        // Content region sits inside the left/right border columns.
        let inner_cols = screen_cols - 2;
        // Both sections share one grid, sized to the widest cell of either.
        let texts: Vec<String> = self
            .entries
            .iter()
            .map(|(k, l)| full_width_entry_text(*k, l))
            .chain(self.shortcuts.iter().map(|(n, l)| shortcut_text(n, l)))
            .collect();
        let natural = natural_width(texts.iter().map(String::as_str), FULL_WIDTH_MIN_CELL);
        let columns = full_width_columns(natural, inner_cols as usize);
        let cell_width = columns.width as u16;
        // Content starts at column 1 (after the left border); a cell in column
        // `col` starts here.
        let content_x = |col: u16| -> u16 { 1 + col * cell_width };
        // Rightmost content column is `screen_cols - 2`; column `screen_cols - 1`
        // holds the right border. `avail` for a cell at `x` is the number of
        // content columns from `x` up to (but excluding) the right border.
        let avail_at = |x: u16| -> usize { (screen_cols - 1 - x) as usize };

        let cols_per_row = columns.count as u16;
        let entry_rows = (self.entries.len() as u16).div_ceil(cols_per_row);
        let has_shortcuts = !self.shortcuts.is_empty();
        let shortcut_rows_full = (self.shortcuts.len() as u16).div_ceil(cols_per_row);

        // Base content: label header row plus the entry rows. The full band also
        // needs a top and bottom border row (2), and the last screen row is
        // reserved for the status bar (1). Bail if even the minimal framed band
        // does not fit above the status bar.
        let base = entry_rows + 1;
        if base + 3 > screen_rows {
            return Vec::new();
        }

        // Fit the Alt shortcuts below the entries: a small "Alt:" label row
        // plus as many shortcut rows as fit, truncating with an ellipsis row.
        // Rows available for content = screen_rows - status(1) - borders(2) - base.
        let mut alt_label = false;
        let mut shortcut_rows = 0u16;
        let mut truncated = false;
        if has_shortcuts {
            let remaining = screen_rows.saturating_sub(3).saturating_sub(base);
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

        let content_rows = base + u16::from(alt_label) + shortcut_rows;
        let band_height = content_rows + 2; // + top/bottom border rows
        let start_y = screen_rows - 1 - band_height;
        // First content row (just below the top border).
        let content_y0 = start_y + 1;

        let mut commands = Vec::new();

        // Top border row: \u{250C}\u{2500}...\u{2500}\u{2510} spanning full width.
        commands.push(DrawCommand {
            x: 0,
            y: start_y,
            text: sharp_box_top_line(inner_cols as usize),
            fg,
            bg,
        });

        // Content rows: left/right edge chars with a blank interior. Content is
        // overlaid on top of this background afterwards.
        for row in 0..content_rows {
            commands.push(DrawCommand {
                x: 0,
                y: content_y0 + row,
                text: format!(
                    "{BOX_VERTICAL}{}{BOX_VERTICAL}",
                    " ".repeat(inner_cols as usize)
                ),
                fg,
                bg,
            });
        }

        // Bottom border row: \u{2514}\u{2500}...\u{2500}\u{2518}.
        commands.push(DrawCommand {
            x: 0,
            y: content_y0 + content_rows,
            text: sharp_box_bottom_line(inner_cols as usize),
            fg,
            bg,
        });

        // Label header on the first content row.
        let label_text = if self.group_label.is_empty() {
            " which-key ".to_string()
        } else {
            format!(" {} ", self.group_label)
        };
        let label_text = prefix_within(&label_text, inner_cols as usize);
        commands.push(DrawCommand {
            x: 1,
            y: content_y0,
            text: label_text,
            fg: key_fg,
            bg,
        });

        // Entry cells, flowing across columns then wrapping to new rows.
        for (i, (key, label)) in self.entries.iter().enumerate() {
            let col = (i as u16) % cols_per_row;
            let row = (i as u16) / cols_per_row;
            let x = content_x(col);
            let y = content_y0 + 1 + row;

            // Never draw past the right edge: cap the cell to the remaining
            // content width (matters on very narrow screens).
            let width = columns.width.min(avail_at(x));
            let entry_str = full_width_entry_text(*key, label);
            let kept = kept_cols(entry_str.width(), width);
            commands.push(DrawCommand {
                x,
                y,
                text: fit_cell(&entry_str, width),
                fg,
                bg,
            });

            // Highlight the key char (drawn after the leading space), unless
            // truncation cut it off.
            if key_survives(kept, *key) {
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
            let alt_y = content_y0 + base;
            let alt_header = " Alt:"
                .chars()
                .take(inner_cols as usize)
                .collect::<String>();
            commands.push(DrawCommand {
                x: 1,
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
                let x = content_x(col);

                // The final row becomes an ellipsis when truncated.
                if truncated && row == last_row {
                    commands.push(DrawCommand {
                        x,
                        y,
                        text: "\u{2026}".to_string(),
                        fg,
                        bg,
                    });
                    continue;
                }

                let width = columns.width.min(avail_at(x));
                let cell_str = shortcut_text(notation, label);
                let kept = kept_cols(cell_str.width(), width);
                commands.push(DrawCommand {
                    x,
                    y,
                    text: fit_cell(&cell_str, width),
                    fg,
                    bg,
                });

                push_notation_highlight(&mut commands, x, y, notation, kept, key_fg, bg);
            }
        }

        commands
    }
}

/// Build a bordered separator/subheading line, e.g. `│──── Alt ────│`, sized to
/// `inner_width` (the box width excluding the two border columns).
fn separator_line(title: &str, inner_width: usize) -> String {
    let title_len = title.width();
    let dashes = inner_width.saturating_sub(title_len);
    let left = dashes / 2;
    let right = dashes - left;
    let dash = BOX_HORIZONTAL.to_string();
    format!(
        "{BOX_VERTICAL}{}{title}{}{BOX_VERTICAL}",
        dash.repeat(left),
        dash.repeat(right)
    )
}

/// Push a highlight draw command for a multi-char key notation in the cell at
/// `cell_x`. Only the part of the notation among the cell's `kept` columns is
/// highlighted, so the colour never covers a truncation ellipsis.
fn push_notation_highlight(
    commands: &mut Vec<DrawCommand>,
    cell_x: u16,
    y: u16,
    notation: &str,
    kept: usize,
    fg: Color,
    bg: Color,
) {
    let text = prefix_within(notation, kept.saturating_sub(1));
    if !text.is_empty() {
        commands.push(DrawCommand {
            x: cell_x + 1,
            y,
            text,
            fg,
            bg,
        });
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

    /// Assert the full-width layout is framed by a light box: a top border row
    /// (`\u{250C}\u{2500}...\u{2510}`) and a bottom border row
    /// (`\u{2514}\u{2500}...\u{2518}`) each spanning the full width at `x == 0`,
    /// plus side-edge rows (`\u{2502}...\u{2502}`). Also checks the frame stays
    /// within bounds and above the status-bar row (`rows - 1`).
    fn assert_full_width_box(commands: &[DrawCommand], cols: u16, rows: u16) {
        let top = commands
            .iter()
            .find(|c| c.x == 0 && c.text.starts_with('\u{250C}') && c.text.ends_with('\u{2510}'))
            .expect("expected a top border row starting with \u{250C} and ending with \u{2510}");
        assert_eq!(
            top.text.chars().count(),
            cols as usize,
            "top border must span the full width"
        );

        let bottom = commands
            .iter()
            .find(|c| c.x == 0 && c.text.starts_with('\u{2514}') && c.text.ends_with('\u{2518}'))
            .expect("expected a bottom border row starting with \u{2514} and ending with \u{2518}");
        assert_eq!(
            bottom.text.chars().count(),
            cols as usize,
            "bottom border must span the full width"
        );

        // The bottom border must sit above the status-bar row (the last row).
        assert!(
            bottom.y < rows - 1,
            "bottom border y={} must be above the status-bar row {}",
            bottom.y,
            rows - 1
        );
        assert!(top.y < bottom.y, "top border must be above bottom border");

        // At least one content row carries side edges.
        assert!(
            commands.iter().any(|c| c.x == 0
                && c.text.starts_with('\u{2502}')
                && c.text.ends_with('\u{2502}')),
            "expected side-edge (\u{2502}) rows"
        );
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
        assert_full_width_box(&commands, cols, rows);
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
        assert_full_width_box(&commands, cols, rows);
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
        // At least one band row spans the full terminal width (the box borders
        // do, `\u{250C}\u{2500}...\u{2510}` / `\u{2514}\u{2500}...\u{2518}`).
        assert!(
            commands
                .iter()
                .any(|c| c.x == 0 && c.text.chars().count() == cols as usize),
            "expected a full-width band row"
        );
        assert_full_width_box(&commands, cols, rows);
    }

    /// The root page's two longest Alt labels, with the short ones around them.
    fn root_popup() -> WhichKeyPopup {
        let mut popup = WhichKeyPopup::new();
        let entries = vec![
            (':', "command palette".to_string()),
            ('a', "send prefix".to_string()),
            ('\u{2423}', "layout next".to_string()),
        ];
        let shortcuts = vec![
            ("Alt-L".to_string(), "move right".to_string()),
            ("Alt-Space".to_string(), "next layout".to_string()),
            ("Alt-a".to_string(), "switch agent".to_string()),
            ("Alt-s".to_string(), "switch session".to_string()),
            ("Alt-t".to_string(), "new tab".to_string()),
        ];
        popup.show("Remux".to_string(), entries, shortcuts);
        popup
    }

    /// Whether `needle` is drawn whole and followed by a gap, a border, or
    /// the end of its draw command, rather than run into the next column.
    fn drawn_in_full(commands: &[DrawCommand], needle: &str) -> bool {
        commands.iter().any(|c| {
            c.text.match_indices(needle).any(|(i, _)| {
                matches!(
                    c.text[i + needle.len()..].chars().next(),
                    None | Some(' ') | Some(BOX_VERTICAL)
                )
            })
        })
    }

    #[test]
    fn natural_width_holds_the_widest_cell_and_a_gap() {
        assert_eq!(natural_width(["ab", "abcd"], 0), 5);
        assert_eq!(natural_width(["ab"], 20), 20);
        assert_eq!(
            natural_width([" \u{2423} \u{2192} x"], 0),
            7,
            "counts chars"
        );
    }

    #[test]
    fn box_columns_prefers_two_then_one_then_truncates() {
        assert_eq!(
            box_columns(23, 15, 80, 40),
            Some(Columns {
                count: 2,
                width: 23
            })
        );
        assert_eq!(
            box_columns(23, 15, 40, 40),
            Some(Columns {
                count: 1,
                width: 23
            })
        );
        assert_eq!(
            box_columns(23, 15, 20, 40),
            Some(Columns {
                count: 1,
                width: 18
            })
        );
        // Two columns only fit vertically: truncate them rather than blank.
        assert_eq!(
            box_columns(23, 15, 40, 8),
            Some(Columns {
                count: 2,
                width: 19
            })
        );
        assert_eq!(box_columns(23, 2, 5, 1), None);
    }

    #[test]
    fn full_width_columns_fill_the_row_then_truncate_one() {
        assert_eq!(
            full_width_columns(23, 118),
            Columns {
                count: 5,
                width: 23
            }
        );
        assert_eq!(
            full_width_columns(23, 10),
            Columns {
                count: 1,
                width: 10
            }
        );
    }

    #[test]
    fn wide_glyph_labels_are_sized_padded_and_cut_by_columns() {
        use unicode_width::UnicodeWidthStr;
        // Six characters, eleven columns: five ideographs at two each.
        let label = " \u{8a2d}\u{5b9a}\u{3092}\u{958b}\u{304f}";
        assert_eq!(label.width(), 11);
        assert_eq!(natural_width([label], 0), 12);

        let whole = fit_cell(label, 14);
        assert_eq!(whole.width(), 14, "padded to the column width: {whole:?}");
        assert!(whole.starts_with(label));

        // Budget 8 columns, 7 for text before the ellipsis: three ideographs
        // and the leading space (7 columns), since a fourth would straddle it.
        let cut = fit_cell(label, 9);
        assert_eq!(cut, " \u{8a2d}\u{5b9a}\u{3092}\u{2026} ");
        assert_eq!(cut.width(), 9, "never wider than its cell: {cut:?}");
    }

    #[test]
    fn fit_cell_pads_or_cuts_with_an_ellipsis() {
        assert_eq!(fit_cell(" a b", 6), " a b  ");
        assert_eq!(fit_cell(" Alt-s switch session", 10), " Alt-s s\u{2026} ");
        assert_eq!(fit_cell(" Alt-s switch session", 10).chars().count(), 10);
    }

    #[test]
    fn every_position_draws_the_longest_labels_in_full() {
        let popup = root_popup();
        let theme = Theme::default();
        for position in [
            WhichKeyPosition::Anchored,
            WhichKeyPosition::Centered,
            WhichKeyPosition::FullWidth,
        ] {
            let commands = popup.render(80, 24, &theme, position.clone());
            assert_within_bounds(&commands, 80, 24);
            for label in ["Alt-s switch session", "Alt-Space next layout"] {
                assert!(
                    drawn_in_full(&commands, label),
                    "{position:?}: {label:?} is cut or runs into the next column"
                );
            }
        }
    }

    #[test]
    fn a_narrow_box_drops_to_one_column_before_truncating() {
        let popup = root_popup();
        let commands = popup.render(30, 24, &Theme::default(), WhichKeyPosition::Anchored);
        assert_within_bounds(&commands, 30, 24);
        assert!(drawn_in_full(&commands, "Alt-Space next layout"));
        assert!(!any_text_contains(&commands, "\u{2026}"));
    }

    #[test]
    fn a_box_too_narrow_for_a_label_ends_it_with_an_ellipsis() {
        let popup = root_popup();
        let theme = Theme::default();
        for position in [
            WhichKeyPosition::Anchored,
            WhichKeyPosition::Centered,
            WhichKeyPosition::FullWidth,
        ] {
            let commands = popup.render(16, 24, &theme, position.clone());
            assert_within_bounds(&commands, 16, 24);
            assert!(
                commands
                    .iter()
                    .any(
                        |c| c.text.split_once("Alt-s swi").is_some_and(|(_, rest)| rest
                            .contains('\u{2026}')
                            && !rest.contains("session"))
                    ),
                "{position:?}: {commands:#?}"
            );
            // The key colour stops before the ellipsis.
            let key_fg = theme.whichkey_key_fg;
            assert!(
                !commands
                    .iter()
                    .any(|c| c.fg == key_fg && c.text.contains('\u{2026}')),
                "{position:?}: a highlight covers the ellipsis"
            );
        }
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
        assert_full_width_box(&commands, cols, rows);
    }
}
