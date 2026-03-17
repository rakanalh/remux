## Context

Remux has a modal input system with Normal, Command, and Visual modes. Visual mode already contains partial search infrastructure (`search_query`, `search_matches`, `current_match` fields in `VisualState`, plus `n`/`N` key handlers). However, search is not a first-class feature — there's no dedicated search prompt UI, no status bar integration, and the `BufferSearch` command under `Ctrl+a b /` is disconnected from Visual mode's search state.

The status bar renders mode, session name, tabs, and layout mode via `StatusInfo` struct in `compositor.rs`. It's server-rendered as part of the composited frame.

## Goals / Non-Goals

**Goals:**
- Dedicated Search mode with text input prompt and match navigation
- `(x/y)` match count displayed next to layout in status bar
- Consolidate buffer/search bindings under `Ctrl+a s`
- Reuse existing `VisualState` search infrastructure where possible

**Non-Goals:**
- Regex search (plain substring matching is sufficient for now)
- Live/incremental search (search executes on Enter, not while typing)
- Search across multiple panes
- Persistent search history

## Decisions

### 1. Search as a new Mode variant vs. sub-state of Visual mode

**Decision**: Add `Search` as a new `Mode` variant in `input.rs`.

**Rationale**: Search mode has distinct keybindings (text input during prompt, then `n`/`N`/`Escape` during navigation) that don't overlap with Visual mode's vim motions. A separate mode is cleaner than overloading Visual mode with sub-states. The mode indicator in the status bar naturally shows `[SEARCH]`.

**Alternative considered**: Extending Visual mode with a search sub-state. Rejected because it would complicate Visual mode's already complex key handler and the two modes have different UX (Visual = scrollback navigation with cursor, Search = find matches and cycle).

### 2. Search state ownership

**Decision**: Add a `SearchState` struct to `InputHandler` (client-side), similar to how `VisualState` is managed. The `SearchState` holds the query string being typed, confirmed query, match positions, and current match index.

**Rationale**: Search is a client-side concern — the client requests scrollback content from the server, searches it locally, and manages highlight/navigation state. This mirrors how Visual mode works.

### 3. Getting scrollback content for search

**Decision**: Reuse the existing scrollback content request mechanism. When search is confirmed, the client sends a message to the server requesting the active pane's full scrollback text. The server responds with the text, and the client performs substring matching.

**Alternative considered**: Server-side search with match positions returned to client. Rejected to keep the server simple and because the client already handles scrollback content for Visual mode.

### 4. Status bar match count

**Decision**: Extend `StatusInfo` with optional `search_info: Option<(usize, usize)>` field (current_match, total_matches). The server gets this from the client's mode state via `ModeChanged` or a new message. `draw_status_bar()` renders `(x/y)` right-aligned next to the layout mode when present.

**Rationale**: StatusInfo is already the conduit for all status bar data. Adding an optional field is minimally invasive.

### 5. Keybinding tree restructuring

**Decision**:
- `s` → Search group: `s` enters search prompt (leaf: `EnterSearchMode`), `e` opens editor (leaf: `BufferEditInEditor`)
- `x` → Session group (moved from `s`)
- Remove `b` (Buffer group) entirely

**Rationale**: `s` for search is intuitive (vim uses `/` but `s` works well as a leader-key group). Session is less frequently used than search, so `x` is a natural fit (e.g., "exit session"). The buffer group only had two commands — both are absorbed into the search group.

### 6. Search mode key handling phases

**Decision**: Search mode has two phases:
1. **Prompt phase**: User types query. Keys go to the prompt (printable chars append, Backspace deletes, Enter confirms, Escape cancels).
2. **Navigation phase**: After Enter, `n`/`N` cycle matches, `Escape` exits to Normal. The search prompt shows the confirmed query (read-only).

**Rationale**: This mirrors vim's `/` search UX and is intuitive for the target user base.

### 7. Search prompt rendering

**Decision**: Render the search prompt as an overlay at the bottom of the active pane, similar to the existing `RenameOverlay` pattern. During prompt phase, show `/query_text█`. During navigation phase, show `/confirmed_query`.

**Rationale**: The rename overlay pattern is already proven in the codebase. Reusing this approach keeps rendering consistent.

## Risks / Trade-offs

- **[Breaking keybindings]** → Users who have `Ctrl+a s` muscle memory for sessions will need to adjust to `Ctrl+a x`. Mitigated by clear documentation and the config being customizable.
- **[Large scrollback search performance]** → Searching 10,000+ lines of scrollback could be slow. Mitigated by using simple substring matching (not regex) and searching only on Enter (not incrementally).
- **[Client-server round trip for scrollback]** → There's latency between confirming the query and seeing results while scrollback content is fetched. For typical scrollback sizes this should be negligible.
