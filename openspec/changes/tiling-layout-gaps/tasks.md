## 1. Data Model & Config

- [x] 1.1 Rename `GapMode` variants: `WithGaps` → `ZellijStyle`, `NoGaps` → `TmuxStyle` in `src/config/mod.rs`. Update serde rename to `zellij_style` / `tmux_style`. Set default to `ZellijStyle`.
- [x] 1.2 Remove `FrameStyle` enum (`Framed`/`Minimal`) from `src/config/mod.rs`.
- [x] 1.3 Remove `frame_style` field from `AppearanceConfig` in `src/config/mod.rs`.
- [x] 1.4 Add `names: Vec<String>` field parallel to `panes: Vec<PaneId>` in `LayoutNode::Stack` in `src/server/layout.rs`.
- [x] 1.5 Add `PaneRename(String)` variant to `RemuxCommand` in `src/protocol.rs`.
- [x] 1.6 Update all references to old `GapMode` variant names (`WithGaps`/`NoGaps`) throughout the codebase.
- [x] 1.7 Remove all references to `FrameStyle` and `frame_style` throughout the codebase.

## 2. Compositor: ZellijStyle

- [x] 2.1 Add `draw_zellij_panes` function in `src/server/compositor.rs` that draws full box borders with rounded corners (`╭`, `╮`, `╰`, `╯`, `─`, `│`) around every pane.
- [x] 2.2 Implement content blitting to inner area `(x+1, y+1, width-2, height-2)` within pane borders.
- [x] 2.3 Render pane name in top border: `╭ zsh ──────────╮`.
- [x] 2.4 For stacked panes, render all names as tabs in top border with active pane highlighted: `╭ zsh | nvim | cargo ─╮`.
- [x] 2.5 Apply distinct border color for the active pane (green or white) vs inactive panes (dark grey).

## 3. Compositor: TmuxStyle

- [x] 3.1 Add `draw_tmux_panes` function in `src/server/compositor.rs` with edge-to-edge content and minimal `│`/`─` dividers at split boundaries.
- [x] 3.2 For stacks with more than one pane, render a 1-row tab bar at top showing pane names with active pane highlighted.
- [x] 3.3 For single-pane stacks, render no tab bar.
- [x] 3.4 Force `gap_size=0` in TmuxStyle mode regardless of config.

## 4. Compositor: Shared

- [x] 4.1 Remove `draw_framed_panes`, `draw_minimal_panes`, `draw_minimal_dividers`, and `draw_pane_border` functions from `src/server/compositor.rs`.
- [x] 4.2 Remove `FrameStyle` import from `src/server/compositor.rs`.
- [x] 4.3 Update `composite()` signature to no longer take `frame_style` param; dispatch on `GapMode` instead (`ZellijStyle` → `draw_zellij_panes`, `TmuxStyle` → `draw_tmux_panes`).

## 5. Pane Naming

- [x] 5.1 Implement process name auto-detection from running process (`/proc/<pid>/comm` or equivalent) in `src/server/session.rs` or `src/server/layout.rs`.
- [x] 5.2 Store custom pane name as `Option<String>` per pane — `None` means auto-detect from process.
- [x] 5.3 Add helper methods for name management: get effective name (custom or auto-detected), set custom name, clear custom name.
- [x] 5.4 Ensure pane names are kept in sync when panes are added to or removed from stacks.

## 6. Keybindings & Rename Flow

- [x] 6.1 Add `p` group ("pane") to default keybinding tree in `src/config/keybindings.rs` containing `r` → `pane_rename`.
- [x] 6.2 Add `"pane_rename"` case to `parse_action` returning `RemuxCommand::PaneRename`.
- [x] 6.3 Implement RENAME input mode on the client: status bar shows `Rename pane: _`, user types name, Enter confirms, Escape cancels.
- [x] 6.4 On confirm, client sends `Command(PaneRename("name"))` to the server.

## 7. Server Integration

- [x] 7.1 Handle `RemuxCommand::PaneRename(name)` in daemon command dispatch in `src/server/daemon.rs`: update the focused pane's name in the layout tree.
- [x] 7.2 Pass pane names to compositor during rendering.
- [x] 7.3 Remove all `frame_style` references from `src/server/daemon.rs`.
- [x] 7.4 Update session state to use renamed `GapMode` variants (`ZellijStyle`/`TmuxStyle`) in `src/server/session.rs`.

## 8. Tests & Verification

- [x] 8.1 Add unit tests for ZellijStyle rendering: box borders, pane names in borders, stacked tabs, active pane highlighting.
- [x] 8.2 Add unit tests for TmuxStyle rendering: minimal dividers, tab bar for multi-pane stacks, no tab bar for single-pane stacks.
- [x] 8.3 Add unit tests for pane naming: auto-detection, custom name, name sync on stack changes.
- [x] 8.4 Add unit tests for pane rename keybinding and action parsing.
- [x] 8.5 Add unit tests for config deserialization with new `GapMode` variants and without `frame_style`.
- [x] 8.6 Run `cargo test` and confirm no regressions.
