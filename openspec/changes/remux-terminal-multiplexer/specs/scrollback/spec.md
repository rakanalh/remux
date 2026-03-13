## ADDED Requirements

### Requirement: Per-pane scrollback buffer
The system SHALL maintain a scrollback buffer for each pane containing lines that have scrolled off the top of the visible area. The buffer size SHALL be configurable (default 10,000 lines).

#### Scenario: Lines scroll off screen
- **WHEN** output causes lines to scroll off the top of the pane
- **THEN** those lines are stored in the scrollback buffer up to the configured limit

#### Scenario: Buffer limit reached
- **WHEN** the scrollback buffer exceeds the configured limit
- **THEN** the oldest lines are discarded

### Requirement: Scrollback navigation in Visual mode
In Visual mode, the user SHALL be able to scroll through the scrollback buffer using vim-style motions (j/k for lines, Ctrl-d/Ctrl-u for half-page, G/gg for top/bottom).

#### Scenario: Scroll up in Visual mode
- **WHEN** the user presses 'k' or Ctrl-u in Visual mode
- **THEN** the view scrolls up through the scrollback buffer

#### Scenario: Jump to top
- **WHEN** the user presses 'gg' in Visual mode
- **THEN** the view jumps to the beginning of the scrollback buffer

### Requirement: Text selection in Visual mode
The user SHALL be able to select text in the scrollback buffer using vim-style visual selection (v for character-wise, V for line-wise).

#### Scenario: Select and yank text
- **WHEN** the user selects text with visual motions and presses 'y'
- **THEN** the selected text is copied to the system clipboard

### Requirement: Edit scrollback in $EDITOR
The user SHALL be able to open the full scrollback buffer (plus visible content) in their $EDITOR as a read-only temporary file.

#### Scenario: Open scrollback in editor
- **WHEN** the user triggers the "buffer:edit_in_editor" command
- **THEN** the full scrollback content is written to a temporary file, $EDITOR is opened with that file in read-only mode, and the temporary file is cleaned up after the editor exits

### Requirement: Search in scrollback
The user SHALL be able to search the scrollback buffer with '/' in Visual mode. Search SHALL highlight matches and allow navigating between them with n/N.

#### Scenario: Search scrollback
- **WHEN** the user presses '/' and types a search term in Visual mode
- **THEN** matching text is highlighted and the view jumps to the first match

#### Scenario: Navigate matches
- **WHEN** the user presses 'n' after a search
- **THEN** the view jumps to the next match
