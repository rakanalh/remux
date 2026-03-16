## MODIFIED Requirements

### Requirement: Normal mode
In Normal mode, all keyboard input SHALL first be checked against the shortcut bindings. If a key matches a shortcut binding, the bound action SHALL execute and the key SHALL NOT be forwarded to the PTY. If a key matches the leader key, the system SHALL transition to Command mode. All other keys SHALL be forwarded to the active pane's PTY.

#### Scenario: Regular key in Normal mode
- **WHEN** the user presses any key that is not the leader key and does not match a shortcut binding while in Normal mode
- **THEN** the key is converted to bytes and sent to the active PTY

#### Scenario: Leader key in Normal mode
- **WHEN** the user presses the leader key while in Normal mode
- **THEN** the system transitions to Command mode

#### Scenario: Shortcut binding in Normal mode
- **WHEN** the user presses a key that matches a shortcut binding
- **THEN** the bound action executes and the key is NOT sent to the PTY

#### Scenario: Shortcut binding priority over PTY
- **WHEN** a key matches both a shortcut binding and would be a valid PTY input
- **THEN** the shortcut binding SHALL take priority and the key SHALL NOT be forwarded

### Requirement: Command mode entered at arbitrary tree depth
The system SHALL support entering Command mode at any depth in the keybinding tree, not only at the root. When entered at a non-root group, the which-key popup SHALL display that group's children immediately.

#### Scenario: Enter command mode at root
- **WHEN** the user presses the leader key in Normal mode
- **THEN** Command mode starts at the root of the keybinding tree

#### Scenario: Enter command mode at group via shortcut
- **WHEN** a shortcut binding references a group prefix (e.g., `@p`)
- **THEN** the system SHALL enter Command mode with the keybinding tree positioned at that group
- **AND** the which-key popup SHALL display that group's children

#### Scenario: Escape from group-entered command mode
- **WHEN** the user presses Escape after entering Command mode via a group shortcut
- **THEN** the system SHALL return to Normal mode (not to the tree root)
