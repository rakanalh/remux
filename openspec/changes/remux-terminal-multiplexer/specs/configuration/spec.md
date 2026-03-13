## ADDED Requirements

### Requirement: TOML configuration file
The system SHALL load configuration from ~/.config/remux/config.toml. If the file does not exist, the system SHALL use built-in defaults.

#### Scenario: Config file exists
- **WHEN** the server starts and config.toml exists
- **THEN** settings from the file override the built-in defaults

#### Scenario: No config file
- **WHEN** the server starts and config.toml does not exist
- **THEN** the system runs with built-in defaults

### Requirement: Keybinding configuration
The user SHALL be able to define keybinding groups and mappings under [modes.normal.keys] in the TOML config. The system SHALL merge user config with defaults (user overrides take precedence).

#### Scenario: User adds custom group
- **WHEN** the user defines [modes.normal.keys.x] with _label = "Custom"
- **THEN** pressing 'x' in Normal mode opens the "Custom" group

#### Scenario: User overrides default
- **WHEN** the user remaps a default keybinding
- **THEN** the user's mapping takes precedence

### Requirement: Appearance configuration
The user SHALL be able to configure: frame style ("framed" or "minimal"), status bar position, mode indicator colors, and the which-key popup timeout.

#### Scenario: Set frame style
- **WHEN** the user sets frame_style = "minimal" in config
- **THEN** pane borders are hidden and a tmux-style status bar is shown

### Requirement: Behavior configuration
The user SHALL be able to configure: default shell, scrollback limit, auto-save interval, and mode-switch key.

#### Scenario: Custom scrollback limit
- **WHEN** the user sets scrollback_lines = 50000 in config
- **THEN** each pane's scrollback buffer stores up to 50,000 lines

#### Scenario: Custom mode-switch key
- **WHEN** the user sets mode_switch_key = "Ctrl-Space" in config
- **THEN** Ctrl-Space switches from Insert to Normal mode instead of Escape

### Requirement: Config reload
The system SHALL watch the config file for changes and reload it without restarting. Keybinding and appearance changes SHALL take effect immediately.

#### Scenario: Config file modified
- **WHEN** the user saves a change to config.toml while remux is running
- **THEN** the new settings are applied without restarting the server
