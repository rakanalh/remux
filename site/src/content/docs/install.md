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

## Add a config file

Remux works without a config file. To start customising, copy the fully commented sample from the repository. Every option in it is commented out at its default value, so copying it verbatim changes nothing until you uncomment a line:

```sh
mkdir -p ~/.config/remux
cp config.sample.toml ~/.config/remux/config.toml
```

:::note
On macOS the default config location is `~/Library/Application Support/remux/config.toml` rather than `~/.config/remux/`. If `XDG_CONFIG_HOME` is set to an absolute path, Remux reads `$XDG_CONFIG_HOME/remux/config.toml` on either platform.
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
- [Concepts](/remux/concepts/): how servers, sessions, tabs and panes fit together.
