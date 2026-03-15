## 1. Protocol and type changes

- [x] 1.1 Update `Mode` enum in `src/client/input.rs`: replace `Insert`, `Normal`, `Rename` with `Passthrough`, `Command`; keep `Visual`
- [x] 1.2 Update `RemuxCommand` enum in `src/protocol.rs`: replace `EnterInsertMode`/`EnterNormalMode` with `EnterPassthrough`/`EnterCommandMode`; remove `PaneRenameUpdate`/`PaneRenameCancel` (rename becomes overlay)
- [x] 1.3 Add `SendKey` variant to `RemuxCommand` for leader-leader passthrough (sends raw bytes to PTY)
- [x] 1.4 Update `ModeChanged` message values: "INSERT"→"PASSTHROUGH", "NORMAL"→"COMMAND"

## 2. Action chains

- [x] 2.1 Change `KeyNode::Leaf` action field from `String` to `Vec<String>` to support action chains
- [x] 2.2 Update `parse_command` to handle semicolon-separated action strings → `Vec<RemuxCommand>`
- [x] 2.3 Update TOML parsing in `KeybindingTree::from_toml` to split leaf values on `;` and trim whitespace
- [x] 2.4 Update `build_default_tree` to use action chains (add `EnterPassthrough` where appropriate)

## 3. Leader key configuration

- [x] 3.1 Add `leader_key: KeyEvent` field to config (default: `Ctrl-a`), parsed via `parse_key_notation`
- [x] 3.2 Add leader-leader binding at the keybinding tree root: `<leader> = ["SendKey <leader>", "EnterPassthrough"]`
- [x] 3.3 Parse `leader` key from `[keybindings.command]` TOML section

## 4. Input handler rewrite

- [x] 4.1 Rewrite `handle_key` dispatch: Passthrough forwards all keys except leader to PTY; leader enters Command mode
- [x] 4.2 Remove `handle_insert_key` and `InsertBindings` — replace with passthrough logic that only checks for leader key
- [x] 4.3 Rename `handle_normal_key` to `handle_command_key` — on leaf match, execute full action chain; reset tree to root after chain completes
- [x] 4.4 Add Escape handling in Command mode: return to Passthrough from any tree depth
- [x] 4.5 Update `handle_visual_key`: Escape returns to Passthrough instead of Normal
- [x] 4.6 Remove `handle_rename_key` — replace with inline overlay logic triggered by rename commands

## 5. Action chain execution

- [x] 5.1 Implement `execute_action_chain(actions: &[RemuxCommand])` that runs actions sequentially, logs failures, continues on error
- [x] 5.2 Handle `EnterPassthrough` and `EnterCommandMode` actions as mode transitions within the chain
- [x] 5.3 After chain completion without `EnterPassthrough`, reset `KeybindingState.current_path` to empty (root)

## 6. Rename overlay

- [x] 6.1 Implement inline text input overlay state (buffer, cursor) as a field on InputHandler rather than a mode
- [x] 6.2 When rename overlay is active, capture keystrokes for text input; Enter confirms, Escape cancels
- [x] 6.3 Wire PaneRename/TabRename commands to activate the overlay

## 7. Compositor and status bar

- [x] 7.1 Update compositor mode display: "PASSTHROUGH", "COMMAND", "VISUAL"
- [x] 7.2 Update which-key rendering to work with command mode (same tree, just renamed context)

## 8. Config and defaults

- [x] 8.1 Remove `[keybindings.insert]` TOML section support and `InsertBindings` struct
- [x] 8.2 Rename `[keybindings.normal]` to `[keybindings.command]` in TOML parsing (support both for migration with deprecation warning)
- [x] 8.3 Update default keybinding tree: remove `i` → EnterInsertMode, keep `v` → EnterVisualMode, add action chains to commonly-used bindings

## 9. Tests

- [x] 9.1 Update existing keybinding tests for action chain parsing and new command names
- [x] 9.2 Add tests for leader key detection in passthrough
- [x] 9.3 Add tests for action chain execution (single, multi, with/without EnterPassthrough)
- [x] 9.4 Add tests for leader-leader double-tap sending raw key
- [x] 9.5 Add tests for Escape exiting command mode at any tree depth
- [x] 9.6 Update protocol round-trip tests for new command variants
