## Why

Remux currently uses internal action strings (e.g., `"tab:new"`, `"pane:split_vertical"`) for keybinding configuration. Users need a clear, discoverable command system — similar to Zellij — where bindings explicitly call named commands with arguments (e.g., `PaneSplit "right"`). This makes the config more intuitive, self-documenting, and extensible.

## What Changes

- **New command syntax**: All user-facing actions become named commands with optional arguments (e.g., `PaneSplit "vertical"`, `TabGoto 3`, `SessionNew "dev"`). Commands use PascalCase names matching the `RemuxCommand` enum variants.
- **Config file keybinding section**: Users define bindings in `~/.config/remux/config.toml` under `[keybindings]` sections per mode (normal, visual, and insert). Each binding maps a key or key sequence to a command string.
- **Insert mode bindings**: Users can bind modifier keys (e.g., `Alt-h`, `Alt-l`) in `[keybindings.insert]` to execute commands without leaving insert mode. This enables ergonomic pane navigation, tab switching, etc. while typing.
- **Key sequence support**: Support for modifier keys (`Ctrl`, `Alt`, `Shift`) and multi-key sequences in the config format.
- **Command documentation**: Each command has a defined name, accepted arguments, and description that can be referenced by users and surfaced in the which-key popup.
- **BREAKING**: The current `[modes.normal.keys]` config format with colon-separated action strings (e.g., `"tab:new"`) is replaced by the new command syntax.

## Capabilities

### New Capabilities

- `command-syntax`: Defines the command naming convention, argument format, and the full catalog of available commands (pane, tab, session, folder, buffer, resize, mode, layout operations).
- `keybinding-config`: Defines the TOML config format for binding keys to commands, including key notation (modifiers, sequences), mode-scoped bindings, default bindings, and merge behavior with user overrides.

### Modified Capabilities

_(No existing specs to modify — `openspec/specs/` is empty)_

## Impact

- **Config format**: Breaking change to `[modes.normal.keys]` — existing user configs need migration to new command syntax.
- **Code affected**:
  - `src/config/keybindings.rs` — `parse_action()` rewritten to parse new command syntax; `KeybindingTree` construction updated.
  - `src/config/mod.rs` — Config deserialization updated for new keybinding format.
  - `src/protocol.rs` — `RemuxCommand` enum may gain new variants or be reorganized.
  - `src/client/input.rs` — Which-key popup updated to show command names; insert mode input handler updated to intercept modifier bindings before forwarding to PTY.
- **No server-side changes**: Commands already flow through `RemuxCommand` enum; only the parsing/config layer changes.
