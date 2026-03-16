## Why

Users currently need to memorize keybinding trees or navigate the which-key popup to execute commands. A command palette provides a faster, discoverable way to find and execute any command by typing its name with tab autocompletion — similar to VS Code's Ctrl+P or Vim's `:` command mode.

## What Changes

- Add a new command palette popup overlay that accepts text input
- Populate the palette with all available `RemuxCommand` variants
- Implement tab autocompletion that cycles through matching commands
- Execute the selected command on Enter, dismiss on Escape
- Add a keybinding to open the command palette (`:` in command mode, i.e., `<Prefix>:`)
- Add a new `RemuxCommand::CommandPalette` variant to trigger opening

## Capabilities

### New Capabilities
- `command-palette`: The popup UI, text input handling, command matching/autocompletion, and execution logic

### Modified Capabilities
None — this is purely additive. No existing specs are changed.

## Impact

- **Client input** (`client/input.rs`): New input handling for palette text entry and tab completion
- **Client UI** (`client/renderer.rs`, new `client/command_palette.rs`): Overlay rendering similar to which-key popup
- **Protocol** (`protocol.rs`): New `CommandPalette` variant in `RemuxCommand`
- **Keybindings** (`config/keybindings.rs`): New default binding, command parsing for the new variant
- **Main event loop** (`main.rs`): Route palette input actions
