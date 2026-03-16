## MODIFIED Requirements

### Requirement: Keybindings are defined per mode in TOML config
Users SHALL define keybindings under `[keybindings.<mode>]` sections in `~/.config/remux/config.toml`. Each mode (command, visual) has its own keybinding table.

#### Scenario: Command mode keybindings section
- **WHEN** a user adds bindings under `[keybindings.command]`
- **THEN** those bindings SHALL only be active when Remux is in Command mode

#### Scenario: Visual mode keybindings section
- **WHEN** a user adds bindings under `[keybindings.visual]`
- **THEN** those bindings SHALL only be active when Remux is in Visual mode

#### Scenario: Normal mode keybindings section
- **WHEN** a user adds bindings under `[keybindings.command]`
- **THEN** those bindings SHALL define shortcuts active in Normal mode
- **AND** the user SHALL remain in Normal mode after direct command execution

#### Scenario: Missing keybindings section
- **WHEN** the config file has no `[keybindings]` section
- **THEN** the system SHALL use the default keybinding set for all modes

### Requirement: Shortcut bindings are flat modifier-key mappings
Shortcut bindings SHALL be a flat table mapping key notation strings to command strings or group prefix references. Key notation strings MUST include at least one modifier. TOML tables (key groups) SHALL NOT be allowed under `[keybindings.command]`.

#### Scenario: Valid command binding
- **WHEN** the config contains `"Alt-h" = "PaneFocusLeft"` under `[keybindings.command]`
- **THEN** the system SHALL register the shortcut binding

#### Scenario: Valid command group binding
- **WHEN** the config contains `"Alt-p" = "@p"` under `[keybindings.command]`
- **THEN** the system SHALL register the binding as a group prefix shortcut

#### Scenario: Shortcut binding without modifier rejected
- **WHEN** the config contains `"p" = "PaneFocusLeft"` under `[keybindings.command]`
- **THEN** the system SHALL report a config parse error

#### Scenario: Shortcut key group rejected
- **WHEN** a user attempts to define a TOML table under `[keybindings.command]`
- **THEN** the system SHALL report a config parse error

## REMOVED Requirements

### Requirement: Insert mode bindings intercept before PTY forwarding
**Reason**: Insert mode has been replaced by Normal mode. The shortcut-before-forwarding behavior is now handled by the shortcut-bindings capability.
**Migration**: Move `[keybindings.insert]` entries to `[keybindings.command]`. Behavior is identical — modifier-based flat bindings checked before PTY forwarding.

### Requirement: Default insert mode bindings for pane navigation
**Reason**: Replaced by default shortcut bindings defined in the shortcut-bindings capability.
**Migration**: The same Alt-based shortcuts are provided as default shortcut bindings. `Alt-p` changes from `TabPrev` to `@p` (open Pane group). `Alt-t` is added for the Tab group.
