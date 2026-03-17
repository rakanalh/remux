## ADDED Requirements

### Requirement: ThemeColor serde type
The system SHALL provide a `ThemeColor` enum that can be deserialized from TOML and converted to both `crossterm::style::Color` and `CellColor`. It SHALL support three formats: named colors (e.g. `"red"`), 256-color index (`{ ansi = N }`), and true-color (`{ rgb = [R, G, B] }`).

#### Scenario: Deserialize named color
- **WHEN** the TOML value is a string like `"green"`
- **THEN** it SHALL parse into a `ThemeColor` representing that named color

#### Scenario: Deserialize indexed color
- **WHEN** the TOML value is `{ ansi = 235 }`
- **THEN** it SHALL parse into a `ThemeColor::Indexed(235)`

#### Scenario: Deserialize RGB color
- **WHEN** the TOML value is `{ rgb = [255, 128, 0] }`
- **THEN** it SHALL parse into a `ThemeColor::Rgb(255, 128, 0)`

#### Scenario: Convert to crossterm Color
- **WHEN** a `ThemeColor` is converted to `crossterm::style::Color`
- **THEN** named colors SHALL map to the corresponding `Color::*` variant, indexed to `Color::AnsiValue`, and RGB to `Color::Rgb`

#### Scenario: Convert to CellColor
- **WHEN** a `ThemeColor` is converted to `CellColor`
- **THEN** named colors SHALL map to their ANSI index, indexed to `CellColor::Indexed`, and RGB to `CellColor::Rgb`

### Requirement: ThemeConfig struct with serde support
The system SHALL provide a `ThemeConfig` struct with `Deserialize` that contains all themeable UI color fields. All fields SHALL have defaults matching the current hardcoded values.

#### Scenario: All theme fields present
- **WHEN** a user provides a complete `[appearance.theme]` section
- **THEN** all color fields SHALL be loaded from the config

#### Scenario: Partial theme config
- **WHEN** a user provides only some theme fields (e.g. only `mode_insert_bg`)
- **THEN** unspecified fields SHALL use their default values

#### Scenario: No theme section
- **WHEN** the config file has no `[appearance.theme]` section
- **THEN** the default theme SHALL be used and rendering SHALL be identical to the current hardcoded behavior

### Requirement: Theme fields cover all UI elements
The `ThemeConfig` SHALL include color fields for: mode indicators (insert/normal/visual fg+bg), pane frame borders (active fg, inactive fg, bg), status bar (fg, bg), tab labels (active fg+bg, inactive fg), which-key popup (fg, bg, key highlight fg), separator/divider fg, pane label fg+bg, and session name fg.

#### Scenario: Theme fields match compositor colors
- **WHEN** using the default theme
- **THEN** every hardcoded `CellColor::Indexed(...)` in the compositor SHALL have a corresponding theme field that produces the same color value

### Requirement: Config loads theme
`Config::theme()` SHALL construct a `Theme` from the `ThemeConfig` in `AppearanceConfig`. The `ThemeConfig` SHALL be stored under `appearance.theme` in the config hierarchy.

#### Scenario: Theme from config
- **WHEN** `Config::load()` is called with a config file containing `[appearance.theme]`
- **THEN** `config.theme()` SHALL return a theme reflecting those values

#### Scenario: Theme defaults
- **WHEN** `Config::load()` is called without a `[appearance.theme]` section
- **THEN** `config.theme()` SHALL return the default theme

### Requirement: Sample config documents theme
`config.sample.toml` SHALL include a commented-out `[appearance.theme]` section listing all theme keys with their default values.

#### Scenario: Sample config completeness
- **WHEN** a user reads `config.sample.toml`
- **THEN** they SHALL see every available theme key with its default value and a brief comment
