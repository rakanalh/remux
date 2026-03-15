## ADDED Requirements

### Requirement: Action chains in keybindings
Each keybinding leaf SHALL map to an ordered sequence of one or more actions (an action chain). Actions in the chain SHALL execute sequentially in order.

#### Scenario: Single action binding
- **WHEN** a keybinding is configured as `n = "TabNew"`
- **THEN** the system executes TabNew as a single-element action chain

#### Scenario: Multi-action binding
- **WHEN** a keybinding is configured as `n = "PaneNew; EnterPassthrough"`
- **THEN** the system executes PaneNew first, then EnterPassthrough

#### Scenario: Action chain with mode transition
- **WHEN** an action chain contains EnterPassthrough
- **THEN** after executing all actions up to and including EnterPassthrough, the system transitions to Passthrough

### Requirement: Which-key resets to root after chain execution
After an action chain completes without an EnterPassthrough action, the system SHALL remain in Command mode and reset the keybinding tree navigation to the root level.

#### Scenario: Stay in command mode
- **WHEN** the user triggers a binding with action chain `"PaneNew"` (no EnterPassthrough)
- **THEN** the command executes and the which-key menu returns to the root level
- **AND** the user can immediately press another key sequence without re-pressing the leader key

#### Scenario: Return to passthrough
- **WHEN** the user triggers a binding with action chain `"PaneNew; EnterPassthrough"`
- **THEN** the command executes and the system returns to Passthrough

### Requirement: Semicolon-separated action syntax
Action chains in TOML config SHALL be represented as semicolon-separated strings. Whitespace around semicolons SHALL be trimmed.

#### Scenario: Parse action chain
- **WHEN** a TOML value is `"ResizeLeft 5; ResizeDown 5"`
- **THEN** it is parsed as two actions: ResizeLeft(5) and ResizeDown(5)

#### Scenario: Single action without semicolon
- **WHEN** a TOML value is `"TabNew"`
- **THEN** it is parsed as a single-element action chain containing TabNew

### Requirement: Best-effort chain execution
If an action in the chain fails to execute, the system SHALL log the failure and continue executing subsequent actions in the chain.

#### Scenario: Failed action in chain
- **WHEN** an action chain is `"PaneClose; TabNew; EnterPassthrough"` and PaneClose fails (e.g., last pane in tab)
- **THEN** TabNew still executes, and EnterPassthrough still transitions to Passthrough
