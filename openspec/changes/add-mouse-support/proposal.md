## Why

Remux currently has no mouse support — all navigation (pane focus, tab switching, stack cycling) requires keyboard shortcuts. Adding mouse interaction makes the multiplexer immediately intuitive for new users and faster for common operations like clicking a visible pane to focus it. Mouse-based text selection with automatic clipboard copy is a standard expectation for terminal multiplexers.

## What Changes

- Enable crossterm mouse capture on the client side
- Handle mouse click events in `main.rs` event loop and forward them to the server
- Extend the client-server protocol with mouse event messages
- Implement hit-testing on the server: map click coordinates to panes, tabs, and stacked tabs
- Click on a pane → focus that pane
- Click on a tab in the status bar → switch to that tab
- Click on a stacked tab label → activate that stack entry
- Click-and-drag to select text → enter visual mode, highlight selection, auto-copy to system clipboard on release

## Capabilities

### New Capabilities

- `mouse-click-focus`: Click-to-focus for panes, tabs, and stacked tabs — hit-testing against layout rects and UI elements
- `mouse-text-selection`: Click-and-drag text selection with visual highlighting and automatic clipboard copy

### Modified Capabilities

- `modal-input`: Mouse events must interact with the mode system — click-drag enters visual mode, release exits it
- `client-server`: Protocol needs new message types for mouse events and clipboard data

## Impact

- **Client input** (`main.rs`, `client/input.rs`): New event branch for `Event::Mouse`, mouse capture enable/disable in terminal setup
- **Protocol** (`protocol.rs`): New `ClientMessage` variants for mouse clicks and drags
- **Server daemon** (`server/daemon.rs`): New command handler for mouse-based focus changes
- **Layout** (`server/layout.rs`): `compute_layout()` results used for hit-testing (read-only, no changes needed)
- **Compositor** (`server/compositor.rs`): Tab bar and stack label positions needed for click targets; selection highlighting during drag
- **Terminal setup** (`client/terminal.rs`): Enable/disable mouse capture
- **Dependencies**: May need a clipboard crate (e.g., `arboard` or OSC 52 escape sequences) for system clipboard access
