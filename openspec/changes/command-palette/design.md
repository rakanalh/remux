## Context

Remux has a which-key popup for navigating keybinding trees, but no way to search and execute commands by name. The which-key popup (`client/whichkey.rs`) and rename overlay (`client/input.rs`) provide established patterns for text overlays and input capture. The `RemuxCommand` enum in `protocol.rs` defines all available commands, and `parse_command()` in `keybindings.rs` already converts PascalCase strings to `RemuxCommand` variants.

## Goals / Non-Goals

**Goals:**
- Provide a command palette popup triggered by `:` in command mode
- Real-time filtering and tab autocompletion of command names
- Execute any `RemuxCommand` variant from the palette, including those with arguments
- Consistent visual style with existing overlays (which-key popup)

**Non-Goals:**
- Fuzzy matching (prefix/substring matching is sufficient for v1)
- Command history or frecency sorting
- Custom user-defined commands or aliases
- Server-side rendering of the palette (this is entirely client-side)

## Decisions

### 1. Client-side only — no protocol changes needed

The command palette is a client-side UI feature. The client already has `parse_command()` to convert strings to `RemuxCommand`, and sends commands to the server via `ClientMessage::Command`. No new server-side handling is needed.

**Alternative considered**: Adding a `RemuxCommand::CommandPalette` variant. Rejected because the palette is purely a UI concern — it produces existing commands, it isn't one itself.

### 2. New `CommandPalette` mode in the `Mode` enum

Add a `CommandPalette` variant to the `Mode` enum alongside Normal, Command, and Visual. This keeps input routing clean — when in `CommandPalette` mode, all keys route to palette handling.

**Alternative considered**: Using a sub-state within Command mode (like `rename_overlay`). Rejected because the palette has its own full input loop (text entry, tab, navigation) that would clutter command mode handling.

### 3. New `CommandPaletteState` struct in a new `client/command_palette.rs` module

Similar to `WhichKeyPopup`, this struct holds the input buffer, filtered matches, selected index, and renders draw commands. Keeping it in its own module follows the existing pattern.

### 4. Command list derived from `RemuxCommand` at compile time

Build the list of available command names from the `RemuxCommand` enum variants. The existing `parse_command()` function handles the reverse direction. We add a `command_names() -> Vec<&'static str>` function that returns all variant names in PascalCase.

**Alternative considered**: Dynamic discovery from keybindings. Rejected because not all commands have keybindings, and the palette should expose everything.

### 5. Tab completion: longest common prefix + cycle

First Tab press completes to the longest common prefix of all matches. Subsequent Tab presses cycle through individual matches. Shift+Tab cycles backward. This matches shell-style completion behavior.

### 6. Trigger via `:` in command mode

The `:` key in command mode opens the palette (`<Prefix>:`). This is the Vim convention for entering command-line mode and avoids conflicting with the existing `p` pane group.

## Risks / Trade-offs

- **Large command list**: All ~40 RemuxCommand variants shown at once may be noisy → Mitigate with good filtering; the list shrinks quickly as user types
- **Argument parsing complexity**: Some commands take strings, some take numbers, some take multiple args → Mitigate by reusing/extending `parse_command()` which already handles this
- **Mode proliferation**: Adding a 4th mode increases state machine complexity → Mitigate by keeping the palette mode self-contained with clear enter/exit transitions
