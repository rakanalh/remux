//! The frame a sidebar wears: one box (or one seam) per SIDEBAR, with the
//! stacked panels inside it separated by a rule.
//!
//! Sidebars are client-side chrome, so the server's compositor never draws
//! them -- but they sit flush against panes it *did* frame, and an unframed
//! strip of text beside a framed pane reads as a rendering bug. These glyphs,
//! colors and thresholds therefore mirror `server::compositor` deliberately:
//! [`crate::server::compositor::draw_zellij_border`] for the box,
//! `draw_tmux_dividers` for the seam.
//!
//! What is NOT mirrored is the pane title. `draw_zellij_border` writes
//! `build_top_border_content` into the top border, which needs a `PaneId` and a
//! stack; a sidebar has neither, and its panels put their own headings in their
//! content. The top border is plain `─` fill.

use crate::config::theme::CompositorTheme;
use crate::config::BorderStyle;
use crate::protocol::RenderCell;

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
    let bg = theme.border_bg();
    let cell = |c: char| RenderCell {
        c,
        fg: fg.clone(),
        bg: bg.clone(),
        bold: false,
        italic: false,
        underline: false,
        hyperlink: None,
        width: 1,
        combining: Vec::new(),
    };

    match style {
        BorderStyle::ZellijStyle => draw_box(grid, w, h, edge, rules, &cell),
        BorderStyle::TmuxStyle => draw_seam(grid, w, h, edge, rules, &cell),
    }
}

/// The zellij box: rounded corners, `─`/`│` edges, and `├──┤` (or `┬│┴` for the
/// bottom sidebar's horizontal stack) rules between panels.
fn draw_box(
    grid: &mut [Vec<RenderCell>],
    w: usize,
    h: usize,
    edge: SidebarEdge,
    rules: &[u16],
    cell: &dyn Fn(char) -> RenderCell,
) {
    // Guarded by `sidebar_frame`, which only reports `framed` above
    // `fits_zellij_border`; belt and braces so every write below is in bounds.
    if w < 3 || h < 3 {
        return;
    }
    grid[0][0] = cell('\u{256D}'); // ╭
    grid[0][w - 1] = cell('\u{256E}'); // ╮
    grid[h - 1][0] = cell('\u{2570}'); // ╰
    grid[h - 1][w - 1] = cell('\u{256F}'); // ╯
    let (top, rest) = grid.split_at_mut(1);
    let bottom = &mut rest[h - 2];
    for col in 1..w - 1 {
        top[0][col] = cell('\u{2500}'); // ─
        bottom[col] = cell('\u{2500}'); // ─
    }
    for row in grid.iter_mut().take(h - 1).skip(1) {
        row[0] = cell('\u{2502}'); // │
        row[w - 1] = cell('\u{2502}'); // │
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
                grid[r][0] = cell('\u{251C}'); // ├
                grid[r][w - 1] = cell('\u{2524}'); // ┤
                for c in grid[r].iter_mut().take(w - 1).skip(1) {
                    *c = cell('\u{2500}'); // ─
                }
            }
            // The bottom sidebar stacks horizontally: the rule is a full-height
            // column tee'd into the top and bottom edges.
            SidebarEdge::Bottom => {
                if r == 0 || r >= w - 1 {
                    continue;
                }
                grid[0][r] = cell('\u{252C}'); // ┬
                grid[h - 1][r] = cell('\u{2534}'); // ┴
                for row in grid.iter_mut().take(h - 1).skip(1) {
                    row[r] = cell('\u{2502}'); // │
                }
            }
        }
    }
}

/// The tmux seam: no box, just the one divider against the content plus a
/// divider between stacked panels, tee'd into the seam where they meet.
fn draw_seam(
    grid: &mut [Vec<RenderCell>],
    w: usize,
    h: usize,
    edge: SidebarEdge,
    rules: &[u16],
    cell: &dyn Fn(char) -> RenderCell,
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
            for row in grid.iter_mut().take(h) {
                row[seam] = cell('\u{2502}'); // │
            }
            for &r in rules {
                let r = r as usize;
                if r >= h {
                    continue;
                }
                for c in grid[r].iter_mut().take(w) {
                    *c = cell('\u{2500}'); // ─
                }
                grid[r][seam] = match edge {
                    SidebarEdge::Right => cell('\u{251C}'), // ├
                    _ => cell('\u{2524}'),                  // ┤
                };
            }
        }
        SidebarEdge::Bottom => {
            if h < 2 {
                return;
            }
            for c in grid[0].iter_mut().take(w) {
                *c = cell('\u{2500}'); // ─
            }
            for &r in rules {
                let r = r as usize;
                if r >= w {
                    continue;
                }
                for row in grid.iter_mut().take(h) {
                    row[r] = cell('\u{2502}'); // │
                }
                grid[0][r] = cell('\u{252C}'); // ┬
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
