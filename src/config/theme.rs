use crossterm::style::Color;
use serde::de::{self, MapAccess, Visitor};
use serde::Deserialize;

use crate::protocol::CellColor;

// ---------------------------------------------------------------------------
// ThemeColor
// ---------------------------------------------------------------------------

/// A color value that can be deserialized from TOML in multiple formats:
/// - A string name: `"green"`, `"bright_blue"`, `"reset"`
/// - An ANSI index table: `{ ansi = 235 }`
/// - An RGB array table: `{ rgb = [255, 128, 0] }`
#[derive(Debug, Clone, PartialEq)]
pub enum ThemeColor {
    /// A named color (e.g. "green", "black", "reset").
    Named(String),
    /// A 256-color palette index.
    Indexed(u8),
    /// A 24-bit true color value.
    Rgb(u8, u8, u8),
}

impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ThemeColorVisitor;

        impl<'de> Visitor<'de> for ThemeColorVisitor {
            type Value = ThemeColor;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter
                    .write_str(r#"a color string ("green"), { ansi = N }, or { rgb = [R, G, B] }"#)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<ThemeColor, E> {
                if let Some(hex) = v.strip_prefix('#') {
                    if hex.len() == 6 {
                        let r = u8::from_str_radix(&hex[0..2], 16).map_err(de::Error::custom)?;
                        let g = u8::from_str_radix(&hex[2..4], 16).map_err(de::Error::custom)?;
                        let b = u8::from_str_radix(&hex[4..6], 16).map_err(de::Error::custom)?;
                        return Ok(ThemeColor::Rgb(r, g, b));
                    }
                    return Err(de::Error::custom("hex color must be 6 digits: #RRGGBB"));
                }
                Ok(ThemeColor::Named(v.to_string()))
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<ThemeColor, M::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("expected 'ansi' or 'rgb' key"))?;
                match key.as_str() {
                    "ansi" => {
                        let val: u8 = map.next_value()?;
                        Ok(ThemeColor::Indexed(val))
                    }
                    "rgb" => {
                        let arr: [u8; 3] = map.next_value()?;
                        Ok(ThemeColor::Rgb(arr[0], arr[1], arr[2]))
                    }
                    other => Err(de::Error::unknown_field(other, &["ansi", "rgb"])),
                }
            }
        }

        deserializer.deserialize_any(ThemeColorVisitor)
    }
}

// ---------------------------------------------------------------------------
// ThemeColor -> crossterm::style::Color
// ---------------------------------------------------------------------------

impl ThemeColor {
    /// Convert to a `crossterm::style::Color` (used client-side for which-key).
    pub fn to_crossterm_color(&self) -> Color {
        match self {
            ThemeColor::Named(name) => named_to_crossterm(name),
            ThemeColor::Indexed(idx) => Color::AnsiValue(*idx),
            ThemeColor::Rgb(r, g, b) => Color::Rgb {
                r: *r,
                g: *g,
                b: *b,
            },
        }
    }

    /// Convert to a `CellColor` (used compositor-side).
    pub fn to_cell_color(&self) -> CellColor {
        match self {
            ThemeColor::Named(name) => named_to_cell_color(name),
            ThemeColor::Indexed(idx) => CellColor::Indexed(*idx),
            ThemeColor::Rgb(r, g, b) => CellColor::Rgb(*r, *g, *b),
        }
    }
}

/// Map a named color string to a `crossterm::style::Color`.
fn named_to_crossterm(name: &str) -> Color {
    match name.to_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::DarkRed,
        "green" => Color::DarkGreen,
        "yellow" => Color::DarkYellow,
        "blue" => Color::DarkBlue,
        "magenta" => Color::DarkMagenta,
        "cyan" => Color::DarkCyan,
        "white" => Color::Grey,
        "dark_grey" | "dark_gray" => Color::DarkGrey,
        "light_red" | "bright_red" => Color::Red,
        "light_green" | "bright_green" => Color::Green,
        "light_yellow" | "bright_yellow" => Color::Yellow,
        "light_blue" | "bright_blue" => Color::Blue,
        "light_magenta" | "bright_magenta" => Color::Magenta,
        "light_cyan" | "bright_cyan" => Color::Cyan,
        "light_grey" | "light_gray" | "bright_white" => Color::White,
        "reset" | "default" => Color::Reset,
        _ => Color::Reset,
    }
}

/// Map a named color string to a `CellColor`.
fn named_to_cell_color(name: &str) -> CellColor {
    match name.to_lowercase().as_str() {
        "black" => CellColor::Indexed(0),
        "red" => CellColor::Indexed(1),
        "green" => CellColor::Indexed(2),
        "yellow" => CellColor::Indexed(3),
        "blue" => CellColor::Indexed(4),
        "magenta" => CellColor::Indexed(5),
        "cyan" => CellColor::Indexed(6),
        "white" => CellColor::Indexed(7),
        "dark_grey" | "dark_gray" => CellColor::Indexed(8),
        "light_red" | "bright_red" => CellColor::Indexed(9),
        "light_green" | "bright_green" => CellColor::Indexed(10),
        "light_yellow" | "bright_yellow" => CellColor::Indexed(11),
        "light_blue" | "bright_blue" => CellColor::Indexed(12),
        "light_magenta" | "bright_magenta" => CellColor::Indexed(13),
        "light_cyan" | "bright_cyan" => CellColor::Indexed(14),
        "light_grey" | "light_gray" | "bright_white" => CellColor::Indexed(15),
        "reset" | "default" => CellColor::Default,
        _ => CellColor::Default,
    }
}

// ---------------------------------------------------------------------------
// ThemeConfig (deserializable from TOML)
// ---------------------------------------------------------------------------

/// User-facing theme configuration. All fields use `ThemeColor` and have
/// sensible defaults that match the current hardcoded compositor values.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub mode_normal_fg: ThemeColor,
    pub mode_normal_bg: ThemeColor,
    pub mode_command_fg: ThemeColor,
    pub mode_command_bg: ThemeColor,
    pub mode_visual_fg: ThemeColor,
    pub mode_visual_bg: ThemeColor,
    pub frame_fg: ThemeColor,
    pub frame_bg: ThemeColor,
    pub frame_active_fg: ThemeColor,
    pub status_bar_fg: ThemeColor,
    pub status_bar_bg: ThemeColor,
    pub tab_active_fg: ThemeColor,
    pub tab_active_bg: ThemeColor,
    pub tab_inactive_fg: ThemeColor,
    pub whichkey_fg: ThemeColor,
    pub whichkey_bg: ThemeColor,
    pub whichkey_key_fg: ThemeColor,
    pub separator_fg: ThemeColor,
    pub pane_label_fg: ThemeColor,
    pub pane_label_bg: ThemeColor,
    pub session_name_fg: ThemeColor,

    // Search mode indicator
    pub mode_search_fg: ThemeColor,
    pub mode_search_bg: ThemeColor,

    // Search highlight colors
    pub search_match_fg: ThemeColor,
    pub search_match_bg: ThemeColor,
    pub search_current_fg: ThemeColor,
    pub search_current_bg: ThemeColor,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            // Mode indicators
            mode_normal_fg: ThemeColor::Rgb(30, 30, 46), // base (dark bg text)
            mode_normal_bg: ThemeColor::Rgb(166, 227, 161), // green
            mode_command_fg: ThemeColor::Rgb(30, 30, 46), // base
            mode_command_bg: ThemeColor::Rgb(137, 180, 250), // blue
            mode_visual_fg: ThemeColor::Rgb(30, 30, 46), // base
            mode_visual_bg: ThemeColor::Rgb(203, 166, 247), // mauve

            // Pane frame
            frame_fg: ThemeColor::Rgb(88, 91, 112), // surface2
            frame_bg: ThemeColor::Rgb(30, 30, 46),  // base
            frame_active_fg: ThemeColor::Rgb(137, 180, 250), // blue

            // Status bar
            status_bar_fg: ThemeColor::Rgb(166, 173, 200), // subtext0
            status_bar_bg: ThemeColor::Rgb(24, 24, 37),    // mantle

            // Tabs
            tab_active_fg: ThemeColor::Rgb(30, 30, 46), // base
            tab_active_bg: ThemeColor::Rgb(137, 180, 250), // blue
            tab_inactive_fg: ThemeColor::Rgb(147, 153, 178), // overlay2

            // Which-key popup
            whichkey_fg: ThemeColor::Rgb(205, 214, 244), // text
            whichkey_bg: ThemeColor::Rgb(24, 24, 37),    // mantle
            whichkey_key_fg: ThemeColor::Rgb(166, 227, 161), // green

            // Separators and labels
            separator_fg: ThemeColor::Rgb(108, 112, 134), // overlay0
            pane_label_fg: ThemeColor::Rgb(205, 214, 244), // text
            pane_label_bg: ThemeColor::Rgb(30, 30, 46),   // base
            session_name_fg: ThemeColor::Rgb(148, 226, 213), // teal

            // Search mode indicator
            mode_search_fg: ThemeColor::Rgb(30, 30, 46), // base
            mode_search_bg: ThemeColor::Rgb(249, 226, 175), // yellow

            // Search highlight colors
            search_match_fg: ThemeColor::Rgb(30, 30, 46), // base
            search_match_bg: ThemeColor::Rgb(88, 91, 112), // surface2 (subtle)
            search_current_fg: ThemeColor::Rgb(30, 30, 46), // base
            search_current_bg: ThemeColor::Rgb(250, 179, 135), // peach (stands out)
        }
    }
}

// ---------------------------------------------------------------------------
// Theme (crossterm colors, used client-side)
// ---------------------------------------------------------------------------

/// Visual theme for the Remux UI. Controls colors for modes, frames, tabs,
/// the status bar, and the which-key popup.
///
/// Uses `crossterm::style::Color` for client-side rendering (e.g. which-key).
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

    // Additional fields
    pub separator_fg: Color,
    pub pane_label_fg: Color,
    pub pane_label_bg: Color,
    pub session_name_fg: Color,

    // Search mode indicator
    pub mode_search_fg: Color,
    pub mode_search_bg: Color,

    // Search highlight colors
    pub search_match_fg: Color,
    pub search_match_bg: Color,
    pub search_current_fg: Color,
    pub search_current_bg: Color,
}

impl Theme {
    /// Construct a `Theme` from a `ThemeConfig`.
    pub fn from_config(config: &ThemeConfig) -> Self {
        Self {
            mode_normal_fg: config.mode_normal_fg.to_crossterm_color(),
            mode_normal_bg: config.mode_normal_bg.to_crossterm_color(),
            mode_command_fg: config.mode_command_fg.to_crossterm_color(),
            mode_command_bg: config.mode_command_bg.to_crossterm_color(),
            mode_visual_fg: config.mode_visual_fg.to_crossterm_color(),
            mode_visual_bg: config.mode_visual_bg.to_crossterm_color(),
            frame_fg: config.frame_fg.to_crossterm_color(),
            frame_bg: config.frame_bg.to_crossterm_color(),
            frame_active_fg: config.frame_active_fg.to_crossterm_color(),
            status_bar_fg: config.status_bar_fg.to_crossterm_color(),
            status_bar_bg: config.status_bar_bg.to_crossterm_color(),
            tab_active_fg: config.tab_active_fg.to_crossterm_color(),
            tab_active_bg: config.tab_active_bg.to_crossterm_color(),
            tab_inactive_fg: config.tab_inactive_fg.to_crossterm_color(),
            whichkey_fg: config.whichkey_fg.to_crossterm_color(),
            whichkey_bg: config.whichkey_bg.to_crossterm_color(),
            whichkey_key_fg: config.whichkey_key_fg.to_crossterm_color(),
            separator_fg: config.separator_fg.to_crossterm_color(),
            pane_label_fg: config.pane_label_fg.to_crossterm_color(),
            pane_label_bg: config.pane_label_bg.to_crossterm_color(),
            session_name_fg: config.session_name_fg.to_crossterm_color(),
            mode_search_fg: config.mode_search_fg.to_crossterm_color(),
            mode_search_bg: config.mode_search_bg.to_crossterm_color(),
            search_match_fg: config.search_match_fg.to_crossterm_color(),
            search_match_bg: config.search_match_bg.to_crossterm_color(),
            search_current_fg: config.search_current_fg.to_crossterm_color(),
            search_current_bg: config.search_current_bg.to_crossterm_color(),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_config(&ThemeConfig::default())
    }
}

// ---------------------------------------------------------------------------
// CompositorTheme (CellColor, used server-side)
// ---------------------------------------------------------------------------

/// Theme for the server-side compositor. Uses `CellColor` fields that map
/// directly to the protocol's color representation.
#[derive(Debug, Clone)]
pub struct CompositorTheme {
    pub mode_normal_fg: CellColor,
    pub mode_normal_bg: CellColor,
    pub mode_command_fg: CellColor,
    pub mode_command_bg: CellColor,
    pub mode_visual_fg: CellColor,
    pub mode_visual_bg: CellColor,
    pub frame_fg: CellColor,
    pub frame_bg: CellColor,
    pub frame_active_fg: CellColor,
    pub status_bar_fg: CellColor,
    pub status_bar_bg: CellColor,
    pub tab_active_fg: CellColor,
    pub tab_active_bg: CellColor,
    pub tab_inactive_fg: CellColor,
    pub whichkey_fg: CellColor,
    pub whichkey_bg: CellColor,
    pub whichkey_key_fg: CellColor,
    pub mode_search_fg: CellColor,
    pub mode_search_bg: CellColor,
    pub separator_fg: CellColor,
    pub pane_label_fg: CellColor,
    pub pane_label_bg: CellColor,
    pub session_name_fg: CellColor,
}

impl CompositorTheme {
    /// Construct a `CompositorTheme` from a `ThemeConfig`.
    pub fn from_config(config: &ThemeConfig) -> Self {
        Self {
            mode_normal_fg: config.mode_normal_fg.to_cell_color(),
            mode_normal_bg: config.mode_normal_bg.to_cell_color(),
            mode_command_fg: config.mode_command_fg.to_cell_color(),
            mode_command_bg: config.mode_command_bg.to_cell_color(),
            mode_visual_fg: config.mode_visual_fg.to_cell_color(),
            mode_visual_bg: config.mode_visual_bg.to_cell_color(),
            frame_fg: config.frame_fg.to_cell_color(),
            frame_bg: config.frame_bg.to_cell_color(),
            frame_active_fg: config.frame_active_fg.to_cell_color(),
            status_bar_fg: config.status_bar_fg.to_cell_color(),
            status_bar_bg: config.status_bar_bg.to_cell_color(),
            tab_active_fg: config.tab_active_fg.to_cell_color(),
            tab_active_bg: config.tab_active_bg.to_cell_color(),
            tab_inactive_fg: config.tab_inactive_fg.to_cell_color(),
            whichkey_fg: config.whichkey_fg.to_cell_color(),
            whichkey_bg: config.whichkey_bg.to_cell_color(),
            whichkey_key_fg: config.whichkey_key_fg.to_cell_color(),
            mode_search_fg: config.mode_search_fg.to_cell_color(),
            mode_search_bg: config.mode_search_bg.to_cell_color(),
            separator_fg: config.separator_fg.to_cell_color(),
            pane_label_fg: config.pane_label_fg.to_cell_color(),
            pane_label_bg: config.pane_label_bg.to_cell_color(),
            session_name_fg: config.session_name_fg.to_cell_color(),
        }
    }

    /// Get foreground/background colors for the mode indicator.
    pub fn mode_colors(&self, mode: &str) -> (CellColor, CellColor) {
        match mode {
            "NORMAL" => (self.mode_normal_fg.clone(), self.mode_normal_bg.clone()),
            "COMMAND" => (self.mode_command_fg.clone(), self.mode_command_bg.clone()),
            "VISUAL" => (self.mode_visual_fg.clone(), self.mode_visual_bg.clone()),
            "SEARCH" => (self.mode_search_fg.clone(), self.mode_search_bg.clone()),
            _ => (CellColor::Indexed(15), CellColor::Indexed(238)),
        }
    }
}

impl Default for CompositorTheme {
    fn default() -> Self {
        Self::from_config(&ThemeConfig::default())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    #[test]
    fn theme_color_serde_string() {
        let val: toml::Value = toml::from_str(r#"color = "green""#).unwrap();
        let tc: ThemeColor = ThemeColor::deserialize(val.get("color").unwrap().clone()).unwrap();
        assert_eq!(tc, ThemeColor::Named("green".to_string()));
    }

    #[test]
    fn theme_color_serde_ansi() {
        // Inline table form that toml supports
        let val: toml::Value = toml::from_str("color = { ansi = 235 }").unwrap();
        let tc: ThemeColor = ThemeColor::deserialize(val.get("color").unwrap().clone()).unwrap();
        assert_eq!(tc, ThemeColor::Indexed(235));
    }

    #[test]
    fn theme_color_serde_rgb() {
        let val: toml::Value = toml::from_str("color = { rgb = [255, 128, 0] }").unwrap();
        let tc: ThemeColor = ThemeColor::deserialize(val.get("color").unwrap().clone()).unwrap();
        assert_eq!(tc, ThemeColor::Rgb(255, 128, 0));
    }

    #[test]
    fn theme_config_default_matches_compositor_hardcoded() {
        let ct = CompositorTheme::default();

        // Mode colors (Catppuccin Mocha)
        assert_eq!(ct.mode_normal_fg, CellColor::Rgb(30, 30, 46)); // base
        assert_eq!(ct.mode_normal_bg, CellColor::Rgb(166, 227, 161)); // green
        assert_eq!(ct.mode_command_fg, CellColor::Rgb(30, 30, 46)); // base
        assert_eq!(ct.mode_command_bg, CellColor::Rgb(137, 180, 250)); // blue
        assert_eq!(ct.mode_visual_fg, CellColor::Rgb(30, 30, 46)); // base
        assert_eq!(ct.mode_visual_bg, CellColor::Rgb(203, 166, 247)); // mauve

        // Frame colors
        assert_eq!(ct.frame_fg, CellColor::Rgb(88, 91, 112)); // surface2
        assert_eq!(ct.frame_bg, CellColor::Rgb(30, 30, 46)); // base
        assert_eq!(ct.frame_active_fg, CellColor::Rgb(137, 180, 250)); // blue

        // Status bar
        assert_eq!(ct.status_bar_fg, CellColor::Rgb(166, 173, 200)); // subtext0
        assert_eq!(ct.status_bar_bg, CellColor::Rgb(24, 24, 37)); // mantle

        // Tabs
        assert_eq!(ct.tab_active_fg, CellColor::Rgb(30, 30, 46)); // base
        assert_eq!(ct.tab_active_bg, CellColor::Rgb(137, 180, 250)); // blue
        assert_eq!(ct.tab_inactive_fg, CellColor::Rgb(147, 153, 178)); // overlay2

        // Separators and session name
        assert_eq!(ct.separator_fg, CellColor::Rgb(108, 112, 134)); // overlay0
        assert_eq!(ct.session_name_fg, CellColor::Rgb(148, 226, 213)); // teal

        // Search mode
        assert_eq!(ct.mode_search_fg, CellColor::Rgb(30, 30, 46)); // base
        assert_eq!(ct.mode_search_bg, CellColor::Rgb(249, 226, 175)); // yellow
    }

    #[test]
    fn partial_theme_config_deserialization() {
        let toml_str = r#"
            mode_normal_bg = "bright_green"
            frame_active_fg = { ansi = 4 }
        "#;
        let config: ThemeConfig = toml::from_str(toml_str).unwrap();
        // Overridden values
        assert_eq!(
            config.mode_normal_bg,
            ThemeColor::Named("bright_green".to_string())
        );
        assert_eq!(config.frame_active_fg, ThemeColor::Indexed(4));
        // Default values preserved
        assert_eq!(config.status_bar_bg, ThemeColor::Rgb(24, 24, 37));
    }

    #[test]
    fn named_color_to_cell_color_mapping() {
        assert_eq!(
            ThemeColor::Named("black".to_string()).to_cell_color(),
            CellColor::Indexed(0)
        );
        assert_eq!(
            ThemeColor::Named("bright_green".to_string()).to_cell_color(),
            CellColor::Indexed(10)
        );
        assert_eq!(
            ThemeColor::Named("bright_blue".to_string()).to_cell_color(),
            CellColor::Indexed(12)
        );
        assert_eq!(
            ThemeColor::Named("bright_magenta".to_string()).to_cell_color(),
            CellColor::Indexed(13)
        );
        assert_eq!(
            ThemeColor::Named("reset".to_string()).to_cell_color(),
            CellColor::Default
        );
    }

    #[test]
    fn named_color_to_crossterm_mapping() {
        assert_eq!(
            ThemeColor::Named("black".to_string()).to_crossterm_color(),
            Color::Black
        );
        assert_eq!(
            ThemeColor::Named("reset".to_string()).to_crossterm_color(),
            Color::Reset
        );
    }

    #[test]
    fn compositor_theme_mode_colors() {
        let ct = CompositorTheme::default();
        let (fg, bg) = ct.mode_colors("NORMAL");
        assert_eq!(fg, CellColor::Rgb(30, 30, 46));
        assert_eq!(bg, CellColor::Rgb(166, 227, 161));

        let (fg, bg) = ct.mode_colors("COMMAND");
        assert_eq!(fg, CellColor::Rgb(30, 30, 46));
        assert_eq!(bg, CellColor::Rgb(137, 180, 250));

        let (fg, bg) = ct.mode_colors("VISUAL");
        assert_eq!(fg, CellColor::Rgb(30, 30, 46));
        assert_eq!(bg, CellColor::Rgb(203, 166, 247));

        let (fg, bg) = ct.mode_colors("SEARCH");
        assert_eq!(fg, CellColor::Rgb(30, 30, 46));
        assert_eq!(bg, CellColor::Rgb(249, 226, 175));
    }

    #[test]
    fn theme_color_serde_hex() {
        let val: toml::Value = toml::from_str(r##"color = "#f5e0dc""##).unwrap();
        let tc: ThemeColor = ThemeColor::deserialize(val.get("color").unwrap().clone()).unwrap();
        assert_eq!(tc, ThemeColor::Rgb(245, 224, 220));
    }

    #[test]
    fn theme_color_serde_hex_uppercase() {
        let val: toml::Value = toml::from_str(r##"color = "#CBA6F7""##).unwrap();
        let tc: ThemeColor = ThemeColor::deserialize(val.get("color").unwrap().clone()).unwrap();
        assert_eq!(tc, ThemeColor::Rgb(203, 166, 247));
    }
}
