## ADDED Requirements

### Requirement: Command palette popup
The system SHALL display a command palette popup overlay when triggered, showing a text input field and a filtered list of available commands.

#### Scenario: Opening the command palette
- **WHEN** user presses `:` in command mode (i.e., `<Prefix>:`)
- **THEN** the system SHALL display a centered popup overlay with a text input field and a list of all available commands

#### Scenario: Closing the command palette
- **WHEN** the command palette is open and user presses Escape
- **THEN** the system SHALL close the palette and return to command mode without executing anything

### Requirement: Command filtering
The system SHALL filter the displayed command list in real-time as the user types, using case-insensitive prefix matching against command names.

#### Scenario: Typing filters commands
- **WHEN** user types "pane" into the palette input
- **THEN** the system SHALL display only commands whose names contain "pane" (e.g., PaneNew, PaneClose, PaneSplitVertical)

#### Scenario: Empty input shows all commands
- **WHEN** the palette is open and the input field is empty
- **THEN** the system SHALL display all available commands

### Requirement: Tab autocompletion
The system SHALL support Tab key to autocomplete to the longest common prefix of matching commands, and cycle through matches when pressed repeatedly.

#### Scenario: Tab completes common prefix
- **WHEN** user types "PaneS" and presses Tab
- **THEN** the system SHALL complete the input to "PaneSplit" (the longest common prefix of PaneSplitVertical and PaneSplitHorizontal) and highlight the first match

#### Scenario: Tab cycles through matches
- **WHEN** the input matches multiple commands and user presses Tab repeatedly
- **THEN** the system SHALL cycle the highlight through each matching command in order

#### Scenario: Tab with single match completes fully
- **WHEN** the input matches exactly one command and user presses Tab
- **THEN** the system SHALL complete the input to the full command name

### Requirement: Command execution
The system SHALL execute the highlighted command when the user presses Enter.

#### Scenario: Execute selected command
- **WHEN** user highlights a command (e.g., PaneNew) and presses Enter
- **THEN** the system SHALL execute that command, close the palette, and return to normal mode

#### Scenario: Execute with typed input
- **WHEN** user types an exact command name and presses Enter without tabbing
- **THEN** the system SHALL execute the command if the typed name matches exactly

#### Scenario: No match on Enter
- **WHEN** user presses Enter and the typed input does not match any command
- **THEN** the system SHALL do nothing (palette stays open)

### Requirement: Command with arguments
The system SHALL support commands that take arguments by allowing the user to type the argument after the command name, separated by a space.

#### Scenario: Command with string argument
- **WHEN** user types "TabRename My Tab" and presses Enter
- **THEN** the system SHALL execute `TabRename("My Tab")`

#### Scenario: Command with numeric argument
- **WHEN** user types "TabGoto 3" and presses Enter
- **THEN** the system SHALL execute `TabGoto(3)`

### Requirement: Visual presentation
The command palette SHALL render as a centered overlay popup with theme-consistent colors, showing the input field at the top and the filtered command list below.

#### Scenario: Palette styling
- **WHEN** the command palette is displayed
- **THEN** the popup SHALL use the theme's whichkey colors for background/foreground, with the highlighted match using an inverted or distinct color

#### Scenario: Palette dimensions
- **WHEN** the command palette is displayed
- **THEN** the popup SHALL be sized to fit the command list (up to a maximum height) and centered horizontally and vertically
