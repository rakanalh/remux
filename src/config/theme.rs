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

/// **The one table of color names.** Each row is `(name, ANSI index, crossterm
/// color)`; both [`named_to_crossterm`] and [`named_to_cell_color`] read it, so
/// a name added here reaches both conversions. They used to enumerate the same
/// sixteen names in two independent `match`es, where adding a name to one
/// silently fell through to the fallback in the other.
///
/// The crossterm value is carried explicitly instead of being derived from the
/// index because crossterm's named variants (`Color::Black`) and its indexed one
/// (`Color::AnsiValue(0)`) emit different SGR sequences, and the named form is
/// what the client has always emitted.
///
/// `"reset"`/`"default"` are deliberately absent: they mean "no color", which
/// each conversion expresses in its own type via the fallback below.
const NAMED_COLORS: &[(&str, u8, Color)] = &[
    ("black", 0, Color::Black),
    ("red", 1, Color::DarkRed),
    ("green", 2, Color::DarkGreen),
    ("yellow", 3, Color::DarkYellow),
    ("blue", 4, Color::DarkBlue),
    ("magenta", 5, Color::DarkMagenta),
    ("cyan", 6, Color::DarkCyan),
    ("white", 7, Color::Grey),
    ("dark_grey", 8, Color::DarkGrey),
    ("dark_gray", 8, Color::DarkGrey),
    ("light_red", 9, Color::Red),
    ("bright_red", 9, Color::Red),
    ("light_green", 10, Color::Green),
    ("bright_green", 10, Color::Green),
    ("light_yellow", 11, Color::Yellow),
    ("bright_yellow", 11, Color::Yellow),
    ("light_blue", 12, Color::Blue),
    ("bright_blue", 12, Color::Blue),
    ("light_magenta", 13, Color::Magenta),
    ("bright_magenta", 13, Color::Magenta),
    ("light_cyan", 14, Color::Cyan),
    ("bright_cyan", 14, Color::Cyan),
    ("light_grey", 15, Color::White),
    ("light_gray", 15, Color::White),
    ("bright_white", 15, Color::White),
];

/// Look a color name up in [`NAMED_COLORS`], case-insensitively.
fn named_lookup(name: &str) -> Option<&'static (&'static str, u8, Color)> {
    let lower = name.to_lowercase();
    NAMED_COLORS.iter().find(|(n, _, _)| *n == lower)
}

/// Map a named color string to a `crossterm::style::Color`.
fn named_to_crossterm(name: &str) -> Color {
    named_lookup(name).map_or(Color::Reset, |(_, _, c)| *c)
}

/// Map a named color string to a `CellColor`.
fn named_to_cell_color(name: &str) -> CellColor {
    named_lookup(name).map_or(CellColor::Default, |(_, i, _)| CellColor::Indexed(*i))
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
    /// Background of a pane's border cells. `None` (the default) leaves them on
    /// the terminal's default background, which is how borders have always been
    /// drawn — so the shipped defaults render exactly as before. Set it to paint
    /// the frame on a solid color.
    pub frame_bg: Option<ThemeColor>,
    pub frame_active_fg: ThemeColor,
    pub status_bar_fg: ThemeColor,
    pub status_bar_bg: ThemeColor,
    pub tab_active_fg: ThemeColor,
    pub tab_active_bg: ThemeColor,
    pub tab_inactive_fg: ThemeColor,
    /// Background of an inactive tab block in a **pane's tab strip** (the zellij
    /// top-border tabs and the tmux 1-row tab bar). Named replacement for the
    /// `Indexed(237)` literal those two renderers each carried.
    ///
    /// The status bar's inactive SESSION tabs are a different concept and keep
    /// inheriting `status_bar_bg`: they sit flat on the bar rather than reading
    /// as a raised block.
    pub tab_inactive_bg: ThemeColor,
    pub whichkey_fg: ThemeColor,
    pub whichkey_bg: ThemeColor,
    pub whichkey_key_fg: ThemeColor,
    pub separator_fg: ThemeColor,
    /// Foreground of the pane label / stacked-tab title drawn on a pane's top
    /// border. `None` (the default) draws it in the border's own color, which
    /// tracks focus (`frame_active_fg` vs `frame_fg`) — the historical behavior,
    /// and something no single static color can reproduce.
    pub pane_label_fg: Option<ThemeColor>,
    /// Background of the pane label on a pane's top border. `None` (the default)
    /// leaves it on the terminal's default background, as before.
    pub pane_label_bg: Option<ThemeColor>,
    pub session_name_fg: ThemeColor,

    // Search mode indicator
    pub mode_search_fg: ThemeColor,
    pub mode_search_bg: ThemeColor,

    // Status bar right-hand segments
    /// The `(n/m)` search-match counter on the status bar. Distinct from
    /// `mode_search_fg`/`_bg` (the `[SEARCH]` mode chip), which use the
    /// Catppuccin palette while the counter has always used ANSI bright yellow.
    pub search_count_fg: ThemeColor,
    pub search_count_bg: ThemeColor,
    /// The layout-mode indicator (`bsp`/`master`/`monocle`/`grid`/`custom`) at
    /// the far right of the status bar.
    pub layout_indicator_fg: ThemeColor,
    pub layout_indicator_bg: ThemeColor,

    // Background-activity markers on inactive status-bar tabs
    pub tab_bell_fg: ThemeColor,
    pub tab_activity_fg: ThemeColor,
    pub tab_silent_fg: ThemeColor,

    /// Mode chip colors for a mode name with no role of its own — the fallback
    /// arm of [`CompositorTheme::mode_colors`].
    pub mode_unknown_fg: ThemeColor,
    pub mode_unknown_bg: ThemeColor,

    // Search highlight colors. These four are deliberately absent from
    // `CompositorTheme`: search highlighting is applied CLIENT-side (see
    // `client::renderer`), over an already-composited frame, so the server never
    // needs them. Every other role above is server-side chrome and therefore
    // lives in both structs.
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

            // Pane frame. `frame_bg` is None so borders keep sitting on the
            // terminal's default background, exactly as they always have.
            frame_fg: ThemeColor::Rgb(88, 91, 112), // surface2
            frame_bg: None,
            frame_active_fg: ThemeColor::Rgb(137, 180, 250), // blue

            // Status bar
            status_bar_fg: ThemeColor::Rgb(166, 173, 200), // subtext0
            status_bar_bg: ThemeColor::Rgb(24, 24, 37),    // mantle

            // Tabs
            tab_active_fg: ThemeColor::Rgb(30, 30, 46), // base
            tab_active_bg: ThemeColor::Rgb(137, 180, 250), // blue
            tab_inactive_fg: ThemeColor::Rgb(147, 153, 178), // overlay2
            tab_inactive_bg: ThemeColor::Indexed(237),  // the historical literal

            // Which-key popup
            whichkey_fg: ThemeColor::Rgb(205, 214, 244), // text
            whichkey_bg: ThemeColor::Rgb(24, 24, 37),    // mantle
            whichkey_key_fg: ThemeColor::Rgb(166, 227, 161), // green

            // Separators and labels. The pane label defaults to None/None: it
            // inherits the border color (which tracks focus) on the default
            // background, as it did before these roles were consumed.
            separator_fg: ThemeColor::Rgb(108, 112, 134), // overlay0
            pane_label_fg: None,
            pane_label_bg: None,
            session_name_fg: ThemeColor::Rgb(148, 226, 213), // teal

            // Search mode indicator
            mode_search_fg: ThemeColor::Rgb(30, 30, 46), // base
            mode_search_bg: ThemeColor::Rgb(249, 226, 175), // yellow

            // Status bar right-hand segments (the historical literals)
            search_count_fg: ThemeColor::Indexed(0),  // black
            search_count_bg: ThemeColor::Indexed(11), // bright yellow
            layout_indicator_fg: ThemeColor::Indexed(0), // black
            layout_indicator_bg: ThemeColor::Indexed(245), // grey

            // Activity markers (the historical literals)
            tab_bell_fg: ThemeColor::Indexed(9), // bright red: urgent
            tab_activity_fg: ThemeColor::Indexed(11), // bright yellow
            tab_silent_fg: ThemeColor::Indexed(10), // bright green

            // Unknown-mode chip (the historical literals)
            mode_unknown_fg: ThemeColor::Indexed(15),
            mode_unknown_bg: ThemeColor::Indexed(238),

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

    // Pane frame colors. `frame_bg` is optional in the same sense as
    // `ThemeConfig::frame_bg`: `None` means "leave the terminal's background".
    pub frame_fg: Color,
    pub frame_bg: Option<Color>,
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

    // Additional fields. The pane-label pair is optional in the same sense as
    // `ThemeConfig`'s: `None` means "inherit the border color / background".
    pub separator_fg: Color,
    pub pane_label_fg: Option<Color>,
    pub pane_label_bg: Option<Color>,
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
            frame_bg: config.frame_bg.as_ref().map(ThemeColor::to_crossterm_color),
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
            pane_label_fg: config
                .pane_label_fg
                .as_ref()
                .map(ThemeColor::to_crossterm_color),
            pane_label_bg: config
                .pane_label_bg
                .as_ref()
                .map(ThemeColor::to_crossterm_color),
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
    /// `None` = leave the terminal's background (see [`ThemeConfig::frame_bg`]).
    pub frame_bg: Option<CellColor>,
    pub frame_active_fg: CellColor,
    pub status_bar_fg: CellColor,
    pub status_bar_bg: CellColor,
    pub tab_active_fg: CellColor,
    pub tab_active_bg: CellColor,
    pub tab_inactive_fg: CellColor,
    pub tab_inactive_bg: CellColor,
    pub whichkey_fg: CellColor,
    pub whichkey_bg: CellColor,
    pub whichkey_key_fg: CellColor,
    pub mode_search_fg: CellColor,
    pub mode_search_bg: CellColor,
    pub separator_fg: CellColor,
    /// `None` = inherit the border color (see [`ThemeConfig::pane_label_fg`]).
    pub pane_label_fg: Option<CellColor>,
    /// `None` = leave the terminal's background.
    pub pane_label_bg: Option<CellColor>,
    pub session_name_fg: CellColor,
    pub search_count_fg: CellColor,
    pub search_count_bg: CellColor,
    pub layout_indicator_fg: CellColor,
    pub layout_indicator_bg: CellColor,
    pub tab_bell_fg: CellColor,
    pub tab_activity_fg: CellColor,
    pub tab_silent_fg: CellColor,
    pub mode_unknown_fg: CellColor,
    pub mode_unknown_bg: CellColor,
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
            frame_bg: config.frame_bg.as_ref().map(ThemeColor::to_cell_color),
            frame_active_fg: config.frame_active_fg.to_cell_color(),
            status_bar_fg: config.status_bar_fg.to_cell_color(),
            status_bar_bg: config.status_bar_bg.to_cell_color(),
            tab_active_fg: config.tab_active_fg.to_cell_color(),
            tab_active_bg: config.tab_active_bg.to_cell_color(),
            tab_inactive_fg: config.tab_inactive_fg.to_cell_color(),
            tab_inactive_bg: config.tab_inactive_bg.to_cell_color(),
            whichkey_fg: config.whichkey_fg.to_cell_color(),
            whichkey_bg: config.whichkey_bg.to_cell_color(),
            whichkey_key_fg: config.whichkey_key_fg.to_cell_color(),
            mode_search_fg: config.mode_search_fg.to_cell_color(),
            mode_search_bg: config.mode_search_bg.to_cell_color(),
            separator_fg: config.separator_fg.to_cell_color(),
            pane_label_fg: config.pane_label_fg.as_ref().map(ThemeColor::to_cell_color),
            pane_label_bg: config.pane_label_bg.as_ref().map(ThemeColor::to_cell_color),
            session_name_fg: config.session_name_fg.to_cell_color(),
            search_count_fg: config.search_count_fg.to_cell_color(),
            search_count_bg: config.search_count_bg.to_cell_color(),
            layout_indicator_fg: config.layout_indicator_fg.to_cell_color(),
            layout_indicator_bg: config.layout_indicator_bg.to_cell_color(),
            tab_bell_fg: config.tab_bell_fg.to_cell_color(),
            tab_activity_fg: config.tab_activity_fg.to_cell_color(),
            tab_silent_fg: config.tab_silent_fg.to_cell_color(),
            mode_unknown_fg: config.mode_unknown_fg.to_cell_color(),
            mode_unknown_bg: config.mode_unknown_bg.to_cell_color(),
        }
    }

    /// Get foreground/background colors for the mode indicator.
    pub fn mode_colors(&self, mode: &str) -> (CellColor, CellColor) {
        match mode {
            "NORMAL" => (self.mode_normal_fg.clone(), self.mode_normal_bg.clone()),
            "COMMAND" => (self.mode_command_fg.clone(), self.mode_command_bg.clone()),
            "VISUAL" => (self.mode_visual_fg.clone(), self.mode_visual_bg.clone()),
            "SEARCH" => (self.mode_search_fg.clone(), self.mode_search_bg.clone()),
            _ => (self.mode_unknown_fg.clone(), self.mode_unknown_bg.clone()),
        }
    }

    /// The background a **border cell** should carry: `frame_bg` when the user
    /// set one, otherwise the terminal's default. The one accessor every border
    /// renderer goes through, so the client's View cells pick the role up for
    /// free through the shared drawing code.
    pub fn border_bg(&self) -> CellColor {
        self.frame_bg.clone().unwrap_or(CellColor::Default)
    }

    /// The `(fg, bg)` a **pane label** on a top border should carry, given the
    /// color that border is being drawn in. `pane_label_fg` defaults to the
    /// border color so the label keeps tracking focus; `pane_label_bg` defaults
    /// to the border background.
    pub fn label_colors(&self, border_fg: &CellColor) -> (CellColor, CellColor) {
        (
            self.pane_label_fg
                .clone()
                .unwrap_or_else(|| border_fg.clone()),
            self.pane_label_bg
                .clone()
                .unwrap_or_else(|| self.border_bg()),
        )
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

        // Frame colors. `frame_bg` is None by design: border cells have always
        // been drawn on the terminal's default background, and the role is
        // opt-in so wiring it up changed nobody's default appearance. It used to
        // default to `Rgb(30, 30, 46)` -- a value no renderer ever read.
        assert_eq!(ct.frame_fg, CellColor::Rgb(88, 91, 112)); // surface2
        assert_eq!(ct.frame_bg, None);
        assert_eq!(ct.border_bg(), CellColor::Default);
        assert_eq!(ct.frame_active_fg, CellColor::Rgb(137, 180, 250)); // blue

        // The pane label defaults to the border's own color/background, so it
        // keeps tracking focus the way it always has.
        assert_eq!(ct.pane_label_fg, None);
        assert_eq!(ct.pane_label_bg, None);
        let border = CellColor::Rgb(1, 2, 3);
        assert_eq!(
            ct.label_colors(&border),
            (border.clone(), CellColor::Default)
        );

        // Status bar
        assert_eq!(ct.status_bar_fg, CellColor::Rgb(166, 173, 200)); // subtext0
        assert_eq!(ct.status_bar_bg, CellColor::Rgb(24, 24, 37)); // mantle

        // Tabs
        assert_eq!(ct.tab_active_fg, CellColor::Rgb(30, 30, 46)); // base
        assert_eq!(ct.tab_active_bg, CellColor::Rgb(137, 180, 250)); // blue
        assert_eq!(ct.tab_inactive_fg, CellColor::Rgb(147, 153, 178)); // overlay2
        assert_eq!(ct.tab_inactive_bg, CellColor::Indexed(237));

        // Status-bar right-hand segments and activity markers: the named roles
        // must default to the literals the renderers used to hardcode.
        assert_eq!(ct.search_count_fg, CellColor::Indexed(0));
        assert_eq!(ct.search_count_bg, CellColor::Indexed(11));
        assert_eq!(ct.layout_indicator_fg, CellColor::Indexed(0));
        assert_eq!(ct.layout_indicator_bg, CellColor::Indexed(245));
        assert_eq!(ct.tab_bell_fg, CellColor::Indexed(9));
        assert_eq!(ct.tab_activity_fg, CellColor::Indexed(11));
        assert_eq!(ct.tab_silent_fg, CellColor::Indexed(10));
        assert_eq!(ct.mode_unknown_fg, CellColor::Indexed(15));
        assert_eq!(ct.mode_unknown_bg, CellColor::Indexed(238));

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
    fn optional_roles_round_trip_when_set() {
        // The three roles that used to be declared, documented and read by
        // NOBODY. Setting them must reach `CompositorTheme`'s accessors.
        let toml_str = r##"
            frame_bg = "#102030"
            pane_label_fg = "#405060"
            pane_label_bg = "#708090"
        "##;
        let config: ThemeConfig = toml::from_str(toml_str).unwrap();
        let ct = CompositorTheme::from_config(&config);
        assert_eq!(ct.border_bg(), CellColor::Rgb(0x10, 0x20, 0x30));
        assert_eq!(
            ct.label_colors(&CellColor::Indexed(7)),
            (
                CellColor::Rgb(0x40, 0x50, 0x60),
                CellColor::Rgb(0x70, 0x80, 0x90)
            )
        );
        // The crossterm-side mirror resolves them too.
        let t = Theme::from_config(&config);
        assert_eq!(
            t.frame_bg,
            Some(Color::Rgb {
                r: 0x10,
                g: 0x20,
                b: 0x30
            })
        );
        assert_eq!(
            t.pane_label_fg,
            Some(Color::Rgb {
                r: 0x40,
                g: 0x50,
                b: 0x60
            })
        );
    }

    #[test]
    fn label_bg_falls_back_to_frame_bg() {
        // `pane_label_bg` unset but `frame_bg` set: the label sits on the frame's
        // background rather than punching a default-colored hole in it.
        let config: ThemeConfig = toml::from_str(r##"frame_bg = "#010203""##).unwrap();
        let ct = CompositorTheme::from_config(&config);
        let (fg, bg) = ct.label_colors(&CellColor::Indexed(4));
        assert_eq!(fg, CellColor::Indexed(4)); // still the border color
        assert_eq!(bg, CellColor::Rgb(1, 2, 3));
    }

    #[test]
    fn named_colors_table_is_consistent_across_both_conversions() {
        // The regression this guards: two independent 16-arm tables, where a name
        // added to one silently fell through to the fallback in the other.
        for (name, idx, color) in NAMED_COLORS {
            let tc = ThemeColor::Named(name.to_string());
            assert_eq!(tc.to_cell_color(), CellColor::Indexed(*idx), "{name}");
            assert_eq!(tc.to_crossterm_color(), *color, "{name}");
            // Case-insensitive in both directions.
            let upper = ThemeColor::Named(name.to_uppercase());
            assert_eq!(upper.to_cell_color(), CellColor::Indexed(*idx), "{name}");
            assert_eq!(upper.to_crossterm_color(), *color, "{name}");
        }
        // "reset"/"default" are not in the table; each conversion expresses
        // "no color" in its own type.
        for name in ["reset", "default", "nonsense"] {
            let tc = ThemeColor::Named(name.to_string());
            assert_eq!(tc.to_cell_color(), CellColor::Default);
            assert_eq!(tc.to_crossterm_color(), Color::Reset);
        }
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
