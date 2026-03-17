## Context

Remux has a persistence module (`persistence.rs`) that saves `ServerState` + pane working directories to `~/.local/share/remux/state.json` on graceful shutdown. The `load_state()` function exists but is never called. An auto-save timer exists in the server event loop but only logs — it never actually saves. The infrastructure is mostly in place; the gap is wiring it together.

All core data structures (`ServerState`, `Session`, `Tab`, `LayoutNode`, `LayoutMode`) already derive `Serialize`/`Deserialize`. Pane working directories are captured via `/proc/<pid>/cwd` at save time.

## Goals / Non-Goals

**Goals:**
- Restore all sessions, tabs, layouts, and pane working directories on server startup
- Save state on every structural mutation so crashes don't lose work
- Provide a config toggle (`automatic_restore`) to disable restore
- Clean up dead auto-save timer code

**Non-Goals:**
- Restoring shell history, environment variables, or running processes
- Persisting scrollback buffer content
- Re-attaching to existing processes (like tmux resurrect plugins do)
- Incremental/differential saves — full state write on each mutation is fine given the small data size

## Decisions

### 1. Restore at server startup, not client attach

Restore happens in `RemuxServer::run()` before accepting connections. This way the state is ready when any client connects.

**Alternative considered:** Restore on first client attach. Rejected because multiple clients could race, and it couples restore to the attach flow unnecessarily.

### 2. Save on structural mutations via a helper function

Add a `save_if_enabled()` async helper that checks config and calls `save_state()`. Call it after every structural command handler (session/tab/pane create/close/rename, layout changes). This is straightforward — each handler already has access to state and panes.

**Alternative considered:** A debounced save channel where mutations post a "save needed" signal and a background task coalesces saves. Rejected as over-engineering — structural mutations are infrequent (human-speed) and `save_state()` writes a small JSON file atomically.

### 3. Spawn PTYs at default size, resize on client attach

Restored panes get 80x24 PTYs. When the first client attaches and sends `Resize`, all panes are resized to actual dimensions. This matches the existing flow — clients always send `Resize` on connect.

### 4. Fall back to $HOME for missing working directories

If a saved CWD no longer exists (deleted project, unmounted drive), spawn the shell in `$HOME` instead of failing the restore.

### 5. Remove auto_save_interval_secs entirely

The timer-based approach is replaced by event-driven saves. Remove the config field, the timer in the server loop, and the dead tick handler.

## Risks / Trade-offs

**[Stale state file]** → If the server crashes without saving, the state file may be outdated. Mitigated by saving on every structural change — the window for data loss is small (only mid-command crashes).

**[Orphaned pane IDs]** → Restored state references pane IDs that no longer have live PTYs. Mitigated by spawning new PTYs for every pane ID during restore, preserving the ID mapping.

**[Large state files]** → With many sessions/tabs, the JSON could grow. Mitigated by the fact that state only contains structure (not content) — even 100 sessions with 10 tabs each would be a few KB.

**[Save I/O on every mutation]** → Each structural change triggers a disk write. Mitigated by atomic write (temp file + rename) and small file size — sub-millisecond for typical state sizes.
