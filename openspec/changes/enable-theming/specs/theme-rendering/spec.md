## ADDED Requirements

### Requirement: Compositor receives theme
The `composite()` function SHALL accept a theme parameter. All drawing functions (`draw_zellij_panes`, `draw_tmux_panes`, `draw_status_bar`) SHALL receive the theme and use it for all UI color decisions.

#### Scenario: Theme parameter threaded through
- **WHEN** `composite()` is called
- **THEN** it SHALL pass the theme to all internal drawing functions

### Requirement: Border colors from theme
Pane border colors SHALL be read from the theme instead of hardcoded values. Active pane borders SHALL use the theme's active frame color. Inactive pane borders SHALL use the theme's frame color.

#### Scenario: Active pane border uses theme
- **WHEN** a pane is focused and borders are drawn
- **THEN** the border color SHALL be the theme's `frame_active_fg` (currently green/`Indexed(2)`)

#### Scenario: Inactive pane border uses theme
- **WHEN** a pane is not focused and borders are drawn
- **THEN** the border color SHALL be the theme's `frame_fg` (currently dark grey/`Indexed(8)`)

### Requirement: Mode indicator colors from theme
The mode indicator label in both status bar and pane headers SHALL use theme colors for each mode (INSERT, NORMAL, VISUAL).

#### Scenario: Insert mode indicator
- **WHEN** the mode is INSERT
- **THEN** the indicator SHALL use `mode_insert_fg` on `mode_insert_bg` from the theme

#### Scenario: Normal mode indicator
- **WHEN** the mode is NORMAL
- **THEN** the indicator SHALL use `mode_normal_fg` on `mode_normal_bg` from the theme

#### Scenario: Visual mode indicator
- **WHEN** the mode is VISUAL
- **THEN** the indicator SHALL use `mode_visual_fg` on `mode_visual_bg` from the theme

### Requirement: Status bar colors from theme
The status bar background, session name, tab labels, and separators SHALL all use theme colors.

#### Scenario: Status bar background
- **WHEN** the status bar is rendered
- **THEN** it SHALL use the theme's `status_bar_bg` for the background fill

#### Scenario: Active tab label
- **WHEN** a tab is active in the status bar
- **THEN** it SHALL use `tab_active_fg` on `tab_active_bg` from the theme

#### Scenario: Inactive tab label
- **WHEN** a tab is inactive in the status bar
- **THEN** it SHALL use `tab_inactive_fg` on `status_bar_bg` from the theme

#### Scenario: Session name color
- **WHEN** the session name is rendered in the status bar
- **THEN** it SHALL use `session_name_fg` on `status_bar_bg` from the theme

### Requirement: Separator and divider colors from theme
All separator characters (`│`, `|`) between UI elements SHALL use the theme's `separator_fg` color.

#### Scenario: Tab separator color
- **WHEN** separators are drawn between tabs
- **THEN** they SHALL use `separator_fg` from the theme

#### Scenario: Tmux-style divider color
- **WHEN** tmux-style dividers are drawn between panes
- **THEN** they SHALL use `separator_fg` from the theme

### Requirement: Pane label colors from theme
Pane name labels shown in border headers or stack tab bars SHALL use theme colors.

#### Scenario: Active pane label
- **WHEN** a pane label is shown for the active pane
- **THEN** it SHALL use mode-based colors from the theme (matching mode indicator colors)

#### Scenario: Inactive pane label in stack
- **WHEN** an inactive pane label is shown in a stack header
- **THEN** it SHALL use `tab_inactive_fg` on the stack header background from the theme

### Requirement: Default theme produces identical output
When no theme is configured, the compositor's output SHALL be visually identical to the current hardcoded rendering.

#### Scenario: Unchanged appearance with defaults
- **WHEN** Remux starts with no `[appearance.theme]` in config
- **THEN** all colors SHALL match the current hardcoded values exactly
