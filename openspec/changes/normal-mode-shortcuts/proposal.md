## Why

Normal mode only intercepts the leader key, forwarding everything else to the PTY. This means modifier-based shortcuts (Alt-h, Alt-p, etc.) that previously worked in the old "insert mode" no longer function. Users need quick, single-keystroke access to common operations (pane navigation, opening keybinding groups) without going through the leader key sequence every time.

## What Changes

- Add a configurable set of **modifier shortcut bindings** — modifier-based key combinations (Alt, Ctrl+Alt, Alt+Shift, etc.) that are checked in Normal mode before forwarding to the PTY
- Shortcut bindings can map to either a direct command (`Alt-h → PaneFocusLeft`) or a keybinding tree group prefix (`Alt-p → open Pane group in Command mode`)
- Default shortcut bindings restore the Alt-based pane navigation shortcuts and add group shortcuts
- **BREAKING**: The old `[keybindings.insert]` config section (if still referenced) is replaced by `[keybindings.command]`

## Capabilities

### New Capabilities
- `shortcut-bindings`: Defines the shortcut binding system — how modifier-based shortcuts are configured, matched, and dispatched in Normal mode, including mapping to commands and keybinding tree groups

### Modified Capabilities
- `keybinding-config`: Add `[keybindings.command]` section for configuring shortcut bindings; remove references to insert mode bindings
- `mode-system`: Normal mode now checks shortcut bindings before forwarding keys to PTY

## Impact

- `src/config/keybindings.rs` — New data structure for shortcut bindings, TOML parsing for `[keybindings.command]`
- `src/client/input.rs` — Normal mode key handler gains shortcut logic; needs ability to enter Command mode at arbitrary tree depth
- `src/client/whichkey.rs` — May need to handle being opened at a non-root tree position
- Config file format — New `[keybindings.command]` section for shortcut bindings
