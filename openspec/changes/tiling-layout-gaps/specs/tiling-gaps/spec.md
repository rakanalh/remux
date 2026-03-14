## ADDED Requirements

### Requirement: ZellijStyle rendering
The system SHALL render panes in ZellijStyle mode with full box borders using rounded corner characters (`╭`, `╮`, `╰`, `╯`), horizontal borders (`─`), and vertical borders (`│`). The border SHALL occupy the outermost cells of the pane rect, and content SHALL be blitted to the inner area at `(x+1, y+1, width-2, height-2)`.

#### Scenario: Single pane with ZellijStyle border
- **WHEN** a tab has one pane and gap mode is `ZellijStyle`
- **THEN** the pane is rendered with a full box border using rounded corners, and the pane content occupies the inner area inset by 1 cell on each side

#### Scenario: Multiple panes with ZellijStyle borders
- **WHEN** a tab has multiple panes split vertically and gap mode is `ZellijStyle`
- **THEN** each pane has its own independent box border with rounded corners, and gap cells between panes are filled with the background color

### Requirement: ZellijStyle pane name display
The system SHALL render the pane name in the top border of each pane in ZellijStyle mode, formatted as `╭ <name> ───╮`.

#### Scenario: Pane name in top border
- **WHEN** a pane named "zsh" is rendered in ZellijStyle mode
- **THEN** the top border reads `╭ zsh ──────────╮` with the name embedded after the top-left corner

#### Scenario: Long pane name truncation
- **WHEN** a pane name is longer than the available border width minus decorations
- **THEN** the name is truncated to fit within the border

### Requirement: ZellijStyle stacked pane tabs
The system SHALL render stacked pane names as tabs in the top border when a stack contains multiple panes. The active pane's tab SHALL be visually highlighted.

#### Scenario: Stacked panes show all names as tabs
- **WHEN** a stack contains panes named "zsh", "nvim", and "cargo" with "nvim" active
- **THEN** the top border renders as `╭ zsh | nvim | cargo ─╮` with "nvim" visually highlighted

#### Scenario: Single pane in stack shows single name
- **WHEN** a stack contains only one pane named "zsh"
- **THEN** the top border renders as `╭ zsh ──────────╮` with no tab separator characters

### Requirement: ZellijStyle active pane highlight
The system SHALL render the active pane's border in a distinct color (green or white) and inactive panes' borders in dark grey when in ZellijStyle mode.

#### Scenario: Active pane border color
- **WHEN** a pane is the active (focused) pane in ZellijStyle mode
- **THEN** its border characters are rendered in the highlight color (green or white)

#### Scenario: Inactive pane border color
- **WHEN** a pane is not the active pane in ZellijStyle mode
- **THEN** its border characters are rendered in dark grey

### Requirement: TmuxStyle rendering
The system SHALL render panes in TmuxStyle mode with edge-to-edge content and minimal `│`/`─` dividers at split boundaries. Gap size SHALL always be 0 in TmuxStyle mode regardless of the configured `gap_size`.

#### Scenario: TmuxStyle edge-to-edge content
- **WHEN** two panes are split vertically in TmuxStyle mode
- **THEN** pane content fills the full pane rect with no border inset, and a single `│` divider is drawn at the split boundary

#### Scenario: TmuxStyle forces gap_size to zero
- **WHEN** gap mode is `TmuxStyle` and `gap_size` is configured as 2
- **THEN** no gap space appears between panes; the effective gap size is 0

### Requirement: TmuxStyle stacked pane tab bar
The system SHALL render a 1-row tab bar at the top of stacks containing more than one pane in TmuxStyle mode, showing pane names with the active pane highlighted. Single-pane stacks SHALL NOT have a tab bar.

#### Scenario: Multi-pane stack tab bar
- **WHEN** a stack has 3 panes named "zsh", "nvim", "cargo" in TmuxStyle mode
- **THEN** a 1-row tab bar is rendered at the top of the stack showing all three names with the active pane highlighted

#### Scenario: Single-pane stack no tab bar
- **WHEN** a stack has only 1 pane in TmuxStyle mode
- **THEN** no tab bar is rendered and the pane content occupies the full stack area

### Requirement: Pane name auto-detection
The system SHALL auto-detect pane names from the running process (e.g., via `/proc/<pid>/comm`). When no custom name is set, the auto-detected process name SHALL be used as the pane name.

#### Scenario: Auto-detected pane name
- **WHEN** a pane is running `nvim` and no custom name has been set
- **THEN** the pane name is displayed as "nvim"

#### Scenario: Custom name overrides auto-detection
- **WHEN** a pane is running `nvim` but has been renamed to "editor"
- **THEN** the pane name is displayed as "editor"

### Requirement: PaneRename command
The system SHALL provide a `PaneRename(String)` command that sets a custom name for the currently focused pane. The rename SHALL trigger an immediate re-render to display the new name.

#### Scenario: Rename focused pane
- **WHEN** the user executes `PaneRename("editor")` on a pane currently named "nvim"
- **THEN** the pane's custom name is set to "editor" and the display updates to show "editor"

#### Scenario: Rename triggers re-render
- **WHEN** the server processes a `PaneRename` command
- **THEN** the compositor re-renders to reflect the updated pane name in borders or tab bars

### Requirement: Rename input mode
The system SHALL provide a RENAME input mode triggered by the `pane_rename` keybinding. In RENAME mode, the client SHALL show "RENAME" in the mode indicator and `Rename pane: _` in the status bar. The user types the new name; Enter confirms and sends the rename command, Escape cancels and returns to Normal mode.

#### Scenario: Enter rename mode
- **WHEN** the user presses `p` then `r` in Normal mode
- **THEN** the client enters RENAME mode with "RENAME" shown in the mode indicator and `Rename pane: _` in the status bar

#### Scenario: Confirm rename
- **WHEN** the user types "editor" and presses Enter in RENAME mode
- **THEN** the client sends `Command(PaneRename("editor"))` to the server and returns to Normal mode

#### Scenario: Cancel rename
- **WHEN** the user presses Escape in RENAME mode
- **THEN** the client returns to Normal mode without sending a rename command

### Requirement: Gap mode toggle
The system SHALL provide a `ToggleGaps` command that switches the current session's gap mode between `ZellijStyle` and `TmuxStyle`. Toggling SHALL trigger an immediate re-layout and re-render.

#### Scenario: Toggle from ZellijStyle to TmuxStyle
- **WHEN** the user executes `ToggleGaps` while gap mode is `ZellijStyle`
- **THEN** gap mode switches to `TmuxStyle`, borders change to minimal dividers, gaps are removed, and the screen re-renders

#### Scenario: Toggle from TmuxStyle to ZellijStyle
- **WHEN** the user executes `ToggleGaps` while gap mode is `TmuxStyle`
- **THEN** gap mode switches to `ZellijStyle`, full box borders appear with pane names, gaps are applied, and the screen re-renders

### Requirement: Gap rendering
The system SHALL fill gap regions (cells not covered by any pane rect) with the terminal default background color in ZellijStyle mode. Gap regions SHALL NOT contain border characters or pane content.

#### Scenario: Gap cells rendered in ZellijStyle
- **WHEN** gap mode is `ZellijStyle` and two panes are split vertically with `gap_size = 1`
- **THEN** the cells between the two pane rects are filled with the default background color

### Requirement: Gaps only between panes
Gaps SHALL only appear between adjacent panes (at split boundaries). The outer edges of the layout (terminal screen boundary) SHALL NOT have gaps.

#### Scenario: Outer edges are flush
- **WHEN** a tab has multiple panes and gap mode is `ZellijStyle`
- **THEN** panes at the screen edges extend to the edge with no outer gap
