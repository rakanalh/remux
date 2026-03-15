## MODIFIED Requirements

### Requirement: Three input modes
The system SHALL support three input modes: Insert, Normal, and Visual. Exactly one mode SHALL be active at any time. The current mode SHALL be displayed in the status bar/frame. Mouse events SHALL be processed in all modes without requiring a mode switch.

#### Scenario: Default mode on attach
- **WHEN** a client attaches to a session
- **THEN** the initial mode is Insert

#### Scenario: Mouse click in any mode
- **WHEN** the user clicks the mouse in Insert, Normal, or Visual mode
- **THEN** the click is processed (focus change, tab switch, etc.) without changing the current mode

### Requirement: Visual mode
In Visual mode, the user SHALL navigate and select text in the active pane's scrollback buffer using vim-style motions. Mouse-initiated selection SHALL be a distinct interaction that does not enter persistent Visual mode.

#### Scenario: Yank selection
- **WHEN** the user selects text and presses 'y' in Visual mode
- **THEN** the selected text is copied to the system clipboard

#### Scenario: Exit Visual mode
- **WHEN** the user presses Escape in Visual mode
- **THEN** the system transitions to Normal mode

#### Scenario: Search in scrollback
- **WHEN** the user presses '/' in Visual mode
- **THEN** a search prompt appears and the user can search the scrollback buffer

#### Scenario: Mouse drag selection
- **WHEN** the user clicks and drags to select text in any mode
- **THEN** the selection is handled as a transient mouse selection (not keyboard Visual mode), text is auto-copied on release, and the previous mode is restored
