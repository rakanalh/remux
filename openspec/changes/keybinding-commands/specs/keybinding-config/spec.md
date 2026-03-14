## ADDED Requirements

### Requirement: Keybindings are defined per mode in TOML config
Users SHALL define keybindings under `[keybindings.<mode>]` sections in `~/.config/remux/config.toml`. Each mode (normal, visual, insert) has its own keybinding table.

#### Scenario: Normal mode keybindings section
- **WHEN** a user adds bindings under `[keybindings.normal]`
- **THEN** those bindings SHALL only be active when Remux is in normal mode

#### Scenario: Visual mode keybindings section
- **WHEN** a user adds bindings under `[keybindings.visual]`
- **THEN** those bindings SHALL only be active when Remux is in visual mode

#### Scenario: Insert mode keybindings section
- **WHEN** a user adds bindings under `[keybindings.insert]`
- **THEN** those bindings SHALL be active when Remux is in insert mode
- **AND** the user SHALL remain in insert mode after the command executes

#### Scenario: Missing keybindings section
- **WHEN** the config file has no `[keybindings]` section
- **THEN** the system SHALL use the default keybinding set for all modes

### Requirement: Key notation supports single keys, modifiers, and sequences
Keys SHALL be specified using a string notation that supports:
- Single characters: `"n"`, `"s"`, `"/"`
- Modifier combinations: `"Ctrl-b"`, `"Alt-n"`, `"Shift-Tab"`
- Special keys: `"Enter"`, `"Esc"`, `"Tab"`, `"Space"`, `"Backspace"`
- Arrow keys: `"Up"`, `"Down"`, `"Left"`, `"Right"`
- Function keys: `"F1"` through `"F12"`

#### Scenario: Single character key binding
- **WHEN** a user writes `"n" = "TabNew"`
- **THEN** pressing `n` in the appropriate mode SHALL execute TabNew

#### Scenario: Modifier key binding
- **WHEN** a user writes `"Ctrl-n" = "TabNew"`
- **THEN** pressing Ctrl+n SHALL execute TabNew

#### Scenario: Special key binding
- **WHEN** a user writes `"Enter" = "PaneNew"`
- **THEN** pressing Enter SHALL execute PaneNew

#### Scenario: Invalid key notation
- **WHEN** a user writes `"Ctrl-Alt-Shift-x" = "TabNew"` using an unsupported modifier chain
- **THEN** the system SHALL report a config parse error

### Requirement: Bindings map keys to command strings
Each keybinding entry SHALL map a key notation string to a command string. The command string uses the command syntax defined in the command-syntax spec.

#### Scenario: Simple key-to-command binding
- **WHEN** the config contains `"n" = "TabNew"` under `[keybindings.normal]`
- **THEN** pressing `n` in normal mode SHALL execute the `TabNew` command

#### Scenario: Key-to-command-with-args binding
- **WHEN** the config contains `"1" = "TabGoto 1"` under `[keybindings.normal]`
- **THEN** pressing `1` in normal mode SHALL execute `TabGoto` with argument `1`

### Requirement: Key groups organize bindings hierarchically
A key MAY map to a TOML table instead of a command string, creating a key group. Key groups define a prefix key that waits for a subsequent key press (which-key behavior).

#### Scenario: Key group definition
- **WHEN** the config contains:
  ```toml
  [keybindings.normal.t]
  _label = "Tab"
  n = "TabNew"
  c = "TabClose"
  ```
- **THEN** pressing `t` SHALL show the which-key popup, and pressing `n` after SHALL execute `TabNew`

#### Scenario: Nested key groups
- **WHEN** a key group contains another table
- **THEN** the system SHALL support arbitrary nesting depth for key sequences

#### Scenario: Group metadata with _label
- **WHEN** a key group contains a `_label` key
- **THEN** the which-key popup SHALL display the label text for that group
- **AND** the `_label` key SHALL NOT be treated as a keybinding

### Requirement: User bindings merge with defaults
User-defined keybindings SHALL be merged on top of the default keybinding set. User bindings override defaults at the leaf level; groups merge recursively.

#### Scenario: Override a single default binding
- **WHEN** the default binds `"n"` to `TabNew` in the `t` group and the user binds `"n"` to `TabClose` in the `t` group
- **THEN** the user's binding (`TabClose`) SHALL take effect, and all other default `t` group bindings SHALL remain

#### Scenario: Add a new binding to an existing group
- **WHEN** the user adds `"x" = "TabClose"` to the `t` group
- **THEN** the `t` group SHALL contain both the user's `x` binding and all default bindings

#### Scenario: Add a new top-level binding
- **WHEN** the user adds `"Ctrl-n" = "TabNew"` at the top level
- **THEN** the new binding SHALL be available alongside all default bindings

#### Scenario: Remove a default binding
- **WHEN** the user binds a key to the empty string `""`
- **THEN** the system SHALL remove that binding (unbind the key)

### Requirement: Default keybinding set covers all standard operations
The system SHALL provide a default keybinding set for normal mode that covers pane, tab, session, folder, buffer, resize, mode, and layout operations. Defaults SHALL use the key group hierarchy with the following top-level groups:

- `t` — Tab operations
- `p` — Pane operations
- `s` — Session operations
- `f` — Folder operations
- `b` — Buffer operations
- `r` — Resize operations
- `i` — `EnterInsertMode` (direct binding)
- `v` — `EnterVisualMode` (direct binding)
- `g` — `ToggleGaps` (direct binding)

#### Scenario: Default bindings are functional without config
- **WHEN** a user has no `[keybindings]` section in their config
- **THEN** all default keybindings SHALL be active and functional

#### Scenario: Default pane split bindings
- **WHEN** a user presses `p` then `s` in normal mode with defaults
- **THEN** the system SHALL execute `PaneSplitVertical`

#### Scenario: Default pane focus bindings
- **WHEN** a user presses `p` then `h` in normal mode with defaults
- **THEN** the system SHALL execute `PaneFocusLeft`

### Requirement: Insert mode bindings intercept before PTY forwarding
In insert mode, the system SHALL check incoming key events against `[keybindings.insert]` before forwarding them to the PTY. If a key matches an insert mode binding, the command SHALL execute and the key SHALL NOT be forwarded to the PTY. Unmatched keys SHALL be forwarded to the PTY as normal.

#### Scenario: Insert mode binding intercepts key
- **WHEN** the user presses `Alt-h` in insert mode and `"Alt-h" = "PaneFocusLeft"` is configured
- **THEN** the system SHALL execute `PaneFocusLeft` and NOT send Alt-h to the PTY
- **AND** the user SHALL remain in insert mode

#### Scenario: Insert mode unbound key passes through
- **WHEN** the user presses `Alt-x` in insert mode and no binding exists for `Alt-x`
- **THEN** the key SHALL be forwarded to the PTY as normal

#### Scenario: Insert mode bindings do not support key groups
- **WHEN** a user attempts to define a key group (TOML table) under `[keybindings.insert]`
- **THEN** the system SHALL report a config parse error, since insert mode bindings MUST be flat (single key → command) to avoid ambiguity with PTY input

### Requirement: Default insert mode bindings for pane navigation
The system SHALL provide default insert mode bindings using Alt-modifier keys for common pane operations:

- `Alt-h` — `PaneFocusLeft`
- `Alt-j` — `PaneFocusDown`
- `Alt-k` — `PaneFocusUp`
- `Alt-l` — `PaneFocusRight`
- `Alt-n` — `TabNext`
- `Alt-p` — `TabPrev`

#### Scenario: Default insert mode pane focus
- **WHEN** a user presses `Alt-h` in insert mode with default bindings
- **THEN** the system SHALL execute `PaneFocusLeft` and remain in insert mode

#### Scenario: User overrides default insert mode binding
- **WHEN** a user adds `"Alt-h" = "TabPrev"` under `[keybindings.insert]`
- **THEN** pressing `Alt-h` in insert mode SHALL execute `TabPrev` instead of the default `PaneFocusLeft`
