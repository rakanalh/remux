## Why

Remux currently has separate `FrameStyle` (`Framed`/`Minimal`) and nascent `GapMode` (`WithGaps`/`NoGaps`) enums that overlap in responsibility. The `FrameStyle` distinction is insufficient — what users actually want is a choice between two holistic rendering modes: a Zellij-style mode with full box borders, pane names, and configurable gaps, and a tmux-style mode with minimal dividers and edge-to-edge content. Additionally, panes currently have no concept of names — users cannot see which process is running in a pane or assign custom names. Stacked panes also lack visual indicators showing which panes exist in the stack and which is active.

This change unifies rendering modes under a renamed `GapMode` (`ZellijStyle`/`TmuxStyle`), removes the redundant `FrameStyle` enum, adds pane naming with auto-detection from running processes, introduces a rename flow with a dedicated keybinding and input mode, and adds stacked pane tab indicators in both rendering modes.

## What Changes

- Rename `GapMode` variants: `WithGaps` becomes `ZellijStyle`, `NoGaps` becomes `TmuxStyle`. Serde values: `zellij_style`, `tmux_style`. Default: `ZellijStyle`.
- Remove `FrameStyle` enum and `frame_style` field from `AppearanceConfig` entirely — `GapMode` now controls both border style and gap behavior.
- ZellijStyle rendering: full box borders with rounded corners around every pane, pane name in the top border, stacked panes show all names as tabs in the top border with the active pane highlighted, active pane gets a distinct border color.
- TmuxStyle rendering: edge-to-edge content with minimal `│`/`─` dividers, 1-row tab bar at top of stacks with more than one pane, `gap_size=0` always.
- Add `names: Vec<String>` parallel to `panes: Vec<PaneId>` in `LayoutNode::Stack` for pane name storage.
- Default pane name auto-detected from the running process (`/proc/<pid>/comm` or similar). Custom name stored as `Option<String>` per pane — `None` means auto-detect.
- Add `PaneRename(String)` command to update the focused pane's name.
- Add pane rename keybinding: `p` then `r` triggers rename mode on the client. Client shows "RENAME" mode with status bar prompt, user types name, Enter confirms, Escape cancels. Client sends `Command(PaneRename("name"))` to server.
- Server updates name in layout tree and triggers re-render.

## Capabilities

### New Capabilities
- `tiling-gaps`: ZellijStyle and TmuxStyle rendering modes, pane naming with auto-detection and custom names, `PaneRename` command, stacked pane tab indicators in both modes, toggle command to switch between modes at runtime.

### Modified Capabilities
- `layout-engine`: `LayoutNode::Stack` gains a `names` field parallel to `panes` for pane name storage. Helpers for name management (get, set, auto-detect).
- `configuration`: Remove `FrameStyle` enum and `frame_style` field. Rename `GapMode` variants to `ZellijStyle`/`TmuxStyle`. Default `gap_mode` is `ZellijStyle`.
- `keybinding-tree`: Add `p` → `r` keybinding for `pane_rename`. Existing `g` → `toggle_gaps` remains.

## Impact

- **Compositor** (`src/server/compositor.rs`): Remove `draw_framed_panes`, `draw_minimal_panes`, `draw_minimal_dividers`, `draw_pane_border`. Add `draw_zellij_panes` (full box borders with rounded corners, pane names, stacked tabs, active highlight) and `draw_tmux_panes` (minimal dividers, 1-row tab bar for stacks). Remove `FrameStyle` import. `composite` no longer takes `frame_style` param.
- **Layout engine** (`src/server/layout.rs`): `LayoutNode::Stack` gets `names` field. Add helpers for name management.
- **Config** (`src/config/mod.rs`): Remove `FrameStyle` enum, rename `GapMode` variants (`ZellijStyle`/`TmuxStyle`), remove `frame_style` from `AppearanceConfig`.
- **Keybindings** (`src/config/keybindings.rs`): Add `p` → `r` rename binding, add `pane_rename` action parsing.
- **Protocol** (`src/protocol.rs`): Add `PaneRename(String)` to `RemuxCommand`.
- **Server daemon** (`src/server/daemon.rs`): Handle `PaneRename`, pass process names to compositor, remove `frame_style` references.
- **Session** (`src/server/session.rs`): Pane name management helpers.
