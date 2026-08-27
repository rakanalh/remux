//! Client-side terminal chrome: sidebars around the server's content area.
//!
//! The server never learns sidebars exist. The client computes a content rect,
//! sends it as `Resize`, blits server frames at its origin, and paints the
//! panels around them.

pub mod geometry;

use anyhow::Result;

// `MIN_CONTENT_COLS`/`MIN_CONTENT_ROWS` are deliberately NOT re-exported: this
// is a binary crate, so a `pub use` nothing outside `geometry` imports trips
// `unused_imports` under `-D warnings`.
pub use geometry::{
    content_rect, effective_sizes, pane_area, panel_rects, PanelGeom, SidebarEdge, SidebarGeom,
};

use crate::client::renderer::Renderer;
use crate::client::sidebar::{make_plugin, PluginEvent, SidebarPlugin};
use crate::config::sidebar::SidebarConfig;
use crate::config::theme::CompositorTheme;
use crate::config::StatusBarPosition;
use crate::server::layout::Rect;

/// One plugin panel plus its layout weight.
pub struct Panel {
    pub plugin: Box<dyn SidebarPlugin>,
    pub weight: u16,
}

/// One sidebar docked to an edge.
pub struct Sidebar {
    pub edge: SidebarEdge,
    pub size: u16,
    pub visible: bool,
    pub panels: Vec<Panel>,
    /// Which panel takes keys when this sidebar is focused. Written by the
    /// navigation task (Task 7); tracked here from the start so focus survives
    /// leaving and re-entering a sidebar.
    pub focused_panel: usize,
}

/// Where keyboard focus currently lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeFocus {
    Content,
    Sidebar { sidebar: usize, panel: usize },
}

pub struct Chrome {
    pub sidebars: Vec<Sidebar>,
    pub focus: ChromeFocus,
}

impl Chrome {
    /// Build from config. An unknown plugin name logs a warning and is skipped
    /// so a config written for a later phase still loads; a sidebar left with
    /// no panels is dropped.
    pub fn from_config(cfg: &[SidebarConfig]) -> Self {
        let mut sidebars = Vec::new();
        for sc in cfg {
            let mut panels = Vec::new();
            for pc in &sc.panel {
                match make_plugin(&pc.plugin) {
                    Some(plugin) => panels.push(Panel {
                        plugin,
                        weight: pc.weight.max(1),
                    }),
                    None => log::warn!(
                        "sidebar: unknown plugin {:?}; skipping this panel",
                        pc.plugin
                    ),
                }
            }
            if panels.is_empty() {
                log::warn!("sidebar: {:?} edge has no usable panels; dropping", sc.edge);
                continue;
            }
            sidebars.push(Sidebar {
                edge: sc.edge,
                size: sc.size,
                visible: sc.visible,
                panels,
                focused_panel: 0,
            });
        }
        Self {
            sidebars,
            focus: ChromeFocus::Content,
        }
    }

    /// Geometry descriptors for the pure layout functions.
    fn geoms(&self) -> Vec<SidebarGeom> {
        self.sidebars
            .iter()
            .map(|s| SidebarGeom {
                edge: s.edge,
                size: s.size,
                visible: s.visible,
                panels: s
                    .panels
                    .iter()
                    .map(|p| {
                        let (min_cols, min_rows) = p.plugin.min_size();
                        PanelGeom {
                            weight: p.weight,
                            min_cols,
                            min_rows,
                        }
                    })
                    .collect(),
            })
            .collect()
    }

    /// The rect handed to the server as `Resize`.
    pub fn content_rect(&self, term_cols: u16, term_rows: u16) -> Rect {
        content_rect(&self.geoms(), term_cols, term_rows)
    }

    /// The content rect minus the status-bar row -- the rect directional edge
    /// tests run against. Consumed by the navigation task (Task 7).
    pub fn pane_area(
        &self,
        term_cols: u16,
        term_rows: u16,
        status_bar: &StatusBarPosition,
    ) -> Rect {
        pane_area(self.content_rect(term_cols, term_rows), status_bar)
    }

    /// Absolute screen rects for every visible panel.
    pub fn panel_rects(&self, term_cols: u16, term_rows: u16) -> Vec<(usize, usize, Rect)> {
        panel_rects(&self.geoms(), term_cols, term_rows)
    }

    /// Whether any sidebar currently occupies space.
    pub fn has_any_visible(&self, term_cols: u16, term_rows: u16) -> bool {
        effective_sizes(&self.geoms(), term_cols, term_rows)
            .iter()
            .any(|s| *s > 0)
    }

    /// Render every visible panel into the renderer's front buffer.
    pub fn paint(
        &self,
        renderer: &mut Renderer,
        term_cols: u16,
        term_rows: u16,
        theme: &CompositorTheme,
    ) -> Result<()> {
        for (si, pi, rect) in self.panel_rects(term_cols, term_rows) {
            let focused = self.focus
                == ChromeFocus::Sidebar {
                    sidebar: si,
                    panel: pi,
                };
            let grid =
                self.sidebars[si].panels[pi]
                    .plugin
                    .render(rect.width, rect.height, focused, theme);
            renderer.paint_panel(rect, &grid)?;
        }
        Ok(())
    }

    /// Broadcast a pushed event to every plugin. Consumed by the session-tree
    /// tasks (Tasks 10 and 12).
    pub fn broadcast(&mut self, ev: &PluginEvent) {
        for s in &mut self.sidebars {
            for p in &mut s.panels {
                p.plugin.on_event(ev);
            }
        }
    }

    /// Index of the first visible sidebar on `edge`, if any. Consumed by the
    /// navigation task (Task 7).
    pub fn sidebar_on(&self, edge: SidebarEdge, term_cols: u16, term_rows: u16) -> Option<usize> {
        let sizes = effective_sizes(&self.geoms(), term_cols, term_rows);
        self.sidebars
            .iter()
            .enumerate()
            .find(|(i, s)| s.edge == edge && sizes[*i] > 0)
            .map(|(i, _)| i)
    }
}
