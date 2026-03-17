## 1. Protocol Layer

- [ ] 1.1 Add `SessionTreeEntry`, `FolderTreeEntry`, `TabTreeEntry`, `PaneTreeEntry` structs to `protocol.rs`
- [ ] 1.2 Add `ClientMessage::ListSessionTree` variant
- [ ] 1.3 Add `ServerMessage::SessionTree { folders, unfiled }` variant
- [ ] 1.4 Add `RemuxCommand::SessionSwitchTab { session, tab_index }` and `RemuxCommand::SessionSwitchPane { session, tab_index, pane_id }` variants
- [ ] 1.5 Add round-trip tests for new protocol messages

## 2. Server Handlers

- [ ] 2.1 Implement `build_session_tree()` in `server/session.rs` that assembles the full hierarchy from `ServerState`
- [ ] 2.2 Handle `ListSessionTree` in `daemon.rs` — call `build_session_tree()` and respond with `SessionTree`
- [ ] 2.3 Handle `SessionSwitchTab` command — attach to session and switch to specified tab
- [ ] 2.4 Handle `SessionSwitchPane` command — attach to session, switch tab, and focus pane
- [ ] 2.5 Handle `TabClose` for a specific tab by index (for delete-tab from session manager)

## 3. Client Session Manager State

- [ ] 3.1 Create `src/client/session_manager.rs` module with `SessionManagerState` struct (tree rows, selected index, expanded set, sub-mode)
- [ ] 3.2 Implement `TreeRow` struct with fields: indent level, node type (Folder/Session/Tab/Pane), display name, expanded state, identifiers
- [ ] 3.3 Implement `build_rows()` to flatten `SessionTree` response into `Vec<TreeRow>` respecting expand/collapse state
- [ ] 3.4 Implement navigation: `select_next()`, `select_prev()` with wrapping
- [ ] 3.5 Implement `toggle_expand()` and `toggle_collapse()` for `+`/`-` keys
- [ ] 3.6 Implement `render()` returning `Vec<DrawCommand>` — bordered popup with tree rows, selection highlight, current-session indicator

## 4. Client Session Manager Actions

- [ ] 4.1 Implement Enter handler — determine node type and return appropriate switch action (Attach, SwitchTab, SwitchPane)
- [ ] 4.2 Implement `d` handler — show inline confirmation prompt, on `y` dispatch KillSession/FolderDelete/TabClose
- [ ] 4.3 Implement `c` handler — enter text input sub-mode for folder name, dispatch FolderNew on Enter
- [ ] 4.4 Implement `n` handler — enter text input for session name, then folder selection, dispatch SessionNew
- [ ] 4.5 Implement `m` handler — show folder selection list, dispatch FolderMoveSession on selection
- [ ] 4.6 Implement tree refresh — re-request `ListSessionTree` after any mutation

## 5. Input Handler Integration

- [ ] 5.1 Add `SessionManager` variant to `Mode` enum in `input.rs`
- [ ] 5.2 Add `session_manager: Option<SessionManagerState>` field to `InputHandler`
- [ ] 5.3 Add `handle_session_manager_key()` dispatch in `InputHandler::handle_key()`
- [ ] 5.4 Add `InputAction` variants for session manager results (SwitchSession, SwitchTab, SwitchPane, RefreshTree, etc.)
- [ ] 5.5 Register `Ctrl+s` as a `ShortcutBinding` that opens the session manager (configurable)

## 6. Renderer Integration

- [ ] 6.1 Add `render_session_manager_overlay()` method to `Renderer`
- [ ] 6.2 Call session manager render in the client event loop when mode is SessionManager
- [ ] 6.3 Wire up session manager actions in the client event loop (`main.rs`) — send protocol messages on switch/create/delete actions

## 7. Client Event Loop

- [ ] 7.1 Handle `ServerMessage::SessionTree` in client — populate `SessionManagerState` with received data
- [ ] 7.2 Send `ListSessionTree` when session manager is opened
- [ ] 7.3 Handle session manager action results — execute attach/detach/command sequences
- [ ] 7.4 Handle tree refresh after mutations — re-send `ListSessionTree` and update state

## 8. Testing

- [ ] 8.1 Unit tests for `SessionManagerState`: navigation, expand/collapse, row building
- [ ] 8.2 Unit tests for `build_session_tree()`: empty state, folders with sessions, unfiled sessions
- [ ] 8.3 Unit tests for session manager key handling: Enter on each node type, delete confirmation flow
- [ ] 8.4 Integration test: open session manager, navigate, switch session
