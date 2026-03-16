pub mod keybindings;
pub mod theme;
pub mod watcher;

use serde::{Deserialize, Serialize};

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
    pub keybindings: KeybindingsConfig,
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
    /// When true (default), mouse text selection auto-copies to clipboard on
    /// release and clears the selection. When false, the selection stays visible
    /// for keyboard adjustment in Visual mode.
    pub mouse_auto_yank: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_shell: None,
            scrollback_lines: 10_000,
            auto_save_interval_secs: 30,
            mouse_auto_yank: true,
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
    pub status_bar_position: StatusBarPosition,
    pub border_style: BorderStyle,
    pub default_layout: DefaultLayout,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            status_bar_position: StatusBarPosition::Bottom,
            border_style: BorderStyle::ZellijStyle,
            default_layout: DefaultLayout::default(),
        }
    }
}

/// Default layout mode for new tabs.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DefaultLayout {
    #[default]
    Bsp,
    Master,
    Monocle,
    Custom,
}

impl DefaultLayout {
    /// Convert this config enum to the layout module's `LayoutMode`.
    pub fn to_layout_mode(&self) -> crate::server::layout::LayoutMode {
        use crate::server::layout::*;
        match self {
            DefaultLayout::Bsp => LayoutMode::Bsp(BspLayout),
            DefaultLayout::Master => LayoutMode::Master(MasterLayout::default()),
            DefaultLayout::Monocle => LayoutMode::Monocle(MonocleLayout),
            DefaultLayout::Custom => LayoutMode::Custom(CustomLayout),
        }
    }
}

/// Border rendering style for pane frames.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BorderStyle {
    ZellijStyle,
    TmuxStyle,
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
    pub command: CommandModeConfig,
}

#[allow(clippy::derivable_impls)]
impl Default for ModesConfig {
    fn default() -> Self {
        Self {
            command: CommandModeConfig::default(),
        }
    }
}

/// Configuration specific to Command mode.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct CommandModeConfig {
    /// Timeout in milliseconds before the which-key popup appears.
    pub timeout_ms: u64,
}

impl Default for CommandModeConfig {
    fn default() -> Self {
        Self { timeout_ms: 500 }
    }
}

// ---------------------------------------------------------------------------
// Keybindings
// ---------------------------------------------------------------------------

/// Per-mode keybinding configuration.
///
/// Example `config.toml`:
/// ```toml
/// [keybindings.command]
/// leader = "Ctrl-a"
///
/// [keybindings.command.t]
/// _label = "Tab"
/// n = "TabNew; EnterNormal"
/// c = "TabClose; EnterNormal"
/// r = "TabRename"
/// ```
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// Command mode keybinding overrides (tree-based).
    pub command: toml::Value,
    /// Visual mode keybinding overrides (tree-based).
    pub visual: toml::Value,
    /// Deprecated: `[keybindings.normal]` is accepted as an alias for `command`.
    pub normal: toml::Value,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            command: toml::Value::Table(toml::map::Map::new()),
            visual: toml::Value::Table(toml::map::Map::new()),
            normal: toml::Value::Table(toml::map::Map::new()),
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
            keybindings: KeybindingsConfig::default(),
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
    ///
    /// Supports both `[keybindings.command]` and the deprecated
    /// `[keybindings.normal]` section (with a warning).
    pub fn keybinding_tree(&self) -> keybindings::KeybindingTree {
        let mut tree = keybindings::KeybindingTree::default();

        // Check for deprecated [keybindings.normal] first.
        if let Some(table) = self.keybindings.normal.as_table() {
            if !table.is_empty() {
                log::warn!("[keybindings.normal] is deprecated; use [keybindings.command] instead");
                if let Some(user_tree) =
                    keybindings::KeybindingTree::from_toml(&self.keybindings.normal)
                {
                    tree.merge(&user_tree);
                }
            }
        }

        // Then merge [keybindings.command] on top (takes priority).
        if let Some(table) = self.keybindings.command.as_table() {
            if !table.is_empty() {
                if let Some(user_tree) =
                    keybindings::KeybindingTree::from_toml(&self.keybindings.command)
                {
                    tree.merge(&user_tree);
                }
            }
        }

        tree
    }

    /// Build the effective shortcut bindings by starting from defaults
    /// and merging any user-defined overrides from `[keybindings.command]`.
    pub fn shortcut_bindings(&self) -> keybindings::ShortcutBindings {
        let mut bindings = keybindings::ShortcutBindings::default();
        if let Some(table) = self.keybindings.command.as_table() {
            if !table.is_empty() {
                if let Some(user_bindings) =
                    keybindings::ShortcutBindings::from_toml(&self.keybindings.command)
                {
                    bindings.merge(&user_bindings);
                }
            }
        }
        bindings
    }

    /// Validate cross-references between config sections.
    /// Logs errors for invalid references. Returns true if valid.
    pub fn validate(&self) -> bool {
        let tree = self.keybinding_tree();
        let shortcuts = self.shortcut_bindings();
        shortcuts.validate_group_refs(&tree)
    }

    /// Parse the leader key from the config.
    ///
    /// Looks in `[keybindings.command]` for a `leader` key. Falls back to
    /// `[keybindings.normal]` for backward compatibility. Defaults to Ctrl-a.
    pub fn leader_key(&self) -> crossterm::event::KeyEvent {
        // Check [keybindings.command] first.
        if let Some(table) = self.keybindings.command.as_table() {
            if table.contains_key("leader") {
                return keybindings::parse_leader_key(table);
            }
        }
        // Fall back to deprecated [keybindings.normal].
        if let Some(table) = self.keybindings.normal.as_table() {
            if table.contains_key("leader") {
                return keybindings::parse_leader_key(table);
            }
        }
        keybindings::default_leader_key()
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
        assert_eq!(config.modes.command.timeout_ms, 500);
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

            [appearance]
            status_bar_position = "top"

            [modes.command]
            timeout_ms = 300

            [keybindings.command.t]
            _label = "Tab"
            n = "TabNew; EnterNormal"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.general.default_shell, Some("/bin/zsh".to_string()));
        assert_eq!(config.general.scrollback_lines, 20_000);
        assert_eq!(
            config.appearance.status_bar_position,
            StatusBarPosition::Top
        );
        assert_eq!(config.modes.command.timeout_ms, 300);
    }

    #[test]
    fn keybinding_tree_merges_user_overrides() {
        let toml_str = r#"
            [keybindings.command.t]
            _label = "Tab"
            x = "TabExtra"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let tree = config.keybinding_tree();
        // Default 'n' for tab should still exist.
        assert!(tree.lookup(&['t', 'n']).is_some());
        // User-added 'x' should also exist.
        assert!(tree.lookup(&['t', 'x']).is_some());
    }

    #[test]
    fn keybinding_tree_deprecated_normal_still_works() {
        let toml_str = r#"
            [keybindings.normal.t]
            _label = "Tab"
            x = "TabExtra"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let tree = config.keybinding_tree();
        // User-added 'x' via deprecated [keybindings.normal] should work.
        assert!(tree.lookup(&['t', 'x']).is_some());
    }

    #[test]
    fn keybinding_tree_default_when_no_overrides() {
        let config = Config::default();
        let tree = config.keybinding_tree();
        assert!(tree.lookup(&['t', 'n']).is_some());
    }

    #[test]
    fn default_border_style_settings() {
        let config = Config::default();
        assert_eq!(config.appearance.border_style, BorderStyle::ZellijStyle);
    }

    #[test]
    fn deserialize_border_style_zellij_style() {
        let toml_str = r#"
            [appearance]
            border_style = "zellij_style"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.appearance.border_style, BorderStyle::ZellijStyle);
    }

    #[test]
    fn deserialize_border_style_tmux_style() {
        let toml_str = r#"
            [appearance]
            border_style = "tmux_style"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.appearance.border_style, BorderStyle::TmuxStyle);
    }

    #[test]
    fn default_appearance_has_zellij_style() {
        let appearance = AppearanceConfig::default();
        assert_eq!(appearance.border_style, BorderStyle::ZellijStyle);
        let AppearanceConfig {
            status_bar_position: _,
            border_style,
            default_layout: _,
        } = &appearance;
        assert_eq!(*border_style, BorderStyle::ZellijStyle);
    }

    #[test]
    fn load_returns_default_when_no_file() {
        let config = Config::load().unwrap();
        assert_eq!(config.general.scrollback_lines, 10_000);
    }

    #[test]
    fn leader_key_default() {
        let config = Config::default();
        let leader = config.leader_key();
        assert_eq!(leader.code, crossterm::event::KeyCode::Char('a'));
        assert_eq!(leader.modifiers, crossterm::event::KeyModifiers::CONTROL);
    }

    #[test]
    fn leader_key_from_command_section() {
        let toml_str = r#"
            [keybindings.command]
            leader = "Ctrl-b"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let leader = config.leader_key();
        assert_eq!(leader.code, crossterm::event::KeyCode::Char('b'));
        assert_eq!(leader.modifiers, crossterm::event::KeyModifiers::CONTROL);
    }
}
