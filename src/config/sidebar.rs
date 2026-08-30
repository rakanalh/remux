//! `[[sidebar]]` configuration.
//!
//! An unknown `plugin` name is NOT rejected here -- it is resolved against the
//! plugin registry at construction time, where it logs a warning and is
//! skipped, so a config written for a later phase still loads on this build.
//! An unknown `edge`, by contrast, is a typo with no forward-compatible
//! reading, so serde rejects it.

use serde::Deserialize;

use crate::client::chrome::SidebarEdge;

fn default_visible() -> bool {
    true
}

fn default_weight() -> u16 {
    1
}

/// One plugin panel inside a sidebar.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PanelConfig {
    /// Registry name of the plugin, e.g. `"sessions"`.
    pub plugin: String,
    /// Share of the sidebar this panel claims relative to its siblings.
    #[serde(default = "default_weight")]
    pub weight: u16,
    /// The editor the `files` panel should open a file with, overriding the
    /// server's own `$EDITOR`. Optional, and normally absent -- the server
    /// already knows `$EDITOR`, and it is the server's answer that matters
    /// because the editor has to exist where the FILE is.
    ///
    /// Ignored by every other plugin.
    #[serde(default)]
    pub editor: Option<String>,
    /// DEPRECATED and ignored. Parsed only so that a config still carrying it
    /// gets a warning naming [`PanelConfig::editor`] instead of silence.
    ///
    /// It used to mean two different things: the FILE MANAGER to run to the old
    /// `files` plugin (required), and the EDITOR to open a file with to
    /// `browser` (optional). The two panels have since merged, and that
    /// ambiguity is why -- a `command = "nnn"` copied from one to the other
    /// dutifully opened every file in `nnn`. See `make_plugin`, which is where
    /// the warning lives and where the decision NOT to alias it to `editor` is
    /// argued.
    ///
    /// Kept in the struct rather than deleted because serde ignores unknown keys
    /// silently: deleting it would take the warning with it.
    #[serde(default)]
    pub command: Option<String>,
}

#[cfg(test)]
impl PanelConfig {
    /// A minimal entry naming `plugin`, for the many tests that only care about
    /// which plugin a panel is.
    pub(crate) fn named(plugin: &str) -> Self {
        Self {
            plugin: plugin.to_string(),
            weight: 1,
            editor: None,
            command: None,
        }
    }
}

/// One sidebar docked to a terminal edge.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SidebarConfig {
    pub edge: SidebarEdge,
    /// Columns for `left`/`right`, rows for `bottom`.
    pub size: u16,
    #[serde(default = "default_visible")]
    pub visible: bool,
    /// `[[sidebar.panel]]` entries, in stacking order.
    #[serde(default)]
    pub panel: Vec<PanelConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::chrome::SidebarEdge;

    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        sidebar: Vec<SidebarConfig>,
    }

    fn parse(s: &str) -> Vec<SidebarConfig> {
        toml::from_str::<Wrapper>(s)
            .expect("config should parse")
            .sidebar
    }

    #[test]
    fn absent_sidebar_table_yields_none() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn parses_edge_size_and_panels() {
        let cfg = parse(
            r#"
[[sidebar]]
edge = "left"
size = 30
visible = true

  [[sidebar.panel]]
  plugin = "sessions"
  weight = 2

  [[sidebar.panel]]
  plugin = "agents"
  weight = 1
"#,
        );
        assert_eq!(cfg.len(), 1);
        assert_eq!(cfg[0].edge, SidebarEdge::Left);
        assert_eq!(cfg[0].size, 30);
        assert!(cfg[0].visible);
        assert_eq!(cfg[0].panel.len(), 2);
        assert_eq!(cfg[0].panel[0].plugin, "sessions");
        assert_eq!(cfg[0].panel[0].weight, 2);
        assert_eq!(cfg[0].panel[1].plugin, "agents");
        assert_eq!(cfg[0].panel[0].editor, None);
        assert_eq!(cfg[0].panel[0].command, None);
    }

    #[test]
    fn visible_defaults_to_true_and_weight_to_one() {
        let cfg = parse(
            r#"
[[sidebar]]
edge = "bottom"
size = 8

  [[sidebar.panel]]
  plugin = "sessions"
"#,
        );
        assert!(cfg[0].visible);
        assert_eq!(cfg[0].panel[0].weight, 1);
    }

    #[test]
    fn all_three_edges_parse() {
        let cfg = parse(
            r#"
[[sidebar]]
edge = "left"
size = 30
[[sidebar]]
edge = "right"
size = 20
[[sidebar]]
edge = "bottom"
size = 6
"#,
        );
        assert_eq!(cfg[0].edge, SidebarEdge::Left);
        assert_eq!(cfg[1].edge, SidebarEdge::Right);
        assert_eq!(cfg[2].edge, SidebarEdge::Bottom);
    }

    #[test]
    fn unknown_edge_is_a_parse_error() {
        let r = toml::from_str::<Wrapper>(
            r#"
[[sidebar]]
edge = "top"
size = 4
"#,
        );
        assert!(r.is_err(), "an unknown edge must be rejected loudly");
    }

    #[test]
    fn a_panel_editor_is_parsed_and_optional() {
        let cfg = parse(
            r#"
[[sidebar]]
edge = "right"
size = 40

  [[sidebar.panel]]
  plugin = "files"
  editor = "hx"

  [[sidebar.panel]]
  plugin = "sessions"
"#,
        );
        assert_eq!(cfg[0].panel[0].editor.as_deref(), Some("hx"));
        assert_eq!(cfg[0].panel[1].editor, None);
    }

    /// A config still carrying the removed field must LOAD -- rejecting it
    /// would cost the user their whole sidebar over a line that is merely
    /// stale. It is `make_plugin` that warns about it and declines to read it.
    #[test]
    fn a_leftover_command_still_parses_and_is_kept_for_the_warning() {
        let cfg = parse(
            r#"
[[sidebar]]
edge = "right"
size = 40

  [[sidebar.panel]]
  plugin = "files"
  command = "nnn"
"#,
        );
        assert_eq!(cfg[0].panel[0].command.as_deref(), Some("nnn"));
        assert_eq!(
            cfg[0].panel[0].editor, None,
            "`command` must NOT be aliased to `editor`: that is what made a \
             file manager copied from the old `files` plugin open every file in it"
        );
    }

    #[test]
    fn a_sidebar_with_no_panels_parses_and_is_empty() {
        // Config written for a later phase must not break this build.
        let cfg = parse(
            r#"
[[sidebar]]
edge = "left"
size = 30
"#,
        );
        assert!(cfg[0].panel.is_empty());
    }
}
