## Why

Remux currently saves session state on shutdown but never restores it on startup. Users lose their entire workspace layout (sessions, tabs, pane arrangements, working directories) every time the server restarts or the machine reboots. This is the most basic expectation of a terminal multiplexer — that your workspace survives restarts.

## What Changes

- Load persisted state on server startup and re-create all sessions, tabs, panes, and layouts with fresh shells in saved working directories
- Save state on every structural change (session/tab/pane create/close/rename, layout mutations) instead of the current non-functional timer
- Add `automatic_restore` config option (default: `true`) to enable/disable restore on startup
- Remove the unused `auto_save_interval_secs` config option and its dead timer code

## Capabilities

### New Capabilities
- `automatic-restore`: Automatic loading of persisted state on server startup, spawning fresh PTYs in saved working directories, with config toggle
- `event-driven-save`: Save state to disk on every structural mutation instead of periodic timer

### Modified Capabilities

## Impact

- `src/server/daemon.rs` — Server startup to call `load_state()` and re-spawn panes; remove auto-save timer; add save calls on structural mutations
- `src/server/persistence.rs` — May need a restore helper to walk loaded state and spawn PTYs
- `src/config/mod.rs` — Add `automatic_restore` field, remove `auto_save_interval_secs`
- `config.sample.toml` — Update config documentation
