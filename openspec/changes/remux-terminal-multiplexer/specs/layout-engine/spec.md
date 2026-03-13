## ADDED Requirements

### Requirement: Binary split tree
The system SHALL represent each tab's layout as a binary tree where internal nodes are splits (horizontal or vertical with a ratio) and leaf nodes are pane stacks.

#### Scenario: Initial tab layout
- **WHEN** a new tab is created
- **THEN** it contains a single pane stack as the root node (no splits)

#### Scenario: Vertical split
- **WHEN** the user splits the current pane stack vertically
- **THEN** the current leaf node is replaced by a VSplit node with the original stack as the first child and a new stack as the second child, each occupying 50% of the width

#### Scenario: Horizontal split
- **WHEN** the user splits the current pane stack horizontally
- **THEN** the current leaf node is replaced by an HSplit node with the original stack as the first child and a new stack as the second child, each occupying 50% of the height

### Requirement: Pane stacking
Each pane stack SHALL contain one or more panes. Exactly one pane in each stack SHALL be the active (visible) pane. Hidden panes SHALL continue running but not render.

#### Scenario: Add pane to stack
- **WHEN** the user creates a new pane in the current stack
- **THEN** the new pane is added to the stack and becomes the active pane

#### Scenario: Cycle through stack
- **WHEN** the user navigates to the next/previous pane in the stack
- **THEN** the active pane index advances/retreats (wrapping around) and only the new active pane renders

### Requirement: Constraint-based sizing
The system SHALL use the cassowary constraint solver to compute pane dimensions from split ratios and minimum size constraints. Minimum pane size SHALL be 2 columns by 2 rows.

#### Scenario: Terminal resize
- **WHEN** the terminal window is resized
- **THEN** the constraint solver recomputes all pane dimensions proportionally and each pane's PTY is resized accordingly

#### Scenario: Pane too small
- **WHEN** a resize would make a pane smaller than the minimum size
- **THEN** the constraint solver enforces the minimum and adjusts neighboring panes

### Requirement: Resize operations
The system SHALL allow the user to resize splits by adjusting the ratio in configurable increments.

#### Scenario: Resize split
- **WHEN** the user resizes a split left/right/up/down
- **THEN** the split ratio is adjusted by the configured increment (default 5%) and pane dimensions are recomputed

### Requirement: Pane focus navigation
The system SHALL allow directional navigation between pane stacks (left/right/up/down) based on spatial position.

#### Scenario: Focus left
- **WHEN** the user navigates left from the current pane stack
- **THEN** focus moves to the nearest pane stack to the left, or does nothing if at the leftmost position

### Requirement: Close pane
The system SHALL allow closing a pane, removing it from its stack. If the stack becomes empty, the stack node is removed from the split tree and the tree is simplified.

#### Scenario: Close last pane in stack
- **WHEN** the user closes the last pane in a stack that is part of a split
- **THEN** the stack is removed, the split node is replaced by the sibling node, and focus moves to the sibling

#### Scenario: Close last pane in tab
- **WHEN** the user closes the last pane in the last stack of a tab
- **THEN** the tab is closed
