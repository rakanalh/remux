//! Command palette overlay.
//!
//! Provides a popup that lets the user type a command name with
//! tab-completion and execute it. Similar in spirit to the which-key popup
//! but with free-form text input and fuzzy filtering.

use crate::client::whichkey::DrawCommand;
use crate::config::theme::Theme;
use crate::protocol::command_names;

/// State for the command palette overlay.
#[derive(Debug, Clone)]
pub struct CommandPaletteState {
    /// The current text input buffer.
    input: String,
    /// Filtered list of matching command names.
    filtered: Vec<(String, Option<String>)>,
    /// Currently selected index in the filtered list.
    selected: usize,
    /// Whether the last action was a tab completion (for cycling).
    tab_cycling: bool,
    /// The input text at the point tab-cycling started.
    tab_base: String,
}

impl CommandPaletteState {
    /// Create a new command palette state with all commands visible.
    pub fn new() -> Self {
        let all = command_names()
            .into_iter()
            .map(|(name, hint)| (name.to_string(), hint.map(|s| s.to_string())))
            .collect();
        Self {
            input: String::new(),
            filtered: all,
            selected: 0,
            tab_cycling: false,
            tab_base: String::new(),
        }
    }

    /// Return the current input string. If a command is selected from the
    /// list, returns that command (possibly with arguments appended).
    pub fn current_input(&self) -> String {
        // If the user typed something, check if it contains a space (argument).
        let trimmed = self.input.trim();
        if trimmed.is_empty() {
            // Use the selected filtered entry.
            if let Some((name, _)) = self.filtered.get(self.selected) {
                return name.clone();
            }
            return String::new();
        }

        // If input contains a space, the first token is the command name and
        // the rest is the argument.
        if let Some(space_idx) = trimmed.find(' ') {
            let cmd_part = &trimmed[..space_idx];
            let arg_part = &trimmed[space_idx + 1..];
            // Try to match the command part against filtered list.
            // If exact match, use it with argument.
            let matched = self
                .filtered
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(cmd_part));
            if let Some((name, _)) = matched {
                if arg_part.is_empty() {
                    return name.clone();
                }
                return format!("{} {}", name, arg_part);
            }
            // Otherwise just return the raw input.
            return trimmed.to_string();
        }

        // No space: if there's an exact match in filtered, prefer it;
        // otherwise use the selected item if the user navigated with arrows.
        if let Some((name, _)) = self.filtered.get(self.selected) {
            // Check if the input exactly matches a command.
            let exact = self
                .filtered
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(trimmed));
            if let Some((n, _)) = exact {
                return n.clone();
            }
            return name.clone();
        }

        trimmed.to_string()
    }

    /// Insert a character at the end of the input buffer and re-filter.
    pub fn insert_char(&mut self, c: char) {
        self.input.push(c);
        self.tab_cycling = false;
        self.filter_commands();
    }

    /// Remove the last character and re-filter.
    pub fn backspace(&mut self) {
        self.input.pop();
        self.tab_cycling = false;
        self.filter_commands();
    }

    /// Move selection up.
    pub fn select_prev(&mut self) {
        if !self.filtered.is_empty() {
            if self.selected == 0 {
                self.selected = self.filtered.len() - 1;
            } else {
                self.selected -= 1;
            }
        }
        self.tab_cycling = false;
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1) % self.filtered.len();
        }
        self.tab_cycling = false;
    }

    /// Filter commands based on the current input text using case-insensitive
    /// substring matching. Only filters on the command name part (before the
    /// first space, if any).
    fn filter_commands(&mut self) {
        let query = self.command_part().to_ascii_lowercase();
        let all = command_names();

        if query.is_empty() {
            self.filtered = all
                .into_iter()
                .map(|(name, hint)| (name.to_string(), hint.map(|s| s.to_string())))
                .collect();
        } else {
            self.filtered = all
                .into_iter()
                .filter(|(name, _)| name.to_ascii_lowercase().contains(&query))
                .map(|(name, hint)| (name.to_string(), hint.map(|s| s.to_string())))
                .collect();
        }

        // Clamp selected index.
        if self.selected >= self.filtered.len() {
            self.selected = 0;
        }
    }

    /// Return the command-name part of the input (everything before the first
    /// space, or the whole input if there is no space).
    fn command_part(&self) -> &str {
        match self.input.find(' ') {
            Some(idx) => &self.input[..idx],
            None => &self.input,
        }
    }

    /// Tab completion.
    ///
    /// First press: complete to the longest common prefix of all filtered
    /// matches. Subsequent presses cycle through the matches.
    /// `reverse`: if true, cycle backward (Shift+Tab).
    pub fn tab_complete(&mut self, reverse: bool) {
        if self.filtered.is_empty() {
            return;
        }

        if self.tab_cycling {
            // Cycle through matches.
            if reverse {
                if self.selected == 0 {
                    self.selected = self.filtered.len() - 1;
                } else {
                    self.selected -= 1;
                }
            } else {
                self.selected = (self.selected + 1) % self.filtered.len();
            }
            // Update the input to the selected command name, preserving any argument.
            let arg_part = self.argument_part().map(|s| s.to_string());
            if let Some((name, _)) = self.filtered.get(self.selected) {
                self.input = match arg_part {
                    Some(arg) => format!("{} {}", name, arg),
                    None => name.clone(),
                };
            }
        } else {
            // First tab press: find longest common prefix.
            self.tab_base = self.input.clone();
            let lcp = self.longest_common_prefix();
            if !lcp.is_empty() && lcp.len() > self.command_part().len() {
                let arg_part = self.argument_part().map(|s| s.to_string());
                self.input = match arg_part {
                    Some(arg) => format!("{} {}", lcp, arg),
                    None => lcp,
                };
                self.filter_commands();
            }
            // If only one match, fill it completely.
            if self.filtered.len() == 1 {
                let arg_part = self.argument_part().map(|s| s.to_string());
                let name = self.filtered[0].0.clone();
                self.input = match arg_part {
                    Some(arg) => format!("{} {}", name, arg),
                    None => name,
                };
                self.selected = 0;
            }
            self.tab_cycling = true;
        }
    }

    /// Return the argument part of the input (everything after the first space).
    fn argument_part(&self) -> Option<&str> {
        self.input.find(' ').map(|idx| &self.input[idx + 1..])
    }

    /// Compute the longest common prefix of all filtered command names.
    fn longest_common_prefix(&self) -> String {
        if self.filtered.is_empty() {
            return String::new();
        }
        let first = &self.filtered[0].0;
        let mut prefix_len = first.len();
        for (name, _) in &self.filtered[1..] {
            prefix_len = first
                .chars()
                .zip(name.chars())
                .take(prefix_len)
                .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
                .count();
        }
        first[..first
            .char_indices()
            .nth(prefix_len)
            .map(|(i, _)| i)
            .unwrap_or(first.len())]
            .to_string()
    }

    /// Get the input text (for rendering).
    pub fn input_text(&self) -> &str {
        &self.input
    }

    /// Get the filtered command list (for rendering).
    pub fn filtered_commands(&self) -> &[(String, Option<String>)] {
        &self.filtered
    }

    /// Get the selected index (for rendering).
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Render the command palette into a list of draw commands.
    ///
    /// Layout: centered popup with a text input field at the top, filtered
    /// command list below. Uses whichkey theme colors.
    pub fn render(&self, screen_cols: u16, screen_rows: u16, theme: &Theme) -> Vec<DrawCommand> {
        let mut commands = Vec::new();

        let max_visible = 10usize;
        let visible_count = self.filtered.len().min(max_visible);
        // popup_height = 1 (top border) + 1 (input line) + 1 (separator) +
        //                visible_count (entries) + 1 (bottom border)
        let popup_height = 4 + visible_count as u16;
        let popup_width = 50u16.min(screen_cols.saturating_sub(4));

        if popup_width < 20 || popup_height > screen_rows {
            return commands;
        }

        let start_x = (screen_cols.saturating_sub(popup_width)) / 2;
        let start_y = (screen_rows.saturating_sub(popup_height)) / 2;

        let fg = theme.whichkey_fg;
        let bg = theme.whichkey_bg;
        let key_fg = theme.whichkey_key_fg;
        let inner_width = (popup_width - 2) as usize;

        // Top border with title.
        let title = " Command Palette ";
        let title_len = title.len();
        let fill = inner_width.saturating_sub(title_len);
        let left_fill = fill / 2;
        let right_fill = fill - left_fill;
        let top = format!(
            "\u{256D}{}{}{}\u{256E}",
            "\u{2500}".repeat(left_fill),
            title,
            "\u{2500}".repeat(right_fill),
        );
        commands.push(DrawCommand {
            x: start_x,
            y: start_y,
            text: top,
            fg,
            bg,
        });

        // Input line.
        let input_display = if self.input.len() > inner_width.saturating_sub(3) {
            let start = self.input.len() - (inner_width.saturating_sub(3));
            format!("> {}", &self.input[start..])
        } else {
            format!("> {}", &self.input)
        };
        let padded_input = format!(
            "\u{2502}{:<width$}\u{2502}",
            input_display,
            width = inner_width
        );
        commands.push(DrawCommand {
            x: start_x,
            y: start_y + 1,
            text: padded_input,
            fg: key_fg,
            bg,
        });

        // Separator.
        let sep = format!("\u{251C}{}\u{2524}", "\u{2500}".repeat(inner_width));
        commands.push(DrawCommand {
            x: start_x,
            y: start_y + 2,
            text: sep,
            fg,
            bg,
        });

        // Determine visible window for scrolling through entries.
        let total = self.filtered.len();
        let scroll_start = if total <= max_visible || self.selected < max_visible / 2 {
            0
        } else if self.selected >= total - max_visible / 2 {
            total - max_visible
        } else {
            self.selected - max_visible / 2
        };

        // Entry rows.
        for (vis_idx, idx) in (scroll_start..scroll_start + visible_count).enumerate() {
            let (name, hint) = &self.filtered[idx];
            let is_selected = idx == self.selected;

            let hint_str = hint
                .as_deref()
                .map(|h| format!(" {}", h))
                .unwrap_or_default();
            let entry_text = format!("{}{}", name, hint_str);
            let prefix = if is_selected { "> " } else { "  " };
            let full_entry = format!("{}{}", prefix, entry_text);

            // Truncate to inner width.
            let truncated: String = full_entry.chars().take(inner_width).collect();
            let padded = format!("\u{2502}{:<width$}\u{2502}", truncated, width = inner_width);

            let row_fg = if is_selected { key_fg } else { fg };
            commands.push(DrawCommand {
                x: start_x,
                y: start_y + 3 + vis_idx as u16,
                text: padded,
                fg: row_fg,
                bg,
            });
        }

        // If no entries, show "No matches".
        if visible_count == 0 {
            let msg = "  No matches";
            let padded = format!("\u{2502}{:<width$}\u{2502}", msg, width = inner_width);
            commands.push(DrawCommand {
                x: start_x,
                y: start_y + 3,
                text: padded,
                fg,
                bg,
            });
        }

        // Bottom border.
        let bottom_y = if visible_count == 0 {
            start_y + 4
        } else {
            start_y + 3 + visible_count as u16
        };
        let bottom = format!("\u{2570}{}\u{256F}", "\u{2500}".repeat(inner_width));
        commands.push(DrawCommand {
            x: start_x,
            y: bottom_y,
            text: bottom,
            fg,
            bg,
        });

        commands
    }
}

impl Default for CommandPaletteState {
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
    fn new_palette_has_all_commands() {
        let palette = CommandPaletteState::new();
        assert!(!palette.filtered.is_empty());
        assert_eq!(palette.selected, 0);
        assert!(palette.input.is_empty());
    }

    #[test]
    fn insert_char_filters() {
        let mut palette = CommandPaletteState::new();
        let initial_count = palette.filtered.len();
        palette.insert_char('T');
        palette.insert_char('a');
        palette.insert_char('b');
        // Should filter to Tab* commands.
        assert!(palette.filtered.len() < initial_count);
        assert!(palette.filtered.iter().all(|(n, _)| n.contains("Tab")));
    }

    #[test]
    fn backspace_refilters() {
        let mut palette = CommandPaletteState::new();
        let initial_count = palette.filtered.len();
        palette.insert_char('Z');
        palette.insert_char('Q');
        palette.insert_char('Z');
        // No command name contains "ZQZ".
        assert!(palette.filtered.is_empty());
        palette.backspace();
        palette.backspace();
        palette.backspace();
        // After removing all chars, all commands should be back.
        assert_eq!(palette.filtered.len(), initial_count);
    }

    #[test]
    fn tab_complete_fills_common_prefix() {
        let mut palette = CommandPaletteState::new();
        palette.insert_char('T');
        palette.insert_char('a');
        palette.insert_char('b');
        palette.tab_complete(false);
        // All filtered items start with "Tab", so the input should at least be "Tab".
        assert!(palette.input.starts_with("Tab"));
    }

    #[test]
    fn tab_complete_single_match() {
        let mut palette = CommandPaletteState::new();
        palette.insert_char('T');
        palette.insert_char('a');
        palette.insert_char('b');
        palette.insert_char('N');
        palette.insert_char('e');
        palette.insert_char('w');
        palette.tab_complete(false);
        assert_eq!(palette.input, "TabNew");
    }

    #[test]
    fn select_next_wraps() {
        let mut palette = CommandPaletteState::new();
        let count = palette.filtered.len();
        for _ in 0..count {
            palette.select_next();
        }
        assert_eq!(palette.selected, 0);
    }

    #[test]
    fn select_prev_wraps() {
        let mut palette = CommandPaletteState::new();
        palette.select_prev();
        assert_eq!(palette.selected, palette.filtered.len() - 1);
    }

    #[test]
    fn current_input_with_args() {
        let mut palette = CommandPaletteState::new();
        // Type "TabRename My Tab"
        for c in "TabRename My Tab".chars() {
            palette.insert_char(c);
        }
        let input = palette.current_input();
        assert_eq!(input, "TabRename My Tab");
    }

    #[test]
    fn case_insensitive_filter() {
        let mut palette = CommandPaletteState::new();
        palette.insert_char('t');
        palette.insert_char('a');
        palette.insert_char('b');
        // Should still match Tab* commands (case-insensitive).
        assert!(!palette.filtered.is_empty());
        assert!(palette.filtered.iter().all(|(n, _)| n.contains("Tab")));
    }

    #[test]
    fn render_returns_draw_commands() {
        let palette = CommandPaletteState::new();
        let theme = Theme::default();
        let commands = palette.render(80, 24, &theme);
        assert!(!commands.is_empty());
    }

    #[test]
    fn render_too_small_screen_returns_empty() {
        let palette = CommandPaletteState::new();
        let theme = Theme::default();
        let commands = palette.render(10, 3, &theme);
        assert!(commands.is_empty());
    }

    #[test]
    fn argument_hint_shown_in_filtered() {
        let mut palette = CommandPaletteState::new();
        palette.insert_char('T');
        palette.insert_char('a');
        palette.insert_char('b');
        palette.insert_char('R');
        // Should have TabRename which has a hint.
        let rename_entry = palette.filtered.iter().find(|(n, _)| n == "TabRename");
        assert!(rename_entry.is_some());
        assert_eq!(rename_entry.unwrap().1.as_deref(), Some("<name>"));
    }

    #[test]
    fn tab_cycling() {
        let mut palette = CommandPaletteState::new();
        palette.insert_char('T');
        palette.insert_char('a');
        palette.insert_char('b');
        // First tab: LCP completion.
        palette.tab_complete(false);
        // Second tab: should cycle to next.
        let first_selected = palette.selected;
        palette.tab_complete(false);
        assert_ne!(palette.selected, first_selected);
    }
}
