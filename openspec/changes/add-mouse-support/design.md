## Context

Remux is a client-server terminal multiplexer built with crossterm. Input events flow from the client event loop (`main.rs`) through `InputHandler` (`client/input.rs`), which either forwards raw bytes to the PTY or converts keys into `RemuxCommand`s sent via `ClientMessage::Command`. The server dispatches commands in `daemon.rs`, updating focus, layout, and tabs. Rendering is handled by the compositor, which produces `RenderCell` buffers sent back to clients.

Currently, only `Event::Key` events are processed. Crossterm already supports mouse events via `Event::Mouse` — they just need to be captured and handled. The server already computes `Rect` positions for all visible panes via `compute_layout()`, providing the foundation for click hit-testing.

## Goals / Non-Goals

**Goals:**

- Click-to-focus: clicking a pane, tab, or stacked tab activates it
- Click-and-drag text selection with visual highlighting and auto-copy to clipboard
- Mouse events handled entirely client-side for hit-testing when possible, server-side only for focus changes
- Use OSC 52 for clipboard access (works over SSH, no external dependencies)

**Non-Goals:**

- Mouse wheel scrolling (future work)
- Right-click context menus
- Mouse-based pane resizing by dragging borders
- Configurable mouse behavior or disabling mouse support

## Decisions

### 1. Client-side vs server-side hit-testing

**Decision:** Perform hit-testing on the server.

The server owns the layout state and computes pane rectangles. The client does not currently have layout geometry. Rather than duplicating layout state on the client, the client sends mouse coordinates to the server, which resolves them to pane/tab/stack targets.

**Alternative considered:** Client-side hit-testing — would require the server to send layout metadata to clients, adding protocol complexity for marginal latency benefit.

### 2. Protocol extension

**Decision:** Add a `MouseClick { x, y }` and `MouseDrag { start_x, start_y, end_x, end_y }` variant to `ClientMessage`, and a `CopyToClipboard { data: String }` variant to `ServerMessage`.

Mouse clicks map to focus commands server-side. For text selection, the server computes selected text from the scrollback buffer and sends the clipboard payload back to the client, which writes it via OSC 52.

**Alternative considered:** Adding mouse button/modifier info — deferred until needed (scroll, right-click).

### 3. Clipboard mechanism

**Decision:** Use OSC 52 escape sequences for clipboard access.

OSC 52 works across local terminals, SSH sessions, and tmux/screen nesting without external crate dependencies. The client writes `\x1b]52;c;<base64-data>\x07` to stdout.

**Alternative considered:** `arboard` crate — requires system clipboard access, fails over SSH, adds a native dependency.

### 4. Text selection rendering

**Decision:** The server handles selection highlighting by inverting fg/bg colors for selected cells in the render buffer.

During a drag, the client sends `MouseDrag` messages. The server tracks selection state (start/end coordinates mapped to scrollback buffer positions), applies highlight to affected cells during compositing, and sends the updated render to clients.

### 5. Mouse capture lifecycle

**Decision:** Enable mouse capture (`crossterm::event::EnableMouseCapture`) during terminal setup in `client/terminal.rs`, and disable it on restore. This mirrors how alternate screen and raw mode are already managed.

### 6. Tab and stack label hit detection

**Decision:** The compositor already renders tabs in the status bar and stack labels in borders. To support click detection, the server will track the screen-space positions of tab labels and stack labels during compositing, storing them as `(x_start, x_end, y, target_index)` tuples. Mouse click coordinates are checked against these regions before falling through to pane hit-testing.

## Risks / Trade-offs

- **[Terminal compatibility]** Not all terminals support OSC 52. → Mitigation: OSC 52 is widely supported (iTerm2, Alacritty, kitty, WezTerm, foot, Windows Terminal). Graceful degradation — if clipboard write fails, selection still works visually.

- **[Latency on drag]** Continuous mouse drag events sent to server could cause latency. → Mitigation: Throttle drag events client-side (e.g., coalesce to ~60fps). Only send final selection on mouse release for clipboard copy.

- **[SSH overhead]** Mouse events over high-latency connections. → Mitigation: The events are small JSON messages. Throttling handles this.

- **[Mode interaction]** Click-drag entering visual mode could conflict with existing visual mode keyboard entry. → Mitigation: Mouse-initiated visual mode is a distinct sub-mode — releasing the mouse button auto-copies and exits, unlike keyboard visual mode which persists.
