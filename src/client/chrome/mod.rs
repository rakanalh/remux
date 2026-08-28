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

    /// The smallest `size` that still lets this sidebar's panels render: the
    /// largest per-panel minimum on the axis `size` measures (columns for a
    /// vertical sidebar, rows for the bottom one).
    ///
    /// This does NOT prevent a panel being dropped -- `split_panels` drops on
    /// the STACK axis, which `size` does not touch (a vertical sidebar's
    /// `bar.height` is `term_rows` whatever its width is). It is the simpler
    /// rule it looks like: never paint a plugin narrower (or shorter) than the
    /// size it says it needs. The vanish check in `resize_focused` is what
    /// keeps panels on screen.
    fn min_size(&self, i: usize) -> u16 {
        let vertical = !matches!(self.sidebars[i].edge, SidebarEdge::Bottom);
        self.sidebars[i]
            .panels
            .iter()
            .map(|p| {
                let (min_cols, min_rows) = p.plugin.min_size();
                if vertical {
                    min_cols
                } else {
                    min_rows
                }
            })
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// Re-target a directional resize at the focused sidebar. Returns `true` if
    /// anything actually changed.
    ///
    /// The axis decides which field moves: PERPENDICULAR to the sidebar's edge
    /// is its `size`, PARALLEL to it (the axis its panels stack along) is the
    /// focused panel's `weight`.
    ///
    /// Direction is SPATIAL, not signed by name -- the user is dragging an edge,
    /// not incrementing a number -- so `ResizeRight` grows a left sidebar and
    /// shrinks a right one, and `ResizeDown` grows the focused panel downward
    /// exactly as it grows a focused pane downward on the server.
    pub fn resize_focused(
        &mut self,
        dir: FocusDirection,
        amount: u16,
        term_cols: u16,
        term_rows: u16,
    ) -> bool {
        use FocusDirection::*;
        let Some((i, p)) = self.focused_panel(term_cols, term_rows) else {
            return false;
        };
        let edge = self.sidebars[i].edge;
        let perpendicular = match edge {
            SidebarEdge::Left | SidebarEdge::Right => matches!(dir, Left | Right),
            SidebarEdge::Bottom => matches!(dir, Up | Down),
        };

        if perpendicular {
            let grow = matches!(
                (edge, &dir),
                (SidebarEdge::Left, Right) | (SidebarEdge::Right, Left) | (SidebarEdge::Bottom, Up)
            );
            // Arithmetic starts from the EFFECTIVE size, not the stored one. A
            // sidebar whose stored 30 was granted 25 must move on the first
            // press: shrinking the stored value would land back on 25 and look
            // like a dropped keystroke.
            let current = effective_sizes(&self.geoms(), term_cols, term_rows)[i];
            let want = if grow {
                current.saturating_add(amount)
            } else {
                current.saturating_sub(amount)
            }
            .max(self.min_size(i));

            let old = self.sidebars[i].size;
            let before = self.laid_out_panels(term_cols, term_rows);
            self.sidebars[i].size = want;
            let granted = effective_sizes(&self.geoms(), term_cols, term_rows)[i];
            // Refuse only when the press moves NOTHING. Rewriting the stored
            // size to a value the layout merely clamped would silently discard
            // a width the user chose at a bigger terminal, with nothing moving
            // on screen to explain it.
            //
            // The test is against `current`, not `want`: a partial grant is a
            // real move. Asking for 83 against a ceiling of 80 from a current
            // 78 gives 80, and refusing that because it is not 83 would wedge
            // the sidebar at 78 forever -- `current` is re-read from
            // `effective_sizes` every press, so the same refusal repeats. That
            // trap opens wherever the distance to the ceiling is not a whole
            // number of steps.
            if granted == current {
                self.sidebars[i].size = old;
                return false;
            }
            // Store what was GRANTED, not what was asked: a partial grant must
            // not leave an unreachable number in the config's place, and the
            // vanish check below has to run against the size actually in force.
            self.sidebars[i].size = granted;
            // A size change is not local. `effective_sizes` shares one column
            // budget between the verticals in declaration order, so growing
            // this sidebar eats the NEXT one's -- and a vertical's growth also
            // narrows `content.width`, which IS the bottom sidebar's bar, whose
            // panels are then dropped on `min_cols`. Checking only this
            // sidebar's granted size sees neither.
            if !none_vanished(&before, &self.laid_out_panels(term_cols, term_rows)) {
                self.sidebars[i].size = old;
                return false;
            }
            return granted != old;
        }

        // Along the stack axis: the focused panel's weight. `amount` is a cell
        // count, which means nothing to a proportional weight, so a press is
        // one unit.
        let grow = matches!(dir, Down | Right);
        let old_weight = self.sidebars[i].panels[p].weight;
        let new_weight = if grow {
            old_weight.saturating_add(WEIGHT_STEP)
        } else {
            old_weight.saturating_sub(WEIGHT_STEP).max(1)
        };
        if new_weight == old_weight {
            return false;
        }
        // No panel may vanish because of a resize. `split_panels` drops a panel
        // whose share falls below its minimum, so an unbounded weight change
        // makes a NEIGHBOUR disappear -- or, shrinking, the focused panel
        // itself, which then ejects the user from the sidebar.
        let before = self.laid_out_panels(term_cols, term_rows);
        self.sidebars[i].panels[p].weight = new_weight;
        if !none_vanished(&before, &self.laid_out_panels(term_cols, term_rows)) {
            self.sidebars[i].panels[p].weight = old_weight;
            return false;
        }
        true
    }

    /// The `(sidebar, panel)` pairs currently laid out, for the no-vanish check.
    fn laid_out_panels(&self, term_cols: u16, term_rows: u16) -> Vec<(usize, usize)> {
        self.panel_rects(term_cols, term_rows)
            .into_iter()
            .map(|(s, p, _)| (s, p))
            .collect()
    }
}

/// One press of a directional resize, in weight units.
const WEIGHT_STEP: u16 = 1;

/// Whether every panel laid out before a resize is still laid out after it.
///
/// A SUBSET test, not equality: a panel APPEARING is a resize rescuing one that
/// did not fit, and refusing that would trap the user. A config (or a persisted
/// state, or a terminal that shrank) can leave a sidebar force-hidden or a panel
/// dropped, and shrinking back is exactly how it is recovered -- under an
/// equality test the recovering press would be refused for changing the set.
fn none_vanished(before: &[(usize, usize)], after: &[(usize, usize)]) -> bool {
    before.iter().all(|p| after.contains(p))
}

/// Intercept a directional resize command. Returns `true` when the chrome
/// consumed it and it must NOT be forwarded to the server.
///
/// Consumed means consumed: a resize clamped to a no-op is swallowed, never
/// leaked to the server as a pane resize the user cannot see. With focus on the
/// content -- including every client with no sidebar configured -- this returns
/// `false` and the command forwards exactly as before.
pub fn intercept_resize(
    chrome: &mut Chrome,
    dir: FocusDirection,
    amount: u16,
    term_cols: u16,
    term_rows: u16,
) -> bool {
    // Same stranded-focus release as `intercept_focus`, for the same reason: a
    // SIGWINCH between the prefix and the chord key can force-hide the sidebar
    // while `chrome.focus` still names it.
    if matches!(chrome.focus, ChromeFocus::Sidebar { .. })
        && chrome.focused_panel(term_cols, term_rows).is_none()
    {
        log::debug!("sidebar: resize focus was stranded by a resize; releasing it to the content");
        chrome.leave_sidebar();
    }
    if !matches!(chrome.focus, ChromeFocus::Sidebar { .. }) {
        return false;
    }
    let described = format!("{dir:?}");
    let changed = chrome.resize_focused(dir, amount, term_cols, term_rows);
    log::debug!("sidebar: resize {described} by {amount} -> changed={changed}");
    true
}

/// The largest border inset the server can apply to a pane on one side.
///
/// `focused_pane_rect` is the pane's interior, so an edge pane's reported rect
/// is short of the pane area by the border width. Both border styles inset by
/// at most one cell per side: zellij-style is 1 all round when the pane is big
/// enough, tmux-style is 1 row at the top when the pane has a stack tab bar and
/// 0 columns either side.
///
/// 1 is exactly right and 2 would be a bug, not merely a looser bound.
/// `layout::MIN_PANE_SIZE` is 2, so a 2-column pane is legal; under tmux-style
/// (`x_off == 0`) its RIGHT neighbour's interior then begins at `rect.x == 2`,
/// and a tolerance of 2 would call that non-edge pane "at the left edge" and
/// steal its `PaneFocusLeft`. Widening this constant needs that case rechecked.
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

    // The key path resolves focus through `focused_panel`, but a command can
    // reach here without passing through it -- a SIGWINCH landing between the
    // prefix and the chord key force-hides the sidebar while `chrome.focus`
    // still names it. Release it here too, or `Prefix p h` would be swallowed
    // with no sidebar on screen.
    if matches!(chrome.focus, ChromeFocus::Sidebar { .. })
        && chrome.focused_panel(term_cols, term_rows).is_none()
    {
        log::debug!("sidebar: focus was stranded by a resize; releasing it to the content");
        chrome.leave_sidebar();
    }

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
                // Walk `panel_rects`, not `panels`: `split_panels` DROPS a panel
                // whose share falls below its `min_size`, so the laid-out indices
                // can be non-contiguous and `panel + 1` may name a panel that is
                // never painted. `focus_edge` and `cycle_focus` both walk the
                // rects; this matches them.
                let rects = chrome.panel_rects(term_cols, term_rows);
                let mine: Vec<usize> = rects
                    .iter()
                    .filter(|(s, _, _)| *s == sidebar)
                    .map(|(_, p, _)| *p)
                    .collect();
                let next = match mine.iter().position(|p| *p == panel) {
                    Some(i) if matches!(dir, Down | Right) => {
                        mine.get(i + 1).copied().unwrap_or(panel)
                    }
                    Some(i) => i.checked_sub(1).map(|j| mine[j]).unwrap_or(panel),
                    // Focus is on a panel that is not laid out. The release above
                    // makes this unreachable; stay put rather than guess.
                    None => panel,
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
    fn a_focus_stranded_by_a_resize_is_released_rather_than_swallowing_the_command() {
        // Review minor 2: the key path releases stranded focus via
        // `focused_panel`, but a command can reach `intercept_focus` without
        // passing through it -- a SIGWINCH between the prefix and the chord key.
        // At 18 columns the sidebar is force-hidden, so `Prefix p h` must be
        // FORWARDED (there is no sidebar on screen to enter), not swallowed.
        let mut c = chrome_with(SidebarEdge::Left, 30);
        c.focus = ChromeFocus::Sidebar {
            sidebar: 0,
            panel: 0,
        };
        let pane = PaneRect {
            x: 1,
            y: 1,
            width: 16,
            height: 27,
        };
        assert!(!intercept_focus(
            &mut c,
            FocusDirection::Left,
            Some(&pane),
            18,
            30,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(c.focus, ChromeFocus::Content);
    }

    #[test]
    fn the_stack_walk_skips_a_panel_that_was_dropped_for_being_too_small() {
        // Review minor 3: `split_panels` drops a panel below its `min_size`, so
        // the laid-out indices are non-contiguous. Walking `panels.len()` would
        // move focus onto index 1 -- a panel that is never painted.
        //
        // The placeholder's min is (8, 2). Over 5 rows with weights 10/1/10,
        // the middle panel's share is 0 rows, so it is dropped and only panels
        // 0 and 2 are laid out.
        let mut c = Chrome::from_config(&[SidebarConfig {
            edge: SidebarEdge::Left,
            size: 30,
            visible: true,
            panel: vec![
                PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 10,
                },
                PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 1,
                },
                PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 10,
                },
            ],
        }]);
        let laid_out: Vec<usize> = c
            .panel_rects(100, 5)
            .into_iter()
            .map(|(_, p, _)| p)
            .collect();
        assert_eq!(laid_out, vec![0, 2], "the fixture no longer drops panel 1");

        c.focus = ChromeFocus::Sidebar {
            sidebar: 0,
            panel: 0,
        };
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Down,
            None,
            100,
            5,
            &StatusBarPosition::Bottom
        ));
        assert_eq!(
            c.focus,
            ChromeFocus::Sidebar {
                sidebar: 0,
                panel: 2
            },
            "the walk landed on the dropped panel instead of skipping it"
        );
        // ... and back up again, skipping it in the other direction too.
        assert!(intercept_focus(
            &mut c,
            FocusDirection::Up,
            None,
            100,
            5,
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

#[cfg(test)]
mod resize_tests {
    use super::*;
    use crate::config::sidebar::{PanelConfig, SidebarConfig};
    use crate::server::layout::FocusDirection::{Down, Left, Right, Up};

    fn focused(edge: SidebarEdge, size: u16, weights: &[u16]) -> Chrome {
        let mut c = Chrome::from_config(&[SidebarConfig {
            edge,
            size,
            visible: true,
            panel: weights
                .iter()
                .map(|w| PanelConfig {
                    plugin: "placeholder".into(),
                    weight: *w,
                })
                .collect(),
        }]);
        assert!(c.focus_edge(edge, 100, 30), "the test sidebar must focus");
        c
    }

    #[test]
    fn direction_is_spatial_for_a_left_sidebar() {
        let mut c = focused(SidebarEdge::Left, 30, &[1]);
        assert!(c.resize_focused(Right, 5, 100, 30));
        assert_eq!(c.sidebars[0].size, 35, "Right must GROW a left sidebar");
        assert!(c.resize_focused(Left, 5, 100, 30));
        assert_eq!(c.sidebars[0].size, 30, "Left must SHRINK a left sidebar");
    }

    #[test]
    fn direction_is_spatial_for_a_right_sidebar() {
        // The mirror image: the same key that grows a left sidebar shrinks this
        // one. Signing the arithmetic by the command's NAME rather than by the
        // edge is exactly the bug this catches.
        let mut c = focused(SidebarEdge::Right, 30, &[1]);
        assert!(c.resize_focused(Left, 5, 100, 30));
        assert_eq!(c.sidebars[0].size, 35, "Left must GROW a right sidebar");
        assert!(c.resize_focused(Right, 5, 100, 30));
        assert_eq!(c.sidebars[0].size, 30, "Right must SHRINK a right sidebar");
    }

    #[test]
    fn direction_is_spatial_for_a_bottom_sidebar() {
        let mut c = focused(SidebarEdge::Bottom, 8, &[1]);
        assert!(c.resize_focused(Up, 2, 100, 30));
        assert_eq!(c.sidebars[0].size, 10, "Up must GROW a bottom sidebar");
        assert!(c.resize_focused(Down, 2, 100, 30));
        assert_eq!(c.sidebars[0].size, 8, "Down must SHRINK a bottom sidebar");
    }

    #[test]
    fn the_stack_axis_moves_the_focused_panels_weight() {
        let mut c = focused(SidebarEdge::Left, 30, &[1, 1]);
        assert!(c.resize_focused(Down, 5, 100, 30));
        assert_eq!(
            c.sidebars[0].panels[0].weight, 2,
            "Down must grow the focused panel, as it grows a focused pane"
        );
        assert_eq!(
            c.sidebars[0].size, 30,
            "the stack axis must not move `size`"
        );
        assert!(c.resize_focused(Up, 5, 100, 30));
        assert_eq!(c.sidebars[0].panels[0].weight, 1);
    }

    #[test]
    fn a_bottom_sidebars_stack_axis_is_horizontal() {
        let mut c = focused(SidebarEdge::Bottom, 8, &[1, 1]);
        assert!(c.resize_focused(Right, 5, 100, 30));
        assert_eq!(c.sidebars[0].panels[0].weight, 2);
        assert_eq!(c.sidebars[0].size, 8);
    }

    #[test]
    fn a_weight_never_falls_below_one() {
        let mut c = focused(SidebarEdge::Left, 30, &[1, 1]);
        assert!(!c.resize_focused(Up, 5, 100, 30), "nothing to shrink");
        assert_eq!(c.sidebars[0].panels[0].weight, 1);
    }

    #[test]
    fn a_weight_change_that_would_drop_a_panel_is_refused() {
        // The placeholder needs 2 rows. In a 30-row sidebar a 14:1 split still
        // leaves the second panel 2 rows; 15:1 does not, and dropping it is
        // exactly what the user did not ask for.
        let mut c = focused(SidebarEdge::Left, 30, &[1, 1]);
        let mut last = 1;
        for _ in 0..40 {
            if !c.resize_focused(Down, 5, 100, 30) {
                break;
            }
            last = c.sidebars[0].panels[0].weight;
        }
        assert!(last > 1, "the weight never grew at all");
        assert_eq!(
            c.panel_rects(100, 30).len(),
            2,
            "growing one panel dropped its neighbour off the screen"
        );
    }

    #[test]
    fn a_size_never_shrinks_below_what_the_panels_need() {
        // The placeholder's `min_size` is 8 columns; below that `split_panels`
        // drops it and the sidebar paints nothing at all.
        let mut c = focused(SidebarEdge::Left, 30, &[1]);
        for _ in 0..20 {
            c.resize_focused(Left, 5, 100, 30);
        }
        assert_eq!(c.sidebars[0].size, 8);
        // Asserting the panel is still LAID OUT would be vacuous: `split_panels`
        // drops on the stack axis, which a vertical sidebar's width does not
        // touch, and it never drops the last panel anyway. What the floor
        // actually buys is the painted WIDTH.
        let rects = c.panel_rects(100, 30);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].2.width, 8, "the panel is painted at the floor");
    }

    #[test]
    fn a_size_never_grows_past_what_the_terminal_can_give() {
        // `MIN_CONTENT_COLS` is 20, so at 100 columns the sidebar stops at 80.
        let mut c = focused(SidebarEdge::Left, 30, &[1]);
        for _ in 0..40 {
            c.resize_focused(Right, 5, 100, 30);
        }
        assert_eq!(c.sidebars[0].size, 80);
        assert!(
            c.content_rect(100, 30).width >= 20,
            "the content rect lost its minimum"
        );
        // And the stored value must not have run ahead of what was granted: a
        // later, smaller terminal would silently force-hide the sidebar.
        assert!(c.has_any_visible(100, 30));
    }

    #[test]
    fn the_arithmetic_starts_from_the_granted_size_not_the_stored_one() {
        // Stored 60 but only 40 granted at this width. One shrink must move the
        // edge the user can actually see, not walk the stored number down to a
        // value that changes nothing on screen.
        let mut c = focused(SidebarEdge::Left, 60, &[1]);
        assert_eq!(effective_sizes(&c.geoms(), 60, 30)[0], 40);
        assert!(c.resize_focused(Left, 5, 60, 30));
        assert_eq!(c.sidebars[0].size, 35);
    }

    /// Two sidebars, focus on the first. The single-sidebar helper cannot see
    /// any of the cross-sidebar cases -- `effective_sizes` only starts sharing
    /// a budget when there is something to share it with.
    fn focused_two(a: SidebarConfig, b: SidebarConfig) -> Chrome {
        let edge = a.edge;
        let mut c = Chrome::from_config(&[a, b]);
        assert!(c.focus_edge(edge, 100, 30), "the test sidebar must focus");
        c
    }

    fn bar(edge: SidebarEdge, size: u16, panels: usize) -> SidebarConfig {
        SidebarConfig {
            edge,
            size,
            visible: true,
            panel: (0..panels)
                .map(|_| PanelConfig {
                    plugin: "placeholder".into(),
                    weight: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn growing_one_sidebar_never_force_hides_another() {
        // 100 columns, `MIN_CONTENT_COLS` 20: the verticals share an 80-column
        // budget in declaration order, so a left sidebar grown to 80 leaves the
        // right one a budget of zero and it disappears -- while the left's own
        // granted size is a perfectly healthy 80.
        let mut c = focused_two(
            bar(SidebarEdge::Left, 30, 1),
            bar(SidebarEdge::Right, 40, 1),
        );
        for _ in 0..40 {
            c.resize_focused(Right, 5, 100, 30);
        }
        let bars: Vec<usize> = c
            .panel_rects(100, 30)
            .into_iter()
            .map(|(s, _, _)| s)
            .collect();
        assert!(
            bars.contains(&1),
            "growing the left sidebar force-hid the right one (left is now {})",
            c.sidebars[0].size
        );
        assert_eq!(
            c.sidebars[0].size, 75,
            "the left sidebar should stop one step short of erasing its neighbour"
        );
    }

    #[test]
    fn growing_a_vertical_never_drops_the_bottom_sidebars_panels() {
        // The second face of the same hole: a vertical's width is subtracted
        // from `content.width`, which IS the bottom sidebar's bar, and ITS
        // panels are dropped on `min_cols` (8 for the placeholder). Three
        // panels need 24 columns between them.
        let mut c = focused_two(
            bar(SidebarEdge::Left, 30, 1),
            bar(SidebarEdge::Bottom, 6, 3),
        );
        let bottom_panels = |c: &Chrome| {
            c.panel_rects(100, 30)
                .into_iter()
                .filter(|(s, _, _)| *s == 1)
                .count()
        };
        assert_eq!(bottom_panels(&c), 3, "all three must start laid out");
        for _ in 0..40 {
            c.resize_focused(Right, 5, 100, 30);
        }
        assert_eq!(
            bottom_panels(&c),
            3,
            "growing the left sidebar dropped a panel out of the bottom one \
             (left is now {})",
            c.sidebars[0].size
        );
    }

    #[test]
    fn a_resize_that_rescues_a_dropped_panel_is_allowed() {
        // The vanish check must be a SUBSET test, not equality. A config can
        // arrive with a sidebar already force-hidden (or a panel already
        // dropped), and shrinking back is exactly how the user recovers it --
        // under an equality test the recovering press would be refused for
        // changing the laid-out set, trapping them.
        let mut c = focused_two(
            bar(SidebarEdge::Left, 80, 1),
            bar(SidebarEdge::Right, 40, 1),
        );
        let bars = |c: &Chrome| -> Vec<usize> {
            c.panel_rects(100, 30)
                .into_iter()
                .map(|(s, _, _)| s)
                .collect()
        };
        assert!(
            !bars(&c).contains(&1),
            "the right sidebar starts force-hidden"
        );
        assert!(
            c.resize_focused(Left, 5, 100, 30),
            "the shrink must be allowed"
        );
        assert_eq!(c.sidebars[0].size, 75);
        assert!(
            bars(&c).contains(&1),
            "shrinking did not bring the right sidebar back"
        );
    }

    #[test]
    fn a_grow_takes_a_partial_grant_rather_than_stalling() {
        // 100 columns, ceiling 80, stored 78: the step of 5 does not divide the
        // 2 columns left, so the press asks for 83 and the layout gives 80.
        // That is a real move and must be taken -- refusing it wedges the
        // sidebar at 78 for good, since every later press asks the same
        // question and gets the same answer.
        let mut c = focused(SidebarEdge::Left, 78, &[1]);
        assert!(
            c.resize_focused(Right, 5, 100, 30),
            "the partial grant was refused"
        );
        assert_eq!(c.sidebars[0].size, 80);
        // And THEN it is genuinely stuck, which is the no-op case.
        assert!(!c.resize_focused(Right, 5, 100, 30));
        assert_eq!(c.sidebars[0].size, 80);
    }

    #[test]
    fn a_grow_the_terminal_cannot_honour_keeps_the_remembered_size() {
        // Stored 60, granted 40 at a 60-column terminal. The press cannot move
        // anything on screen, so it must not rewrite 60 down to 40 either --
        // the user chose that width at a bigger terminal and will want it back.
        let mut c = focused(SidebarEdge::Left, 60, &[1]);
        assert_eq!(effective_sizes(&c.geoms(), 60, 30)[0], 40);
        assert!(!c.resize_focused(Right, 5, 60, 30));
        assert_eq!(c.sidebars[0].size, 60, "a clamped grow discarded the size");
    }

    #[test]
    fn with_focus_on_the_content_nothing_is_consumed() {
        // The regression gate: this is every client without a sidebar, and
        // every client whose sidebar is not focused.
        let mut c = focused(SidebarEdge::Left, 30, &[1]);
        c.leave_sidebar();
        assert!(!intercept_resize(&mut c, Right, 5, 100, 30));
        assert_eq!(c.sidebars[0].size, 30);

        let mut empty = Chrome::from_config(&[]);
        assert!(!intercept_resize(&mut empty, Right, 5, 100, 30));
    }

    #[test]
    fn a_clamped_resize_is_still_consumed() {
        // Nothing inside a sidebar reaches the server -- a resize with nowhere
        // to go is a swallowed no-op, never a leaked pane resize.
        let mut c = focused(SidebarEdge::Left, 30, &[1]);
        for _ in 0..20 {
            c.resize_focused(Left, 5, 100, 30);
        }
        assert!(!c.resize_focused(Left, 5, 100, 30), "already at the floor");
        assert!(
            intercept_resize(&mut c, Left, 5, 100, 30),
            "a clamped resize must still be consumed"
        );
    }

    #[test]
    fn focus_stranded_by_a_resize_is_released_rather_than_consumed() {
        // A SIGWINCH between the prefix and the chord key can force-hide the
        // sidebar while `chrome.focus` still names it. The command must reach
        // the server, not vanish into a panel nobody can see.
        let mut c = focused(SidebarEdge::Left, 30, &[1]);
        assert!(!intercept_resize(&mut c, Right, 5, 20, 30));
        assert_eq!(c.focus, ChromeFocus::Content);
    }
}
