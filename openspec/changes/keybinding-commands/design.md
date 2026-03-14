## Context

Remux currently has a working keybinding system with a `KeybindingTree` (trie-like structure) and `parse_action()` that converts colon-separated strings (e.g., `"tab:new"`, `"resize:left 5"`) into `RemuxCommand` enum variants. The config allows overrides via `[modes.normal.keys]` in TOML. This works but the action string format is ad-hoc, inconsistent (bare commands vs colon-delimited), and not discoverable by users.

The codebase already has the right architecture — `RemuxCommand` enum, `KeybindingTree`, merge logic, which-key popup. This change replaces the internal action string format with a user-facing command syntax, not the underlying machinery.

## Goals / Non-Goals

**Goals:**
- Replace `parse_action()` with a command parser that handles PascalCase command names + positional args
- Update TOML config format from `[modes.normal.keys]` to `[keybindings.<mode>]` (normal, visual, insert)
- Keep full backward-compatible merge behavior (user overrides on top of defaults)
- Make commands self-documenting (command names match `RemuxCommand` variants directly)

**Non-Goals:**
- Adding new commands beyond what `RemuxCommand` already supports
- Changing the `KeybindingTree` data structure or traversal logic
- Adding Lua/scripting support for custom commands
- Changing the which-key popup rendering (beyond displaying new command names)
- Supporting multiple simultaneous modifier keys (e.g., `Ctrl-Alt-x`)

## Decisions

### 1. Command names are derived directly from RemuxCommand variant names

The command name is the exact PascalCase name of the `RemuxCommand` variant (e.g., `PaneSplitVertical`, `TabGoto`, `ResizeLeft`). No separate registry or mapping table.

**Why over a separate command registry:** Zero maintenance burden — adding a variant to `RemuxCommand` automatically makes it available as a command. A mapping table would drift.

**Why over keeping colon-separated format:** The colon format (`tab:new`, `pane:split_vertical`) is an internal convention that doesn't match any user-facing concept. PascalCase matches the Rust enum directly and is more readable.

### 2. Command parsing via string split, not a full parser

Parse command strings by splitting on whitespace, where the first token is the command name and remaining tokens are positional arguments. Quoted strings (double quotes) are handled for arguments containing spaces.

**Why over a full parser (nom/pest):** The grammar is trivial — `CommandName [arg1] [arg2]`. A full parser adds a dependency for no benefit. String splitting with quote handling is ~30 lines of code.

**Why over serde-based parsing:** Commands appear as TOML string values, not structured data. Serde can't help parse the command string itself.

### 3. Config section renamed from `[modes.normal.keys]` to `[keybindings.normal]`

The new section name is shorter and more conventional. The `keys` nesting level was redundant since keybindings are the only thing defined per mode.

**Why not keep `[modes.normal.keys]`:** The `modes` section might grow to hold other mode-specific settings (e.g., cursor shape). Separating keybindings avoids conflating concerns.

### 4. Key notation uses Modifier-Key format with dashes

Keys are notated as `"Ctrl-b"`, `"Alt-n"`, `"Shift-Tab"`. Single character keys are just `"n"`, `"s"`. Special keys use their name: `"Enter"`, `"Esc"`, `"Space"`.

**Why dashes over plus signs (`Ctrl+b`):** Dashes are more common in terminal multiplexer configs (tmux, Zellij). Plus signs require quoting in some TOML contexts.

**Why single modifier only:** Multiple modifiers (`Ctrl-Alt-x`) are unreliably reported by terminal emulators. Supporting them would create bindings that silently don't work on many terminals.

### 5. Unbinding via empty string

Setting a key to `""` removes it from the merged tree. This gives users a way to disable default bindings they don't want.

**Why over a special `Unbind` command:** Empty string is visually clear (`"x" = ""` obviously means "x does nothing"), and avoids introducing a meta-command that doesn't map to a `RemuxCommand`.

### 6. Insert mode bindings are flat (no key groups) and intercept before PTY

Insert mode bindings are checked against a flat `HashMap<KeyEvent, RemuxCommand>` before the key is forwarded to the PTY. If matched, the command executes and the key is consumed. If unmatched, the key passes through to the PTY. No key groups or which-key popups in insert mode.

**Why flat, not tree:** In insert mode, every keystroke that doesn't match a binding must reach the PTY immediately. A multi-key sequence would require buffering keystrokes and introduce latency/ambiguity — did the user mean to type `Alt-h` followed by `j`, or is `Alt-h, j` a sequence? Flat bindings avoid this entirely.

**Why modifier keys only by convention (not enforced):** Defaults use `Alt-*` to avoid conflicting with normal typing, but users can bind any key. If someone binds `"x" = "PaneClose"` in insert mode, they can't type `x` — but that's their choice. No artificial restrictions.

### 7. `parse_action()` replaced, not wrapped

The existing `parse_action()` function is replaced entirely with a new `parse_command()`. The old colon-separated format is not supported as a fallback.

**Why not support both formats:** Supporting two formats doubles the testing surface, confuses documentation, and delays migration. The old format was never documented as a public API.

## Risks / Trade-offs

- **Breaking config change** → Users with existing `[modes.normal.keys]` configs will get parse errors on upgrade. Mitigation: error messages will clearly indicate the old format and point to the new one. The user base is currently just the author, so migration cost is minimal.
- **PascalCase may feel unfamiliar to some users** → Most terminal multiplexer configs use snake_case or kebab-case. Mitigation: PascalCase matches the Rust source directly, making the mapping unambiguous. Trade-off accepted for consistency.
- **No validation of argument count at config parse time** → Command argument errors only surface when the command executes. Mitigation: the parser validates argument types (string vs number) at parse time; only missing/extra arguments may slip through. Full validation can be added later without changing the format.
