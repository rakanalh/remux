## MODIFIED Requirements

### Requirement: Search in scrollback
The user SHALL be able to search the scrollback buffer via Search mode (`Ctrl+a s`) instead of only from Visual mode. Search SHALL highlight matches and allow navigating between them with `n`/`N`. The `(x/y)` match count SHALL be displayed in the status bar.

#### Scenario: Search scrollback via search mode
- **WHEN** the user presses `Ctrl+a s` and types a search term
- **THEN** matching text is highlighted and the view jumps to the first match

#### Scenario: Navigate matches
- **WHEN** the user presses `n` after a search
- **THEN** the view jumps to the next match

## REMOVED Requirements

### Requirement: Buffer keybinding group
**Reason**: Buffer commands (`Ctrl+a b`) are consolidated under the new search group (`Ctrl+a s`). `BufferSearch` is replaced by search mode, `BufferEditInEditor` moves to `Ctrl+a s e`.
**Migration**: Use `Ctrl+a s` for search, `Ctrl+a s e` for edit-in-editor.
