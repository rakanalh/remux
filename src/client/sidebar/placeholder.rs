//! A minimal plugin that renders its title and a focus marker.
//!
//! Exists so the chrome, renderer, navigation, and mouse routing can be built
//! and tested before any real plugin exists. Kept afterwards as a fixture.

use crossterm::event::{KeyEvent, MouseEventKind};

use super::{blank_grid, draw_text, PluginAction, PluginEvent, SidebarPlugin};
use crate::config::theme::CompositorTheme;
use crate::protocol::RenderCell;

pub struct PlaceholderPlugin {
    counter: u32,
}

impl PlaceholderPlugin {
    pub fn new() -> Self {
        Self { counter: 0 }
    }
}

impl Default for PlaceholderPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SidebarPlugin for PlaceholderPlugin {
    fn title(&self) -> &str {
        "Placeholder"
    }

    fn min_size(&self) -> (u16, u16) {
        (8, 2)
    }

    fn render(
        &self,
        cols: u16,
        rows: u16,
        focused: bool,
        theme: &CompositorTheme,
    ) -> Vec<Vec<RenderCell>> {
        let bg = theme
            .frame_bg
            .clone()
            .unwrap_or(crate::protocol::CellColor::Default);
        let mut grid = blank_grid(cols, rows, bg.clone());
        if grid.is_empty() {
            return grid;
        }
        let header_fg = if focused {
            theme.frame_active_fg.clone()
        } else {
            theme.frame_fg.clone()
        };
        draw_text(&mut grid, 0, 0, self.title(), header_fg, bg.clone());
        if rows > 1 {
            let marker = if focused { "focused" } else { "idle" };
            draw_text(
                &mut grid,
                0,
                1,
                &format!("{marker} {}", self.counter),
                theme.status_bar_fg.clone(),
                bg,
            );
        }
        grid
    }

    fn on_key(&mut self, key: KeyEvent) -> PluginAction {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char('j') | KeyCode::Char('k') => {
                self.counter = self.counter.wrapping_add(1);
                PluginAction::Redraw
            }
            _ => PluginAction::None,
        }
    }

    fn on_mouse(&mut self, _x: u16, _y: u16, kind: MouseEventKind) -> PluginAction {
        use crossterm::event::MouseButton;
        if let MouseEventKind::Down(MouseButton::Left) = kind {
            self.counter = self.counter.wrapping_add(1);
            return PluginAction::Redraw;
        }
        PluginAction::None
    }

    fn on_event(&mut self, _ev: &PluginEvent) {}
}

#[cfg(test)]
mod tests {
    use crate::client::sidebar::{blank_grid, draw_text, make_plugin};
    use crate::config::theme::CompositorTheme;
    use crate::protocol::CellColor;

    #[test]
    fn registry_resolves_the_placeholder() {
        assert!(make_plugin("placeholder").is_some());
    }

    #[test]
    fn registry_returns_none_for_an_unknown_name() {
        assert!(make_plugin("no-such-plugin").is_none());
    }

    #[test]
    fn render_returns_exactly_the_requested_dimensions() {
        let p = make_plugin("placeholder").unwrap();
        let theme = CompositorTheme::default();
        let grid = p.render(24, 7, false, &theme);
        assert_eq!(grid.len(), 7);
        assert!(grid.iter().all(|r| r.len() == 24));
    }

    #[test]
    fn render_at_zero_size_returns_an_empty_grid_and_does_not_panic() {
        let p = make_plugin("placeholder").unwrap();
        let theme = CompositorTheme::default();
        assert!(p.render(0, 0, false, &theme).is_empty());
    }

    #[test]
    fn the_title_appears_in_the_first_row() {
        let p = make_plugin("placeholder").unwrap();
        let theme = CompositorTheme::default();
        let grid = p.render(24, 4, false, &theme);
        let row: String = grid[0].iter().map(|c| c.c).collect();
        assert!(
            row.contains(p.title()),
            "title missing from header row: {row:?}"
        );
    }

    #[test]
    fn blank_grid_is_filled_with_spaces() {
        let g = blank_grid(3, 2, CellColor::Default);
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|r| r.iter().all(|c| c.c == ' ')));
    }

    #[test]
    fn draw_text_clips_at_the_right_edge_instead_of_panicking() {
        let mut g = blank_grid(5, 1, CellColor::Default);
        draw_text(
            &mut g,
            3,
            0,
            "abcdefgh",
            CellColor::Default,
            CellColor::Default,
        );
        let row: String = g[0].iter().map(|c| c.c).collect();
        assert_eq!(row, "   ab");
    }

    #[test]
    fn draw_text_off_grid_is_a_no_op() {
        let mut g = blank_grid(5, 1, CellColor::Default);
        draw_text(&mut g, 9, 9, "x", CellColor::Default, CellColor::Default);
        assert!(g[0].iter().all(|c| c.c == ' '));
    }

    #[test]
    fn draw_text_reserves_a_continuation_cell_for_wide_glyphs() {
        // RenderCell.width == 0 marks the continuation of a wide lead; the
        // renderer skips those, so a panel must emit them or CJK text shifts.
        let mut g = blank_grid(4, 1, CellColor::Default);
        draw_text(&mut g, 0, 0, "日本", CellColor::Default, CellColor::Default);
        assert_eq!(g[0][0].width, 2);
        assert_eq!(g[0][1].width, 0);
        assert_eq!(g[0][2].width, 2);
        assert_eq!(g[0][3].width, 0);
    }
}
