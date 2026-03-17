## Context

Remux has server-side session/folder/tab/pane management exposed through `RemuxCommand` variants and CLI commands. The client has overlay patterns (WhichKeyPopup, RenameOverlay, CommandPaletteState) that render via `Vec<DrawCommand>` and are dispatched through `InputHandler`. There is no interactive session browser — users must detach or use a separate terminal to switch sessions.

The existing `SessionList` protocol message returns a flat list of `SessionListEntry` (name, folder, tab_count, client_count). This is insufficient for a tree view that shows tabs and panes within each session.

## Goals / Non-Goals

**Goals:**
- Provide a full-screen session manager overlay triggered by `Ctrl+s`
- Display a collapsible tree: folders → sessions → tabs → panes
- Support switching to any session/tab/pane via Enter
- Support management actions: create folder (c), create session (n), move session (m), delete item (d)
- Follow existing overlay patterns (DrawCommand rendering, InputHandler dispatch)

**Non-Goals:**
- Session search/filtering (can be added later)
- Drag-and-drop reordering
- Multi-select operations
- Remote session management (only local server)

## Decisions

### 1. New overlay mode vs. new Mode variant

**Decision**: Add `SessionManager` as a new `Mode` variant rather than an overlay on top of existing modes.

**Rationale**: The session manager is a full takeover UI (like Visual mode) with its own keybinding set, not a quick overlay like RenameOverlay. It needs exclusive input handling. The CommandPalette is also a Mode, which is the established pattern for complex overlays.

**Alternatives considered**: Using an `Option<SessionManagerState>` overlay field (like RenameOverlay) — rejected because the session manager has complex multi-key interactions that conflict with Normal mode pass-through.

### 2. Protocol: new SessionTree message

**Decision**: Add a new `ClientMessage::ListSessionTree` request and `ServerMessage::SessionTree` response that returns the full hierarchy.

**Rationale**: The existing `SessionList` only returns session-level info. The tree view needs tabs and pane names/IDs within each session. A single request/response is simpler than multiple round-trips.

**Structure**:
```
SessionTree {
  folders: Vec<FolderTreeEntry>,    // { name, sessions: Vec<SessionTreeEntry> }
  unfiled: Vec<SessionTreeEntry>,   // sessions not in any folder
}
SessionTreeEntry {
  name: String,
  tabs: Vec<TabTreeEntry>,         // { id, name, panes: Vec<PaneTreeEntry> }
  client_count: usize,
  is_current: bool,                // whether this client is attached to it
}
TabTreeEntry { id: TabId, name: String, panes: Vec<PaneTreeEntry> }
PaneTreeEntry { id: PaneId, name: String, is_focused: bool }
```

### 3. Tree node representation (client-side)

**Decision**: Use a flat `Vec<TreeRow>` computed from the server response, where each row has an indent level, node type, expanded/collapsed state, and display text.

**Rationale**: Rendering and navigation are simpler on a flat list. The tree structure is only needed for expand/collapse logic. This is the standard approach for tree views in TUIs.

### 4. Switching semantics

**Decision**: Enter on a node triggers a switch appropriate to the node type:
- **Folder**: No-op (folders aren't attachable)
- **Session**: `ClientMessage::Attach { session_name }` (server handles detach+reattach)
- **Tab**: New `RemuxCommand::SessionSwitchTab { session, tab_index }` — attaches to session and switches to that tab
- **Pane**: New `RemuxCommand::SessionSwitchPane { session, tab_index, pane_id }` — attaches, switches tab, focuses pane

**Rationale**: Users should be able to jump directly to any level. Switching to a tab/pane in a different session requires attach first, so the server must handle both atomically.

### 5. Delete confirmation

**Decision**: When `d` is pressed, show an inline confirmation prompt at the bottom of the session manager popup ("Delete <name>? y/n"). Only proceed on `y`, cancel on any other key.

**Rationale**: Avoids accidental deletion of sessions with running processes. Keeps the interaction within the session manager rather than spawning a separate dialog.

### 6. Keybinding: Ctrl+s

**Decision**: Bind `Ctrl+s` as a `ShortcutBinding` (intercepted in Normal mode) that opens the session manager.

**Rationale**: `Ctrl+s` is conventionally "save" but in a terminal multiplexer context it's available and intuitive for "sessions". It follows the pattern of existing shortcut bindings (Alt+key, Ctrl+key). Users can rebind via `[keybindings.command]` config.

**Note**: `Ctrl+s` is traditionally `XOFF` (flow control). Remux already controls the terminal, so this doesn't conflict.

### 7. Move session workflow

**Decision**: When `m` is pressed, show a sub-prompt listing folders + "(top level)" option. User navigates with up/down and confirms with Enter.

**Rationale**: Simpler than typing a folder name. The list of folders is short enough to display inline.

### 8. Create session workflow

**Decision**: When `n` is pressed, show a text input for the session name, then a folder selection (same as move). Default to top-level (no folder).

**Rationale**: Matches the user's expectation from the requirements. The folder selection reuses the same UI component as move.

## Risks / Trade-offs

- **[Performance]** Large numbers of sessions/folders could make the tree slow to render → Mitigation: paginate or virtualize the list if needed; unlikely for typical use (< 50 sessions)
- **[Protocol versioning]** New message types require client/server compatibility → Mitigation: server returns `Error` for unknown messages; client can fall back to `ListSessions`
- **[Ctrl+s conflict]** Some users may have terminal flow control expectations → Mitigation: configurable shortcut; document in help
