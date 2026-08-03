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
    /// Named remote servers reachable over SSH, keyed by a short label used in
    /// the session manager tree. Configured via `[remotes.<name>]` tables.
    pub remotes: std::collections::HashMap<String, RemoteConfig>,
}

// ---------------------------------------------------------------------------
// Remotes
// ---------------------------------------------------------------------------

/// Configuration for a single remote Remux server reachable over SSH.
///
/// Example `config.toml`:
/// ```toml
/// [remotes.pi]
/// ssh = "pi@raspberrypi.local"
/// remux_path = "/usr/local/bin/remux"
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    /// SSH destination, e.g. `"user@host"` (relies on `~/.ssh/config`).
    pub ssh: String,
    /// Optional SSH port (`-p`).
    pub port: Option<u16>,
    /// Optional identity file (`-i`).
    pub identity: Option<String>,
    /// Path to the `remux` binary on the remote host.
    pub remux_path: String,
    /// Extra arguments passed to `ssh` before the destination.
    pub extra_args: Vec<String>,
    /// When true, connect this remote automatically at client startup (instead
    /// of lazily on demand).
    pub auto_connect: bool,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            ssh: String::new(),
            port: None,
            identity: None,
            remux_path: "remux".to_string(),
            extra_args: Vec::new(),
            auto_connect: false,
        }
    }
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
    /// When true (default), the server persists session state to disk (after
    /// every structural change and on shutdown). When false, no persistence
    /// happens at all -- sessions are never written to disk and
    /// `automatic_restore` is ignored.
    pub save_sessions: bool,
    /// When true (default), automatically restore persisted sessions live on
    /// startup. When false (and `save_sessions` is true), persisted sessions
    /// are loaded as dormant/resurrectable instead of being brought live, and
    /// can be materialized on demand from the session manager.
    pub automatic_restore: bool,
    /// When true (default), mouse text selection auto-copies to clipboard on
    /// release and clears the selection. When false, the selection stays visible
    /// for keyboard adjustment in Visual mode.
    pub mouse_auto_yank: bool,
    /// When true (default), an application running in a pane may put text on the
    /// user's system clipboard by emitting `OSC 52` — how editors, pagers and
    /// TUI tools copy. Set it false to let nothing but the user's own selections
    /// reach the clipboard. Consulted server-side, so for a remote session it is
    /// the *remote* server's setting that applies. Clipboard *reads* are never
    /// served, whatever this is set to.
    pub allow_app_clipboard: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_shell: None,
            scrollback_lines: 10_000,
            save_sessions: true,
            automatic_restore: true,
            mouse_auto_yank: true,
            allow_app_clipboard: true,
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
    pub theme: theme::ThemeConfig,
    /// Placement style for the which-key hint popup.
    pub which_key_position: WhichKeyPosition,
    /// Width of the popup terminal as a percentage of the session's content
    /// area. Clamped to `POPUP_MIN_PCT..=POPUP_MAX_PCT` (20..=100) when a
    /// session is created, and adjustable at runtime with the resize commands.
    pub popup_width_pct: u8,
    /// Height of the popup terminal as a percentage of the session's content
    /// area. Clamped like `popup_width_pct`.
    pub popup_height_pct: u8,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            status_bar_position: StatusBarPosition::Bottom,
            border_style: BorderStyle::ZellijStyle,
            default_layout: DefaultLayout::default(),
            theme: theme::ThemeConfig::default(),
            which_key_position: WhichKeyPosition::default(),
            popup_width_pct: 80,
            popup_height_pct: 80,
        }
    }
}

/// Placement style for the which-key hint popup.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WhichKeyPosition {
    /// A bordered box centered horizontally and anchored to the bottom of the
    /// screen (the historical default).
    #[default]
    Anchored,
    /// The same bordered box, centered both horizontally and vertically.
    Centered,
    /// An emacs/ivy-like panel spanning the full terminal width, anchored to
    /// the bottom above the status bar row.
    FullWidth,
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
    /// Session-manager chord overrides (`chord = "ActionName"`). Absent = the
    /// built-in defaults; user entries override/extend them.
    pub session_manager: toml::Value,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            command: toml::Value::Table(toml::map::Map::new()),
            visual: toml::Value::Table(toml::map::Map::new()),
            normal: toml::Value::Table(toml::map::Map::new()),
            session_manager: toml::Value::Table(toml::map::Map::new()),
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
            remotes: std::collections::HashMap::new(),
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
    pub fn theme(&self) -> theme::Theme {
        theme::Theme::from_config(&self.appearance.theme)
    }

    /// Return the compositor theme for the current configuration.
    pub fn compositor_theme(&self) -> theme::CompositorTheme {
        theme::CompositorTheme::from_config(&self.appearance.theme)
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

    /// Build the effective session-manager chord bindings, starting from the
    /// built-in defaults and applying any `[keybindings.session_manager]`
    /// overrides. When the section is absent this yields the defaults.
    pub fn session_manager_bindings(&self) -> keybindings::SessionManagerBindings {
        keybindings::SessionManagerBindings::from_toml(&self.keybindings.session_manager)
    }

    /// Validate cross-references between config sections.
    /// Logs errors for invalid references. Returns true if valid.
    pub fn validate(&self) -> bool {
        let mut valid = true;
        for problem in self.binding_problems() {
            log::error!("{problem}");
            valid = false;
        }

        // Not `&&`: both checks must run so both get logged.
        let tree = self.keybinding_tree();
        let shortcuts = self.shortcut_bindings();
        let groups_valid = shortcuts.validate_group_refs(&tree);
        valid && groups_valid
    }

    /// Every keybinding whose action string does not resolve, as a
    /// human-readable message naming the binding and the offending name.
    ///
    /// Split out from [`Config::validate`] so tests can assert on the messages
    /// directly rather than scraping the log.
    pub fn binding_problems(&self) -> Vec<String> {
        keybindings::unresolved_actions(&self.keybinding_tree(), &self.shortcut_bindings())
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
        assert!(config.general.automatic_restore);
        assert!(config.general.save_sessions);
        assert_eq!(config.modes.command.timeout_ms, 500);
        assert_eq!(
            config.appearance.status_bar_position,
            StatusBarPosition::Bottom
        );
        assert!(config.general.default_shell.is_none());
    }

    #[test]
    fn default_which_key_position_is_anchored() {
        let config = Config::default();
        assert_eq!(
            config.appearance.which_key_position,
            WhichKeyPosition::Anchored
        );
    }

    #[test]
    fn deserialize_which_key_position_full_width() {
        let toml_str = r#"
            [appearance]
            which_key_position = "full_width"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.appearance.which_key_position,
            WhichKeyPosition::FullWidth
        );
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
        assert!(config.general.automatic_restore);
        assert!(config.general.save_sessions);
    }

    #[test]
    fn deserialize_save_sessions_false() {
        let toml_str = r#"
            [general]
            save_sessions = false
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.general.save_sessions);
        // automatic_restore keeps its default when unspecified.
        assert!(config.general.automatic_restore);
    }

    #[test]
    fn deserialize_full_config() {
        let toml_str = r#"
            [general]
            default_shell = "/bin/zsh"
            scrollback_lines = 20000
            automatic_restore = false

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
            theme: _,
            which_key_position: _,
            popup_width_pct: _,
            popup_height_pct: _,
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
    fn deserialize_remotes_config() {
        let toml_str = r#"
            [remotes.pi]
            ssh = "pi@raspberrypi.local"
            remux_path = "/usr/local/bin/remux"

            [remotes.server]
            ssh = "user@example.com"
            port = 2222
            identity = "~/.ssh/id_ed25519"
            extra_args = ["-o", "StrictHostKeyChecking=no"]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.remotes.len(), 2);

        let pi = &config.remotes["pi"];
        assert_eq!(pi.ssh, "pi@raspberrypi.local");
        assert_eq!(pi.remux_path, "/usr/local/bin/remux");
        assert!(pi.port.is_none());
        assert!(pi.extra_args.is_empty());

        let server = &config.remotes["server"];
        assert_eq!(server.port, Some(2222));
        assert_eq!(server.identity.as_deref(), Some("~/.ssh/id_ed25519"));
        // remux_path defaults to "remux" when omitted.
        assert_eq!(server.remux_path, "remux");
        assert_eq!(server.extra_args, vec!["-o", "StrictHostKeyChecking=no"]);
    }

    #[test]
    fn default_config_has_no_remotes() {
        let config = Config::default();
        assert!(config.remotes.is_empty());
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

    // -- Binding validation at config load ------------------------------------

    /// The shipped defaults are clean: loading a config with no keybinding
    /// overrides reports nothing and validates.
    #[test]
    fn valid_config_loads_silently() {
        let config = Config::default();
        assert!(config.binding_problems().is_empty());
        assert!(config.validate());

        // A config that overrides bindings with REAL action names is equally
        // silent -- including the client-only ones.
        let toml_str = r#"
            [keybindings.command]
            "Alt-y" = "PaneFocusRight"

            [keybindings.command.w]
            n = "ViewNew"
            g = "TabNew; EnterNormal"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(
            config.binding_problems().is_empty(),
            "{:#?}",
            config.binding_problems()
        );
        assert!(config.validate());
    }

    /// A typo'd binding is reported at config load, naming the binding and the
    /// bad action -- instead of silently doing nothing when the key is pressed.
    #[test]
    fn typo_in_binding_is_reported_at_load() {
        let toml_str = r#"
            [keybindings.command]
            "Alt-y" = "PaneFocusRigth"

            [keybindings.command.w]
            n = "ViewNwe"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();

        let problems = config.binding_problems();
        assert_eq!(problems.len(), 2, "{problems:#?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("binding 'w n'") && p.contains("unknown action 'ViewNwe'")),
            "{problems:#?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("shortcut 'Alt-y'")
                    && p.contains("unknown action 'PaneFocusRigth'")),
            "{problems:#?}"
        );
        // `validate` logs each problem and reports the config as invalid.
        assert!(!config.validate());
    }

    /// `config.sample.toml` is the user-facing list of what can be bound, so
    /// it is one more thing that can drift away from the registry. Every
    /// bindable action must be documented there.
    ///
    /// A substring check, so a name contained in a longer documented name
    /// (`ViewClose` inside a hypothetical `ViewCloseAll`) would pass without
    /// its own entry. Good enough to catch a whole action going undocumented,
    /// which is the drift that actually happens.
    #[test]
    fn sample_config_documents_every_action() {
        let sample = include_str!("../../config.sample.toml");
        let undocumented: Vec<&str> = crate::protocol::action_specs()
            .iter()
            .map(|spec| spec.name)
            .filter(|name| !sample.contains(name))
            .collect();
        assert!(
            undocumented.is_empty(),
            "config.sample.toml does not document: {undocumented:?}"
        );
    }

    /// Bad action names and bad group references are reported together: one
    /// does not mask the other.
    #[test]
    fn validate_reports_bad_actions_and_bad_group_refs_together() {
        let toml_str = r#"
            [keybindings.command]
            "Alt-y" = "PaneFocusRigth"
            "Alt-u" = "@Z"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.binding_problems().len(), 1);
        assert!(!config.validate());
    }
}
