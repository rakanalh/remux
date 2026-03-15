## Why

Remux currently only supports manual layout construction — users build a binary tree by splitting panes one at a time. This requires deliberate effort to arrange panes and produces no consistent spatial pattern. Automatic layout modes (like dwm/bspwm) let the system arrange panes according to an algorithm, so users can focus on creating panes and let the layout engine handle placement.

## What Changes

- Add four layout modes per tab: **BSP** (binary space partitioning), **Master** (center master with side panes), **Monocle** (fullscreen cycling), and **Custom** (current manual tree behavior).
- **BSP**: Each new pane (via `PaneNew`) splits the most recently added pane's area, alternating vertical/horizontal direction automatically.
- **Master**: One designated master pane occupies the center with a larger share. Non-master panes distribute evenly between left and right columns. With 2 panes, falls back to a vertical split where master gets more space (~60%). A configurable keybinding sets which pane is the master.
- **Monocle**: All panes (including those inside stacks, which are flattened) occupy the full area one at a time. Navigation uses existing `PaneStackNext`/`PaneStackPrev` commands.
- **Custom**: The current manual binary tree. Entered automatically when the user performs `PaneSplitVertical`, `PaneSplitHorizontal`, or any `Resize` command while in an automatic mode.
- `PaneNew` adds a pane to the pool and the current automatic layout re-arranges. `PaneSplitVertical`/`PaneSplitHorizontal` eject to Custom mode.
- A new command (`LayoutNext` or similar) cycles through layout modes, bound to `<Prefix><Space>` by default.
- Stacked panes are treated as a single unit in BSP and Master layouts — the stack occupies one slot and is not broken apart. Monocle is the exception: stacks are flattened.
- **BREAKING**: `PaneNew` behavior changes from "split at focused pane" to "add to layout pool" when in an automatic layout mode.
- Default layout mode for new tabs is BSP, configurable via `appearance.default_layout` in config.

## Capabilities

### New Capabilities
- `automatic-layouts`: Core layout mode system — BSP, Master, Monocle algorithms, mode switching, and the interaction between automatic and Custom modes.

### Modified Capabilities
- `layout-engine`: The layout engine needs to support layout modes as a wrapper around the existing tree. `compute_layout` must handle the new algorithms. `PaneNew` behavior branches based on active mode.
- `configuration`: New config fields for `default_layout` and master pane keybinding.

## Impact

- **`src/server/layout.rs`**: Major changes — new `LayoutMode` enum, layout algorithms for BSP/Master/Monocle, mode tracking per tab, mode transition logic (automatic → Custom on manual split/resize).
- **`src/server/session.rs`**: `Tab` struct gains a `layout_mode` field.
- **`src/server/daemon.rs`**: `PaneNew` handler branches on layout mode. New `LayoutNext`/`SetMaster` command handlers. Split/resize handlers trigger mode transition to Custom.
- **`src/protocol.rs`**: New `RemuxCommand` variants (`LayoutNext`, `SetMaster`).
- **`src/config/mod.rs`**: New `default_layout` config field.
- **`src/server/compositor.rs`**: No changes expected — it already consumes `Vec<(PaneId, Rect)>` from `compute_layout`.
