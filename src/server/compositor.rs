//! Server-side compositing engine.
//!
//! Takes the current session's layout tree and all pane screens, then produces
//! a full-screen buffer of `RenderCell`s with frames/borders and a status bar.

use std::collections::HashMap;

use crate::config::FrameStyle;
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
pub fn composite(
    layout: &LayoutNode,
    pane_screens: &HashMap<PaneId, &Screen>,
    area: Rect,
    frame_style: &FrameStyle,
    status_info: &StatusInfo,
    total_cols: u16,
    total_rows: u16,
) -> Vec<Vec<RenderCell>> {
    let mut buffer = vec![vec![RenderCell::default(); total_cols as usize]; total_rows as usize];

    let pane_rects = layout::compute_layout(layout, area);

    match frame_style {
        FrameStyle::Framed => {
            draw_framed_panes(&mut buffer, &pane_rects, pane_screens, layout);
        }
        FrameStyle::Minimal => {
            draw_minimal_panes(&mut buffer, &pane_rects, pane_screens);
        }
    }

    // Draw status bar on the last row.
    draw_status_bar(&mut buffer, total_cols, total_rows, status_info);

    buffer
}

// ---------------------------------------------------------------------------
// Framed style (zellij-like)
// ---------------------------------------------------------------------------

/// Draw panes with box-drawing character borders.
///
/// Uses a two-pass approach: first blit all pane content, then draw all
/// borders on top. This prevents a later pane's blit from overwriting a
/// border drawn by an earlier pane (e.g. horizontal split separator).
fn draw_framed_panes(
    buffer: &mut [Vec<RenderCell>],
    pane_rects: &[(PaneId, Rect)],
    pane_screens: &HashMap<PaneId, &Screen>,
    layout: &LayoutNode,
) {
    // Pass 1: blit all pane screen content.
    for &(pane_id, rect) in pane_rects {
        if rect.width == 0 || rect.height == 0 {
            continue;
        }

        let screen = match pane_screens.get(&pane_id) {
            Some(s) => s,
            None => continue,
        };

        blit_screen(buffer, screen, rect);
    }

    // Pass 2: draw borders on top of content.
    if pane_rects.len() > 1 {
        for &(pane_id, rect) in pane_rects {
            if rect.width == 0 || rect.height == 0 {
                continue;
            }

            let stack_panes = layout::find_stack_for_pane(layout, pane_id);
            let tab_labels: Vec<String> = match &stack_panes {
                Some(panes) if panes.len() > 1 => panes.iter().map(|p| format!("{p}")).collect(),
                _ => Vec::new(),
            };

            draw_pane_border(buffer, rect, &tab_labels, pane_id);
        }
    }
}

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

/// Draw a box-drawing border around a pane rectangle.
fn draw_pane_border(
    buffer: &mut [Vec<RenderCell>],
    rect: Rect,
    _tab_labels: &[String],
    _active_pane: PaneId,
) {
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.width as usize;
    let h = rect.height as usize;

    let border_cell = |c: char| RenderCell {
        c,
        fg: CellColor::Indexed(8), // Dark grey
        bg: CellColor::Default,
        bold: false,
        italic: false,
        underline: false,
    };

    // We only draw partial borders (right and bottom edges) to avoid
    // double-drawing. The leftmost and topmost panes will have their borders
    // at the screen edge.

    // Right edge.
    if x + w < buffer.first().map_or(0, |r| r.len()) {
        for row in y..y + h {
            if row < buffer.len() {
                buffer[row][x + w - 1] = border_cell('\u{2502}'); // │
            }
        }
    }

    // Bottom edge.
    if y + h < buffer.len() {
        for col in x..x + w {
            if col < buffer[y + h].len() {
                buffer[y + h][col] = border_cell('\u{2500}'); // ─
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal style (tmux-like)
// ---------------------------------------------------------------------------

/// Draw panes with minimal dividers (no full borders).
fn draw_minimal_panes(
    buffer: &mut [Vec<RenderCell>],
    pane_rects: &[(PaneId, Rect)],
    pane_screens: &HashMap<PaneId, &Screen>,
) {
    for &(pane_id, rect) in pane_rects {
        if rect.width == 0 || rect.height == 0 {
            continue;
        }

        let screen = match pane_screens.get(&pane_id) {
            Some(s) => s,
            None => continue,
        };

        blit_screen(buffer, screen, rect);
    }

    // Draw dividers between adjacent panes.
    if pane_rects.len() > 1 {
        draw_minimal_dividers(buffer, pane_rects);
    }
}

/// Draw simple divider lines between adjacent panes.
fn draw_minimal_dividers(buffer: &mut [Vec<RenderCell>], pane_rects: &[(PaneId, Rect)]) {
    let divider_cell = |c: char| RenderCell {
        c,
        fg: CellColor::Indexed(8),
        bg: CellColor::Default,
        bold: false,
        italic: false,
        underline: false,
    };

    // Check for vertical boundaries (right edge of one pane == left edge of next).
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
            &FrameStyle::Minimal,
            &status,
            10,
            5,
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
            &FrameStyle::Minimal,
            &status,
            20,
            5,
        );

        // The last row should have the mode indicator.
        let last_row = &result[4];
        let text: String = last_row.iter().map(|c| c.c).collect();
        assert!(text.contains("NORMAL"));
    }

    #[test]
    fn test_horizontal_split_framed_has_separator() {
        // Horizontal split: pane 1 on top, pane 2 on bottom.
        let mut layout = LayoutNode::new_stack(1);
        layout.split_horizontal(1, 2);

        let screen1 = Screen::new(20, 5, 100);
        let screen2 = Screen::new(20, 5, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen1);
        pane_screens.insert(2, &screen2);

        // 10 rows for panes + 1 for status bar = 11 total.
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
            &FrameStyle::Framed,
            &status,
            20,
            11,
        );

        // The separator should be at row 5 (bottom edge of the top pane which
        // has height 5). The framed style draws the bottom border at y + h.
        let separator_row = &result[5];
        let has_horizontal_border = separator_row.iter().any(|cell| cell.c == '\u{2500}'); // ─
        assert!(
            has_horizontal_border,
            "expected horizontal border character at row 5, got: {:?}",
            separator_row.iter().map(|c| c.c).collect::<String>()
        );
    }
}
