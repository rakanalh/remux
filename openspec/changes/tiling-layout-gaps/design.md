## Context

Remux currently has a `FrameStyle` enum (`Framed`/`Minimal`) and a `GapMode` enum (`WithGaps`/`NoGaps`). These overlap in intent — `FrameStyle` controls border rendering while `GapMode` controls spacing, but in practice users want one of two holistic modes: Zellij-style (borders + gaps + pane names) or tmux-style (minimal dividers, edge-to-edge). Panes also lack names, making it hard to identify what is running where, especially in stacked layouts.

## Goals / Non-Goals

**Goals:**
- Unify rendering modes under renamed `GapMode` variants: `ZellijStyle` and `TmuxStyle`.
- Remove the redundant `FrameStyle` enum entirely.
- Implement ZellijStyle rendering with full box borders, rounded corners, pane names, stacked pane tabs, and active pane highlighting.
- Implement TmuxStyle rendering with minimal dividers, edge-to-edge content, and 1-row tab bars for multi-pane stacks.
- Add pane naming: auto-detect from running process, support custom names via `PaneRename` command.
- Add a rename flow with keybinding (`p` → `r`), RENAME input mode, and status bar prompt.
- Maintain existing gap toggle and gap size configuration.

**Non-Goals:**
- Per-pane or per-split gap overrides — gaps are uniform.
- Fractional or sub-cell gap sizes.
- Animated transitions when toggling modes.
- Mouse interaction with borders or tab bars.

## Decisions

### 1. GapMode Rename: WithGaps → ZellijStyle, NoGaps → TmuxStyle

**Decision**: Rename `GapMode::WithGaps` to `GapMode::ZellijStyle` and `GapMode::NoGaps` to `GapMode::TmuxStyle`. Serde values are `zellij_style` and `tmux_style`. Default is `ZellijStyle`.

**Rationale**: The old names described only the gap behavior, but each mode now encompasses a complete rendering philosophy (borders, names, tabs, colors). Naming them after the multiplexers they emulate is more descriptive and intuitive.

### 2. Remove FrameStyle entirely

**Decision**: Delete the `FrameStyle` enum (`Framed`/`Minimal`) and its `frame_style` field from `AppearanceConfig`. `GapMode` alone controls all rendering behavior including border style, gap spacing, and pane name display.

**Rationale**: With ZellijStyle handling full borders and TmuxStyle handling minimal dividers, `FrameStyle` is redundant. Keeping both would create confusing combinations (e.g., `Framed` + `TmuxStyle`). A single enum simplifies config, code paths, and user mental model.

### 3. ZellijStyle Rendering

**Decision**: In ZellijStyle mode:
- Every pane gets a full box border using rounded corner characters: `╭` (top-left), `╮` (top-right), `╰` (bottom-left), `╯` (bottom-right), `─` (horizontal), `│` (vertical).
- The border occupies the outermost cells of the pane rect. Content is blitted to the inner area at `(x+1, y+1, width-2, height-2)`.
- The pane name is rendered in the top border: `╭ zsh ──────────╮`.
- For stacked panes, all pane names appear as tabs in the top border with the active pane highlighted: `╭ zsh | nvim | cargo ─╮`.
- The active pane's border uses a distinct color (green or white) versus inactive panes (dark grey).

**Rationale**: This matches the Zellij aesthetic that users expect from a "gapped" mode. Rounded corners feel modern. Embedding names in borders is space-efficient. Stacked tabs give visibility into the stack without consuming extra rows.

### 4. TmuxStyle Rendering

**Decision**: In TmuxStyle mode:
- Content is rendered edge-to-edge with no border characters consuming pane space.
- Minimal `│` and `─` dividers are drawn at split boundaries (current Minimal logic).
- For stacks with more than one pane: a 1-row tab bar is rendered at the top showing pane names, with the active pane highlighted.
- For single-pane stacks: no tab bar.
- `gap_size` is always 0 in this mode.

**Rationale**: This preserves the traditional tmux experience — maximum content space, minimal chrome. The tab bar for stacks is the minimum UI needed to show stack contents without borders.

### 5. Pane Names

**Decision**: Add `names: Vec<String>` to `LayoutNode::Stack`, parallel to `panes: Vec<PaneId>`. Each pane has a name that defaults to the running process name (detected via `/proc/<pid>/comm` or equivalent). A custom name is stored as `Option<String>` per pane — `None` triggers auto-detection. `PaneRename(String)` command sets a custom name on the focused pane.

**Rationale**: Parallel vecs keep the data model simple and avoid wrapping PaneId in a struct. Process name auto-detection gives useful defaults without user effort. `Option<String>` for custom names means clearing a custom name reverts to auto-detection.

### 6. Rename Flow

**Decision**: The rename flow is:
1. User presses `p` then `r` (pane group → rename action).
2. Client enters RENAME mode, showing "RENAME" in the mode indicator and `Rename pane: _` in the status bar.
3. User types the new name. Enter confirms, Escape cancels.
4. On confirm, client sends `Command(PaneRename("name"))` to the server.
5. Server updates the name in the layout tree and triggers re-render.

**Rationale**: Using a dedicated input mode is consistent with how other rename operations could work. The `p` → `r` binding is discoverable via the which-key popup. Status bar feedback makes the mode visible.

### 7. Keybindings

**Decision**:
- `g` → `toggle_gaps` (existing, unchanged).
- `p` → group "pane" containing `r` → `pane_rename`.

**Rationale**: `p` for pane is natural. `r` for rename is mnemonic. The `p` group can later host other pane-specific actions.

### 8. Session-level gap state

**Decision**: Each session stores `gap_mode: GapMode` initialized from config. `ToggleGaps` flips between `ZellijStyle` and `TmuxStyle`. The compositor reads the session's mode, not the config directly.

**Rationale**: Per-session toggling without mutating config. Consistent with existing design.

### 9. Gap size and layout computation

**Decision**: Gap size is applied at layout computation time (unchanged from current design). In `TmuxStyle`, gap_size is forced to 0 regardless of config. In `ZellijStyle`, the configured `gap_size` is used.

**Rationale**: Applying gaps at layout time keeps PTY dimensions correct. TmuxStyle forcing 0 ensures edge-to-edge rendering.

## Risks / Trade-offs

- **Small terminals**: ZellijStyle borders consume 2 rows and 2 columns per pane. With many splits, panes may hit minimum size. Mitigation: existing `MIN_PANE_SIZE` enforcement applies.
- **Process name detection**: `/proc/<pid>/comm` is Linux-specific. Mitigation: fall back to a generic default name (e.g., "pane") on unsupported platforms.
- **Stacked tab overflow**: If many panes are stacked, names may not fit in the top border. Mitigation: truncate names or show `+N` for overflow.
- **Breaking config change**: Renaming `with_gaps`/`no_gaps` to `zellij_style`/`tmux_style` and removing `frame_style` breaks existing configs. Mitigation: since Remux is pre-1.0, breaking changes are acceptable. Document in release notes.
- **Serialization**: Old session files may have `WithGaps`/`NoGaps` values and `frame_style`. Mitigation: use `#[serde(alias)]` for old variant names during deserialization, ignore unknown fields.
