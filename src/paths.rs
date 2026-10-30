//! Where remux keeps its files, resolved the same way on every platform.
//!
//! The `dirs` crate reads the XDG variables on Linux and IGNORES them on macOS,
//! where `data_dir()` and `config_dir()` are both `~/Library/Application Support`
//! and `state_dir()` is `None`. That is not a difference remux can leave alone.
//! An isolated test harness exports `XDG_STATE_HOME` and `XDG_DATA_HOME` to keep
//! away from the user's real files; on a Mac the server ignored both and wrote the
//! user's real `state.json`. The socket was the one path that stayed isolated,
//! because `server::daemon::socket_path` reads `XDG_RUNTIME_DIR` itself.
//!
//! So each resolver here prefers the environment variable and falls back to the
//! platform. `socket_path` is left as it is: it consults `dirs::runtime_dir()`
//! first, which reads `XDG_RUNTIME_DIR` on Linux and is `None` on macOS, so it
//! already lands on the variable on both.
//!
//! Under `cfg(test)` none of this is consulted: every resolver answers with a
//! directory in `mod sandbox`, so setting `XDG_DATA_HOME` in a unit test does
//! not move `data_home()`. The `configured_*` functions still hold the real
//! resolution, and the tests below exercise it directly.

use std::ffi::OsString;
use std::path::PathBuf;

/// Combine an XDG environment variable with the platform's own answer.
///
/// A value is honoured only when it is ABSOLUTE, which also rejects an empty one.
/// The XDG base directory specification requires that, and `dirs` already applies
/// it on Linux, so Linux behaviour is unchanged rather than merely similar: a
/// relative value is ignored on both paths instead of being resolved against the
/// working directory, which for the server is whichever directory the client
/// happened to start it from.
///
/// Pure, and takes both inputs as parameters, so the tests can reach every branch
/// without `std::env::set_var`. That call mutates state shared by every test
/// thread in the binary.
fn resolve(env_value: Option<OsString>, platform: Option<PathBuf>) -> Option<PathBuf> {
    env_value
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or(platform)
}

/// `$XDG_DATA_HOME`: the directory holding saved sessions.
pub fn data_home() -> PathBuf {
    #[cfg(test)]
    return sandbox::dir("data");
    #[cfg(not(test))]
    configured_data_home()
}

fn configured_data_home() -> PathBuf {
    resolve(std::env::var_os("XDG_DATA_HOME"), dirs::data_dir())
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// The platform's data directory with `$XDG_DATA_HOME` ignored.
///
/// One caller, `persistence::load_state`, which uses it to find a `state.json`
/// written before remux honoured `XDG_DATA_HOME` on macOS. Nothing else should
/// read it: for every other purpose the answer wanted is [`data_home`].
///
/// `None` under test. With [`data_home`] in the sandbox, the platform
/// directory would otherwise differ from it on Linux too, and `load_state`
/// would read the user's real `state.json` as a legacy file.
pub fn platform_data_home() -> Option<PathBuf> {
    #[cfg(test)]
    return None;
    #[cfg(not(test))]
    dirs::data_dir()
}

/// `$XDG_STATE_HOME`: the directory holding the logs and the sidebar layout.
pub fn state_home() -> PathBuf {
    #[cfg(test)]
    return sandbox::dir("state");
    #[cfg(not(test))]
    configured_state_home()
}

fn configured_state_home() -> PathBuf {
    resolve(std::env::var_os("XDG_STATE_HOME"), dirs::state_dir())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `$XDG_CONFIG_HOME`.
///
/// `Option`, where the two above return a `PathBuf`, because both callers read
/// "no config directory" as "no config file" and use the built-in defaults. A
/// `/tmp` fallback would instead have remux read its configuration out of a
/// world-writable directory.
pub fn config_home() -> Option<PathBuf> {
    #[cfg(test)]
    return Some(sandbox::dir("config"));
    #[cfg(not(test))]
    configured_config_home()
}

fn configured_config_home() -> Option<PathBuf> {
    resolve(std::env::var_os("XDG_CONFIG_HOME"), dirs::config_dir())
}

/// The config file, shared by the loader and the live-reload watcher so the two
/// cannot disagree about which file is the config.
pub fn config_file() -> Option<PathBuf> {
    config_home().map(|d| d.join("remux").join("config.toml"))
}

/// Where the unit tests' files go instead of the user's directories.
///
/// The writers (`persistence::save_state`, the mailbox, `sidebar.json`, the
/// hook counters) take no path parameter. They resolve their path through this
/// module, so a test that drives a real handler writes wherever these
/// functions point. The environment does not point anywhere safe: `cargo test`
/// run from an ordinary shell inherits the user's own directories, and the
/// server tests that save through a handler overwrote the user's `state.json`.
///
/// One directory per test PROCESS, not per test. The writers are global, and a
/// per-thread directory would separate a test from the tokio worker threads
/// that save on its behalf. The directory is never removed, because a `static`
/// is never dropped.
///
/// `cfg(test)` is set only in the crate's own unit-test binary. A Rust
/// integration test under `tests/*.rs` would link the normal build, skip this
/// sandbox, and write to the user's directories again.
#[cfg(test)]
mod sandbox {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    static ROOT: OnceLock<PathBuf> = OnceLock::new();

    pub(super) fn dir(kind: &str) -> PathBuf {
        ROOT.get_or_init(|| {
            tempfile::Builder::new()
                .prefix("remux-test-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .expect("create the test sandbox")
                .keep()
        })
        .join(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn an_absolute_env_value_wins_over_the_platform_directory() {
        let got = resolve(
            Some(OsString::from("/srv/state")),
            Some(PathBuf::from("/home/u/Library/Application Support")),
        );
        assert_eq!(got, Some(PathBuf::from("/srv/state")));
    }

    /// The branch that keeps Linux unchanged. `dirs` discards a relative value,
    /// so honouring one here would have made the two paths disagree.
    #[test]
    fn a_relative_env_value_falls_through_to_the_platform_directory() {
        let platform = Some(PathBuf::from("/home/u/.local/share"));
        assert_eq!(
            resolve(Some(OsString::from("state")), platform.clone()),
            platform
        );
    }

    #[test]
    fn an_empty_env_value_falls_through_to_the_platform_directory() {
        let platform = Some(PathBuf::from("/home/u/.local/share"));
        assert_eq!(resolve(Some(OsString::new()), platform.clone()), platform);
    }

    #[test]
    fn an_unset_env_value_falls_through_to_the_platform_directory() {
        let platform = Some(PathBuf::from("/home/u/.local/share"));
        assert_eq!(resolve(None, platform.clone()), platform);
    }

    #[test]
    fn nothing_resolves_when_neither_source_has_an_answer() {
        assert_eq!(resolve(None, None), None);
    }

    /// The real readers, against the real process environment.
    ///
    /// Both variables are exercised in ONE test on purpose. `set_var` is
    /// process-global, so two tests doing this could not run in parallel; a single
    /// test needs no lock. Each is restored to what it was, because a harness
    /// running `cargo test` may have set it.
    ///
    /// `XDG_DATA_HOME` is deliberately NOT touched here, and that is not an
    /// oversight. `daemon`'s `a_command_saves_the_session_state_into_the_test_sandbox`
    /// reads it to name the directory a test must never write; moving the
    /// variable under it from another test thread would point that check at the
    /// wrong directory. `resolve` above covers every branch
    /// [`configured_data_home`] has, and its two lines are the same two lines as
    /// [`configured_state_home`]'s.
    #[test]
    fn the_resolvers_read_the_environment() {
        let saved: Vec<(&str, Option<OsString>)> = ["XDG_STATE_HOME", "XDG_CONFIG_HOME"]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();

        std::env::set_var("XDG_STATE_HOME", "/tmp/rmx-paths/state");
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/rmx-paths/config");
        assert_eq!(configured_state_home(), Path::new("/tmp/rmx-paths/state"));
        assert_eq!(
            configured_config_home(),
            Some(PathBuf::from("/tmp/rmx-paths/config"))
        );

        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        assert_eq!(configured_config_home(), dirs::config_dir());

        for (key, value) in saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn under_test_every_directory_is_in_one_sandbox_and_none_is_configured() {
        let root = data_home().parent().expect("sandbox root").to_path_buf();
        assert!(root
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("remux-test-")));
        assert_eq!(data_home(), root.join("data"));
        assert_eq!(state_home(), root.join("state"));
        assert_eq!(config_home(), Some(root.join("config")));
        assert_eq!(
            config_file(),
            Some(root.join("config").join("remux").join("config.toml"))
        );
        assert_eq!(platform_data_home(), None);
        assert_ne!(data_home(), configured_data_home());
    }
}
