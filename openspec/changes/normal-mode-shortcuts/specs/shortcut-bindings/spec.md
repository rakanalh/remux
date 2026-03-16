## ADDED Requirements

### Requirement: Normal mode shortcut bindings
The system SHALL support a set of modifier-based keybindings that are checked in Normal mode before forwarding keys to the PTY. These bindings use key notation strings with modifiers (e.g., `"Alt-h"`, `"Alt-Shift-h"`, `"Ctrl-Alt-p"`) as keys, mapped to either a command string or a keybinding tree group reference.

#### Scenario: Shortcut binding matches
- **WHEN** the user presses a key combination in Normal mode that matches a shortcut binding
- **THEN** the system SHALL execute the bound action and NOT forward the key to the PTY

#### Scenario: Shortcut binding does not match
- **WHEN** the user presses a key combination in Normal mode that does not match any shortcut binding
- **THEN** the key SHALL be forwarded to the PTY as normal

#### Scenario: Plain keys are not intercepted
- **WHEN** a shortcut binding is defined with a plain character key (no modifier)
- **THEN** the system SHALL report a config parse error, since shortcut bindings MUST use at least one modifier (Alt, Ctrl, or Ctrl-Alt) to avoid capturing keys meant for the PTY

### Requirement: Shortcut bindings map to commands or group prefixes
A shortcut binding value SHALL be either a command string (executed directly) or a group prefix reference using `"@<group-key>"` syntax that enters Command mode at the specified keybinding tree group.

#### Scenario: Shortcut binding executes a command
- **WHEN** the user presses `Alt-h` and the shortcut binding maps `"Alt-h"` to `"PaneFocusLeft"`
- **THEN** the system SHALL execute `PaneFocusLeft` and remain in Normal mode

#### Scenario: Shortcut binding opens a keybinding group
- **WHEN** the user presses `Alt-p` and the shortcut binding maps `"Alt-p"` to `"@p"`
- **THEN** the system SHALL enter Command mode with the keybinding tree navigated to the `p` (Pane) group
- **AND** the which-key popup SHALL display the Pane group's children

#### Scenario: Shortcut binding opens a nested group
- **WHEN** the shortcut binding maps `"Alt-t"` to `"@t"`
- **AND** the user presses `Alt-t`
- **THEN** the system SHALL enter Command mode at the `t` (Tab) group and display its children

#### Scenario: Invalid group prefix reference
- **WHEN** a shortcut binding maps to `"@z"` but no group `z` exists in the keybinding tree
- **THEN** the system SHALL report a config parse error at load time

### Requirement: Default shortcut bindings
The system SHALL provide default shortcut bindings for common operations:

- `Alt-h` — `PaneFocusLeft`
- `Alt-j` — `PaneFocusDown`
- `Alt-k` — `PaneFocusUp`
- `Alt-l` — `PaneFocusRight`
- `Alt-n` — `TabNext`
- `Alt-p` — `@p` (open Pane group)
- `Alt-t` — `@t` (open Tab group)

#### Scenario: Default pane navigation via Alt
- **WHEN** a user presses `Alt-h` in Normal mode with default bindings
- **THEN** the system SHALL execute `PaneFocusLeft` and remain in Normal mode

#### Scenario: Default group shortcut via Alt
- **WHEN** a user presses `Alt-p` in Normal mode with default bindings
- **THEN** the system SHALL enter Command mode at the Pane group

#### Scenario: Defaults active without config
- **WHEN** the config file has no `[keybindings.command]` section
- **THEN** all default shortcut bindings SHALL be active

### Requirement: User shortcut bindings merge with defaults
User-defined shortcut bindings SHALL be merged on top of the default set. User bindings override defaults at the key level.

#### Scenario: Override a default shortcut binding
- **WHEN** the user configures `"Alt-h" = "TabPrev"` under `[keybindings.command]`
- **THEN** pressing `Alt-h` in Normal mode SHALL execute `TabPrev` instead of the default `PaneFocusLeft`

#### Scenario: Add a new shortcut binding
- **WHEN** the user configures `"Alt-Shift-h" = "ResizeLeft 5"` under `[keybindings.command]`
- **THEN** pressing Alt+Shift+h in Normal mode SHALL execute `ResizeLeft 5`
- **AND** all default shortcut bindings SHALL remain active

#### Scenario: Remove a default shortcut binding
- **WHEN** the user configures `"Alt-h" = ""` under `[keybindings.command]`
- **THEN** pressing Alt-h in Normal mode SHALL forward the key to the PTY
