---
title: Config reference
description: Every config.toml section and option, with defaults.
---

Every option Remux reads from `config.toml`, section by section. Every option is optional. **Reloads** says whether an edit applies live (the client picks it up on save) or needs `remux restart` (the server reads it once at startup). See [Configuration](/remux/configuration/) for where the file lives.

## `[general]`

Server-side. Needs `remux restart`.

| Option | Type | Default | Meaning |
|---|---|---|---|
| `default_shell` | string | unset | Program new panes run. Unset uses `$SHELL`, else `/bin/sh`. |
| `scrollback_lines` | integer | `10000` | Scrollback lines kept per pane. |
| `save_sessions` | bool | `true` | Save session state to disk after every structural change and on shutdown. `false` writes nothing and ignores `automatic_restore`. |
| `automatic_restore` | bool | `true` | On startup, bring saved sessions back live. `false` loads them as dormant entries you can revive from the session manager. |
| `mouse_auto_yank` | bool | `true` | Copy a mouse selection to the clipboard on release and clear it. `false` keeps the selection on screen so you can adjust it in Visual mode. |
| `allow_app_clipboard` | bool | `true` | Let programs in a pane set your clipboard with OSC 52. Clipboard reads are never served, whatever this is set to. |

## `[appearance]`

| Option | Type | Default | Reloads | Meaning |
|---|---|---|---|---|
| `border_style` | `"zellij_style"` \| `"tmux_style"` | `"zellij_style"` | restart | Boxed panes with names in the border, or minimal tmux-style dividers. `Ctrl-a g` toggles it at runtime. |
| `default_layout` | `"bsp"` \| `"master"` \| `"monocle"` \| `"grid"` \| `"columns"` \| `"rows"` \| `"custom"` | `"bsp"` | restart | Layout every new tab starts in. If `[layouts]` disables it, new tabs use the first enabled layout. |
| `which_key_position` | `"anchored"` \| `"centered"` \| `"full_width"` | `"anchored"` | live | Where the which-key popup is drawn. |
| `popup_width_pct` | integer | `80` | restart | Width of the popup terminal, as a percentage of the content area. Clamped to 20–100. |
| `popup_height_pct` | integer | `80` | restart | Height of the popup terminal, as a percentage of the content area. Clamped to 20–100. |
| `pane_title` | string | unset | live | Template pane names are drawn from. Placeholders: `{title}`, `{command}`, `{session}`, `{tab}`, `{cwd}`, `{host}`. Unset shows the pane's title if it has one, else its command. |
| `status_bar_position` | `"top"` \| `"bottom"` | `"bottom"` | — | **Has no effect.** The status bar is always drawn on the bottom row. |

`pane_title` is a client setting: your client sends it to every server it views, remotes included, and each one names panes with it. A name set with `PaneRename` always wins over the template.

## `[appearance.theme]`

Colours for the UI chrome. Each value accepts:

- a named colour: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white`, `dark_grey`, `bright_red`, `bright_green`, `bright_yellow`, `bright_blue`, `bright_magenta`, `bright_cyan`, `bright_white`. Also accepted: `dark_gray`, `light_grey`/`light_gray` (same as `bright_white`) and `light_<colour>` for each `bright_<colour>`. Case does not matter.
- a hex colour: `"#89b4fa"` (6 digits only)
- a 256-colour index: `{ ansi = 237 }`
- an RGB triple: `{ rgb = [137, 180, 250] }`

:::caution
An unrecognised colour name, `"reset"` included, falls back to the terminal's default colour. Remux does not warn about it.
:::

Defaults are the Catppuccin Mocha palette. Colours for pane frames, tab chips and the status bar are drawn by the **server** and need `remux restart`. Everything else, and `tab_style`, reloads live. See [Theme](/remux/theme/) for what each role colours.

| Option | Default |
|---|---|
| `mode_normal_fg` / `mode_normal_bg` | `"#1e1e2e"` / `"#a6e3a1"` |
| `mode_command_fg` / `mode_command_bg` | `"#1e1e2e"` / `"#89b4fa"` |
| `mode_visual_fg` / `mode_visual_bg` | `"#1e1e2e"` / `"#cba6f7"` |
| `mode_search_fg` / `mode_search_bg` | `"#1e1e2e"` / `"#f9e2af"` |
| `mode_unknown_fg` / `mode_unknown_bg` | `{ ansi = 15 }` / `{ ansi = 238 }` |
| `frame_fg` | `"#585b70"` |
| `frame_active_fg` | `"#89b4fa"` |
| `frame_bg` | unset (the terminal's background) |
| `status_bar_fg` / `status_bar_bg` | `"#a6adc8"` / `"#181825"` |
| `tab_active_fg` / `tab_active_bg` | `"#1e1e2e"` / `"#89b4fa"` |
| `tab_inactive_fg` / `tab_inactive_bg` | `"#9399b2"` / `{ ansi = 237 }` |
| `tab_style` | `"plain"` (one of `plain`, `rounded`, `slanted`, `square`) |
| `tab_bell_fg` / `tab_activity_fg` / `tab_silent_fg` | `{ ansi = 9 }` / `{ ansi = 11 }` / `{ ansi = 10 }` |
| `layout_indicator_fg` / `layout_indicator_bg` | `{ ansi = 0 }` / `{ ansi = 245 }` |
| `search_count_fg` / `search_count_bg` | `{ ansi = 0 }` / `{ ansi = 11 }` |
| `sidebar_current_bg` | `"#3e385e"` |
| `whichkey_fg` / `whichkey_bg` / `whichkey_key_fg` | `"#cdd6f4"` / `"#181825"` / `"#a6e3a1"` |
| `separator_fg` | `"#6c7086"` |
| `pane_label_fg` / `pane_label_bg` | unset / unset (the border's colour, on the terminal's background) |
| `session_name_fg` | `"#94e2d5"` |
| `search_match_fg` / `search_match_bg` | `"#1e1e2e"` / `"#585b70"` |
| `search_current_fg` / `search_current_bg` | `"#1e1e2e"` / `"#fab387"` |

`rounded` and `slanted` tab styles draw Powerline glyphs and need a Nerd Font. `square` works in any font.

## `[layouts]`

Which automatic layouts `LayoutNext` (`Alt-Space`, `Ctrl-a Space`) cycles through, for tabs and Views. Server-side, needs `remux restart`. The cycle order is fixed: bsp, master, monocle, grid, columns, rows.

| Option | Type | Default |
|---|---|---|
| `bsp` | bool | `true` |
| `master` | bool | `true` |
| `monocle` | bool | `true` |
| `grid` | bool | `true` |
| `columns` | bool | `true` |
| `rows` | bool | `true` |

A missing key counts as enabled. Custom is always available, because you get it by splitting or resizing by hand. Disabling every layout counts as enabling all of them, and a warning goes to the log. See [Layouts](/remux/layouts/).

## `[modes.command]`

| Option | Type | Default | Meaning |
|---|---|---|---|
| `timeout_ms` | integer | `500` | **Has no effect.** The which-key popup opens as soon as you press the leader. |

## `[keybindings.command]`

The leader, the command-mode key tree and the Alt shortcuts. Reloads live. The full syntax is on [Keybindings](/remux/keybindings/).

| Key | Value | Meaning |
|---|---|---|
| `leader` | key notation | Key that enters Command mode. Default `"Ctrl-a"`. |
| `"<Mod>-<key>"` (e.g. `"Alt-h"`) | action chain, `"@<group>"`, or `""` | A shortcut that works in Normal mode without the leader. The key needs a modifier. |
| a single character (e.g. `v`) | action chain or `""` | A binding pressed right after the leader. |
| `[keybindings.command.<char>]` | table | A group of bindings. `_label` sets the name the which-key popup shows. |

An empty string (`""`) removes a default binding.

`[keybindings.normal]` is a **deprecated** alias for `[keybindings.command]`. It still works but logs a warning, and `[keybindings.command]` wins where both set the same key.

## `[keybindings.session_manager]`

One- or two-character chords used inside the session manager, mapped to action names. Reloads live. The defaults and the list of actions are on [Keybindings](/remux/keybindings/#session-manager-chords).

## `[keybindings.visual]`

**Has no effect.** Visual mode keys are built in and cannot be rebound.

## `[remotes.<name>]`

A remote Remux server reached over SSH. `<name>` is the label shown in the session tree. Reloads live. See [Remotes over SSH](/remux/remotes/).

| Option | Type | Default | Meaning |
|---|---|---|---|
| `ssh` | string | none (required) | SSH destination, e.g. `"user@host"` or a `~/.ssh/config` host alias. |
| `remux_path` | string | `"remux"` | Path to the `remux` binary on the remote. |
| `port` | integer | unset | SSH port, passed as `-p`. |
| `identity` | string | unset | Identity file, passed as `-i`. |
| `extra_args` | array of strings | `[]` | Extra `ssh` arguments, placed before the destination. |
| `auto_connect` | bool | `false` | Connect when the client starts, instead of the first time you expand the remote. |

```toml
[remotes.pi]
ssh = "pi@raspberrypi.local"
remux_path = "/usr/local/bin/remux"
auto_connect = true
```

## `[[sidebar]]`

A sidebar docked to one edge of the terminal. There are none by default. Declare one table per edge. A second sidebar on an edge that is already taken is dropped with a warning. Reloads live. See [Sidebars](/remux/sidebars/).

| Option | Type | Default | Meaning |
|---|---|---|---|
| `edge` | `"left"` \| `"right"` \| `"bottom"` | none (required) | Edge to dock to. An unknown value is a parse error. |
| `size` | integer | none (required) | Starting thickness: columns for `left`/`right`, rows for `bottom`. The frame is drawn inside it. |
| `visible` | bool | `true` | Whether it starts open. |

Once you toggle or resize a sidebar, its size, visibility and panel weights are saved to `$XDG_STATE_HOME/remux/sidebar.json` and take precedence over `size` and `visible` on later starts.

### `[[sidebar.panel]]`

Panels stack inside their sidebar in the order written.

| Option | Type | Default | Meaning |
|---|---|---|---|
| `plugin` | string | none (required) | `sessions`, `files`, `agents` or `placeholder`. An unknown name is skipped with a warning. `browser` is accepted as an old name for `files`. |
| `weight` | integer | `1` | This panel's share of the sidebar relative to its siblings. |
| `editor` | string | unset | `files` only: the editor to open a file with. Unset uses the server's `$EDITOR`, then `vi`. |
| `command` | string | unset | **Deprecated and ignored**, with a warning. Use `editor`. |

```toml
[[sidebar]]
edge = "left"
size = 30

  [[sidebar.panel]]
  plugin = "sessions"

  [[sidebar.panel]]
  plugin = "files"
```

## `[agents]`

What the `agents` sidebar panel counts as an AI agent, and how it tells a blocked one. Server-side, needs `remux restart`. See [Agents](/remux/agents/).

| Option | Type | Default | Meaning |
|---|---|---|---|
| `commands` | array of strings | `["claude", "codex", "aider", "gemini", "omp", "opencode"]` | Foreground commands that count as an agent. Name the agent itself, not a launcher such as `node`. |
| `working_ms` | integer | `500` | A pane that got output within this many milliseconds shows as working. |
| `scan_rows` | integer | `24` | How many lines up from the bottom of the live screen the patterns are matched against. |

### `[[agents.pattern]]`

A regex that, when it matches near the bottom of an agent's screen, marks the agent as needing input.

| Option | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | none (required) | Label that shows up in the server log. |
| `command` | string | unset | Only apply to this agent. Unset applies to every agent. |
| `regex` | string | none (required) | A Rust regex. An invalid one is skipped with a warning. |

:::caution[Patterns replace, they don't merge]
If you declare any `[[agents.pattern]]`, it replaces the whole built-in set. Copy the defaults you want to keep.
:::

The built-in patterns:

| `name` | `command` | `regex` |
|---|---|---|
| `claude-proceed` | `claude` | `(?i)do you want to (proceed\|continue\|allow\|use this api key)` |
| `claude-select` | `claude` | `(?i)enter to select.*esc to cancel` |
| `codex-approve` | `codex` | `(?i)would you like to (run\|make\|grant\|send)` |
| `codex-decline` | `codex` | `(?i)no, and tell codex what to do differently` |
| `omp-allow` | `omp` | `(?i)allow tool:` |
| `opencode-permission` | `opencode` | `(?i)permission required` |
| `opencode-allow` | `opencode` | `(?i)allow once\s+allow always\s+reject` |

In TOML, double the backslashes inside a basic string (`"allow once\\s+allow always"`) or use a literal string (`'allow once\s+allow always'`).

## Related

- [Configuration](/remux/configuration/): file location and reload behaviour
- [Keybindings](/remux/keybindings/): the default keymap and every bindable command
- [Theme](/remux/theme/): what each colour role paints
