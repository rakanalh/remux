## ADDED Requirements

### Requirement: State is saved on every structural mutation
The server SHALL persist state to disk after every command that changes the session/tab/pane structure or layout.

#### Scenario: Session created
- **WHEN** a new session is created
- **THEN** the current state is saved to disk

#### Scenario: Session deleted
- **WHEN** a session is killed
- **THEN** the current state is saved to disk

#### Scenario: Session renamed
- **WHEN** a session is renamed
- **THEN** the current state is saved to disk

#### Scenario: Tab created
- **WHEN** a new tab is created
- **THEN** the current state is saved to disk

#### Scenario: Tab closed
- **WHEN** a tab is closed
- **THEN** the current state is saved to disk

#### Scenario: Tab renamed
- **WHEN** a tab is renamed
- **THEN** the current state is saved to disk

#### Scenario: Pane created via split
- **WHEN** a pane is created via vertical or horizontal split
- **THEN** the current state is saved to disk

#### Scenario: Pane closed
- **WHEN** a pane is closed (manually or automatically on process exit)
- **THEN** the current state is saved to disk

#### Scenario: Layout mode changed
- **WHEN** the layout mode is switched (e.g., Bsp to Master)
- **THEN** the current state is saved to disk

#### Scenario: Folder created or modified
- **WHEN** a folder is created, renamed, deleted, or a session is moved between folders
- **THEN** the current state is saved to disk

### Requirement: Auto-save timer is removed
The unused `auto_save_interval_secs` config option and its associated timer code SHALL be removed.

#### Scenario: Config with auto_save_interval_secs
- **WHEN** the config file contains `auto_save_interval_secs`
- **THEN** the field is ignored (does not cause an error)

### Requirement: State is saved atomically
State saves SHALL use atomic write (write to temp file, then rename) to prevent corruption if the server crashes mid-write.

#### Scenario: Crash during save
- **WHEN** the server crashes while writing state
- **THEN** the previous valid state file remains intact
