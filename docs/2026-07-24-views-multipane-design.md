# Views: cross-machine read/write multi-pane monitor

**Status:** design for review — NOT yet implemented. Build will be held uncommitted until tested (may be discarded).
**Date:** 2026-07-24

## Goal
A **View** is a virtual tab whose cells are live **aliases** to real panes that may live on
different machines / sessions / tabs. Each cell is **read/write**: output streams from the real
pane, and keystrokes to the focused cell route back to that real pane on its server. Lets the user
watch and drive several long-running things (Claude sessions, builds, logs) from one tab.

This is `link-window`/`join-pane` semantics generalized to **per-pane** and **cross-server** — the
gap no existing tool (tmux, zellij, iTerm `-CC`, tmate/ttyd) fills. remux is well-suited: it already
has a multi-server client (`ConnectionManager`), per-pane `Screen`+diff streaming, and
min-across-viewers pane sizing (e2fd468).

## Core model
- `View { cells: Vec<ViewCell>, layout: LayoutMode }` where `ViewCell { conn: ConnId, pane_id: PaneId }`.
- A View is a special **tab** (client-side virtual layout); its cells are references, not owned panes.
- Focus, `Alt-hjkl`, and mouse click move between cells like normal panes.
- Exactly **one** focused cell at a time; input goes only to it. Unmistakable active-cell border.

## The unavoidable tradeoff (decided: start with A, add B)
One PTY has one size; an interactive app can't render to two sizes at once. A cell smaller than the
pane's home tab forces **smallest-wins reflow** of the shared pane (everywhere it's shown).
- **Model A — honest shared pane (MVP):** cell counts as a viewer; pane sizes to min across viewers.
  Simple, predictable. Size cells generously or use for reflow-friendly apps.
- **Model B — focus-to-zoom (follow-up):** unfocused cells render **read-only clipped** (no size
  demand → no reflow); focusing a cell enlarges it to a workable size and makes it read/write there.
  Watching never reflows; only the actively-driven cell demands a size. This is the version that
  feels right with Claude panes.

## Grid layout (new, default for Views)
New `LayoutMode::Grid` (src/server/layout.rs) — equal-size panes:
- `cols = ceil(sqrt(n))`, `rows = ceil(n / cols)`, row-major fill, equal splits.
- General-purpose (usable for normal tabs too), but is the **default** layout a new View starts in.
- Changeable via the existing `LayoutNext` (Prefix Space / Alt-Space); cycle becomes
  Grid → Bsp → Master → Monocle → Grid for Views.

## Views are multiple + named; "active view" disambiguation
`ViewNew` can create several views, so each View has a **name**. `ViewNew { name }` takes the name
as an argument (mirrors `CreateSession { name }`); the interactive `Prefix v n` opens a text sub-mode
to type it (reuse the CreateFolder-style buffer), falling back to an auto-name (`View 1`, `View 2`,
…) if left empty. Renamable later via `ViewRename { name }`. Disambiguation rule:
- **Operate-in-place** (you are *inside* a view): `ViewRemovePane`, `LayoutNext`, `ViewClose` act on
  the view currently being viewed. No ambiguity.
- **Add-from-outside** (`ViewAddPane`, session-manager `va`): there is no "current view", so open a
  **view-picker popup** — the same widget/style as the session-switcher `SessionSwitchOverlay`.

### View-picker popup (reuses `SessionSwitchOverlay`)
- Lists existing views by name, with a **`＋ New view`** entry at the top.
- Select a view → alias the focused pane into it. Select `＋ New view` → create (auto-named) + add.
- Fast paths: **zero** views → skip popup, create `View 1` and add; **one** view → optionally add
  directly (config toggle) so the popup only appears when there is a real choice.
- Cells that come from different servers carry the host/folder prefix already used in the session
  switcher.

## Commands + keybindings
New commands (RemuxCommand): `ViewNew { name }`, `ViewAddPane` (→ view-picker popup),
`ViewRemovePane` (drop focused cell from the current view), `ViewClose`, `ViewRename { name }`.

Prefix which-key: new top-level group **`v` = view** (verify `v` is free at top level):
- `Prefix v n` — new view (prompt for name; empty, Grid layout)
- `Prefix v a` — add current focused pane to a view (opens the picker)
- `Prefix v x` — remove focused cell from the current view
- `Prefix v Space` — cycle current view's layout (alias of LayoutNext)

Session manager (the natural multi-select surface, vim chords like existing tn/pn/...):
- `va` — add the selected node's pane(s) to a view (opens the picker; multi-select supported)
- multi-select across servers/sessions/tabs → "add to view"
All keybindings stay under `[keybindings.command]` / `[keybindings.session_manager]` per existing
config structure (no new passthrough concept).

## Protocol deltas (the only genuinely new server capability)
1. `SubscribePane { conn, pane_id, size }` / `UnsubscribePane { pane_id }` — server streams that
   pane's `Screen`/diffs regardless of foreground attachment, and folds `size` into the pane's
   min-across-viewers sizing (so a remote server sizes its pane to the smallest live viewer,
   including this client's cell).
2. **Input by identity:** a focused cell sends `Input` targeted at `(conn, pane_id)` rather than
   "the foreground session's focused pane." Extension of existing `ConnectionManager` routing.
3. Per-pane resize is carried by `SubscribePane.size` (re-sent when the cell's allotment changes).

Output/render: reuse `blit_screen(buffer, screen, rect, offset)` — the client composites each
subscribed cell's `Screen` into its grid rect. (Model B: unfocused = clipped/letterboxed,
bottom-anchored to show latest output.)

## Sizing
Pane effective size = min over all viewers: its home-tab allotment on any attached client + every
View cell currently showing it. remux already does min-across-clients; the View cell is just another
viewer, cross-server via `SubscribePane.size`.

## Safety
Read/write cells can target production machines. One focused cell; strict input-to-focused-only;
clear active indicator. Optional **broadcast mode** (type once → all cells, iTerm-style) is a future
add, explicit and off by default.

## Staged build plan
1. `LayoutMode::Grid` + `LayoutNext` cycle (self-contained, testable in isolation).
2. `SubscribePane` streaming primitive (output only) + client-side View tab rendering (read-only first).
3. Input-by-identity routing → Model A (honest shared pane, read/write MVP).
4. Commands + keybindings (`ViewNew/AddPane/RemovePane/Close`, `v` group, session-manager `va`).
5. Model B (focus-to-zoom): clipped unfocused cells, enlarge-on-focus.

Nothing here is throwaway even if Views are dropped: Grid layout, `SubscribePane`, and
input-by-identity are independently useful.

## Guardrail
Per user: build without committing until the user has tested and decided to keep it.
