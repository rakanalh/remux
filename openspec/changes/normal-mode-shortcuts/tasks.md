## 1. Data Structures

- [x] 1.1 Add `NormalizedKeyEvent` wrapper type in `src/config/keybindings.rs` with `Hash`, `Eq`, and conversion from `crossterm::KeyEvent` (normalizing `kind`/`state` fields away)
- [x] 1.2 Add `InterceptAction` enum with `Command(Vec<String>)` and `GroupPrefix(Vec<char>)` variants
- [x] 1.3 Add `ShortcutBindings` struct with `HashMap<NormalizedKeyEvent, InterceptAction>` and `lookup(&self, key: &KeyEvent) -> Option<&InterceptAction>` method

## 2. Default Bindings

- [x] 2.1 Implement `Default for ShortcutBindings` with the default shortcut set: Alt-h/j/k/l for pane focus, Alt-n for TabNext, Alt-p for `@p`, Alt-t for `@t`
- [x] 2.2 Add unit tests for default bindings lookup

## 3. TOML Parsing

- [x] 3.1 Add `ShortcutBindings::from_toml()` parser that reads flat key-notation → value entries from a TOML table, distinguishing `@`-prefixed group references from command strings
- [x] 3.2 Add validation: reject entries without a modifier, reject TOML table values (key groups)
- [x] 3.3 Add merge logic: user shortcut bindings merge on top of defaults, empty string unbinds
- [x] 3.4 Wire TOML parsing into config loading — read `[keybindings.command]` section
- [x] 3.5 Add unit tests for TOML parsing, merge, and validation errors

## 4. Input Handler Integration

- [x] 4.1 Add `shortcut_bindings: ShortcutBindings` field to `InputHandler`
- [x] 4.2 Modify `handle_normal_key` (Normal mode handler) to check shortcut bindings before leader key and PTY forwarding
- [x] 4.3 For `InterceptAction::Command` — execute the action chain and return commands, staying in Normal mode
- [x] 4.4 For `InterceptAction::GroupPrefix` — enter Command mode with the keybinding state pre-navigated to the target group path

## 5. Command Mode Group Entry

- [x] 5.1 Add method to `KeybindingState` (or equivalent) to initialize at a given tree path instead of root
- [x] 5.2 Ensure which-key popup displays the target group's children when entering Command mode via group prefix
- [x] 5.3 Ensure Escape from group-entered Command mode returns to Normal mode (not tree root)
- [x] 5.4 Add unit tests for entering Command mode at non-root tree positions

## 6. Validation

- [x] 6.1 Add post-load validation that `@<key>` group references resolve to actual groups in the keybinding tree
- [x] 6.2 Add config error reporting for invalid group references

## 7. Cleanup

- [x] 7.1 Remove old insert-mode binding references from `keybindings.rs` if any remain
- [x] 7.2 Update `InputHandler::new` to accept and store `ShortcutBindings`
