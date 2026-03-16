## 1. Protocol & Config

- [x] 1.1 Add `CommandPalette` variant to `Mode` enum in `client/input.rs`
- [x] 1.2 Add `command_names() -> Vec<&'static str>` function to `protocol.rs` that returns all `RemuxCommand` variant names
- [x] 1.3 Add `:` keybinding in command mode to trigger the palette (`<Prefix>:`) (add `InputAction` variants: `CommandPaletteOpen`, `CommandPaletteUpdate`, `CommandPaletteComplete`, `CommandPaletteExecute`, `CommandPaletteClose`)

## 2. Command Palette State

- [x] 2.1 Create `src/client/command_palette.rs` module with `CommandPaletteState` struct (input buffer, filtered matches, selected index, tab-cycle state)
- [x] 2.2 Implement `filter_commands()` — case-insensitive substring matching against the command name list
- [x] 2.3 Implement `tab_complete()` — longest common prefix completion, then cycle through matches on subsequent presses
- [x] 2.4 Implement `render()` — produce `DrawCommand` list for the popup overlay (reuse theme whichkey colors, centered layout)

## 3. Input Handling

- [x] 3.1 Add `handle_command_palette_key()` method to `InputHandler` — route character input, Tab, Shift+Tab, Enter, Escape, Backspace
- [x] 3.2 Wire `Mode::CommandPalette` into `handle_key()` dispatch in `InputHandler`
- [x] 3.3 On Enter, parse the selected/typed command via `parse_command()` and return `InputAction::Execute`

## 4. Rendering

- [x] 4.1 Add `render_command_palette_overlay()` method to `Renderer` using draw commands from `CommandPaletteState::render()`
- [x] 4.2 Add `clear_command_palette_overlay()` to restore underlying content when palette closes

## 5. Event Loop Integration

- [x] 5.1 Wire palette input actions in the main client event loop (`main.rs`) — open, update, close, execute
- [x] 5.2 Send `ModeChanged` message to server when entering/exiting palette mode so status bar reflects the mode

## 6. Argument Support

- [x] 6.1 Extend palette to detect space after command name and pass remaining text as argument to `parse_command()`
- [x] 6.2 Show argument hint in the palette UI for commands that accept parameters
