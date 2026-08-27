//! Client-side terminal chrome: sidebars around the server's content area.

pub mod geometry;

// Re-exported as the layers above consume them; a name that nothing outside
// `geometry` imports yet trips `unused_imports`, so it is added when its first
// consumer lands rather than up front.
pub use geometry::SidebarEdge;
