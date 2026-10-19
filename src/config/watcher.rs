//! Config file watching for live-reload.
//!
//! This module watches `~/.config/remux/config.toml` for changes and sends
//! the reloaded configuration through a channel when the file is modified.
//!
//! The config path may be a symlink (a dotfiles checkout is the usual reason).
//! An editor then saves the TARGET, in a directory the link's own directory
//! watch never sees, so the target's directory is watched as well and an event
//! on either path reloads.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::Config;

/// Return the path to the config file, if determinable.
pub fn config_file_path() -> Option<PathBuf> {
    crate::paths::config_file()
}

/// `path` with its directory canonicalized and its final component left alone.
///
/// The leaf is not resolved on purpose: for a symlink that would turn the link
/// into its target, and the link and the target must stay distinguishable. The
/// directory is resolved because the watch backend may report either spelling
/// of it (on macOS, `/var/...` and `/private/var/...`).
fn keep_leaf(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(name),
        _ => path.to_path_buf(),
    }
}

/// Where `link` really points, when it is a symlink that resolves and ends up
/// somewhere else. `None` for a regular file, and for a dangling link, so both
/// behave as an unlinked config does.
fn resolve_target(link: &Path) -> Option<PathBuf> {
    let target = std::fs::canonicalize(link).ok()?;
    (keep_leaf(&target) != keep_leaf(link)).then_some(target)
}

/// Whether a watch event on `event_path` concerns the config: it names the
/// link path itself, or the canonical `target` the link resolves to.
///
/// Both sides are normalized with [`keep_leaf`] first, so a reporting spelling
/// that differs only in how the directory is written still matches.
fn is_config_path(event_path: &Path, link: &Path, target: Option<&Path>) -> bool {
    let event = keep_leaf(event_path);
    event == keep_leaf(link) || target.is_some_and(|t| event == keep_leaf(t))
}

/// The watcher and the directories it is watching.
///
/// Only the setup code and the re-resolve thread touch this. The event callback
/// never does: adding a watch restarts the backend's event stream on some
/// platforms and waits for the callback to return, so a callback that took this
/// lock while another thread held it inside `watch` would deadlock.
struct Watching {
    watcher: RecommendedWatcher,
    watched: HashSet<PathBuf>,
}

impl Watching {
    /// Watch `dir` unless it already is, keyed by its canonical form so two
    /// spellings of one directory do not produce duplicate events.
    fn watch_dir(&mut self, dir: &Path) {
        let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if self.watched.contains(&key) {
            return;
        }
        match self.watcher.watch(&key, RecursiveMode::NonRecursive) {
            Ok(()) => {
                self.watched.insert(key);
            }
            Err(e) => log::warn!("config watch: cannot watch {}: {e}", key.display()),
        }
    }
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
/// If the config path is a symlink, the directory of its target is watched too.
/// A link created or retargeted after startup is picked up when an event arrives
/// for the link path in its own directory, which re-resolves it and watches the
/// new target's directory. A dangling link at startup is not an error: it
/// behaves like a plain config path until the link starts to resolve.
///
/// Returns `Ok(())` when the watcher is set up. The actual watching happens
/// asynchronously via the notify crate's event loop.
pub fn watch_config(tx: tokio::sync::mpsc::UnboundedSender<Config>) -> Result<WatchHandle> {
    let config_path =
        config_file_path().ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;
    watch_path(config_path, Config::load, tx)
}

/// [`watch_config`] for an explicit path and reload function.
fn watch_path<T: Send + 'static>(
    config_path: PathBuf,
    reload: impl Fn() -> Result<T> + Send + 'static,
    tx: tokio::sync::mpsc::UnboundedSender<T>,
) -> Result<WatchHandle> {
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?
        .to_path_buf();

    // Create the config directory if it doesn't exist, so we can watch it.
    std::fs::create_dir_all(&config_dir)?;

    let target = Arc::new(Mutex::new(resolve_target(&config_path)));
    // The callback asks this thread to look at the link again; see `Watching`
    // for why the callback cannot do it itself.
    let (relink_tx, relink_rx) = std::sync::mpsc::channel::<()>();

    let cb_target = Arc::clone(&target);
    let watched_path = config_path.clone();
    let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let event = match res {
            Ok(e) => e,
            Err(_) => return,
        };

        // An event on the link itself may mean it was created or retargeted.
        let link_leaf = keep_leaf(&watched_path);
        if event.paths.iter().any(|p| keep_leaf(p) == link_leaf) {
            let _ = relink_tx.send(());
        }

        // We care about Create and Modify events on the config file.
        let dominated = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
        if !dominated {
            return;
        }

        // Check that the event is for our specific file: the link path, or
        // where the link points.
        let current = cb_target.lock().ok().and_then(|t| t.clone());
        let is_our_file = event
            .paths
            .iter()
            .any(|p| is_config_path(p, &watched_path, current.as_deref()));
        if !is_our_file {
            return;
        }

        // Attempt to reload.
        match reload() {
            Ok(config) => {
                let _ = tx.send(config);
            }
            Err(e) => {
                log::warn!("failed to reload config: {}", e);
            }
        }
    })?;

    let mut watching = Watching {
        watcher,
        watched: HashSet::new(),
    };
    watching.watch_dir(&config_dir);
    if let Some(dir) = target.lock().ok().and_then(|t| t.clone()) {
        if let Some(parent) = dir.parent() {
            watching.watch_dir(parent);
        }
    }
    if watching.watched.is_empty() {
        anyhow::bail!("cannot watch {}", config_dir.display());
    }
    let watching = Arc::new(Mutex::new(watching));

    // Holds only a weak reference, so dropping the handle drops the watcher,
    // which drops the callback and with it the sender, which ends this thread.
    let weak = Arc::downgrade(&watching);
    std::thread::spawn(move || {
        while relink_rx.recv().is_ok() {
            let now = resolve_target(&config_path);
            if let Ok(mut t) = target.lock() {
                t.clone_from(&now);
            }
            let (Some(dir), Some(watching)) =
                (now.as_deref().and_then(Path::parent), weak.upgrade())
            else {
                continue;
            };
            let mut guard = watching.lock();
            if let Ok(w) = guard.as_mut() {
                w.watch_dir(dir);
            }
        }
    });

    Ok(WatchHandle {
        _watching: watching,
    })
}

/// Handle that keeps the file watcher alive. The watcher stops when this
/// handle is dropped.
pub struct WatchHandle {
    _watching: Arc<Mutex<Watching>>,
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

    use std::time::{Duration, Instant};

    #[test]
    fn the_link_and_its_target_both_count_as_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles");
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(real.join("config.toml"), "").unwrap();
        let link = cfg.join("config.toml");
        std::os::unix::fs::symlink(real.join("config.toml"), &link).unwrap();
        let target = resolve_target(&link).expect("a live link resolves");

        assert!(is_config_path(&link, &link, Some(&target)));
        assert!(is_config_path(
            &real.join("config.toml"),
            &link,
            Some(&target)
        ));
        assert!(!is_config_path(
            &real.join("other.toml"),
            &link,
            Some(&target)
        ));
        assert!(!is_config_path(
            &cfg.join("other.toml"),
            &link,
            Some(&target)
        ));
        // Without a target only the link itself counts.
        assert!(!is_config_path(&real.join("config.toml"), &link, None));
        assert!(is_config_path(&link, &link, None));
    }

    #[test]
    fn paths_match_across_spellings_of_the_directory() {
        // On macOS the temp dir lives under `/var`, a symlink to `/private/var`;
        // the backend may report either.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("config.toml");
        let canonical = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("config.toml");
        assert!(is_config_path(&canonical, &link, None));
        assert!(is_config_path(&link, &canonical, None));
    }

    #[test]
    fn a_regular_file_and_a_dangling_link_have_no_target() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.toml");
        std::fs::write(&plain, "").unwrap();
        assert_eq!(resolve_target(&plain), None);
        let dangling = dir.path().join("dangling.toml");
        std::os::unix::fs::symlink(dir.path().join("missing.toml"), &dangling).unwrap();
        assert_eq!(resolve_target(&dangling), None);
    }

    /// Wait for a message, rewriting the file now and then so a slow backend
    /// start cannot make the test miss its only event.
    fn expect_reload(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
        write: impl Fn(&str),
    ) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut n = 0;
        while Instant::now() < deadline {
            n += 1;
            write(&format!("v{n}"));
            std::thread::sleep(Duration::from_millis(300));
            if let Ok(got) = rx.try_recv() {
                return Some(got);
            }
        }
        None
    }

    #[test]
    fn saving_the_target_of_a_symlinked_config_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles");
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        let target = real.join("config.toml");
        std::fs::write(&target, "v0").unwrap();
        let link = cfg.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let read = link.clone();
        let _handle = watch_path(link, move || Ok(std::fs::read_to_string(&read)?), tx)
            .expect("the watcher starts");

        let got = expect_reload(&mut rx, |body| std::fs::write(&target, body).unwrap());
        assert!(
            got.is_some(),
            "no reload after the symlink's target was saved"
        );
    }

    #[test]
    fn a_link_created_after_startup_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles");
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        let target = real.join("config.toml");
        std::fs::write(&target, "v0").unwrap();
        let link = cfg.join("config.toml");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let read = link.clone();
        let _handle = watch_path(
            link.clone(),
            move || Ok(std::fs::read_to_string(&read)?),
            tx,
        )
        .expect("the watcher starts with no config file at all");

        std::os::unix::fs::symlink(&target, &link).unwrap();
        // The link's own creation reloads and points the watcher at the target;
        // drain that, then a save of the target must come through.
        let _ = expect_reload(&mut rx, |_| {});
        while rx.try_recv().is_ok() {}
        let got = expect_reload(&mut rx, |body| std::fs::write(&target, body).unwrap());
        assert!(got.is_some(), "the retargeted link's target is not watched");
    }

    #[test]
    fn a_dangling_link_at_startup_does_not_fail_the_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(dir.path().join("missing.toml"), &link).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        assert!(watch_path(link, || Ok(String::new()), tx).is_ok());
    }
}
