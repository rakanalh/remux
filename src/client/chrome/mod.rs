//! Client-side terminal chrome: sidebars around the server's content area.

pub mod geometry;

// Nothing outside the geometry tests consumes these yet -- the rendering and
// plugin layers that import them from `chrome::` land in later tasks.
#[allow(unused_imports)]
pub use geometry::{
    content_rect, effective_sizes, pane_area, panel_rects, PanelGeom, SidebarEdge, SidebarGeom,
    MIN_CONTENT_COLS, MIN_CONTENT_ROWS,
};
