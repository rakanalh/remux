## ADDED Requirements

### Requirement: Three input modes
The system SHALL support three input modes: Insert, Normal, and Visual. Exactly one mode SHALL be active at any time. The current mode SHALL be displayed in the status bar/frame.

#### Scenario: Default mode on attach
- **WHEN** a client attaches to a session
- **THEN** the initial mode is Insert

### Requirement: Insert mode
In Insert mode, all key events SHALL be passed through to the active pane's PTY, except the mode-switch key (Escape by default, configurable).

#### Scenario: Typing in Insert mode
- **WHEN** the user presses any key other than Escape in Insert mode
- **THEN** the key is written to the active pane's PTY

#### Scenario: Exit Insert mode
- **WHEN** the user presses Escape in Insert mode
- **THEN** the system transitions to Normal mode

### Requirement: Normal mode
In Normal mode, key events SHALL be interpreted as commands via the keybinding tree. No keys are passed to the PTY.

#### Scenario: Enter Insert mode from Normal
- **WHEN** the user presses 'i' or Enter in Normal mode
- **THEN** the system transitions to Insert mode

#### Scenario: Enter Visual mode from Normal
- **WHEN** the user presses 'v' in Normal mode
- **THEN** the system transitions to Visual mode and the scrollback buffer is activated

#### Scenario: Command key in Normal mode
- **WHEN** the user presses a key that maps to a command in Normal mode
- **THEN** the command is executed

### Requirement: Visual mode
In Visual mode, the user SHALL navigate and select text in the active pane's scrollback buffer using vim-style motions.

#### Scenario: Yank selection
- **WHEN** the user selects text and presses 'y' in Visual mode
- **THEN** the selected text is copied to the system clipboard

#### Scenario: Exit Visual mode
- **WHEN** the user presses Escape in Visual mode
- **THEN** the system transitions to Normal mode

#### Scenario: Search in scrollback
- **WHEN** the user presses '/' in Visual mode
- **THEN** a search prompt appears and the user can search the scrollback buffer
