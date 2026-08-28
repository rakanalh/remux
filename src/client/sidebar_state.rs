//! Persisted sidebar runtime state.
//!
//! The config supplies the DEFAULTS; this file records what the user did to
//! them at runtime -- which sidebars are open, how wide they are, and how the
//! panels inside them are weighted -- so their adjustments survive a client
//! restart.
//!
//! `focused_panel` is deliberately absent: focus is a within-session concept,
//! and restoring the keyboard into a panel a fresh client never showed the user
//! would be a surprise, not a convenience.
//!
//! Nothing here is allowed to fail a client start. A missing file is the normal
//! first run; an unreadable or corrupt one degrades to "use the config
//! defaults" with a `warn!`. State written for an edge the config no longer
//! declares is ignored rather than applied or rejected -- people edit their
//! config after state has been written, and the client has to survive it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::client::chrome::{Chrome, SidebarEdge};

/// Everything about the chrome that outlives a client process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarState {
    #[serde(default)]
    pub bars: Vec<BarState>,
}

/// One sidebar's persisted state, keyed by the edge it is docked to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarState {
    pub edge: SidebarEdge,
    pub visible: bool,
    pub size: u16,
    /// One weight per panel, in the sidebar's stacking order. Matched
    /// positionally against the config's panels, so a list of a different
    /// length applies as far as it goes.
    #[serde(default)]
    pub weights: Vec<u16>,
}

impl SidebarState {
    /// Snapshot the persistable fields of a live `Chrome`.
    pub fn from_chrome(chrome: &Chrome) -> Self {
        Self {
            bars: chrome
                .sidebars
                .iter()
                .map(|s| BarState {
                    edge: s.edge,
                    visible: s.visible,
                    size: s.size,
                    weights: s.panels.iter().map(|p| p.weight).collect(),
                })
                .collect(),
        }
    }

    /// Parse persisted JSON, falling back to "no state" on anything malformed.
    pub fn from_json(s: &str) -> Self {
        match serde_json::from_str(s) {
            Ok(state) => state,
            Err(e) => {
                log::warn!(
                    "sidebar state: ignoring unparseable state ({e}); using config defaults"
                );
                Self::default()
            }
        }
    }
}

/// `$XDG_STATE_HOME/remux/sidebar.json`, with the same fallback chain the log
/// directory uses so both land in one place.
fn state_path() -> PathBuf {
    dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("remux")
        .join("sidebar.json")
}

/// Read the persisted state. Never fails: every error path is a `warn!` and the
/// config defaults.
pub fn load() -> SidebarState {
    load_from(&state_path())
}

/// `load`, against an explicit path. Split out so the tests exercise the real
/// I/O without mutating the process environment.
pub fn load_from(path: &Path) -> SidebarState {
    if !path.exists() {
        // The normal first run. Not worth a warning.
        return SidebarState::default();
    }
    match std::fs::read_to_string(path) {
        Ok(body) => SidebarState::from_json(&body),
        Err(e) => {
            log::warn!(
                "sidebar state: cannot read {}: {e}; using config defaults",
                path.display()
            );
            SidebarState::default()
        }
    }
}

/// Persist the chrome's current state. Best effort: a failure to write is
/// logged and otherwise ignored, because losing a remembered sidebar width is
/// never worth interrupting the session over.
pub fn save(chrome: &Chrome) {
    save_to(&state_path(), &SidebarState::from_chrome(chrome));
}

/// `save`, against an explicit path and state.
///
/// Writes a sibling temp file and renames it over the target, so a client that
/// dies mid-write leaves the previous state intact rather than a truncated file
/// the next start has to discard. The temp name carries the pid: two clients
/// sharing an `XDG_STATE_HOME` would otherwise write the SAME temp file and
/// rename a torn interleaving of the two into place.
pub fn save_to(path: &Path, state: &SidebarState) {
    let Some(dir) = path.parent() else {
        log::warn!("sidebar state: {} has no parent directory", path.display());
        return;
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        log::warn!("sidebar state: cannot create {}: {e}", dir.display());
        return;
    }
    let body = match serde_json::to_string_pretty(state) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("sidebar state: cannot serialize: {e}");
            return;
        }
    };
    let tmp = tmp_path(path);
    if let Err(e) = std::fs::write(&tmp, body) {
        log::warn!("sidebar state: cannot write {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        log::warn!("sidebar state: cannot rename into {}: {e}", path.display());
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The scratch file `save_to` renames into place.
///
/// Per-process: two clients sharing an `XDG_STATE_HOME` writing the same temp
/// name can interleave and rename a torn file into place, which would narrow
/// the "never corrupt" property to a single writer.
fn tmp_path(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.tmp", std::process::id()))
}

/// Overlay persisted state onto a config-built `Chrome`.
///
/// Bars are matched by `edge`, each config sidebar claimed at most once, so a
/// config with two sidebars on the same edge (which `panel_rects` supports,
/// stacking them inward) zips positionally instead of applying every state
/// entry to the first one. A state entry with no match left is ignored.
pub fn apply(chrome: &mut Chrome, state: &SidebarState) {
    let mut claimed = vec![false; chrome.sidebars.len()];
    for bar in &state.bars {
        let found = (0..chrome.sidebars.len())
            .find(|i| !claimed[*i] && chrome.sidebars[*i].edge == bar.edge);
        let Some(i) = found else {
            log::debug!(
                "sidebar state: no {:?} sidebar in the config; ignoring its saved state",
                bar.edge
            );
            continue;
        };
        claimed[i] = true;
        let sb = &mut chrome.sidebars[i];
        sb.visible = bar.visible;
        sb.size = bar.size;
        // Positional, and short-circuited by the shorter of the two: a config
        // that gained or lost a panel since the state was written applies what
        // still lines up and leaves the rest at their config weights.
        for (panel, weight) in sb.panels.iter_mut().zip(bar.weights.iter()) {
            panel.weight = *weight;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::chrome::{Chrome, SidebarEdge};
    use crate::config::sidebar::{PanelConfig, SidebarConfig};

    fn chrome() -> Chrome {
        Chrome::from_config(&[SidebarConfig {
            edge: SidebarEdge::Left,
            size: 30,
            visible: true,
            panel: vec![
                PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 2,
                },
                PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 1,
                },
            ],
        }])
    }

    #[test]
    fn a_round_trip_preserves_visibility_size_and_weights() {
        let mut c = chrome();
        c.sidebars[0].visible = false;
        c.sidebars[0].size = 44;
        c.sidebars[0].panels[1].weight = 5;
        let state = SidebarState::from_chrome(&c);

        let mut fresh = chrome();
        apply(&mut fresh, &state);
        assert!(!fresh.sidebars[0].visible);
        assert_eq!(fresh.sidebars[0].size, 44);
        assert_eq!(fresh.sidebars[0].panels[1].weight, 5);
    }

    #[test]
    fn state_for_an_edge_the_config_no_longer_declares_is_ignored() {
        let state = SidebarState {
            bars: vec![BarState {
                edge: SidebarEdge::Right,
                visible: false,
                size: 9,
                weights: vec![1],
            }],
        };
        let mut c = chrome(); // only a Left sidebar
        apply(&mut c, &state); // must not panic
        assert_eq!(c.sidebars.len(), 1);
        assert_eq!(c.sidebars[0].size, 30, "unrelated state must not apply");
    }

    #[test]
    fn a_weight_list_shorter_than_the_panels_applies_what_it_has() {
        let state = SidebarState {
            bars: vec![BarState {
                edge: SidebarEdge::Left,
                visible: true,
                size: 30,
                weights: vec![7],
            }],
        };
        let mut c = chrome();
        apply(&mut c, &state);
        assert_eq!(c.sidebars[0].panels[0].weight, 7);
        assert_eq!(
            c.sidebars[0].panels[1].weight, 1,
            "untouched panel keeps its config weight"
        );
    }

    #[test]
    fn a_weight_list_longer_than_the_panels_applies_what_fits() {
        // The mirror of the short-list case: a config that LOST a panel.
        let state = SidebarState {
            bars: vec![BarState {
                edge: SidebarEdge::Left,
                visible: true,
                size: 30,
                weights: vec![7, 8, 9],
            }],
        };
        let mut c = chrome();
        apply(&mut c, &state);
        assert_eq!(c.sidebars[0].panels[0].weight, 7);
        assert_eq!(c.sidebars[0].panels[1].weight, 8);
    }

    #[test]
    fn load_returns_empty_state_when_the_file_is_absent_or_corrupt() {
        // Never fail a client start over persisted chrome state.
        let s = SidebarState::from_json("{ not json");
        assert!(s.bars.is_empty());
    }

    #[test]
    fn json_naming_an_unknown_edge_is_discarded_whole() {
        // A hand-edited or future-version file must not take the client down.
        let s = SidebarState::from_json(
            r#"{"bars":[{"edge":"top","visible":true,"size":4,"weights":[]}]}"#,
        );
        assert!(s.bars.is_empty());
    }

    #[test]
    fn a_missing_file_loads_as_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("remux").join("sidebar.json");
        assert!(load_from(&path).bars.is_empty());
    }

    #[test]
    fn an_unparseable_file_loads_as_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sidebar.json");
        std::fs::write(&path, "{ not json").expect("write");
        assert!(load_from(&path).bars.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_through_the_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory that does not exist yet: the first save creates it.
        let path = dir.path().join("remux").join("sidebar.json");

        let mut c = chrome();
        c.sidebars[0].visible = false;
        c.sidebars[0].size = 41;
        c.sidebars[0].panels[0].weight = 6;
        save_to(&path, &SidebarState::from_chrome(&c));
        assert!(path.exists(), "save did not write the file");

        let mut fresh = chrome();
        apply(&mut fresh, &load_from(&path));
        assert!(!fresh.sidebars[0].visible);
        assert_eq!(fresh.sidebars[0].size, 41);
        assert_eq!(fresh.sidebars[0].panels[0].weight, 6);
        // The temp file is renamed, not left behind -- and the name is
        // per-process, so check the directory rather than one fixed name.
        let left: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert_eq!(left.len(), 1, "a temp file was left behind: {left:?}");
    }

    #[test]
    fn the_temp_file_is_per_process() {
        let t = tmp_path(Path::new("/x/sidebar.json"));
        let name = t.file_name().expect("name").to_string_lossy().into_owned();
        assert!(name.ends_with(".tmp"), "{name}");
        assert!(
            name.contains(&std::process::id().to_string()),
            "two clients sharing a state dir would write the same temp file: {name}"
        );
        assert_ne!(t, PathBuf::from("/x/sidebar.json"));
    }

    #[test]
    fn two_sidebars_on_one_edge_are_matched_positionally() {
        let cfg = SidebarConfig {
            edge: SidebarEdge::Left,
            size: 30,
            visible: true,
            panel: vec![PanelConfig {
                plugin: "placeholder".into(),
                weight: 1,
            }],
        };
        let mut c = Chrome::from_config(&[cfg.clone(), cfg]);
        apply(
            &mut c,
            &SidebarState {
                bars: vec![
                    BarState {
                        edge: SidebarEdge::Left,
                        visible: true,
                        size: 11,
                        weights: vec![],
                    },
                    BarState {
                        edge: SidebarEdge::Left,
                        visible: false,
                        size: 22,
                        weights: vec![],
                    },
                ],
            },
        );
        assert_eq!(c.sidebars[0].size, 11);
        assert_eq!(c.sidebars[1].size, 22);
        assert!(
            !c.sidebars[1].visible,
            "the second entry took the second bar"
        );
    }

    #[test]
    fn focus_is_not_persisted() {
        let mut c = chrome();
        c.sidebars[0].focused_panel = 1;
        let json = serde_json::to_string(&SidebarState::from_chrome(&c)).expect("serialize");
        assert!(
            !json.contains("focus"),
            "focus is a within-session concept: {json}"
        );

        let mut fresh = chrome();
        fresh.sidebars[0].focused_panel = 1;
        apply(&mut fresh, &SidebarState::from_chrome(&c));
        assert_eq!(
            fresh.sidebars[0].focused_panel, 1,
            "apply must leave focus alone"
        );
    }

    #[test]
    fn a_chrome_with_no_sidebars_saves_and_applies_as_a_no_op() {
        // The regression gate: with no `[[sidebar]]` configured nothing here
        // may misbehave.
        let mut c = Chrome::from_config(&[]);
        let state = SidebarState::from_chrome(&c);
        assert!(state.bars.is_empty());
        apply(&mut c, &SidebarState::default());
        assert!(c.sidebars.is_empty());
    }
}
