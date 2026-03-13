pub mod keybindings;
pub mod theme;
pub mod watcher;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Config root
// ---------------------------------------------------------------------------

/// Top-level Remux configuration, loaded from `~/.config/remux/config.toml`.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub appearance: AppearanceConfig,
    pub modes: ModesConfig,
}

// ---------------------------------------------------------------------------
// General
// ---------------------------------------------------------------------------

/// General settings that affect the overall behaviour of Remux.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Override the default login shell (`$SHELL` is used if `None`).
    pub default_shell: Option<String>,
    /// Maximum number of scrollback lines per pane.
    pub scrollback_lines: usize,
    /// Interval in seconds between automatic session saves.
    pub auto_save_interval_secs: u64,
    /// Key used to switch from Insert to Normal mode (e.g. `"Esc"`).
    pub mode_switch_key: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_shell: None,
            scrollback_lines: 10_000,
            auto_save_interval_secs: 30,
            mode_switch_key: "Esc".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Appearance
// ---------------------------------------------------------------------------

/// Visual appearance settings.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub frame_style: FrameStyle,
    pub status_bar_position: StatusBarPosition,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            frame_style: FrameStyle::Framed,
            status_bar_position: StatusBarPosition::Bottom,
        }
    }
}

/// How pane borders are drawn.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FrameStyle {
    Framed,
    Minimal,
}

/// Where the status bar is placed.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusBarPosition {
    Top,
    Bottom,
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

/// Per-mode configuration.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ModesConfig {
    pub normal: NormalModeConfig,
}

#[allow(clippy::derivable_impls)]
impl Default for ModesConfig {
    fn default() -> Self {
        Self {
            normal: NormalModeConfig::default(),
        }
    }
}

/// Configuration specific to Normal mode.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NormalModeConfig {
    /// Timeout in milliseconds before the which-key popup appears.
    pub timeout_ms: u64,
    /// Raw TOML value for keybinding definitions. Parsed separately by
    /// [`keybindings::KeybindingTree::from_toml`].
    pub keys: toml::Value,
}

impl Default for NormalModeConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 500,
            keys: toml::Value::Table(toml::map::Map::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Default for the root Config
// ---------------------------------------------------------------------------

#[allow(clippy::derivable_impls)]
impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            appearance: AppearanceConfig::default(),
            modes: ModesConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

impl Config {
    /// Load the configuration from `~/.config/remux/config.toml`.
    ///
    /// If the file does not exist, returns the default configuration.
    /// If the file exists but contains invalid TOML, returns an error.
    pub fn load() -> anyhow::Result<Self> {
        let config_path = match dirs::config_dir() {
            Some(dir) => dir.join("remux").join("config.toml"),
            None => return Ok(Self::default()),
        };

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&config_path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Return the theme for the current configuration.
    ///
    /// Currently returns the default theme; theme customization can be added
    /// later.
    pub fn theme(&self) -> theme::Theme {
        theme::Theme::default()
    }

    /// Build the effective keybinding tree by starting from defaults and
    /// merging any user-defined overrides from the config file.
    pub fn keybinding_tree(&self) -> keybindings::KeybindingTree {
        let mut tree = keybindings::KeybindingTree::default();

        // If the user provided keybinding overrides, parse and merge them.
        if let Some(table) = self.modes.normal.keys.as_table() {
            if !table.is_empty() {
                if let Some(user_tree) =
                    keybindings::KeybindingTree::from_toml(&self.modes.normal.keys)
                {
                    tree.merge(&user_tree);
                }
            }
        }

        tree
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let config = Config::default();
        assert_eq!(config.general.scrollback_lines, 10_000);
        assert_eq!(config.general.auto_save_interval_secs, 30);
        assert_eq!(config.general.mode_switch_key, "Esc");
        assert_eq!(config.modes.normal.timeout_ms, 500);
        assert_eq!(config.appearance.frame_style, FrameStyle::Framed);
        assert_eq!(
            config.appearance.status_bar_position,
            StatusBarPosition::Bottom
        );
        assert!(config.general.default_shell.is_none());
    }

    #[test]
    fn deserialize_partial_config() {
        let toml_str = r#"
            [general]
            scrollback_lines = 5000
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.general.scrollback_lines, 5000);
        // Other values should be defaults.
        assert_eq!(config.general.auto_save_interval_secs, 30);
    }

    #[test]
    fn deserialize_full_config() {
        let toml_str = r#"
            [general]
            default_shell = "/bin/zsh"
            scrollback_lines = 20000
            auto_save_interval_secs = 60
            mode_switch_key = "Esc"

            [appearance]
            frame_style = "minimal"
            status_bar_position = "top"

            [modes.normal]
            timeout_ms = 300

            [modes.normal.keys.t]
            _label = "Tab"
            n = "tab:new"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.general.default_shell, Some("/bin/zsh".to_string()));
        assert_eq!(config.general.scrollback_lines, 20_000);
        assert_eq!(config.appearance.frame_style, FrameStyle::Minimal);
        assert_eq!(
            config.appearance.status_bar_position,
            StatusBarPosition::Top
        );
        assert_eq!(config.modes.normal.timeout_ms, 300);
    }

    #[test]
    fn keybinding_tree_merges_user_overrides() {
        let toml_str = r#"
            [modes.normal.keys.t]
            _label = "Tab"
            x = "tab:extra"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let tree = config.keybinding_tree();
        // Default 'n' for tab should still exist.
        assert!(tree.lookup(&['t', 'n']).is_some());
        // User-added 'x' should also exist.
        assert!(tree.lookup(&['t', 'x']).is_some());
    }

    #[test]
    fn keybinding_tree_default_when_no_overrides() {
        let config = Config::default();
        let tree = config.keybinding_tree();
        assert!(tree.lookup(&['t', 'n']).is_some());
    }

    #[test]
    fn load_returns_default_when_no_file() {
        let config = Config::load().unwrap();
        assert_eq!(config.general.scrollback_lines, 10_000);
    }
}
