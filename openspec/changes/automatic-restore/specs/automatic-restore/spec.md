## ADDED Requirements

### Requirement: Server restores persisted state on startup
When `automatic_restore` is enabled in config and a persisted state file exists, the server SHALL load the state and reconstruct all sessions, tabs, layouts, and panes on startup before accepting client connections.

#### Scenario: Successful restore with saved state
- **WHEN** the server starts with `automatic_restore = true` and a valid `state.json` exists
- **THEN** all sessions, tabs, and layout trees from the saved state are restored, a fresh shell is spawned for each pane in its saved working directory, and the server accepts connections with the restored state

#### Scenario: No saved state file
- **WHEN** the server starts with `automatic_restore = true` but no `state.json` exists
- **THEN** the server starts with empty state as normal

#### Scenario: Restore disabled by config
- **WHEN** the server starts with `automatic_restore = false`
- **THEN** the server starts with empty state regardless of whether a state file exists

#### Scenario: Corrupted or invalid state file
- **WHEN** the server starts with `automatic_restore = true` but `state.json` contains invalid data
- **THEN** the server logs a warning and starts with empty state

### Requirement: Restored panes spawn shells in saved working directories
Each pane in the restored state SHALL have a fresh shell spawned in the working directory that was captured at save time.

#### Scenario: Working directory exists
- **WHEN** a pane is restored and its saved working directory exists on disk
- **THEN** the shell is spawned with that directory as its CWD

#### Scenario: Working directory no longer exists
- **WHEN** a pane is restored but its saved working directory has been deleted or is inaccessible
- **THEN** the shell is spawned in `$HOME` instead

### Requirement: Restored pane IDs are preserved
The server SHALL preserve pane ID assignments from the saved state so that layout trees referencing specific pane IDs remain valid.

#### Scenario: Pane ID continuity
- **WHEN** state is restored with panes having IDs 1, 2, 5
- **THEN** the restored panes use those same IDs, and the next-pane-ID counter is set to at least 6

### Requirement: Restored panes resize on client attach
Restored panes SHALL be spawned at a default size and resized to actual terminal dimensions when the first client attaches.

#### Scenario: Client attaches after restore
- **WHEN** a client attaches to a restored session and sends a Resize message
- **THEN** all panes in the session are resized to fit the client's terminal dimensions

### Requirement: Config toggle for automatic restore
The config file SHALL support an `automatic_restore` boolean option under `[general]` that controls whether state is restored on startup.

#### Scenario: Config specifies automatic_restore = true
- **WHEN** the config contains `automatic_restore = true`
- **THEN** the server attempts to restore persisted state on startup

#### Scenario: Config specifies automatic_restore = false
- **WHEN** the config contains `automatic_restore = false`
- **THEN** the server skips restore and starts fresh

#### Scenario: Config omits automatic_restore
- **WHEN** the config does not specify `automatic_restore`
- **THEN** the server defaults to `automatic_restore = true`
