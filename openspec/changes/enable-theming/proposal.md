## Why

All UI colors in the compositor are hardcoded as indexed ANSI values, even though a `Theme` struct and `Config::theme()` method already exist. Users cannot customize Remux's appearance without editing source code. Wiring up the existing theme infrastructure to the rendering path and exposing it via TOML config makes the UI fully customizable with minimal effort.

## What Changes

- Replace all hardcoded `CellColor::Indexed(...)` values in the compositor with lookups from the `Theme` struct
- Add TOML deserialization to `Theme` so users can define a `[theme]` section in `config.toml`
- Update `Config::theme()` to load user-defined theme values (falling back to defaults)
- Convert `Theme` colors from `crossterm::style::Color` to a serde-compatible color type that maps to `CellColor`
- Add a `[theme]` section to `config.sample.toml` documenting all theme keys
- Pass the theme through the server to the compositor's drawing functions

## Capabilities

### New Capabilities
- `theme-config`: TOML-based theme configuration — parsing `[theme]` from config, color type serialization, and merging with defaults
- `theme-rendering`: Compositor uses theme colors for all UI elements — borders, status bar, mode indicators, tabs, and pane labels

### Modified Capabilities
_(none — no existing spec-level behavior changes)_

## Impact

- **Config**: `Config` struct gains a `theme` field; `Theme` struct gains `Deserialize`
- **Server/Compositor**: All `draw_*` functions receive a `&Theme` parameter instead of using hardcoded colors
- **Protocol**: `CellColor` usage unchanged — only the _source_ of color values changes (theme instead of literals)
- **Client**: Which-key popup already uses `Theme` — no changes needed there
- **Dependencies**: No new crates (serde/toml already in use)
