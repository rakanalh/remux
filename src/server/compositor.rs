//! Server-side compositing engine.
//!
//! Takes the current session's layout tree and all pane screens, then produces
//! a full-screen buffer of `RenderCell`s with frames/borders and a status bar.

use std::collections::HashMap;

use crate::config::GapMode;
use crate::protocol::{CellColor, RenderCell};
use crate::screen::{Cell, Color, Screen};
use crate::server::layout::{self, LayoutNode, PaneId, Rect};

// ---------------------------------------------------------------------------
// Status info (passed from the daemon)
// ---------------------------------------------------------------------------

/// Information needed to render the status bar.
pub struct StatusInfo {
    /// Current mode name (e.g. "INSERT", "NORMAL", "VISUAL").
    pub mode: String,
    /// Session name or path.
    pub session_name: String,
    /// Tab list: (name, is_active) pairs.
    pub tabs: Vec<(String, bool)>,
}

// ---------------------------------------------------------------------------
// Color conversion
// ---------------------------------------------------------------------------

/// Convert a screen `Color` to a protocol `CellColor`.
fn convert_color(color: &Color) -> CellColor {
    match color {
        Color::Default => CellColor::Default,
        Color::Indexed(idx) => CellColor::Indexed(*idx),
        Color::Rgb(r, g, b) => CellColor::Rgb(*r, *g, *b),
    }
}

/// Convert a screen `Cell` to a protocol `RenderCell`.
fn cell_to_render_cell(cell: &Cell) -> RenderCell {
    RenderCell {
        c: cell.c,
        fg: convert_color(&cell.attrs.fg),
        bg: convert_color(&cell.attrs.bg),
        bold: cell.attrs.bold,
        italic: cell.attrs.italic,
        underline: cell.attrs.underline,
    }
}

// ---------------------------------------------------------------------------
// Compositing
// ---------------------------------------------------------------------------

/// Composite a full screen buffer from the layout, pane screens, frames,
/// and status bar.
///
/// `area` is the rectangle available for pane content (excluding the status bar).
/// `total_cols` and `total_rows` are the full terminal dimensions.
/// `focused_pane` is the currently focused pane, used for active border highlighting.
#[allow(clippy::too_many_arguments)]
pub fn composite(
    layout: &LayoutNode,
    pane_screens: &HashMap<PaneId, &Screen>,
    area: Rect,
    gap_mode: &GapMode,
    status_info: &StatusInfo,
    total_cols: u16,
    total_rows: u16,
    gap_size: u16,
    focused_pane: PaneId,
) -> Vec<Vec<RenderCell>> {
    let mut buffer = vec![vec![RenderCell::default(); total_cols as usize]; total_rows as usize];

    let pane_rects = layout::compute_layout(layout, area, gap_size);

    match gap_mode {
        GapMode::ZellijStyle => {
            draw_zellij_panes(&mut buffer, &pane_rects, pane_screens, layout, focused_pane);
        }
        GapMode::TmuxStyle => {
            // TmuxStyle always uses gap_size=0, enforced at the caller level
            // (daemon.rs). Content is edge-to-edge with minimal dividers.
            draw_tmux_panes(&mut buffer, &pane_rects, pane_screens, layout, focused_pane);
        }
    }

    // Draw status bar on the last row.
    draw_status_bar(&mut buffer, total_cols, total_rows, status_info);

    buffer
}

// ---------------------------------------------------------------------------
// Zellij-style rendering (full box borders with rounded corners)
// ---------------------------------------------------------------------------

/// Draw panes with full box-drawing borders using rounded corners.
///
/// Every pane gets a border (including single-pane layouts). The active pane
/// gets a green border; inactive panes get dark grey. Stacked panes show
/// tab names in the top border.
fn draw_zellij_panes(
    buffer: &mut [Vec<RenderCell>],
    pane_rects: &[(PaneId, Rect)],
    pane_screens: &HashMap<PaneId, &Screen>,
    layout: &LayoutNode,
    focused_pane: PaneId,
) {
    for &(pane_id, rect) in pane_rects {
        if rect.width == 0 || rect.height == 0 {
            continue;
        }

        let screen = match pane_screens.get(&pane_id) {
            Some(s) => s,
            None => continue,
        };

        let is_active = pane_id == focused_pane;
        let border_fg = if is_active {
            CellColor::Indexed(2) // Green for active pane
        } else {
            CellColor::Indexed(8) // Dark grey for inactive panes
        };

        // If rect is too small for borders, blit content to full rect.
        if rect.width < 3 || rect.height < 3 {
            blit_screen(buffer, screen, rect);
            continue;
        }

        // Blit screen content to inner area (inside border).
        let inner = Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width - 2,
            height: rect.height - 2,
        };
        blit_screen(buffer, screen, inner);

        // Draw the full box border with rounded corners.
        let x = rect.x as usize;
        let y = rect.y as usize;
        let w = rect.width as usize;
        let h = rect.height as usize;

        let border_cell = |c: char| RenderCell {
            c,
            fg: border_fg.clone(),
            bg: CellColor::Default,
            bold: false,
            italic: false,
            underline: false,
        };

        // Top-left corner.
        set_cell(buffer, y, x, border_cell('\u{256D}')); // ╭

        // Top-right corner.
        if x + w > 0 {
            set_cell(buffer, y, x + w - 1, border_cell('\u{256E}')); // ╮
        }

        // Bottom-left corner.
        if y + h > 0 {
            set_cell(buffer, y + h - 1, x, border_cell('\u{2570}')); // ╰
        }

        // Bottom-right corner.
        if x + w > 0 && y + h > 0 {
            set_cell(buffer, y + h - 1, x + w - 1, border_cell('\u{256F}')); // ╯
        }

        // Build the top border content (with pane name / tab labels).
        let stack_info = layout::find_stack_names(layout, pane_id);
        let top_content = build_top_border_content(&stack_info, pane_id, &border_fg);

        // Write the top border: fill between corners.
        let top_start = x + 1;
        let top_end = x + w - 1;
        let mut col = top_start;

        // Write tab content cells.
        for cell in &top_content {
            if col >= top_end {
                break;
            }
            set_cell(buffer, y, col, cell.clone());
            col += 1;
        }

        // Fill remaining top border with ─.
        while col < top_end {
            set_cell(buffer, y, col, border_cell('\u{2500}')); // ─
            col += 1;
        }

        // Bottom border (fill between corners).
        for col in (x + 1)..(x + w - 1) {
            set_cell(buffer, y + h - 1, col, border_cell('\u{2500}')); // ─
        }

        // Left edge (between corners).
        for row in (y + 1)..(y + h - 1) {
            set_cell(buffer, row, x, border_cell('\u{2502}')); // │
        }

        // Right edge (between corners).
        for row in (y + 1)..(y + h - 1) {
            set_cell(buffer, row, x + w - 1, border_cell('\u{2502}')); // │
        }
    }
}

/// Build the render cells for the top border content (pane name or tab labels).
///
/// For single-pane stacks: ` name ` (space-padded name).
/// For multi-pane stacks: ` name1 | name2 | name3 ` with active tab highlighted.
fn build_top_border_content(
    stack_info: &Option<(Vec<String>, Vec<PaneId>, usize)>,
    pane_id: PaneId,
    border_fg: &CellColor,
) -> Vec<RenderCell> {
    let mut cells = Vec::new();

    let (names, pane_ids, active_idx) = match stack_info {
        Some((n, p, a)) => (n, p, *a),
        None => return cells,
    };

    let is_multi = pane_ids.len() > 1;

    if !is_multi {
        // Single pane: show name if non-empty.
        let name = names.first().map(|s| s.as_str()).unwrap_or("");
        if !name.is_empty() {
            // Leading space.
            cells.push(RenderCell {
                c: ' ',
                fg: border_fg.clone(),
                bg: CellColor::Default,
                bold: false,
                italic: false,
                underline: false,
            });
            for ch in name.chars() {
                cells.push(RenderCell {
                    c: ch,
                    fg: border_fg.clone(),
                    bg: CellColor::Default,
                    bold: false,
                    italic: false,
                    underline: false,
                });
            }
            cells.push(RenderCell {
                c: ' ',
                fg: border_fg.clone(),
                bg: CellColor::Default,
                bold: false,
                italic: false,
                underline: false,
            });
        }
    } else {
        // Multi-pane stack: show all names as tabs.
        // Leading space.
        cells.push(RenderCell {
            c: ' ',
            fg: border_fg.clone(),
            bg: CellColor::Default,
            bold: false,
            italic: false,
            underline: false,
        });

        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                // Separator: " | "
                for sep_ch in [' ', '|', ' '] {
                    cells.push(RenderCell {
                        c: sep_ch,
                        fg: CellColor::Indexed(8), // dark grey separator
                        bg: CellColor::Default,
                        bold: false,
                        italic: false,
                        underline: false,
                    });
                }
            }

            let is_this_active = i == active_idx;
            let display_name = if name.is_empty() {
                format!("{}", pane_ids[i])
            } else {
                name.clone()
            };

            let (tab_fg, tab_bold) = if is_this_active {
                (CellColor::Indexed(15), true) // White, bold for active tab
            } else {
                (CellColor::Indexed(8), false) // Dark grey for inactive tabs
            };

            // Highlight: also check if this is the pane we're currently rendering
            // (in case of stacked panes, only the active pane is rendered)
            let _ = pane_id; // pane_id is always the active one being rendered

            for ch in display_name.chars() {
                cells.push(RenderCell {
                    c: ch,
                    fg: tab_fg.clone(),
                    bg: CellColor::Default,
                    bold: tab_bold,
                    italic: false,
                    underline: false,
                });
            }
        }

        // Trailing space.
        cells.push(RenderCell {
            c: ' ',
            fg: border_fg.clone(),
            bg: CellColor::Default,
            bold: false,
            italic: false,
            underline: false,
        });
    }

    cells
}

// ---------------------------------------------------------------------------
// Tmux-style rendering (edge-to-edge content with minimal dividers)
// ---------------------------------------------------------------------------

/// Draw panes with edge-to-edge content and minimal dividers between adjacent panes.
///
/// For stacks with more than one pane, a 1-row tab bar is rendered at the top
/// of the pane area. For single-pane stacks, the full area is used for content.
fn draw_tmux_panes(
    buffer: &mut [Vec<RenderCell>],
    pane_rects: &[(PaneId, Rect)],
    pane_screens: &HashMap<PaneId, &Screen>,
    layout: &LayoutNode,
    _focused_pane: PaneId,
) {
    for &(pane_id, rect) in pane_rects {
        if rect.width == 0 || rect.height == 0 {
            continue;
        }

        let screen = match pane_screens.get(&pane_id) {
            Some(s) => s,
            None => continue,
        };

        let stack_info = layout::find_stack_names(layout, pane_id);
        let is_multi = stack_info
            .as_ref()
            .map(|(_, panes, _)| panes.len() > 1)
            .unwrap_or(false);

        if is_multi && rect.height >= 2 {
            // Draw 1-row tab bar at the top, content below.
            draw_tmux_tab_bar(buffer, rect, &stack_info);

            let content_rect = Rect {
                x: rect.x,
                y: rect.y + 1,
                width: rect.width,
                height: rect.height - 1,
            };
            blit_screen(buffer, screen, content_rect);
        } else {
            // Single-pane stack or too small: full content area.
            blit_screen(buffer, screen, rect);
        }
    }

    // Draw dividers between adjacent panes.
    if pane_rects.len() > 1 {
        draw_tmux_dividers(buffer, pane_rects);
    }
}

/// Draw a 1-row tab bar at the top of a pane rect for multi-pane stacks.
fn draw_tmux_tab_bar(
    buffer: &mut [Vec<RenderCell>],
    rect: Rect,
    stack_info: &Option<(Vec<String>, Vec<PaneId>, usize)>,
) {
    let y = rect.y as usize;
    let x_start = rect.x as usize;
    let x_end = (rect.x + rect.width) as usize;

    // Fill tab bar background.
    for col in x_start..x_end {
        set_cell(
            buffer,
            y,
            col,
            RenderCell {
                c: ' ',
                fg: CellColor::Indexed(7),
                bg: CellColor::Indexed(8), // Dark grey background
                bold: false,
                italic: false,
                underline: false,
            },
        );
    }

    let (names, pane_ids, active_idx) = match stack_info {
        Some((n, p, a)) => (n, p, *a),
        None => return,
    };

    let mut col = x_start;

    for (i, name) in names.iter().enumerate() {
        if col >= x_end {
            break;
        }

        if i > 0 {
            // Separator: " | "
            for sep_ch in [' ', '|', ' '] {
                if col < x_end {
                    set_cell(
                        buffer,
                        y,
                        col,
                        RenderCell {
                            c: sep_ch,
                            fg: CellColor::Indexed(7),
                            bg: CellColor::Indexed(8),
                            bold: false,
                            italic: false,
                            underline: false,
                        },
                    );
                    col += 1;
                }
            }
        }

        let is_active = i == active_idx;
        let display_name = if name.is_empty() {
            format!("{}", pane_ids[i])
        } else {
            name.clone()
        };

        let (fg, bold) = if is_active {
            (CellColor::Indexed(15), true) // White, bold for active
        } else {
            (CellColor::Indexed(7), false) // Grey for inactive
        };

        for ch in display_name.chars() {
            if col >= x_end {
                break;
            }
            set_cell(
                buffer,
                y,
                col,
                RenderCell {
                    c: ch,
                    fg: fg.clone(),
                    bg: CellColor::Indexed(8),
                    bold,
                    italic: false,
                    underline: false,
                },
            );
            col += 1;
        }
    }
}

/// Draw simple divider lines between adjacent panes (tmux style).
fn draw_tmux_dividers(buffer: &mut [Vec<RenderCell>], pane_rects: &[(PaneId, Rect)]) {
    let divider_cell = |c: char| RenderCell {
        c,
        fg: CellColor::Indexed(8),
        bg: CellColor::Default,
        bold: false,
        italic: false,
        underline: false,
    };

    for i in 0..pane_rects.len() {
        for j in (i + 1)..pane_rects.len() {
            let (_, r1) = pane_rects[i];
            let (_, r2) = pane_rects[j];

            // Vertical divider: r1 is left of r2.
            if r1.x + r1.width == r2.x {
                let top = r1.y.max(r2.y) as usize;
                let bottom = (r1.y + r1.height).min(r2.y + r2.height) as usize;
                let col = r2.x as usize;
                if col > 0 {
                    for row in top..bottom {
                        if row < buffer.len() && (col - 1) < buffer[row].len() {
                            buffer[row][col - 1] = divider_cell('\u{2502}'); // │
                        }
                    }
                }
            }

            // Horizontal divider: r1 is above r2.
            if r1.y + r1.height == r2.y {
                let left = r1.x.max(r2.x) as usize;
                let right = (r1.x + r1.width).min(r2.x + r2.width) as usize;
                let row = r2.y as usize;
                if row > 0 {
                    for c in left..right {
                        if (row - 1) < buffer.len() && c < buffer[row - 1].len() {
                            buffer[row - 1][c] = divider_cell('\u{2500}'); // ─
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Blit a screen's content into the buffer at the given rectangle.
fn blit_screen(buffer: &mut [Vec<RenderCell>], screen: &Screen, rect: Rect) {
    for row in 0..rect.height as usize {
        let buf_y = rect.y as usize + row;
        if buf_y >= buffer.len() {
            break;
        }
        for col in 0..rect.width as usize {
            let buf_x = rect.x as usize + col;
            if buf_x >= buffer[buf_y].len() {
                break;
            }
            if row < screen.grid.len() && col < screen.grid[row].len() {
                buffer[buf_y][buf_x] = cell_to_render_cell(&screen.grid[row][col]);
            }
        }
    }
}

/// Safely set a cell in the buffer (bounds-checked).
fn set_cell(buffer: &mut [Vec<RenderCell>], row: usize, col: usize, cell: RenderCell) {
    if row < buffer.len() && col < buffer[row].len() {
        buffer[row][col] = cell;
    }
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

/// Draw the status bar on the last row of the buffer.
fn draw_status_bar(buffer: &mut [Vec<RenderCell>], cols: u16, rows: u16, info: &StatusInfo) {
    let bar_row = (rows as usize).saturating_sub(1);
    if bar_row >= buffer.len() {
        return;
    }

    let cols = cols as usize;

    // Fill the status bar background.
    for col in 0..cols {
        if col < buffer[bar_row].len() {
            buffer[bar_row][col] = RenderCell {
                c: ' ',
                fg: CellColor::Indexed(15), // White
                bg: CellColor::Indexed(8),  // Dark grey
                bold: false,
                italic: false,
                underline: false,
            };
        }
    }

    // Mode indicator.
    let mode_str = format!(" [{}] ", info.mode);
    let (mode_fg, mode_bg) = match info.mode.as_str() {
        "INSERT" => (CellColor::Indexed(0), CellColor::Indexed(2)), // Black on green
        "NORMAL" => (CellColor::Indexed(0), CellColor::Indexed(4)), // Black on blue
        "VISUAL" => (CellColor::Indexed(0), CellColor::Indexed(5)), // Black on magenta
        _ => (CellColor::Indexed(15), CellColor::Indexed(8)),
    };

    let mut x = 0;
    for ch in mode_str.chars() {
        if x < cols && x < buffer[bar_row].len() {
            buffer[bar_row][x] = RenderCell {
                c: ch,
                fg: mode_fg.clone(),
                bg: mode_bg.clone(),
                bold: true,
                italic: false,
                underline: false,
            };
        }
        x += 1;
    }

    // Session name.
    let session_str = format!(" {} ", info.session_name);
    for ch in session_str.chars() {
        if x < cols && x < buffer[bar_row].len() {
            buffer[bar_row][x] = RenderCell {
                c: ch,
                fg: CellColor::Indexed(15),
                bg: CellColor::Indexed(8),
                bold: false,
                italic: false,
                underline: false,
            };
        }
        x += 1;
    }

    // Separator.
    if x < cols && x < buffer[bar_row].len() {
        buffer[bar_row][x] = RenderCell {
            c: '\u{2502}',
            fg: CellColor::Indexed(7),
            bg: CellColor::Indexed(8),
            bold: false,
            italic: false,
            underline: false,
        };
        x += 1;
    }

    // Tab list.
    for (i, (tab_name, is_active)) in info.tabs.iter().enumerate() {
        if i > 0 {
            // Tab separator.
            let sep = " | ";
            for ch in sep.chars() {
                if x < cols && x < buffer[bar_row].len() {
                    buffer[bar_row][x] = RenderCell {
                        c: ch,
                        fg: CellColor::Indexed(7),
                        bg: CellColor::Indexed(8),
                        bold: false,
                        italic: false,
                        underline: false,
                    };
                }
                x += 1;
            }
        }

        let tab_str = if *is_active {
            format!(" *{tab_name}* ")
        } else {
            format!(" {tab_name} ")
        };

        let (tab_fg, tab_bg, tab_bold) = if *is_active {
            (CellColor::Indexed(0), CellColor::Indexed(15), true)
        } else {
            (CellColor::Indexed(7), CellColor::Indexed(8), false)
        };

        for ch in tab_str.chars() {
            if x < cols && x < buffer[bar_row].len() {
                buffer[bar_row][x] = RenderCell {
                    c: ch,
                    fg: tab_fg.clone(),
                    bg: tab_bg.clone(),
                    bold: tab_bold,
                    italic: false,
                    underline: false,
                };
            }
            x += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_composite_single_pane() {
        let layout = LayoutNode::new_stack(1);
        let screen = Screen::new(10, 5, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen);

        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 4,
        };
        let status = StatusInfo {
            mode: "INSERT".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::TmuxStyle,
            &status,
            10,
            5,
            0,
            1,
        );

        assert_eq!(result.len(), 5);
        assert_eq!(result[0].len(), 10);
    }

    #[test]
    fn test_convert_color() {
        assert_eq!(convert_color(&Color::Default), CellColor::Default);
        assert_eq!(convert_color(&Color::Indexed(5)), CellColor::Indexed(5));
        assert_eq!(
            convert_color(&Color::Rgb(10, 20, 30)),
            CellColor::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn test_status_bar_drawn() {
        let layout = LayoutNode::new_stack(1);
        let screen = Screen::new(20, 5, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 4,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "main".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::TmuxStyle,
            &status,
            20,
            5,
            0,
            1,
        );

        // The last row should have the mode indicator.
        let last_row = &result[4];
        let text: String = last_row.iter().map(|c| c.c).collect();
        assert!(text.contains("NORMAL"));
    }

    #[test]
    fn test_gap_cells_are_default() {
        // Vertical split with gap_size=2 should leave gap columns as default cells.
        let mut layout = LayoutNode::new_stack(1);
        layout.split_vertical(1, 2);

        let screen1 = Screen::new(20, 8, 100);
        let screen2 = Screen::new(20, 8, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen1);
        pane_screens.insert(2, &screen2);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 8,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let gap_size = 2;
        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::TmuxStyle,
            &status,
            20,
            9,
            gap_size,
            1,
        );

        // Compute the pane rects to find where the gap is.
        let pane_rects = layout::compute_layout(&layout, area, gap_size);
        let (_, r1) = pane_rects[0];
        let (_, r2) = pane_rects[1];
        // The gap is between r1.x + r1.width and r2.x.
        let gap_start = (r1.x + r1.width) as usize;
        let gap_end = r2.x as usize;
        assert!(gap_end > gap_start, "gap region should be non-empty");

        // Verify gap columns contain default cells in content rows.
        let default_cell = RenderCell::default();
        for (row, row_cells) in result.iter().enumerate().take(area.height as usize) {
            for (col, cell) in row_cells.iter().enumerate().take(gap_end).skip(gap_start) {
                assert_eq!(
                    *cell, default_cell,
                    "gap cell at ({col}, {row}) should be default"
                );
            }
        }
    }

    #[test]
    fn test_zellij_single_pane_has_border() {
        let layout = LayoutNode::new_stack(1);
        let screen = Screen::new(18, 8, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::ZellijStyle,
            &status,
            20,
            11,
            0,
            1,
        );

        // Top-left corner should be ╭.
        assert_eq!(result[0][0].c, '\u{256D}');
        // Top-right corner should be ╮.
        assert_eq!(result[0][19].c, '\u{256E}');
        // Bottom-left corner should be ╰.
        assert_eq!(result[9][0].c, '\u{2570}');
        // Bottom-right corner should be ╯.
        assert_eq!(result[9][19].c, '\u{256F}');
    }

    #[test]
    fn test_zellij_active_pane_green_border() {
        let mut layout = LayoutNode::new_stack(1);
        layout.split_vertical(1, 2);

        let screen1 = Screen::new(10, 10, 100);
        let screen2 = Screen::new(10, 10, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen1);
        pane_screens.insert(2, &screen2);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::ZellijStyle,
            &status,
            20,
            11,
            0,
            1, // pane 1 is focused
        );

        // Active pane (1) top-left corner should be green (Indexed(2)).
        assert_eq!(result[0][0].fg, CellColor::Indexed(2));

        // Inactive pane (2) should have dark grey border.
        let pane_rects = layout::compute_layout(&layout, area, 0);
        let (_, r2) = pane_rects[1];
        assert_eq!(
            result[r2.y as usize][r2.x as usize].fg,
            CellColor::Indexed(8)
        );
    }

    #[test]
    fn test_zellij_horizontal_split_has_borders() {
        let mut layout = LayoutNode::new_stack(1);
        layout.split_horizontal(1, 2);

        let screen1 = Screen::new(20, 5, 100);
        let screen2 = Screen::new(20, 5, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen1);
        pane_screens.insert(2, &screen2);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::ZellijStyle,
            &status,
            20,
            11,
            0,
            1,
        );

        // Both panes should have rounded corners.
        assert_eq!(result[0][0].c, '\u{256D}'); // Top pane top-left
        assert_eq!(result[0][19].c, '\u{256E}'); // Top pane top-right

        // Check that horizontal border characters exist somewhere.
        let has_horizontal = result.iter().take(10).any(|row| {
            row.iter()
                .any(|cell| cell.c == '\u{2500}' || cell.c == '\u{2570}' || cell.c == '\u{256F}')
        });
        assert!(has_horizontal, "expected horizontal border characters");
    }

    #[test]
    fn test_tmux_single_pane_no_tab_bar() {
        let layout = LayoutNode::new_stack(1);
        let screen = Screen::new(20, 10, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::TmuxStyle,
            &status,
            20,
            11,
            0,
            1,
        );

        // First row should not have tab bar (dark grey background).
        // It should just have the pane content (default bg for empty screen).
        assert_ne!(result[0][0].bg, CellColor::Indexed(8));
    }

    #[test]
    fn test_tmux_multi_pane_stack_has_tab_bar() {
        let mut layout = LayoutNode::new_stack(1);
        layout.add_to_stack(1, 2);

        let screen1 = Screen::new(20, 10, 100);
        let screen2 = Screen::new(20, 10, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen1);
        pane_screens.insert(2, &screen2);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        // Pane 2 is active (last added).
        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::TmuxStyle,
            &status,
            20,
            11,
            0,
            2,
        );

        // First row should be the tab bar with dark grey background.
        assert_eq!(result[0][0].bg, CellColor::Indexed(8));
    }

    #[test]
    fn test_zellij_pane_name_in_border() {
        let mut layout = LayoutNode::new_stack(1);
        layout::set_pane_name(&mut layout, 1, "myshell");

        let screen = Screen::new(18, 8, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::ZellijStyle,
            &status,
            20,
            11,
            0,
            1,
        );

        // The top row should contain the pane name "myshell".
        let top_row: String = result[0].iter().map(|c| c.c).collect();
        assert!(
            top_row.contains("myshell"),
            "expected pane name 'myshell' in top border, got: {top_row}"
        );
    }

    #[test]
    fn test_zellij_stacked_tabs_in_border() {
        let mut layout = LayoutNode::new_stack(1);
        layout.add_to_stack(1, 2);
        layout::set_pane_name(&mut layout, 1, "vim");
        layout::set_pane_name(&mut layout, 2, "cargo");

        let screen = Screen::new(28, 8, 100);
        let mut pane_screens = HashMap::new();
        // Only the active pane (2) is rendered in compute_layout.
        pane_screens.insert(2, &screen);

        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::ZellijStyle,
            &status,
            30,
            11,
            0,
            2,
        );

        // The top row should contain both stacked pane names.
        let top_row: String = result[0].iter().map(|c| c.c).collect();
        assert!(
            top_row.contains("vim"),
            "expected 'vim' in top border, got: {top_row}"
        );
        assert!(
            top_row.contains("cargo"),
            "expected 'cargo' in top border, got: {top_row}"
        );
    }

    #[test]
    fn test_tmux_single_pane_no_border() {
        let layout = LayoutNode::new_stack(1);
        let screen = Screen::new(20, 10, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::TmuxStyle,
            &status,
            20,
            11,
            0,
            1,
        );

        // No border characters should appear in the pane area.
        let border_chars = [
            '\u{256D}', '\u{256E}', '\u{2570}', '\u{256F}', '\u{2502}', '\u{2500}',
        ];
        for (row, row_cells) in result.iter().enumerate().take(10) {
            for (col, cell) in row_cells.iter().enumerate().take(20) {
                assert!(
                    !border_chars.contains(&cell.c),
                    "unexpected border character '{}' at ({col}, {row})",
                    cell.c
                );
            }
        }
    }

    #[test]
    fn test_tmux_stacked_tab_bar() {
        let mut layout = LayoutNode::new_stack(1);
        layout.add_to_stack(1, 2);
        layout.add_to_stack(2, 3);
        layout::set_pane_name(&mut layout, 1, "bash");
        layout::set_pane_name(&mut layout, 2, "vim");
        layout::set_pane_name(&mut layout, 3, "htop");

        let screen = Screen::new(40, 10, 100);
        let mut pane_screens = HashMap::new();
        // Active pane is 3 (last added).
        pane_screens.insert(3, &screen);

        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::TmuxStyle,
            &status,
            40,
            11,
            0,
            3,
        );

        // First row should be a tab bar with all pane names.
        let tab_row: String = result[0].iter().map(|c| c.c).collect();
        assert!(
            tab_row.contains("bash"),
            "expected 'bash' in tab bar, got: {tab_row}"
        );
        assert!(
            tab_row.contains("vim"),
            "expected 'vim' in tab bar, got: {tab_row}"
        );
        assert!(
            tab_row.contains("htop"),
            "expected 'htop' in tab bar, got: {tab_row}"
        );
    }

    #[test]
    fn test_tmux_dividers_between_splits() {
        let mut layout = LayoutNode::new_stack(1);
        layout.split_vertical(1, 2);

        let screen1 = Screen::new(10, 10, 100);
        let screen2 = Screen::new(10, 10, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen1);
        pane_screens.insert(2, &screen2);

        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("tab-1".to_string(), true)],
        };

        let result = composite(
            &layout,
            &pane_screens,
            area,
            &GapMode::TmuxStyle,
            &status,
            20,
            11,
            0,
            1,
        );

        // There should be vertical divider characters between the two panes.
        let pane_rects = layout::compute_layout(&layout, area, 0);
        let (_, r1) = pane_rects[0];
        let divider_col = (r1.x + r1.width - 1) as usize;
        let has_divider = (0..10).any(|row| result[row][divider_col].c == '\u{2502}');
        assert!(has_divider, "expected vertical divider between panes");
    }
}
