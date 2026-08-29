//! Pure sidebar geometry.
//!
//! Splits the terminal into a content rect (handed to the server as the
//! client's `Resize`) and a set of absolutely-positioned panel rects. Kept free
//! of I/O and of plugin trait objects so every edge combination is unit-tested.

use crate::config::{BorderStyle, StatusBarPosition};
use crate::server::compositor::{fits_zellij_border, pane_content_rect};
use crate::server::layout::Rect;

/// Which terminal edge a sidebar is docked to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarEdge {
    Left,
    Right,
    Bottom,
}

/// The geometric facts about one panel: how much of its sidebar it claims and
/// the smallest rect it can usefully render into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelGeom {
    pub weight: u16,
    pub min_cols: u16,
    pub min_rows: u16,
}

/// The geometric facts about one sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarGeom {
    pub edge: SidebarEdge,
    /// Columns for `Left`/`Right`, rows for `Bottom`.
    pub size: u16,
    pub visible: bool,
    pub panels: Vec<PanelGeom>,
}

/// The narrowest content rect we will hand the server.
pub const MIN_CONTENT_COLS: u16 = 20;
/// The shortest content rect we will hand the server.
pub const MIN_CONTENT_ROWS: u16 = 5;

/// Per-sidebar effective size after clamping, in the same order as `sidebars`.
///
/// A sidebar is shrunk so the content rect keeps its minimum; if even that is
/// impossible the sidebar is force-hidden, reported as size `0`. Verticals are
/// resolved before the bottom sidebar because verticals own the corners.
pub fn effective_sizes(sidebars: &[SidebarGeom], term_cols: u16, term_rows: u16) -> Vec<u16> {
    let mut sizes = vec![0u16; sidebars.len()];

    // -- Verticals: share the column budget, left resolved before right. --
    let mut cols_left = term_cols;
    for (i, s) in sidebars.iter().enumerate() {
        if !s.visible || s.size == 0 {
            continue;
        }
        if !matches!(s.edge, SidebarEdge::Left | SidebarEdge::Right) {
            continue;
        }
        let budget = cols_left.saturating_sub(MIN_CONTENT_COLS);
        let granted = s.size.min(budget);
        if granted == 0 {
            // Logged because there is no other trace: a sidebar restored at a
            // persisted size that no longer fits simply is not painted, and to
            // the user that is indistinguishable from the feature being broken.
            log::debug!(
                "sidebar: force-hiding the {:?} sidebar -- size {} does not fit in \
                 {term_cols}x{term_rows} while keeping {MIN_CONTENT_COLS} content columns",
                s.edge,
                s.size
            );
            continue;
        }
        sizes[i] = granted;
        cols_left -= granted;
    }

    // -- Bottom: takes from the row budget. --
    let mut rows_left = term_rows;
    for (i, s) in sidebars.iter().enumerate() {
        if !s.visible || s.size == 0 || s.edge != SidebarEdge::Bottom {
            continue;
        }
        let budget = rows_left.saturating_sub(MIN_CONTENT_ROWS);
        let granted = s.size.min(budget);
        if granted == 0 {
            log::debug!(
                "sidebar: force-hiding the bottom sidebar -- size {} does not fit in \
                 {term_cols}x{term_rows} while keeping {MIN_CONTENT_ROWS} content rows",
                s.size
            );
            continue;
        }
        sizes[i] = granted;
        rows_left -= granted;
    }

    sizes
}

/// The rect handed to the server as the client's `Resize`.
pub fn content_rect(sidebars: &[SidebarGeom], term_cols: u16, term_rows: u16) -> Rect {
    let sizes = effective_sizes(sidebars, term_cols, term_rows);
    let mut x = 0u16;
    let mut width = term_cols;
    let mut height = term_rows;

    for (i, s) in sidebars.iter().enumerate() {
        let size = sizes[i];
        if size == 0 {
            continue;
        }
        match s.edge {
            SidebarEdge::Left => {
                x += size;
                width -= size;
            }
            SidebarEdge::Right => width -= size,
            SidebarEdge::Bottom => height -= size,
        }
    }

    Rect {
        x,
        y: 0,
        width,
        height,
    }
}

/// How a sidebar's frame divides its bar: the interior its panels share, and
/// the gap the frame reserves between two stacked panels for the rule it draws
/// there.
///
/// The frame is drawn INSIDE the bar, exactly as a pane's border is drawn
/// inside its rect. A sidebar's `size` therefore does not change when it gains
/// a frame -- `content_rect`, and so the `Resize` the server is handed, is
/// untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarFrame {
    /// The rect the panels divide.
    pub interior: Rect,
    /// Cells between two stacked panels, for the separator rule.
    pub gap: u16,
    /// Whether a frame is drawn at all. `false` means the bar is too small to
    /// carry one and renders exactly as it did before frames existed.
    pub framed: bool,
}

/// The frame a sidebar on `edge` gets under `style`, given its bar rect.
///
/// The style's own inset comes from [`pane_content_rect`] -- the one definition
/// of "what is left of a rect once this border style has taken its share", the
/// same call the compositor and the PTY-sizing path go through. A sidebar is
/// not a stack, so `multi_stack` is `false`.
///
/// **Zellij style** is a full box, and `pane_content_rect` insets by one cell
/// on every side, gated on [`fits_zellij_border`]. The gate is repeated here
/// explicitly rather than inferred from `interior != bar`: whether a frame is
/// wanted is a separate question from how big it is, and the two agreeing is
/// something to assert, not to assume.
///
/// **tmux style is where reuse stops, and deliberately.** `pane_content_rect`
/// returns the rect unchanged for a non-stack tmux pane, which is correct for a
/// PANE: tmux dividers are not inset at all, they are drawn ON TOP of the
/// neighbour's last column (`draw_tmux_dividers` writes `buffer[row][col - 1]`,
/// inside the LEFT rect). A sidebar cannot do that. The rect on the far side of
/// its seam is the server's content rect; anything the client paints there is
/// overwritten on the next diff render, and for a right or bottom sidebar the
/// compositor's rule would put the divider inside that rect rather than inside
/// the sidebar. So the seam is RESERVED out of the sidebar's own interior --
/// the last column of a left sidebar, the first of a right one, the first row
/// of the bottom one -- which is the only placement available on all three
/// edges. That reservation is the delta; the glyphs and the cell styling still
/// come from the shared primitives in `chrome::frame`.
pub fn sidebar_frame(style: &BorderStyle, edge: SidebarEdge, bar: Rect) -> SidebarFrame {
    let unframed = SidebarFrame {
        interior: bar,
        gap: 0,
        framed: false,
    };
    if bar.width == 0 || bar.height == 0 {
        return unframed;
    }
    match style {
        BorderStyle::ZellijStyle => {
            if !fits_zellij_border(bar.width, bar.height) {
                return unframed;
            }
            SidebarFrame {
                interior: pane_content_rect(style, bar, false),
                gap: 1,
                framed: true,
            }
        }
        BorderStyle::TmuxStyle => {
            // Starts from the style's own inset (a no-op for a non-stack tmux
            // pane), then reserves the seam. See the doc comment above.
            let base = pane_content_rect(style, bar, false);
            let interior = match edge {
                SidebarEdge::Left => {
                    if base.width < 2 {
                        return unframed;
                    }
                    Rect {
                        width: base.width - 1,
                        ..base
                    }
                }
                SidebarEdge::Right => {
                    if base.width < 2 {
                        return unframed;
                    }
                    Rect {
                        x: base.x + 1,
                        width: base.width - 1,
                        ..base
                    }
                }
                SidebarEdge::Bottom => {
                    if base.height < 2 {
                        return unframed;
                    }
                    Rect {
                        y: base.y + 1,
                        height: base.height - 1,
                        ..base
                    }
                }
            };
            SidebarFrame {
                interior,
                gap: 1,
                framed: true,
            }
        }
    }
}

/// How many cells of the axis a sidebar's `size` measures its frame consumes.
///
/// Perpendicular to the edge: both sides of the zellij box, the single tmux
/// divider. This is what a sidebar's minimum `size` has to clear on top of its
/// plugins' own minimums, or shrinking to that minimum would leave an interior
/// narrower than the plugin asked for.
pub fn frame_size_inset(style: &BorderStyle) -> u16 {
    match style {
        BorderStyle::ZellijStyle => 2,
        BorderStyle::TmuxStyle => 1,
    }
}

/// Absolute screen rects for every visible sidebar's BAR -- its full extent,
/// frame included -- as `(sidebar_index, rect)`.
///
/// Vertical sidebars span the full terminal height and are laid out from each
/// edge inward, so two on the same edge stack side by side rather than
/// overlapping; the bottom sidebar spans only the columns between the
/// verticals, which own the corners.
pub fn bar_rects(sidebars: &[SidebarGeom], term_cols: u16, term_rows: u16) -> Vec<(usize, Rect)> {
    let sizes = effective_sizes(sidebars, term_cols, term_rows);
    let content = content_rect(sidebars, term_cols, term_rows);
    let mut out = Vec::new();

    let mut left_x = 0u16;
    let mut right_x = term_cols;
    let mut bottom_y = term_rows;

    for (i, s) in sidebars.iter().enumerate() {
        let size = sizes[i];
        if size == 0 {
            continue;
        }
        let bar = match s.edge {
            SidebarEdge::Left => {
                let r = Rect {
                    x: left_x,
                    y: 0,
                    width: size,
                    height: term_rows,
                };
                left_x += size;
                r
            }
            SidebarEdge::Right => {
                right_x -= size;
                Rect {
                    x: right_x,
                    y: 0,
                    width: size,
                    height: term_rows,
                }
            }
            SidebarEdge::Bottom => {
                bottom_y -= size;
                Rect {
                    x: content.x,
                    y: bottom_y,
                    width: content.width,
                    height: size,
                }
            }
        };
        out.push((i, bar));
    }

    out
}

/// Absolute screen rects for every visible panel, as
/// `(sidebar_index, panel_index, rect)`.
///
/// These are the panels' INTERIORS: the bar minus the frame `style` draws
/// around it, and minus the rules between stacked panels. A plugin renders into
/// exactly this rect, a mouse event hits a panel only inside it (a click on the
/// frame itself belongs to no panel), and a panel's `min_size` is measured
/// against it.
///
/// Vertical sidebars span the full terminal height and stack their panels
/// vertically; the bottom sidebar spans only the columns between the verticals
/// and stacks its panels horizontally.
pub fn panel_rects(
    sidebars: &[SidebarGeom],
    term_cols: u16,
    term_rows: u16,
    style: &BorderStyle,
) -> Vec<(usize, usize, Rect)> {
    let mut out = Vec::new();
    for (i, bar) in bar_rects(sidebars, term_cols, term_rows) {
        let s = &sidebars[i];
        let frame = sidebar_frame(style, s.edge, bar);
        let vertical = !matches!(s.edge, SidebarEdge::Bottom);
        for (pi, rect) in split_panels(frame.interior, &s.panels, vertical, frame.gap) {
            out.push((i, pi, rect));
        }
    }
    out
}

/// The extent left for panel CONTENT once `n` panels' separating rules have
/// taken theirs.
///
/// `gap` sits BETWEEN panels, so `n` panels need `n - 1` of them.
fn content_extent(extent: u16, gap: u16, n: usize) -> u16 {
    let rules = (n.saturating_sub(1) as u32) * gap as u32;
    extent.saturating_sub(rules.min(u16::MAX as u32) as u16)
}

/// Divide `bar` -- the sidebar's INTERIOR -- among `panels` in proportion to
/// weight, leaving `gap` cells between neighbours for the frame's separator
/// rule, dropping any panel whose share falls below its minimum, and giving the
/// remainder to the last surviving panel so the division is exact.
///
/// The minimum test runs against the extent left AFTER the rules are deducted,
/// and is recomputed on every drop: dropping a panel returns both its share and
/// one rule to the survivors, so a rule must never be able to push a panel
/// below its minimum without that being visible to the check.
fn split_panels(bar: Rect, panels: &[PanelGeom], vertical: bool, gap: u16) -> Vec<(usize, Rect)> {
    if panels.is_empty() {
        return Vec::new();
    }
    let extent = if vertical { bar.height } else { bar.width };

    // Drop panels whose weighted share cannot meet their minimum. Repeat, since
    // dropping one enlarges everyone else's share and may rescue a neighbour.
    let mut kept: Vec<usize> = (0..panels.len()).collect();
    loop {
        let avail = content_extent(extent, gap, kept.len());
        let total: u32 = kept.iter().map(|i| panels[*i].weight.max(1) as u32).sum();
        let Some(&victim) = kept.iter().find(|i| {
            let share = (avail as u32 * panels[**i].weight.max(1) as u32 / total) as u16;
            let min = if vertical {
                panels[**i].min_rows
            } else {
                panels[**i].min_cols
            };
            share < min
        }) else {
            break;
        };
        if kept.len() == 1 {
            break; // never drop the last one; it takes the whole bar
        }
        kept.retain(|i| *i != victim);
    }

    let avail = content_extent(extent, gap, kept.len());
    let total: u32 = kept.iter().map(|i| panels[*i].weight.max(1) as u32).sum();
    let mut out = Vec::with_capacity(kept.len());
    // `offset` walks the bar (content plus rules); `used` counts content only,
    // so the last panel's remainder is measured against `avail`.
    let mut offset = 0u16;
    let mut used = 0u16;
    for (n, &i) in kept.iter().enumerate() {
        let last = n + 1 == kept.len();
        let span = if last {
            avail - used
        } else {
            (avail as u32 * panels[i].weight.max(1) as u32 / total) as u16
        };
        let rect = if vertical {
            Rect {
                x: bar.x,
                y: bar.y + offset,
                width: bar.width,
                height: span,
            }
        } else {
            Rect {
                x: bar.x + offset,
                y: bar.y,
                width: span,
                height: bar.height,
            }
        };
        out.push((i, rect));
        used += span;
        offset += span + if last { 0 } else { gap };
    }
    out
}

/// The content rect minus the status-bar row -- the rect directional edge tests
/// must run against, rather than the content rect itself.
///
/// **The bar is always the LAST row, whatever `status_bar_position` says.** The
/// server composites it onto the final row of the frame unconditionally
/// (`compositor.rs`, `draw_status_bar`); nothing in the compositor consults the
/// option. `status_bar_position` is parsed and validated but not honoured, so
/// `"top"` is inert.
///
/// This used to reserve the FIRST row under `Top`, on the assumption that the
/// option worked. It does not, so that arm moved the pane area off the actual
/// panes: with a sidebar configured, `"top"` made directional entry into it off
/// by one. Reserving the last row unconditionally makes the setting harmlessly
/// inert instead of quietly wrong.
///
/// `_status_bar` is kept in the signature deliberately: it is the seam to
/// restore when the server learns to honour the option, and every caller
/// already threads it. Re-branch on it only together with that server change,
/// or this bug comes back.
pub fn pane_area(content: Rect, _status_bar: &StatusBarPosition) -> Rect {
    if content.height == 0 {
        return content;
    }
    Rect {
        x: content.x,
        y: content.y,
        width: content.width,
        height: content.height - 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StatusBarPosition;

    /// The style every geometry test that does not say otherwise runs under --
    /// the configured default, and the one that frames most heavily (a box on
    /// all four sides), so an interior expectation here is the tightest one.
    const ZJ: BorderStyle = BorderStyle::ZellijStyle;

    fn sb(edge: SidebarEdge, size: u16, weights: &[u16]) -> SidebarGeom {
        SidebarGeom {
            edge,
            size,
            visible: true,
            panels: weights
                .iter()
                .map(|w| PanelGeom {
                    weight: *w,
                    min_cols: 1,
                    min_rows: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn no_sidebars_is_the_whole_terminal() {
        let r = content_rect(&[], 100, 30);
        assert_eq!(
            r,
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 30
            }
        );
    }

    #[test]
    fn left_sidebar_shifts_origin_and_shrinks_width() {
        let r = content_rect(&[sb(SidebarEdge::Left, 30, &[1])], 100, 30);
        assert_eq!(
            r,
            Rect {
                x: 30,
                y: 0,
                width: 70,
                height: 30
            }
        );
    }

    #[test]
    fn right_sidebar_shrinks_width_only() {
        let r = content_rect(&[sb(SidebarEdge::Right, 24, &[1])], 100, 30);
        assert_eq!(
            r,
            Rect {
                x: 0,
                y: 0,
                width: 76,
                height: 30
            }
        );
    }

    #[test]
    fn bottom_sidebar_shrinks_height_only() {
        let r = content_rect(&[sb(SidebarEdge::Bottom, 8, &[1])], 100, 30);
        assert_eq!(
            r,
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 22
            }
        );
    }

    #[test]
    fn hidden_sidebar_takes_no_space() {
        let mut s = sb(SidebarEdge::Left, 30, &[1]);
        s.visible = false;
        assert_eq!(
            content_rect(&[s], 100, 30),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 30
            }
        );
    }

    #[test]
    fn all_three_edges_combine() {
        let r = content_rect(
            &[
                sb(SidebarEdge::Left, 30, &[1]),
                sb(SidebarEdge::Right, 20, &[1]),
                sb(SidebarEdge::Bottom, 6, &[1]),
            ],
            120,
            40,
        );
        assert_eq!(
            r,
            Rect {
                x: 30,
                y: 0,
                width: 70,
                height: 34
            }
        );
    }

    #[test]
    fn verticals_own_the_corners_so_bottom_spans_between_them() {
        // Decision 2 in the spec: the bottom sidebar's bar starts after the left
        // sidebar and ends before the right one, while the verticals run the
        // full terminal height.
        //
        // Asserted on `bar_rects`, which is where this fact now lives: a
        // sidebar's BAR is what claims terminal space, and the frame is drawn
        // inside it. Before frames the panel rect and the bar were the same
        // rect, so this used to read `panel_rects`; the numbers below are
        // unchanged, only the function they are asked of.
        let sbs = [
            sb(SidebarEdge::Left, 30, &[1]),
            sb(SidebarEdge::Right, 20, &[1]),
            sb(SidebarEdge::Bottom, 6, &[1]),
        ];
        let bars = bar_rects(&sbs, 120, 40);
        let left = bars.iter().find(|(s, _)| *s == 0).unwrap().1;
        let right = bars.iter().find(|(s, _)| *s == 1).unwrap().1;
        let bottom = bars.iter().find(|(s, _)| *s == 2).unwrap().1;

        assert_eq!(
            left,
            Rect {
                x: 0,
                y: 0,
                width: 30,
                height: 40
            }
        );
        assert_eq!(
            right,
            Rect {
                x: 100,
                y: 0,
                width: 20,
                height: 40
            }
        );
        assert_eq!(
            bottom,
            Rect {
                x: 30,
                y: 34,
                width: 70,
                height: 6
            }
        );
    }

    #[test]
    fn a_framed_panel_sits_one_cell_inside_its_bar_on_every_side() {
        // The companion to the test above, and the whole point of the frame
        // work: the bar is unchanged, the PANEL is what shrank. A 30-column
        // sidebar still claims 30 columns and gives its plugin 28.
        let sbs = [
            sb(SidebarEdge::Left, 30, &[1]),
            sb(SidebarEdge::Right, 20, &[1]),
            sb(SidebarEdge::Bottom, 6, &[1]),
        ];
        let rects = panel_rects(&sbs, 120, 40, &ZJ);
        let left = rects.iter().find(|(s, _, _)| *s == 0).unwrap().2;
        let right = rects.iter().find(|(s, _, _)| *s == 1).unwrap().2;
        let bottom = rects.iter().find(|(s, _, _)| *s == 2).unwrap().2;

        assert_eq!(
            left,
            Rect {
                x: 1,
                y: 1,
                width: 28,
                height: 38
            }
        );
        assert_eq!(
            right,
            Rect {
                x: 101,
                y: 1,
                width: 18,
                height: 38
            }
        );
        assert_eq!(
            bottom,
            Rect {
                x: 31,
                y: 35,
                width: 68,
                height: 4
            }
        );
    }

    #[test]
    fn the_tmux_frame_takes_only_the_seam_against_the_content() {
        // tmux style has no per-pane box, so a sidebar gets exactly one
        // divider, on the side facing the content: the last column of a left
        // sidebar, the first of a right one, the top row of the bottom one.
        let sbs = [
            sb(SidebarEdge::Left, 30, &[1]),
            sb(SidebarEdge::Right, 20, &[1]),
            sb(SidebarEdge::Bottom, 6, &[1]),
        ];
        let rects = panel_rects(&sbs, 120, 40, &BorderStyle::TmuxStyle);
        let left = rects.iter().find(|(s, _, _)| *s == 0).unwrap().2;
        let right = rects.iter().find(|(s, _, _)| *s == 1).unwrap().2;
        let bottom = rects.iter().find(|(s, _, _)| *s == 2).unwrap().2;

        assert_eq!(
            left,
            Rect {
                x: 0,
                y: 0,
                width: 29,
                height: 40
            }
        );
        assert_eq!(
            right,
            Rect {
                x: 101,
                y: 0,
                width: 19,
                height: 40
            }
        );
        assert_eq!(
            bottom,
            Rect {
                x: 30,
                y: 35,
                width: 70,
                height: 5
            }
        );
    }

    #[test]
    fn the_frame_never_moves_the_content_rect() {
        // The load-bearing invariant of the whole frame change: the frame is
        // drawn INSIDE `size`, so the rect handed to the server as `Resize` is
        // identical in every style, and identical to what it was before frames
        // existed. `content_rect` does not even take a style -- this pins that
        // it never needs to.
        let sbs = [
            sb(SidebarEdge::Left, 30, &[1, 1]),
            sb(SidebarEdge::Right, 20, &[1]),
            sb(SidebarEdge::Bottom, 6, &[1, 1]),
        ];
        let content = content_rect(&sbs, 120, 40);
        assert_eq!(
            content,
            Rect {
                x: 30,
                y: 0,
                width: 70,
                height: 34
            }
        );
        for style in [ZJ, BorderStyle::TmuxStyle] {
            for (i, bar) in bar_rects(&sbs, 120, 40) {
                let f = sidebar_frame(&style, sbs[i].edge, bar);
                assert!(f.interior.width <= bar.width);
                // No panel may be laid out outside its own bar, which is what
                // would let a frame push into the content rect.
                for (_, _, r) in panel_rects(&sbs, 120, 40, &style)
                    .into_iter()
                    .filter(|(s, _, _)| *s == i)
                {
                    assert!(r.x >= bar.x && r.x + r.width <= bar.x + bar.width, "{r:?}");
                    assert!(
                        r.y >= bar.y && r.y + r.height <= bar.y + bar.height,
                        "{r:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_framed_sidebars_interior_is_the_panes_content_rect() {
        // The anti-duplication assertion: the zellij inset is not re-derived
        // here, it IS `pane_content_rect`. If someone reintroduces a local
        // copy that drifts, this fails.
        let bar = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 40,
        };
        assert_eq!(
            sidebar_frame(&ZJ, SidebarEdge::Left, bar).interior,
            pane_content_rect(&ZJ, bar, false)
        );
        // ... and the tmux interior is that same shared inset MINUS the seam,
        // which is the one documented delta: a pane's divider is drawn over its
        // neighbour, and a sidebar has no neighbour it is allowed to paint.
        let tm = BorderStyle::TmuxStyle;
        let base = pane_content_rect(&tm, bar, false);
        assert_eq!(base, bar, "a non-stack tmux pane is not inset at all");
        assert_eq!(
            sidebar_frame(&tm, SidebarEdge::Left, bar).interior,
            Rect {
                width: base.width - 1,
                ..base
            }
        );
    }

    #[test]
    fn a_bar_too_small_to_frame_gives_its_panel_the_whole_bar() {
        // The degrade path: `fits_zellij_border` is 3x3, so a 2-column sidebar
        // renders exactly as it did before frames existed rather than drawing a
        // box with no inside.
        let bar = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 30,
        };
        let f = sidebar_frame(&ZJ, SidebarEdge::Left, bar);
        assert!(!f.framed);
        assert_eq!(f.interior, bar);
        assert_eq!(f.gap, 0);
        // ... and tmux, which needs only a column for its seam, still frames.
        let t = sidebar_frame(&BorderStyle::TmuxStyle, SidebarEdge::Left, bar);
        assert!(t.framed);
        assert_eq!(t.interior.width, 1);
        // A single column has nowhere to put the seam AND a panel.
        let one = Rect { width: 1, ..bar };
        assert!(!sidebar_frame(&BorderStyle::TmuxStyle, SidebarEdge::Left, one).framed);
    }

    #[test]
    fn stacked_panels_split_by_weight() {
        // Was 20/10 over the full 30-row bar. The interior is 28 rows, and one
        // of those is the rule between the two panels, so 27 rows divide 2:1
        // into 18 and 9 with the rule at bar-local row 19.
        let sbs = [sb(SidebarEdge::Left, 30, &[2, 1])];
        let rects = panel_rects(&sbs, 100, 30, &ZJ);
        assert_eq!(rects.len(), 2);
        assert_eq!(
            rects[0].2,
            Rect {
                x: 1,
                y: 1,
                width: 28,
                height: 18
            }
        );
        assert_eq!(
            rects[1].2,
            Rect {
                x: 1,
                y: 20,
                width: 28,
                height: 9
            }
        );
        assert_eq!(
            rects[1].2.y,
            rects[0].2.y + rects[0].2.height + 1,
            "the panels must leave exactly one row for the rule between them"
        );
    }

    #[test]
    fn weight_remainder_goes_to_the_last_panel() {
        // Was: 31 rows over weights 1,1,1 must not lose a row. The arithmetic
        // now runs on the INTERIOR minus the rules -- a 32-row terminal gives a
        // 30-row interior, two rules leave 28 for content, and 28 over three
        // equal weights is 9/9/10. The invariant is the same one: content plus
        // rules must fill the interior exactly, with nothing lost to rounding.
        let sbs = [sb(SidebarEdge::Left, 30, &[1, 1, 1])];
        let rects = panel_rects(&sbs, 100, 32, &ZJ);
        assert_eq!(rects.len(), 3);
        let content: u16 = rects.iter().map(|(_, _, r)| r.height).sum();
        let rules = rects.len() as u16 - 1;
        assert_eq!(content + rules, 30, "the interior is not filled exactly");
        assert_eq!(rects[2].2.height, 10, "the remainder went somewhere else");
        // ... and the last panel ends flush against the bottom border.
        let last = rects[2].2;
        assert_eq!(last.y + last.height, 31);
    }

    #[test]
    fn bottom_sidebar_panels_split_horizontally() {
        // Was 50/50 across the full 100-column bar. The interior is 98 columns
        // starting at x=1; one is the rule between the panels, so 97 divide
        // into 48 and 49 (the remainder goes to the last panel, as always).
        let sbs = [sb(SidebarEdge::Bottom, 6, &[1, 1])];
        let rects = panel_rects(&sbs, 100, 30, &ZJ);
        assert_eq!(
            rects[0].2,
            Rect {
                x: 1,
                y: 25,
                width: 48,
                height: 4
            }
        );
        assert_eq!(
            rects[1].2,
            Rect {
                x: 50,
                y: 25,
                width: 49,
                height: 4
            }
        );
        assert_eq!(
            rects[1].2.x,
            rects[0].2.x + rects[0].2.width + 1,
            "the bottom sidebar's rule is a COLUMN between horizontally stacked panels"
        );
    }

    #[test]
    fn oversized_sidebar_is_clamped_to_keep_minimum_content() {
        // 100 cols, a left sidebar asking for 95 would leave 5 columns.
        let sizes = effective_sizes(&[sb(SidebarEdge::Left, 95, &[1])], 100, 30);
        assert_eq!(sizes[0], 100 - MIN_CONTENT_COLS);
        let r = content_rect(&[sb(SidebarEdge::Left, 95, &[1])], 100, 30);
        assert_eq!(r.width, MIN_CONTENT_COLS);
    }

    #[test]
    fn sidebar_that_cannot_fit_at_all_is_force_hidden() {
        // Terminal narrower than the content minimum: the sidebar must vanish
        // rather than produce a zero-width content rect.
        let sizes = effective_sizes(&[sb(SidebarEdge::Left, 30, &[1])], 18, 30);
        assert_eq!(sizes[0], 0);
        let r = content_rect(&[sb(SidebarEdge::Left, 30, &[1])], 18, 30);
        assert_eq!(
            r,
            Rect {
                x: 0,
                y: 0,
                width: 18,
                height: 30
            }
        );
    }

    #[test]
    fn content_rect_is_never_zero_in_either_dimension() {
        for cols in 1u16..40 {
            for rows in 1u16..20 {
                let r = content_rect(
                    &[
                        sb(SidebarEdge::Left, 30, &[1]),
                        sb(SidebarEdge::Bottom, 10, &[1]),
                    ],
                    cols,
                    rows,
                );
                assert!(r.width > 0, "zero width at {cols}x{rows}");
                assert!(r.height > 0, "zero height at {cols}x{rows}");
            }
        }
    }

    #[test]
    fn panel_below_min_size_is_dropped() {
        let sbs = [SidebarGeom {
            edge: SidebarEdge::Left,
            size: 30,
            visible: true,
            panels: vec![
                PanelGeom {
                    weight: 10,
                    min_cols: 1,
                    min_rows: 1,
                },
                PanelGeom {
                    weight: 1,
                    min_cols: 1,
                    min_rows: 8,
                },
            ],
        }];
        // 10 rows total, so an 8-row interior. The second panel's weighted
        // share of the 7 rows left after the rule is 0, below its min of 8, so
        // it is dropped -- and dropping it returns the rule as well, leaving the
        // survivor the whole 8-row interior (it was the whole 10-row bar before
        // frames).
        let rects = panel_rects(&sbs, 100, 10, &ZJ);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].2.height, 8);
    }

    #[test]
    fn a_panel_min_size_is_measured_against_the_interior_not_the_bar() {
        // The discriminating case for checking mins after the frame: a 10-row
        // bar has an 8-row interior, so a panel asking for 9 rows does NOT fit
        // even though the bar is big enough for it. Measuring against the bar
        // would lay out a panel one row taller than the rect it is handed.
        let sbs = [SidebarGeom {
            edge: SidebarEdge::Left,
            size: 30,
            visible: true,
            panels: vec![
                PanelGeom {
                    weight: 1,
                    min_cols: 1,
                    min_rows: 9,
                },
                PanelGeom {
                    weight: 1,
                    min_cols: 1,
                    min_rows: 1,
                },
            ],
        }];
        let rects = panel_rects(&sbs, 100, 10, &ZJ);
        assert_eq!(rects.len(), 1, "both panels were kept in an 8-row interior");
        assert_eq!(rects[0].2.height, 8);
    }

    #[test]
    fn pane_area_excludes_the_status_bar_row_at_the_bottom() {
        let content = Rect {
            x: 30,
            y: 0,
            width: 70,
            height: 34,
        };
        let pa = pane_area(content, &StatusBarPosition::Bottom);
        assert_eq!(
            pa,
            Rect {
                x: 30,
                y: 0,
                width: 70,
                height: 33
            }
        );
    }

    /// `Top` is INERT, and that is the assertion -- not an oversight.
    ///
    /// This test previously pinned `y: 1, height: 33`, encoding the belief that
    /// the server honours `status_bar_position`. It does not: the bar is always
    /// composited on the last row, so reserving the first row pointed the pane
    /// area one row off the real panes and made directional entry into a
    /// configured sidebar off by one under `"top"`.
    ///
    /// Pinned as an EQUIVALENCE rather than as literal numbers, so that wiring
    /// the server up to honour the option fails here loudly and deliberately,
    /// instead of silently reintroducing the skew.
    #[test]
    fn pane_area_ignores_status_bar_position_because_the_server_does() {
        let content = Rect {
            x: 30,
            y: 0,
            width: 70,
            height: 34,
        };
        let top = pane_area(content, &StatusBarPosition::Top);
        let bottom = pane_area(content, &StatusBarPosition::Bottom);
        assert_eq!(
            top, bottom,
            "`top` must be inert while the server ignores it"
        );
        assert_eq!(
            top,
            Rect {
                x: 30,
                y: 0,
                width: 70,
                height: 33
            },
            "the reserved row is the LAST one, where the server actually draws"
        );
    }

    #[test]
    fn two_bottom_sidebars_stack_instead_of_overlapping() {
        let sbs = [
            sb(SidebarEdge::Bottom, 6, &[1]),
            sb(SidebarEdge::Bottom, 4, &[1]),
        ];
        // On `bar_rects`: overlap is a property of the BARS, which is what
        // claims terminal space. Frames are drawn inside them and cannot make
        // two bars overlap or stop them doing so.
        let rects = bar_rects(&sbs, 100, 40);
        let first = rects.iter().find(|(s, _)| *s == 0).unwrap().1;
        let second = rects.iter().find(|(s, _)| *s == 1).unwrap().1;
        // First declared sits closest to the bottom edge; the second stacks above it.
        assert_eq!(
            first,
            Rect {
                x: 0,
                y: 34,
                width: 100,
                height: 6
            }
        );
        assert_eq!(
            second,
            Rect {
                x: 0,
                y: 30,
                width: 100,
                height: 4
            }
        );
        assert!(
            second.y + second.height <= first.y,
            "bottom sidebars overlap: {second:?} vs {first:?}"
        );
    }

    #[test]
    fn two_left_sidebars_stack_side_by_side() {
        let sbs = [
            sb(SidebarEdge::Left, 20, &[1]),
            sb(SidebarEdge::Left, 10, &[1]),
        ];
        // Also on `bar_rects`, and for the same reason as the test above.
        let rects = bar_rects(&sbs, 100, 30);
        assert_eq!(
            rects[0].1,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 30
            }
        );
        assert_eq!(
            rects[1].1,
            Rect {
                x: 20,
                y: 0,
                width: 10,
                height: 30
            }
        );
        assert_eq!(content_rect(&sbs, 100, 30).x, 30);
    }

    #[test]
    fn same_axis_budget_goes_to_the_first_declared_sidebar() {
        // Documented, deliberate rule: when two same-axis sidebars compete for a
        // budget that cannot satisfy both, the earlier entry wins and the later
        // one is force-hidden rather than both being shrunk.
        let sbs = [
            sb(SidebarEdge::Left, 60, &[1]),
            sb(SidebarEdge::Right, 60, &[1]),
        ];
        let sizes = effective_sizes(&sbs, 100, 30);
        assert_eq!(sizes[0], 60);
        assert_eq!(
            sizes[1], 20,
            "second same-axis sidebar takes only the leftover budget"
        );
        assert_eq!(content_rect(&sbs, 100, 30).width, MIN_CONTENT_COLS);
    }
}
