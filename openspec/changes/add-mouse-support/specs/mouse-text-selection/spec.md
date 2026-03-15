## ADDED Requirements

### Requirement: Click-and-drag to select text
The system SHALL enter a mouse selection mode when the user clicks and drags within a pane. The selection SHALL span from the initial click position to the current drag position, covering all characters between the two points.

#### Scenario: Drag across text in a single pane
- **WHEN** the user clicks at position (x1, y1) and drags to position (x2, y2) within the same pane
- **THEN** all text between the start and end positions is selected

#### Scenario: Drag starts in one pane and enters another
- **WHEN** the user starts a drag in one pane and the cursor moves into another pane's area
- **THEN** the selection remains bounded to the pane where the drag started

### Requirement: Visual highlighting of selected text
The system SHALL visually highlight selected text by inverting the foreground and background colors of selected cells. The highlighting SHALL update in real-time as the user drags.

#### Scenario: Selection highlight during drag
- **WHEN** the user is actively dragging to select text
- **THEN** the selected region is rendered with inverted foreground and background colors

#### Scenario: Selection cleared
- **WHEN** the user releases the mouse button after a selection
- **THEN** the visual highlighting is removed after the text is copied

### Requirement: Automatic clipboard copy on selection
The system SHALL automatically copy the selected text to the system clipboard when the user releases the mouse button after a drag selection. The clipboard write SHALL use OSC 52 escape sequences.

#### Scenario: Copy on mouse release
- **WHEN** the user releases the mouse button after selecting text via drag
- **THEN** the selected text is copied to the system clipboard via OSC 52

#### Scenario: Empty selection
- **WHEN** the user clicks and releases without dragging (no text selected)
- **THEN** no clipboard operation occurs; the click is treated as a focus event

### Requirement: Selection coordinates mapped to scrollback buffer
The system SHALL map screen coordinates of a selection to positions in the pane's scrollback buffer, accounting for the pane's position within the layout and any scroll offset.

#### Scenario: Select text in a pane at non-zero offset
- **WHEN** the pane is positioned at layout offset (px, py) and the user selects from screen position (sx, sy)
- **THEN** the selection maps to pane-local coordinates (sx - px, sy - py) in the pane's buffer
