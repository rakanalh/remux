## Context

Remux currently uses a vim-style modal system where Escape toggles between Insert and Normal modes. This conflicts with inner terminal applications (Vim, Neovim, Emacs evil-mode) that also use Escape. The current system has four modes (Insert, Normal, Visual, Rename) with Insert mode having its own flat keybinding map for Alt-based shortcuts.

The leader key approach (inspired by tmux's prefix key and vim's leader key) eliminates the Escape conflict by intercepting only a single key combination in passthrough state.

## Goals / Non-Goals

**Goals:**
- Eliminate the Escape key conflict with inner terminal applications
- Support action chains (multiple actions per keybinding) with explicit mode transitions
- Maintain the existing keybinding tree structure for command navigation
- Keep the leader key configurable via TOML config
- Allow "sticky" command mode for rapid-fire operations (create pane, resize, move — all without re-pressing leader)
- Preserve Visual mode for scrollback navigation and text selection

**Non-Goals:**
- Custom per-mode keybinding trees (command mode uses one tree; visual mode has hardcoded keys)
- Chord detection or simultaneous key combinations beyond modifier keys
- Leader key timeout (no time-based fallback — leader always enters command mode)
- Rename mode redesign (will be handled as a command that opens inline input, not a separate mode)

## Decisions

### 1. Three-mode system: Passthrough, Command, Visual

**Decision**: Replace the four-mode system (Insert, Normal, Visual, Rename) with three modes (Passthrough, Command, Visual).

**Rationale**: "Insert mode" is a misnomer — the terminal application handles its own input modes. Remux's job is to either pass keys through or intercept them. Rename mode becomes an inline input triggered by a command, not a top-level mode.

**Alternatives considered**:
- Keep four modes with leader key: Adds complexity without benefit. Rename is better as a command overlay.
- Two modes (Passthrough + Command, merge Visual into Command): Visual mode has fundamentally different keybindings (scrollback navigation) that don't belong in the command tree.

### 2. Action chains replace single actions

**Decision**: Each keybinding leaf maps to a `Vec<String>` of action descriptors instead of a single `String`. Actions execute in sequence. The semicolon `;` separates actions in config strings.

Example: `"PaneNew; EnterPassthrough"` creates a pane and returns to passthrough.
Example: `"PaneNew"` creates a pane and stays in command mode (which-key resets to root).

**Rationale**: This makes mode transitions explicit and composable. The user controls when to return to passthrough, enabling rapid-fire command sequences.

**Alternatives considered**:
- Implicit mode return after every command: Loses the "sticky command mode" capability. Users would need to press leader before every command.
- Special "sticky" flag per binding: More complex config, less intuitive than just omitting EnterPassthrough.

### 3. Leader key stored as KeyEvent, not char

**Decision**: Store the leader key as a `crossterm::event::KeyEvent` (key code + modifiers) rather than a plain character. Default: `KeyEvent { code: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL }`.

**Rationale**: The leader key must match modifier combinations (Ctrl+a, Ctrl+b). A plain char cannot represent this. The existing `parse_key_notation` function already parses strings like `"Ctrl-a"` into `KeyEvent`.

### 4. Leader-leader sends raw key to PTY

**Decision**: In command mode, pressing the leader key again is a built-in binding at the root level: it sends the leader key's byte sequence to the active PTY and returns to passthrough. This is equivalent to a root-level binding `<leader> = "SendKey <leader>; EnterPassthrough"`.

**Rationale**: Since the leader key is stolen from the inner application, there must be a way to send it. Double-tap is the established convention (tmux `Ctrl-b Ctrl-b`).

### 5. Escape in command mode returns to passthrough

**Decision**: Pressing Escape while in command mode (at any depth in the keybinding tree) returns to passthrough. This is hardcoded, not configurable.

**Rationale**: Users expect Escape to cancel/exit. Since Escape is no longer used to enter command mode, it's free to serve as the "cancel and go back to passthrough" key. No conflict with inner applications because they don't receive keys in command mode.

### 6. Remove InsertBindings entirely

**Decision**: Remove the `InsertBindings` struct and all Alt-based passthrough shortcuts. In passthrough, only the leader key is intercepted.

**Rationale**: Alt-based shortcuts in passthrough steal keys from inner applications (Alt-h conflicts with some TUI apps). With the leader key approach, all Remux commands go through the command tree: `<leader> p h` replaces `Alt-h` for pane focus left.

### 7. TOML config format for action chains

**Decision**: In TOML config, action chains are semicolon-separated strings:

```toml
[keybindings.command]
leader = "Ctrl-a"

[keybindings.command.t]
_label = "Tab"
n = "TabNew; EnterPassthrough"
c = "TabClose; EnterPassthrough"

[keybindings.command.p]
_label = "Pane"
n = "PaneNew; EnterPassthrough"
```

**Rationale**: Semicolon separation keeps the TOML simple (single string values). No schema change needed for the TOML structure — only the parsing of leaf values changes to split on `;`.

### 8. Rename as command overlay, not a mode

**Decision**: `PaneRename` and `TabRename` commands activate an inline text input overlay within command mode. The overlay captures keystrokes until Enter (confirm) or Escape (cancel), then returns to command mode (or passthrough if the action chain included EnterPassthrough after the rename command).

**Rationale**: Rename is a transient text input, not a persistent mode. It doesn't need its own mode enum variant or keybinding set.

## Risks / Trade-offs

- **Muscle memory disruption**: Users accustomed to Escape-based mode switching will need to relearn. → Mitigation: This is early in development, few users affected. The leader key is configurable.
- **Two-key overhead**: Every command requires leader + key(s) instead of just Escape + key(s). → Mitigation: Sticky command mode means leader is pressed once for multiple commands. For single commands with EnterPassthrough, it's the same number of keys as tmux.
- **Leader key conflicts with Ctrl-a (readline)**: Ctrl-a is "beginning of line" in bash/readline. → Mitigation: `<leader><leader>` sends the raw Ctrl-a. Users can also configure a different leader key. This is the same trade-off tmux makes with Ctrl-b.
- **Action chain execution errors**: If an action in the middle of a chain fails, should subsequent actions still execute? → Decision: Yes, best-effort execution. Each action is independent. Log failures but continue the chain.
