## Context

Remux has a `Theme` struct (`src/config/theme.rs`) using `crossterm::style::Color` and a `Config::theme()` method that returns `Theme::default()`. The which-key popup already reads from `Theme`. However, the compositor (`src/server/compositor.rs`) hardcodes ~30 `CellColor::Indexed(...)` literals for borders, status bar, mode indicators, tabs, and pane labels. The `Theme` struct is not serializable — it uses `crossterm::style::Color` which doesn't implement `Deserialize`.

The compositor runs server-side and uses `CellColor` (from `protocol.rs`), while the theme lives in config and uses `crossterm::style::Color`. These two color types need bridging.

## Goals / Non-Goals

**Goals:**
- Users can customize all UI colors via `[theme]` in `config.toml`
- All hardcoded colors in the compositor are replaced with theme lookups
- Sensible defaults preserved — unconfigured Remux looks identical to today
- Theme is loaded once at config parse time and threaded through to drawing functions

**Non-Goals:**
- Runtime theme switching / hot-reloading (future work)
- Named theme presets (e.g. "dracula", "solarized") — users define colors directly
- Customizing box-drawing characters or border shapes (that's `border_style`)
- Per-pane or per-tab theming

## Decisions

### 1. Serde-compatible color type in theme

**Decision**: Add a `ThemeColor` enum with serde support that converts to both `crossterm::style::Color` (client-side) and `CellColor` (compositor-side).

**Alternatives considered**:
- _Derive serde on `crossterm::style::Color`_ — not possible without a newtype wrapper; crossterm doesn't provide serde impls.
- _Use string parsing_ (e.g. `"#ff0000"`, `"red"`) — more user-friendly but adds parsing complexity. Start with structured TOML representation; string sugar can be added later.

**Format in TOML**:
```toml
[theme]
mode_insert_fg = "black"         # named color
mode_insert_bg = "green"
frame_fg = { ansi = 8 }          # 256-color index
status_bar_bg = { rgb = [35, 35, 35] }  # true-color
```

### 2. Theme field on AppearanceConfig (not top-level)

**Decision**: Add `theme: ThemeConfig` as a field on the existing `AppearanceConfig` struct, matching the existing `[appearance]` section. In the TOML file, this becomes `[appearance.theme]`.

**Rationale**: Theme is appearance configuration. Keeping it under `[appearance]` is consistent with `border_style` and `status_bar_position` already being there.

**Alternative**: Top-level `[theme]` section — simpler TOML path but breaks the existing organizational structure.

### 3. Thread `&Theme` through compositor functions

**Decision**: Add a `theme: &Theme` parameter to `composite()`, `draw_zellij_panes()`, `draw_tmux_panes()`, and `draw_status_bar()`. The theme is converted from `ThemeConfig` → `CompositorTheme` (using `CellColor`) at the compositor boundary.

**Rationale**: The compositor works with `CellColor`, not `crossterm::style::Color`. Converting once at the boundary avoids repeated conversions inside tight drawing loops.

### 4. Keep existing `Theme` struct for client-side (which-key)

**Decision**: The existing `Theme` struct using `crossterm::style::Color` remains for client-side use (which-key popup). `ThemeConfig` (serde) converts to both `Theme` (client) and `CompositorTheme` (server).

**Flow**: `TOML → ThemeConfig (serde) → Theme (crossterm colors, client) + CompositorTheme (CellColor, server)`

### 5. Missing theme fields in the current Theme struct

**Decision**: Add fields to cover all compositor-hardcoded colors currently missing from `Theme`:
- `separator_fg` — for `│` and `|` dividers (currently `Indexed(240)`)
- `pane_label_fg` / `pane_label_bg` — for pane name labels in borders
- `session_name_fg` — for session name in status bar (currently `Indexed(6)`, cyan)

## Risks / Trade-offs

- **[Compositor test breakage]** → Tests assert specific `CellColor::Indexed(...)` values. They'll need updating to expect theme-derived colors. Mitigation: default theme produces identical colors.
- **[Theme struct divergence]** → Two theme representations (`Theme` for client, `CompositorTheme` for server) could drift. Mitigation: both are derived from a single `ThemeConfig` source of truth.
- **[Config file complexity]** → Users unfamiliar with ANSI color indices may struggle. Mitigation: support named colors (`"red"`, `"green"`) alongside `{ ansi = N }` and `{ rgb = [R,G,B] }`. Document defaults in `config.sample.toml`.
