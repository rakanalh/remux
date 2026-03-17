## Why

Sessions and folders exist as server-side concepts with CLI commands for CRUD, but there is no interactive UI for browsing, switching, or organizing them from within an attached client. Users must detach or use a separate terminal to manage sessions. A visual session manager popup — similar to tmux's session picker — would make session navigation fast and discoverable.

## What Changes

- Add a new `SessionManager` overlay mode accessible via `Ctrl+s` (shortcut binding)
- Display a collapsible tree: folders → sessions → tabs → panes
- Support navigation (up/down, expand/collapse), switching (Enter), and management actions (create folder, create session, move session, delete item)
- Add new protocol messages for querying the full session tree (folders, sessions, tabs, panes) and for management operations (move session, delete folder/session/tab)
- Add confirmation prompt for destructive delete operations

## Capabilities

### New Capabilities
- `session-manager`: The interactive session manager popup — tree rendering, navigation, keybindings, switching, and management actions (create, move, delete)

### Modified Capabilities
_None — existing session/folder protocol messages are sufficient for CRUD; the new capability adds the client-side UI and any missing protocol messages._

## Impact

- **Client input** (`src/client/input.rs`): New `Mode::SessionManager` or overlay state, key dispatch for session manager actions
- **New client module** (`src/client/session_manager.rs`): Tree state, filtering, rendering via `DrawCommand`
- **Renderer** (`src/client/renderer.rs`): New `render_session_manager_overlay()` method
- **Protocol** (`src/protocol.rs`): New `SessionTree` request/response messages carrying folder→session→tab→pane hierarchy; possibly `MoveSession`, `DeleteTab`, `DeleteFolder` commands if not already covered
- **Keybindings** (`src/config/keybindings.rs`): New default shortcut `Ctrl+s` → open session manager
- **Server session** (`src/server/session.rs`): Handler for tree query; possibly new delete/move operations
