## 1. Config Changes

- [ ] 1.1 Add `automatic_restore: bool` field to config struct with default `true`
- [ ] 1.2 Parse `automatic_restore` from `[general]` section in TOML config
- [ ] 1.3 Remove `auto_save_interval_secs` field from config struct (keep TOML parsing lenient so old configs don't break)
- [ ] 1.4 Update `config.sample.toml` — add `automatic_restore = true`, remove `auto_save_interval_secs`

## 2. Event-Driven Save

- [ ] 2.1 Create `save_if_enabled()` async helper in daemon.rs that captures pane CWDs and calls `save_state()` when automatic_restore is enabled
- [ ] 2.2 Add `save_if_enabled()` calls after session create/delete/rename handlers
- [ ] 2.3 Add `save_if_enabled()` calls after tab create/close/rename/move handlers
- [ ] 2.4 Add `save_if_enabled()` calls after pane split/close handlers (including auto-close on process exit)
- [ ] 2.5 Add `save_if_enabled()` calls after layout mode changes and folder mutations
- [ ] 2.6 Remove the auto-save timer interval and its tick handler from the server event loop

## 3. Automatic Restore on Startup

- [ ] 3.1 Create `restore_state()` function in persistence.rs that takes loaded `PersistedState` and spawns PTYs for each pane in its saved CWD (falling back to $HOME if CWD missing)
- [ ] 3.2 Wire `restore_state()` into `RemuxServer::run()` — call before accepting connections when `automatic_restore` is enabled
- [ ] 3.3 Ensure restored pane IDs are preserved and `next_pane_id`/`next_tab_id` counters are set correctly
- [ ] 3.4 Start PTY forwarding for all restored panes
- [ ] 3.5 Handle corrupted/invalid state file gracefully — log warning, start fresh

## 4. Cleanup and Testing

- [ ] 4.1 Verify `cargo build` and `cargo test` pass
- [ ] 4.2 Verify state.json is written on structural changes (manual test)
- [ ] 4.3 Verify server restores sessions/tabs/layouts from state.json on restart (manual test)
