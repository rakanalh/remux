use crossterm::style::Color;

/// Visual theme for the Remux UI. Controls colors for modes, frames, tabs,
/// the status bar, and the which-key popup.
#[derive(Debug, Clone)]
pub struct Theme {
    // Mode indicator colors
    pub mode_normal_fg: Color,
    pub mode_normal_bg: Color,
    pub mode_command_fg: Color,
    pub mode_command_bg: Color,
    pub mode_visual_fg: Color,
    pub mode_visual_bg: Color,

    // Pane frame colors
    pub frame_fg: Color,
    pub frame_bg: Color,
    pub frame_active_fg: Color,

    // Status bar
    pub status_bar_fg: Color,
    pub status_bar_bg: Color,

    // Tab bar
    pub tab_active_fg: Color,
    pub tab_active_bg: Color,
    pub tab_inactive_fg: Color,

    // Which-key popup
    pub whichkey_fg: Color,
    pub whichkey_bg: Color,
    pub whichkey_key_fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            // Normal mode: green tones
            mode_normal_fg: Color::Black,
            mode_normal_bg: Color::Green,

            // Command mode: blue tones
            mode_command_fg: Color::Black,
            mode_command_bg: Color::Blue,

            // Visual mode: magenta tones
            mode_visual_fg: Color::Black,
            mode_visual_bg: Color::Magenta,

            // Frame: subdued gray, active highlighted
            frame_fg: Color::DarkGrey,
            frame_bg: Color::Reset,
            frame_active_fg: Color::White,

            // Status bar
            status_bar_fg: Color::White,
            status_bar_bg: Color::DarkGrey,

            // Tabs
            tab_active_fg: Color::Black,
            tab_active_bg: Color::White,
            tab_inactive_fg: Color::Grey,

            // Which-key popup
            whichkey_fg: Color::AnsiValue(252), // Light grey text
            whichkey_bg: Color::AnsiValue(235), // Very dark grey (matches status bar)
            whichkey_key_fg: Color::AnsiValue(10), // Bright green keys
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_is_valid() {
        let theme = Theme::default();
        // Sanity check that distinct modes have distinct background colors.
        assert_ne!(
            format!("{:?}", theme.mode_normal_bg),
            format!("{:?}", theme.mode_command_bg)
        );
        assert_ne!(
            format!("{:?}", theme.mode_command_bg),
            format!("{:?}", theme.mode_visual_bg)
        );
    }
}
