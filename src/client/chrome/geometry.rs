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

/// How a sidebar is drawn.
///
/// Only the edge treatment. `Boxed` follows the pane border style (a zellij box
/// or a tmux seam). `Flat` has no frame, just one quiet divider against the
/// content. The panels inside draw the same in both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarStyle {
    #[default]
    Boxed,
    Flat,
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
    pub style: SidebarStyle,
    /// Whether a sidebar that is not `visible` shrinks to a one-cell strip
    /// instead of taking no space.
    pub collapse_to_strip: bool,
    pub panels: Vec<PanelGeom>,
}

/// Thickness of a collapsed sidebar's strip: one column for `Left`/`Right`,
/// one row for `Bottom`.
pub const STRIP_THICKNESS: u16 = 1;

impl SidebarGeom {
    /// Whether this sidebar is drawn as a strip: collapsed, and configured to
    /// leave one behind.
    pub fn is_strip(&self) -> bool {
        !self.visible && self.collapse_to_strip
    }

    /// The size this sidebar asks the layout for, or `None` when it takes no
    /// space.
    fn requested(&self) -> Option<u16> {
        if self.size == 0 {
            None
        } else if self.visible {
            Some(self.size)
        } else if self.collapse_to_strip {
            Some(STRIP_THICKNESS)
        } else {
            None
        }
    }
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

    // Expanded sidebars are resolved before strips, so a strip can never take
    // the last column an expanded sidebar was about to be granted. A strip is
    // one cell and is dropped by the same rule as any sidebar: when granting it
    // would leave fewer than the minimum content, so a terminal too small for
    // content is too small for strips as well.
    let passes = [false, true];

    // -- Verticals: share the column budget, left resolved before right. --
    let mut cols_left = term_cols;
    for strips in passes {
        for (i, s) in sidebars.iter().enumerate() {
            let Some(want) = s.requested() else {
                continue;
            };
            if s.is_strip() != strips {
                continue;
            }
            if !matches!(s.edge, SidebarEdge::Left | SidebarEdge::Right) {
                continue;
            }
            let budget = cols_left.saturating_sub(MIN_CONTENT_COLS);
            let granted = want.min(budget);
            if granted == 0 {
                // Logged because there is no other trace: a sidebar restored at a
                // persisted size that no longer fits simply is not painted, and to
                // the user that is indistinguishable from the feature being broken.
                log::debug!(
                    "sidebar: force-hiding the {:?} sidebar -- size {} does not fit in \
                     {term_cols}x{term_rows} while keeping {MIN_CONTENT_COLS} content columns",
                    s.edge,
                    want
                );
                continue;
            }
            sizes[i] = granted;
            cols_left -= granted;
        }
    }

    // -- Bottom: takes from the row budget. --
    let mut rows_left = term_rows;
    for strips in passes {
        for (i, s) in sidebars.iter().enumerate() {
            let Some(want) = s.requested() else {
                continue;
            };
            if s.is_strip() != strips || s.edge != SidebarEdge::Bottom {
                continue;
            }
            let budget = rows_left.saturating_sub(MIN_CONTENT_ROWS);
            let granted = want.min(budget);
            if granted == 0 {
                log::debug!(
                    "sidebar: force-hiding the bottom sidebar -- size {} does not fit in \
                     {term_cols}x{term_rows} while keeping {MIN_CONTENT_ROWS} content rows",
                    want
                );
                continue;
            }
            sizes[i] = granted;
            rows_left -= granted;
        }
    }

    sizes
}

/// The rect handed to the server as the client's `Resize`.
///
/// Its LAST row is the server's status-bar row. With no sidebar it is on
/// screen, as it has always been. With a sidebar the client draws the bar
/// itself across the terminal's last row (see [`status_bar_owned`]), so the
/// last row of this rect is VIRTUAL: the renderer drops it, and with a bottom
/// sidebar the screen row it would name belongs to that sidebar. The rect keeps
/// its size either way, so the `Resize` the server sees does not change.
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

/// The frame a sidebar gets under its own `style`, given its bar rect.
///
/// `Boxed` is [`sidebar_frame`] unchanged. `Flat` reserves exactly what the
/// tmux seam reserves, one column (or row, for the bottom edge) on the side
/// facing the panes, and nothing for a box. That reservation is the divider
/// itself, so the flat inner rect has no one-cell inset on any other side and
/// does not depend on the pane border style. Reusing the tmux derivation keeps
/// the "bar too small to carry a seam" fallback in one place.
pub fn frame_for(
    style: SidebarStyle,
    border: &BorderStyle,
    edge: SidebarEdge,
    bar: Rect,
) -> SidebarFrame {
    match style {
        SidebarStyle::Boxed => sidebar_frame(border, edge, bar),
        SidebarStyle::Flat => sidebar_frame(&BorderStyle::TmuxStyle, edge, bar),
    }
}

/// [`frame_size_inset`] for a sidebar of the given `style`.
pub fn inset_for(style: SidebarStyle, border: &BorderStyle, edge: SidebarEdge) -> u16 {
    match style {
        SidebarStyle::Boxed => frame_size_inset(border, edge),
        SidebarStyle::Flat => frame_size_inset(&BorderStyle::TmuxStyle, edge),
    }
}

/// How many cells of the axis a sidebar's `size` measures its frame consumes.
///
/// This is what a sidebar's minimum `size` has to clear on top of its plugins'
/// own minimums, or shrinking to that minimum would leave an interior narrower
/// than the plugin asked for.
///
/// **Measured, not tabulated.** It used to be a hardcoded `2` for zellij and
/// `1` for tmux -- a second statement of a fact [`sidebar_frame`] already
/// knows, and which `sidebar_frame` in turn takes from [`pane_content_rect`].
/// Two numbers that must agree, written in two places, is the same drift risk
/// as two copies of the drawing code. So it asks the real function against a
/// probe bar and measures what came back: one derivation, and it cannot
/// disagree with the frame that is actually drawn.
///
/// The probe is deliberately large -- the question is what a frame costs, not
/// what a bar too small to carry one degrades to.
pub fn frame_size_inset(style: &BorderStyle, edge: SidebarEdge) -> u16 {
    const PROBE: u16 = 64;
    let bar = Rect {
        x: 0,
        y: 0,
        width: PROBE,
        height: PROBE,
    };
    let frame = sidebar_frame(style, edge, bar);
    if !frame.framed {
        return 0;
    }
    match edge {
        SidebarEdge::Left | SidebarEdge::Right => PROBE.saturating_sub(frame.interior.width),
        SidebarEdge::Bottom => PROBE.saturating_sub(frame.interior.height),
    }
}

/// Whether the client draws the status bar itself, across the terminal's last
/// row, rather than showing the server's.
///
/// True exactly when a sidebar takes space, which is exactly when the content
/// rect is not the whole terminal. The server draws the bar as wide as the
/// content rect, so between two sidebars it would stop short of both edges and
/// its right-aligned segments would sit at the content's right edge. With no
/// sidebar the server's bar already spans the terminal, and it is left alone so
/// that case is unchanged.
pub fn status_bar_owned(content: Rect, term_cols: u16, term_rows: u16) -> bool {
    content
        != Rect {
            x: 0,
            y: 0,
            width: term_cols,
            height: term_rows,
        }
}

/// The screen row the client draws the status bar on, or `None` when the
/// server's own bar is shown. See [`status_bar_owned`].
pub fn status_bar_rect(sidebars: &[SidebarGeom], term_cols: u16, term_rows: u16) -> Option<Rect> {
    let content = content_rect(sidebars, term_cols, term_rows);
    (term_rows > 0 && status_bar_owned(content, term_cols, term_rows)).then_some(Rect {
        x: 0,
        y: term_rows - 1,
        width: term_cols,
        height: 1,
    })
}

/// Map a mouse position bound for the content to the server frame's
/// coordinates.
///
/// Positions are clamped into the content rect, so a gesture that strayed over
/// a sidebar stays on the content edge instead of underflowing. When the client
/// owns the status bar, a position on the terminal's last row is on the
/// client's bar. It maps to the server's bar row, the content rect's last, at
/// the SAME column: both bars are laid out from column 0 by the same code, so
/// the server's tab hit regions line up with what the user clicked.
pub fn content_point(
    content: Rect,
    term_cols: u16,
    term_rows: u16,
    col: u16,
    row: u16,
) -> (u16, u16) {
    let last_x = content.width.saturating_sub(1);
    let last_y = content.height.saturating_sub(1);
    if status_bar_owned(content, term_cols, term_rows) && row + 1 == term_rows {
        return (col.min(last_x), last_y);
    }
    (
        col.saturating_sub(content.x).min(last_x),
        row.saturating_sub(content.y).min(last_y),
    )
}

/// Whether a position is on the client's status bar beyond the columns the
/// server's bar covers. The client's bar is as wide as the terminal and the
/// server's only as wide as the content rect, so nothing on the server answers
/// a press there; [`content_point`] would clamp it onto the server bar's last
/// column instead.
pub fn past_the_server_bar(
    content: Rect,
    term_cols: u16,
    term_rows: u16,
    col: u16,
    row: u16,
) -> bool {
    status_bar_owned(content, term_cols, term_rows) && row + 1 == term_rows && col >= content.width
}

/// Absolute screen rects for every visible sidebar's BAR -- its full extent,
/// frame included -- as `(sidebar_index, rect)`.
///
/// Vertical sidebars span the terminal height down to the status bar, which
/// the client draws across the last row whenever a sidebar is shown. They are
/// laid out from each edge inward, so two on the same edge stack side by side
/// rather than overlapping. The bottom sidebar sits directly above the status
/// bar and spans only the columns between the verticals, which own the
/// corners.
pub fn bar_rects(sidebars: &[SidebarGeom], term_cols: u16, term_rows: u16) -> Vec<(usize, Rect)> {
    let sizes = effective_sizes(sidebars, term_cols, term_rows);
    let content = content_rect(sidebars, term_cols, term_rows);
    let mut out = Vec::new();

    // The rows above the status bar. With no sidebar nothing below is laid
    // out, so the bar row is only given up when there is something to give it
    // to.
    let body_rows = if status_bar_owned(content, term_cols, term_rows) {
        term_rows.saturating_sub(1)
    } else {
        term_rows
    };
    let mut left_x = 0u16;
    let mut right_x = term_cols;
    let mut bottom_y = body_rows;

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
                    height: body_rows,
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
                    height: body_rows,
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
/// Vertical sidebars span the terminal height down to the status bar and stack
/// their panels vertically; the bottom sidebar spans only the columns between
/// the verticals and stacks its panels horizontally. See [`bar_rects`].
pub fn panel_rects(
    sidebars: &[SidebarGeom],
    term_cols: u16,
    term_rows: u16,
    style: &BorderStyle,
) -> Vec<(usize, usize, Rect)> {
    let mut out = Vec::new();
    for (i, bar) in bar_rects(sidebars, term_cols, term_rows) {
        let s = &sidebars[i];
        if s.is_strip() {
            continue;
        }
        let frame = frame_for(s.style, style, s.edge, bar);
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

/// What a point on a sidebar's bar belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarHit {
    /// The collapse button of an expanded sidebar.
    Button,
    /// Anywhere on a collapsed sidebar's strip.
    Strip,
    /// The rest of an expanded sidebar's frame (or its panels, which a caller
    /// that has already ruled panels out never sees here).
    Frame,
}

/// The glyph of an expanded sidebar's collapse button: it points at the edge
/// the sidebar will retreat to.
pub fn collapse_glyph(edge: SidebarEdge) -> char {
    match edge {
        SidebarEdge::Left => '\u{25C0}',
        SidebarEdge::Right => '\u{25B6}',
        SidebarEdge::Bottom => '\u{25BC}',
    }
}

/// The glyph on a collapsed strip: it points back into the terminal.
pub fn expand_glyph(edge: SidebarEdge) -> char {
    match edge {
        SidebarEdge::Left => '\u{25B6}',
        SidebarEdge::Right => '\u{25C0}',
        SidebarEdge::Bottom => '\u{25B2}',
    }
}

/// Cells in a collapse chip: a space, the glyph, a space.
pub const CHIP_LEN: u16 = 3;

/// The narrowest interior (along the chip's row) that still gets a full chip.
/// Narrower than this the chip would cover most of its row, so the button
/// falls back to a single glyph on the edge line.
const MIN_CHIP_ROOM: u16 = 6;

/// The cell of a bar's inner edge that holds the single-glyph fallback: the
/// bottom end of a vertical sidebar's edge line, the right end of the bottom
/// sidebar's.
///
/// `inset` steps the cell back along the line, to clear a box corner.
fn glyph_cell(edge: SidebarEdge, bar: Rect, inset: u16) -> Option<Rect> {
    let (x, y) = match edge {
        SidebarEdge::Left => (
            bar.x + bar.width.checked_sub(1)?,
            bar.y + bar.height.checked_sub(1 + inset)?,
        ),
        SidebarEdge::Right => (bar.x, bar.y + bar.height.checked_sub(1 + inset)?),
        SidebarEdge::Bottom => (bar.x + bar.width.checked_sub(1 + inset)?, bar.y),
    };
    Some(Rect {
        x,
        y,
        width: 1,
        height: 1,
    })
}

/// The collapse button of an EXPANDED sidebar, in screen coordinates, or
/// `None` when the bar has nowhere to put one.
///
/// The button is a three-cell chip inside the sidebar's interior, against the
/// edge facing the panes: on the interior's last row, right-aligned for a left
/// sidebar and left-aligned for a right one; on the bottom sidebar's first
/// interior row, at its right end. Taking the interior from [`frame_for`] means
/// the chip sits inside a box and beside a flat divider without a per-style
/// case. Its panel's content on those cells is overpainted.
///
/// An interior too narrow for the chip gets a single glyph on the edge line
/// instead (one cell in from a zellij box's corner). The returned rect is what
/// is drawn and what is hit-tested, so the two cannot disagree.
pub fn collapse_button(
    style: SidebarStyle,
    border: &BorderStyle,
    edge: SidebarEdge,
    bar: Rect,
) -> Option<Rect> {
    let frame = frame_for(style, border, edge, bar);
    if !frame.framed {
        return None;
    }
    let inner = frame.interior;
    if inner.width >= MIN_CHIP_ROOM && inner.height >= 1 {
        let y = match edge {
            SidebarEdge::Left | SidebarEdge::Right => inner.y + inner.height - 1,
            SidebarEdge::Bottom => inner.y,
        };
        let x = match edge {
            SidebarEdge::Right => inner.x,
            _ => inner.x + inner.width - CHIP_LEN,
        };
        return Some(Rect {
            x,
            y,
            width: CHIP_LEN,
            height: 1,
        });
    }
    let boxed_corner = style == SidebarStyle::Boxed && matches!(border, BorderStyle::ZellijStyle);
    let inset = u16::from(boxed_corner);
    let along = match edge {
        SidebarEdge::Left | SidebarEdge::Right => bar.height,
        SidebarEdge::Bottom => bar.width,
    };
    if along < 3 + inset {
        return None;
    }
    glyph_cell(edge, bar, inset)
}

/// The chip on a collapsed strip, in screen coordinates: three cells along the
/// strip, centred, with the glyph in the middle one. A strip shorter than the
/// chip gets a single centred glyph, and a zero-length one gets nothing.
///
/// The strip is `bar` itself.
pub fn strip_chip(edge: SidebarEdge, bar: Rect) -> Option<Rect> {
    let vertical = !matches!(edge, SidebarEdge::Bottom);
    let len = if vertical { bar.height } else { bar.width };
    if len == 0 || bar.width == 0 || bar.height == 0 {
        return None;
    }
    let n = len.min(CHIP_LEN);
    let at = (len - n) / 2;
    Some(if vertical {
        Rect {
            x: bar.x,
            y: bar.y + at,
            width: 1,
            height: n,
        }
    } else {
        Rect {
            x: bar.x + at,
            y: bar.y,
            width: n,
            height: 1,
        }
    })
}

/// What the sidebar bar under a screen point is, as `(sidebar_index, hit)`.
///
/// Painting and hit-testing both go through [`collapse_button`] and
/// [`bar_rects`], so a button is clickable exactly where it is drawn.
pub fn bar_hit(
    sidebars: &[SidebarGeom],
    term_cols: u16,
    term_rows: u16,
    border: &BorderStyle,
    x: u16,
    y: u16,
) -> Option<(usize, BarHit)> {
    let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
    for (i, bar) in bar_rects(sidebars, term_cols, term_rows) {
        if !inside(bar) {
            continue;
        }
        let s = &sidebars[i];
        if s.is_strip() {
            return Some((i, BarHit::Strip));
        }
        let button = collapse_button(s.style, border, s.edge, bar);
        return Some((
            i,
            if button.is_some_and(inside) {
                BarHit::Button
            } else {
                BarHit::Frame
            },
        ));
    }
    None
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
            style: Default::default(),
            collapse_to_strip: false,
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
        // full terminal height down to the status bar, which the client draws
        // across the last row whenever a sidebar is shown.
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
                height: 39
            }
        );
        assert_eq!(
            right,
            Rect {
                x: 100,
                y: 0,
                width: 20,
                height: 39
            }
        );
        assert_eq!(
            bottom,
            Rect {
                x: 30,
                y: 33,
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
                height: 37
            }
        );
        assert_eq!(
            right,
            Rect {
                x: 101,
                y: 1,
                width: 18,
                height: 37
            }
        );
        assert_eq!(
            bottom,
            Rect {
                x: 31,
                y: 34,
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
                height: 39
            }
        );
        assert_eq!(
            right,
            Rect {
                x: 101,
                y: 0,
                width: 19,
                height: 39
            }
        );
        assert_eq!(
            bottom,
            Rect {
                x: 30,
                y: 34,
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
    fn the_size_inset_is_measured_from_the_frame_not_tabulated() {
        // Derived from `sidebar_frame`, so it cannot disagree with the frame
        // that is actually drawn. Pinned so a change to either is deliberate.
        for edge in [SidebarEdge::Left, SidebarEdge::Right, SidebarEdge::Bottom] {
            assert_eq!(frame_size_inset(&ZJ, edge), 2, "zellij {edge:?}");
            assert_eq!(
                frame_size_inset(&BorderStyle::TmuxStyle, edge),
                1,
                "tmux {edge:?}"
            );
        }
        // And the derivation really is the shared one: for zellij the whole
        // inset is what `pane_content_rect` takes off the axis.
        let bar = Rect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        };
        assert_eq!(
            frame_size_inset(&ZJ, SidebarEdge::Left),
            bar.width - pane_content_rect(&ZJ, bar, false).width
        );
    }

    /// The property the inset exists for: a sidebar shrunk to its floor still
    /// hands its plugin the interior the plugin asked for.
    #[test]
    fn the_size_inset_is_exactly_what_a_floor_has_to_clear() {
        // Bottom is included deliberately: it is the edge where the axis the
        // inset measures flips from width to height, which is the whole reason
        // `frame_size_inset` takes an `edge` at all. Left/Right alone would
        // pass on a version that always measured width.
        for style in [ZJ, BorderStyle::TmuxStyle] {
            for edge in [SidebarEdge::Left, SidebarEdge::Right, SidebarEdge::Bottom] {
                let want = 8u16; // a plugin minimum, on the axis `size` measures
                let size = want + frame_size_inset(&style, edge);
                let vertical = !matches!(edge, SidebarEdge::Bottom);
                let bar = if vertical {
                    Rect {
                        x: 0,
                        y: 0,
                        width: size,
                        height: 40,
                    }
                } else {
                    Rect {
                        x: 0,
                        y: 0,
                        width: 40,
                        height: size,
                    }
                };
                let interior = sidebar_frame(&style, edge, bar).interior;
                let got = if vertical {
                    interior.width
                } else {
                    interior.height
                };
                assert_eq!(
                    got, want,
                    "{style:?}/{edge:?}: a sidebar at its floor gave the plugin {got} cells"
                );
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
    fn a_flat_interior_loses_only_the_divider_whatever_the_border_style() {
        let bar = Rect {
            x: 10,
            y: 0,
            width: 30,
            height: 40,
        };
        for border in [BorderStyle::ZellijStyle, BorderStyle::TmuxStyle] {
            let left = frame_for(SidebarStyle::Flat, &border, SidebarEdge::Left, bar);
            assert!(left.framed);
            assert_eq!(left.interior, Rect { width: 29, ..bar });
            let right = frame_for(SidebarStyle::Flat, &border, SidebarEdge::Right, bar);
            assert_eq!(
                right.interior,
                Rect {
                    x: 11,
                    width: 29,
                    ..bar
                }
            );
            let bottom = frame_for(SidebarStyle::Flat, &border, SidebarEdge::Bottom, bar);
            assert_eq!(
                bottom.interior,
                Rect {
                    y: 1,
                    height: 39,
                    ..bar
                }
            );
            assert_eq!(left.gap, 1);
        }
    }

    #[test]
    fn a_boxed_sidebar_is_framed_exactly_as_before() {
        let bar = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 40,
        };
        for border in [BorderStyle::ZellijStyle, BorderStyle::TmuxStyle] {
            for edge in [SidebarEdge::Left, SidebarEdge::Right, SidebarEdge::Bottom] {
                assert_eq!(
                    frame_for(SidebarStyle::Boxed, &border, edge, bar),
                    sidebar_frame(&border, edge, bar)
                );
                assert_eq!(
                    inset_for(SidebarStyle::Boxed, &border, edge),
                    frame_size_inset(&border, edge)
                );
            }
        }
        assert_eq!(
            inset_for(SidebarStyle::Flat, &ZJ, SidebarEdge::Left),
            1,
            "a flat sidebar spends one column of its size on the divider"
        );
    }

    #[test]
    fn flat_panels_stack_with_a_one_row_rule_and_no_inset() {
        let mut flat = sb(SidebarEdge::Left, 30, &[1, 1]);
        flat.style = SidebarStyle::Flat;
        let rects = panel_rects(&[flat], 100, 30, &ZJ);
        assert_eq!(rects.len(), 2);
        // A 29-row bar, minus one row for the rule, split 14 and 14 (the last
        // panel takes the remainder, so 14 + 14 = 28).
        assert_eq!(
            rects[0].2,
            Rect {
                x: 0,
                y: 0,
                width: 29,
                height: 14
            }
        );
        assert_eq!(
            rects[1].2,
            Rect {
                x: 0,
                y: 15,
                width: 29,
                height: 14
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
        // A 30-row terminal gives a 29-row bar above the status bar and a
        // 27-row interior. One of those is the rule between the two panels, so
        // 26 rows divide 2:1 into 17 and 9 with the rule at bar-local row 18.
        let sbs = [sb(SidebarEdge::Left, 30, &[2, 1])];
        let rects = panel_rects(&sbs, 100, 30, &ZJ);
        assert_eq!(rects.len(), 2);
        assert_eq!(
            rects[0].2,
            Rect {
                x: 1,
                y: 1,
                width: 28,
                height: 17
            }
        );
        assert_eq!(
            rects[1].2,
            Rect {
                x: 1,
                y: 19,
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
        // now runs on the INTERIOR minus the rules -- a 33-row terminal gives a
        // 32-row bar above the status bar and a 30-row interior, two rules
        // leave 28 for content, and 28 over three
        // equal weights is 9/9/10. The invariant is the same one: content plus
        // rules must fill the interior exactly, with nothing lost to rounding.
        let sbs = [sb(SidebarEdge::Left, 30, &[1, 1, 1])];
        let rects = panel_rects(&sbs, 100, 33, &ZJ);
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
                y: 24,
                width: 48,
                height: 4
            }
        );
        assert_eq!(
            rects[1].2,
            Rect {
                x: 50,
                y: 24,
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
            style: Default::default(),
            collapse_to_strip: false,
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
        // 11 rows total: a 10-row bar above the status bar, so an 8-row
        // interior. The second panel's weighted
        // share of the 7 rows left after the rule is 0, below its min of 8, so
        // it is dropped -- and dropping it returns the rule as well, leaving the
        // survivor the whole 8-row interior (it was the whole 10-row bar before
        // frames).
        let rects = panel_rects(&sbs, 100, 11, &ZJ);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].2.height, 8);
    }

    #[test]
    fn a_panel_min_size_is_measured_against_the_interior_not_the_bar() {
        // The discriminating case for checking mins after the frame: a 10-row
        // bar (an 11-row terminal less the status bar) has an 8-row interior,
        // so a panel asking for 9 rows does NOT fit even though the bar is big
        // enough for it. Measuring against the bar would lay out a panel one
        // row taller than the rect it is handed.
        let sbs = [SidebarGeom {
            edge: SidebarEdge::Left,
            size: 30,
            visible: true,
            style: Default::default(),
            collapse_to_strip: false,
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
        let rects = panel_rects(&sbs, 100, 11, &ZJ);
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
        // First declared sits closest to the bottom edge, directly above the
        // status bar row; the second stacks above it.
        assert_eq!(
            first,
            Rect {
                x: 0,
                y: 33,
                width: 100,
                height: 6
            }
        );
        assert_eq!(
            second,
            Rect {
                x: 0,
                y: 29,
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
                height: 29
            }
        );
        assert_eq!(
            rects[1].1,
            Rect {
                x: 20,
                y: 0,
                width: 10,
                height: 29
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

    // -- the client-drawn status bar ---------------------------------------

    #[test]
    fn with_no_sidebar_the_server_keeps_its_status_bar() {
        assert_eq!(status_bar_rect(&[], 100, 30), None);
        assert!(!status_bar_owned(content_rect(&[], 100, 30), 100, 30));
        // A sidebar that is configured but hidden takes no space, so it must not
        // take the bar either.
        let mut hidden = sb(SidebarEdge::Left, 30, &[1]);
        hidden.visible = false;
        assert_eq!(status_bar_rect(&[hidden], 100, 30), None);
    }

    #[test]
    fn any_shown_sidebar_puts_the_bar_across_the_whole_last_row() {
        let full_width = Some(Rect {
            x: 0,
            y: 29,
            width: 100,
            height: 1,
        });
        for edge in [SidebarEdge::Left, SidebarEdge::Right, SidebarEdge::Bottom] {
            assert_eq!(
                status_bar_rect(&[sb(edge, 8, &[1])], 100, 30),
                full_width,
                "{edge:?}"
            );
        }
    }

    #[test]
    fn left_and_right_sidebars_stop_one_row_above_the_status_bar() {
        let sbs = [
            sb(SidebarEdge::Left, 20, &[1]),
            sb(SidebarEdge::Right, 15, &[1]),
        ];
        let bar = status_bar_rect(&sbs, 100, 30).unwrap();
        for (_, r) in bar_rects(&sbs, 100, 30) {
            assert_eq!(r.y + r.height, bar.y, "{r:?} must end where the bar begins");
        }
    }

    #[test]
    fn the_bottom_sidebar_sits_directly_above_the_status_bar() {
        let sbs = [sb(SidebarEdge::Bottom, 6, &[1])];
        let bar = status_bar_rect(&sbs, 100, 30).unwrap();
        let bottom = bar_rects(&sbs, 100, 30)[0].1;
        assert_eq!(bottom.y + bottom.height, bar.y);
        // The panes end directly above the sidebar, so directional entry and
        // the pane area are unchanged by the move.
        let content = content_rect(&sbs, 100, 30);
        let panes = pane_area(content, &StatusBarPosition::Bottom);
        assert_eq!(panes.y + panes.height, bottom.y);
    }

    /// The server is sent the same `Resize` as before the bar moved: the rect
    /// keeps its size and only its last row stops being on screen.
    #[test]
    fn owning_the_bar_does_not_change_the_rect_the_server_is_sent() {
        let sbs = [
            sb(SidebarEdge::Left, 20, &[1]),
            sb(SidebarEdge::Bottom, 6, &[1]),
        ];
        assert_eq!(
            content_rect(&sbs, 100, 30),
            Rect {
                x: 20,
                y: 0,
                width: 80,
                height: 24
            }
        );
    }

    #[test]
    fn a_click_on_the_client_bar_lands_on_the_servers_bar_row_at_the_same_column() {
        let sbs = [
            sb(SidebarEdge::Left, 20, &[1]),
            sb(SidebarEdge::Bottom, 6, &[1]),
        ];
        let content = content_rect(&sbs, 100, 30);
        // Column 5 is under the left sidebar in every row but the last. On the
        // bar it is column 5 of the server's bar row, NOT content column 0.
        assert_eq!(content_point(content, 100, 30, 5, 29), (5, 23));
        // Past the server bar's width it clamps rather than leaving the row,
        // which is right for a drag and wrong for a press.
        assert_eq!(content_point(content, 100, 30, 99, 29), (79, 23));
        assert!(past_the_server_bar(content, 100, 30, 80, 29));
        assert!(!past_the_server_bar(content, 100, 30, 79, 29));
        assert!(!past_the_server_bar(content, 100, 30, 90, 28));
        // Elsewhere the mapping is the plain clamped translation.
        assert_eq!(content_point(content, 100, 30, 25, 3), (5, 3));
        assert_eq!(content_point(content, 100, 30, 5, 3), (0, 3));
    }

    #[test]
    fn with_no_sidebar_a_click_maps_exactly_as_before() {
        let content = content_rect(&[], 100, 30);
        assert_eq!(content_point(content, 100, 30, 7, 29), (7, 29));
        assert_eq!(content_point(content, 100, 30, 99, 0), (99, 0));
        assert!(!past_the_server_bar(content, 100, 30, 99, 29));
    }

    fn strip_of(edge: SidebarEdge) -> SidebarGeom {
        let mut s = sb(edge, 30, &[1]);
        s.visible = false;
        s.collapse_to_strip = true;
        s
    }

    #[test]
    fn a_collapsed_sidebar_is_a_one_cell_strip_on_its_edge() {
        let r = |e| bar_rects(&[strip_of(e)], 100, 30);
        assert_eq!(
            r(SidebarEdge::Left)[0].1,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 29
            }
        );
        assert_eq!(
            r(SidebarEdge::Right)[0].1,
            Rect {
                x: 99,
                y: 0,
                width: 1,
                height: 29
            }
        );
        assert_eq!(
            r(SidebarEdge::Bottom)[0].1,
            Rect {
                x: 0,
                y: 28,
                width: 100,
                height: 1
            }
        );
        // The panes get the rest of the terminal.
        assert_eq!(
            content_rect(&[strip_of(SidebarEdge::Left)], 100, 30),
            Rect {
                x: 1,
                y: 0,
                width: 99,
                height: 30
            }
        );
        assert_eq!(
            content_rect(&[strip_of(SidebarEdge::Bottom)], 100, 30).height,
            29
        );
    }

    #[test]
    fn without_collapse_to_strip_a_hidden_sidebar_takes_nothing() {
        let mut s = strip_of(SidebarEdge::Left);
        s.collapse_to_strip = false;
        assert!(bar_rects(&[s.clone()], 100, 30).is_empty());
        assert_eq!(content_rect(&[s], 100, 30).width, 100);
    }

    #[test]
    fn a_strip_owns_no_panel() {
        assert!(panel_rects(&[strip_of(SidebarEdge::Left)], 100, 30, &ZJ).is_empty());
    }

    #[test]
    fn strips_on_different_edges_do_not_overlap() {
        let sbs = [
            strip_of(SidebarEdge::Left),
            strip_of(SidebarEdge::Right),
            strip_of(SidebarEdge::Bottom),
        ];
        let bars = bar_rects(&sbs, 100, 30);
        assert_eq!(bars.len(), 3);
        // Verticals own the corners; the bottom strip spans what is between.
        assert_eq!(
            bars[2].1,
            Rect {
                x: 1,
                y: 28,
                width: 98,
                height: 1
            }
        );
        for (i, (_, a)) in bars.iter().enumerate() {
            for (_, b) in bars.iter().skip(i + 1) {
                let apart = a.x + a.width <= b.x
                    || b.x + b.width <= a.x
                    || a.y + a.height <= b.y
                    || b.y + b.height <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_strip_is_dropped_by_the_same_rule_as_any_sidebar() {
        // 20 columns is the content minimum: not even a one-cell strip fits.
        assert!(bar_rects(&[strip_of(SidebarEdge::Left)], 20, 30).is_empty());
        assert_eq!(bar_rects(&[strip_of(SidebarEdge::Left)], 21, 30).len(), 1);
        assert!(bar_rects(&[strip_of(SidebarEdge::Bottom)], 100, 5).is_empty());
    }

    #[test]
    fn a_strip_never_starves_an_expanded_sidebar_of_its_budget() {
        // 40 columns leave 20 to give. The expanded right sidebar asks for 30
        // and is granted all 20 even though the left strip is listed first.
        let sbs = [
            strip_of(SidebarEdge::Left),
            sb(SidebarEdge::Right, 30, &[1]),
        ];
        assert_eq!(effective_sizes(&sbs, 40, 30), vec![0, 20]);
    }

    #[test]
    fn the_collapse_button_is_a_three_cell_chip_inside_the_sidebar_against_the_edge() {
        let bar = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 29,
        };
        let chip = |style, border: &BorderStyle, edge, bar| {
            collapse_button(style, border, edge, bar).map(|r| (r.x, r.y, r.width, r.height))
        };
        // Flat: the last row, right-aligned against the divider (column 29).
        assert_eq!(
            chip(SidebarStyle::Flat, &ZJ, SidebarEdge::Left, bar),
            Some((26, 28, 3, 1))
        );
        // Boxed zellij: the last INTERIOR row, against the inner border.
        assert_eq!(
            chip(SidebarStyle::Boxed, &ZJ, SidebarEdge::Left, bar),
            Some((26, 27, 3, 1))
        );
        assert_eq!(
            chip(
                SidebarStyle::Boxed,
                &BorderStyle::TmuxStyle,
                SidebarEdge::Left,
                bar
            ),
            Some((26, 28, 3, 1))
        );
        // A right sidebar: left-aligned against the divider at its first column.
        let right = Rect { x: 70, ..bar };
        assert_eq!(
            chip(SidebarStyle::Flat, &ZJ, SidebarEdge::Right, right),
            Some((71, 28, 3, 1))
        );
        assert_eq!(
            chip(SidebarStyle::Boxed, &ZJ, SidebarEdge::Right, right),
            Some((71, 27, 3, 1))
        );
        // The bottom sidebar: the first interior row, at the right end.
        let bottom = Rect {
            x: 0,
            y: 20,
            width: 100,
            height: 8,
        };
        assert_eq!(
            chip(SidebarStyle::Flat, &ZJ, SidebarEdge::Bottom, bottom),
            Some((97, 21, 3, 1))
        );
        assert_eq!(
            chip(SidebarStyle::Boxed, &ZJ, SidebarEdge::Bottom, bottom),
            Some((96, 21, 3, 1))
        );
    }

    #[test]
    fn the_chip_always_lies_inside_the_interior_its_panels_share() {
        for style in [SidebarStyle::Flat, SidebarStyle::Boxed] {
            for border in [ZJ, BorderStyle::TmuxStyle] {
                for edge in [SidebarEdge::Left, SidebarEdge::Right, SidebarEdge::Bottom] {
                    let bar = Rect {
                        x: 5,
                        y: 3,
                        width: 30,
                        height: 12,
                    };
                    let inner = frame_for(style, &border, edge, bar).interior;
                    let c = collapse_button(style, &border, edge, bar).unwrap();
                    assert_eq!(c.width, CHIP_LEN);
                    assert!(
                        c.x >= inner.x
                            && c.x + c.width <= inner.x + inner.width
                            && c.y >= inner.y
                            && c.y < inner.y + inner.height,
                        "{style:?} {border:?} {edge:?}: {c:?} outside {inner:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_sidebar_too_narrow_for_the_chip_falls_back_to_one_glyph() {
        let narrow = Rect {
            x: 0,
            y: 0,
            width: 5,
            height: 29,
        };
        assert_eq!(
            collapse_button(SidebarStyle::Flat, &ZJ, SidebarEdge::Left, narrow),
            Some(Rect {
                x: 4,
                y: 28,
                width: 1,
                height: 1
            })
        );
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 5,
            height: 2,
        };
        assert_eq!(
            collapse_button(SidebarStyle::Flat, &ZJ, SidebarEdge::Left, tiny),
            None
        );
    }

    #[test]
    fn the_strip_chip_is_three_cells_centred_along_the_strip() {
        let v = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 29,
        };
        assert_eq!(
            strip_chip(SidebarEdge::Left, v),
            Some(Rect {
                x: 0,
                y: 13,
                width: 1,
                height: 3
            })
        );
        let r = Rect { x: 99, ..v };
        assert_eq!(
            strip_chip(SidebarEdge::Right, r).map(|c| (c.x, c.height)),
            Some((99, 3))
        );
        let h = Rect {
            x: 1,
            y: 28,
            width: 98,
            height: 1,
        };
        assert_eq!(
            strip_chip(SidebarEdge::Bottom, h),
            Some(Rect {
                x: 1 + 47,
                y: 28,
                width: 3,
                height: 1
            })
        );
        // Shorter than the chip: what fits, centred. Nothing: nothing.
        assert_eq!(
            strip_chip(SidebarEdge::Left, Rect { height: 2, ..v }).map(|c| c.height),
            Some(2)
        );
        assert_eq!(strip_chip(SidebarEdge::Left, Rect { height: 0, ..v }), None);
    }

    #[test]
    fn bar_hit_finds_the_button_the_strip_and_the_rest_of_the_frame() {
        let mut flat = sb(SidebarEdge::Left, 30, &[1]);
        flat.style = SidebarStyle::Flat;
        let sbs = [flat];
        // The button is where `collapse_button` puts it, no other cell is.
        let bar = bar_rects(&sbs, 100, 30)[0].1;
        let b = collapse_button(SidebarStyle::Flat, &ZJ, SidebarEdge::Left, bar).unwrap();
        for y in 0..29 {
            for x in 0..30 {
                let hit = bar_hit(&sbs, 100, 30, &ZJ, x, y).map(|(_, h)| h);
                let on_chip = y == b.y && x >= b.x && x < b.x + b.width;
                let want = if on_chip {
                    BarHit::Button
                } else {
                    BarHit::Frame
                };
                assert_eq!(hit, Some(want), "({x},{y})");
            }
        }
        assert_eq!(
            bar_hit(&sbs, 100, 30, &ZJ, 50, 5),
            None,
            "content is not chrome"
        );

        let strips = [strip_of(SidebarEdge::Left)];
        for y in 0..29 {
            assert_eq!(
                bar_hit(&strips, 100, 30, &ZJ, 0, y),
                Some((0, BarHit::Strip)),
                "anywhere on the strip, row {y}"
            );
        }
        assert_eq!(bar_hit(&strips, 100, 30, &ZJ, 1, 5), None);
    }
}
