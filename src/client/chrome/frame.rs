//! The frame a sidebar wears: one box (or one seam) per SIDEBAR, with the
//! stacked panels inside it separated by a rule.
//!
//! Sidebars are client-side chrome, so the server's compositor never draws
//! them -- but they sit flush against panes it *did* frame, and an unframed
//! strip of text beside a framed pane reads as a rendering bug. Nothing here
//! re-implements a border: the box comes from
//! [`draw_zellij_box`](crate::server::compositor::draw_zellij_box) and the
//! seam from
//! [`draw_divider_column`](crate::server::compositor::draw_divider_column) /
//! [`draw_divider_row`](crate::server::compositor::draw_divider_row) -- the
//! same primitives `draw_zellij_border` and `draw_tmux_dividers` call for the
//! panes, so the two cannot drift.
//!
//! What is left here is only what a sidebar has and a pane does not:
//!
//! * **no title.** `draw_zellij_border` overlays `build_top_border_content` on
//!   the top edge; a sidebar has no `PaneId` and no stack, and its panels put
//!   their own headings in their content, so it calls the shared box and
//!   overlays nothing.
//! * **rules between stacked panels.** `├┤` across a vertical sidebar, `┬┴`
//!   down the bottom one. Panes never draw these -- a pane's neighbour draws
//!   its own border -- so the junction glyphs are new, not duplicated. The runs
//!   between them are the shared ones, and every cell is built by the shared
//!   [`border_cell`](crate::server::compositor::border_cell).

use crate::config::theme::CompositorTheme;
use crate::config::BorderStyle;
use crate::protocol::RenderCell;
use crate::server::compositor::{
    border_cell, draw_divider_column, draw_divider_row, draw_zellij_box, put_cell,
};
use crate::server::layout::Rect;

use super::geometry::SidebarEdge;

/// Draw a sidebar's frame into `grid`, a bar-sized grid indexed
/// `[row][col]` in bar-local coordinates.
///
/// `rules` are bar-local offsets along the STACK axis at which a panel
/// separator goes: a row for a vertical sidebar, a column for the bottom one.
/// They come from the gaps `split_panels` left between the panel rects, so a
/// sidebar with one panel (or one whose neighbours were dropped) gets none.
///
/// `active` -- whether any of this sidebar's panels holds the keyboard --
/// selects `frame_active_fg` over `frame_fg`, the same signal a focused pane's
/// border carries, but ONLY under zellij style. tmux-style panes have no
/// focused-border treatment at all (`draw_tmux_panes` takes the focused pane
/// and never reads it; every divider is `frame_fg`), so highlighting a
/// tmux-style sidebar would make it the one framed thing on screen that reacts
/// to focus. Matching the panes beside it is the whole point of this module.
pub fn draw_sidebar_frame(
    grid: &mut [Vec<RenderCell>],
    style: &BorderStyle,
    edge: SidebarEdge,
    active: bool,
    rules: &[u16],
    theme: &CompositorTheme,
) {
    let h = grid.len();
    if h == 0 || grid[0].is_empty() {
        return;
    }
    let w = grid[0].len();

    let fg = match style {
        BorderStyle::ZellijStyle if active => theme.frame_active_fg.clone(),
        _ => theme.frame_fg.clone(),
    };

    match style {
        BorderStyle::ZellijStyle => {
            // The box, unchanged from the one the panes wear. The grid is
            // bar-local, so the bar IS the rect.
            draw_zellij_box(
                grid,
                Rect {
                    x: 0,
                    y: 0,
                    width: w as u16,
                    height: h as u16,
                },
                &fg,
                theme,
            );
            draw_box_rules(grid, w, h, edge, rules, &fg, theme);
        }
        BorderStyle::TmuxStyle => draw_seam(grid, w, h, edge, rules, &fg, theme),
    }
}

/// The `├───┤` (or `┬ │ ┴`) rules between a framed sidebar's stacked panels.
///
/// The run itself is the shared divider primitive; only the two junction
/// glyphs, which tee the rule into the box's edges, are drawn here.
fn draw_box_rules(
    grid: &mut [Vec<RenderCell>],
    w: usize,
    h: usize,
    edge: SidebarEdge,
    rules: &[u16],
    fg: &crate::protocol::CellColor,
    theme: &CompositorTheme,
) {
    // Guarded by `sidebar_frame`, which only reports `framed` above
    // `fits_zellij_border`; belt and braces so every junction below lands on a
    // box edge that exists.
    if w < 3 || h < 3 {
        return;
    }
    for &r in rules {
        let r = r as usize;
        match edge {
            // A vertical sidebar stacks its panels vertically: the rule is a
            // full-width row tee'd into both side edges.
            SidebarEdge::Left | SidebarEdge::Right => {
                if r == 0 || r >= h - 1 {
                    continue;
                }
                draw_divider_row(grid, r, 1, w - 1, fg, theme);
                put_cell(grid, r, 0, border_cell('\u{251C}', fg, theme)); // ├
                put_cell(grid, r, w - 1, border_cell('\u{2524}', fg, theme)); // ┤
            }
            // The bottom sidebar stacks horizontally: the rule is a full-height
            // column tee'd into the top and bottom edges.
            SidebarEdge::Bottom => {
                if r == 0 || r >= w - 1 {
                    continue;
                }
                draw_divider_column(grid, r, 1, h - 1, fg, theme);
                put_cell(grid, 0, r, border_cell('\u{252C}', fg, theme)); // ┬
                put_cell(grid, h - 1, r, border_cell('\u{2534}', fg, theme)); // ┴
            }
        }
    }
}

/// The tmux seam: no box, just the one divider against the content plus a
/// divider between stacked panels, tee'd into the seam where they meet.
///
/// Both runs are the shared divider primitives; the tee is the only glyph
/// choice made here.
fn draw_seam(
    grid: &mut [Vec<RenderCell>],
    w: usize,
    h: usize,
    edge: SidebarEdge,
    rules: &[u16],
    fg: &crate::protocol::CellColor,
    theme: &CompositorTheme,
) {
    match edge {
        SidebarEdge::Left | SidebarEdge::Right => {
            if w < 2 {
                return;
            }
            // The seam column is the one `sidebar_frame` kept out of the
            // interior: the last column of a left sidebar, the first of a right.
            let seam = match edge {
                SidebarEdge::Right => 0,
                _ => w - 1,
            };
            draw_divider_column(grid, seam, 0, h, fg, theme);
            for &r in rules {
                let r = r as usize;
                if r >= h {
                    continue;
                }
                draw_divider_row(grid, r, 0, w, fg, theme);
                let tee = match edge {
                    SidebarEdge::Right => '\u{251C}', // ├
                    _ => '\u{2524}',                  // ┤
                };
                put_cell(grid, r, seam, border_cell(tee, fg, theme));
            }
        }
        SidebarEdge::Bottom => {
            if h < 2 {
                return;
            }
            draw_divider_row(grid, 0, 0, w, fg, theme);
            for &r in rules {
                let r = r as usize;
                if r >= w {
                    continue;
                }
                draw_divider_column(grid, r, 0, h, fg, theme);
                put_cell(grid, 0, r, border_cell('\u{252C}', fg, theme)); // ┬
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::sidebar::blank_grid;
    use crate::config::theme::{CompositorTheme, ThemeConfig};
    use crate::protocol::CellColor;

    fn theme() -> CompositorTheme {
        CompositorTheme::from_config(&ThemeConfig::default())
    }

    fn chars(grid: &[Vec<RenderCell>]) -> Vec<String> {
        grid.iter()
            .map(|r| r.iter().map(|c| c.c).collect())
            .collect()
    }

    #[test]
    fn the_zellij_box_encloses_the_bar_and_rules_between_panels() {
        let t = theme();
        let mut g = blank_grid(6, 6, CellColor::Default);
        draw_sidebar_frame(
            &mut g,
            &BorderStyle::ZellijStyle,
            SidebarEdge::Left,
            false,
            &[3],
            &t,
        );
        assert_eq!(
            chars(&g),
            vec!["╭────╮", "│    │", "│    │", "├────┤", "│    │", "╰────╯"]
        );
    }

    /// The box a sidebar wears is byte-for-byte the one a pane wears.
    ///
    /// The point of the shared primitive: if the two ever diverge, this fails
    /// rather than the difference reaching a screen.
    #[test]
    fn the_sidebars_box_is_the_panes_box() {
        let t = theme();
        let mut sidebar = blank_grid(8, 5, CellColor::Default);
        draw_sidebar_frame(
            &mut sidebar,
            &BorderStyle::ZellijStyle,
            SidebarEdge::Left,
            false,
            &[],
            &t,
        );
        let mut pane = blank_grid(8, 5, CellColor::Default);
        crate::server::compositor::draw_zellij_box(
            &mut pane,
            Rect {
                x: 0,
                y: 0,
                width: 8,
                height: 5,
            },
            &t.frame_fg,
            &t,
        );
        assert_eq!(chars(&sidebar), chars(&pane));
        assert_eq!(sidebar[0][0].fg, pane[0][0].fg);
        assert_eq!(sidebar[0][0].bg, pane[0][0].bg);
    }

    #[test]
    fn the_bottom_sidebars_zellij_rule_is_a_column() {
        let t = theme();
        let mut g = blank_grid(6, 4, CellColor::Default);
        draw_sidebar_frame(
            &mut g,
            &BorderStyle::ZellijStyle,
            SidebarEdge::Bottom,
            false,
            &[3],
            &t,
        );
        assert_eq!(
            chars(&g),
            vec![
                "\u{256D}\u{2500}\u{2500}\u{252C}\u{2500}\u{256E}",
                "\u{2502}  \u{2502} \u{2502}",
                "\u{2502}  \u{2502} \u{2502}",
                "\u{2570}\u{2500}\u{2500}\u{2534}\u{2500}\u{256F}"
            ]
        );
    }

    #[test]
    fn a_focused_zellij_frame_uses_the_active_color() {
        let t = theme();
        let mut off = blank_grid(6, 6, CellColor::Default);
        let mut on = blank_grid(6, 6, CellColor::Default);
        draw_sidebar_frame(
            &mut off,
            &BorderStyle::ZellijStyle,
            SidebarEdge::Left,
            false,
            &[],
            &t,
        );
        draw_sidebar_frame(
            &mut on,
            &BorderStyle::ZellijStyle,
            SidebarEdge::Left,
            true,
            &[],
            &t,
        );
        assert_eq!(off[0][0].fg, t.frame_fg);
        assert_eq!(on[0][0].fg, t.frame_active_fg);
        assert_ne!(t.frame_fg, t.frame_active_fg);
    }

    #[test]
    fn the_tmux_seam_is_one_column_and_never_a_box() {
        let t = theme();
        let mut g = blank_grid(4, 3, CellColor::Default);
        draw_sidebar_frame(
            &mut g,
            &BorderStyle::TmuxStyle,
            SidebarEdge::Left,
            false,
            &[],
            &t,
        );
        assert_eq!(chars(&g), vec!["   │", "   │", "   │"]);
    }

    /// The seam cell is the cell a tmux PANE divider is made of.
    ///
    /// `draw_tmux_dividers` builds its dividers from the same `border_cell` in
    /// `theme.frame_fg`; this pins that the sidebar's seam is indistinguishable
    /// from one.
    #[test]
    fn the_seam_cell_is_a_tmux_pane_divider_cell() {
        let t = theme();
        let mut g = blank_grid(4, 3, CellColor::Default);
        draw_sidebar_frame(
            &mut g,
            &BorderStyle::TmuxStyle,
            SidebarEdge::Left,
            false,
            &[],
            &t,
        );
        let want = crate::server::compositor::border_cell('\u{2502}', &t.frame_fg, &t);
        assert_eq!(g[1][3], want);
    }

    #[test]
    fn a_focused_tmux_seam_keeps_the_inactive_color_like_a_tmux_pane_divider() {
        let t = theme();
        let mut g = blank_grid(4, 3, CellColor::Default);
        draw_sidebar_frame(
            &mut g,
            &BorderStyle::TmuxStyle,
            SidebarEdge::Left,
            true,
            &[],
            &t,
        );
        assert_eq!(g[0][3].fg, t.frame_fg);
    }
}
