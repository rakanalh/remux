## ADDED Requirements

### Requirement: Commands use PascalCase naming convention
All user-facing commands SHALL use PascalCase names that match the internal `RemuxCommand` enum variants. The command name alone uniquely identifies the operation.

#### Scenario: PascalCase command names
- **WHEN** a user writes a command in their config
- **THEN** the command name MUST be in PascalCase (e.g., `PaneSplitVertical`, `TabNew`, `SessionDetach`)

#### Scenario: Invalid command name
- **WHEN** a user writes a command name that does not match any known command
- **THEN** the system SHALL report a config parse error identifying the unknown command name

### Requirement: Commands accept positional arguments
Commands that require parameters SHALL accept them as space-separated positional arguments after the command name. String arguments containing spaces MUST be quoted with double quotes.

#### Scenario: Command with no arguments
- **WHEN** a user writes `TabNew`
- **THEN** the system SHALL execute the TabNew command with no arguments

#### Scenario: Command with numeric argument
- **WHEN** a user writes `TabGoto 3`
- **THEN** the system SHALL execute TabGoto with the argument `3` (parsed as usize)

#### Scenario: Command with string argument
- **WHEN** a user writes `SessionNew "my-project"`
- **THEN** the system SHALL execute SessionNew with the argument `"my-project"`

#### Scenario: Command with multiple arguments
- **WHEN** a user writes `FolderMoveSession "dev-session" "work"`
- **THEN** the system SHALL execute FolderMoveSession with session=`"dev-session"` and folder=`"work"`

#### Scenario: Invalid argument type
- **WHEN** a user writes `TabGoto "abc"` where a numeric argument is expected
- **THEN** the system SHALL report a config parse error identifying the type mismatch

### Requirement: Full command catalog
The system SHALL provide the following commands organized by category. Each command maps 1:1 to a `RemuxCommand` variant.

**Pane commands:**
- `PaneNew` — Create a new pane
- `PaneClose` — Close the focused pane
- `PaneSplitVertical` — Split focused pane vertically
- `PaneSplitHorizontal` — Split focused pane horizontally
- `PaneFocusLeft` — Focus pane to the left
- `PaneFocusRight` — Focus pane to the right
- `PaneFocusUp` — Focus pane above
- `PaneFocusDown` — Focus pane below
- `PaneStackAdd` — Add focused pane to a stack
- `PaneStackNext` — Focus next pane in stack
- `PaneStackPrev` — Focus previous pane in stack

**Tab commands:**
- `TabNew` — Create a new tab
- `TabClose` — Close the active tab
- `TabGoto <index>` — Jump to tab by index (1-based)
- `TabNext` — Focus next tab
- `TabPrev` — Focus previous tab
- `TabMove <index>` — Move tab to position (1-based)
- `TabRename <name>` — Rename the active tab

**Session commands:**
- `SessionNew <name>` — Create a new session
- `SessionDetach` — Detach from current session
- `SessionRename <name>` — Rename current session
- `SessionList` — List all sessions
- `SessionSave` — Save session state

**Folder commands:**
- `FolderNew <name>` — Create a new folder
- `FolderDelete <name>` — Delete a folder
- `FolderList` — List all folders
- `FolderMoveSession <session> <folder>` — Move a session to a folder

**Buffer commands:**
- `BufferEditInEditor` — Open scrollback in external editor
- `BufferSearch` — Search scrollback buffer

**Resize commands:**
- `ResizeLeft <amount>` — Resize pane left by amount (default: 1)
- `ResizeRight <amount>` — Resize pane right by amount (default: 1)
- `ResizeUp <amount>` — Resize pane up by amount (default: 1)
- `ResizeDown <amount>` — Resize pane down by amount (default: 1)

**Mode commands:**
- `EnterInsertMode` — Switch to insert mode
- `EnterNormalMode` — Switch to normal mode
- `EnterVisualMode` — Switch to visual mode

**Layout commands:**
- `ToggleGaps` — Toggle pane gap rendering on/off

#### Scenario: Every RemuxCommand variant has a corresponding user command
- **WHEN** a new `RemuxCommand` variant is added to the protocol
- **THEN** a corresponding PascalCase command name MUST be added to the command catalog

#### Scenario: Command with optional argument uses default
- **WHEN** a user writes `ResizeLeft` without specifying an amount
- **THEN** the system SHALL use the default amount of 1

### Requirement: Command parsing produces RemuxCommand values
The command parser SHALL convert a command string into the corresponding `RemuxCommand` enum variant with all arguments properly typed.

#### Scenario: Parse parameterless command
- **WHEN** the parser receives `"PaneClose"`
- **THEN** it SHALL produce `RemuxCommand::PaneClose`

#### Scenario: Parse command with numeric parameter
- **WHEN** the parser receives `"ResizeLeft 5"`
- **THEN** it SHALL produce `RemuxCommand::ResizeLeft(5)`

#### Scenario: Parse command with string parameter
- **WHEN** the parser receives `"TabRename \"main\""`
- **THEN** it SHALL produce `RemuxCommand::TabRename("main".to_string())`

#### Scenario: Parse command with multiple parameters
- **WHEN** the parser receives `"FolderMoveSession \"my-session\" \"work\""`
- **THEN** it SHALL produce `RemuxCommand::FolderMoveSession { session: "my-session".to_string(), folder: "work".to_string() }`
