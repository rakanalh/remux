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
    resolve(std::env::var_os("XDG_DATA_HOME"), dirs::data_dir())
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// The platform's data directory with `$XDG_DATA_HOME` ignored.
///
/// One caller, `persistence::load_state`, which uses it to find a `state.json`
/// written before remux honoured `XDG_DATA_HOME` on macOS. Nothing else should
/// read it: for every other purpose the answer wanted is [`data_home`].
pub fn platform_data_home() -> Option<PathBuf> {
    dirs::data_dir()
}

/// `$XDG_STATE_HOME`: the directory holding the logs and the sidebar layout.
pub fn state_home() -> PathBuf {
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
    resolve(std::env::var_os("XDG_CONFIG_HOME"), dirs::config_dir())
}

/// The config file, shared by the loader and the live-reload watcher so the two
/// cannot disagree about which file is the config.
pub fn config_file() -> Option<PathBuf> {
    config_home().map(|d| d.join("remux").join("config.toml"))
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
    /// oversight. `persistence::tests::test_load_state_no_file` calls the global
    /// `load_state()`, which resolves through [`data_home`]; moving the variable
    /// under it from another test thread would change which file it reads
    /// mid-test, and the false red would look exactly like a bug in this module.
    /// `resolve` above covers every branch [`data_home`] has, and its two lines
    /// are the same two lines as [`state_home`]'s.
    #[test]
    fn the_resolvers_read_the_environment() {
        let saved: Vec<(&str, Option<OsString>)> = ["XDG_STATE_HOME", "XDG_CONFIG_HOME"]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();

        std::env::set_var("XDG_STATE_HOME", "/tmp/rmx-paths/state");
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/rmx-paths/config");
        assert_eq!(state_home(), Path::new("/tmp/rmx-paths/state"));
        assert_eq!(config_home(), Some(PathBuf::from("/tmp/rmx-paths/config")));
        assert_eq!(
            config_file(),
            Some(PathBuf::from("/tmp/rmx-paths/config/remux/config.toml"))
        );

        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        assert_eq!(config_home(), dirs::config_dir());

        for (key, value) in saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
