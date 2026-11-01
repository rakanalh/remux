//! Where each agent keeps its configuration, as that agent resolves it: its
//! own environment variable first, then a directory under `HOME`.
//!
//! A variable is honoured only when it holds an absolute path. A relative one
//! would resolve against whatever directory `remux integrate` was run from,
//! and the agent itself, started elsewhere, would read a different file.
//! `HOME` is passed in, read directly by the caller as
//! [`super::hooks_file::home_and_binary`] does, so a throwaway one is honoured.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|p| p.is_absolute())
}

fn var(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_config_dir(home: &Path) -> PathBuf {
    claude_config_dir_with(home, var)
}

/// `$CODEX_HOME`, else `~/.codex`.
#[allow(dead_code)]
pub fn codex_home(home: &Path) -> PathBuf {
    codex_home_with(home, var)
}

/// `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
#[allow(dead_code)]
pub fn pi_agent_dir(home: &Path) -> PathBuf {
    pi_agent_dir_with(home, var)
}

/// `$PI_CODING_AGENT_DIR`, else `~/$PI_CONFIG_DIR/agent`, `PI_CONFIG_DIR`
/// defaulting to `.omp`.
#[allow(dead_code)]
pub fn omp_agent_dir(home: &Path) -> PathBuf {
    omp_agent_dir_with(home, var)
}

/// `$OPENCODE_CONFIG_DIR`, else `$XDG_CONFIG_HOME/opencode`, else
/// `~/.config/opencode`.
#[allow(dead_code)]
pub fn opencode_config_dir(home: &Path) -> PathBuf {
    opencode_config_dir_with(home, var)
}

fn claude_config_dir_with(home: &Path, var: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    absolute(var("CLAUDE_CONFIG_DIR")).unwrap_or_else(|| home.join(".claude"))
}

fn codex_home_with(home: &Path, var: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    absolute(var("CODEX_HOME")).unwrap_or_else(|| home.join(".codex"))
}

fn pi_agent_dir_with(home: &Path, var: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    absolute(var("PI_CODING_AGENT_DIR")).unwrap_or_else(|| home.join(".pi").join("agent"))
}

/// `PI_CONFIG_DIR` names a directory under `HOME`, not a path, so it is kept
/// only when it is one plain component: anything that could climb out of
/// `HOME` falls back to `.omp`.
fn omp_agent_dir_with(home: &Path, var: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    if let Some(dir) = absolute(var("PI_CODING_AGENT_DIR")) {
        return dir;
    }
    let name = var("PI_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|p| {
            matches!(
                p.components().collect::<Vec<_>>()[..],
                [Component::Normal(_)]
            )
        })
        .unwrap_or_else(|| PathBuf::from(".omp"));
    home.join(name).join("agent")
}

fn opencode_config_dir_with(home: &Path, var: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    absolute(var("OPENCODE_CONFIG_DIR"))
        .or_else(|| absolute(var("XDG_CONFIG_HOME")).map(|d| d.join("opencode")))
        .unwrap_or_else(|| home.join(".config").join("opencode"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const HOME: &str = "/home/u";

    fn with(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |name| vars.get(name).cloned()
    }

    type Resolver = fn(&Path, &dyn Fn(&str) -> Option<OsString>) -> PathBuf;

    fn each() -> [(&'static str, Resolver, &'static str); 4] {
        [
            (
                "CLAUDE_CONFIG_DIR",
                |h, v| claude_config_dir_with(h, v),
                "/home/u/.claude",
            ),
            ("CODEX_HOME", |h, v| codex_home_with(h, v), "/home/u/.codex"),
            (
                "PI_CODING_AGENT_DIR",
                |h, v| pi_agent_dir_with(h, v),
                "/home/u/.pi/agent",
            ),
            (
                "PI_CODING_AGENT_DIR",
                |h, v| omp_agent_dir_with(h, v),
                "/home/u/.omp/agent",
            ),
        ]
    }

    #[test]
    fn unset_is_the_home_default() {
        for (var, resolve, default) in each() {
            assert_eq!(
                resolve(Path::new(HOME), &with(&[])),
                Path::new(default),
                "{var}"
            );
        }
        assert_eq!(
            opencode_config_dir_with(Path::new(HOME), with(&[])),
            Path::new("/home/u/.config/opencode")
        );
    }

    #[test]
    fn an_absolute_value_is_honoured() {
        for (var, resolve, _) in each() {
            assert_eq!(
                resolve(Path::new(HOME), &with(&[(var, "/elsewhere/dir")])),
                Path::new("/elsewhere/dir"),
                "{var}"
            );
        }
        assert_eq!(
            opencode_config_dir_with(Path::new(HOME), with(&[("OPENCODE_CONFIG_DIR", "/oc")])),
            Path::new("/oc")
        );
        assert_eq!(
            opencode_config_dir_with(Path::new(HOME), with(&[("XDG_CONFIG_HOME", "/xdg")])),
            Path::new("/xdg/opencode")
        );
    }

    #[test]
    fn a_relative_value_is_ignored() {
        for (var, resolve, default) in each() {
            assert_eq!(
                resolve(Path::new(HOME), &with(&[(var, "relative/dir")])),
                Path::new(default),
                "{var}"
            );
        }
        for var in ["OPENCODE_CONFIG_DIR", "XDG_CONFIG_HOME"] {
            assert_eq!(
                opencode_config_dir_with(Path::new(HOME), with(&[(var, "rel")])),
                Path::new("/home/u/.config/opencode"),
                "{var}"
            );
        }
    }

    #[test]
    fn opencode_prefers_its_own_variable_to_xdg() {
        let vars = with(&[("OPENCODE_CONFIG_DIR", "/oc"), ("XDG_CONFIG_HOME", "/xdg")]);
        assert_eq!(
            opencode_config_dir_with(Path::new(HOME), vars),
            Path::new("/oc")
        );
    }

    #[test]
    fn omp_takes_pi_config_dir_as_one_name_under_home() {
        let resolve = |vars: &[(&str, &str)]| omp_agent_dir_with(Path::new(HOME), with(vars));
        assert_eq!(
            resolve(&[("PI_CONFIG_DIR", ".mine")]),
            Path::new("/home/u/.mine/agent")
        );
        for bad in ["../escape", "/abs", "a/b", ""] {
            assert_eq!(
                resolve(&[("PI_CONFIG_DIR", bad)]),
                Path::new("/home/u/.omp/agent"),
                "{bad:?}"
            );
        }
        assert_eq!(
            resolve(&[("PI_CODING_AGENT_DIR", "/pi"), ("PI_CONFIG_DIR", ".mine")]),
            Path::new("/pi")
        );
    }
}
