## Why

Remux has partial search infrastructure in Visual mode (`/`, `n`, `N`) and a `BufferSearch` command under `Ctrl+a b`, but there's no dedicated search mode with a proper prompt UI, match highlighting, or match count display. Users need a vim-like search experience accessible directly from normal mode via `Ctrl+a s`, with match count visible in the status bar.

## What Changes

- Add a new **Search mode** activated by `Ctrl+a s` that opens a search prompt overlay in the active pane
- Typing a query searches the pane's scrollback + visible grid for matches
- `n` / `N` cycle forward/backward through matches (vim-style)
- Match count displayed as `(x/y)` next to the layout name in the bottom status bar
- **BREAKING**: `Ctrl+a s` is reassigned from the Session group to Search mode. Session keybindings move elsewhere (e.g., under a different key).
- **BREAKING**: `Ctrl+a b` (Buffer group) is removed. `BufferEditInEditor` and `BufferSearch` move under `Ctrl+a s` as search-related actions.

## Capabilities

### New Capabilities
- `search-mode`: Dedicated search mode with prompt overlay, match cycling, match highlighting, and status bar integration

### Modified Capabilities
- `remux-terminal-multiplexer`: Keybinding tree changes — buffer group removed, session group relocated, search mode added under `s`. Search requirements move from Visual-mode-only to a first-class mode.

## Impact

- **Keybindings**: `Ctrl+a s` changes meaning (session → search). `Ctrl+a b` removed. Session bindings need a new home.
- **Input handling**: New `Search` mode in `input.rs` with its own key handler, search prompt overlay, and match navigation state.
- **Status bar**: `StatusInfo` struct in `compositor.rs` extended with search match count fields. `draw_status_bar()` updated to render `(x/y)` beside layout.
- **Protocol**: May need new commands/messages for search state between client and server.
- **Screen**: `screen.rs` scrollback search logic needs to be accessible for the search mode to query matches.
