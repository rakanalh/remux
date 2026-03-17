## ADDED Requirements

### Requirement: Session manager activation
The system SHALL open the session manager overlay when the user presses `Ctrl+s` in Normal mode. The system SHALL request the full session tree from the server and display it once received.

#### Scenario: Open session manager
- **WHEN** user presses `Ctrl+s` in Normal mode
- **THEN** the input mode changes to SessionManager and a `ListSessionTree` request is sent to the server

#### Scenario: Close session manager
- **WHEN** user presses `Esc` or `q` while in the session manager
- **THEN** the session manager closes and input returns to Normal mode

### Requirement: Session tree display
The system SHALL display a tree structure showing folders, sessions, tabs, and panes. Each level SHALL be visually indented. The current session SHALL be highlighted distinctly.

#### Scenario: Tree with folders and sessions
- **WHEN** the session tree is rendered
- **THEN** folders appear at the top level, sessions appear indented under their folder (or at top level if unfiled), tabs appear indented under sessions, and panes appear indented under tabs

#### Scenario: Current session indicator
- **WHEN** the client is attached to a session
- **THEN** that session's entry in the tree SHALL be visually marked as current (e.g., with an asterisk or distinct color)

### Requirement: Tree navigation
The system SHALL support cursor navigation through the tree using `j`/Down to move down and `k`/Up to move up. The selected row SHALL be visually highlighted.

#### Scenario: Move cursor down
- **WHEN** user presses `j` or Down arrow
- **THEN** the cursor moves to the next visible row, wrapping to the top if at the bottom

#### Scenario: Move cursor up
- **WHEN** user presses `k` or Up arrow
- **THEN** the cursor moves to the previous visible row, wrapping to the bottom if at the top

### Requirement: Tree expand and collapse
The system SHALL support collapsing and expanding tree nodes. Folders, sessions, and tabs SHALL be collapsible. Collapsed nodes hide their children from the visible list.

#### Scenario: Collapse a node
- **WHEN** user presses `-` on a folder, session, or tab node
- **THEN** that node's children are hidden and the node shows a collapsed indicator

#### Scenario: Expand a node
- **WHEN** user presses `+` on a collapsed folder, session, or tab node
- **THEN** that node's children become visible and the node shows an expanded indicator

#### Scenario: Collapse on pane node
- **WHEN** user presses `-` on a pane node (leaf)
- **THEN** nothing happens (panes have no children)

### Requirement: Switch to session
The system SHALL switch the client to the selected session when the user presses Enter on a session node. The session manager closes after switching.

#### Scenario: Switch to a different session
- **WHEN** user presses Enter on a session that is not the current session
- **THEN** the client detaches from the current session, attaches to the selected session, and the session manager closes

#### Scenario: Enter on current session
- **WHEN** user presses Enter on the session the client is already attached to
- **THEN** the session manager closes (no switch needed)

### Requirement: Switch to tab
The system SHALL switch to the selected tab's session and activate that tab when the user presses Enter on a tab node.

#### Scenario: Switch to tab in another session
- **WHEN** user presses Enter on a tab in a different session
- **THEN** the client attaches to that session and switches to the selected tab

#### Scenario: Switch to tab in current session
- **WHEN** user presses Enter on a tab in the current session
- **THEN** the active tab switches to the selected tab and the session manager closes

### Requirement: Switch to pane
The system SHALL switch to the selected pane's session, tab, and focus that pane when the user presses Enter on a pane node.

#### Scenario: Switch to pane in another session
- **WHEN** user presses Enter on a pane in a different session
- **THEN** the client attaches to that session, switches to the pane's tab, focuses the pane, and the session manager closes

#### Scenario: Switch to pane in current session
- **WHEN** user presses Enter on a pane in the current session
- **THEN** the system switches to the pane's tab (if needed), focuses the pane, and the session manager closes

### Requirement: Create folder
The system SHALL allow creating a new folder by pressing `c`. A text input prompt SHALL appear for the folder name.

#### Scenario: Create folder successfully
- **WHEN** user presses `c`, types a folder name, and presses Enter
- **THEN** a `FolderNew` command is sent to the server and the tree refreshes to show the new folder

#### Scenario: Cancel folder creation
- **WHEN** user presses `c` then presses Esc
- **THEN** the folder creation is cancelled and the session manager returns to normal navigation

### Requirement: Create session
The system SHALL allow creating a new session by pressing `n`. A text input prompt SHALL appear for the session name, followed by a folder selection.

#### Scenario: Create session in folder
- **WHEN** user presses `n`, types a session name, presses Enter, then selects a folder
- **THEN** a `SessionNew` command is sent with the session name and folder, and the tree refreshes

#### Scenario: Create session at top level
- **WHEN** user presses `n`, types a session name, presses Enter, then selects "(top level)"
- **THEN** a `SessionNew` command is sent with no folder, and the tree refreshes

#### Scenario: Cancel session creation
- **WHEN** user presses `n` then presses Esc at any prompt
- **THEN** the session creation is cancelled

### Requirement: Move session to folder
The system SHALL allow moving a session to a different folder by pressing `m` while a session is selected. A folder selection list SHALL appear.

#### Scenario: Move session to folder
- **WHEN** user selects a session, presses `m`, then selects a target folder
- **THEN** a `FolderMoveSession` command is sent and the tree refreshes showing the session under the new folder

#### Scenario: Move session to top level
- **WHEN** user selects a session, presses `m`, then selects "(top level)"
- **THEN** a `FolderMoveSession` command is sent with no folder and the tree refreshes

#### Scenario: Move pressed on non-session node
- **WHEN** user presses `m` while a folder, tab, or pane is selected
- **THEN** nothing happens (only sessions can be moved)

### Requirement: Delete item with confirmation
The system SHALL allow deleting the selected item by pressing `d`. A confirmation prompt SHALL appear before deletion. Deleting a folder deletes all sessions within it. Deleting a session kills it. Deleting a tab closes it.

#### Scenario: Delete session confirmed
- **WHEN** user selects a session, presses `d`, and confirms with `y`
- **THEN** a `KillSession` command is sent and the tree refreshes without the deleted session

#### Scenario: Delete folder confirmed
- **WHEN** user selects a folder, presses `d`, and confirms with `y`
- **THEN** all sessions in the folder are killed, the folder is deleted, and the tree refreshes

#### Scenario: Delete tab confirmed
- **WHEN** user selects a tab, presses `d`, and confirms with `y`
- **THEN** the tab is closed and the tree refreshes

#### Scenario: Delete cancelled
- **WHEN** user presses `d` then presses any key other than `y`
- **THEN** the deletion is cancelled and the session manager returns to normal navigation

#### Scenario: Delete pane
- **WHEN** user selects a pane and presses `d`
- **THEN** nothing happens (panes are not deletable from the session manager)

### Requirement: Session tree protocol
The system SHALL support a `ListSessionTree` client message and `SessionTree` server response that returns the full hierarchy of folders, sessions, tabs, and panes.

#### Scenario: Request session tree
- **WHEN** the client sends a `ListSessionTree` message
- **THEN** the server responds with a `SessionTree` message containing all folders with their sessions, unfiled sessions, and each session's tabs and panes

#### Scenario: Empty server
- **WHEN** the server has no sessions
- **THEN** the `SessionTree` response contains empty folders and empty unfiled lists
