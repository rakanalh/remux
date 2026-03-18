//! Server-side compositing engine.
//!
//! Takes the current session's layout tree and all pane screens, then produces
//! a full-screen buffer of `RenderCell`s with frames/borders and a status bar.

use std::collections::HashMap;

use crate::config::theme::CompositorTheme;
use crate::config::BorderStyle;
use crate::protocol::{CellColor, RenderCell};
use crate::screen::{Cell, Color, Screen};
use crate::server::layout::{self, LayoutNode, PaneId, Rect};

// ---------------------------------------------------------------------------
// Mouse selection (shared with daemon)
// ---------------------------------------------------------------------------

/// Describes an active mouse text selection for a specific pane.
///
/// Coordinates are in pane-local space (relative to the pane's content area).
#[derive(Debug, Clone)]
pub struct MouseSelection {
    /// The pane that owns this selection.
    pub pane_id: PaneId,
    /// Start position (col, row) in pane-local coordinates.
    pub start: (u16, u16),
    /// End position (col, row) in pane-local coordinates.
    pub end: (u16, u16),
}

// ---------------------------------------------------------------------------
// Hit testing
// ---------------------------------------------------------------------------

/// Result of a hit test at a given screen coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickTarget {
    /// Click landed inside a pane's content area.
    Pane(PaneId),
    /// Click landed on a tab label in the status bar.
    Tab(usize),
    /// Click landed on a stack label (pane tab in a multi-pane stack header).
    StackLabel(PaneId),
    /// Click did not hit any interactive region.
    None,
}

/// A tracked screen region for a tab label.
#[derive(Debug, Clone)]
pub struct TabRegion {
    pub x_start: u16,
    pub x_end: u16,
    pub y: u16,
    pub tab_index: usize,
}

/// A tracked screen region for a stack (pane tab) label.
#[derive(Debug, Clone)]
pub struct StackRegion {
    pub x_start: u16,
    pub x_end: u16,
    pub y: u16,
    pub pane_id: PaneId,
}

/// Regions collected during compositing for hit testing.
#[derive(Debug, Clone, Default)]
pub struct HitRegions {
    pub tab_regions: Vec<TabRegion>,
    pub stack_regions: Vec<StackRegion>,
}

/// Perform a hit test at the given screen coordinates.
///
/// Checks tab labels first, then stack labels, then pane content areas.
pub fn hit_test(
    x: u16,
    y: u16,
    regions: &HitRegions,
    pane_rects: &[(PaneId, Rect)],
) -> ClickTarget {
    // Check tab labels first (status bar).
    for region in &regions.tab_regions {
        if y == region.y && x >= region.x_start && x < region.x_end {
            return ClickTarget::Tab(region.tab_index);
        }
    }

    // Check stack labels (pane tab headers).
    for region in &regions.stack_regions {
        if y == region.y && x >= region.x_start && x < region.x_end {
            return ClickTarget::StackLabel(region.pane_id);
        }
    }

    // Check pane content areas.
    for &(pane_id, rect) in pane_rects {
        if x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height {
            return ClickTarget::Pane(pane_id);
        }
    }

    ClickTarget::None
}

// ---------------------------------------------------------------------------
// Status info (passed from the daemon)
// ---------------------------------------------------------------------------

/// Information needed to render the status bar.
pub struct StatusInfo {
    /// Current mode name (e.g. "NORMAL", "COMMAND", "VISUAL", "SEARCH").
    pub mode: String,
    /// Session name or path.
    pub session_name: String,
    /// Tab list: (name, is_active) pairs.
    pub tabs: Vec<(String, bool)>,
    /// Layout mode name (e.g. "bsp", "master", "monocle", "custom").
    pub layout_mode: String,
    /// Search match info: (current_match_index, total_matches). `None` when not searching.
    pub search_info: Option<(usize, usize)>,
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
    let (fg, bg) = if cell.attrs.reverse {
        // Swap fg and bg. When both are Default, use explicit colors
        // so the inversion is visible (Default fg=light, Default bg=dark).
        let mut fg = convert_color(&cell.attrs.bg);
        let mut bg = convert_color(&cell.attrs.fg);
        if fg == CellColor::Default && bg == CellColor::Default {
            fg = CellColor::Indexed(0); // black foreground
            bg = CellColor::Indexed(7); // white background
        } else {
            if fg == CellColor::Default {
                fg = CellColor::Indexed(0); // dark on default bg
            }
            if bg == CellColor::Default {
                bg = CellColor::Indexed(7); // light on default fg
            }
        }
        (fg, bg)
    } else {
        (convert_color(&cell.attrs.fg), convert_color(&cell.attrs.bg))
    };
    RenderCell {
        c: cell.c,
        fg,
        bg,
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
    border_style: &BorderStyle,
    status_info: &StatusInfo,
    total_cols: u16,
    total_rows: u16,
    gap_size: u16,
    focused_pane: PaneId,
    selection: Option<&MouseSelection>,
    scroll_offsets: &HashMap<PaneId, usize>,
    theme: &CompositorTheme,
) -> (Vec<Vec<RenderCell>>, HitRegions) {
    let mut buffer = vec![vec![RenderCell::default(); total_cols as usize]; total_rows as usize];
    let mut hit_regions = HitRegions::default();

    let pane_rects = layout::compute_layout(layout, area, gap_size);

    let mode = status_info.mode.as_str();

    match border_style {
        BorderStyle::ZellijStyle => {
            draw_zellij_panes(
                &mut buffer,
                &pane_rects,
                pane_screens,
                layout,
                focused_pane,
                mode,
                &mut hit_regions,
                scroll_offsets,
                theme,
            );
        }
        BorderStyle::TmuxStyle => {
            // TmuxStyle always uses gap_size=0, enforced at the caller level
            // (daemon.rs). Content is edge-to-edge with minimal dividers.
            draw_tmux_panes(
                &mut buffer,
                &pane_rects,
                pane_screens,
                layout,
                focused_pane,
                mode,
                &mut hit_regions,
                scroll_offsets,
                theme,
            );
        }
    }

    // Apply selection highlighting (invert fg/bg for selected cells).
    if let Some(sel) = selection {
        if let Some((_, pane_rect)) = pane_rects.iter().find(|(id, _)| *id == sel.pane_id) {
            apply_selection_highlight(&mut buffer, sel, pane_rect, border_style);
        }
    }

    // Draw status bar on the last row.
    draw_status_bar(
        &mut buffer,
        total_cols,
        total_rows,
        status_info,
        &mut hit_regions,
        theme,
    );

    (buffer, hit_regions)
}

/// Apply fg/bg inversion for cells within the mouse selection range.
///
/// Selection coordinates are in pane-local space; they are mapped to screen
/// coordinates using the pane's rect and the border offsets.
fn apply_selection_highlight(
    buffer: &mut [Vec<RenderCell>],
    sel: &MouseSelection,
    pane_rect: &Rect,
    border_style: &BorderStyle,
) {
    // Compute the content offset inside the pane rect (skip borders).
    let (x_off, y_off) = match border_style {
        BorderStyle::ZellijStyle => {
            if pane_rect.width >= 3 && pane_rect.height >= 3 {
                (1u16, 1u16)
            } else {
                (0, 0)
            }
        }
        BorderStyle::TmuxStyle => (0, 0),
    };

    // Normalize selection so start <= end in reading order.
    let (start, end) = normalize_selection(sel.start, sel.end);

    let (start_col, start_row) = start;
    let (end_col, end_row) = end;

    for row in start_row..=end_row {
        let screen_row = (pane_rect.y + y_off + row) as usize;
        if screen_row >= buffer.len() {
            continue;
        }

        let row_start_col = if row == start_row { start_col } else { 0 };
        let row_end_col = if row == end_row {
            end_col
        } else {
            pane_rect.width.saturating_sub(x_off * 2).saturating_sub(1)
        };

        for col in row_start_col..=row_end_col {
            let screen_col = (pane_rect.x + x_off + col) as usize;
            if screen_col >= buffer[screen_row].len() {
                continue;
            }
            let cell = &mut buffer[screen_row][screen_col];
            // Set light grey background for selection.
            cell.bg = CellColor::Indexed(7);
            // Ensure foreground contrasts with selection background.
            match &cell.fg {
                CellColor::Default | CellColor::Indexed(7) => {
                    cell.fg = CellColor::Indexed(0); // Black text on light grey
                }
                _ => {} // Keep colored text as-is
            }
        }
    }
}

/// Normalize a selection so that the start position comes before the end
/// position in reading order (top-to-bottom, left-to-right).
fn normalize_selection(start: (u16, u16), end: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if start.1 < end.1 || (start.1 == end.1 && start.0 <= end.0) {
        (start, end)
    } else {
        (end, start)
    }
}

// ---------------------------------------------------------------------------
// Zellij-style rendering (full box borders with rounded corners)
// ---------------------------------------------------------------------------

/// Draw panes with full box-drawing borders using rounded corners.
///
/// Every pane gets a border (including single-pane layouts). The active pane
/// gets a green border; inactive panes get dark grey. Stacked panes show
/// tab names in the top border.
#[allow(clippy::too_many_arguments)]
fn draw_zellij_panes(
    buffer: &mut [Vec<RenderCell>],
    pane_rects: &[(PaneId, Rect)],
    pane_screens: &HashMap<PaneId, &Screen>,
    layout: &LayoutNode,
    focused_pane: PaneId,
    mode: &str,
    hit_regions: &mut HitRegions,
    scroll_offsets: &HashMap<PaneId, usize>,
    theme: &CompositorTheme,
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
            theme.frame_active_fg.clone()
        } else {
            theme.frame_fg.clone()
        };

        let offset = scroll_offsets.get(&pane_id).copied().unwrap_or(0);

        // If rect is too small for borders, blit content to full rect.
        if rect.width < 3 || rect.height < 3 {
            blit_screen(buffer, screen, rect, offset);
            continue;
        }

        // Blit screen content to inner area (inside border).
        let inner = Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width - 2,
            height: rect.height - 2,
        };
        blit_screen(buffer, screen, inner, offset);

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
        let available_width = w.saturating_sub(2); // inside the two corner chars
        let top_content = build_top_border_content(
            &stack_info,
            pane_id,
            &border_fg,
            mode,
            available_width,
            theme,
        );

        // Track stack label regions for hit testing (multi-pane stacks).
        if let Some((names, pane_ids, _active_idx)) = &stack_info {
            if pane_ids.len() > 1 {
                // Compute positions of each tab label in the top border.
                // Layout: corner + space + [tab0] + " | " + [tab1] + ... + space + corner
                let max_name_len = names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        if n.is_empty() {
                            format!("{}", pane_ids[i]).len()
                        } else {
                            n.chars().count()
                        }
                    })
                    .max()
                    .unwrap_or(0);
                let tab_width = (max_name_len + 2).min(available_width);
                // Start after corner (x) + 1 (space)
                let mut label_x = (x + 1 + 1) as u16; // corner + leading space
                for (i, pid) in pane_ids.iter().enumerate() {
                    if i > 0 {
                        label_x += 3; // " | " separator
                    }
                    let label_end = label_x + tab_width as u16;
                    hit_regions.stack_regions.push(StackRegion {
                        x_start: label_x,
                        x_end: label_end,
                        y: y as u16,
                        pane_id: *pid,
                    });
                    label_x = label_end;
                }
            }
        }

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
/// For multi-pane stacks: equal-width tabs with mode-based coloring.
fn build_top_border_content(
    stack_info: &Option<(Vec<String>, Vec<PaneId>, usize)>,
    pane_id: PaneId,
    border_fg: &CellColor,
    mode: &str,
    max_width: usize,
    theme: &CompositorTheme,
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
        let _ = pane_id;
        // Build display names and find the longest one.
        let display_names: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                if name.is_empty() {
                    format!("{}", pane_ids[i])
                } else {
                    name.clone()
                }
            })
            .collect();
        let max_name_len = display_names
            .iter()
            .map(|n| n.chars().count())
            .max()
            .unwrap_or(0);
        // Fixed tab width: longest name + 2 (1 space padding each side), capped to fit.
        let tab_width = (max_name_len + 2).min(max_width);

        let (active_fg, active_bg) = theme.mode_colors(mode);

        // Leading space before first tab.
        cells.push(RenderCell {
            c: ' ',
            fg: border_fg.clone(),
            bg: CellColor::Default,
            bold: false,
            italic: false,
            underline: false,
        });

        for (i, display_name) in display_names.iter().enumerate() {
            if i > 0 {
                // Separator: " | "
                for sep_ch in [' ', '|', ' '] {
                    cells.push(RenderCell {
                        c: sep_ch,
                        fg: theme.frame_fg.clone(),
                        bg: CellColor::Default,
                        bold: false,
                        italic: false,
                        underline: false,
                    });
                }
            }

            let is_this_active = i == active_idx;
            let (tab_fg, tab_bg, tab_bold) = if is_this_active {
                (active_fg.clone(), active_bg.clone(), true)
            } else {
                (
                    theme.tab_inactive_fg.clone(),
                    CellColor::Indexed(237),
                    false,
                )
            };

            // Center the name within tab_width.
            let name_len = display_name.chars().count();
            let content_len = name_len.min(tab_width);
            let pad_total = tab_width.saturating_sub(content_len);
            let pad_left = pad_total / 2;
            let pad_right = pad_total - pad_left;

            for _ in 0..pad_left {
                cells.push(RenderCell {
                    c: ' ',
                    fg: tab_fg.clone(),
                    bg: tab_bg.clone(),
                    bold: tab_bold,
                    italic: false,
                    underline: false,
                });
            }
            for ch in display_name.chars().take(tab_width) {
                cells.push(RenderCell {
                    c: ch,
                    fg: tab_fg.clone(),
                    bg: tab_bg.clone(),
                    bold: tab_bold,
                    italic: false,
                    underline: false,
                });
            }
            for _ in 0..pad_right {
                cells.push(RenderCell {
                    c: ' ',
                    fg: tab_fg.clone(),
                    bg: tab_bg.clone(),
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
#[allow(clippy::too_many_arguments)]
fn draw_tmux_panes(
    buffer: &mut [Vec<RenderCell>],
    pane_rects: &[(PaneId, Rect)],
    pane_screens: &HashMap<PaneId, &Screen>,
    layout: &LayoutNode,
    _focused_pane: PaneId,
    mode: &str,
    hit_regions: &mut HitRegions,
    scroll_offsets: &HashMap<PaneId, usize>,
    theme: &CompositorTheme,
) {
    for &(pane_id, rect) in pane_rects {
        if rect.width == 0 || rect.height == 0 {
            continue;
        }

        let screen = match pane_screens.get(&pane_id) {
            Some(s) => s,
            None => continue,
        };

        let offset = scroll_offsets.get(&pane_id).copied().unwrap_or(0);

        let stack_info = layout::find_stack_names(layout, pane_id);
        let is_multi = stack_info
            .as_ref()
            .map(|(_, panes, _)| panes.len() > 1)
            .unwrap_or(false);

        if is_multi && rect.height >= 2 {
            // Draw 1-row tab bar at the top, content below.
            draw_tmux_tab_bar(buffer, rect, &stack_info, mode, hit_regions, theme);

            let content_rect = Rect {
                x: rect.x,
                y: rect.y + 1,
                width: rect.width,
                height: rect.height - 1,
            };
            blit_screen(buffer, screen, content_rect, offset);
        } else {
            // Single-pane stack or too small: full content area.
            blit_screen(buffer, screen, rect, offset);
        }
    }

    // Draw dividers between adjacent panes.
    if pane_rects.len() > 1 {
        draw_tmux_dividers(buffer, pane_rects, theme);
    }
}

/// Draw a 1-row tab bar at the top of a pane rect for multi-pane stacks.
fn draw_tmux_tab_bar(
    buffer: &mut [Vec<RenderCell>],
    rect: Rect,
    stack_info: &Option<(Vec<String>, Vec<PaneId>, usize)>,
    mode: &str,
    hit_regions: &mut HitRegions,
    theme: &CompositorTheme,
) {
    let y = rect.y as usize;
    let x_start = rect.x as usize;
    let x_end = (rect.x + rect.width) as usize;
    let total_width = rect.width as usize;

    // Fill tab bar background.
    for col in x_start..x_end {
        set_cell(
            buffer,
            y,
            col,
            RenderCell {
                c: ' ',
                fg: theme.status_bar_fg.clone(),
                bg: theme.status_bar_bg.clone(),
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

    // Build display names and find the longest one.
    let display_names: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if name.is_empty() {
                format!("{}", pane_ids[i])
            } else {
                name.clone()
            }
        })
        .collect();
    let max_name_len = display_names
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0);
    // Fixed tab width: longest name + 2 (1 space padding each side), capped to fit.
    let tab_width = (max_name_len + 2).min(total_width);

    let (active_fg, active_bg) = theme.mode_colors(mode);
    let mut col = x_start;

    for (i, display_name) in display_names.iter().enumerate() {
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
                            fg: theme.separator_fg.clone(),
                            bg: theme.status_bar_bg.clone(),
                            bold: false,
                            italic: false,
                            underline: false,
                        },
                    );
                    col += 1;
                }
            }
        }

        // Track the stack label region for hit testing.
        let label_start = col as u16;
        let label_end = (col + tab_width).min(x_end) as u16;
        hit_regions.stack_regions.push(StackRegion {
            x_start: label_start,
            x_end: label_end,
            y: y as u16,
            pane_id: pane_ids[i],
        });

        let is_active = i == active_idx;
        let (fg, bg, bold) = if is_active {
            (active_fg.clone(), active_bg.clone(), true)
        } else {
            (
                theme.tab_inactive_fg.clone(),
                CellColor::Indexed(237),
                false,
            )
        };

        // Center the name within tab_width.
        let name_len = display_name.chars().count();
        let content_len = name_len.min(tab_width);
        let pad_total = tab_width.saturating_sub(content_len);
        let pad_left = pad_total / 2;
        let pad_right = pad_total - pad_left;

        for _ in 0..pad_left {
            if col < x_end {
                set_cell(
                    buffer,
                    y,
                    col,
                    RenderCell {
                        c: ' ',
                        fg: fg.clone(),
                        bg: bg.clone(),
                        bold,
                        italic: false,
                        underline: false,
                    },
                );
                col += 1;
            }
        }
        for ch in display_name.chars().take(tab_width) {
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
                    bg: bg.clone(),
                    bold,
                    italic: false,
                    underline: false,
                },
            );
            col += 1;
        }
        for _ in 0..pad_right {
            if col < x_end {
                set_cell(
                    buffer,
                    y,
                    col,
                    RenderCell {
                        c: ' ',
                        fg: fg.clone(),
                        bg: bg.clone(),
                        bold,
                        italic: false,
                        underline: false,
                    },
                );
                col += 1;
            }
        }
    }
}

/// Draw simple divider lines between adjacent panes (tmux style).
fn draw_tmux_dividers(
    buffer: &mut [Vec<RenderCell>],
    pane_rects: &[(PaneId, Rect)],
    theme: &CompositorTheme,
) {
    let divider_cell = |c: char| RenderCell {
        c,
        fg: theme.frame_fg.clone(),
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
fn blit_screen(buffer: &mut [Vec<RenderCell>], screen: &Screen, rect: Rect, scroll_offset: usize) {
    if scroll_offset == 0 {
        // Original behavior: blit from grid directly (fast path)
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
    } else {
        // Scrollback view: blit from combined scrollback+grid buffer
        let total = screen.total_lines();
        let view_bottom = total.saturating_sub(scroll_offset);
        let view_top = view_bottom.saturating_sub(rect.height as usize);

        for row in 0..rect.height as usize {
            let line_idx = view_top + row;
            let buf_y = rect.y as usize + row;
            if buf_y >= buffer.len() {
                break;
            }
            if let Some(line) = screen.line_at(line_idx) {
                for col in 0..rect.width as usize {
                    let buf_x = rect.x as usize + col;
                    if buf_x >= buffer[buf_y].len() {
                        break;
                    }
                    if col < line.len() {
                        buffer[buf_y][buf_x] = cell_to_render_cell(&line[col]);
                    }
                }
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
fn draw_status_bar(
    buffer: &mut [Vec<RenderCell>],
    cols: u16,
    rows: u16,
    info: &StatusInfo,
    hit_regions: &mut HitRegions,
    theme: &CompositorTheme,
) {
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
                fg: theme.status_bar_fg.clone(),
                bg: theme.status_bar_bg.clone(),
                bold: false,
                italic: false,
                underline: false,
            };
        }
    }

    // Mode indicator.
    let mode_str = format!(" [{}] ", info.mode);
    let (mode_fg, mode_bg) = theme.mode_colors(info.mode.as_str());

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
                fg: theme.session_name_fg.clone(),
                bg: theme.status_bar_bg.clone(),
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
            fg: theme.separator_fg.clone(),
            bg: theme.status_bar_bg.clone(),
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
                        fg: theme.separator_fg.clone(),
                        bg: theme.status_bar_bg.clone(),
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
            (
                theme.tab_active_fg.clone(),
                theme.tab_active_bg.clone(),
                true,
            )
        } else {
            (
                theme.tab_inactive_fg.clone(),
                theme.status_bar_bg.clone(),
                false,
            )
        };

        let tab_x_start = x;
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
        hit_regions.tab_regions.push(TabRegion {
            x_start: tab_x_start as u16,
            x_end: x as u16,
            y: bar_row as u16,
            tab_index: i,
        });
    }

    // Build right-side content: search info + layout mode.
    let mut right_parts: Vec<String> = Vec::new();

    if let Some((current, total)) = info.search_info {
        right_parts.push(format!(" ({}/{}) ", current + 1, total));
    }

    if !info.layout_mode.is_empty() {
        right_parts.push(format!(" {} ", info.layout_mode));
    }

    if !right_parts.is_empty() {
        let right_str: String = right_parts.concat();
        let right_len = right_str.len();
        let right_x = cols.saturating_sub(right_len);
        // Only draw if it doesn't overlap with the left-side content.
        if right_x > x {
            let mut rx = right_x;
            // For search info portion, use yellow colors; for layout, use grey.
            let search_info_len = if info.search_info.is_some() {
                right_parts[0].len()
            } else {
                0
            };

            for (i, ch) in right_str.chars().enumerate() {
                if rx < cols && rx < buffer[bar_row].len() {
                    let (fg, bg) = if i < search_info_len {
                        (CellColor::Indexed(0), CellColor::Indexed(11)) // Black on bright yellow
                    } else {
                        (CellColor::Indexed(0), CellColor::Indexed(245)) // Black on grey
                    };
                    buffer[bar_row][rx] = RenderCell {
                        c: ch,
                        fg,
                        bg,
                        bold: false,
                        italic: false,
                        underline: false,
                    };
                }
                rx += 1;
            }
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
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::TmuxStyle,
            &status,
            10,
            5,
            0,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
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
            mode: "COMMAND".to_string(),
            session_name: "main".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::TmuxStyle,
            &status,
            20,
            5,
            0,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
        );

        // The last row should have the mode indicator.
        let last_row = &result[4];
        let text: String = last_row.iter().map(|c| c.c).collect();
        assert!(text.contains("COMMAND"));
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let gap_size = 2;
        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::TmuxStyle,
            &status,
            20,
            9,
            gap_size,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::ZellijStyle,
            &status,
            20,
            11,
            0,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::ZellijStyle,
            &status,
            20,
            11,
            0,
            1, // pane 1 is focused
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
        );

        // Active pane (1) top-left corner should be green (Catppuccin Mocha blue).
        assert_eq!(result[0][0].fg, CellColor::Rgb(137, 180, 250));

        // Inactive pane (2) should have dark grey border.
        let pane_rects = layout::compute_layout(&layout, area, 0);
        let (_, r2) = pane_rects[1];
        assert_eq!(
            result[r2.y as usize][r2.x as usize].fg,
            CellColor::Rgb(88, 91, 112)
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::ZellijStyle,
            &status,
            20,
            11,
            0,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::TmuxStyle,
            &status,
            20,
            11,
            0,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        // Pane 2 is active (last added).
        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::TmuxStyle,
            &status,
            20,
            11,
            0,
            2,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
        );

        // First row should be the tab bar (inactive tab uses 237 background).
        assert_eq!(result[0][0].bg, CellColor::Indexed(237));
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::ZellijStyle,
            &status,
            20,
            11,
            0,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::ZellijStyle,
            &status,
            30,
            11,
            0,
            2,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::TmuxStyle,
            &status,
            20,
            11,
            0,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::TmuxStyle,
            &status,
            40,
            11,
            0,
            3,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
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
            mode: "COMMAND".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (result, _hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::TmuxStyle,
            &status,
            20,
            11,
            0,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
        );

        // There should be vertical divider characters between the two panes.
        let pane_rects = layout::compute_layout(&layout, area, 0);
        let (_, r1) = pane_rects[0];
        let divider_col = (r1.x + r1.width - 1) as usize;
        let has_divider = (0..10).any(|row| result[row][divider_col].c == '\u{2502}');
        assert!(has_divider, "expected vertical divider between panes");
    }

    // -----------------------------------------------------------------------
    // Hit testing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_hit_test_pane_interior() {
        let pane_rects = vec![
            (
                1,
                Rect {
                    x: 0,
                    y: 0,
                    width: 40,
                    height: 12,
                },
            ),
            (
                2,
                Rect {
                    x: 40,
                    y: 0,
                    width: 40,
                    height: 12,
                },
            ),
        ];
        let regions = HitRegions::default();

        assert_eq!(hit_test(5, 5, &regions, &pane_rects), ClickTarget::Pane(1));
        assert_eq!(hit_test(50, 5, &regions, &pane_rects), ClickTarget::Pane(2));
    }

    #[test]
    fn test_hit_test_tab_label() {
        let pane_rects = vec![(
            1,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 23,
            },
        )];
        let regions = HitRegions {
            tab_regions: vec![
                TabRegion {
                    x_start: 20,
                    x_end: 30,
                    y: 23,
                    tab_index: 0,
                },
                TabRegion {
                    x_start: 33,
                    x_end: 43,
                    y: 23,
                    tab_index: 1,
                },
            ],
            stack_regions: vec![],
        };

        assert_eq!(hit_test(25, 23, &regions, &pane_rects), ClickTarget::Tab(0));
        assert_eq!(hit_test(35, 23, &regions, &pane_rects), ClickTarget::Tab(1));
    }

    #[test]
    fn test_hit_test_stack_label() {
        let pane_rects = vec![(
            1,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 12,
            },
        )];
        let regions = HitRegions {
            tab_regions: vec![],
            stack_regions: vec![
                StackRegion {
                    x_start: 2,
                    x_end: 12,
                    y: 0,
                    pane_id: 1,
                },
                StackRegion {
                    x_start: 15,
                    x_end: 25,
                    y: 0,
                    pane_id: 2,
                },
            ],
        };

        assert_eq!(
            hit_test(5, 0, &regions, &pane_rects),
            ClickTarget::StackLabel(1)
        );
        assert_eq!(
            hit_test(20, 0, &regions, &pane_rects),
            ClickTarget::StackLabel(2)
        );
    }

    #[test]
    fn test_hit_test_border_gap() {
        let pane_rects = vec![
            (
                1,
                Rect {
                    x: 0,
                    y: 0,
                    width: 39,
                    height: 12,
                },
            ),
            (
                2,
                Rect {
                    x: 41,
                    y: 0,
                    width: 39,
                    height: 12,
                },
            ),
        ];
        let regions = HitRegions::default();

        // Click in the gap between panes.
        assert_eq!(hit_test(40, 5, &regions, &pane_rects), ClickTarget::None);
    }

    #[test]
    fn test_hit_test_outside() {
        let pane_rects = vec![(
            1,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 23,
            },
        )];
        let regions = HitRegions::default();

        // Below all pane rects, no tab regions defined at this y.
        assert_eq!(hit_test(5, 24, &regions, &pane_rects), ClickTarget::None);
    }

    #[test]
    fn test_hit_test_priority_tab_over_pane() {
        // Tab label at the same row as the last pane row -- tab should win.
        let pane_rects = vec![(
            1,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
        )];
        let regions = HitRegions {
            tab_regions: vec![TabRegion {
                x_start: 10,
                x_end: 20,
                y: 23,
                tab_index: 0,
            }],
            stack_regions: vec![],
        };

        assert_eq!(hit_test(15, 23, &regions, &pane_rects), ClickTarget::Tab(0));
    }

    #[test]
    fn test_hit_test_priority_stack_over_pane() {
        // Stack label at the top border of a pane -- stack should win.
        let pane_rects = vec![(
            1,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 12,
            },
        )];
        let regions = HitRegions {
            tab_regions: vec![],
            stack_regions: vec![StackRegion {
                x_start: 2,
                x_end: 12,
                y: 0,
                pane_id: 3,
            }],
        };

        assert_eq!(
            hit_test(5, 0, &regions, &pane_rects),
            ClickTarget::StackLabel(3)
        );
    }

    // -----------------------------------------------------------------------
    // Coordinate mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_screen_to_pane_local_mapping() {
        // Pane at (10, 5) with size 30x10.
        let pane_rect = Rect {
            x: 10,
            y: 5,
            width: 30,
            height: 10,
        };

        // Screen coordinate (15, 8) should map to pane-local (5, 3).
        let local_x = 15u16.saturating_sub(pane_rect.x);
        let local_y = 8u16.saturating_sub(pane_rect.y);
        assert_eq!(local_x, 5);
        assert_eq!(local_y, 3);
    }

    #[test]
    fn test_screen_to_pane_local_clamped() {
        // Pane at (10, 5) with size 30x10.
        let pane_rect = Rect {
            x: 10,
            y: 5,
            width: 30,
            height: 10,
        };

        // Screen coordinate beyond pane bounds should clamp.
        let local_x = 50u16
            .saturating_sub(pane_rect.x)
            .min(pane_rect.width.saturating_sub(1));
        let local_y = 20u16
            .saturating_sub(pane_rect.y)
            .min(pane_rect.height.saturating_sub(1));
        assert_eq!(local_x, 29);
        assert_eq!(local_y, 9);
    }

    #[test]
    fn test_screen_to_pane_local_at_origin() {
        // Pane at (0, 0) with size 80x24.
        let pane_rect = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };

        let local_x = 10u16.saturating_sub(pane_rect.x);
        let local_y = 5u16.saturating_sub(pane_rect.y);
        assert_eq!(local_x, 10);
        assert_eq!(local_y, 5);
    }

    #[test]
    fn test_hit_regions_populated_by_composite() {
        // Verify that compositing a layout with tabs produces tab regions.
        let layout = LayoutNode::new_stack(1);
        let screen = Screen::new(78, 23, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1, &screen);

        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 23,
        };
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![("Tab 1".to_string(), true), ("Tab 2".to_string(), false)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        let (_result, hit_regions) = composite(
            &layout,
            &pane_screens,
            area,
            &BorderStyle::ZellijStyle,
            &status,
            80,
            24,
            0,
            1,
            None,
            &HashMap::new(),
            &CompositorTheme::default(),
        );

        // Should have 2 tab regions (one per tab in status bar).
        assert_eq!(hit_regions.tab_regions.len(), 2);
        assert_eq!(hit_regions.tab_regions[0].tab_index, 0);
        assert_eq!(hit_regions.tab_regions[1].tab_index, 1);
        // Tab regions should be on the last row.
        assert_eq!(hit_regions.tab_regions[0].y, 23);
    }
}
