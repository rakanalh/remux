## ADDED Requirements

### Requirement: Auto-save state
The system SHALL automatically save the current state (folders, sessions, tabs, layouts, cwds) to ~/.local/share/remux/state.json at a configurable interval (default 30 seconds).

#### Scenario: Auto-save triggers
- **WHEN** the configured interval elapses
- **THEN** the current server state is serialized to state.json atomically (write to temp file, then rename)

#### Scenario: State changes saved
- **WHEN** the user creates a new tab and auto-save triggers
- **THEN** the new tab appears in state.json with its layout and pane cwds

### Requirement: Session resurrection
The system SHALL restore sessions from state.json when the server starts and the file exists. Restoration SHALL recreate the folder/session/tab hierarchy, reconstruct layouts, and spawn new shell processes in the saved cwds.

#### Scenario: Resurrect after restart
- **WHEN** the server starts and state.json exists
- **THEN** all saved sessions are recreated with their folder structure, tab names, split layouts, and panes spawned in the saved working directories

#### Scenario: No state file
- **WHEN** the server starts and state.json does not exist
- **THEN** the server starts with no sessions

### Requirement: Configurable auto-save
The auto-save interval SHALL be configurable via TOML config. Setting the interval to 0 SHALL disable auto-save. Manual save SHALL be available as a command.

#### Scenario: Disable auto-save
- **WHEN** the user sets auto_save_interval_secs = 0 in config
- **THEN** automatic state saving is disabled

#### Scenario: Manual save
- **WHEN** the user triggers the "session:save" command
- **THEN** the state is immediately saved to state.json
