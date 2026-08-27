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
use crate::server::layout::{FocusDirection, Rect};

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

impl Chrome {
    /// Toggle a sidebar's visibility. Returns `true` if anything changed.
    ///
    /// Hiding the sidebar that currently holds focus returns focus to the
    /// content -- otherwise the keyboard would be trapped in a panel that is
    /// no longer on screen.
    pub fn toggle_edge(&mut self, edge: SidebarEdge) -> bool {
        let Some(i) = self.sidebars.iter().position(|s| s.edge == edge) else {
            return false;
        };
        self.sidebars[i].visible = !self.sidebars[i].visible;
        if !self.sidebars[i].visible {
            if let ChromeFocus::Sidebar { sidebar, panel } = self.focus {
                if sidebar == i {
                    self.sidebars[i].focused_panel = panel;
                    self.focus = ChromeFocus::Content;
                }
            }
        }
        true
    }

    /// Focus a sidebar, opening it first if hidden. Returns `false` when there
    /// is no such sidebar, or when it cannot be laid out at this terminal size.
    ///
    /// The size check is not cosmetic: `effective_sizes` force-hides a sidebar
    /// that would leave too little content, and a panel dropped for being below
    /// its `min_size` is not painted either. Focusing one of those would swallow
    /// every keystroke into a panel nobody can see.
    pub fn focus_edge(&mut self, edge: SidebarEdge, term_cols: u16, term_rows: u16) -> bool {
        let Some(i) = self.sidebars.iter().position(|s| s.edge == edge) else {
            log::debug!("sidebar: focus_edge({edge:?}) -- no sidebar on that edge");
            return false;
        };
        let was_visible = self.sidebars[i].visible;
        self.sidebars[i].visible = true;
        let rects = self.panel_rects(term_cols, term_rows);
        let mut mine = rects.iter().filter(|(s, _, _)| *s == i).map(|(_, p, _)| *p);
        let Some(first) = mine.next() else {
            // Logged rather than silent: to the user this looks like a dropped
            // keypress, and `client.log` is where that gets diagnosed.
            log::debug!(
                "sidebar: focus_edge({edge:?}) refused -- no panel is laid out at                  {term_cols}x{term_rows}; leaving focus on the content"
            );
            self.sidebars[i].visible = was_visible;
            return false;
        };
        // Return to the panel the user last left, when it is still laid out.
        let want = self.sidebars[i].focused_panel;
        let panel = if rects.iter().any(|(s, p, _)| *s == i && *p == want) {
            want
        } else {
            first
        };
        self.focus = ChromeFocus::Sidebar { sidebar: i, panel };
        self.sidebars[i].focused_panel = panel;
        true
    }

    /// Cycle focus through every visible panel, then back to the content area.
    pub fn cycle_focus(&mut self, term_cols: u16, term_rows: u16) {
        let rects = self.panel_rects(term_cols, term_rows);
        if rects.is_empty() {
            self.focus = ChromeFocus::Content;
            return;
        }
        self.focus = match self.focus {
            ChromeFocus::Content => {
                let (s, p, _) = rects[0];
                ChromeFocus::Sidebar {
                    sidebar: s,
                    panel: p,
                }
            }
            ChromeFocus::Sidebar { sidebar, panel } => {
                let at = rects
                    .iter()
                    .position(|(s, p, _)| *s == sidebar && *p == panel);
                match at {
                    Some(i) if i + 1 < rects.len() => {
                        let (s, p, _) = rects[i + 1];
                        ChromeFocus::Sidebar {
                            sidebar: s,
                            panel: p,
                        }
                    }
                    _ => ChromeFocus::Content,
                }
            }
        };
        if let ChromeFocus::Sidebar { sidebar, panel } = self.focus {
            self.sidebars[sidebar].focused_panel = panel;
        }
    }

    /// The focused panel's `(sidebar, panel)` indices, but only while that
    /// panel is actually laid out.
    ///
    /// Focus is plain indices, so a resize that force-hides a sidebar or drops
    /// an undersized panel can leave it pointing at something not on screen.
    /// Every keyboard path goes through this rather than indexing `sidebars`
    /// directly.
    pub fn focused_panel(&self, term_cols: u16, term_rows: u16) -> Option<(usize, usize)> {
        let ChromeFocus::Sidebar { sidebar, panel } = self.focus else {
            return None;
        };
        self.panel_rects(term_cols, term_rows)
            .into_iter()
            .find(|(s, p, _)| *s == sidebar && *p == panel)
            .map(|(s, p, _)| (s, p))
    }

    /// Leave the sidebar for the content area, remembering the panel we left
    /// so a later `focus_edge` comes back to it.
    pub fn leave_sidebar(&mut self) {
        if let ChromeFocus::Sidebar { sidebar, panel } = self.focus {
            self.sidebars[sidebar].focused_panel = panel;
        }
        self.focus = ChromeFocus::Content;
    }
}

/// The largest border inset the server can apply to a pane on one side.
///
/// `focused_pane_rect` is the pane's interior, so an edge pane's reported rect
/// is short of the pane area by the border width. Both border styles inset by
/// at most one cell per side (zellij-style: 1 all round when the pane is big
/// enough; tmux-style: 1 row at the top when the pane has a stack tab bar), and
/// a pane that is genuinely NOT at an edge is a whole neighbouring pane away --
/// far more than one cell -- so a one-cell tolerance cannot confuse the two.
const BORDER_INSET: u16 = 1;

/// Intercept a directional focus command. Returns `true` when the command was
/// consumed by the chrome and must NOT be forwarded to the server.
///
/// Edge tests run against `pane_area` -- the content rect minus the status-bar
/// row -- because `status_bar_position` is configurable, and using the content
/// rect directly is off by one for whichever setting is not in use.
///
/// Nothing inside a sidebar ever falls through to the server: a direction with
/// nowhere to go is a swallowed no-op, never a leaked `PaneFocus*`.
pub fn intercept_focus(
    chrome: &mut Chrome,
    dir: FocusDirection,
    pane_rect: Option<&crate::protocol::PaneRect>,
    term_cols: u16,
    term_rows: u16,
    status_bar: &StatusBarPosition,
) -> bool {
    use FocusDirection::*;

    match chrome.focus {
        ChromeFocus::Sidebar { sidebar, panel } => {
            let edge = chrome.sidebars[sidebar].edge;
            let toward_content = matches!(
                (edge, &dir),
                (SidebarEdge::Left, Right) | (SidebarEdge::Right, Left) | (SidebarEdge::Bottom, Up)
            );
            if toward_content {
                chrome.leave_sidebar();
                return true;
            }
            // Along the stack axis: vertical sidebars stack vertically, the
            // bottom sidebar stacks horizontally.
            let along_stack = match edge {
                SidebarEdge::Left | SidebarEdge::Right => matches!(dir, Up | Down),
                SidebarEdge::Bottom => matches!(dir, Left | Right),
            };
            if along_stack {
                let n = chrome.sidebars[sidebar].panels.len();
                let next = if matches!(dir, Down | Right) {
                    if panel + 1 < n {
                        panel + 1
                    } else {
                        panel
                    }
                } else {
                    panel.saturating_sub(1)
                };
                chrome.focus = ChromeFocus::Sidebar {
                    sidebar,
                    panel: next,
                };
                chrome.sidebars[sidebar].focused_panel = next;
            }
            // Everything else inside a sidebar is a swallowed no-op: never
            // fall through to another sidebar, never reach the server.
            true
        }
        ChromeFocus::Content => {
            let Some(rect) = pane_rect else {
                // Never guess: forward.
                return false;
            };
            let pa = chrome.pane_area(term_cols, term_rows, status_bar);
            // `pane_rect` is content-relative; `pane_area` is screen-absolute.
            // Compare in content-relative space.
            let content = chrome.content_rect(term_cols, term_rows);
            let pa_rel_y = pa.y.saturating_sub(content.y);
            // The server reports the pane's INTERIOR (see the `focused_pane_rect`
            // construction in `build_composite`: the pane rect inset by the
            // border), so an edge pane's interior sits BORDER_INSET cells inside
            // the pane area, not flush against it. Testing for equality here is
            // how this silently never fires with borders on -- the default.
            let at_edge = match dir {
                Left => rect.x <= BORDER_INSET,
                Right => rect.x + rect.width + BORDER_INSET >= pa.width,
                Up => rect.y <= pa_rel_y + BORDER_INSET,
                Down => rect.y + rect.height + BORDER_INSET >= pa_rel_y + pa.height,
            };
            if !at_edge {
                return false;
            }
            let edge = match dir {
                Left => SidebarEdge::Left,
                Right => SidebarEdge::Right,
                Down => SidebarEdge::Bottom,
                // No `Top` variant exists; the request asked for left, right
                // and bottom. Written generically so adding one costs nothing.
                Up => return false,
            };
            let Some(si) = chrome.sidebar_on(edge, term_cols, term_rows) else {
                return false;
            };
            // Enter the panel nearest the departing pane along the stack axis.
            let rects = chrome.panel_rects(term_cols, term_rows);
            let mine: Vec<_> = rects.iter().filter(|(s, _, _)| *s == si).collect();
            let Some(&&(_, fallback, _)) = mine.first() else {
                // The sidebar takes space but every panel was dropped for being
                // below its minimum: there is nothing to focus. Forward.
                return false;
            };
            let pi = match edge {
                SidebarEdge::Left | SidebarEdge::Right => {
                    let center = content.y + rect.y + rect.height / 2;
                    mine.iter()
                        .find(|(_, _, r)| center >= r.y && center < r.y + r.height)
                        .map(|(_, p, _)| *p)
                        .unwrap_or(fallback)
                }
                SidebarEdge::Bottom => {
                    let center = content.x + rect.x + rect.width / 2;
                    mine.iter()
                        .find(|(_, _, r)| center >= r.x && center < r.x + r.width)
                        .map(|(_, p, _)| *p)
                        .unwrap_or(fallback)
                }
            };
            chrome.focus = ChromeFocus::Sidebar {
                sidebar: si,
                panel: pi,
            };
            chrome.sidebars[si].focused_panel = pi;
            true
        }
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;
    use crate::config::sidebar::{PanelConfig, SidebarConfig};
    use crate::protocol::PaneRect;
    use crate::server::layout::FocusDirection;

    fn chrome_with(edge: SidebarEdge, size: u16) -> Chrome {
        Chrome::from_config(&[SidebarConfig {
            edge,
            size,
            visible: true,
            panel: vec![PanelConfig {
                plugin: "placeholder".into(),
                weight: 1,
            }],
        }])
    }

    fn chrome_with_panels(edge: SidebarEdge, size: u16, n: usize) -> Chrome {
        Chrome::from_config(&[SidebarConfig {
            edge,
            size,
            visible: true,
            panel: (0..n)
                .map(|_| PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 1,
                })
                .collect(),
        }])
    }

    #[test]
    fn toggling_a_sidebar_off_releases_the_keyboard() {
        let mut c = chrome_with(SidebarEdge::Left, 30);
        assert!(c.focus_edge(SidebarEdge::Left, 100, 30));
        assert!(c.toggle_edge(SidebarEdge::Left));
        assert!(!c.sidebars[0].visible);
        assert_eq!(
            c.focus,
            ChromeFocus::Content,
            "hiding the focused sidebar left the keyboard in an invisible panel"
        );
    }

    #[test]
    fn toggling_an_edge_with_no_sidebar_changes_nothing() {
        let mut c = chrome_with(SidebarEdge::Left, 30);
        assert!(!c.toggle_edge(SidebarEdge::Bottom));
        assert!(c.sidebars[0].visible);
    }

    #[test]
    fn focus_edge_reopens_a_hidden_sidebar() {
        let mut c = chrome_with(SidebarEdge::Left, 30);
        c.sidebars[0].visible = false;
        assert!(c.focus_edge(SidebarEdge::Left, 100, 30));
        assert!(c.sidebars[0].visible);
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 0
            }
        );
    }

    #[test]
    fn focus_edge_returns_to_the_panel_you_left() {
        let mut c = chrome_with_panels(SidebarEdge::Left, 30, 3);
        c.focus = ChromeFocus::Sidebar {
            sidebar: 0,
            panel: 2,
        };
        c.leave_sidebar();
        assert_eq!(c.focus, ChromeFocus::Content);
        assert!(c.focus_edge(SidebarEdge::Left, 100, 30));
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 2
            },
            "re-entering reset to the first panel instead of the remembered one"
        );
    }

    #[test]
    fn focus_edge_refuses_a_sidebar_too_wide_for_the_terminal() {
        // 18 columns cannot hold a 30-wide sidebar AND MIN_CONTENT_COLS, so
        // `effective_sizes` force-hides it. Focusing it anyway would swallow
        // every keystroke into a panel that is not on screen.
        let mut c = chrome_with(SidebarEdge::Left, 30);
        c.sidebars[0].visible = false;
        assert!(!c.focus_edge(SidebarEdge::Left, 18, 30));
        assert_eq!(c.focus, ChromeFocus::Content);
        assert!(!c.sidebars[0].visible, "visibility was not restored");
    }

    #[test]
    fn cycle_walks_every_panel_then_returns_to_the_content() {
        let mut c = Chrome::from_config(&[
            SidebarConfig {
                edge: SidebarEdge::Left,
                size: 30,
                visible: true,
                panel: vec![
                    PanelConfig {
                        plugin: "placeholder".into(),
                        weight: 1,
                    },
                    PanelConfig {
                        plugin: "placeholder".into(),
                        weight: 1,
                    },
                ],
            },
            SidebarConfig {
                edge: SidebarEdge::Right,
                size: 20,
                visible: true,
                panel: vec![PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 1,
                }],
            },
        ]);
        let mut seen = Vec::new();
        for _ in 0..4 {
            c.cycle_focus(120, 30);
            seen.push(c.focus);
        }
        assert_eq!(
            seen,
            vec![
                ChromeFocus::Sidebar {
                    sidebar: 0,
                    panel: 0
                },
                ChromeFocus::Sidebar {
                    sidebar: 0,
                    panel: 1
                },
                ChromeFocus::Sidebar {
                    sidebar: 1,
                    panel: 0
                },
                ChromeFocus::Content,
            ]
        );
    }

    #[test]
    fn cycle_with_no_sidebar_stays_on_the_content() {
        let mut c = Chrome::from_config(&[]);
        c.cycle_focus(100, 30);
        assert_eq!(c.focus, ChromeFocus::Content);
    }

    #[test]
    fn a_focus_stranded_by_a_resize_is_not_reported_as_focused() {
        // The sidebar fits at 100 columns and is force-hidden at 18. Focus is
        // plain indices, so it survives the resize pointing at a panel that is
        // no longer laid out; every keyboard path must see `None` here rather
        // than index into `sidebars` and feed keys to an invisible panel.
        let mut c = chrome_with(SidebarEdge::Left, 30);
        assert!(c.focus_edge(SidebarEdge::Left, 100, 30));
        assert_eq!(c.focused_panel(100, 30), Some((0, 0)));
        assert_eq!(c.focused_panel(18, 30), None);
    }

    #[test]
    fn the_bottom_edge_test_uses_the_pane_area_not_the_content_rect() {
        // The discriminating case for `pane_area`. Content is 25 tall (30 rows
        // minus a 5-row bottom sidebar); with the status bar on the last row the
        // pane area is rows 0..24, so the bottom pane's INTERIOR ends at row 23
        // (24 minus its border). Measuring against the content rect instead
        // looks for row 24 and misses -- `Alt+j` would never enter the sidebar.
        let mut c = chrome_with(SidebarEdge::Bottom, 5);
        let pane = PaneRect {
            x: 1,
            y: 1,
            width: 98,
            height: 22,
        };
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Down,
            Some(&pane),
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 0
            }
        );
    }

    #[test]
    fn a_bordered_edge_pane_still_counts_as_being_at_the_edge() {
        // The server reports the pane's INTERIOR, so with the default border an
        // edge pane's rect starts at x=1, never x=0. Testing for equality is how
        // this silently never fires in the configuration everyone actually runs.
        let mut c = chrome_with(SidebarEdge::Left, 30);
        let pane = PaneRect {
            x: 1,
            y: 1,
            width: 68,
            height: 27,
        };
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Left,
            Some(&pane),
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 0
            }
        );
    }

    #[test]
    fn the_border_tolerance_does_not_swallow_a_real_neighbour() {
        // The mirror of the test above: a one-cell tolerance must not turn the
        // RIGHT half of a split into "at the left edge". Bordered rects, as the
        // server actually reports them.
        let mut c = chrome_with(SidebarEdge::Left, 30);
        let right_half = PaneRect {
            x: 36,
            y: 1,
            width: 33,
            height: 27,
        };
        assert!(!intercept_focus(
            &mut c,
            FocusDirection::Left,
            Some(&right_half),
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(c.focus, ChromeFocus::Content);
        // ... and the LEFT half of the same split is not "at the right edge".
        let left_half = PaneRect {
            x: 1,
            y: 1,
            width: 33,
            height: 27,
        };
        assert!(!intercept_focus(
            &mut c,
            FocusDirection::Right,
            Some(&left_half),
            100,
            30,
            &StatusBarPosition::Bottom
        ));
    }

    #[test]
    fn a_pane_entering_a_stacked_sidebar_lands_on_the_panel_beside_it() {
        // Two stacked panels over 30 rows: panel 0 is rows 0..15, panel 1 is
        // 15..30. A pane in the LOWER half of the content must enter panel 1.
        let mut c = chrome_with_panels(SidebarEdge::Left, 30, 2);
        let lower = PaneRect {
            x: 1,
            y: 16,
            width: 68,
            height: 12,
        };
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Left,
            Some(&lower),
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 1
            }
        );
    }

    #[test]
    fn moving_left_from_the_leftmost_pane_enters_a_left_sidebar() {
        let mut c = chrome_with(SidebarEdge::Left, 30);
        // Content is 70 wide at x=30; pane_area drops the bottom status row.
        let pane = PaneRect {
            x: 0,
            y: 0,
            width: 35,
            height: 29,
        };
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Left,
            Some(&pane),
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 0
            }
        );
    }

    #[test]
    fn moving_left_from_a_non_edge_pane_is_forwarded() {
        let mut c = chrome_with(SidebarEdge::Left, 30);
        let pane = PaneRect {
            x: 35,
            y: 0,
            width: 35,
            height: 29,
        };
        assert!(!intercept_focus(
            &mut c,
            FocusDirection::Left,
            Some(&pane),
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(c.focus, ChromeFocus::Content);
    }

    #[test]
    fn moving_left_with_no_left_sidebar_is_forwarded() {
        let mut c = chrome_with(SidebarEdge::Right, 20);
        let pane = PaneRect {
            x: 0,
            y: 0,
            width: 80,
            height: 29,
        };
        assert!(!intercept_focus(
            &mut c,
            FocusDirection::Left,
            Some(&pane),
            100,
            30,
            &StatusBarPosition::Bottom
        ));
    }

    #[test]
    fn an_unknown_pane_rect_is_always_forwarded_never_guessed() {
        let mut c = chrome_with(SidebarEdge::Left, 30);
        assert!(!intercept_focus(
            &mut c,
            FocusDirection::Left,
            None,
            100,
            30,
            &StatusBarPosition::Bottom
        ));
    }

    #[test]
    fn the_bottom_edge_test_accounts_for_a_top_status_bar() {
        // Spec assertion 14: with the status bar on top, pane_area starts at
        // y=1, so a pane ending at the terminal's last row is still the bottom
        // edge and must enter the bottom sidebar.
        let mut c = chrome_with(SidebarEdge::Bottom, 5);
        // Content is 25 tall; with a top status bar pane_area is y=1..25.
        let pane = PaneRect {
            x: 0,
            y: 1,
            width: 100,
            height: 24,
        };
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Down,
            Some(&pane),
            100,
            30,
            &StatusBarPosition::Top
        ));
    }

    #[test]
    fn leaving_a_left_sidebar_rightwards_returns_to_content() {
        let mut c = chrome_with(SidebarEdge::Left, 30);
        c.focus = ChromeFocus::Sidebar {
            sidebar: 0,
            panel: 0,
        };
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Right,
            None,
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(c.focus, ChromeFocus::Content);
    }

    #[test]
    fn moving_down_inside_a_vertical_sidebar_walks_the_stack() {
        let mut c = Chrome::from_config(&[SidebarConfig {
            edge: SidebarEdge::Left,
            size: 30,
            visible: true,
            panel: vec![
                PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 1,
                },
                PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 1,
                },
            ],
        }]);
        c.focus = ChromeFocus::Sidebar {
            sidebar: 0,
            panel: 0,
        };
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Down,
            None,
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 1
            }
        );
    }

    #[test]
    fn moving_past_the_last_stacked_panel_is_a_swallowed_no_op() {
        let mut c = chrome_with(SidebarEdge::Left, 30);
        c.focus = ChromeFocus::Sidebar {
            sidebar: 0,
            panel: 0,
        };
        // Swallowed (never forwarded to the server) but focus does not move.
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Down,
            None,
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 0
            }
        );
    }

    #[test]
    fn moving_further_left_inside_a_left_sidebar_is_a_swallowed_no_op() {
        let mut c = chrome_with(SidebarEdge::Left, 30);
        c.focus = ChromeFocus::Sidebar {
            sidebar: 0,
            panel: 0,
        };
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Left,
            None,
            100,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 0
            }
        );
    }
}
