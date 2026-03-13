## Context

Remux is a new terminal multiplexer built in Rust, combining the best features of tmux (session persistence) and zellij (stackable panes, frames) while introducing a novel modal keybinding system with which-key discoverability. There is no existing codebase — this is a greenfield project.

The target user is a power user comfortable with vim/emacs-style modal interfaces who wants a terminal multiplexer that is both powerful and discoverable.

## Goals / Non-Goals

**Goals:**
- Fully functional terminal multiplexer with client-server architecture
- Stackable panes within a split tree layout
- Modal input system (Insert/Normal/Visual) with hierarchical keybinding groups
- Which-key popup for keybinding discoverability
- Automatic session resurrection (layout + cwd)
- Read-only scrollback editing in $EDITOR
- Configurable appearance (zellij-style frames or tmux-style status bar)
- TOML-based configuration

**Non-Goals:**
- Plugin system (bake features in for now)
- Scrollback preservation across restarts
- Process state resurrection (only layout + cwd)
- Mouse support (may add later)
- Sixel/image protocol support
- Windows support (Unix/Linux only)
- Lua scripting

## Decisions

### 1. Direct crossterm rendering over TUI framework
**Decision**: Render directly via crossterm, no ratatui or other TUI framework.
**Rationale**: Zellij proves this works for terminal multiplexers. TUI frameworks add abstraction that fights against the need to relay raw VT output from child processes. Direct rendering gives full control over escape sequence passthrough and diff-based screen updates.
**Alternatives**: ratatui (too high-level for multiplexer rendering), notcurses (C library, FFI overhead).

### 2. VTE crate for terminal parsing
**Decision**: Use the `vte` crate (alacritty's parser) for parsing escape sequences from child processes.
**Rationale**: Battle-tested in alacritty, fast, and used by zellij. Each pane maintains a virtual screen buffer updated by feeding PTY output through the VTE parser.
**Alternatives**: `vt100` crate (higher-level but less control, less battle-tested at scale).

### 3. nix + libc for PTY management
**Decision**: Use `nix` and `libc` directly for PTY allocation instead of wrapper crates.
**Rationale**: Zellij uses this approach. Direct POSIX calls give full control over file descriptors, signal handling, and process groups. Wrapper crates like `portable-pty` add abstraction we don't need since we're Unix-only.

### 4. Cassowary constraint solver for layout
**Decision**: Use the `cassowary` crate for computing pane dimensions from the split tree.
**Rationale**: Zellij uses cassowary for its layout engine. Constraint solving handles resize propagation naturally — when a split ratio changes or the terminal resizes, constraints propagate correct sizes to all panes without manual calculation.

### 5. Binary split tree for layout model
**Decision**: Layout is a binary tree where internal nodes are splits (horizontal/vertical with a ratio) and leaf nodes are pane stacks.
**Rationale**: Simple recursive data structure that naturally represents any combination of splits. Resize operations walk the tree. Zellij uses a similar model.

```
enum LayoutNode {
    Split { direction: Direction, ratio: f32, first: Box<LayoutNode>, second: Box<LayoutNode> },
    Stack { panes: Vec<PaneId>, active: usize },
}
```

### 6. Interprocess crate for Unix socket IPC
**Decision**: Use `interprocess` for client-server communication over Unix sockets.
**Rationale**: Zellij uses this. Handles Unix socket creation, connection, and cleanup. The protocol will use serde_json-serialized messages for simplicity (protobuf is overkill for our scope).

### 7. Tokio async runtime
**Decision**: Use tokio for async I/O multiplexing across PTYs and the socket listener.
**Rationale**: Each pane's PTY output is read in a separate tokio task. The server event loop selects across PTY reads, client messages, and timers (auto-save). Tokio is the standard and zellij uses it.

### 8. Modal input with keybinding tree in TOML
**Decision**: Three modes (Insert/Normal/Visual). In Normal mode, keys are interpreted as commands via a hierarchical tree structure defined in TOML config.
**Rationale**: Eliminates the leader-key conflict problem entirely. TOML nested tables map naturally to the keybinding tree. Each table with a `_label` key becomes a which-key group.

### 9. JSON for state persistence
**Decision**: Persist session state as JSON at ~/.local/share/remux/state.json with configurable auto-save interval.
**Rationale**: JSON is human-readable for debugging, serde_json is already a dependency, and the state structure (folders/sessions/tabs/layouts/cwds) maps naturally to JSON. No need for a database.

### 10. Diff-based rendering
**Decision**: Maintain a front buffer and back buffer. Render changes by diffing the two and emitting only the changed cells.
**Rationale**: Essential for performance. Full redraws cause visible flicker and waste bandwidth (important for remote sessions). tmux and zellij both use this technique.

## Risks / Trade-offs

**[VT parsing correctness]** → Terminal escape sequences are complex (thousands of sequences across xterm, VT100, VT220, etc.). The vte crate handles parsing but we need to correctly maintain per-pane screen state (cursor position, attributes, scrollback). Mitigation: Start with basic VT100 support, expand incrementally. Test against common programs (vim, htop, less).

**[Performance under many panes]** → Each pane has a tokio task reading PTY output and updating its screen buffer, even when hidden. Mitigation: Hidden panes still read PTY output (to avoid blocking the child) but skip rendering. Only the visible pane's buffer is composited into the frame.

**[Cassowary complexity]** → Constraint solvers can produce unexpected results with conflicting constraints. Mitigation: Keep constraints simple (minimum pane size, ratio-based splits). Fall back to equal distribution if solver fails.

**[Mode confusion]** → Users may not know which mode they're in. Mitigation: Always display current mode in the status bar/frame. Clear visual distinction between modes (color-coded mode indicator).

**[Large scrollback memory]** → Full scrollback buffers per pane can consume significant memory. Mitigation: Configurable scrollback limit (default 10,000 lines). Hidden panes still accumulate scrollback.
