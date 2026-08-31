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
use crate::server::session::TabActivity;

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
    /// Tab list: `(name, is_active, activity)` triples. `activity` drives the
    /// background-activity marker/color for non-active tabs.
    pub tabs: Vec<(String, bool, TabActivity)>,
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
        width: cell.width,
        combining: cell.combining.clone(),
        hyperlink: cell.hyperlink.clone(),
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
    // NOTHING is logged per frame here, and that is deliberate. This ran once
    // per composited frame and `draw_zellij_panes` once per PANE per frame, at
    // `debug!` -- and `main.rs` pins the logger at `Debug` with no `RUST_LOG` to
    // turn it down, into a log that is never rotated. Frames are driven by pane
    // output, so a single pane scrolling a build log wrote lines for as long as
    // the build ran. Everything both lines reported is pure derived geometry
    // recomputed from the arguments, so it told a reader nothing the inputs did
    // not already say.
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
    // Compute the content offset inside the pane rect (skip borders), using the
    // SAME threshold the renderer used to decide whether to draw one.
    let (x_off, y_off) = match border_style {
        BorderStyle::ZellijStyle => {
            if fits_zellij_border(pane_rect.width, pane_rect.height) {
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

/// Whether a rect is big enough to carry a zellij-style box border plus at
/// least one interior row/column.
///
/// Public so the client-side View compositor decides border-vs-no-border with
/// the SAME threshold a normal tab's panes use (below it, content is blitted
/// edge-to-edge over the whole rect).
pub fn fits_zellij_border(width: u16, height: u16) -> bool {
    width >= 3 && height >= 3
}

/// Whether `pane_id` renders as a *multi-pane stack* in `layout` -- i.e. it gets
/// a tab strip (zellij: labels in the top border; tmux: a 1-row tab bar) rather
/// than a plain pane.
///
/// Always ask this about the layout actually being drawn: under zoom that is the
/// tab's *effective* layout (a synthetic single-pane stack), not the real tree.
pub fn is_multi_stack(layout: &LayoutNode, pane_id: PaneId) -> bool {
    layout::find_stack_names(layout, pane_id)
        .map(|(_, panes, _)| panes.len() > 1)
        .unwrap_or(false)
}

/// **The one definition of a pane's content (blit) rect.** `rect` is the pane's
/// full allotment; the result is what is left once the border style has taken
/// its share -- the zellij box (one cell all round) or the tmux stack tab bar
/// (the top row).
///
/// Both the compositor and the PTY-sizing path (`active_tab_content_sizes`) go
/// through here, so a pane's screen is always exactly as large as the area
/// painted for it. They used to compute it separately and could disagree by one
/// row, leaving a dead strip at the bottom of a zoomed stacked pane.
pub fn pane_content_rect(style: &BorderStyle, rect: Rect, multi_stack: bool) -> Rect {
    match style {
        BorderStyle::ZellijStyle => {
            if fits_zellij_border(rect.width, rect.height) {
                Rect {
                    x: rect.x + 1,
                    y: rect.y + 1,
                    width: rect.width - 2,
                    height: rect.height - 2,
                }
            } else {
                rect
            }
        }
        BorderStyle::TmuxStyle => {
            if multi_stack && rect.height >= 2 {
                Rect {
                    x: rect.x,
                    y: rect.y + 1,
                    width: rect.width,
                    height: rect.height - 1,
                }
            } else {
                rect
            }
        }
    }
}

/// Draw one pane's zellij-style box border: rounded corners, `border_fg`-colored
/// edges, and the top-border label / stacked-tab content from
/// [`build_top_border_content`].
///
/// `rect` is in BUFFER coordinates and is expected to satisfy
/// [`fits_zellij_border`]; smaller rects still draw safely (every write is
/// bounds-checked) but leave no interior.
///
/// Factored out of [`draw_zellij_panes`] so the client-side View compositor
/// draws a cell's border with byte-for-byte the same glyphs, colors and title
/// treatment as a normal tab's pane border (see `crate::client::view`); keeping
/// one implementation is what makes them stay identical as the code evolves.
/// Hit-testing (stacked-tab regions) stays in [`draw_zellij_panes`] -- it is not
/// drawing, and a view cell's single-title stack has no tabs to hit.
pub fn draw_zellij_border(
    buffer: &mut [Vec<RenderCell>],
    rect: Rect,
    border_fg: &CellColor,
    stack_info: &Option<(Vec<String>, Vec<PaneId>, usize)>,
    pane_id: PaneId,
    mode: &str,
    theme: &CompositorTheme,
) {
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.width as usize;
    if w == 0 || rect.height == 0 {
        return;
    }

    // The box itself is shared: the sidebar chrome draws the same one.
    draw_zellij_box(buffer, rect, border_fg, theme);

    // Then the pane-specific part, and the ONLY pane-specific part: the name /
    // tab labels, overlaid on the top edge the box already filled with ─.
    let available_width = w.saturating_sub(2); // inside the two corner chars
    let top_content =
        build_top_border_content(stack_info, pane_id, border_fg, mode, available_width, theme);
    // The range stops at the right corner, so overlong content is clipped
    // exactly as it was when this loop carried its own counter.
    for (col, cell) in ((x + 1)..(x + w - 1)).zip(top_content.iter()) {
        set_cell(buffer, y, col, cell.clone());
    }
}

/// The glyphs the **rounded** box family is drawn from.
///
/// Border chrome is drawn by two different mechanisms here: the compositor
/// paints `RenderCell` grids (panes, popups, the client's sidebar frame), while
/// the client's overlays -- which-key, the command palette, the session
/// manager, the pickers -- emit `DrawCommand` text lines painted over the
/// finished frame. The MECHANISMS genuinely differ and cannot share a drawing
/// routine, but the glyphs must not: a corner changed for the panes and missed
/// for the overlays is the drift that had `\u{2570}` spelled out in seven
/// separate `format!`s.
///
/// **There are TWO families, deliberately.** This rounded one is worn by
/// everything that frames content -- panes, popups, the sidebar, and every
/// overlay's own box. The `BOX_SHARP_*` set below is a second, lighter family
/// used by exactly one thing: which-key's full-width band, which is a strip
/// across the bottom of the screen rather than a box around content, and reads
/// better square. Both families live here so neither can be changed in one
/// place and missed in another -- but they are not interchangeable, and code
/// picking between them is making a real choice.
pub const BOX_TOP_LEFT: char = '\u{256D}'; // the corner glyphs
pub const BOX_TOP_RIGHT: char = '\u{256E}';
pub const BOX_BOTTOM_LEFT: char = '\u{2570}';
pub const BOX_BOTTOM_RIGHT: char = '\u{256F}';
pub const BOX_HORIZONTAL: char = '\u{2500}';
pub const BOX_VERTICAL: char = '\u{2502}';
/// Tee junctions, where a rule meets an edge. Used by the sidebar frame, the
/// only border here that divides itself.
pub const BOX_TEE_LEFT: char = '\u{251C}';
pub const BOX_TEE_RIGHT: char = '\u{2524}';
pub const BOX_TEE_DOWN: char = '\u{252C}';
pub const BOX_TEE_UP: char = '\u{2534}';

/// A box's top edge as a string: `\u{256D}\u{2500}\u{2500}\u{256E}`, with
/// `inner_width` rule glyphs between the corners.
///
/// For the `DrawCommand` overlays; the grid drawing uses [`draw_zellij_box`].
/// Both take their glyphs from the constants above.
pub fn box_top_line(inner_width: usize) -> String {
    format!(
        "{BOX_TOP_LEFT}{}{BOX_TOP_RIGHT}",
        BOX_HORIZONTAL.to_string().repeat(inner_width)
    )
}

/// A box's top edge with a title set into it.
///
/// The caller computes the two fills: how a title is centred differs per
/// overlay, the corners and the rule between them do not.
pub fn box_top_line_titled(left_fill: usize, title: &str, right_fill: usize) -> String {
    let dash = BOX_HORIZONTAL.to_string();
    format!(
        "{BOX_TOP_LEFT}{}{title}{}{BOX_TOP_RIGHT}",
        dash.repeat(left_fill),
        dash.repeat(right_fill)
    )
}

/// A box's bottom edge as a string.
pub fn box_bottom_line(inner_width: usize) -> String {
    format!(
        "{BOX_BOTTOM_LEFT}{}{BOX_BOTTOM_RIGHT}",
        BOX_HORIZONTAL.to_string().repeat(inner_width)
    )
}

/// A horizontal rule ACROSS a box, tee'd into both side edges.
///
/// The overlays' section dividers. It was four verbatim copies of the same
/// `format!` (the command palette, the session manager twice, the pickers).
pub fn box_rule_line(inner_width: usize) -> String {
    format!(
        "{BOX_TEE_LEFT}{}{BOX_TEE_RIGHT}",
        BOX_HORIZONTAL.to_string().repeat(inner_width)
    )
}

/// The **sharp** box family: which-key's full-width band, and nothing else.
///
/// See the note on [`BOX_TOP_LEFT`] -- this is the second family, kept here for
/// the same reason and NOT interchangeable with the rounded one.
pub const BOX_SHARP_TOP_LEFT: char = '\u{250C}';
pub const BOX_SHARP_TOP_RIGHT: char = '\u{2510}';
pub const BOX_SHARP_BOTTOM_LEFT: char = '\u{2514}';
pub const BOX_SHARP_BOTTOM_RIGHT: char = '\u{2518}';

/// The sharp family's top edge.
pub fn sharp_box_top_line(inner_width: usize) -> String {
    format!(
        "{BOX_SHARP_TOP_LEFT}{}{BOX_SHARP_TOP_RIGHT}",
        BOX_HORIZONTAL.to_string().repeat(inner_width)
    )
}

/// The sharp family's bottom edge.
pub fn sharp_box_bottom_line(inner_width: usize) -> String {
    format!(
        "{BOX_SHARP_BOTTOM_LEFT}{}{BOX_SHARP_BOTTOM_RIGHT}",
        BOX_HORIZONTAL.to_string().repeat(inner_width)
    )
}

/// The foreground a border wears, given whether the thing it frames holds
/// focus. **The one statement of the active/inactive rule.**
///
/// The glyphs are shared by [`draw_zellij_box`] and the string builders above;
/// this is the same argument one level up. It was written out three times --
/// `draw_zellij_panes`, `view::cell_border_fg` and the sidebar frame -- and
/// `draw_zellij_box` cannot own the choice, because it takes the colour as a
/// parameter. So the choice lives here instead.
///
/// Callers that are always focused (a popup owns input whenever it is visible)
/// pass `true` rather than reaching for `frame_active_fg` themselves.
pub fn border_fg(theme: &CompositorTheme, active: bool) -> CellColor {
    if active {
        theme.frame_active_fg.clone()
    } else {
        theme.frame_fg.clone()
    }
}

/// One cell of a border, in `border_fg` on the theme's border background.
///
/// **The one construction of a border cell.** Every frame drawn anywhere -- the
/// zellij box, the tmux dividers, a View cell's border, the client's sidebar
/// chrome -- goes through here, so a change to how a border cell is styled
/// cannot reach one of them and miss another.
///
/// `draw_tmux_tab_bar` deliberately does NOT use this: a tab bar is
/// status-bar-styled, not border-styled, and folding it in would be false
/// sharing.
pub fn border_cell(c: char, border_fg: &CellColor, theme: &CompositorTheme) -> RenderCell {
    RenderCell {
        c,
        fg: border_fg.clone(),
        bg: theme.border_bg(),
        bold: false,
        italic: false,
        underline: false,
        hyperlink: None,
        width: 1,
        combining: Vec::new(),
    }
}

/// Draw a zellij-style box: rounded corners and `─`/`│` edges, nothing inside.
///
/// **The one implementation of the box.** [`draw_zellij_border`] calls this and
/// then overlays the pane's title run on the top edge; the client's sidebar
/// chrome calls it and overlays nothing. Two callers, one set of glyphs.
///
/// `rect` is in BUFFER coordinates, and is NOT required to satisfy
/// [`fits_zellij_border`] -- every write is bounds-checked, so a smaller rect
/// draws safely and simply leaves no interior. Deciding whether a box is
/// wanted at all is the caller's job (`draw_zellij_panes` and
/// `chrome::geometry::sidebar_frame` both gate on `fits_zellij_border`).
pub fn draw_zellij_box(
    buffer: &mut [Vec<RenderCell>],
    rect: Rect,
    border_fg: &CellColor,
    theme: &CompositorTheme,
) {
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.width as usize;
    let h = rect.height as usize;
    if w == 0 || h == 0 {
        return;
    }
    let cell = |c: char| border_cell(c, border_fg, theme);

    // Corners.
    set_cell(buffer, y, x, cell(BOX_TOP_LEFT));
    set_cell(buffer, y, x + w - 1, cell(BOX_TOP_RIGHT));
    set_cell(buffer, y + h - 1, x, cell(BOX_BOTTOM_LEFT));
    set_cell(buffer, y + h - 1, x + w - 1, cell(BOX_BOTTOM_RIGHT));

    // Top and bottom edges, between the corners.
    for col in (x + 1)..(x + w - 1) {
        set_cell(buffer, y, col, cell(BOX_HORIZONTAL));
        set_cell(buffer, y + h - 1, col, cell(BOX_HORIZONTAL));
    }

    // Left and right edges, between the corners.
    for row in (y + 1)..(y + h - 1) {
        set_cell(buffer, row, x, cell(BOX_VERTICAL));
        set_cell(buffer, row, x + w - 1, cell(BOX_VERTICAL));
    }
}

/// Draw a vertical divider run: `│` down `col`, rows `y0..y1`.
///
/// Shared by the tmux pane dividers and the client's sidebar seam so the glyph
/// and the styling have one definition. Bounds-checked; an empty range draws
/// nothing.
pub fn draw_divider_column(
    buffer: &mut [Vec<RenderCell>],
    col: usize,
    y0: usize,
    y1: usize,
    border_fg: &CellColor,
    theme: &CompositorTheme,
) {
    for row in y0..y1 {
        set_cell(
            buffer,
            row,
            col,
            border_cell(BOX_VERTICAL, border_fg, theme),
        );
    }
}

/// Draw a horizontal divider run: `─` along `row`, columns `x0..x1`.
///
/// The counterpart to [`draw_divider_column`], and shared the same way.
pub fn draw_divider_row(
    buffer: &mut [Vec<RenderCell>],
    row: usize,
    x0: usize,
    x1: usize,
    border_fg: &CellColor,
    theme: &CompositorTheme,
) {
    for col in x0..x1 {
        set_cell(
            buffer,
            row,
            col,
            border_cell(BOX_HORIZONTAL, border_fg, theme),
        );
    }
}

/// Write one already-built cell into `buffer`, bounds-checked.
///
/// Public so a client-side frame can overlay a junction glyph (`├┤┬┴`) onto a
/// run drawn by the shared primitives above without restating the bounds check.
pub fn put_cell(buffer: &mut [Vec<RenderCell>], row: usize, col: usize, cell: RenderCell) {
    set_cell(buffer, row, col, cell);
}

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
        let border_fg = border_fg(theme, is_active);

        let offset = scroll_offsets.get(&pane_id).copied().unwrap_or(0);

        // Blit screen content to the inner area (inside the border); when the
        // rect is too small to carry a border that IS the whole rect.
        let inner = pane_content_rect(&BorderStyle::ZellijStyle, rect, false);
        if !fits_zellij_border(rect.width, rect.height) {
            blit_screen(buffer, screen, inner, offset);
            continue;
        }

        blit_screen(buffer, screen, inner, offset);

        // Draw the full box border with rounded corners (shared with the
        // client-side View compositor, so both stay identical).
        let stack_info = layout::find_stack_names(layout, pane_id);
        draw_zellij_border(buffer, rect, &border_fg, &stack_info, pane_id, mode, theme);

        let x = rect.x as usize;
        let y = rect.y as usize;
        let w = rect.width as usize;
        let available_width = w.saturating_sub(2); // inside the two corner chars

        // Track stack label regions for hit testing (multi-pane stacks). The
        // strip's own content starts one column inside the left corner, so the
        // layout's strip-relative offsets are translated by `x + 1`.
        if let Some((names, pane_ids, _active_idx)) = &stack_info {
            if pane_ids.len() > 1 {
                let display_names = display_tab_names(names, pane_ids);
                let strip_x = (x + 1) as u16;
                for entry in
                    tab_strip_layout(&display_names, available_width, &BorderStyle::ZellijStyle)
                {
                    hit_regions.stack_regions.push(StackRegion {
                        x_start: strip_x + entry.start as u16,
                        x_end: strip_x + entry.end as u16,
                        y: y as u16,
                        pane_id: pane_ids[entry.index],
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tab strip geometry (shared by every renderer AND every hit-tester)
// ---------------------------------------------------------------------------

/// The separator drawn between adjacent tabs in a pane tab strip.
const TAB_SEPARATOR: &str = " | ";

/// One tab's placement in a tab strip: the tab's index and the columns
/// `[start, end)` it occupies, relative to the strip's own left edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabStripEntry {
    pub index: usize,
    pub start: usize,
    pub end: usize,
}

/// The display name of each tab in a strip: the pane's name, or its id when the
/// name is empty. **Always measure and lay out tab strips over these**, never
/// over the raw names -- the id fallback is what a strip actually renders.
pub fn display_tab_names(names: &[String], pane_ids: &[PaneId]) -> Vec<String> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if name.is_empty() {
                pane_ids
                    .get(i)
                    .map(|id| format!("{id}"))
                    .unwrap_or_default()
            } else {
                name.clone()
            }
        })
        .collect()
}

/// The fixed per-tab width of a strip: the longest display name plus one padding
/// column on each side, capped to the strip width. `0` when there are no tabs.
///
/// Measured in CHARS. One hit-tester used to measure the id fallback in BYTES,
/// which agreed with the renderer only because ids are numeric.
pub fn tab_strip_width(display_names: &[String], width: usize) -> usize {
    if display_names.is_empty() {
        return 0;
    }
    let max_name = display_names
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0);
    (max_name + 2).min(width)
}

/// **The one tab-strip geometry.** Where each tab lands in a strip `width`
/// columns wide, relative to the strip's left edge.
///
/// Four sites used to recompute `(max_name_len + 2).min(width)` and the run of
/// separators independently -- [`build_top_border_content`], the stacked-tab
/// hit-test in [`draw_zellij_panes`], [`draw_tmux_tab_bar`], and the client's
/// Monocle-strip hit-test -- so a change to the formula silently desynced a
/// renderer from its own hit-tester. All four now go through here.
///
/// The leading offset is style-dependent, and the single-tab case is the
/// off-by-one that used to make a 1-cell Monocle view's hit-test boundaries
/// wrong: zellij's top border pads a MULTI-tab strip with one leading space, but
/// draws a lone title as a bare ` name ` chip flush at the strip's start; the
/// tmux tab bar is always flush.
///
/// Entries past the right edge are dropped; the last visible one is clipped.
pub fn tab_strip_layout(
    display_names: &[String],
    width: usize,
    style: &BorderStyle,
) -> Vec<TabStripEntry> {
    let mut out = Vec::new();
    let tab_width = tab_strip_width(display_names, width);
    if tab_width == 0 {
        return out;
    }
    let mut x = match style {
        BorderStyle::ZellijStyle if display_names.len() > 1 => 1,
        _ => 0,
    };
    for index in 0..display_names.len() {
        if index > 0 {
            x += TAB_SEPARATOR.chars().count();
        }
        if x >= width {
            break;
        }
        let end = (x + tab_width).min(width);
        out.push(TabStripEntry {
            index,
            start: x,
            end,
        });
        x = end;
    }
    out
}

/// Build the render cells for the top border content (pane name or tab labels).
///
/// For single-pane stacks: ` name ` (space-padded name), in the theme's
/// `pane_label_fg`/`pane_label_bg` roles -- which default to the border's own
/// color, so an unconfigured label keeps tracking focus as it always has. A pane
/// with no name gets no label at all.
/// For multi-pane stacks: equal-width tabs (placed by [`tab_strip_layout`]) with
/// mode-based coloring for the active one and `tab_inactive_fg`/`tab_inactive_bg`
/// for the rest.
///
/// Public so the client-side view compositor can render a Monocle cell strip
/// with byte-for-byte the same tab styling as a normal stacked pane's top
/// border.
pub fn build_top_border_content(
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
    let border_bg = theme.border_bg();
    let styled = |c: char, fg: &CellColor, bg: &CellColor, bold: bool| RenderCell {
        c,
        fg: fg.clone(),
        bg: bg.clone(),
        bold,
        italic: false,
        underline: false,
        hyperlink: None,
        width: 1,
        combining: Vec::new(),
    };

    if !is_multi {
        // Single pane: show the name if non-empty, as a ` name ` chip flush at
        // the strip's start (which is what `tab_strip_layout` reports for a lone
        // title, so the hit-test agrees).
        let name = names.first().map(|s| s.as_str()).unwrap_or("");
        if name.is_empty() {
            return cells;
        }
        let (label_fg, label_bg) = theme.label_colors(border_fg);
        let single = [name.to_string()];
        let entry = match tab_strip_layout(&single, max_width, &BorderStyle::ZellijStyle).first() {
            Some(e) => *e,
            None => return cells,
        };
        for ch in format!(" {name} ").chars().take(entry.end - entry.start) {
            cells.push(styled(ch, &label_fg, &label_bg, false));
        }
        return cells;
    }

    let _ = pane_id;
    let display_names = display_tab_names(names, pane_ids);
    let layout = tab_strip_layout(&display_names, max_width, &BorderStyle::ZellijStyle);
    let (active_fg, active_bg) = theme.mode_colors(mode);

    for entry in &layout {
        // Fill the gap the layout left in front of this tab: the strip's single
        // leading pad space before the first tab, the `" | "` separator between
        // tabs. Deriving the gap from the layout's own offsets is what keeps the
        // painted columns and the hit-tested columns identical by construction.
        let gap = entry.start.saturating_sub(cells.len());
        if entry.index == 0 {
            for _ in 0..gap {
                cells.push(styled(' ', border_fg, &border_bg, false));
            }
        } else {
            for ch in TAB_SEPARATOR.chars().take(gap) {
                cells.push(styled(ch, &theme.frame_fg, &border_bg, false));
            }
        }

        let (tab_fg, tab_bg, tab_bold) = if entry.index == active_idx {
            (active_fg.clone(), active_bg.clone(), true)
        } else {
            (
                theme.tab_inactive_fg.clone(),
                theme.tab_inactive_bg.clone(),
                false,
            )
        };

        // Center the name within the tab's own width.
        let tab_width = entry.end - entry.start;
        let display_name = &display_names[entry.index];
        let content_len = display_name.chars().count().min(tab_width);
        let pad_total = tab_width - content_len;
        let pad_left = pad_total / 2;
        let pad_right = pad_total - pad_left;

        for _ in 0..pad_left {
            cells.push(styled(' ', &tab_fg, &tab_bg, tab_bold));
        }
        for ch in display_name.chars().take(content_len) {
            cells.push(styled(ch, &tab_fg, &tab_bg, tab_bold));
        }
        for _ in 0..pad_right {
            cells.push(styled(' ', &tab_fg, &tab_bg, tab_bold));
        }
    }

    // Trailing space after the last tab.
    if !layout.is_empty() {
        cells.push(styled(' ', border_fg, &border_bg, false));
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
        let content_rect = pane_content_rect(
            &BorderStyle::TmuxStyle,
            rect,
            is_multi_stack(layout, pane_id),
        );

        // A reserved top row (and only that) means a tab bar goes there; a
        // single-pane stack or a rect too small to spare the row blits full.
        if content_rect.y > rect.y {
            draw_tmux_tab_bar(buffer, rect, &stack_info, mode, hit_regions, theme);
        }
        blit_screen(buffer, screen, content_rect, offset);
    }

    // Draw dividers between adjacent panes.
    if pane_rects.len() > 1 {
        draw_tmux_dividers(buffer, pane_rects, theme);
    }
}

/// Draw a 1-row tab bar at the top of a pane rect for multi-pane stacks.
///
/// Public so the client-side View compositor renders its Monocle title strip
/// with the same tmux-style treatment a normal stacked pane's tab bar gets
/// (status-bar background fill, `separator_fg` `" | "` separators, mode-colored
/// active tab) instead of the zellij top-border treatment.
pub fn draw_tmux_tab_bar(
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
                hyperlink: None,
                width: 1,
                combining: Vec::new(),
            },
        );
    }

    let (names, pane_ids, active_idx) = match stack_info {
        Some((n, p, a)) => (n, p, *a),
        None => return,
    };

    let display_names = display_tab_names(names, pane_ids);
    let layout = tab_strip_layout(&display_names, total_width, &BorderStyle::TmuxStyle);
    let (active_fg, active_bg) = theme.mode_colors(mode);
    let styled = |c: char, fg: &CellColor, bg: &CellColor, bold: bool| RenderCell {
        c,
        fg: fg.clone(),
        bg: bg.clone(),
        bold,
        italic: false,
        underline: false,
        hyperlink: None,
        width: 1,
        combining: Vec::new(),
    };

    let mut prev_end: Option<usize> = None;
    for entry in &layout {
        // The layout leaves a gap between one tab and the next; that gap is the
        // `" | "` separator. Its extent is read back OUT of the layout rather
        // than assumed, so the drawn columns and the hit regions below stay
        // identical by construction even if the separator ever changes width.
        let mut col = x_start + entry.start;
        if let Some(pe) = prev_end {
            let gap = entry.start.saturating_sub(pe);
            for (i, ch) in TAB_SEPARATOR.chars().take(gap).enumerate() {
                let sep_col = x_start + pe + i;
                if sep_col < x_end {
                    set_cell(
                        buffer,
                        y,
                        sep_col,
                        styled(ch, &theme.separator_fg, &theme.status_bar_bg, false),
                    );
                }
            }
        }
        prev_end = Some(entry.end);

        hit_regions.stack_regions.push(StackRegion {
            x_start: (x_start + entry.start) as u16,
            x_end: (x_start + entry.end).min(x_end) as u16,
            y: y as u16,
            pane_id: pane_ids[entry.index],
        });

        let (fg, bg, bold) = if entry.index == active_idx {
            (active_fg.clone(), active_bg.clone(), true)
        } else {
            (
                theme.tab_inactive_fg.clone(),
                theme.tab_inactive_bg.clone(),
                false,
            )
        };

        // Center the name within the tab's own width.
        let tab_width = entry.end - entry.start;
        let display_name = &display_names[entry.index];
        let content_len = display_name.chars().count().min(tab_width);
        let pad_total = tab_width - content_len;
        let pad_left = pad_total / 2;

        let block = std::iter::repeat_n(' ', pad_left)
            .chain(display_name.chars().take(content_len))
            .chain(std::iter::repeat_n(' ', pad_total - pad_left));
        for ch in block {
            if col >= x_end {
                break;
            }
            set_cell(buffer, y, col, styled(ch, &fg, &bg, bold));
            col += 1;
        }
    }
}

/// Draw simple divider lines between adjacent panes (tmux style).
///
/// Public so the client-side View compositor separates its (borderless) tmux
/// -style cells with exactly the same dividers a normal tab's panes get. Rects
/// are in BUFFER coordinates.
pub fn draw_tmux_dividers(
    buffer: &mut [Vec<RenderCell>],
    pane_rects: &[(PaneId, Rect)],
    theme: &CompositorTheme,
) {
    // A divider IS the frame in tmux style, so it wears the frame's colors --
    // built by the shared `border_cell` and drawn by the shared runs, the same
    // ones the client's sidebar seam uses.
    // tmux dividers never track focus (this function is not even told which
    // pane is focused), so the rule is asked with `false`.
    let fg = border_fg(theme, false);

    for i in 0..pane_rects.len() {
        for j in (i + 1)..pane_rects.len() {
            let (_, r1) = pane_rects[i];
            let (_, r2) = pane_rects[j];

            // Vertical divider: r1 is left of r2. It lands in r1's LAST column,
            // overwriting content -- a pane may paint over its neighbour, which
            // is the one thing the client's sidebar cannot do (the rect on the
            // far side of its seam belongs to the server).
            if r1.x + r1.width == r2.x {
                let top = r1.y.max(r2.y) as usize;
                let bottom = (r1.y + r1.height).min(r2.y + r2.height) as usize;
                let col = r2.x as usize;
                if col > 0 {
                    draw_divider_column(buffer, col - 1, top, bottom, &fg, theme);
                }
            }

            // Horizontal divider: r1 is above r2, landing in r1's last row.
            if r1.y + r1.height == r2.y {
                let left = r1.x.max(r2.x) as usize;
                let right = (r1.x + r1.width).min(r2.x + r2.width) as usize;
                let row = r2.y as usize;
                if row > 0 {
                    draw_divider_row(buffer, row - 1, left, right, &fg, theme);
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

// ---------------------------------------------------------------------------
// Popup terminal overlay
// ---------------------------------------------------------------------------

/// Draw the session's popup terminal on top of an already-composited buffer,
/// returning the popup's interior (content) rect.
///
/// Deliberately a **post-pass over the finished frame** rather than a layout
/// participant: the popup floats above whatever the normal pass produced -- a
/// plain layout, or a zoomed pane's single-pane substitution -- without changing
/// any pane's rect. It steals no space; it only paints over.
///
/// The whole rect is cleared first so nothing from the layout underneath bleeds
/// through when the popup's PTY is momentarily smaller than its interior.
#[allow(clippy::too_many_arguments)]
pub fn draw_popup(
    buffer: &mut [Vec<RenderCell>],
    rect: Rect,
    pane_id: PaneId,
    screen: &Screen,
    title: &str,
    mode: &str,
    scroll_offset: usize,
    selection: Option<&MouseSelection>,
    theme: &CompositorTheme,
) -> Rect {
    if rect.width == 0 || rect.height == 0 {
        return rect;
    }

    // The popup owns input whenever it is visible, so it always wears the
    // active-frame color -- said through the shared rule rather than by
    // reaching for the theme role directly.
    let border_fg = border_fg(theme, true);
    let blank = RenderCell::default();

    // Clear the popup's footprint so the layout underneath cannot show through.
    for row in rect.y..rect.y.saturating_add(rect.height) {
        for col in rect.x..rect.x.saturating_add(rect.width) {
            set_cell(buffer, row as usize, col as usize, blank.clone());
        }
    }

    // Too small for a frame: paint content edge-to-edge (mirrors
    // `draw_zellij_panes`' small-rect fallback, via the SAME threshold).
    if !fits_zellij_border(rect.width, rect.height) {
        blit_screen(buffer, screen, rect, scroll_offset);
        return rect;
    }

    // The interior comes from the one definition of a style's content rect,
    // not from a local `+1 / -2`.
    let interior = pane_content_rect(&BorderStyle::ZellijStyle, rect, false);
    blit_screen(buffer, screen, interior, scroll_offset);

    // The frame is a zellij pane's frame -- the same box, the same title
    // treatment -- because a popup floats OVER those panes and any difference
    // reads as a bug. `draw_zellij_border` is box + title overlay, which is
    // exactly what this used to spell out for itself.
    let stack_info = Some((vec![title.to_string()], vec![pane_id], 0usize));
    draw_zellij_border(buffer, rect, &border_fg, &stack_info, pane_id, mode, theme);

    // A drag-selection inside the popup highlights against the popup's own rect
    // (the layout pass can't: the popup pane is in no layout tree).
    if let Some(sel) = selection {
        if sel.pane_id == pane_id {
            apply_selection_highlight(buffer, sel, &rect, &BorderStyle::ZellijStyle);
        }
    }

    interior
}

/// A single pane's rendered screen plus the cursor/DECCKM state a View cell
/// needs to render the cursor and encode input. Returned by
/// [`render_pane_snapshot`].
pub(crate) struct PaneRenderSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Vec<RenderCell>>,
    /// Cursor position, clamped into `cols`/`rows`.
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    /// The pane's DECCKM (application cursor keys) state.
    pub application_cursor_keys: bool,
}

/// Snapshot a single pane's rendered screen into a standalone cell buffer.
///
/// The buffer is sized to the pane's *own* current dimensions and blitted at
/// scroll_offset 0 (the live view). The cursor position is clamped into the
/// pane's bounds so a focused View cell can place the terminal cursor. When
/// `cols == 0 || rows == 0` the buffer is empty, which callers treat as a
/// no-content snapshot.
pub(crate) fn render_pane_snapshot(screen: &Screen) -> PaneRenderSnapshot {
    render_pane_snapshot_at(screen, 0)
}

/// Like [`render_pane_snapshot`] but blitted at `scroll_offset` lines back into
/// the pane's scrollback (0 = live view). Used by the per-subscriber `ScrollPane`
/// path so a View cell can scroll its source pane's history independently. When
/// scrolled (`scroll_offset > 0`) the cursor is reported hidden, mirroring the
/// foreground scrollback view where the live cursor is not shown.
pub(crate) fn render_pane_snapshot_at(screen: &Screen, scroll_offset: usize) -> PaneRenderSnapshot {
    let cols = screen.cols;
    let rows = screen.rows;
    let mut buffer: Vec<Vec<RenderCell>> =
        vec![vec![RenderCell::default(); cols as usize]; rows as usize];
    blit_screen(
        &mut buffer,
        screen,
        Rect {
            x: 0,
            y: 0,
            width: cols,
            height: rows,
        },
        scroll_offset,
    );
    PaneRenderSnapshot {
        cols,
        rows,
        cells: buffer,
        cursor_x: screen.cursor_x.min(cols.saturating_sub(1)),
        cursor_y: screen.cursor_y.min(rows.saturating_sub(1)),
        cursor_visible: screen.cursor_visible && scroll_offset == 0,
        application_cursor_keys: screen.application_cursor_keys,
    }
}

/// Like [`render_pane_snapshot_at`] but with a drag-selection highlight applied
/// over the blitted grid.
///
/// The selection's coordinates are content-relative to the pane's own grid, so
/// the highlight is applied against a zero-origin rect in `TmuxStyle` — the
/// combination that makes [`apply_selection_highlight`] subtract no border
/// offset. (A View cell's border is drawn by the *client*, around this grid, so
/// the grid itself is borderless here.) Rows/columns are clamped to the
/// snapshot's current size first, so a selection made before the pane shrank
/// highlights what is still there instead of nothing.
pub(crate) fn render_pane_snapshot_selected(
    screen: &Screen,
    scroll_offset: usize,
    selection: Option<&MouseSelection>,
) -> PaneRenderSnapshot {
    let mut snap = render_pane_snapshot_at(screen, scroll_offset);
    if let Some(sel) = selection {
        if snap.cols > 0 && snap.rows > 0 {
            let max_col = snap.cols - 1;
            let max_row = snap.rows - 1;
            let clamped = MouseSelection {
                pane_id: sel.pane_id,
                start: (sel.start.0.min(max_col), sel.start.1.min(max_row)),
                end: (sel.end.0.min(max_col), sel.end.1.min(max_row)),
            };
            apply_selection_highlight(
                &mut snap.cells,
                &clamped,
                &Rect {
                    x: 0,
                    y: 0,
                    width: snap.cols,
                    height: snap.rows,
                },
                &BorderStyle::TmuxStyle,
            );
        }
    }
    snap
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

/// A styled run of text on the status bar's right-hand side.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusSegment {
    pub text: String,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
}

/// **The one definition of the status bar's right-hand content**: the search
/// counter (when searching) followed by the layout-mode indicator.
///
/// The client's View status bar used to build its own right side and styled the
/// layout indicator teal-bold-on-mantle, while this bar draws it black-on-grey
/// and not bold -- so entering a View visibly changed the indicator. Both sides
/// now call this, exactly as both call [`draw_zellij_border`] for their frames.
pub fn status_right_segments(
    search_info: Option<(usize, usize)>,
    layout_mode: &str,
    theme: &CompositorTheme,
) -> Vec<StatusSegment> {
    let mut segments = Vec::new();
    if let Some((current, total)) = search_info {
        segments.push(StatusSegment {
            text: format!(" ({}/{}) ", current + 1, total),
            fg: theme.search_count_fg.clone(),
            bg: theme.search_count_bg.clone(),
            bold: false,
        });
    }
    if !layout_mode.is_empty() {
        segments.push(StatusSegment {
            text: format!(" {layout_mode} "),
            fg: theme.layout_indicator_fg.clone(),
            bg: theme.layout_indicator_bg.clone(),
            bold: false,
        });
    }
    segments
}

/// Paint `segments` right-aligned on a status-bar row `cols` wide, given that
/// the left-hand content already occupies columns `0..left_end`.
///
/// When the two would overlap, the right side is dropped rather than shifted:
/// a truncated session/tab list is far more confusing than a missing layout
/// hint, and this is the behavior the server bar has always had. The View bar
/// used to append the indicator after the left content instead; sharing this
/// function is what settles the disagreement.
///
/// Widths are counted in CHARS.
pub fn draw_right_segments(
    row: &mut [RenderCell],
    cols: usize,
    left_end: usize,
    segments: &[StatusSegment],
) {
    let total: usize = segments.iter().map(|s| s.text.chars().count()).sum();
    if total == 0 {
        return;
    }
    let start = cols.saturating_sub(total);
    if start <= left_end {
        return;
    }
    let mut x = start;
    for seg in segments {
        for ch in seg.text.chars() {
            if x < cols && x < row.len() {
                row[x] = RenderCell {
                    c: ch,
                    fg: seg.fg.clone(),
                    bg: seg.bg.clone(),
                    bold: seg.bold,
                    italic: false,
                    underline: false,
                    hyperlink: None,
                    width: 1,
                    combining: Vec::new(),
                };
            }
            x += 1;
        }
    }
}

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
                hyperlink: None,
                width: 1,
                combining: Vec::new(),
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
                hyperlink: None,
                width: 1,
                combining: Vec::new(),
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
                hyperlink: None,
                width: 1,
                combining: Vec::new(),
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
            hyperlink: None,
            width: 1,
            combining: Vec::new(),
        };
        x += 1;
    }

    // Tab list.
    for (i, (tab_name, is_active, activity)) in info.tabs.iter().enumerate() {
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
                        hyperlink: None,
                        width: 1,
                        combining: Vec::new(),
                    };
                }
                x += 1;
            }
        }

        // Background-activity marker. The active tab is always shown clean
        // (gated on `!is_active`, not on the activity value) so a briefly-stale
        // state never leaks a marker onto the focused tab. A leading marker
        // char is folded into `tab_str`, so the hit-test width below (derived
        // from the rendered string) stays correct automatically.
        let marker = if *is_active {
            None
        } else {
            match activity {
                TabActivity::Bell => Some(('!', theme.tab_bell_fg.clone())), // urgent
                TabActivity::Activity => Some(('\u{25CF}', theme.tab_activity_fg.clone())), // ●
                TabActivity::Silent => Some(('\u{2713}', theme.tab_silent_fg.clone())), // ✓
                TabActivity::None => None,
            }
        };

        let tab_str = if *is_active {
            format!(" {tab_name} ")
        } else if let Some((mark, _)) = marker {
            format!(" {mark} {tab_name} ")
        } else {
            format!(" {tab_name} ")
        };

        let (tab_fg, tab_bg, tab_bold) = if *is_active {
            (
                theme.tab_active_fg.clone(),
                theme.tab_active_bg.clone(),
                true,
            )
        } else if let Some((_, mark_fg)) = marker {
            // Highlight the whole label in the attention color so a background
            // tab needing attention stands out even at a glance.
            (mark_fg, theme.status_bar_bg.clone(), true)
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
                    hyperlink: None,
                    width: 1,
                    combining: Vec::new(),
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

    // Right-side content (search counter + layout indicator), built and painted
    // by the shared helpers the View status bar also uses.
    let segments = status_right_segments(info.search_info, &info.layout_mode, theme);
    draw_right_segments(&mut buffer[bar_row], cols, x, &segments);
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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

    /// A popup's frame IS a zellij pane's frame -- same box, same title
    /// treatment, same colors.
    ///
    /// `draw_popup` used to spell the whole box out for itself: its own
    /// `border_cell` closure, its own corners and edges, its own title loop and
    /// its own `+1 / -2` interior. Three copies of the box existed (pane,
    /// popup, sidebar chrome). This pins the popup to the shared one, so a
    /// change to a corner glyph or a border color cannot reach the panes and
    /// miss the thing floating over them.
    #[test]
    fn a_popups_frame_is_a_zellij_panes_frame() {
        let theme = CompositorTheme::default();
        let screen = Screen::new(10, 4, 100);
        let rect = Rect {
            x: 1,
            y: 1,
            width: 12,
            height: 6,
        };
        let mut popup = vec![vec![RenderCell::default(); 16]; 10];
        draw_popup(
            &mut popup, rect, 7, &screen, "term", "NORMAL", 0, None, &theme,
        );

        let mut pane = vec![vec![RenderCell::default(); 16]; 10];
        let stack = Some((vec!["term".to_string()], vec![7], 0usize));
        draw_zellij_border(
            &mut pane,
            rect,
            &theme.frame_active_fg,
            &stack,
            7,
            "NORMAL",
            &theme,
        );

        // The perimeter only: the popup also blits its screen into the interior.
        let (x0, y0) = (rect.x as usize, rect.y as usize);
        let (x1, y1) = (x0 + rect.width as usize - 1, y0 + rect.height as usize - 1);
        for y in y0..=y1 {
            for x in x0..=x1 {
                if y != y0 && y != y1 && x != x0 && x != x1 {
                    continue;
                }
                assert_eq!(
                    popup[y][x], pane[y][x],
                    "popup and pane frames differ at ({x}, {y})"
                );
            }
        }
    }

    /// The popup's interior is `pane_content_rect`, not a local `+1 / -2`.
    #[test]
    fn a_popups_interior_is_the_shared_content_rect() {
        let theme = CompositorTheme::default();
        let screen = Screen::new(10, 4, 100);
        let rect = Rect {
            x: 1,
            y: 1,
            width: 12,
            height: 6,
        };
        let mut buf = vec![vec![RenderCell::default(); 16]; 10];
        let interior = draw_popup(
            &mut buf, rect, 7, &screen, "term", "NORMAL", 0, None, &theme,
        );
        // LITERAL numbers, not `pane_content_rect(...)`. Comparing against the
        // very function `draw_popup` calls is a tautology: re-inline the old
        // `+1 / -2` and it would still pass. These numbers fail against a
        // drifted local copy AND against a change to `pane_content_rect`.
        assert_eq!(
            interior,
            Rect {
                x: 2,
                y: 2,
                width: 10,
                height: 4
            }
        );
        // ... and they are what the shared function says, which is the claim.
        assert_eq!(
            interior,
            pane_content_rect(&BorderStyle::ZellijStyle, rect, false)
        );
    }

    /// Every border in the program resolves its focus colour through one rule.
    ///
    /// The glyphs are shared; this is the same argument one level up, and it
    /// was written out three times before (`draw_zellij_panes`,
    /// `view::cell_border_fg`, the sidebar frame).
    #[test]
    fn one_rule_decides_every_borders_focus_colour() {
        let theme = CompositorTheme::default();
        assert_eq!(border_fg(&theme, true), theme.frame_active_fg);
        assert_eq!(border_fg(&theme, false), theme.frame_fg);
        assert_ne!(
            theme.frame_fg, theme.frame_active_fg,
            "the default theme must distinguish them or this test proves nothing"
        );

        // A focused pane's border really is what the rule returns.
        let layout = LayoutNode::new_stack(1);
        let screen = Screen::new(18, 8, 100);
        let mut panes = HashMap::new();
        panes.insert(1, &screen);
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "t".to_string(),
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };
        let (buf, _) = composite(
            &layout,
            &panes,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 10,
            },
            &BorderStyle::ZellijStyle,
            &status,
            20,
            11,
            0,
            1,
            None,
            &HashMap::new(),
            &theme,
        );
        assert_eq!(buf[0][0].fg, border_fg(&theme, true));
    }

    /// Every string builder, pinned to LITERAL glyphs.
    ///
    /// Deliberately literal rather than written in terms of the constants: a
    /// golden that moves when the constant moves pins nothing. This is what
    /// makes a typo in `BOX_TEE_LEFT`, or in the sharp family, fail here --
    /// probing found that a drifted `box_rule_line` reached the
    /// command-palette PTY harness without a single assertion noticing.
    #[test]
    fn the_string_builders_are_pinned_to_literal_glyphs() {
        assert_eq!(box_top_line(3), "\u{256D}\u{2500}\u{2500}\u{2500}\u{256E}");
        assert_eq!(
            box_bottom_line(3),
            "\u{2570}\u{2500}\u{2500}\u{2500}\u{256F}"
        );
        assert_eq!(box_rule_line(3), "\u{251C}\u{2500}\u{2500}\u{2500}\u{2524}");
        assert_eq!(
            box_top_line_titled(1, "x", 1),
            "\u{256D}\u{2500}x\u{2500}\u{256E}"
        );
        // The second family. Sharp corners, and they must stay sharp.
        assert_eq!(
            sharp_box_top_line(3),
            "\u{250C}\u{2500}\u{2500}\u{2500}\u{2510}"
        );
        assert_eq!(
            sharp_box_bottom_line(3),
            "\u{2514}\u{2500}\u{2500}\u{2500}\u{2518}"
        );
    }

    /// The string builders and the grid drawing use the SAME glyphs.
    ///
    /// The overlays cannot share a drawing routine with the compositor -- they
    /// emit `DrawCommand` text, not cells -- so this is what keeps the two
    /// mechanisms showing the same box.
    #[test]
    fn the_string_builders_and_the_grid_agree_on_every_glyph() {
        let theme = CompositorTheme::default();
        let rect = Rect {
            x: 0,
            y: 0,
            width: 6,
            height: 3,
        };
        let mut grid = vec![vec![RenderCell::default(); 6]; 3];
        draw_zellij_box(&mut grid, rect, &theme.frame_fg, &theme);
        let top: String = grid[0].iter().map(|c| c.c).collect();
        let bottom: String = grid[2].iter().map(|c| c.c).collect();
        assert_eq!(top, box_top_line(4));
        assert_eq!(bottom, box_bottom_line(4));
        assert_eq!(
            box_top_line_titled(1, "ab", 1),
            format!("{BOX_TOP_LEFT}\u{2500}ab\u{2500}{BOX_TOP_RIGHT}")
        );
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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

        // First row should be the tab bar; its inactive tab wears the named
        // `tab_inactive_bg` role (which defaults to the historical Indexed 237).
        assert_eq!(result[0][0].bg, CompositorTheme::default().tab_inactive_bg);
    }

    // -----------------------------------------------------------------------
    // Tab strip geometry: ONE helper drives both renderers and both hit-testers
    // -----------------------------------------------------------------------

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tab_strip_layout_places_multi_tabs_with_separators() {
        // Longest name "gamma" (5) + 2 = 7-wide tabs, a leading pad space in
        // zellij style, `" | "` (3) between tabs.
        let n = names(&["alpha", "bb", "gamma"]);
        let zj = tab_strip_layout(&n, 80, &BorderStyle::ZellijStyle);
        assert_eq!(
            zj,
            vec![
                TabStripEntry {
                    index: 0,
                    start: 1,
                    end: 8
                },
                TabStripEntry {
                    index: 1,
                    start: 11,
                    end: 18
                },
                TabStripEntry {
                    index: 2,
                    start: 21,
                    end: 28
                },
            ]
        );
        // The tmux tab bar is flush left; everything else is identical.
        let tm = tab_strip_layout(&n, 80, &BorderStyle::TmuxStyle);
        assert_eq!(
            tm,
            vec![
                TabStripEntry {
                    index: 0,
                    start: 0,
                    end: 7
                },
                TabStripEntry {
                    index: 1,
                    start: 10,
                    end: 17
                },
                TabStripEntry {
                    index: 2,
                    start: 20,
                    end: 27
                },
            ]
        );
    }

    #[test]
    fn tab_strip_layout_single_title_is_flush_not_offset() {
        // The off-by-one that made a 1-cell Monocle view's hit-test wrong: the
        // zellij top border draws a LONE title as a bare ` name ` chip flush at
        // the strip's start, not offset by the multi-tab leading pad space.
        let n = names(&["solo"]);
        let zj = tab_strip_layout(&n, 80, &BorderStyle::ZellijStyle);
        assert_eq!(
            zj,
            vec![TabStripEntry {
                index: 0,
                start: 0,
                end: 6
            }]
        );

        // And the layout agrees with what `build_top_border_content` paints:
        // the chip occupies exactly columns 0..6 of the strip.
        let theme = CompositorTheme::default();
        let stack = Some((names(&["solo"]), vec![1 as PaneId], 0usize));
        let cells = build_top_border_content(&stack, 1, &theme.frame_fg, "NORMAL", 80, &theme);
        let text: String = cells.iter().map(|c| c.c).collect();
        assert_eq!(text, " solo ");
        assert_eq!(cells.len(), zj[0].end - zj[0].start);
    }

    #[test]
    fn tab_strip_layout_measures_chars_not_bytes() {
        // The zellij hit-test used to measure the id fallback in BYTES while the
        // renderer measured in chars; they agreed only because ids are numeric.
        // A multi-byte title makes the two answers differ by 4 columns.
        let n = names(&["日本語", "ab"]);
        assert_eq!(tab_strip_width(&n, 80), 5); // 3 chars + 2, NOT 9 bytes + 2
        let layout = tab_strip_layout(&n, 80, &BorderStyle::ZellijStyle);
        assert_eq!(
            layout,
            vec![
                TabStripEntry {
                    index: 0,
                    start: 1,
                    end: 6
                },
                TabStripEntry {
                    index: 1,
                    start: 9,
                    end: 14
                },
            ]
        );
    }

    #[test]
    fn tab_strip_layout_drops_tabs_past_the_right_edge() {
        let n = names(&["alpha", "bb", "gamma"]);
        // Room for the first tab and a clipped second; the third is dropped.
        let layout = tab_strip_layout(&n, 14, &BorderStyle::ZellijStyle);
        assert_eq!(
            layout,
            vec![
                TabStripEntry {
                    index: 0,
                    start: 1,
                    end: 8
                },
                TabStripEntry {
                    index: 1,
                    start: 11,
                    end: 14
                },
            ]
        );
        assert!(tab_strip_layout(&names(&[]), 80, &BorderStyle::ZellijStyle).is_empty());
    }

    #[test]
    fn clipped_tab_is_painted_exactly_where_the_layout_says() {
        // A strip too narrow for its tabs: the last visible tab is clipped, and
        // its painted block must fill exactly the clipped range the layout
        // reports (padding is centered within the CLIPPED width, so paint and
        // hit-test agree; the old code centered within the full tab width and
        // let the caller truncate from the right, which desynced the two).
        let theme = CompositorTheme::default();
        let stack = Some((names(&["alpha", "bb"]), vec![1 as PaneId, 2], 0usize));
        let layout = tab_strip_layout(&names(&["alpha", "bb"]), 14, &BorderStyle::ZellijStyle);
        assert_eq!(
            layout[1],
            TabStripEntry {
                index: 1,
                start: 11,
                end: 14
            }
        );
        let cells = build_top_border_content(&stack, 1, &theme.frame_fg, "NORMAL", 14, &theme);
        // The clipped tab occupies columns 11..14 of the strip content.
        for (i, cell) in cells.iter().enumerate().take(14).skip(11) {
            assert_eq!(cell.bg, theme.tab_inactive_bg, "strip col {i}");
        }
        // ...and the separator immediately before it is not part of the block.
        assert_ne!(cells[10].bg, theme.tab_inactive_bg);
    }

    #[test]
    fn tmux_tab_bar_separator_is_placed_from_the_layout() {
        // Regression guard for the separator's column arithmetic: it is derived
        // from the previous tab's end, not from a hardcoded back-offset (which
        // would underflow at `x_start == 0` if a tab ever started before it).
        let theme = CompositorTheme::default();
        let display = names(&["a", "b"]);
        let stack = Some((display.clone(), vec![1 as PaneId, 2], 0usize));
        let mut buffer = vec![vec![RenderCell::default(); 20]; 2];
        let mut regions = HitRegions::default();
        draw_tmux_tab_bar(
            &mut buffer,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 2,
            },
            &stack,
            "NORMAL",
            &mut regions,
            &theme,
        );
        let layout = tab_strip_layout(&display, 20, &BorderStyle::TmuxStyle);
        // 3-wide tabs flush left: 0..3, separator 3..6, second tab 6..9.
        assert_eq!(layout[0].end, 3);
        assert_eq!(layout[1].start, 6);
        let sep: String = (layout[0].end..layout[1].start)
            .map(|x| buffer[0][x].c)
            .collect();
        assert_eq!(sep, TAB_SEPARATOR);
        assert_eq!(buffer[0][layout[0].end].fg, theme.separator_fg);

        // A strip so narrow the second tab is dropped must still not panic.
        let mut narrow = vec![vec![RenderCell::default(); 4]; 2];
        let mut r2 = HitRegions::default();
        draw_tmux_tab_bar(
            &mut narrow,
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            },
            &stack,
            "NORMAL",
            &mut r2,
            &theme,
        );
        assert_eq!(r2.stack_regions.len(), 1, "only the first tab fits");
    }

    #[test]
    fn empty_pane_names_fall_back_to_ids_everywhere() {
        // `display_tab_names` is what every strip measures and renders over.
        let d = display_tab_names(&names(&["", "named"]), &[7, 8]);
        assert_eq!(d, names(&["7", "named"]));
    }

    #[test]
    fn zellij_hit_regions_match_the_painted_tab_columns() {
        // The regression the shared helper exists to prevent: a renderer and its
        // own hit-tester disagreeing about where a tab starts. Composite a real
        // 2-pane stack and check every recorded stack region against the columns
        // that actually carry a tab-block background.
        let mut layout = LayoutNode::new_stack(1);
        assert!(layout.add_to_stack(1, 2), "stacked pane 2 onto pane 1");
        layout::set_pane_name(&mut layout, 1, "alpha");
        layout::set_pane_name(&mut layout, 2, "bb");
        let s1 = Screen::new(30, 8, 100);
        let s2 = Screen::new(30, 8, 100);
        let mut pane_screens = HashMap::new();
        pane_screens.insert(1 as PaneId, &s1);
        pane_screens.insert(2 as PaneId, &s2);
        let theme = CompositorTheme::default();
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        let mut regions = HitRegions::default();
        let mut buffer = vec![vec![RenderCell::default(); 40]; 10];
        draw_zellij_panes(
            &mut buffer,
            &[(1, area)],
            &pane_screens,
            &layout,
            1,
            "NORMAL",
            &mut regions,
            &HashMap::new(),
            &theme,
        );
        assert_eq!(regions.stack_regions.len(), 2, "one region per stacked tab");
        let (_, mode_bg) = theme.mode_colors("NORMAL");
        // Which tab is ACTIVE is the stack's own business, not the focused
        // pane's -- ask the same source the renderer did.
        let (_, stack_ids, active_idx) = layout::find_stack_names(&layout, 1).expect("stack info");
        let active_pane = stack_ids[active_idx];
        for region in &regions.stack_regions {
            let expect_bg = if region.pane_id == active_pane {
                mode_bg.clone()
            } else {
                theme.tab_inactive_bg.clone()
            };
            for x in region.x_start..region.x_end {
                assert_eq!(
                    buffer[0][x as usize].bg, expect_bg,
                    "pane {} col {x} is not part of its painted tab block",
                    region.pane_id
                );
            }
            // The column just left of the region is NOT part of the block, so
            // the region's left edge is the tab's true left edge.
            assert_ne!(
                buffer[0][(region.x_start - 1) as usize].bg,
                expect_bg,
                "region for pane {} starts one column too late",
                region.pane_id
            );
        }
    }

    #[test]
    fn tmux_tab_bar_hit_regions_match_the_painted_columns() {
        let theme = CompositorTheme::default();
        let stack = Some((names(&["alpha", "bb"]), vec![1 as PaneId, 2], 0usize));
        let mut buffer = vec![vec![RenderCell::default(); 40]; 3];
        let mut regions = HitRegions::default();
        draw_tmux_tab_bar(
            &mut buffer,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 3,
            },
            &stack,
            "NORMAL",
            &mut regions,
            &theme,
        );
        assert_eq!(regions.stack_regions.len(), 2);
        let (_, mode_bg) = theme.mode_colors("NORMAL");
        // Flush left, so the first tab starts at column 0.
        assert_eq!(regions.stack_regions[0].x_start, 0);
        for region in &regions.stack_regions {
            // `stack` above declares index 0 (pane 1) active.
            let expect_bg = if region.pane_id == 1 {
                mode_bg.clone()
            } else {
                theme.tab_inactive_bg.clone()
            };
            for x in region.x_start..region.x_end {
                assert_eq!(buffer[0][x as usize].bg, expect_bg, "col {x}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Status bar right-hand segments (shared with the client's View bar)
    // -----------------------------------------------------------------------

    #[test]
    fn status_right_segments_are_themed_not_hardcoded() {
        let theme = CompositorTheme::default();
        let segs = status_right_segments(Some((2, 9)), "bsp", &theme);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, " (3/9) ");
        assert_eq!(segs[0].fg, theme.search_count_fg);
        assert_eq!(segs[0].bg, theme.search_count_bg);
        assert!(!segs[0].bold);
        assert_eq!(segs[1].text, " bsp ");
        assert_eq!(segs[1].fg, theme.layout_indicator_fg);
        assert_eq!(segs[1].bg, theme.layout_indicator_bg);
        assert!(!segs[1].bold, "the layout indicator is NOT bold");

        // No search, no layout name -> nothing to draw.
        assert!(status_right_segments(None, "", &theme).is_empty());
        assert_eq!(status_right_segments(None, "grid", &theme).len(), 1);
    }

    #[test]
    fn draw_right_segments_is_right_aligned_and_drops_on_overlap() {
        let theme = CompositorTheme::default();
        let segs = status_right_segments(None, "grid", &theme); // " grid " = 6
        let mut row = vec![RenderCell::default(); 20];
        draw_right_segments(&mut row, 20, 0, &segs);
        let text: String = row.iter().map(|c| c.c).collect();
        assert_eq!(text, format!("{}{}", " ".repeat(14), " grid "));
        assert_eq!(row[19].fg, theme.layout_indicator_fg);
        assert_eq!(row[19].bg, theme.layout_indicator_bg);

        // The left content reaches column 15: the segment would overlap it, so
        // it is dropped rather than shifted (the server bar's long-standing
        // rule, which the View bar now shares -- it used to append instead).
        let mut row2 = vec![RenderCell::default(); 20];
        draw_right_segments(&mut row2, 20, 15, &segs);
        assert!(
            row2.iter().all(|c| c.bg != theme.layout_indicator_bg),
            "should have drawn nothing"
        );
    }

    #[test]
    fn draw_right_segments_counts_chars_not_bytes() {
        // The server bar used to size the right side with `String::len()`
        // (bytes) while indexing it by char, so a multi-byte layout name would
        // have mis-aligned it.
        let theme = CompositorTheme::default();
        let segs = status_right_segments(None, "日本", &theme); // 4 chars, 8 bytes
        let mut row = vec![RenderCell::default(); 20];
        draw_right_segments(&mut row, 20, 0, &segs);
        let text: String = row.iter().map(|c| c.c).collect();
        assert!(
            text.ends_with(" 日本 "),
            "not right-aligned by chars: {text:?}"
        );
        // 4 chars -> starts at column 16. Sized in BYTES it would have started
        // at column 12.
        assert_eq!(row[17].c, '日');
        assert_ne!(row[13].bg, theme.layout_indicator_bg);
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
    fn render_pane_snapshot_at_shows_scrollback_and_hides_cursor() {
        // 4-col x 2-row grid; feed 6 lines so 4 fall into scrollback.
        let mut s = Screen::new(4, 2, 100);
        s.process_output(b"L1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6");

        // Live view (offset 0): shows the last two rows and a visible cursor.
        let live = render_pane_snapshot_at(&s, 0);
        let live_text: String = live.cells[live.rows as usize - 1]
            .iter()
            .map(|c| c.c)
            .collect();
        assert!(
            live_text.starts_with("L6"),
            "live bottom row should be the newest line, got {live_text:?}"
        );
        assert!(live.cursor_visible, "cursor visible at offset 0");

        // Scrolled back one line: the bottom row is now the previous line, and
        // the cursor is reported hidden (matches the foreground scrollback view).
        let back = render_pane_snapshot_at(&s, 1);
        let back_text: String = back.cells[back.rows as usize - 1]
            .iter()
            .map(|c| c.c)
            .collect();
        assert!(
            back_text.starts_with("L5"),
            "scrolled bottom row should be one line earlier, got {back_text:?}"
        );
        assert!(!back.cursor_visible, "cursor hidden while scrolled back");
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
            tabs: vec![("Tab 1".to_string(), true, TabActivity::None)],
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
            tabs: vec![
                ("Tab 1".to_string(), true, TabActivity::None),
                ("Tab 2".to_string(), false, TabActivity::None),
            ],
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

    /// A background tab with non-None activity renders a leading marker glyph
    /// in an attention color, while the active tab stays clean.
    #[test]
    fn test_status_bar_renders_activity_marker() {
        let mut buffer = vec![vec![RenderCell::default(); 80]; 24];
        let theme = CompositorTheme::default();
        let mut hit_regions = HitRegions::default();
        let status = StatusInfo {
            mode: "NORMAL".to_string(),
            session_name: "test".to_string(),
            tabs: vec![
                ("Tab 1".to_string(), true, TabActivity::None),
                ("Tab 2".to_string(), false, TabActivity::Activity),
                ("Tab 3".to_string(), false, TabActivity::Bell),
                ("Tab 4".to_string(), false, TabActivity::Silent),
            ],
            layout_mode: "bsp".to_string(),
            search_info: None,
        };

        draw_status_bar(&mut buffer, 80, 24, &status, &mut hit_regions, &theme);

        let bar: String = buffer[23].iter().map(|c| c.c).collect();
        // Each non-active tab shows its distinct marker glyph; the active tab
        // shows its plain name with no marker, distinguished by styling only.
        assert!(
            bar.contains('\u{25CF}'),
            "activity ● marker missing: {bar:?}"
        );
        assert!(bar.contains('!'), "bell ! marker missing: {bar:?}");
        assert!(bar.contains('\u{2713}'), "silent ✓ marker missing: {bar:?}");
        // The active tab renders its plain name (no `*` markers) and is set
        // apart purely by the active fg/bg/bold styling on its cells.
        assert!(bar.contains("Tab 1"), "active tab name missing: {bar:?}");
        let active_col = buffer[23]
            .iter()
            .position(|c| c.c == 'T' && c.bg == theme.tab_active_bg)
            .expect("active tab styled cell present");
        assert_eq!(buffer[23][active_col].fg, theme.tab_active_fg);
        assert!(
            buffer[23][active_col].bold,
            "active tab should be bold: {bar:?}"
        );

        // The Activity marker cell must carry the attention color from the
        // named `tab_activity_fg` role (default: bright yellow) rather than the
        // plain inactive-tab foreground.
        let marker_col = buffer[23]
            .iter()
            .position(|c| c.c == '\u{25CF}')
            .expect("activity marker present");
        assert_eq!(buffer[23][marker_col].fg, theme.tab_activity_fg);
        assert_ne!(buffer[23][marker_col].fg, theme.tab_inactive_fg);
    }
}
