---
title: Troubleshooting
description: Logs, version skew, stale servers and other common problems.
---

## Logs

Remux writes three log files, one per kind of process:

| File | Written by |
|---|---|
| `server.log` | The background server |
| `client.log` | The `remux` client you run in your terminal, and CLI commands such as `remux ls` |
| `relay.log` | The relay a client starts on a remote over SSH (so it is on the **remote** machine) |

They live in `$XDG_STATE_HOME/remux/`. With `XDG_STATE_HOME` unset, that is `~/.local/state/remux/` on both Linux and macOS.

Debug logging is always on; there is no `RUST_LOG` or verbosity switch to set. Panics are written to the log too, so a crash leaves a `panicked at …` line behind. The files are never rotated, so delete them yourself if they grow large.

```sh
tail -f ~/.local/state/remux/server.log
```

## Where files live

Every location honours its XDG variable first, when it is set to an absolute path.

| What | Linux default | macOS default |
|---|---|---|
| Config | `~/.config/remux/config.toml` | `~/Library/Application Support/remux/config.toml` |
| Saved sessions | `~/.local/share/remux/state.json` | `~/Library/Application Support/remux/state.json` |
| Logs and `sidebar.json` | `~/.local/state/remux/` | `~/.local/state/remux/` |
| Socket | `$XDG_RUNTIME_DIR/remux.sock` | `$XDG_RUNTIME_DIR/remux.sock` |

The variables are `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR` respectively. If no runtime directory is known, the socket falls back to `/tmp/remux.sock`. The server's pid file, `remux.pid`, sits next to the socket.

:::note
On a Mac the config is **not** read from `~/.config` unless you export `XDG_CONFIG_HOME=$HOME/.config`. If your edits seem to be ignored on macOS, check which path you are editing.
:::

## A config change did nothing

The client watches `config.toml` and applies its own settings when you save: keybindings, remotes, sidebars, `pane_title`, `tab_style`, `which_key_position` and the colours of what it draws. Settings the **server** reads, such as `[general]`, `[layouts]`, `[agents]`, `default_layout`, `border_style` and the colours of pane borders, tab strips and the status bar, are only picked up when it starts. [Configuration](/remux/configuration/#client-settings-and-server-settings) has the full list, and [Theme](/remux/theme/#when-changes-apply) the appearance settings.

```sh
remux restart
```

`remux restart` stops the server, saving your sessions first, and starts it again. Your sessions come back; [Views](/remux/views/) do not, because they live only in the server's memory.

If a saved edit still does nothing, the file may not parse. A config that fails to load on reload is ignored (the previous one stays in effect) and the reason is logged to `client.log` as `failed to reload config`. At startup, a config that does not parse stops `remux` with the parse error.


## A rebuilt binary still behaves the old way

Rebuilding or reinstalling Remux does not replace the server that is already running. Restart it:

```sh
remux restart
```

## "build mismatch: rebuild + restart"

A server's row in the session manager or the `sessions` panel shows `(build mismatch: rebuild + restart)` when that server was built from a different commit than your client. This can happen on the `local` row too, when you rebuilt Remux but did not restart the server. Remux cannot tell which side is behind, so:

1. Build or install the **same commit** on every machine.
2. Run `remux restart` on every machine whose server was running during the upgrade.

A `-dirty` suffix alone does not count as a mismatch.

## "incompatible remux: remote protocol vN, local vM"

The client refuses to connect to a server from an incompatible version of Remux, and aborts with this error. The fix is the same as above: install the same version everywhere and restart the servers. See [Remotes](/remux/remotes/) for connecting to other machines.

## The status bar will not move to the top

`status_bar_position = "top"` is accepted but currently has no effect: the status bar is always on the bottom row.

## The files panel does not follow `cd`

This is expected. The [files panel](/remux/sidebars/#files) follows the directory of the focused pane and updates when **focus** moves, not when you `cd` inside a pane. Move focus away and back, or navigate the panel with `h`/`l`.

## The agents list is empty

- Check that the agent's command is in `[agents] commands`. List the agent itself (`claude`, `codex`), never its launcher (`node`, `bun`).
- If you edited `[agents]`, run `remux restart`.
- `<server>: no detection` means that server's platform cannot detect agents. Linux and macOS are supported.
- A pane stuck on "needs input", or one that never turns red: `server.log` says why each pane was classified as it was. Adjust the pattern and restart. See [Agents](/remux/agents/#blocked-prompt-patterns).

## Starting Remux inside a Remux pane

Remux refuses to start a client inside one of its own panes. Detach first or use another terminal, or set `REMUX_ALLOW_NESTED=1` if you really want a nested client.

## Related

- [Configuration](/remux/configuration/)
- [CLI](/remux/cli/): `remux stop`, `remux restart` and the other subcommands.
- [Remotes](/remux/remotes/)
