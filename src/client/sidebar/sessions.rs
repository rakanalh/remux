//! The session-tree plugin. Implemented in Task 12; this stub keeps the plugin
//! registry compiling.

use crossterm::event::{KeyEvent, MouseEventKind};

use super::{blank_grid, draw_text, PluginAction, PluginEvent, SidebarPlugin};
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, RenderCell};

pub struct SessionsPlugin {}

impl SessionsPlugin {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for SessionsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SidebarPlugin for SessionsPlugin {
    fn title(&self) -> &str {
        "Sessions"
    }

    fn min_size(&self) -> (u16, u16) {
        (12, 3)
    }

    fn render(
        &self,
        cols: u16,
        rows: u16,
        _focused: bool,
        theme: &CompositorTheme,
    ) -> Vec<Vec<RenderCell>> {
        let bg = theme.frame_bg.clone().unwrap_or(CellColor::Default);
        let mut grid = blank_grid(cols, rows, bg.clone());
        if !grid.is_empty() {
            draw_text(&mut grid, 0, 0, self.title(), theme.frame_fg.clone(), bg);
        }
        grid
    }

    fn on_key(&mut self, _key: KeyEvent) -> PluginAction {
        PluginAction::None
    }

    fn on_mouse(&mut self, _x: u16, _y: u16, _kind: MouseEventKind) -> PluginAction {
        PluginAction::None
    }

    fn on_event(&mut self, _ev: &PluginEvent) {}
}
