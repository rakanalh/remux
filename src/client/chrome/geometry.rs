//! Pure sidebar geometry.
//!
//! Splits the terminal into a content rect (handed to the server as the
//! client's `Resize`) and a set of absolutely-positioned panel rects. Kept free
//! of I/O and of plugin trait objects so every edge combination is unit-tested.

use crate::config::StatusBarPosition;
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

/// Absolute screen rects for every visible panel, as
/// `(sidebar_index, panel_index, rect)`.
///
/// Vertical sidebars span the full terminal height and stack their panels
/// vertically; the bottom sidebar spans only the columns between the verticals
/// and stacks its panels horizontally.
pub fn panel_rects(
    sidebars: &[SidebarGeom],
    term_cols: u16,
    term_rows: u16,
) -> Vec<(usize, usize, Rect)> {
    let sizes = effective_sizes(sidebars, term_cols, term_rows);
    let content = content_rect(sidebars, term_cols, term_rows);
    let mut out = Vec::new();

    // Verticals are laid out from each edge inward, so two left sidebars stack
    // side by side rather than overlapping.
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
        let vertical = !matches!(s.edge, SidebarEdge::Bottom);
        for (pi, rect) in split_panels(bar, &s.panels, vertical) {
            out.push((i, pi, rect));
        }
    }

    out
}

/// Divide `bar` among `panels` in proportion to weight, dropping any panel
/// whose share falls below its minimum and giving the remainder to the last
/// surviving panel so the division is exact.
fn split_panels(bar: Rect, panels: &[PanelGeom], vertical: bool) -> Vec<(usize, Rect)> {
    if panels.is_empty() {
        return Vec::new();
    }
    let extent = if vertical { bar.height } else { bar.width };

    // Drop panels whose weighted share cannot meet their minimum. Repeat, since
    // dropping one enlarges everyone else's share and may rescue a neighbour.
    let mut kept: Vec<usize> = (0..panels.len()).collect();
    loop {
        let total: u32 = kept.iter().map(|i| panels[*i].weight.max(1) as u32).sum();
        let Some(&victim) = kept.iter().find(|i| {
            let share = (extent as u32 * panels[**i].weight.max(1) as u32 / total) as u16;
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

    let total: u32 = kept.iter().map(|i| panels[*i].weight.max(1) as u32).sum();
    let mut out = Vec::with_capacity(kept.len());
    let mut used = 0u16;
    for (n, &i) in kept.iter().enumerate() {
        let last = n + 1 == kept.len();
        let span = if last {
            extent - used
        } else {
            (extent as u32 * panels[i].weight.max(1) as u32 / total) as u16
        };
        let rect = if vertical {
            Rect {
                x: bar.x,
                y: bar.y + used,
                width: bar.width,
                height: span,
            }
        } else {
            Rect {
                x: bar.x + used,
                y: bar.y,
                width: span,
                height: bar.height,
            }
        };
        out.push((i, rect));
        used += span;
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
        // Decision 2 in the spec: the bottom panel's rect starts after the left
        // sidebar and ends before the right one, while the verticals run the
        // full terminal height.
        let sbs = [
            sb(SidebarEdge::Left, 30, &[1]),
            sb(SidebarEdge::Right, 20, &[1]),
            sb(SidebarEdge::Bottom, 6, &[1]),
        ];
        let rects = panel_rects(&sbs, 120, 40);
        let left = rects.iter().find(|(s, _, _)| *s == 0).unwrap().2;
        let right = rects.iter().find(|(s, _, _)| *s == 1).unwrap().2;
        let bottom = rects.iter().find(|(s, _, _)| *s == 2).unwrap().2;

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
    fn stacked_panels_split_by_weight() {
        let sbs = [sb(SidebarEdge::Left, 30, &[2, 1])];
        let rects = panel_rects(&sbs, 100, 30);
        assert_eq!(rects.len(), 2);
        assert_eq!(
            rects[0].2,
            Rect {
                x: 0,
                y: 0,
                width: 30,
                height: 20
            }
        );
        assert_eq!(
            rects[1].2,
            Rect {
                x: 0,
                y: 20,
                width: 30,
                height: 10
            }
        );
    }

    #[test]
    fn weight_remainder_goes_to_the_last_panel() {
        // 30 rows over weights 1,1,1 divides evenly; 31 must not lose a row.
        let sbs = [sb(SidebarEdge::Left, 30, &[1, 1, 1])];
        let rects = panel_rects(&sbs, 100, 31);
        let total: u16 = rects.iter().map(|(_, _, r)| r.height).sum();
        assert_eq!(total, 31);
        assert_eq!(rects[2].2.height, 11);
    }

    #[test]
    fn bottom_sidebar_panels_split_horizontally() {
        let sbs = [sb(SidebarEdge::Bottom, 6, &[1, 1])];
        let rects = panel_rects(&sbs, 100, 30);
        assert_eq!(
            rects[0].2,
            Rect {
                x: 0,
                y: 24,
                width: 50,
                height: 6
            }
        );
        assert_eq!(
            rects[1].2,
            Rect {
                x: 50,
                y: 24,
                width: 50,
                height: 6
            }
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
        // 10 rows total: the second panel's weighted share is 1 row, below its
        // min of 8, so it is dropped and the first takes everything.
        let rects = panel_rects(&sbs, 100, 10);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].2.height, 10);
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
        let rects = panel_rects(&sbs, 100, 40);
        let first = rects.iter().find(|(s, _, _)| *s == 0).unwrap().2;
        let second = rects.iter().find(|(s, _, _)| *s == 1).unwrap().2;
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
        let rects = panel_rects(&sbs, 100, 30);
        assert_eq!(
            rects[0].2,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 30
            }
        );
        assert_eq!(
            rects[1].2,
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
