## ADDED Requirements

### Requirement: Three-mode system
The system SHALL support exactly three input modes: Passthrough, Command, and Visual. Exactly one mode SHALL be active at any time. The current mode SHALL be displayed in the status bar.

#### Scenario: Default mode on attach
- **WHEN** a client attaches to a session
- **THEN** the initial mode is Passthrough

#### Scenario: Mode display in status bar
- **WHEN** the mode changes
- **THEN** the status bar displays the current mode name: "PASSTHROUGH", "COMMAND", or "VISUAL"

### Requirement: Passthrough mode
In Passthrough mode, all keyboard input SHALL be forwarded to the active pane's PTY, except for the leader key which SHALL be intercepted to enter Command mode.

#### Scenario: Regular key in passthrough
- **WHEN** the user presses any key that is not the leader key while in Passthrough
- **THEN** the key is converted to bytes and sent to the active PTY

#### Scenario: Leader key in passthrough
- **WHEN** the user presses the leader key while in Passthrough
- **THEN** the system transitions to Command mode

### Requirement: Command mode
In Command mode, all keyboard input SHALL be consumed by the keybinding tree navigator. No keys SHALL be forwarded to the PTY. The which-key overlay SHALL display available bindings at the current tree depth.

#### Scenario: Navigate keybinding tree
- **WHEN** the user presses a key that maps to a Group node in the keybinding tree
- **THEN** the which-key display updates to show the group's children

#### Scenario: Execute leaf binding
- **WHEN** the user presses a key that maps to a Leaf node in the keybinding tree
- **THEN** the leaf's action chain executes

#### Scenario: Invalid key in command mode
- **WHEN** the user presses a key that has no mapping at the current tree depth
- **THEN** the key is ignored and the which-key menu remains unchanged

#### Scenario: Escape exits command mode
- **WHEN** the user presses Escape while in Command mode (at any tree depth)
- **THEN** the system returns to Passthrough

### Requirement: Visual mode
Visual mode SHALL provide scrollback navigation and text selection using vim-style motions. Visual mode SHALL be entered via the leader key sequence `<leader>v`.

#### Scenario: Enter visual mode
- **WHEN** the user presses `<leader>v` (leader key followed by 'v')
- **THEN** the system transitions to Visual mode

#### Scenario: Exit visual mode
- **WHEN** the user presses Escape in Visual mode
- **THEN** the system returns to Passthrough

### Requirement: Rename as command overlay
Rename operations (PaneRename, TabRename) SHALL activate an inline text input overlay. The overlay SHALL capture keystrokes until Enter (confirm) or Escape (cancel). The overlay is not a separate mode.

#### Scenario: Pane rename overlay
- **WHEN** the user triggers the PaneRename command from a keybinding
- **THEN** an inline text input appears for entering the new name
- **AND** pressing Enter confirms the rename and the overlay closes
- **AND** pressing Escape cancels the rename and the overlay closes

#### Scenario: Mode unchanged during rename
- **WHEN** the rename overlay is active
- **THEN** the system mode remains Command (or Passthrough if the action chain included EnterPassthrough before the rename resolved)
