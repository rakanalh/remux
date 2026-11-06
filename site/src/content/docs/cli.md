---
title: CLI reference
description: Every remux subcommand and flag.
---

```
remux [COMMAND]
```

With no command, `remux` attaches you to a session. The other commands manage sessions and the server from a shell, `split` and `new-tab` open panes from a script running inside Remux, `msg` and `agent` are for [agent messaging](/remux/messaging/), and `integrate` installs a coding agent's hooks for [messaging](/remux/messaging/) or for [resuming it after a restart](/remux/session-restore/).

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
| [`remux msg`](#remux-msg) | Leave messages for other panes and read your own |
| [`remux agent`](#remux-agent) | Name panes and list the ones that can be sent to |
| [`remux integrate`](#remux-integrate) | Install, check or remove a coding agent's hooks for messaging or session restore |

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

Kills a session and every pane in it, then prints `Killed session '<NAME>'.` and exits `0`. A client attached to that session moves to another session, or exits if no session is left.

It never starts a server. Errors go to stderr.

| Exit code | Meaning |
|---|---|
| `0` | The session is gone. |
| `1` | The session does not exist, no server is running, the connection failed, or the server did not answer within 10 seconds. |
| `2` | The command line is wrong, for example `<NAME>` is missing. |

After a timeout or a lost connection the kill may still have happened. Run `remux ls` to check.

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
A restarted server re-creates each saved pane with a fresh shell in the directory the pane was in. Programs that were running in the panes are not carried over, except an AI coding agent whose resume integration is installed, which is reopened on its conversation. See [Resuming agents after a restart](/remux/session-restore/).
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

- The session is the one the pane is in **now**. The command sends `$REMUX_PANE`, and the server looks up which session holds that pane, so a pane moved to another session, or a session renamed since the pane started, still gets the right one. When that pane no longer exists, the session named by `$REMUX_SESSION` is used instead.
- If `$REMUX_SESSION` is unset, the command refuses with `not inside a remux pane` and exits non-zero, even when `$REMUX_PANE` is set. It never guesses a session.
- The pane that gets split is that session's **currently focused** pane in its active tab, not necessarily the pane the command ran in. `$REMUX_PANE` picks the session, never the spot.
- If no server is running on this machine, the command exits non-zero. It does not start a server.
- On success it prints nothing and exits 0. Errors go to stderr with a non-zero exit, so scripts can check `$?`.

## `remux msg`

```shell
remux msg send [OPTIONS] [TARGET] <TEXT | --stdin | --file <FILE>>
remux msg read [OPTIONS]
remux msg ack [--pane <NAME>] <IDS... | --all>
```

Leave a message in a pane's mailbox, read your own, or mark messages read. Run them from inside a pane: a send's sender is the pane the command runs in. See [Agent messaging](/remux/messaging/).

`<TARGET>` is an agent's name, or `host:name` for an agent on a [remote](/remux/remotes/). Leave it out with `--reply-to` to answer that message's sender.

| `msg send` flag | Meaning |
|---|---|
| `--stdin` | Read the message from standard input, which must be a pipe or a file |
| `-f`, `--file <FILE>` | Read the message from a regular file |
| `--deliver <MODE>` | `auto` (default), `submit` or `mailbox`. See [Delivery](/remux/messaging/#delivery). |
| `--defer` | While the agent is waiting on its user, keep the message queued until it is not |
| `--wait` | Once the message is typed, wait for the agent's turn to end and say how it ended |
| `--timeout <DUR>` | How long `--defer` and `--wait` may take together: `30`, `30s`, `2m`, at most `1h`. Default: `10m`. |
| `--reply-to <ID>` | The number of the message this one answers, as `msg read` shows it. With no `<TARGET>`, the reply goes to that message's sender. With a `<TARGET>` that sent you that message, it is answered the same way; any other id is sent as given. |
| `--json` | Print the answer as JSON |

| `msg read` flag | Meaning |
|---|---|
| `--pane <NAME>` | Read another agent's mailbox, by its name. This marks nothing read. |
| `--all` | Include messages already read |
| `--peek` | Do not mark anything read |
| `--wait <DUR>` | Wait this long for mail when there is none. Exits 0 with no output if nothing arrives. |
| `--format <FORMAT>` | `text` (default) or `json`. `context` is accepted and is the same as `text`. |

`msg ack` takes message ids, or `--all` for every unread message, and `--pane <NAME>` for another agent's mailbox.

`remux msg hook` is run by an agent's hooks, never by hand. [`remux integrate`](#remux-integrate) installs it.

## `remux agent`

```shell
remux agent ls [--json]
remux agent status [TARGET] [--json]
remux agent name <NAME | --clear> [--pane <NAME>]
remux agent wait <TARGET> --state <idle|working|blocked> [--timeout <DUR>] [--json]
remux agent session <AGENT> <--hook | --id <ID> | --path <PATH>> [--source <SOURCE>] [--pane <NAME>]
```

| Command | Does |
|---|---|
| `ls` | List every agent, named pane and pane with mail. With `--json`, `tab_index` counts from 0; the table's `TAB` counts from 1. |
| `status` | Show one pane, or every pane worth listing |
| `name` | Give this pane the name other panes send to (`[a-z][a-z0-9_-]{0,31}`), or remove it with `--clear`. `--pane <NAME>` names another agent, and only works from outside every pane. An agent with no name of its own is listed under the one remux gave it after its session. |
| `wait` | Wait until a pane's agent is idle, working or blocked (waiting on its user). Exits 0 when it is, and also when the time runs out, saying so. `--timeout` defaults to `10m`. |
| `session` | Run by an agent's hook or plugin, never by hand: a report typed at the prompt is refused. Reports the session the pane's agent is running, so that a restore resumes it. See below. |

`remux agent session` is what the `<agent>-resume` integrations install. `<AGENT>` is `claude`, `codex`, `opencode`, `pi` or `omp`. It always exits 0 and prints nothing on stdout, and outside a pane it does nothing at all.

| `agent session` flag | Meaning |
|---|---|
| `--hook` | Read the agent's hook payload from standard input |
| `--id <ID>` | The session's id |
| `--path <PATH>` | The session file's absolute path |
| `--source <SOURCE>` | How the session started: `startup`, `resume`, `clear`, `compact`, `fork`, `new` or `select`. Read from the payload with `--hook`. |
| `--pane <NAME>` | Another pane, by its name. Only from outside every pane. |

See [Resuming agents after a restart](/remux/session-restore/).

## `remux integrate`

```shell
remux integrate [OPTIONS] <HARNESS>
```

Install, check or remove what a coding agent needs in its own files to use Remux. Run it on the machine the agent runs on, as the user it runs as. With no flag, it installs.

| `<HARNESS>` | Installs |
|---|---|
| `claude-messaging` | Claude Code's `Stop` and `UserPromptSubmit` hooks in `$CLAUDE_CONFIG_DIR/settings.json` (`~/.claude` by default), and the remux-messaging skill. See [Claude Code hooks](/remux/messaging/#claude-code-hooks). |
| `claude-resume` | Claude Code's `SessionStart` hook that reports the session for restore after `remux restart`, in the same `settings.json` |
| `codex-resume` | Codex's `SessionStart` hook in `$CODEX_HOME/hooks.json` (`~/.codex` by default), and `[features] hooks = true` in its `config.toml` |
| `pi-resume` | pi's `remux-agent-session.ts` extension in `$PI_CODING_AGENT_DIR` (`~/.pi/agent` by default) |
| `omp-resume` | omp's `remux-agent-session.ts` extension in `~/$PI_CONFIG_DIR/agent` (`~/.omp/agent` by default) |
| `opencode-resume` | OpenCode 1.x and 2.x: a server plugin and a TUI plugin under `$OPENCODE_CONFIG_DIR/plugins/`, the TUI one also listed in its `tui.json(c)` for 1.x (`~/.config/opencode` by default) |

The `-resume` targets refuse an agent config directory that does not exist yet: run the agent once first. See [Resuming agents after a restart](/remux/session-restore/).

| Flag | Meaning |
|---|---|
| `--status` | Report whether the integration is installed, stale or absent |
| `--dry-run` | Print what an install would change, and change nothing |
| `--uninstall` | Remove remux's hooks, and nothing of yours |
| `--allow-dev-path` | Write hooks that run this binary even though it is a cargo build output (under a `target/` directory). Only for running a Remux built from source under `target/`; an installed binary never needs it. |

Every target takes these flags.

## Environment variables

Remux sets these in every pane:

| Variable | Value |
|---|---|
| `REMUX` | `1`. Marks the shell as running inside Remux. |
| `REMUX_SESSION` | Name of the session the pane was created in. Read by `split` and `new-tab` as the fallback, and its presence is what marks a shell as inside a pane. |
| `REMUX_PANE` | ID of the pane. Read by `split` and `new-tab` to find the session the pane is in now. |
| `TERM` | `xterm-256color` |

These are set when the pane starts and never updated, so `$REMUX_SESSION` keeps the old name after you rename the session or move the pane to another session. `split` and `new-tab` still find the right session through `$REMUX_PANE`. The one case left is a process that outlives its pane, such as a `nohup`'d job, after its session was renamed or closed: with the pane gone and the name matching no session, the command fails with an error naming both.

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
