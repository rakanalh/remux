## Context

Remux's mode system was refactored from Normal/Insert/Visual to Normal/Command/Visual. Normal mode forwards all keys to the PTY except the leader key (`Ctrl-a`). The old insert mode had flat Alt-modifier bindings (`Alt-h` → `PaneFocusLeft`, etc.) that were checked before PTY forwarding — these no longer function since the shortcut logic was removed with insert mode.

The keybinding tree (`KeybindingTree`) uses `HashMap<char, KeyNode>` and only supports plain character keys. Command mode's `handle_command_key` explicitly rejects keys with Alt/Ctrl modifiers. This means modifier-based shortcuts cannot participate in the tree system without changes.

## Goals / Non-Goals

**Goals:**
- Restore modifier-based shortcuts in Normal mode (Alt-h/j/k/l for pane nav)
- Enable shortcuts that jump directly into keybinding tree groups (Alt-p → Pane group)
- Support Alt, Shift, and compound modifiers (Alt-Shift-h) in shortcut bindings
- User-configurable via `[keybindings.command]` with merge-over-defaults semantics

**Non-Goals:**
- Changing the keybinding tree key type from `char` to `KeyEvent` — the tree stays char-indexed for Command mode
- Supporting key groups/sequences in shortcut bindings — they remain flat
- Mouse-based shortcuts

## Decisions

### Shortcut binding map: `HashMap<KeyEvent, InterceptAction>`

A new data structure alongside the keybinding tree, not inside it. Shortcut bindings map full `KeyEvent` values (with modifiers) to actions, unlike the char-indexed tree used in Command mode.

```rust
enum InterceptAction {
    /// Execute commands and stay in Normal mode.
    Command(Vec<String>),
    /// Enter Command mode at the given tree path.
    GroupPrefix(Vec<char>),
}

struct ShortcutBindings {
    bindings: HashMap<NormalizedKeyEvent, InterceptAction>,
}
```

**Why not extend the keybinding tree?** The tree is designed for sequential key navigation in Command mode. Normal mode shortcuts are single-keystroke with modifiers — fundamentally different interaction pattern. Keeping them separate avoids complexity in the tree traversal logic and the which-key display.

**KeyEvent normalization:** `crossterm::KeyEvent` includes `kind` and `state` fields that vary across terminals. We normalize to just `(KeyCode, KeyModifiers)` for hashing and comparison.

### Group prefix syntax: `@<key-path>`

In TOML config, `"Alt-p" = "@p"` means "enter Command mode at group `p`". The `@` prefix distinguishes group references from command strings. Multi-level paths use dots: `"@t.s"` would navigate to a nested subgroup (future-proof, not needed for defaults).

**Why `@`?** It's visually distinct from command names (PascalCase), doesn't conflict with any command syntax, and reads naturally as "at group p".

### Shortcut check order in Normal mode

```
key event → shortcut bindings → leader key check → forward to PTY
```

Shortcut bindings are checked first because they use modifier keys that would never match the leader key (leader is `Ctrl-a`, shortcuts use `Alt-*`). Checking shortcuts first avoids an unnecessary leader comparison for the common case of Alt-shortcuts.

### Require modifiers on shortcut bindings

Shortcut bindings MUST have at least one modifier (Alt, Ctrl, or Ctrl-Alt). Plain character keys (`"p" = "..."`) are rejected at config parse time. This prevents accidentally capturing keys meant for the terminal application.

Shift-only is allowed for special keys (e.g., `Shift-Tab`) but not for plain characters (since `Shift-h` is just `H`, which the PTY should receive).

### Default bindings

```toml
[keybindings.command]
"Alt-h" = "PaneFocusLeft"
"Alt-j" = "PaneFocusDown"
"Alt-k" = "PaneFocusUp"
"Alt-l" = "PaneFocusRight"
"Alt-n" = "TabNext"
"Alt-p" = "@p"
"Alt-t" = "@t"
```

Changed from old insert-mode defaults: `Alt-p` was `TabPrev`, now opens the Pane group. `Alt-t` is new for the Tab group. The hjkl navigation and `Alt-n` remain the same.

## Risks / Trade-offs

- **Alt key conflicts with terminal apps:** Some TUI apps (vim, emacs, mc) use Alt-key combos. Users can unbind with `"Alt-h" = ""` if needed. This is the same trade-off tmux makes with its prefix key.
- **KeyEvent hashing:** `crossterm::KeyEvent` doesn't implement `Hash`. We need a wrapper type (`NormalizedKeyEvent`) with manual Hash/Eq on `(KeyCode, KeyModifiers)`.
- **Group validation at load time:** `@p` must reference a valid group in the keybinding tree. Since shortcut bindings and the tree are parsed separately, validation happens after both are loaded.
