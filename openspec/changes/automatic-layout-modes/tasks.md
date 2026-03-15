## 1. Data Model

- [x] 1.1 Add `LayoutMode` enum (`Bsp`, `Master`, `Monocle`, `Custom`) to `layout.rs` with Serialize/Deserialize
- [x] 1.2 Add `layout_mode: LayoutMode` and `pane_order: Vec<PaneId>` fields to `Tab` struct in `session.rs`
- [x] 1.3 ~~Add `master_pane: usize` field to `Tab` struct~~ — stored in `MasterLayout.master_idx` instead
- [x] 1.4 Update `ServerState::create_session` and `create_tab` to accept/use default layout mode
- [x] 1.5 Maintain `pane_order` on pane create (append) and pane close (remove) in daemon handlers

## 2. Layout Algorithms

- [x] 2.1 Implement `build_bsp_tree(panes: &[PaneId]) -> LayoutNode` — alternating V/H splits on newest pane
- [x] 2.2 Implement `build_master_tree(panes: &[PaneId], master: usize, master_ratio: f32) -> LayoutNode` — center master, left/right columns
- [x] 2.3 Implement `build_monocle_tree(layout: &LayoutNode, active_pane: PaneId) -> LayoutNode` — flatten all panes from all stacks into single stack
- [x] 2.4 Write unit tests for BSP tree with 1, 2, 3, 5 panes
- [x] 2.5 Write unit tests for Master tree with 1, 2, 3, 5 panes
- [x] 2.6 Write unit tests for Monocle tree including stack flattening
- [x] 2.7 Write unit tests verifying stacks are treated as atomic in BSP and Master

## 3. Mode Switching and Ejection

- [x] 3.1 Add `LayoutNext` and `SetMaster` variants to `RemuxCommand` in `protocol.rs`
- [x] 3.2 Implement `LayoutNext` handler in daemon — cycle BSP → Master → Monocle → BSP, rebuild tree from `pane_order`
- [x] 3.3 Implement `SetMaster` handler — update `master_pane` index and rebuild if in Master mode
- [x] 3.4 Add ejection logic to `PaneSplitVertical`/`PaneSplitHorizontal` handlers — transition to Custom before applying split
- [x] 3.5 Add ejection logic to `ResizeLeft`/`ResizeRight`/`ResizeUp`/`ResizeDown` handlers — transition to Custom before applying resize
- [x] 3.6 Write tests for mode cycling order including Custom → BSP transition

## 4. PaneNew Behavior

- [x] 4.1 Modify `PaneNew` handler to branch on layout mode: automatic modes append to `pane_order` and rebuild tree; Custom mode uses existing split behavior
- [x] 4.2 Write tests for `PaneNew` in each layout mode

## 5. Configuration

- [x] 5.1 Add `default_layout: LayoutMode` field to `AppearanceConfig` in `config/mod.rs` (default: `Bsp`)
- [x] 5.2 Wire config default into `create_session`/`create_tab` calls
- [x] 5.3 Add `LayoutNext` to default keybindings under prefix group (bound to `Space`)
- [x] 5.4 Add `SetMaster` to default keybindings (choose appropriate key in prefix group)
- [x] 5.5 Update `config.sample.toml` with new layout settings and keybindings

## 6. Integration and Polish

- [x] 6.1 Ensure `pane_order` and `layout_mode` are included in session save/restore serialization
- [ ] 6.2 Verify compositor works unchanged with all layout mode outputs (manual testing)
- [ ] 6.3 Test mode cycling with gaps enabled (ZellijStyle) and disabled (TmuxStyle)
- [ ] 6.4 Test Monocle → BSP transition preserves all panes (stacks don't re-group)
