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
//!
//! # Precedence, and why a hot-reload does not use `apply`
//!
//! At STARTUP the persisted state wins outright ([`apply`]): the config is the
//! default, and what the user last dragged is what they expect to come back to.
//!
//! A hot-reload cannot use that rule. `sidebar.json` is written the moment
//! anyone toggles or resizes a sidebar, so from then on it holds a `size` for
//! every edge -- and a startup-style overlay would silently revert every
//! `size = ...` the user then typed into their config, reproducing the "I
//! edited my config and nothing happened" complaint that hot-reload exists to
//! fix. [`apply_on_reload`] therefore compares the OLD config against the NEW
//! one, field by field:
//!
//! > **What you just typed wins; what you did not type keeps what you dragged.**
//!
//! A field whose config value changed takes the config value; a field whose
//! config value is unchanged keeps the persisted runtime value. A brand-new
//! edge takes its config wholesale, stale state for it and all.
//!
//! "Brand new" means *never seen this session*, not *absent from the previous
//! file* -- which is why the caller advances its snapshot with
//! [`merge_seen_config`] rather than replacing it. Commenting a `[[sidebar]]`
//! block out and back in types nothing new, so it has to return the width the
//! user dragged; a restart with the block present already does exactly that
//! through [`apply`], and a reload disagreeing with the restart path it is
//! meant to replace would be its own bug. An edge no config this session has
//! ever declared is still brand new, which is what the rule was for.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::client::chrome::{Chrome, SidebarEdge};
use crate::config::sidebar::SidebarConfig;

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
///
/// Entries for edges the chrome does not currently have are CARRIED FORWARD
/// (see [`merged`]) rather than dropped, so commenting a `[[sidebar]]` block
/// out and back in returns the width the user dragged.
pub fn save(chrome: &Chrome) {
    let path = state_path();
    let previous = load_from(&path);
    save_to(&path, &merged(chrome, &previous));
}

/// The state to persist for `chrome`, keeping `previous`'s entries for edges
/// `chrome` no longer has.
///
/// `SidebarState::from_chrome` alone describes only what is configured right
/// now, so writing it verbatim DESTROYS the remembered size and visibility of
/// every edge the config has since dropped. Commenting a `[[sidebar]]` block
/// out to try something without it, then putting it back, would hand the user
/// the config default instead of what they had -- quiet data loss on an action
/// that reads as reversible.
pub fn merged(chrome: &Chrome, previous: &SidebarState) -> SidebarState {
    let mut state = SidebarState::from_chrome(chrome);
    for bar in &previous.bars {
        if !state.bars.iter().any(|b| b.edge == bar.edge) {
            log::debug!(
                "sidebar state: keeping the saved {:?} state; that edge is not in the config now",
                bar.edge
            );
            state.bars.push(bar.clone());
        }
    }
    state
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

/// Advance the caller's "config seen so far this session" snapshot.
///
/// An upsert, not a replace: `incoming`'s entries win for the edges it
/// declares, and entries for edges it does NOT declare are carried forward.
/// That is what makes [`apply_on_reload`] read a re-added `[[sidebar]]` block
/// as unchanged rather than brand new -- commenting a block out and back in
/// types nothing new, so the width the user dragged has to come back.
///
/// Replacing the snapshot instead loses the edge's last-seen VALUES, and
/// without them there is nothing to diff a re-added block against.
pub fn merge_seen_config(seen: &[SidebarConfig], incoming: &[SidebarConfig]) -> Vec<SidebarConfig> {
    let mut out = incoming.to_vec();
    for sc in seen {
        if !out.iter().any(|c| c.edge == sc.edge) {
            out.push(sc.clone());
        }
    }
    out
}

/// Overlay persisted state onto a config-built `Chrome` at HOT-RELOAD time.
///
/// See the module docs for why this is not [`apply`]. Per edge, per field: a
/// config value the user just changed wins, a config value they left alone
/// yields to whatever they set at runtime. An edge with no entry in `old_cfg`
/// is brand new and keeps its config values untouched -- and `old_cfg` is
/// every edge seen SO FAR this session (see [`merge_seen_config`]), not just
/// the previous file's.
///
/// `chrome` must already be built from `new_cfg`, so its fields hold the new
/// config values before this runs -- that is what makes "unchanged" mean
/// "restore the persisted value" rather than "leave whatever is there".
pub fn apply_on_reload(
    chrome: &mut Chrome,
    state: &SidebarState,
    old_cfg: &[SidebarConfig],
    new_cfg: &[SidebarConfig],
) {
    for sb in &mut chrome.sidebars {
        // `Chrome::from_config` resolves an edge to its FIRST config entry and
        // drops the rest, so both lookups have to do the same.
        let Some(old) = old_cfg.iter().find(|c| c.edge == sb.edge) else {
            log::debug!(
                "sidebar state: the {:?} edge is new in this config; taking it wholesale",
                sb.edge
            );
            continue;
        };
        let Some(new) = new_cfg.iter().find(|c| c.edge == sb.edge) else {
            continue;
        };
        let Some(bar) = state.bars.iter().find(|b| b.edge == sb.edge) else {
            continue;
        };
        if old.size == new.size {
            sb.size = bar.size;
        }
        if old.visible == new.visible {
            sb.visible = bar.visible;
        }
        // Weights are compared as a LIST: a panel added, removed or reordered
        // is itself something the user just typed, and the saved weights no
        // longer describe the stack they were written for.
        let old_weights: Vec<u16> = old.panel.iter().map(|p| p.weight).collect();
        let new_weights: Vec<u16> = new.panel.iter().map(|p| p.weight).collect();
        if old_weights == new_weights {
            for (panel, weight) in sb.panels.iter_mut().zip(bar.weights.iter()) {
                panel.weight = *weight;
            }
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
    fn merge_seen_config_upserts_the_incoming_values() {
        let seen = cfg(30, true, &[1]);
        let incoming = cfg(50, false, &[2]);
        let out = merge_seen_config(&seen, &incoming);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].size, out[0].visible), (50, false));
        assert_eq!(out[0].panel[0].weight, 2);
    }

    #[test]
    fn merge_seen_config_remembers_an_edge_the_new_config_dropped() {
        // The whole reason this is an upsert. Replaced instead, a re-added
        // block has no last-seen values to diff against and reads as brand
        // new, so it comes back at its config default rather than the width
        // the user dragged.
        let seen = cfg(30, true, &[1]);
        let out = merge_seen_config(&seen, &[]);
        assert_eq!(out.len(), 1, "the dropped edge was forgotten");
        assert_eq!(out[0].edge, SidebarEdge::Left);
        assert_eq!(out[0].size, 30);
    }

    #[test]
    fn merge_seen_config_adds_an_edge_seen_for_the_first_time() {
        let out = merge_seen_config(&[], &cfg(30, true, &[1]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].edge, SidebarEdge::Left);
    }

    #[test]
    fn saving_keeps_state_for_an_edge_the_config_no_longer_declares() {
        // Comment a `[[sidebar]]` block out to try something without it, put it
        // back, and the width you dragged has to still be there. Writing
        // `from_chrome` verbatim destroys it, and the reload arm now saves --
        // so this is reachable without the user touching a sidebar at all.
        let previous = SidebarState {
            bars: vec![
                BarState {
                    edge: SidebarEdge::Left,
                    visible: true,
                    size: 30,
                    weights: vec![1],
                },
                BarState {
                    edge: SidebarEdge::Right,
                    visible: false,
                    size: 55,
                    weights: vec![3],
                },
            ],
        };
        // A chrome with only the left edge: the right one is gone from config.
        let c = Chrome::from_config(&cfg(30, true, &[1]));
        let out = merged(&c, &previous);

        let right = out
            .bars
            .iter()
            .find(|b| b.edge == SidebarEdge::Right)
            .expect("the dropped edge's state was erased");
        assert_eq!((right.size, right.visible), (55, false));
        // ...and the live edge still reports what the chrome actually has.
        let left = out
            .bars
            .iter()
            .find(|b| b.edge == SidebarEdge::Left)
            .expect("the live edge is missing");
        assert_eq!(left.size, 30);
    }

    #[test]
    fn saving_does_not_resurrect_a_stale_entry_over_the_live_one() {
        // The carried-forward entries must never shadow an edge that IS in the
        // chrome, or a dragged width could be overwritten by an older one.
        let previous = SidebarState {
            bars: vec![BarState {
                edge: SidebarEdge::Left,
                visible: false,
                size: 99,
                weights: vec![8],
            }],
        };
        let mut c = Chrome::from_config(&cfg(30, true, &[1]));
        c.sidebars[0].size = 42;
        let out = merged(&c, &previous);
        assert_eq!(out.bars.len(), 1, "the stale entry was appended anyway");
        assert_eq!(out.bars[0].size, 42);
        assert!(out.bars[0].visible);
    }

    // -- apply_on_reload: "what you typed wins, what you didn't keeps what
    //    you dragged" ------------------------------------------------------

    /// One left sidebar, parameterised on the fields the rule turns on.
    fn cfg(size: u16, visible: bool, weights: &[u16]) -> Vec<SidebarConfig> {
        vec![SidebarConfig {
            edge: SidebarEdge::Left,
            size,
            visible,
            panel: weights
                .iter()
                .map(|w| PanelConfig {
                    plugin: "placeholder".into(),
                    weight: *w,
                })
                .collect(),
        }]
    }

    /// What the user dragged out at runtime: a wider, hidden bar with the
    /// weights swapped, so every field differs from every config below.
    fn dragged() -> SidebarState {
        SidebarState {
            bars: vec![BarState {
                edge: SidebarEdge::Left,
                visible: false,
                size: 44,
                weights: vec![7, 9],
            }],
        }
    }

    fn reloaded(old: &[SidebarConfig], new: &[SidebarConfig], state: &SidebarState) -> Chrome {
        let mut c = Chrome::from_config(new);
        apply_on_reload(&mut c, state, old, new);
        c
    }

    #[test]
    fn an_untouched_size_keeps_the_width_the_user_dragged() {
        // The config's `size` did not change, so the runtime width stands --
        // this is what stops an unrelated edit from snapping a hand-dragged
        // sidebar back to its config default.
        let old = cfg(30, true, &[2, 1]);
        let new = cfg(30, true, &[2, 1]);
        let c = reloaded(&old, &new, &dragged());
        assert_eq!(c.sidebars[0].size, 44, "the dragged width was reverted");
    }

    #[test]
    fn a_size_the_user_just_typed_wins_over_the_persisted_one() {
        // The exact complaint this rule exists for: `sidebar.json` holds a
        // size from the first resize onward, and a startup-style overlay makes
        // every later `size = ...` in the config dead.
        let old = cfg(30, true, &[2, 1]);
        let new = cfg(50, true, &[2, 1]);
        let c = reloaded(&old, &new, &dragged());
        assert_eq!(c.sidebars[0].size, 50, "the typed width lost to the state");
    }

    #[test]
    fn a_visible_flip_in_the_config_wins_over_the_persisted_one() {
        let old = cfg(30, true, &[2, 1]);
        let new = cfg(30, false, &[2, 1]);
        let mut state = dragged();
        state.bars[0].visible = true;
        let c = reloaded(&old, &new, &state);
        assert!(!c.sidebars[0].visible, "the typed visibility lost");
    }

    #[test]
    fn an_untouched_visible_keeps_the_runtime_state() {
        let old = cfg(30, true, &[2, 1]);
        let new = cfg(30, true, &[2, 1]);
        let c = reloaded(&old, &new, &dragged());
        assert!(
            !c.sidebars[0].visible,
            "a sidebar the user closed reopened on an unrelated edit"
        );
    }

    #[test]
    fn untouched_weights_keep_the_runtime_ones_and_typed_weights_win() {
        let old = cfg(30, true, &[2, 1]);
        let same = reloaded(&old, &cfg(30, true, &[2, 1]), &dragged());
        assert_eq!(
            (
                same.sidebars[0].panels[0].weight,
                same.sidebars[0].panels[1].weight
            ),
            (7, 9),
            "unchanged weights did not keep the runtime values"
        );
        let typed = reloaded(&old, &cfg(30, true, &[3, 1]), &dragged());
        assert_eq!(
            (
                typed.sidebars[0].panels[0].weight,
                typed.sidebars[0].panels[1].weight
            ),
            (3, 1),
            "typed weights lost to the persisted ones"
        );
    }

    #[test]
    fn a_panel_added_to_the_stack_counts_as_a_typed_weight_change() {
        // The saved weights describe a stack that no longer exists, so they do
        // not get to apply positionally to a different one.
        let old = cfg(30, true, &[2, 1]);
        let new = cfg(30, true, &[2, 1, 1]);
        let c = reloaded(&old, &new, &dragged());
        let got: Vec<u16> = c.sidebars[0].panels.iter().map(|p| p.weight).collect();
        assert_eq!(got, vec![2, 1, 1], "stale weights applied to a new stack");
    }

    #[test]
    fn a_brand_new_edge_takes_its_config_wholesale() {
        // Stale state for an edge the previous config never declared must not
        // reach through and resize or hide a sidebar the user just added.
        let old: Vec<SidebarConfig> = vec![];
        let new = cfg(30, true, &[2, 1]);
        let c = reloaded(&old, &new, &dragged());
        assert_eq!(c.sidebars[0].size, 30, "stale state sized a new sidebar");
        assert!(c.sidebars[0].visible, "stale state hid a new sidebar");
        let got: Vec<u16> = c.sidebars[0].panels.iter().map(|p| p.weight).collect();
        assert_eq!(got, vec![2, 1], "stale state reweighted a new sidebar");
    }

    #[test]
    fn the_old_config_must_advance_across_two_reloads() {
        // The regression this pins: if the caller keeps diffing against the
        // STARTUP config, the second edit of the same field still reads as
        // "changed" forever and the runtime value can never win again.
        let startup = cfg(30, true, &[2, 1]);
        let first = cfg(50, true, &[2, 1]);

        // Reload 1: the user typed 50, so 50 wins over the persisted 44.
        let mut chrome = Chrome::from_config(&first);
        let mut state = dragged();
        apply_on_reload(&mut chrome, &state, &startup, &first);
        assert_eq!(chrome.sidebars[0].size, 50);

        // The user then drags it to 60, which persists.
        chrome.sidebars[0].size = 60;
        state = SidebarState::from_chrome(&chrome);

        // Reload 2: an unrelated edit, `size` unchanged since reload 1, so the
        // dragged 60 must survive. Diffed against `startup` instead of
        // `first`, `size` would read as changed (30 -> 50) and snap back.
        let second = cfg(50, false, &[2, 1]);
        let mut chrome2 = Chrome::from_config(&second);
        apply_on_reload(&mut chrome2, &state, &first, &second);
        assert_eq!(
            chrome2.sidebars[0].size, 60,
            "the old-config snapshot did not advance; a dragged width was lost"
        );
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

    /// A duplicate edge is dropped at config time, and a `sidebar.json` written
    /// before that rule existed must not resurrect it.
    ///
    /// This test used to assert the opposite -- that two left sidebars were
    /// matched positionally. `panel_rects` does stack them, but nothing can
    /// ADDRESS the inner one: `sidebar_on` / `toggle_edge` / `focus_edge` all
    /// resolve an edge to its first match. So the config now keeps one per edge,
    /// and the surplus saved entry is ignored rather than applied to a sidebar
    /// on some other edge.
    #[test]
    fn a_duplicate_edge_is_dropped_and_its_saved_state_ignored() {
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
        assert_eq!(
            c.sidebars.len(),
            1,
            "the second left sidebar must be dropped"
        );

        // State written when duplicates were allowed: two left bars.
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
        assert_eq!(c.sidebars.len(), 1);
        assert_eq!(c.sidebars[0].size, 11, "the first saved bar still applies");
        assert!(
            c.sidebars[0].visible,
            "the orphaned second bar must not leak its state onto the survivor"
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
