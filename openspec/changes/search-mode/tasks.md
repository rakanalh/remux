## 1. Keybinding restructuring

- [ ] 1.1 Move Session group from `s` to `x` in `build_default_tree()` in `src/config/keybindings.rs`
- [ ] 1.2 Remove Buffer group (`b`) from `build_default_tree()`
- [ ] 1.3 Add Search group under `s` with: `s` → `EnterSearchMode` (leaf), `e` → `BufferEditInEditor` (leaf)
- [ ] 1.4 Add `EnterSearchMode` variant to `RemuxCommand` in `src/protocol.rs`

## 2. Search mode and state

- [ ] 2.1 Add `Search` variant to `Mode` enum in `src/client/input.rs`
- [ ] 2.2 Create `SearchState` struct with fields: `query_buffer: String`, `confirmed_query: Option<String>`, `matches: Vec<(usize, usize)>`, `current_match: usize`, `phase: SearchPhase` (enum: Prompt, Navigation)
- [ ] 2.3 Add `search_state: Option<SearchState>` field to `InputHandler`
- [ ] 2.4 Implement `handle_search_key()` for prompt phase: printable chars append to query, Backspace deletes, Enter confirms, Escape cancels to Normal
- [ ] 2.5 Implement `handle_search_key()` for navigation phase: `n` next match, `N` prev match (with wrapping), Escape exits to Normal

## 3. Scrollback search

- [ ] 3.1 Add a client→server message to request pane scrollback content for search (or reuse existing mechanism if available)
- [ ] 3.2 Add a server→client response with scrollback text content
- [ ] 3.3 Implement substring matching in client: given scrollback text and query, return `Vec<(usize, usize)>` match positions (line, column)
- [ ] 3.4 On search confirm, send scrollback request, receive content, compute matches, jump to nearest match

## 4. Status bar integration

- [ ] 4.1 Add `search_info: Option<(usize, usize)>` field to `StatusInfo` in `src/server/compositor.rs` (current_match_index, total_matches)
- [ ] 4.2 Update `draw_status_bar()` to render `(x/y)` next to layout mode when `search_info` is `Some`
- [ ] 4.3 Send search match info from client to server via `ModeChanged` or a new message so `StatusInfo` can be populated
- [ ] 4.4 Clear `search_info` when exiting search mode

## 5. Search prompt rendering

- [ ] 5.1 Add search prompt overlay rendering (similar to `RenameOverlay`): show `/query█` during prompt phase, `/confirmed_query` during navigation
- [ ] 5.2 Render the prompt at the bottom of the active pane area

## 6. Mode indicator

- [ ] 6.1 Add `[SEARCH]` mode display in status bar when in Search mode (with distinct color, e.g., yellow background)
- [ ] 6.2 Send `ModeChanged { mode: "SEARCH" }` to server when entering search mode

## 7. Cleanup

- [ ] 7.1 Remove `BufferSearch` variant from `RemuxCommand` (replaced by search mode)
- [ ] 7.2 Update any command parsing/handling that references `BufferSearch` or the buffer group
- [ ] 7.3 Remove search-related keybindings from Visual mode (`/`, `n`, `N`) if search mode fully replaces them, or keep Visual mode search as a secondary entry point
