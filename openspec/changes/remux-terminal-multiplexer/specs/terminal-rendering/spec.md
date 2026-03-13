## ADDED Requirements

### Requirement: Per-pane virtual screen buffer
The system SHALL maintain a virtual screen buffer for each pane, updated by feeding PTY output through the VTE parser. The buffer SHALL track cell content, text attributes (bold, italic, color, etc.), and cursor position.

#### Scenario: PTY output updates buffer
- **WHEN** a child process outputs text with ANSI formatting
- **THEN** the pane's virtual screen buffer is updated with the correct characters and attributes at the correct positions

### Requirement: Diff-based rendering
The system SHALL maintain a front buffer (last rendered state) and back buffer (current state). On each render cycle, only cells that differ between the two buffers SHALL be emitted to the terminal.

#### Scenario: Partial screen update
- **WHEN** a single pane's content changes
- **THEN** only the changed cells in that pane's region are redrawn, not the entire screen

### Requirement: Zellij-style pane frames
The system SHALL render frames around each pane stack by default. Frames SHALL display the pane stack's tab headers (showing stacked pane names with the active one highlighted) and the pane stack's title.

#### Scenario: Render frame with stack tabs
- **WHEN** a pane stack has multiple panes
- **THEN** the frame's top border shows tab-like headers for each pane in the stack, with the active pane's name highlighted

### Requirement: Configurable frame style
The system SHALL support switching between zellij-style frames (borders around each pane) and tmux-style (no pane borders, single status bar at bottom).

#### Scenario: tmux-style configuration
- **WHEN** the user sets frame_style = "minimal" in config
- **THEN** pane borders are hidden and a single status bar is shown at the bottom

### Requirement: Status bar
The system SHALL display a status bar showing: current mode indicator, folder/session path, tab list with active tab highlighted, and optionally the time. The status bar position and content SHALL be configurable.

#### Scenario: Mode indicator display
- **WHEN** the user is in Normal mode
- **THEN** the status bar displays "NORMAL" (or configured label) with the configured color

### Requirement: Which-key popup rendering
The system SHALL render the which-key popup as a floating overlay centered at the bottom of the screen, showing available keys in a two-column layout.

#### Scenario: Popup rendering
- **WHEN** the which-key popup is triggered
- **THEN** a bordered floating box is rendered showing keys and their labels, without overwriting the underlying content permanently (restored when dismissed)
