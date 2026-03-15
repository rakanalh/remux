## MODIFIED Requirements

### Requirement: Appearance configuration
The user SHALL be able to configure: frame style ("framed" or "minimal"), status bar position, mode indicator colors, the which-key popup timeout, and the default layout mode for new tabs.

#### Scenario: Set frame style
- **WHEN** the user sets frame_style = "minimal" in config
- **THEN** pane borders are hidden and a tmux-style status bar is shown

#### Scenario: Set default layout mode
- **WHEN** the user sets `default_layout = "master"` in config
- **THEN** new tabs are created with Master layout mode instead of BSP

#### Scenario: Default layout mode not set
- **WHEN** the user does not set `default_layout` in config
- **THEN** new tabs use BSP layout mode
