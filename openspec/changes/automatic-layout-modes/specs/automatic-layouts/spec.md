## ADDED Requirements

### Requirement: Layout mode enum
The system SHALL support four layout modes per tab: `Bsp`, `Master`, `Monocle`, and `Custom`. The mode SHALL be stored on the `Tab` struct and persisted with session state.

#### Scenario: New tab default mode
- **WHEN** a new tab is created
- **THEN** its layout mode is set to the configured default (BSP unless overridden)

#### Scenario: Layout mode serialization
- **WHEN** a session is saved and restored
- **THEN** each tab's layout mode is preserved

### Requirement: Pane ordering
Each tab SHALL maintain a `pane_order: Vec<PaneId>` that tracks all pane IDs in insertion order. This list is the source of truth for automatic layout algorithms.

#### Scenario: Pane added
- **WHEN** a new pane is created (via `PaneNew` or any split command)
- **THEN** the pane ID is appended to `pane_order`

#### Scenario: Pane removed
- **WHEN** a pane is closed
- **THEN** the pane ID is removed from `pane_order`

#### Scenario: Pane order persistence
- **WHEN** a session is saved and restored
- **THEN** the pane order is preserved

### Requirement: BSP layout algorithm
In BSP mode, the system SHALL arrange panes by recursively splitting the most recently added pane's area, alternating between vertical and horizontal splits. The first pane occupies the full area. The second pane splits vertically (left/right, 50/50). The third pane splits the second pane's area horizontally. The pattern continues alternating direction on the newest pane.

#### Scenario: BSP with 1 pane
- **WHEN** a tab is in BSP mode with 1 pane
- **THEN** the pane occupies the full area

#### Scenario: BSP with 2 panes
- **WHEN** a tab is in BSP mode with 2 panes
- **THEN** the layout is a vertical split (left/right, 50/50)

#### Scenario: BSP with 3 panes
- **WHEN** a tab is in BSP mode with 3 panes
- **THEN** pane 1 is on the left (50%), pane 2 is top-right (25%), pane 3 is bottom-right (25%)

#### Scenario: BSP with 5 panes
- **WHEN** a tab is in BSP mode with 5 panes
- **THEN** the layout alternates V/H/V/H splits, each splitting the newest pane's slot

#### Scenario: BSP treats stacks as atomic
- **WHEN** a tab is in BSP mode and a stack contains multiple panes
- **THEN** the stack occupies one slot in the BSP tree and is not broken apart

### Requirement: Master layout algorithm
In Master mode, the system SHALL arrange panes with one designated master pane in the center and non-master panes distributed evenly between left and right columns.

#### Scenario: Master with 1 pane
- **WHEN** a tab is in Master mode with 1 pane
- **THEN** the pane occupies the full area

#### Scenario: Master with 2 panes
- **WHEN** a tab is in Master mode with 2 panes
- **THEN** the layout is a vertical split where the master pane gets `master_ratio` (default 60%) of the width and the other pane gets the remainder

#### Scenario: Master with 3 panes
- **WHEN** a tab is in Master mode with 3 panes (master + 2 others)
- **THEN** the master occupies the center column, one non-master pane is on the left, one is on the right, and left/right columns share the remaining width equally

#### Scenario: Master with 5 panes
- **WHEN** a tab is in Master mode with 5 panes (master + 4 others)
- **THEN** the master is in the center, 2 non-master panes are stacked vertically on the left, and 2 are stacked vertically on the right

#### Scenario: Master treats stacks as atomic
- **WHEN** a tab is in Master mode and a stack contains multiple panes
- **THEN** the stack occupies one slot in the master layout and is not broken apart

#### Scenario: Set master pane
- **WHEN** the user executes `SetMaster` on a pane
- **THEN** that pane becomes the master and the layout is rebuilt with it in the center

### Requirement: Monocle layout algorithm
In Monocle mode, the system SHALL display one pane at a time occupying the full area. All panes, including those inside stacks, SHALL be flattened into a single list. Navigation SHALL use `PaneStackNext`/`PaneStackPrev` to cycle through all panes.

#### Scenario: Monocle with 3 panes
- **WHEN** a tab is in Monocle mode with 3 panes
- **THEN** only one pane is visible at a time, occupying the full area

#### Scenario: Monocle flattens stacks
- **WHEN** a tab enters Monocle mode and contains a stack with 3 panes
- **THEN** all 3 panes from the stack become individual entries in the monocle cycle

#### Scenario: Monocle navigation
- **WHEN** the user executes `PaneStackNext` in Monocle mode
- **THEN** the next pane in the flattened list becomes visible

### Requirement: Layout mode cycling
The system SHALL provide a `LayoutNext` command that cycles through layout modes in the order: BSP → Master → Monocle → BSP. If the current mode is Custom, `LayoutNext` SHALL transition to BSP.

#### Scenario: Cycle from BSP
- **WHEN** the user executes `LayoutNext` while in BSP mode
- **THEN** the tab switches to Master mode and the layout is rebuilt

#### Scenario: Cycle from Custom
- **WHEN** the user executes `LayoutNext` while in Custom mode
- **THEN** the tab switches to BSP mode and the layout is rebuilt from `pane_order`

#### Scenario: Cycle completes full loop
- **WHEN** the user executes `LayoutNext` three times starting from BSP
- **THEN** the mode cycles BSP → Master → Monocle → BSP

### Requirement: Ejection to Custom mode
The system SHALL transition from any automatic layout mode to Custom mode when the user performs a manual layout action: `PaneSplitVertical`, `PaneSplitHorizontal`, `ResizeLeft`, `ResizeRight`, `ResizeUp`, or `ResizeDown`.

#### Scenario: Split ejects to Custom
- **WHEN** the user executes `PaneSplitVertical` while in BSP mode
- **THEN** the current BSP-generated tree becomes the Custom tree, the split is applied to it, and the mode becomes Custom

#### Scenario: Resize ejects to Custom
- **WHEN** the user executes `ResizeLeft` while in Master mode
- **THEN** the current Master-generated tree becomes the Custom tree, the resize is applied, and the mode becomes Custom

#### Scenario: PaneNew does not eject
- **WHEN** the user executes `PaneNew` while in BSP mode
- **THEN** the new pane is added to `pane_order` and the BSP layout is rebuilt (mode stays BSP)

### Requirement: PaneNew in automatic modes
When in an automatic layout mode (BSP, Master, Monocle), `PaneNew` SHALL add a new pane to `pane_order` and rebuild the layout using the current algorithm. The new pane SHALL receive focus.

#### Scenario: PaneNew in BSP
- **WHEN** the user executes `PaneNew` in BSP mode with 2 existing panes
- **THEN** a third pane is created, appended to `pane_order`, and the BSP tree is rebuilt with 3 panes

#### Scenario: PaneNew in Monocle
- **WHEN** the user executes `PaneNew` in Monocle mode
- **THEN** a new pane is created and becomes the visible pane in the monocle cycle
