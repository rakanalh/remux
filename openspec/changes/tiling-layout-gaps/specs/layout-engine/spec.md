## MODIFIED Requirements

### Requirement: Constraint-based sizing
The system SHALL use ratio-based division to compute pane dimensions from split ratios and minimum size constraints. Minimum pane size SHALL be 2 columns by 2 rows. When gap mode is `ZellijStyle`, the system SHALL subtract the configured `gap_size` cells from the available space at each split boundary before dividing the remaining space by ratio. When gap mode is `TmuxStyle`, gap size SHALL be 0.

#### Scenario: Terminal resize
- **WHEN** the terminal window is resized
- **THEN** the constraint solver recomputes all pane dimensions proportionally (respecting gap spacing if active) and each pane's PTY is resized accordingly

#### Scenario: Pane too small
- **WHEN** a resize would make a pane smaller than the minimum size
- **THEN** the system enforces the minimum and adjusts neighboring panes

#### Scenario: Split with ZellijStyle gaps active
- **WHEN** a vertical split is computed with `gap_size = 2` in `ZellijStyle` mode and total width is 80
- **THEN** 2 cells are reserved for the gap, and the remaining 78 cells are divided by the split ratio between the two children

#### Scenario: Split in TmuxStyle mode
- **WHEN** a vertical split is computed in `TmuxStyle` mode and total width is 80
- **THEN** all 80 cells are divided by the split ratio between the two children with no gap spacing

#### Scenario: Nested splits with gaps
- **WHEN** a layout has nested splits and gap mode is `ZellijStyle`
- **THEN** each split level subtracts `gap_size` independently, producing uniform gaps between all leaf panes

### Requirement: Pane names in Stack nodes
The `LayoutNode::Stack` variant SHALL maintain a `names: Vec<String>` field parallel to the `panes: Vec<PaneId>` field. The names vector SHALL always have the same length as the panes vector.

#### Scenario: Adding a pane to a stack
- **WHEN** a new pane is added to a stack
- **THEN** a corresponding name entry is added to the names vector at the same index

#### Scenario: Removing a pane from a stack
- **WHEN** a pane is removed from a stack
- **THEN** the corresponding name entry is removed from the names vector

#### Scenario: Stack name-pane consistency
- **WHEN** the layout tree is queried for a stack's contents
- **THEN** `names.len()` equals `panes.len()` and each name at index `i` corresponds to the pane at index `i`
