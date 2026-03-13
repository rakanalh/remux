## ADDED Requirements

### Requirement: Hierarchical keybinding groups
The system SHALL organize Normal mode keybindings as a tree of groups. Each group is identified by a key and contains sub-keys that map to either commands or nested groups. Groups SHALL have a label for display in the which-key popup.

#### Scenario: Navigate keybinding tree
- **WHEN** the user presses 't' in Normal mode and 't' is a group key
- **THEN** the system enters the 't' group context and waits for the next key

#### Scenario: Execute leaf command
- **WHEN** the user presses 't' then 'n' and 'tn' maps to "tab:new"
- **THEN** a new tab is created

### Requirement: Which-key popup
The system SHALL display a popup showing available keys and their labels after the user presses a group key. The popup SHALL appear after a configurable timeout (default 500ms). If the user presses the next key before the timeout, the popup is skipped.

#### Scenario: Popup appears after timeout
- **WHEN** the user presses a group key and waits longer than the configured timeout
- **THEN** a which-key popup is displayed showing all available sub-keys with labels

#### Scenario: Fast typing skips popup
- **WHEN** the user presses a group key followed by a sub-key within the timeout
- **THEN** the command executes without showing the popup

### Requirement: Configurable timeout
The which-key popup timeout SHALL be configurable in the TOML config file. The timeout SHALL also support being disabled (popup always shows) or set to 0 (popup never shows, keys still work).

#### Scenario: Custom timeout
- **WHEN** the user sets timeout_ms = 300 in config
- **THEN** the which-key popup appears after 300ms of inactivity in a group

### Requirement: Cancel key sequence
The user SHALL be able to cancel a partial key sequence by pressing Escape, returning to the root of the keybinding tree.

#### Scenario: Cancel partial sequence
- **WHEN** the user presses a group key then Escape
- **THEN** the partial sequence is discarded and Normal mode remains at the root level

### Requirement: User-configurable keybindings
The user SHALL be able to fully customize keybinding groups, keys, labels, and command mappings via the TOML config file. The system SHALL provide sensible defaults.

#### Scenario: Custom keybinding group
- **WHEN** the user defines a custom group 'x' with label "Custom" and sub-keys in config
- **THEN** pressing 'x' in Normal mode opens that group with the user-defined sub-keys

#### Scenario: Override default keybinding
- **WHEN** the user remaps 'tn' from "tab:new" to "tab:rename" in config
- **THEN** pressing 'tn' in Normal mode renames the tab instead of creating a new one
