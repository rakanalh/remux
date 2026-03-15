## ADDED Requirements

### Requirement: Click to focus pane
The system SHALL focus a pane when the user clicks within its visible area. The clicked pane SHALL become the active pane for the current tab, and its border SHALL update to reflect focused state.

#### Scenario: Click on an inactive pane
- **WHEN** the user clicks on a pane that is not currently focused
- **THEN** the clicked pane becomes the focused pane and its border changes to the active color

#### Scenario: Click on the already-focused pane
- **WHEN** the user clicks on the pane that is already focused
- **THEN** nothing changes; the pane remains focused

#### Scenario: Click on a border or gap between panes
- **WHEN** the user clicks on a border or gap area that does not belong to any pane
- **THEN** no focus change occurs

### Requirement: Click to switch tab
The system SHALL switch to a tab when the user clicks on its label in the status bar. The clicked tab SHALL become the active tab for the session.

#### Scenario: Click on an inactive tab label
- **WHEN** the user clicks on a tab label in the status bar that is not the active tab
- **THEN** the session switches to the clicked tab and the display updates to show that tab's layout

#### Scenario: Click on the active tab label
- **WHEN** the user clicks on the tab label that is already active
- **THEN** nothing changes

### Requirement: Click to activate stacked pane
The system SHALL activate a stacked pane when the user clicks on its label in the stack header. The clicked stack entry SHALL become the active pane within that stack.

#### Scenario: Click on an inactive stack label
- **WHEN** a stack contains multiple panes and the user clicks on the label of an inactive stacked pane
- **THEN** the clicked pane becomes the active pane in the stack and is displayed

#### Scenario: Click on the active stack label
- **WHEN** the user clicks on the label of the already-active stacked pane
- **THEN** nothing changes

### Requirement: Server-side hit testing
The system SHALL resolve mouse click coordinates to their target (pane, tab, or stacked pane) on the server using the current layout geometry. Hit testing SHALL check tab labels and stack labels before pane areas.

#### Scenario: Hit test priority order
- **WHEN** the user clicks at coordinates that overlap both a tab label and a pane area
- **THEN** the tab label takes priority and the tab is switched

#### Scenario: Click outside any interactive region
- **WHEN** the user clicks on an area that is not a pane, tab label, or stack label
- **THEN** no action is taken
