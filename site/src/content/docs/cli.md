---
title: CLI reference
description: Every remux subcommand and flag.
---

```
remux [COMMAND]
```

With no command, `remux` attaches you to a session. The other commands manage sessions and the server from a shell, and `split` and `new-tab` open panes from a script running inside Remux.

| Command | Does |
|---|---|
| [`remux`](#remux) | Start the server if needed and attach |
| [`remux new`](#remux-new) | Create a session and attach to it |
| [`remux attach`](#remux-attach) | Attach to an existing session |
| [`remux attach-remote`](#remux-attach-remote) | Attach to a session on another machine over SSH |
| [`remux ls`](#remux-ls) | List sessions |
| [`remux kill`](#remux-kill) | Kill a session |
| [`remux stop`](#remux-stop) | Stop the server, saving sessions first |
| [`remux restart`](#remux-restart) | Stop the server and start a new one |
| [`remux split`](#remux-split) | Split the focused pane (from inside a pane) |
| [`remux new-tab`](#remux-new-tab) | Open a new tab (from inside a pane) |

Every command takes `-h`/`--help`. `remux -V` (or `--version`) prints the version.

## `remux`

```shell
remux
```

Starts the server in the background if none is running, then attaches to the first session the server lists. If there are no sessions, it creates one called `main`.

## `remux new`

```shell
remux new -s <SESSION> [-f <FOLDER>]
```

Creates a session and attaches to it.

| Flag | Meaning |
|---|---|
| `-s`, `--session <SESSION>` | Name of the new session. Required. |
| `-f`, `--folder <FOLDER>` | Put the session in this folder of the session tree. The folder is created if it does not exist. |

## `remux attach`

```shell
remux attach <NAME>
```

Attaches to an existing session by name. Starts the server first if it is not running.

## `remux attach-remote`

```shell
remux attach-remote [--remux-path <PATH>] <DEST> <NAME>
```

Attaches straight to a session on another machine. It runs `ssh <DEST> <PATH> relay`, which starts Remux's server on the remote if it is not already running there.

| Argument | Meaning |
|---|---|
| `<DEST>` | SSH destination, e.g. `user@host` or a `~/.ssh/config` host alias |
| `<NAME>` | Session on the remote to attach to |
| `--remux-path <PATH>` | Path to `remux` on the remote. Default: `remux`. |

For remotes you use regularly, `[remotes.<name>]` in the config is the better route: they appear in the session tree next to your local sessions. See [Remotes over SSH](/remux/remotes/).

## `remux ls`

```shell
remux ls
```

Prints a table of sessions: `NAME`, `FOLDER`, `TABS` and `CLIENTS` (how many clients are attached). If no server is running it says so and does not start one.

## `remux kill`

```shell
remux kill <NAME>
```

Kills a session and every pane in it. If no server is running it says so and does not start one.

## `remux stop`

```shell
remux stop
```

Sends the server `SIGTERM`. The server saves session state (if `save_sessions` is on) and exits. `stop` waits up to 5 seconds for the server to go and warns if it is still running after that.

## `remux restart`

```shell
remux restart
```

Stops the server as `stop` does, then starts a new one. Saved sessions are restored when it starts. Run this after changing any server-side setting. See [Configuration](/remux/configuration/#client-settings-and-server-settings).

:::caution
A restarted server re-creates each saved pane with a fresh shell in the directory the pane was in. Programs that were running in the panes are not carried over.
:::

## `remux split`

```shell
remux split [--right | --below] [-c <DIR>] [-- COMMAND...]
```

Splits the focused pane of the current session and runs `COMMAND` in the new pane. With no command, the new pane runs your login shell.

| Flag | Meaning |
|---|---|
| `--below` | Put the new pane below the focused one. The default. |
| `--right` | Put the new pane to the right of the focused one. Cannot be combined with `--below`. |
| `-c`, `--cwd <DIR>` | Working directory for the new pane. Default: the focused pane's. |
| `-- COMMAND...` | Program and arguments to run. Put them after `--` so flags meant for the program are not read as flags for `remux`. |

```shell
remux split
remux split --right -- nvim /tmp/notes.md
remux split -c ~/src/remux -- cargo watch -x test
```

## `remux new-tab`

```shell
remux new-tab [-c <DIR>] [-- COMMAND...]
```

Opens a new tab in the current session and runs `COMMAND` in its pane, or your login shell if there is no command.

| Flag | Meaning |
|---|---|
| `-c`, `--cwd <DIR>` | Working directory for the tab's pane. Default: the focused pane's. |
| `-- COMMAND...` | Program and arguments to run |

The new tab starts in `appearance.default_layout`.

### How `split` and `new-tab` pick their target

Both commands are meant to be run **from inside a Remux pane**, by you or by a script, an editor or a file manager's opener hook.

- The session comes from `$REMUX_SESSION`, which every pane gets. If it is unset, the command refuses with `not inside a remux pane` and exits non-zero. It never guesses a session.
- The pane that gets split is that session's **currently focused** pane in its active tab, not necessarily the pane the command ran in.
- If no server is running on this machine, the command exits non-zero. It does not start a server.
- On success it prints nothing and exits 0. Errors go to stderr with a non-zero exit, so scripts can check `$?`.

## Environment variables

Remux sets these in every pane:

| Variable | Value |
|---|---|
| `REMUX` | `1`. Marks the shell as running inside Remux. |
| `REMUX_SESSION` | Name of the session the pane was created in. Read by `split` and `new-tab`. |
| `REMUX_PANE` | ID of the pane |
| `TERM` | `xterm-256color` |

These are set when the pane starts and never updated. If you rename the session later, `$REMUX_SESSION` in panes that already exist still has the old name.

Remux reads:

| Variable | Effect |
|---|---|
| `REMUX_ALLOW_NESTED` | When set, lets `remux`, `remux new`, `remux attach` and `remux attach-remote` run inside a Remux pane. Without it they refuse, because `$REMUX` is set. |
| `XDG_CONFIG_HOME` | Where `remux/config.toml` is read from. |
| `XDG_STATE_HOME` | Where `remux/client.log`, `server.log`, `relay.log` and `sidebar.json` are written. |
| `XDG_DATA_HOME` | Where saved sessions (`remux/state.json`) live. |
| `XDG_RUNTIME_DIR` | Where the server's socket (`remux.sock`) and pid file live. |
| `SHELL` | The shell new panes run, unless `general.default_shell` is set. |
| `EDITOR` | The editor `BufferEditInEditor` opens scrollback in (the client's `$EDITOR`, then `$VISUAL`, then `vi`), and the editor the `files` sidebar panel opens files with (the server's `$EDITOR`, then `vi`). |

## Internal commands

`remux server` runs the server in the foreground and `remux relay` carries a client's connection to the server over SSH. Remux starts them itself, so you should not need to run either one. They do not appear in `--help`.

## Related

- [Configuration](/remux/configuration/): what `remux restart` is for
- [Sessions & persistence](/remux/sessions/): saving and restoring sessions
- [Remotes over SSH](/remux/remotes/): remotes in the session tree
- [Troubleshooting](/remux/troubleshooting/)
