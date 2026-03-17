## ADDED Requirements

### Requirement: Search mode activation
The user SHALL be able to enter Search mode by pressing `Ctrl+a s`. Upon activation, a search prompt overlay SHALL appear at the bottom of the active pane, replacing the status bar area or appearing as an inline prompt (e.g., `/query`).

#### Scenario: Enter search mode
- **WHEN** the user presses `Ctrl+a s`
- **THEN** the mode changes to SEARCH, a search prompt appears at the bottom of the screen, and the cursor moves to the prompt for text input

#### Scenario: Cancel search mode with no query
- **WHEN** the user presses `Escape` while the search prompt is empty
- **THEN** the mode returns to NORMAL and the search prompt disappears

### Requirement: Search query input
The user SHALL be able to type a search query in the search prompt. The search SHALL be performed against the active pane's scrollback buffer and visible grid content. Pressing `Enter` SHALL confirm the query and begin match navigation.

#### Scenario: Type and confirm search query
- **WHEN** the user types "error" in the search prompt and presses `Enter`
- **THEN** the system searches the active pane's output for "error", highlights all matches, jumps to the nearest match, and displays the match count in the status bar

#### Scenario: Search with no matches
- **WHEN** the user confirms a query that has no matches in the pane output
- **THEN** the status bar shows `(0/0)` and the view does not change

#### Scenario: Backspace in search prompt
- **WHEN** the user presses `Backspace` while typing a query
- **THEN** the last character is removed from the query

### Requirement: Match navigation
After confirming a search query, the user SHALL be able to cycle through matches using `n` (next) and `N` (previous), wrapping around when reaching the end or beginning of matches.

#### Scenario: Navigate to next match
- **WHEN** the user presses `n` after a search with matches
- **THEN** the view scrolls to and highlights the next match, and the status bar counter updates (e.g., `(2/5)` → `(3/5)`)

#### Scenario: Navigate to previous match
- **WHEN** the user presses `N` after a search with matches
- **THEN** the view scrolls to and highlights the previous match, and the status bar counter updates

#### Scenario: Wrap around forward
- **WHEN** the user is on the last match and presses `n`
- **THEN** the view wraps to the first match

#### Scenario: Wrap around backward
- **WHEN** the user is on the first match and presses `N`
- **THEN** the view wraps to the last match

### Requirement: Search match count in status bar
The status bar SHALL display the current match index and total match count as `(x/y)` next to the layout name when a search is active.

#### Scenario: Match count display
- **WHEN** the user has an active search with 5 matches and is viewing match 3
- **THEN** the status bar shows `(3/5)` next to the layout mode name

#### Scenario: Match count clears on exit
- **WHEN** the user exits search mode by pressing `Escape`
- **THEN** the `(x/y)` indicator disappears from the status bar

### Requirement: Exit search mode
The user SHALL be able to exit search mode by pressing `Escape`. Exiting SHALL clear the search highlights and match count from the status bar, and return to NORMAL mode.

#### Scenario: Exit search during navigation
- **WHEN** the user presses `Escape` while navigating matches
- **THEN** the mode returns to NORMAL, search highlights are cleared, and the status bar match count disappears

### Requirement: Buffer edit in editor under search group
The `BufferEditInEditor` command SHALL be accessible under `Ctrl+a s e`, moved from the former `Ctrl+a b e` binding.

#### Scenario: Edit buffer from search group
- **WHEN** the user presses `Ctrl+a s e`
- **THEN** the active pane's scrollback buffer opens in the user's `$EDITOR`

### Requirement: Session keybindings relocation
The Session keybindings previously under `Ctrl+a s` SHALL be moved to a different key to make room for search mode. Session commands SHALL remain fully accessible under the new binding.

#### Scenario: Session group at new key
- **WHEN** the user presses `Ctrl+a x`
- **THEN** the session submenu appears with all previous session commands (new, detach, rename, list)
