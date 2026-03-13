## Why

Terminal multiplexers like tmux and zellij each get some things right but none nail the full experience. tmux lacks stackable panes and scrollback editing. Zellij lacks session resurrection. Neither offers composable, discoverable keybindings with a which-key interface. Remux combines the best of both with a modal, vim-inspired command system built in Rust.

## What Changes

- New terminal multiplexer application built from scratch in Rust
- Client-server architecture with Unix socket IPC (sessions persist across detach/attach)
- Hierarchical session model: Folder (optional) → Session → Tab → Split Tree → Pane Stack → Pane
- Zellij-style stackable panes where hidden panes keep running but don't render
- Vertical and horizontal splits with constraint-based layout (cassowary)
- Three input modes: Insert (passthrough), Normal (command tree), Visual (scrollback selection)
- Emacs/vim-style hierarchical keybinding groups with configurable which-key popup
- Read-only scrollback buffer editing in $EDITOR
- Automatic session resurrection (layout + cwd persistence)
- Zellij-style pane frames with option to configure tmux-style status bar
- TOML-based configuration for keybindings, appearance, and behavior

## Capabilities

### New Capabilities
- `pty-management`: PTY allocation, child process spawning, I/O multiplexing via nix/libc
- `layout-engine`: Binary split tree with cassowary constraint solving, pane stacks, resize operations
- `session-model`: Folder/session/tab hierarchy, creation, deletion, navigation
- `modal-input`: Insert/Normal/Visual mode state machine, key event routing
- `keybinding-tree`: Hierarchical keybinding groups, TOML config parsing, which-key popup rendering
- `terminal-rendering`: Crossterm-based frame rendering, VTE parsing per pane, diff-based screen updates
- `client-server`: Unix socket IPC, daemon lifecycle, multi-client attach, message protocol
- `session-persistence`: Auto-save state (layout + cwd) on interval, resurrection on server start
- `scrollback`: Per-pane scrollback buffer, Visual mode navigation, export to $EDITOR (readonly)
- `configuration`: TOML config loading, theme/appearance settings, frame style toggle

### Modified Capabilities

(none — greenfield project)

## Impact

- New Rust binary crate with ~15 dependencies (crossterm, vte, nix, tokio, cassowary, etc.)
- Unix socket at /tmp/remux-$UID/remux.sock for IPC
- State file at ~/.local/share/remux/state.json for persistence
- Config file at ~/.config/remux/config.toml
- CLI commands: remux, remux new, remux attach, remux ls, remux kill
