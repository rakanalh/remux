## Why

The current mode system uses Escape to toggle between Insert and Normal mode, which conflicts with inner terminal applications (e.g., Vim) that also use Escape. This makes Remux unusable with modal editors without awkward workarounds. A leader-key based system (like tmux's prefix key) eliminates this conflict while preserving the vim-style keybinding tree.

## What Changes

- **BREAKING**: Replace the Escape-based Insert↔Normal mode toggle with a configurable leader key (default: `Ctrl+a`)
- **BREAKING**: Remove "Insert Mode" — replace with **Passthrough** (default state where all keys go to the inner application, only the leader key is intercepted)
- **BREAKING**: Rename "Normal Mode" to **Command Mode** — entered via leader key, navigates the keybinding tree
- Leader key double-tap (`<leader><leader>`) sends the raw leader key to the inner application
- Keybindings map to **action chains** — ordered sequences of actions (e.g., `NewPane; EnterPassthrough`) rather than single actions
- If an action chain does not contain `EnterPassthrough`, the user stays in Command Mode and the which-key tree resets to root for further commands
- If an action chain contains `EnterPassthrough`, execution completes and returns to passthrough
- Visual Mode remains, entered via `<leader>v`
- Remove Alt-based quick commands from passthrough (no longer needed — use leader key sequences instead)

## Capabilities

### New Capabilities
- `leader-key`: Configurable leader key, double-tap passthrough, leader key detection in passthrough mode
- `action-chains`: Keybinding actions as ordered sequences with mode transitions as explicit actions in the chain
- `mode-system`: Three-mode system (Passthrough, Command, Visual) replacing the current four-mode system (Insert, Normal, Visual, Rename)

### Modified Capabilities
- `modal-input`: Fundamental change to mode transitions — Escape no longer switches modes, leader key replaces it

## Impact

- `src/client/input.rs`: Complete rewrite of mode handling, key dispatch, and passthrough logic
- `src/config/keybindings.rs`: Action chains replace single actions, leader key configuration, remove InsertBindings
- `src/protocol.rs`: Update `ModeChanged` variants (Insert→Passthrough, Normal→Command), `RemuxCommand` variants for new actions
- `src/server/compositor.rs`: Update status bar mode display names
- Default keybinding configuration: All bindings need updating to use action chains
- **Breaking change** for any existing user configuration
