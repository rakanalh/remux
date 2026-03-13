## ADDED Requirements

### Requirement: Session hierarchy
The system SHALL support the hierarchy: Folder (optional) → Session → Tab → Layout (split tree). Sessions MAY belong to a folder or exist at the top level.

#### Scenario: Create session without folder
- **WHEN** the user creates a session without specifying a folder
- **THEN** the session is created at the top level

#### Scenario: Create session in folder
- **WHEN** the user creates a session with a folder name
- **THEN** the session is created inside that folder, creating the folder if it doesn't exist

### Requirement: Folder management
The system SHALL allow creating, renaming, deleting, and listing folders. Deleting a folder SHALL require it to be empty (no sessions).

#### Scenario: Delete non-empty folder
- **WHEN** the user attempts to delete a folder that contains sessions
- **THEN** the operation fails with an error message

#### Scenario: List folders
- **WHEN** the user lists folders
- **THEN** all folders are displayed with their session count

### Requirement: Session management
The system SHALL allow creating, renaming, deleting, and listing sessions. Each session MUST have a unique name (globally, not per-folder).

#### Scenario: Create session
- **WHEN** the user creates a new session
- **THEN** a session is created with one tab containing one pane stack with one shell pane

#### Scenario: Delete session
- **WHEN** the user deletes a session
- **THEN** all panes in the session are terminated and the session is removed

#### Scenario: Duplicate session name
- **WHEN** the user creates a session with a name that already exists
- **THEN** the operation fails with an error message

### Requirement: Tab management
The system SHALL allow creating, closing, renaming, reordering, and navigating tabs within a session.

#### Scenario: Create tab
- **WHEN** the user creates a new tab
- **THEN** a tab is added to the current session with a single pane stack containing a shell

#### Scenario: Navigate to tab by number
- **WHEN** the user presses a tab number (1-9) in Normal mode
- **THEN** the corresponding tab becomes active

#### Scenario: Close tab
- **WHEN** the user closes a tab
- **THEN** all panes in the tab are terminated and the tab is removed. If it was the last tab, the session is closed.

### Requirement: Move session between folders
The system SHALL allow moving a session from one folder to another, or from top-level to a folder and vice versa.

#### Scenario: Move session to folder
- **WHEN** the user moves a session into a folder
- **THEN** the session is removed from its current location and placed in the target folder
