---
title: Install
description: Build and install Remux from source with cargo.
---

Remux is distributed as source. You build it with cargo, either straight from the Git repository or from a local checkout. There are no prebuilt binaries or distribution packages.

## Requirements

- A Unix-like system. Remux runs every pane in a POSIX pseudo-terminal, so it does not run on Windows. Linux is the primary platform.
- A stable Rust toolchain with `cargo`. The easiest way to get one is [rustup](https://rustup.rs/).
- `ssh` on your `PATH`, if you plan to use [remote servers](/remux/remotes/).

## Install with cargo

```sh
cargo install --git https://github.com/rakanalh/remux
```

This builds the `remux` binary and installs it into `~/.cargo/bin`. Make sure that directory is on your `PATH`.

## Build from source

```sh
git clone https://github.com/rakanalh/remux
cd remux
cargo build --release
```

The binary is at `target/release/remux`. Copy it somewhere on your `PATH`, or run it from there.

## Check the install

```sh
remux --version
```

It prints the crate version, for example `remux 0.1.0`.

## macOS: make Option work as Alt

A Mac keyboard has no Alt key. Remux's Alt shortcuts use the Option key (⌥) instead, but most macOS terminals send Option as a key for typing special characters (Option-b types `∫`), so those shortcuts never reach Remux. Set your terminal to send Option as Meta, an Esc prefix, which is what Remux reads as Alt:

| Terminal | Setting |
|---|---|
| Terminal.app | **Terminal → Settings → Profiles → Keyboard**, select your profile and turn on **Use Option as Meta key**. |
| iTerm2 | **Settings → Profiles → Keys → General**, set **Left Option key** (and **Right Option key** if you want both) to **Esc+** (the iTerm2 manual calls it “+Esc”). iTerm2 advises against **Meta**. |
| Ghostty | `macos-option-as-alt = true` in the config, or `left` / `right` for one Option key. U.S. layouts already default to `true`. |
| Alacritty | `option_as_alt = "Both"` (or `"OnlyLeft"` / `"OnlyRight"`) in the `[window]` table. The default is `"None"`. |
| WezTerm | The left Option key already acts as Alt. To use the right one too, set `config.send_composed_key_when_right_alt_is_pressed = false`. |
| Kitty | `macos_option_as_alt both` (or `left` / `right`) in `kitty.conf`. The default is `no`. Kitty applies it on restart, not on a config reload. |

Setting only the left Option key keeps the right one for typing accented and special characters.

<!-- os-keys: off -->

The docs show every key for your keyboard: use the **Linux / macOS** menu at the top of the page. On macOS, `Alt-h` is shown as ⌥h and `Alt-H` as ⌥⇧H. Your config file still spells the modifier `Alt-` on every platform.

<!-- os-keys: on -->

## macOS: make Cmd+K / Cmd+L reach Remux

Most macOS terminals handle Cmd+K themselves: they clear their own screen and do not tell the program running in them. Remux cannot detect that, so its view of the screen goes blank until something repaints it. Set your terminal to send Cmd+K and Cmd+L to Remux instead, as `ESC [107;9u` and `ESC [108;9u` (the kitty keyboard protocol's form of `k` and `l` with Super). Remux then runs:

| Key | Command | Does |
|---|---|---|
| Cmd+K | `PaneClearScrollback` | Clears the focused pane and drops its scrollback. At a shell prompt it also sends `Ctrl-l` so the shell redraws the prompt; a running command is sent nothing |
| Cmd+L | `PaneClear` | Sends `Ctrl-l` to the focused pane, so the shell clears the screen, and keeps the scrollback. A running command gets the `Ctrl-l` as input, as in a plain terminal |

| Terminal | Setting |
|---|---|
| Ghostty | `keybind = cmd+k=text:\x1b[107;9u` and `keybind = cmd+l=text:\x1b[108;9u` in the config. |
| WezTerm | Add to `config.keys`: `{ key = 'k', mods = 'SUPER', action = wezterm.action.SendString '\x1b[107;9u' }` and the same with `'l'` and `'\x1b[108;9u'`. |
| Alacritty | In the config: `[[keyboard.bindings]]` with `key = "K"`, `mods = "Command"`, `chars = "\u001b[107;9u"`, and a second binding with `key = "L"` and `chars = "\u001b[108;9u"`. |
| Kitty | `map cmd+k send_text all \x1b[107;9u` and `map cmd+l send_text all \x1b[108;9u` in `kitty.conf`. These replace kitty's own Cmd+K and Cmd+L clears. Use `send_text`, not `send_key`, which sends nothing for Cmd to a program in legacy keyboard mode. `kitten show-key` shows what the key sends. |
| iTerm2 | **Settings → Profiles → Keys → Key Mappings**, add Cmd+K with the action **Send Hex Codes** and `0x1b 0x5b 0x31 0x30 0x37 0x3b 0x39 0x75`, and Cmd+L with `0x1b 0x5b 0x31 0x30 0x38 0x3b 0x39 0x75`. |
| Terminal.app | Cannot send these keys. Use `Prefix p c` for `PaneClearScrollback`, and run `PaneClear` from the command palette (`Prefix :`) to keep the scrollback. A typed `Ctrl-l` drops the scrollback too. |

If the terminal has already cleared its screen, `Prefix r` (`Redraw`) repaints it from what Remux last drew.

The keys are ordinary shortcuts, `"Cmd-k"` and `"Cmd-l"` under `[keybindings.command]`, so you can rebind or unbind them. `Super-` is the same modifier as `Cmd-`.

## Add a config file

Remux works without a config file. To start customising, copy the fully commented sample from the repository. Every option in it is commented out at its default value, so copying it verbatim changes nothing until you uncomment a line:

```sh
mkdir -p ~/.config/remux
cp config.sample.toml ~/.config/remux/config.toml
```

:::note
That is the Linux location. On macOS the default is `~/Library/Application Support/remux/config.toml`, so copy the sample there instead. If `XDG_CONFIG_HOME` is set to an absolute path, Remux reads `$XDG_CONFIG_HOME/remux/config.toml` on either platform.
:::

See [Configuration](/remux/configuration/) for how the file is organised and which settings apply live.

## Upgrading

Remux runs a background server that keeps your sessions alive, so installing a new binary does not replace the server that is already running. After upgrading, restart it:

```sh
remux restart
```

Sessions are saved before the server stops and restored when it starts again (with the default `save_sessions = true` and `automatic_restore = true`).

:::caution
If you use [remotes](/remux/remotes/), build the **same commit** on every machine. The client and each server compare build stamps, and the session manager flags any server, local or remote, whose build differs with `(build mismatch: rebuild + restart)`.
:::

## Next steps

- [Quick start](/remux/quick-start/): open a session, split panes and detach.
- [macOS setup](#macos-make-option-work-as-alt): make the Option key work for the Alt shortcuts, and [Cmd+K / Cmd+L](#macos-make-cmdk--cmdl-reach-remux) reach Remux.
- [Concepts](/remux/concepts/): how servers, sessions, tabs and panes fit together.
