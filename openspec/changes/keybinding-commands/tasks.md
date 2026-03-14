## 1. Command Parser

- [x] 1.1 Implement `parse_command(input: &str) -> Result<RemuxCommand>` that splits on whitespace, matches PascalCase command name to `RemuxCommand` variant, and parses positional arguments (with double-quote support for strings)
- [x] 1.2 Add unit tests for `parse_command`: parameterless commands, numeric args, string args, multi-arg commands, optional defaults (e.g., `ResizeLeft` → amount=1), invalid command name, invalid arg type

## 2. Config Format Migration

- [x] 2.1 Rename config section from `[modes.normal.keys]` to `[keybindings.normal]`, `[keybindings.visual]`, and `[keybindings.insert]` in the `Config` struct and TOML deserialization
- [x] 2.2 Implement key notation parser: single chars, `Ctrl-x`/`Alt-x`/`Shift-x` modifiers, special keys (`Enter`, `Esc`, `Tab`, `Space`, `Backspace`, arrow keys, `F1`-`F12`)
- [x] 2.3 Add unit tests for key notation parsing: valid single keys, modifier combos, special keys, invalid notation

## 3. Keybinding Tree Update

- [x] 3.1 Update `KeybindingTree::default()` to use PascalCase command strings instead of colon-separated action strings (e.g., `"PaneSplitVertical"` instead of `"pane:split_vertical"`)
- [x] 3.2 Replace `parse_action()` calls with `parse_command()` in the leaf-action resolution path (`src/client/input.rs`)
- [x] 3.3 Support unbinding via empty string — when merging user config, a key mapped to `""` removes it from the tree

## 4. Insert Mode Bindings

- [x] 4.1 Build flat `HashMap<KeyEvent, RemuxCommand>` from `[keybindings.insert]` config (reject key groups with parse error)
- [x] 4.2 Add default insert mode bindings: `Alt-h`/`Alt-j`/`Alt-k`/`Alt-l` for pane focus, `Alt-n`/`Alt-p` for tab next/prev
- [x] 4.3 In insert mode input handler (`src/client/input.rs`), check incoming key against insert bindings before forwarding to PTY — if matched, execute command and consume key; remain in insert mode
- [x] 4.4 Add tests: insert binding intercepts key, unbound key passes through to PTY, command executes without leaving insert mode

## 5. Which-Key Display

- [x] 5.1 Update which-key popup to display PascalCase command names for leaf bindings instead of colon-separated action strings

## 6. Cleanup

- [x] 6.1 Remove old `parse_action()` function and its tests
- [x] 6.2 Update or create example config snippet in comments/docs showing the new `[keybindings.normal]` and `[keybindings.insert]` format
