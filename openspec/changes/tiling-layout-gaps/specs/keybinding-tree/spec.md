## MODIFIED Requirements

### Requirement: User-configurable keybindings
The user SHALL be able to fully customize keybinding groups, keys, labels, and command mappings via the TOML config file. The system SHALL provide sensible defaults. The default keybinding tree SHALL include a root-level `g` key bound to `toggle_gaps` and a `p` group ("pane") containing `r` bound to `pane_rename`.

#### Scenario: Default toggle gaps binding
- **WHEN** the user has not customized keybindings and presses `g` in Normal mode
- **THEN** the `toggle_gaps` command is executed, switching gap mode for the current session

#### Scenario: Default pane rename binding
- **WHEN** the user has not customized keybindings and presses `p` then `r` in Normal mode
- **THEN** the `pane_rename` command is triggered, entering RENAME input mode

#### Scenario: User overrides gaps binding
- **WHEN** the user remaps `g` to a different command in config
- **THEN** the user's mapping takes precedence and `toggle_gaps` is no longer bound to `g`

#### Scenario: User overrides pane rename binding
- **WHEN** the user remaps `p` → `r` to a different command in config
- **THEN** the user's mapping takes precedence and `pane_rename` is no longer bound to `p` → `r`

#### Scenario: Custom keybinding group
- **WHEN** the user defines a custom group `x` with label "Custom" and sub-keys in config
- **THEN** pressing `x` in Normal mode opens that group with the user-defined sub-keys

### Requirement: Pane rename action parsing
The keybinding system SHALL parse the action string `"pane_rename"` and map it to the `RemuxCommand::PaneRename` command. The system SHALL enter RENAME input mode when this action is triggered.

#### Scenario: Parse pane_rename action
- **WHEN** the keybinding system encounters the action string `"pane_rename"`
- **THEN** it maps to `RemuxCommand::PaneRename` and triggers RENAME mode on the client

#### Scenario: Unknown action string
- **WHEN** the keybinding system encounters an unrecognized action string
- **THEN** it reports an error or ignores the binding gracefully
