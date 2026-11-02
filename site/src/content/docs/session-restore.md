---
title: Resuming agents after a restart
description: Bring each pane's AI coding agent back to its conversation when the server restores your sessions.
---

A restored pane normally comes back as a fresh shell in the directory it was in (see [Persistence and restore](/remux/sessions/#persistence-and-restore)). With agent session restore, a pane that was running an AI coding agent comes back with that agent reopened on the same conversation.

It works with **Claude Code**, **Codex**, **pi**, **omp** and **OpenCode**. Each agent's hook or plugin runs `remux agent session <agent>` when a session starts, and the server keeps the session beside the pane's working directory in `state.json`. When the server restores sessions, after a `remux restart` for example, it types the agent's resume command into the pane's new shell, with a leading space so that a shell set to ignore such lines leaves it out of the history.

## Install

Install the integration for each agent you use, on the machine the agent runs on, as the user it runs as:

```sh
remux integrate claude-resume     # SessionStart hook in $CLAUDE_CONFIG_DIR/settings.json (~/.claude)
remux integrate codex-resume      # SessionStart hook in $CODEX_HOME/hooks.json (~/.codex), and
                                  #   [features] hooks = true in its config.toml
remux integrate pi-resume         # extension in $PI_CODING_AGENT_DIR/extensions (~/.pi/agent)
remux integrate omp-resume        # extension in ~/$PI_CONFIG_DIR/agent/extensions (~/.omp/agent)
remux integrate opencode-resume   # server and TUI plugins in plugins/, the TUI one also in
                                  #   tui.json(c), in $OPENCODE_CONFIG_DIR (~/.config/opencode)
```

Each takes the same flags as `claude-messaging`:

| Flag | Does |
|---|---|
| `--status` | Reports whether the integration is installed, stale or absent |
| `--dry-run` | Prints what an install would change, and changes nothing |
| `--uninstall` | Removes remux's hook or plugin, and nothing of yours |

Each install refuses an agent config directory that does not exist yet: run the agent once first.

- `claude-resume` and `claude-messaging` share one `settings.json`.
- Uninstalling `codex-resume` leaves `hooks = true` in place, since other hooks may rely on it.
- pi and omp both honour `$PI_CODING_AGENT_DIR`, so with it set they resolve to one directory and both installs are refused.

### Codex: trust the hook, then send a prompt

Codex asks whether to trust a new or changed hook the first time it starts after the install ("Hooks need review"). Pick **Trust all and continue**, or the hook does not run and the session is not reported.

Codex runs that hook when the first prompt is sent, not when it starts, so a Codex pane is remembered once you have sent it something.

### OpenCode 1.x and 2.x

One install serves both, so upgrading OpenCode needs no reinstall. 2.x finds the TUI plugin's directory by itself, and 1.x reads the `tui.json(c)` entry.

- **Under 2.x only the full TUI reports.** 2.x runs server plugins in one background service that every TUI on the machine shares, so an `opencode mini` or `opencode run` session is not resumed, and neither is a TUI started with `--server`, whose session is on another machine.
- **Under 1.x**, `opencode --mini` and `opencode run` report as well.
- 2.x copies `tui.json` into a new `cli.json` the first time it starts, so `--uninstall` also takes Remux's entry out of `cli.json`.

## What gets typed

| Agent | Typed after a restart |
|---|---|
| claude | `claude --resume=<id>` |
| codex | `codex resume -- <id>` |
| pi | `pi --session <file>` |
| omp | `omp --resume=<file>` |
| opencode | `opencode --session=<id>` |

The value is joined to its flag, or put after `--`, so that a stored value can never be read as another option of the agent. pi's parser does not take a joined value, and its value is always an absolute path, so it is passed as a separate word.

## Settings

Two settings in `[general]`:

```toml
[general]
resume_agents = true          # default: true
resume_agent_delay_ms = 100   # default: 100
```

- `resume_agents = false` turns it all off: reports are refused and nothing is stored or typed.
- `resume_agent_delay_ms` spaces the typed commands out, so several agents do not all start at once.

Both are read by the server, so they need `remux restart`. See the [config reference](/remux/config-reference/#general).

## How it behaves

- **Only the agent in the pane.** A report typed at the prompt is refused, and so is a nested run: a `claude -p` or a subagent inside the pane's Claude, a `codex exec` inside its Codex, an omp started from omp's own shell tool. Another agent's session in a pane is never replaced while it runs.
- **Quitting forgets it.** The moment the pane's shell takes the foreground back, the session is forgotten, so a pane where you quit the agent comes back as a plain shell. Suspending the agent with `^Z` hands the foreground back too, and forgets it as well.
- **A slow `.zshrc` forgets it until the agent reports again.** zsh runs `.zshrc` with job control on, so any external command there (`starship init`, `zoxide init`, a `sleep`) takes the foreground and gives it back before the typed resume line runs, which forgets the session on every restore. The resumed agent reports it again as soon as it starts, so this only matters if the server is restarted again before that. bash, including `/bin/sh` on most Linux systems, runs its rc without job control and is not affected.
- **A resume that did not take is not retried.** After typing a resume, Remux waits for the agent to take the pane's foreground, or to report the session again, which an agent that resumed does as it starts. A session typed and never confirmed either way (the agent is gone, or something else runs in its place) is not typed on the next restore. Only the record's own agent confirms it: the shell, or another agent, does not.
- **Missing directories.** A pane whose saved directory is gone comes back as a plain shell rather than resuming in your home directory.
- **One resume per session.** Two panes holding the same session resume it once, in the lower-numbered pane.

## Privacy

- **Ids stay out of the logs.** `server.log` and `client.log` name the agent, the pane and the length of the session id or path, never the value.
- **`state.json` is yours alone.** Because it holds session ids, which name transcripts on disk, the server writes it `0600` whatever your umask, and closes up a file an older build left readable by others.

## Upgrading

This needs protocol 22. Rebuild Remux on every machine and run `remux restart` on each.

## Verify it works

1. Install the integration, for example `remux integrate claude-resume`, and check it with `--status`.
2. In a Remux pane, start the agent and send it one message.
3. Check that the server stored the session:

   ```sh
   grep 'agent session' ~/.local/state/remux/server.log
   ```

   The line for that pane ends in `-> Stored`. It names the agent and the length of the id, not the id itself. The log lives under `$XDG_STATE_HOME/remux/` if you set it.
4. Run `remux restart` and attach again. The pane's shell types the resume command and the agent reopens on the conversation.

If nothing was stored, check the [behaviour notes](#how-it-behaves) above; a refused report shows its refusal code in place of `Stored`. For Codex, make sure you trusted the hook and sent a prompt.

## Related

- [Sessions & persistence](/remux/sessions/): what a restore brings back
- [Agents](/remux/agents/): agent detection and states
- [Agent messaging](/remux/messaging/)
- [CLI reference](/remux/cli/#remux-integrate)
- [Config reference](/remux/config-reference/#general)
- [Troubleshooting](/remux/troubleshooting/#logs)
