# Views build — implementation decisions & deferrals (steps 2–5)

**Status:** built on branch `views`, held UNCOMMITTED for review/testing (per the guardrail).
**Date:** 2026-07-26
**Companion to:** `2026-07-24-views-multipane-design.md`

This records decisions taken while implementing the staged build plan, including
three places where the original design doc left a genuine gap that had to be
resolved to proceed. Everything here is reversible — nothing is committed.

## What landed per step

- **Step 1 (pre-existing on branch):** `LayoutMode::Grid`.
- **Step 2:** `SubscribePane`/`UnsubscribePane` + `ServerMessage::PaneContent`
  (full per-pane snapshot). Server streams a subscribed pane's rendered content
  to that client regardless of foreground session/tab. Snapshot built by a new
  `render_pane_snapshot(screen)` helper reusing the private `blit_screen`.
- **Step 3 (server):** `InputToPane { pane_id, data }` routes keystrokes to a
  pane by identity, independent of foreground focus (input-by-identity).
  `SubscribePane` also carries the cell's `cols`/`rows`.
- **Step 3 (client) + Step 4:** client-side `View` model, a client compositor
  that assembles `PaneContent` snapshots into a grid, focus movement between
  cells, targeted input, the `ViewNew/AddPane/RemovePane/Close/Rename` commands,
  a which-key group, a view-picker overlay, and the session-manager chord.
- **Step 5:** Model B focus-to-zoom (unfocused cells clipped/read-only).

## Design gaps resolved (please review)

### 1. `v` is not free — view which-key group placed on `w`
The doc (§Commands, "verify `v` is free at top level") assumed `v` was available.
It is not: `keybindings.rs` binds `v` → `EnterVisualMode` at the prefix top level.
Rather than silently relocate visual mode (a common, load-bearing key), the view
group is bound to **`w`** ("view/window"): `w n` new, `w a` add pane, `w x`
remove cell, `w Space` layout-next. **Rebindable** in config. If you'd rather
have `v`, we must first move visual mode to another key.

### 2. The doc's client render strategy is not literally implementable
The doc (§Output/render) says "reuse `blit_screen(buffer, screen, rect, offset)`
— the client composites each subscribed cell's `Screen`." The client has no
`Screen` objects and `blit_screen` is server-private; the client only ever
receives finished `Vec<Vec<RenderCell>>`. Resolution: a **client-side
compositor** (`src/client/view.rs`) blits each cell's `PaneContent` snapshot
(already a `Vec<Vec<RenderCell>>`) into its grid rect. Same visual result,
snapshot-based instead of `Screen`-based.

### 3. Model A "min-across-viewers" pane sizing is DEFERRED
`SubscribePane` now carries the cell's `cols`/`rows`, and the server stores them
per-subscriber, but does **not** yet fold them into a pane's size. True Model A
(smallest-viewer-wins reflow of the shared pane) requires changing the pane
resize path (`broadcast_full_render`'s size math currently mins only over
*attached* clients of the pane's own session; a non-attached View-cell subscriber
is not counted). Until then, a cell renders the pane's **current-size** snapshot,
clipped/letterboxed into the cell rect (bottom-anchored to show latest output).
This coincides with Model B's "unfocused = read-only clipped, no size demand".

Consequence: **focus-to-zoom (step 5) changes layout + which cell is read/write,
but does not yet resize the real PTY on focus.** Making the focused cell demand a
workable size from its (possibly remote) source pane is the remaining piece of
the sizing work, tracked as a follow-up.

## Validation boundary (important)
- **Steps 2–3 (server/protocol): behaviorally verified.** Two Python protocol
  harnesses pass, including one proving input reaches a *non-focused* pane via
  `InputToPane` while another pane is focused.
- **Steps 3-client + 4: built and compile-verified only.** The pure client
  compositor/geometry/model in `src/client/view.rs` has 14 passing unit tests,
  but the `main.rs` integration — `PaneContent`→composite→paint, the `SendToPty`
  → `InputToPane` fork, the `PaneFocus` intercept, the view-picker/session-tree
  round-trip — has **never executed**. It needs a human at a real terminal.
- **Step 5 (Model B): not built.** Without the deferred PTY-resize-on-focus,
  Model B collapses to a layout variant: its defining "unfocused cells don't
  demand size" property is *already* true because no cell demands size yet, and
  its "focus enlarges to a workable size" needs the real pane resized on the
  source server — the same deferred sizing fold. Model B is not meaningfully
  implementable until that sizing work lands.

## Deferred / stubbed (carry-forward TODO)
- Model A min-across-viewers PTY sizing (fold `SubscribePane.cols/rows` into the
  pane's size + resize) — the gating item for real Model A and Model B.
- Bsp/Master view layouts (only Grid + Monocle today).
- Cursor in `PaneContent` (none on the wire → hardware cursor hidden in views).
- Per-focused-cell `application_cursor_keys` (currently tracks the foreground
  session, so arrow-key encoding into a cross-session cell may be wrong).
- View re-activation / switcher: `w q` deactivates but keeps the view; there is
  no `w w` to re-show it, and `w a` into an *inactive* view adds an inert cell.
- Mouse-click cell focus; session-manager `va` chord; `ViewRename`.
- Re-subscribe cells at new sizes on view-layout change.
- View has no status-bar row (composites the full terminal).

## Suggested manual test (first human run)
`w n` (name a view) → `w a` (add focused pane, pick the view) → type into it →
Alt-h/j/k/l to move the active cell → `w space` (Grid↔Monocle) → `w x` (remove
cell) → `w q` (close). Two probes most likely to reveal defects:
1. Add a pane from a *different* session running a full-screen app (vim) and
   press arrows — wrong encoding ⇒ the `application_cursor_keys` gap above.
2. On `w q`, confirm the screen repaints cleanly (no leftover view borders) —
   the enter/leave transition relies on a `Resize` nudge to force a fresh server
   FullRender against the renderer's diff back-buffer.
Note: `w n` before adding any pane paints an all-blank screen — expected, not a hang.

## Other notes
- `materialize_session` (resurrect path) has its own per-pane forwarding loop;
  the subscriber fan-out was added there too, else a subscribed-then-resurrected
  pane would stop streaming.
- The subscriber fan-out is guarded by a cheap "any subscriber?" check so panes
  with zero subscribers pay nothing per PTY batch.
- Client TUI behavior is not covered by the Python protocol harness (that drives
  the socket, not a terminal); client-side logic is covered by Rust unit tests
  on the pure compositor/geometry/model functions plus build/clippy.
