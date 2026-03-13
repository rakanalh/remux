//! Config file watching for live-reload.
//!
//! This module watches `~/.config/remux/config.toml` for changes and sends
//! the reloaded configuration through a channel when the file is modified.

use std::path::PathBuf;

use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::Config;

/// Return the path to the config file, if determinable.
pub fn config_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("remux").join("config.toml"))
}

/// Watch the config file for changes, reloading and sending new Config values
/// through the provided channel.
///
/// This function spawns a blocking file watcher on the current thread. It is
/// intended to be called from a dedicated thread or a `tokio::task::spawn_blocking`
/// context.
///
/// The watcher monitors the config directory (not just the file) because many
/// editors perform atomic saves by writing a temp file and renaming it, which
/// can appear as a delete+create rather than a modify.
///
/// Returns `Ok(())` when the watcher is set up. The actual watching happens
/// asynchronously via the notify crate's event loop.
pub fn watch_config(tx: tokio::sync::mpsc::UnboundedSender<Config>) -> Result<WatchHandle> {
    let config_path =
        config_file_path().ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;

    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?
        .to_path_buf();

    // Create the config directory if it doesn't exist, so we can watch it.
    std::fs::create_dir_all(&config_dir)?;

    let watched_path = config_path.clone();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let event = match res {
            Ok(e) => e,
            Err(_) => return,
        };

        // We care about Create and Modify events on the config file.
        let dominated = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
        if !dominated {
            return;
        }

        // Check that the event is for our specific file.
        let is_our_file = event.paths.iter().any(|p| p == &watched_path);
        if !is_our_file {
            return;
        }

        // Attempt to reload.
        match Config::load() {
            Ok(config) => {
                let _ = tx.send(config);
            }
            Err(e) => {
                log::warn!("failed to reload config: {}", e);
            }
        }
    })?;

    watcher.watch(&config_dir, RecursiveMode::NonRecursive)?;

    Ok(WatchHandle { _watcher: watcher })
}

/// Handle that keeps the file watcher alive. The watcher stops when this
/// handle is dropped.
pub struct WatchHandle {
    _watcher: RecommendedWatcher,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_file_path_is_deterministic() {
        // On most systems, this should return Some.
        let path = config_file_path();
        if let Some(p) = path {
            assert!(p.ends_with("remux/config.toml") || p.ends_with("remux\\config.toml"));
        }
    }
}
