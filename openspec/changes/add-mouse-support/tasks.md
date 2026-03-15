## 1. Enable Mouse Capture

- [ ] 1.1 Add `EnableMouseCapture` to terminal setup in `client/terminal.rs` and `DisableMouseCapture` to terminal restore
- [ ] 1.2 Add `Event::Mouse` branch to the event loop in `main.rs` alongside existing `Event::Key` handling

## 2. Protocol Extension

- [ ] 2.1 Add `MouseClick { x: u16, y: u16 }` variant to `ClientMessage` in `protocol.rs`
- [ ] 2.2 Add `MouseDrag { start_x: u16, start_y: u16, end_x: u16, end_y: u16 }` variant to `ClientMessage`
- [ ] 2.3 Add `CopyToClipboard { data: String }` variant to `ServerMessage`

## 3. Client-Side Mouse Event Handling

- [ ] 3.1 In `main.rs` event loop, translate crossterm `MouseEvent` (Down, Drag, Up) into `ClientMessage::MouseClick` or `ClientMessage::MouseDrag` and send to server
- [ ] 3.2 Throttle drag events client-side to avoid flooding the server (coalesce to ~60fps)
- [ ] 3.3 Handle `ServerMessage::CopyToClipboard` on the client by writing OSC 52 escape sequence to stdout

## 4. Server-Side Hit Testing Infrastructure

- [ ] 4.1 Add `ClickTarget` struct/enum to represent hit test results: `Pane(PaneId)`, `Tab(usize)`, `StackLabel(PaneId)`, `None`
- [ ] 4.2 Track tab label positions (x_start, x_end, y, tab_index) during status bar rendering in `compositor.rs`
- [ ] 4.3 Track stack label positions (x_start, x_end, y, pane_id) during border/stack header rendering in `compositor.rs`
- [ ] 4.4 Implement `hit_test(x, y, tab_regions, stack_regions, pane_rects) -> ClickTarget` function that checks tab labels first, then stack labels, then pane areas

## 5. Click-to-Focus

- [ ] 5.1 Handle `ClientMessage::MouseClick` in `daemon.rs` — run hit test and dispatch to appropriate focus/tab/stack command
- [ ] 5.2 For `ClickTarget::Pane(id)`: set `tab.focused_pane = id` and trigger re-render
- [ ] 5.3 For `ClickTarget::Tab(index)`: call existing tab switch logic (`TabGoto`)
- [ ] 5.4 For `ClickTarget::StackLabel(id)`: activate the stacked pane within its stack

## 6. Mouse Text Selection

- [ ] 6.1 Add selection state to the server: `MouseSelection { pane_id: PaneId, start: (u16, u16), end: (u16, u16) }` tracked per client
- [ ] 6.2 Handle `ClientMessage::MouseDrag` in `daemon.rs` — map screen coordinates to pane-local coordinates using layout rects, update selection state
- [ ] 6.3 In the compositor, apply fg/bg color inversion for cells within the active mouse selection range
- [ ] 6.4 On mouse button release (final drag event or a subsequent click), extract selected text from the pane's scrollback buffer
- [ ] 6.5 Send `ServerMessage::CopyToClipboard` with the extracted text to the originating client
- [ ] 6.6 Clear the selection state and trigger re-render to remove highlighting

## 7. Testing

- [ ] 7.1 Add unit tests for `hit_test` function with various coordinate inputs (pane interior, tab label, stack label, border/gap, outside)
- [ ] 7.2 Add unit tests for coordinate mapping from screen space to pane-local space
- [ ] 7.3 Add protocol round-trip tests for `MouseClick`, `MouseDrag`, and `CopyToClipboard` message variants
- [ ] 7.4 Manual integration test: click panes, tabs, stacked tabs to verify focus changes
- [ ] 7.5 Manual integration test: click-drag to select text and verify clipboard content
