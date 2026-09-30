---
title: Configuration
description: Where config.toml lives, how it is loaded, and what needs a restart.
---

Remux reads one TOML file. Everything in it is optional: with no file at all you get the built-in defaults, and a file only needs the keys you want to change.

## Where the file lives

| Platform | Path |
|---|---|
| Any, with `$XDG_CONFIG_HOME` set | `$XDG_CONFIG_HOME/remux/config.toml` |
| Linux | `~/.config/remux/config.toml` |
| macOS | `~/Library/Application Support/remux/config.toml` |

`$XDG_CONFIG_HOME` wins on every platform, macOS included, but only when it is an **absolute** path. An empty or relative value is ignored and the platform default is used.

:::note[macOS]
With `$XDG_CONFIG_HOME` unset, a Mac keeps the config under `~/Library/Application Support`, not `~/.config`. If you want `~/.config/remux/config.toml`, export `XDG_CONFIG_HOME=$HOME/.config`.
:::

## Start from the sample

The repository ships `config.sample.toml`, which lists every option commented out at its default value. Copy it into place and uncomment what you want to change:

```shell
mkdir -p ~/.config/remux
cp config.sample.toml ~/.config/remux/config.toml
```

A minimal config is usually a handful of lines:

```toml
[general]
scrollback_lines = 50000

[appearance]
default_layout = "master"
which_key_position = "centered"

[keybindings.command]
"Alt-n" = "PaneSplitVertical"
```

In TOML, a table's own keys must come before any of its sub-tables. Put `leader` and the `"Alt-…"` shortcuts under `[keybindings.command]` before any `[keybindings.command.p]`-style group.

## Client settings and server settings

Remux is two processes. The **server** owns the sessions, panes and PTYs, and it draws the pane frames, tab strips and status bar. The **client** is the program in your terminal. It draws the overlays, the which-key popup, sidebars and Views, and it handles every key you press. Both read the same `config.toml`, but each one only uses the settings it owns, and the two reload differently.

**The client reloads live.** It watches the config file, so saving the file applies these settings straight away:

- `[keybindings.command]` (the leader, the command tree and the Alt shortcuts) and `[keybindings.session_manager]`
- `appearance.which_key_position`
- `appearance.pane_title` and `appearance.theme.tab_style`, which the client sends to every server it views
- `[appearance.theme]` colours for what the client draws: the which-key popup, the session manager and other overlays, sidebars, View cells and their status bar, and search highlights
- `[remotes.*]`
- `[[sidebar]]`

**The server reads the file once, at startup.** Run `remux restart` after changing any of these:

- everything under `[general]`
- `appearance.border_style`, `default_layout`, `popup_width_pct`, `popup_height_pct`
- `[layouts]`
- `[agents]`
- `[appearance.theme]` colours for what the server draws: pane frames, tab chips and the status bar

`remux restart` stops the server (saving sessions first) and starts a new one. Saved sessions come back when it starts, as long as `save_sessions` is on. See [Sessions & persistence](/remux/sessions/).

:::caution[A reload rebuilds the sidebars]
On every config reload the client rebuilds its sidebars from scratch. The sessions panel loses which nodes you had expanded and what you had selected. Sidebar sizes and visibility survive: what you just typed in the config wins, and anything you left alone keeps the size you dragged it to.
:::

### Remote servers

A server reads the config **on its own machine**. When you work in a session on a remote over SSH, all the server-side settings (`[general]` including `allow_app_clipboard`, `[agents]`, frame and status-bar colours, `default_layout`) come from the remote's `config.toml`. Your client's own keybindings, overlays, `pane_title` and `tab_style` still apply to it. See [Remotes over SSH](/remux/remotes/).

## When the file is wrong

- **Invalid TOML at startup**: Remux fails to start. Fix the file and try again.
- **Invalid TOML on a live reload**: the client logs a warning and keeps the config it already had.
- **Unknown keys** are ignored silently. A misspelled option does nothing and produces no warning, so if a setting seems to have no effect, check its spelling against the [config reference](/remux/config-reference/).
- **Unknown action names in keybindings** are reported as errors in the client log, naming the binding and the bad action. The key does nothing.

Logs live under `$XDG_STATE_HOME/remux/` (by default `~/.local/state/remux/`): `client.log` for the client and `server.log` for the server.

## Related

- [Config reference](/remux/config-reference/): every option, its type and default
- [Keybindings](/remux/keybindings/): the leader, the command tree and the Alt shortcuts
- [Theme](/remux/theme/): colours and tab styles
- [CLI reference](/remux/cli/): `remux restart` and friends
