## Context

Remux uses a binary tree (`LayoutNode`) for pane arrangement. Users manually split panes to build the tree. There is no concept of an automatic layout algorithm — every split direction, position, and ratio is determined by user action.

The existing `compute_layout` function walks the tree and produces `Vec<(PaneId, Rect)>` which the compositor consumes. This interface is layout-algorithm-agnostic and can remain unchanged.

Stacked panes (`LayoutNode::Stack` with multiple panes) are already a first-class concept — they hold multiple panes where one is active/visible.

## Goals / Non-Goals

**Goals:**
- Support four layout modes per tab: BSP, Master, Monocle, Custom
- Cycle between modes with a single keybinding (`<Prefix><Space>`)
- Preserve the existing manual layout as "Custom" mode — no regression
- Treat stacked panes as atomic units in BSP and Master (flatten only in Monocle)
- Smooth transition: any manual split/resize ejects from automatic mode to Custom
- Configurable default layout mode

**Non-Goals:**
- Floating/overlapping pane layout
- Per-pane resize constraints in automatic modes (all panes get equal share, except master)
- Saving/restoring the previous automatic layout after ejecting to Custom
- User-defined custom layout algorithms or scripting

## Decisions

### Decision 1: Layout mode as a separate concern from the tree

The `LayoutMode` enum lives on the `Tab` struct alongside the existing `layout: LayoutNode` field. Automatic modes do not store a separate tree — they **rebuild the tree from a pane list** each time `compute_layout` is called (or when panes are added/removed).

**Why**: Automatic layouts are deterministic given a list of panes and an algorithm. Storing a separate tree would duplicate state. Rebuilding is cheap (it's just constructing a small tree of N nodes where N is the pane count).

**Alternative considered**: Store both a "canonical pane list" and a "computed tree" as cached state. Rejected because the tree construction is trivial and caching adds complexity without meaningful performance gain.

### Decision 2: Pane ordering via a `pane_order: Vec<PaneId>` on Tab

Automatic layouts need a stable ordering of panes (BSP needs insertion order, Master needs to know which is the master). The `Tab` struct gains a `pane_order: Vec<PaneId>` that tracks all pane IDs in insertion order. This is the source of truth for automatic layout algorithms.

When in Custom mode, `pane_order` is still maintained (appended on create, removed on close) but not used for layout — the `layout` tree is authoritative.

**Why**: The binary tree doesn't preserve insertion order or expose a flat pane list easily. A parallel vec is simple and gives algorithms what they need.

### Decision 3: Trait-based layout algorithms

Each layout mode implements a `LayoutAlgorithm` trait:

```rust
pub trait LayoutAlgorithm: Send + Sync {
    fn name(&self) -> &str;
    fn build_tree(&self, panes: &[PaneId], active_pane: PaneId) -> LayoutNode;
}
```

Concrete implementations:
- `BspLayout` — alternates V/H splits on the newest pane's slot
- `MasterLayout { master_idx: usize, master_ratio: f32 }` — center master with left/right columns
- `MonocleLayout` — single stack with all panes, active pane visible
- `CustomLayout { tree: LayoutNode }` — wraps the manually-built tree, `build_tree` returns it as-is

Each produces a `LayoutNode` that `compute_layout` processes as usual. The compositor doesn't know or care which mode generated the tree.

**Why**: Trait-based design is extensible, separates each algorithm cleanly, and keeps state (e.g., master_idx, master_ratio) co-located with the algorithm that uses it. Each implementation is self-contained and independently testable.

### Decision 4: Mode transition to Custom on manual actions

When the tab is in BSP, Master, or Monocle mode, the following commands transition to Custom:
- `PaneSplitVertical` / `PaneSplitHorizontal` — the split is performed on the current tree, which becomes the Custom tree
- `ResizeLeft` / `ResizeRight` / `ResizeUp` / `ResizeDown` — the resize is applied and mode becomes Custom

The transition is one-way per user action. The user can cycle back to an automatic mode with `LayoutNext`, which rebuilds the tree from `pane_order`.

**Why**: Clean mental model — automatic modes are "hands-off", any manual intervention means you want control. Cycling back to automatic is always available.

### Decision 5: Monocle flattens stacks

In Monocle mode, `build_monocle_tree` extracts all pane IDs from all stacks (not just active ones) and creates a single flat stack. Navigation uses `PaneStackNext`/`PaneStackPrev`.

When switching away from Monocle back to BSP or Master, the original stack groupings are lost — all panes become individual entries in `pane_order`. This is acceptable because the user explicitly chose to change layouts.

**Why**: Monocle's purpose is "one pane at a time". Keeping stacks as sub-groups would mean some monocle slots show one pane while others show a stack-within-monocle, which is confusing.

### Decision 6: Master layout geometry

- **2 panes**: Vertical split, master gets `master_ratio` (default 0.6) of the width
- **3+ panes**: Three-column layout. Center column is master at `master_ratio` width. Left and right columns share the remaining width equally. Non-master panes alternate left/right and are stacked vertically within each column.
- **Master designation**: The master pane is tracked by index into `pane_order` (default: 0, the first pane). `SetMaster` command changes which pane is master.

### Decision 7: Layout mode cycling order

`LayoutNext` cycles: BSP → Master → Monocle → BSP (skipping Custom). If currently in Custom, `LayoutNext` goes to BSP. There is no `LayoutPrev` command initially.

## Risks / Trade-offs

- **[Stack groupings lost on Monocle round-trip]** → Acceptable trade-off. Users can re-stack panes after switching back. Preserving groupings would require shadow state that adds complexity for an edge case.
- **[PaneNew behavior change is breaking]** → Mitigated by making BSP the default, which produces similar visual results to the old behavior (alternating splits). Users who want the old exact behavior can set `default_layout = "custom"` in config.
- **[Tree rebuild on every pane add/remove in automatic modes]** → Performance is not a concern. Pane counts are small (typically <20) and tree construction is O(n).
- **[No undo for Custom ejection]** → User can cycle back to automatic with `LayoutNext`. The exact manual adjustments are lost, but the pane set is preserved.
