//! Config file watching for live-reload.
//!
//! This module watches `~/.config/remux/config.toml` for changes and sends
//! the reloaded configuration through a channel when the file is modified.
//!
//! The config path may be a symlink (a dotfiles checkout is the usual reason).
//! An editor then saves the TARGET, in a directory the link's own directory
//! watch never sees, so the target's directory is watched as well and an event
//! on either path reloads.
//!
//! Known limit: a chain of links is followed by what `canonicalize` resolves,
//! so retargeting a link in the middle of a chain is not noticed until the
//! config's own link path produces an event.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// `path` canonicalized as far as it exists: its nearest existing ancestor is
/// resolved and the part that does not exist yet is appended unchanged.
fn normalize_lenient(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(real) = std::fs::canonicalize(ancestor) {
            return match path.strip_prefix(ancestor) {
                Ok(rest) => real.join(rest),
                Err(_) => path.to_path_buf(),
            };
        }
    }
    path.to_path_buf()
}

/// Where a symlinked config points.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// The canonical target when the link resolves, otherwise the path the
    /// link names, which may not exist yet.
    path: PathBuf,
    /// The link does not resolve (yet).
    dangling: bool,
}

/// The target of `link` and the directory to watch for it, or `None` when
/// `link` is not a symlink.
///
/// A live link resolves to its canonical target, whose directory is watched. A
/// dangling link yields the path it names (a relative one is taken from the
/// link's directory) and the nearest ancestor that exists, so the target's
/// creation is seen: when its directory is missing too, that ancestor reports
/// the directories being made on the way to it.
fn locate(link: &Path) -> Option<(Target, PathBuf)> {
    if let Ok(real) = std::fs::canonicalize(link) {
        if keep_leaf(&real) == keep_leaf(link) {
            return None;
        }
        let dir = real.parent()?.to_path_buf();
        return Some((
            Target {
                path: real,
                dangling: false,
            },
            dir,
        ));
    }
    let named = std::fs::read_link(link).ok()?;
    let intended = if named.is_absolute() {
        named
    } else {
        link.parent()?.join(named)
    };
    let dir = intended
        .parent()?
        .ancestors()
        .find(|a| a.is_dir())?
        .to_path_buf();
    Some((
        Target {
            path: intended,
            dangling: true,
        },
        dir,
    ))
}

/// What a watch event means for the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Classified {
    /// It names the link path or the target: a reload is due.
    is_config: bool,
    /// The link may have moved, appeared or started to resolve: look again.
    relink: bool,
}

/// Classify the paths of one event against the link and its current target.
///
/// The link, the target and every event path are normalized once per call, so a
/// backend that reports `/private/var/...` for a `/var/...` config still
/// matches. An event that names an ancestor of a dangling target also asks for a
/// re-resolve, because that is how the target's directory appearing is seen.
fn classify(event_paths: &[PathBuf], link: &Path, target: Option<&Target>) -> Classified {
    let link_leaf = keep_leaf(link);
    let target_leaf = target.map(|t| normalize_lenient(&t.path));
    let mut out = Classified {
        is_config: false,
        relink: false,
    };
    for path in event_paths {
        let leaf = keep_leaf(path);
        let on_link = leaf == link_leaf;
        let on_target = target_leaf.as_deref() == Some(leaf.as_path());
        out.is_config |= on_link || on_target;
        out.relink |= on_link;
        if let (Some(t), Some(tl)) = (target, target_leaf.as_deref()) {
            out.relink |= t.dangling && tl.starts_with(&leaf);
        }
    }
    out
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
    /// The config's own directory, which is never unwatched.
    keep: PathBuf,
}

impl Watching {
    /// Watch `dir` unless it already is, keyed by its canonical form so two
    /// spellings of one directory do not produce duplicate events. Returns the
    /// key the watch is held under, or `None` when it could not be watched.
    fn watch_dir(&mut self, dir: &Path) -> Option<PathBuf> {
        let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if self.watched.contains(&key) {
            return Some(key);
        }
        match self.watcher.watch(&key, RecursiveMode::NonRecursive) {
            Ok(()) => {
                self.watched.insert(key.clone());
                Some(key)
            }
            Err(e) => {
                log::warn!("config watch: cannot watch {}: {e}", key.display());
                None
            }
        }
    }

    /// Stop watching `key`, a directory that was watched for an earlier target,
    /// so a superseded ancestor (possibly `$HOME`) does not keep a kernel watch
    /// running. The config's own directory and `current`, the directory now
    /// watched for the target, are never dropped: both are compared as keys, so
    /// a different spelling of either cannot remove a watch still in use.
    fn unwatch_dir(&mut self, key: &Path, current: &Path) {
        if key == self.keep || key == current || !self.watched.remove(key) {
            return;
        }
        if let Err(e) = self.watcher.unwatch(key) {
            log::debug!("config watch: cannot unwatch {}: {e}", key.display());
        }
    }
}

/// The most times one look at the link may chase a target directory that
/// appears while it is being watched, which bounds the loop by path depth.
const MAX_RELINK_STEPS: usize = 64;

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
/// new target's directory. A link that dangles at startup is followed once its
/// target appears: the nearest existing ancestor of the target is watched, the
/// watch moves down as the missing directories are created (and the ancestor is
/// unwatched), and the config is reloaded once the link starts to resolve.
/// Adding a watch only reports what happens afterwards, so every move looks
/// again until the directory watched is the target's own.
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
    reload: impl Fn() -> Result<T> + Send + Sync + 'static,
    tx: tokio::sync::mpsc::UnboundedSender<T>,
) -> Result<WatchHandle> {
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?
        .to_path_buf();

    // Create the config directory if it doesn't exist, so we can watch it.
    std::fs::create_dir_all(&config_dir)?;

    // Reloading and sending is shared by the event callback and the re-resolve
    // thread, which reloads when a dangling link starts to resolve.
    let emit: Arc<dyn Fn() + Send + Sync> = Arc::new(move || match reload() {
        Ok(config) => {
            let _ = tx.send(config);
        }
        Err(e) => {
            log::warn!("failed to reload config: {}", e);
        }
    });

    let located = locate(&config_path);
    let extra_dir = located.as_ref().map(|(_, dir)| dir.clone());
    let target = Arc::new(Mutex::new(located.map(|(t, _)| t)));
    // The callback asks this thread to look at the link again; see `Watching`
    // for why the callback cannot do it itself.
    let (relink_tx, relink_rx) = std::sync::mpsc::channel::<()>();

    // Set when the callback reloaded for the target while the link dangled, so
    // the thread does not reload a second time for the same appearance.
    let reloaded_while_dangling = Arc::new(AtomicBool::new(false));
    let cb_flag = Arc::clone(&reloaded_while_dangling);
    let cb_target = Arc::clone(&target);
    let cb_emit = Arc::clone(&emit);
    let watched_path = config_path.clone();
    let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let event = match res {
            Ok(e) => e,
            Err(_) => return,
        };

        let current = cb_target.lock().ok().and_then(|t| t.clone());
        let class = classify(&event.paths, &watched_path, current.as_ref());
        if class.relink {
            let _ = relink_tx.send(());
        }

        // We care about Create and Modify events on the config file.
        let dominated = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
        if !dominated || !class.is_config {
            return;
        }

        if current.as_ref().is_some_and(|t| t.dangling) {
            cb_flag.store(true, Ordering::SeqCst);
        }
        cb_emit();
    })?;

    let mut watching = Watching {
        watcher,
        watched: HashSet::new(),
        keep: std::fs::canonicalize(&config_dir).unwrap_or_else(|_| config_dir.clone()),
    };
    watching.watch_dir(&config_dir);
    let extra_key = extra_dir.as_ref().and_then(|dir| watching.watch_dir(dir));
    if watching.watched.is_empty() {
        anyhow::bail!("cannot watch {}", config_dir.display());
    }
    let watching = Arc::new(Mutex::new(watching));

    // Holds only a weak reference, so dropping the handle drops the watcher,
    // which drops the callback and with it the sender, which ends this thread.
    let weak = Arc::downgrade(&watching);
    std::thread::spawn(move || {
        // The directory watched for the target, other than the config's own.
        let mut extra = extra_key;
        while relink_rx.recv().is_ok() {
            // One look answers every request queued behind it.
            while relink_rx.try_recv().is_ok() {}
            let Some(watching) = weak.upgrade() else {
                return;
            };
            let was_dangling = target
                .lock()
                .ok()
                .and_then(|t| t.as_ref().map(|t| t.dangling))
                .unwrap_or(false);

            let mut now = locate(&config_path);
            for _ in 0..MAX_RELINK_STEPS {
                let Some((_, dir)) = now.clone() else {
                    break;
                };
                if let Ok(mut w) = watching.lock() {
                    let key = w.watch_dir(&dir);
                    if let (Some(old), Some(new)) = (extra.as_ref(), key.as_ref()) {
                        if old != new {
                            w.unwatch_dir(old, new);
                        }
                    }
                    if key.is_some() {
                        extra = key;
                    }
                }
                // The watch reports only what happens from now on. Whatever
                // appeared while it was being added (the next directory down,
                // or the file itself) is found by looking again.
                let again = locate(&config_path);
                let settled = again.as_ref().map(|(_, d)| d) == Some(&dir);
                now = again;
                if settled {
                    break;
                }
            }
            let live_now = now.as_ref().is_some_and(|(t, _)| !t.dangling);
            if let Ok(mut t) = target.lock() {
                *t = now.map(|(t, _)| t);
            }
            // The creation that made the link resolve may have happened before
            // it was watched, so no event is coming for it. When the callback
            // already reloaded for it, this would be the same appearance twice.
            if was_dangling && live_now && !reloaded_while_dangling.swap(false, Ordering::SeqCst) {
                emit();
            }
            if !live_now {
                reloaded_while_dangling.store(false, Ordering::SeqCst);
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

    fn paths(p: &[&Path]) -> Vec<PathBuf> {
        p.iter().map(|p| p.to_path_buf()).collect()
    }

    fn live(path: PathBuf) -> Target {
        Target {
            path,
            dangling: false,
        }
    }

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
        let (target, watch) = locate(&link).expect("a live link resolves");
        assert!(!target.dangling);
        assert_eq!(watch, std::fs::canonicalize(&real).unwrap());

        let is_config = |p: &Path, t: Option<&Target>| classify(&paths(&[p]), &link, t).is_config;
        assert!(is_config(&link, Some(&target)));
        assert!(is_config(&real.join("config.toml"), Some(&target)));
        assert!(!is_config(&real.join("other.toml"), Some(&target)));
        assert!(!is_config(&cfg.join("other.toml"), Some(&target)));
        // Without a target only the link itself counts.
        assert!(!is_config(&real.join("config.toml"), None));
        assert!(is_config(&link, None));
    }

    #[test]
    fn only_an_event_on_the_link_or_toward_a_dangling_target_asks_to_look_again() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("config.toml");
        let live_target = live(dir.path().join("real.toml"));
        assert!(classify(&paths(&[&link]), &link, Some(&live_target)).relink);
        assert!(!classify(&paths(&[&live_target.path]), &link, Some(&live_target)).relink);
        let dangling = Target {
            path: dir.path().join("deep/er/config.toml"),
            dangling: true,
        };
        // The directory being made on the way to the target.
        assert!(classify(&paths(&[&dir.path().join("deep")]), &link, Some(&dangling)).relink);
        assert!(
            !classify(
                &paths(&[&dir.path().join("elsewhere")]),
                &link,
                Some(&dangling)
            )
            .relink
        );
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
        assert!(classify(&paths(&[&canonical]), &link, None).is_config);
        assert!(classify(&paths(&[&link]), &canonical, None).is_config);
    }

    #[test]
    fn a_regular_file_has_no_target_and_a_dangling_link_names_one() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.toml");
        std::fs::write(&plain, "").unwrap();
        assert_eq!(locate(&plain), None);
        let dangling = dir.path().join("dangling.toml");
        std::os::unix::fs::symlink("missing.toml", &dangling).unwrap();
        let (target, watch) = locate(&dangling).expect("a dangling link still names a target");
        assert!(target.dangling);
        assert_eq!(target.path, dir.path().join("missing.toml"));
        assert_eq!(
            std::fs::canonicalize(&watch).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
        // A target whose directory is missing too is watched from the nearest
        // ancestor that exists.
        let deeper = dir.path().join("deeper.toml");
        std::os::unix::fs::symlink("a/b/c.toml", &deeper).unwrap();
        let (_, watch) = locate(&deeper).unwrap();
        assert_eq!(
            std::fs::canonicalize(&watch).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
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

    fn start_watching(link: &Path) -> (WatchHandle, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let read = link.to_path_buf();
        let handle = watch_path(
            link.to_path_buf(),
            move || Ok(std::fs::read_to_string(&read)?),
            tx,
        )
        .expect("the watcher starts");
        (handle, rx)
    }

    #[test]
    fn a_link_dangling_at_startup_is_followed_once_its_target_appears() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles");
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        let target = real.join("config.toml");
        let link = cfg.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (_handle, mut rx) = start_watching(&link);
        let got = expect_reload(&mut rx, |body| std::fs::write(&target, body).unwrap());
        assert!(
            got.is_some(),
            "no reload after the dangling target was created"
        );
    }

    /// Wait for a reload that nothing is prompting, so a missed event cannot be
    /// papered over by writing the file again.
    fn expect_quiet_reload(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    ) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(got) = rx.try_recv() {
                return Some(got);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    #[test]
    fn a_target_created_with_its_directories_in_one_burst_is_loaded_without_a_second_write() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let target = dir.path().join("dotfiles/remux/config.toml");
        let link = cfg.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (_handle, mut rx) = start_watching(&link);

        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "v1").unwrap();
        assert!(
            expect_quiet_reload(&mut rx).is_some(),
            "the first version of the config was never loaded"
        );
    }

    #[test]
    fn a_superseded_ancestor_watch_is_dropped_once_the_target_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let target = dir.path().join("dotfiles/remux/config.toml");
        let link = cfg.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (handle, mut rx) = start_watching(&link);
        let root = std::fs::canonicalize(dir.path()).unwrap();
        assert!(handle._watching.lock().unwrap().watched.contains(&root));

        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "v1").unwrap();
        assert!(expect_quiet_reload(&mut rx).is_some());

        let watched = handle._watching.lock().unwrap().watched.clone();
        assert!(
            !watched.contains(&root),
            "the ancestor is still watched: {watched:?}"
        );
        assert!(watched.contains(&std::fs::canonicalize(&cfg).unwrap()));
        assert!(watched.contains(&std::fs::canonicalize(target.parent().unwrap()).unwrap()));
    }

    /// A `../` link, as GNU stow makes them, to a directory that exists, with the
    /// file moved in by a rename (one event). The spelling of the link's
    /// directory differs from its canonical form, which must not make the
    /// re-resolve drop the watch it just kept.
    #[test]
    fn a_relative_dotdot_link_keeps_its_watch_after_the_target_is_renamed_in() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let real = dir.path().join("dotfiles");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        let link = cfg.join("config.toml");
        std::os::unix::fs::symlink("../dotfiles/config.toml", &link).unwrap();
        let (_handle, mut rx) = start_watching(&link);

        let staged = dir.path().join("staged.toml");
        std::fs::write(&staged, "v1").unwrap();
        std::fs::rename(&staged, real.join("config.toml")).unwrap();
        assert!(
            expect_quiet_reload(&mut rx).is_some(),
            "the first version was never loaded"
        );
        while rx.try_recv().is_ok() {}

        std::fs::write(real.join("config.toml"), "v2").unwrap();
        assert!(
            expect_quiet_reload(&mut rx).is_some(),
            "a save after the target appeared was ignored: the watch was dropped"
        );
    }

    #[test]
    fn one_appearance_of_the_target_is_one_reload() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let real = dir.path().join("dotfiles");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        let link = cfg.join("config.toml");
        std::os::unix::fs::symlink(real.join("config.toml"), &link).unwrap();
        let (_handle, mut rx) = start_watching(&link);

        let staged = dir.path().join("staged.toml");
        std::fs::write(&staged, "v1").unwrap();
        std::fs::rename(&staged, real.join("config.toml")).unwrap();
        assert!(expect_quiet_reload(&mut rx).is_some());
        std::thread::sleep(Duration::from_millis(1500));
        assert!(rx.try_recv().is_err(), "the same appearance reloaded twice");
    }
}
