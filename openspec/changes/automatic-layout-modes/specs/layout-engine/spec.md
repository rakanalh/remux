## MODIFIED Requirements

### Requirement: Binary split tree
The system SHALL represent each tab's layout as a binary tree where internal nodes are splits (horizontal or vertical with a ratio) and leaf nodes are pane stacks. In automatic layout modes (BSP, Master, Monocle), the tree SHALL be rebuilt from the tab's `pane_order` using the active layout algorithm whenever panes are added or removed. In Custom mode, the tree SHALL be manipulated directly as before.

#### Scenario: Initial tab layout
- **WHEN** a new tab is created
- **THEN** it contains a single pane stack as the root node and the layout mode is set to the configured default

#### Scenario: Vertical split
- **WHEN** the user splits the current pane stack vertically
- **THEN** the current leaf node is replaced by a VSplit node with the original stack as the first child and a new stack as the second child, each occupying 50% of the width
- **AND** if the tab was in an automatic mode, the mode transitions to Custom

#### Scenario: Horizontal split
- **WHEN** the user splits the current pane stack horizontally
- **THEN** the current leaf node is replaced by an HSplit node with the original stack as the first child and a new stack as the second child, each occupying 50% of the height
- **AND** if the tab was in an automatic mode, the mode transitions to Custom

#### Scenario: Automatic mode tree rebuild
- **WHEN** a pane is added or removed while in an automatic layout mode
- **THEN** the layout tree is rebuilt from `pane_order` using the active algorithm

### Requirement: Resize operations
The system SHALL allow the user to resize splits by adjusting the ratio in configurable increments. Performing a resize while in an automatic layout mode SHALL transition the tab to Custom mode before applying the resize.

#### Scenario: Resize split
- **WHEN** the user resizes a split left/right/up/down
- **THEN** the split ratio is adjusted by the configured increment (default 5%) and pane dimensions are recomputed

#### Scenario: Resize in automatic mode
- **WHEN** the user resizes while in BSP or Master mode
- **THEN** the tab transitions to Custom mode and the resize is applied to the now-Custom tree
