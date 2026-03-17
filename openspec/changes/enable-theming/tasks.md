## 1. ThemeColor type and serde

- [ ] 1.1 Add `ThemeColor` enum to `src/config/theme.rs` with `Named(String)`, `Indexed(u8)`, and `Rgb(u8, u8, u8)` variants. Implement `Deserialize` supporting string (`"green"`), `{ ansi = N }`, and `{ rgb = [R,G,B] }` formats.
- [ ] 1.2 Implement `ThemeColor → crossterm::style::Color` conversion (for client-side/which-key)
- [ ] 1.3 Implement `ThemeColor → CellColor` conversion (for compositor-side)

## 2. ThemeConfig struct

- [ ] 2.1 Create `ThemeConfig` struct with `Deserialize` and all theme fields using `ThemeColor`. Add missing fields: `separator_fg`, `pane_label_fg`, `pane_label_bg`, `session_name_fg`. Implement `Default` matching current hardcoded values.
- [ ] 2.2 Add `theme: ThemeConfig` field to `AppearanceConfig`. Update `Config::theme()` to construct `Theme` from `ThemeConfig`.
- [ ] 2.3 Add `CompositorTheme` struct using `CellColor` fields. Add conversion from `ThemeConfig → CompositorTheme`.

## 3. Wire theme into compositor

- [ ] 3.1 Add `theme: &CompositorTheme` parameter to `composite()`, `draw_zellij_panes()`, `draw_tmux_panes()`, and `draw_status_bar()`
- [ ] 3.2 Replace hardcoded border colors in `draw_zellij_panes()` with theme lookups (`frame_active_fg`, `frame_fg`, `frame_bg`)
- [ ] 3.3 Replace hardcoded mode indicator colors in both `draw_zellij_panes()` and `draw_status_bar()` with theme lookups
- [ ] 3.4 Replace hardcoded status bar colors (background, session name, separators, tab labels) with theme lookups
- [ ] 3.5 Replace hardcoded colors in `draw_tmux_panes()` (dividers, tab bar, labels) with theme lookups

## 4. Pass theme from daemon to compositor

- [ ] 4.1 Update daemon to construct `CompositorTheme` from config and pass it to `composite()` calls

## 5. Config and documentation

- [ ] 5.1 Add commented-out `[appearance.theme]` section to `config.sample.toml` with all keys and default values
- [ ] 5.2 Add tests: `ThemeColor` serde round-trips, `ThemeConfig` default matches hardcoded values, partial config deserialization

## 6. Verify

- [ ] 6.1 Run full test suite — all existing compositor tests pass with default theme (identical output)
- [ ] 6.2 Manual smoke test — Remux with no theme config looks identical to current behavior
