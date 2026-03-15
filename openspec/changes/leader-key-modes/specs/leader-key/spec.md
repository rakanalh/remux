## ADDED Requirements

### Requirement: Configurable leader key
The system SHALL support a configurable leader key (default: Ctrl+a) that transitions from Passthrough to Command mode. The leader key SHALL be configurable via TOML config at `keybindings.command.leader`.

#### Scenario: Default leader key
- **WHEN** no custom leader key is configured
- **THEN** the leader key is Ctrl+a

#### Scenario: Custom leader key
- **WHEN** the user sets `leader = "Ctrl-b"` in the TOML config
- **THEN** Ctrl+b is used as the leader key instead of Ctrl+a

#### Scenario: Leader key enters command mode
- **WHEN** the user is in Passthrough and presses the leader key
- **THEN** the system transitions to Command mode and displays the which-key root menu

### Requirement: Leader key double-tap passthrough
When in Command mode at the root of the keybinding tree, pressing the leader key again SHALL send the leader key's byte sequence to the active PTY and return to Passthrough.

#### Scenario: Send Ctrl+a to inner application
- **WHEN** the user presses Ctrl+a (leader) followed by Ctrl+a (leader) again
- **THEN** the raw Ctrl+a byte (0x01) is sent to the active pane's PTY
- **AND** the system returns to Passthrough

#### Scenario: Leader key at non-root depth
- **WHEN** the user is navigating a submenu in Command mode (e.g., pressed 't' for Tab group) and presses the leader key
- **THEN** the leader key press is ignored (not a valid key in the submenu) and the which-key menu remains at the current depth
