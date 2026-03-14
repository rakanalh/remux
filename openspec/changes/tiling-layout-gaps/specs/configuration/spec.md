## MODIFIED Requirements

### Requirement: GapMode configuration
The user SHALL be able to configure `gap_mode` in the `[appearance]` section with values `zellij_style` or `tmux_style`. The default gap mode SHALL be `ZellijStyle`. The `gap_size` field SHALL control the gap width in cells (default 1) and only takes effect in `ZellijStyle` mode.

#### Scenario: Set gap mode to ZellijStyle
- **WHEN** the user sets `gap_mode = "zellij_style"` in the `[appearance]` section
- **THEN** new sessions default to `ZellijStyle` mode with full box borders, pane names, and configurable gap spacing

#### Scenario: Set gap mode to TmuxStyle
- **WHEN** the user sets `gap_mode = "tmux_style"` in the `[appearance]` section
- **THEN** new sessions default to `TmuxStyle` mode with minimal dividers and edge-to-edge content

#### Scenario: Default gap mode when not configured
- **WHEN** the user does not specify `gap_mode` in config
- **THEN** gap mode defaults to `ZellijStyle`

#### Scenario: Set gap size
- **WHEN** the user sets `gap_size = 3` in the `[appearance]` section
- **THEN** 3 cells of gap space appear between adjacent panes when gap mode is `ZellijStyle`

#### Scenario: Default gap size
- **WHEN** the user does not specify `gap_size` in config
- **THEN** gap size defaults to 1 cell

### Requirement: FrameStyle removal
The `FrameStyle` enum and the `frame_style` field SHALL be removed from `AppearanceConfig`. The system SHALL NOT accept `frame_style` as a configuration key. `GapMode` SHALL be the sole control for rendering mode, encompassing both border style and gap behavior.

#### Scenario: Config without frame_style
- **WHEN** the user provides a config file without a `frame_style` field
- **THEN** the config deserializes successfully using only `gap_mode` for rendering mode

#### Scenario: Config with old frame_style field
- **WHEN** the user provides a config file that still contains a `frame_style` field
- **THEN** the field is ignored (unknown field) and `gap_mode` alone determines rendering behavior

### Requirement: Appearance configuration
The user SHALL be able to configure: gap mode (`zellij_style` or `tmux_style`), gap size (in cells), status bar position, mode indicator colors, and the which-key popup timeout. The `frame_style` option SHALL NOT be available.

#### Scenario: Full appearance config
- **WHEN** the user sets `gap_mode = "tmux_style"` and `gap_size = 2` in the `[appearance]` section
- **THEN** the appearance config is deserialized with `TmuxStyle` mode and gap size 2

#### Scenario: Minimal appearance config
- **WHEN** the user provides an `[appearance]` section with no fields
- **THEN** all appearance settings use their defaults: `ZellijStyle` gap mode, gap size 1
