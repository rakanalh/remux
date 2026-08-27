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
