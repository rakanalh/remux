//! The right-click context menu.
//!
//! Pure client state: where the menu sits, which row is highlighted and what a
//! row does. Deciding WHAT was clicked, and so which items exist, is the
//! caller's job (the server for a pane or a tab label, a sidebar plugin for its
//! rows). This module only lays the menu out on screen, moves a highlight and
//! turns a click or a key into an action.

use crossterm::event::{KeyCode, KeyEvent};

use crate::client::registry::ConnId;
use crate::client::sidebar::PluginRequest;
use crate::client::tree_model::JumpTarget;
use crate::client::whichkey::DrawCommand;
use crate::config::theme::Theme;
use crate::config::ZoomMode;
use crate::protocol::{ContextTarget, PaneId, RemuxCommand};
use crate::server::layout::Rect;

/// What choosing a row does. Every variant maps to a path the keyboard already
/// has, so the menu adds no new behaviour of its own.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuAction {
    /// Send a command to the foreground server. A row that acts on the clicked
    /// pane or tab wraps its command in `OnPane` or `OnTab`, so the server
    /// resolves the target when the row is chosen and drops it if it is gone.
    Command(RemuxCommand),
    /// Open the rename prompt for the named entity.
    Rename(RenameFor),
    /// Go to a session, tab or pane, on any connection.
    Jump(JumpTarget),
    /// Go to a target, then run an action that acts on the focused entity:
    /// the server's focus is where the jump landed.
    JumpThen(JumpTarget, Box<MenuAction>),
    /// Kill a session, as the session manager's delete does.
    KillSession { conn: ConnId, name: String },
    /// Close a tab by its id, so a tab moved since the menu opened is still the
    /// one closed.
    CloseTab {
        conn: ConnId,
        session: String,
        tab_id: u64,
    },
    /// Close a pane by id, as the session manager's close does.
    ClosePane {
        conn: ConnId,
        session: String,
        pane_id: u64,
    },
    /// Hand a request to the client, as a sidebar plugin does.
    Request(PluginRequest),
    /// Copy text to the clipboard.
    CopyText(String),
    /// Close the menu. The "Cancel" row of a confirmation.
    Cancel,
}

/// The entity the rename prompt renames. It names its target, so the new name
/// lands on what was clicked and not on what is focused when Enter is pressed.
#[derive(Debug, Clone, PartialEq)]
pub enum RenameFor {
    /// A pane of the foreground session.
    Pane(PaneId),
    /// A tab of the foreground session, by id.
    Tab(u64),
    /// A session on any connection, by name.
    Session { conn: ConnId, name: String },
    /// A tab of a session on any connection, by id.
    SessionTab {
        conn: ConnId,
        session: String,
        tab_id: u64,
    },
}

impl RenameFor {
    /// The word the prompt is titled with.
    pub fn label(&self) -> &'static str {
        match self {
            RenameFor::Pane(_) => "Pane",
            RenameFor::Tab(_) | RenameFor::SessionTab { .. } => "Tab",
            RenameFor::Session { .. } => "Session",
        }
    }

    /// The command that applies `name`, and the connection it goes to, `None`
    /// for the foreground server.
    pub fn command(&self, name: String) -> (Option<ConnId>, RemuxCommand) {
        match self {
            RenameFor::Pane(pane_id) => (None, on_pane(*pane_id, RemuxCommand::PaneRename(name))),
            RenameFor::Tab(tab_id) => (None, on_tab(*tab_id, RemuxCommand::TabRename(name))),
            RenameFor::Session { conn, name: old } => (
                Some(conn.clone()),
                RemuxCommand::SessionRenameByName {
                    old: old.clone(),
                    new: name,
                },
            ),
            RenameFor::SessionTab {
                conn,
                session,
                tab_id,
            } => (
                Some(conn.clone()),
                RemuxCommand::TabRenameById {
                    session: session.clone(),
                    tab_id: *tab_id,
                    name,
                },
            ),
        }
    }
}

fn on_pane(pane_id: PaneId, command: RemuxCommand) -> RemuxCommand {
    RemuxCommand::OnPane {
        pane_id,
        command: Box::new(command),
    }
}

fn on_tab(tab_id: u64, command: RemuxCommand) -> RemuxCommand {
    RemuxCommand::OnTab {
        tab_id,
        command: Box::new(command),
    }
}

/// One row of the menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub label: String,
    /// The row's current keybinding, from the live config.
    pub hint: Option<String>,
    /// A disabled row is drawn dim and cannot be chosen.
    pub enabled: bool,
    /// Whether choosing the row first asks for confirmation, in the same menu.
    pub confirm: bool,
    pub action: MenuAction,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, action: MenuAction) -> Self {
        Self {
            label: label.into(),
            hint: None,
            enabled: true,
            confirm: false,
            action,
        }
    }

    pub fn hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn confirm(mut self) -> Self {
        self.confirm = true;
        self
    }
}

/// What a key or a click did to the menu.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuOutcome {
    /// Nothing changed.
    Ignored,
    /// The highlight or the rows changed; repaint the menu.
    Redraw,
    /// Close the menu without acting.
    Close,
    /// Close the menu and run the action.
    Run(MenuAction),
}

/// An open menu.
#[derive(Debug, Clone)]
pub struct ContextMenu {
    items: Vec<MenuItem>,
    selected: Option<usize>,
    anchor: (u16, u16),
    screen: (u16, u16),
    rect: Rect,
}

const FIRST_LABEL_COLUMN: u16 = 2;

impl ContextMenu {
    /// A menu that opens at `anchor` on a `screen` of `(cols, rows)`, moved so
    /// that it fits. `None` when there is no row to show.
    pub fn open(items: Vec<MenuItem>, anchor: (u16, u16), screen: (u16, u16)) -> Option<Self> {
        if items.is_empty() {
            return None;
        }
        let mut menu = Self {
            selected: None,
            rect: Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            items,
            anchor,
            screen,
        };
        menu.relayout();
        menu.selected = menu.step_from(None, 1);
        Some(menu)
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Whether the cell `(col, row)` is on the menu, its frame included.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        let r = self.rect;
        col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
    }

    /// The row under `(col, row)`, `None` on the frame or off the menu.
    pub fn item_at(&self, col: u16, row: u16) -> Option<usize> {
        let r = self.rect;
        let inside = col > r.x && col + 1 < r.x + r.width && row > r.y && row + 1 < r.y + r.height;
        inside.then(|| usize::from(row - r.y - 1))
    }

    /// Highlight the row under the pointer. A disabled row and the frame keep
    /// the highlight where it was.
    pub fn hover(&mut self, col: u16, row: u16) -> MenuOutcome {
        match self.item_at(col, row) {
            Some(i) if self.items[i].enabled && self.selected != Some(i) => {
                self.selected = Some(i);
                MenuOutcome::Redraw
            }
            _ => MenuOutcome::Ignored,
        }
    }

    /// A left press at `(col, row)`.
    pub fn click(&mut self, col: u16, row: u16) -> MenuOutcome {
        match self.item_at(col, row) {
            Some(i) if self.items[i].enabled => self.activate(i),
            Some(_) => MenuOutcome::Ignored,
            None if self.contains(col, row) => MenuOutcome::Ignored,
            None => MenuOutcome::Close,
        }
    }

    /// A key while the menu is open. `j`/`k` and the arrows move the
    /// highlight, Enter chooses it and Esc closes the menu. Every other key is
    /// swallowed: the menu is modal.
    pub fn key(&mut self, key: KeyEvent) -> MenuOutcome {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => MenuOutcome::Close,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Enter => match self.selected {
                Some(i) => self.activate(i),
                None => MenuOutcome::Ignored,
            },
            _ => MenuOutcome::Ignored,
        }
    }

    fn move_selection(&mut self, delta: i32) -> MenuOutcome {
        match self.step_from(self.selected, delta) {
            Some(i) if self.selected != Some(i) => {
                self.selected = Some(i);
                MenuOutcome::Redraw
            }
            _ => MenuOutcome::Ignored,
        }
    }

    /// The rows that fit on screen. A menu taller than the screen draws only
    /// these, so the highlight must never rest on a row below them.
    fn drawn_rows(&self) -> usize {
        self.items
            .len()
            .min(usize::from(self.rect.height.saturating_sub(2)))
    }

    /// The enabled row after `from` in the direction of `delta`, wrapping.
    fn step_from(&self, from: Option<usize>, delta: i32) -> Option<usize> {
        let n = self.drawn_rows();
        if n == 0 {
            return None;
        }
        let start = match (from, delta > 0) {
            (Some(i), _) => i,
            (None, true) => n - 1,
            (None, false) => 0,
        };
        (1..=n)
            .map(|k| {
                if delta > 0 {
                    (start + k) % n
                } else {
                    (start + n - k) % n
                }
            })
            .find(|&i| self.items[i].enabled)
    }

    fn activate(&mut self, index: usize) -> MenuOutcome {
        let item = self.items[index].clone();
        if item.action == MenuAction::Cancel {
            return MenuOutcome::Close;
        }
        if !item.confirm {
            return MenuOutcome::Run(item.action);
        }
        self.items = vec![
            MenuItem::new(format!("Confirm: {}", item.label), item.action),
            MenuItem::new("Cancel", MenuAction::Cancel),
        ];
        self.relayout();
        self.selected = Some(1);
        MenuOutcome::Redraw
    }

    fn relayout(&mut self) {
        let (cols, rows) = self.screen;
        let widest = self.items.iter().map(row_text_width).max().unwrap_or(0);
        let width = (widest + 2).min(usize::from(cols)) as u16;
        let height = (self.items.len() as u16 + 2).min(rows);
        let x = self.anchor.0.min(cols.saturating_sub(width));
        // A menu that does not fit below the pointer opens above it, so the
        // row that was clicked stays visible.
        let y = if self.anchor.1 + height <= rows {
            self.anchor.1
        } else {
            self.anchor
                .1
                .saturating_sub(height)
                .min(rows.saturating_sub(height))
        };
        self.rect = Rect {
            x,
            y,
            width,
            height,
        };
    }

    /// The menu as draw commands, one per row of the frame.
    pub fn render(&self, theme: &Theme) -> Vec<DrawCommand> {
        let r = self.rect;
        let inner = usize::from(r.width.saturating_sub(2));
        let (fg, bg, key_fg) = (theme.whichkey_fg, theme.whichkey_bg, theme.whichkey_key_fg);
        let mut out = Vec::with_capacity(self.items.len() + 2);
        out.push(DrawCommand {
            x: r.x,
            y: r.y,
            text: format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(inner)),
            fg,
            bg,
        });
        for (i, item) in self
            .items
            .iter()
            .enumerate()
            .take(usize::from(r.height.saturating_sub(2)))
        {
            let selected = self.selected == Some(i);
            let (row_fg, row_bg) = match (selected, item.enabled) {
                (true, _) => (bg, key_fg),
                (false, true) => (fg, bg),
                (false, false) => (crossterm::style::Color::DarkGrey, bg),
            };
            out.push(DrawCommand {
                x: r.x,
                y: r.y + 1 + i as u16,
                text: format!("\u{2502}{}\u{2502}", row_text(item, inner)),
                fg: row_fg,
                bg: row_bg,
            });
        }
        out.push(DrawCommand {
            x: r.x,
            y: r.y + r.height.saturating_sub(1),
            text: format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner)),
            fg,
            bg,
        });
        out
    }
}

/// The rows for a right-click on what the server reported. `hint` maps an
/// action name to its current keybinding.
///
/// Each row applies the precondition of the command it runs. The only command
/// the server refuses is a fullscreen zoom in Monocle, where every pane is
/// already full-area, so that is the only row disabled for a pane. A float zoom
/// of a lone pane is accepted and still useful, so zoom is offered whatever the
/// pane count. Fullscreen zoom is offered as the second row only when the
/// configured mode is fullscreen: there is no command that forces it when the
/// configured mode is float.
pub fn items_for(target: &ContextTarget, hint: &dyn Fn(&str) -> Option<String>) -> Vec<MenuItem> {
    match *target {
        ContextTarget::Pane {
            pane_id,
            zoomed,
            zoom_mode,
            monocle,
        } => {
            let row = |label: &str, action: &str, command: RemuxCommand| {
                MenuItem::new(label, MenuAction::Command(on_pane(pane_id, command)))
                    .hint(hint(action))
            };
            let mut items = Vec::new();
            if zoomed {
                items.push(row(
                    "Unzoom",
                    "PaneToggleZoom",
                    RemuxCommand::PaneToggleZoom,
                ));
            } else {
                let fullscreen_refused = monocle && zoom_mode == ZoomMode::Fullscreen;
                let label = match zoom_mode {
                    ZoomMode::Fullscreen => "Fullscreen zoom",
                    ZoomMode::Float => "Float zoom",
                };
                items.push(
                    row(label, "PaneToggleZoom", RemuxCommand::PaneToggleZoom)
                        .enabled(!fullscreen_refused),
                );
                if zoom_mode == ZoomMode::Fullscreen {
                    items.push(row(
                        "Float zoom",
                        "PaneToggleZoomFloat",
                        RemuxCommand::PaneToggleZoomFloat,
                    ));
                }
            }
            items.push(row(
                "Split right",
                "PaneSplitVertical",
                RemuxCommand::PaneSplitVertical,
            ));
            items.push(row(
                "Split down",
                "PaneSplitHorizontal",
                RemuxCommand::PaneSplitHorizontal,
            ));
            items.push(
                MenuItem::new("Rename", MenuAction::Rename(RenameFor::Pane(pane_id)))
                    .hint(hint("PaneRename")),
            );
            items.push(row("Close", "PaneClose", RemuxCommand::PaneClose));
            items
        }
        ContextTarget::Tab {
            tab_id,
            index,
            count,
        } => {
            let row = |label: &str, command: RemuxCommand| {
                MenuItem::new(label, MenuAction::Command(on_tab(tab_id, command)))
            };
            vec![
                MenuItem::new("Rename", MenuAction::Rename(RenameFor::Tab(tab_id)))
                    .hint(hint("TabRename")),
                row("Close", RemuxCommand::TabClose)
                    .hint(hint("TabClose"))
                    .confirm(),
                row("Move left", RemuxCommand::TabMoveBy(-1)).enabled(index > 0),
                row("Move right", RemuxCommand::TabMoveBy(1)).enabled(index + 1 < count),
            ]
        }
    }
}

fn row_text_width(item: &MenuItem) -> usize {
    let label = item.label.chars().count();
    let hint = item.hint.as_ref().map_or(0, |h| h.chars().count() + 2);
    usize::from(FIRST_LABEL_COLUMN) + label + hint + 1
}

/// A row padded to `inner` cells: the label on the left, the hint right-aligned.
fn row_text(item: &MenuItem, inner: usize) -> String {
    let lead = " ".repeat(usize::from(FIRST_LABEL_COLUMN) - 1);
    let hint = item.hint.as_deref().unwrap_or("");
    let used = lead.chars().count() + item.label.chars().count() + hint.chars().count();
    let gap = inner
        .saturating_sub(used + 1)
        .max(usize::from(!hint.is_empty()));
    let text = format!("{lead}{}{}{hint} ", item.label, " ".repeat(gap));
    text.chars()
        .take(inner)
        .chain(std::iter::repeat(' '))
        .take(inner)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn cmd(name: &str, c: RemuxCommand) -> MenuItem {
        MenuItem::new(name, MenuAction::Command(c))
    }

    fn three() -> Vec<MenuItem> {
        vec![
            cmd("Zoom", RemuxCommand::PaneToggleZoom),
            cmd("Split right", RemuxCommand::PaneSplitVertical).hint(Some("Alt-v".into())),
            cmd("Close", RemuxCommand::PaneClose),
        ]
    }

    fn pane(zoomed: bool, zoom_mode: ZoomMode, monocle: bool) -> ContextTarget {
        ContextTarget::Pane {
            pane_id: 7,
            zoomed,
            zoom_mode,
            monocle,
        }
    }

    fn no_hints(_: &str) -> Option<String> {
        None
    }

    fn labels(items: &[MenuItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn a_pane_menu_offers_both_zooms_split_rename_and_close() {
        let items = items_for(&pane(false, ZoomMode::Fullscreen, false), &no_hints);
        assert_eq!(
            labels(&items),
            [
                "Fullscreen zoom",
                "Float zoom",
                "Split right",
                "Split down",
                "Rename",
                "Close"
            ]
        );
        assert_eq!(
            items[1].action,
            MenuAction::Command(on_pane(7, RemuxCommand::PaneToggleZoomFloat))
        );
        assert_eq!(
            items[2].action,
            MenuAction::Command(on_pane(7, RemuxCommand::PaneSplitVertical))
        );
        assert_eq!(
            items[3].action,
            MenuAction::Command(on_pane(7, RemuxCommand::PaneSplitHorizontal))
        );
        assert!(items.iter().all(|i| i.enabled));
    }

    #[test]
    fn the_first_zoom_row_follows_the_configured_mode() {
        let items = items_for(&pane(false, ZoomMode::Float, false), &no_hints);
        assert_eq!(items[0].label, "Float zoom");
        assert_eq!(
            items[0].action,
            MenuAction::Command(on_pane(7, RemuxCommand::PaneToggleZoom))
        );
        assert_eq!(
            labels(&items).iter().filter(|l| l.contains("zoom")).count(),
            1,
            "no command forces a fullscreen zoom in float mode"
        );
    }

    #[test]
    fn a_zoomed_pane_offers_unzoom_instead_of_zoom() {
        let items = items_for(&pane(true, ZoomMode::Float, false), &no_hints);
        assert_eq!(items[0].label, "Unzoom");
        assert_eq!(
            items[0].action,
            MenuAction::Command(on_pane(7, RemuxCommand::PaneToggleZoom))
        );
        assert_eq!(items.len(), 5, "Unzoom replaces the zoom rows");
    }

    #[test]
    fn fullscreen_zoom_is_disabled_where_the_server_refuses_it() {
        let items = items_for(&pane(false, ZoomMode::Fullscreen, true), &no_hints);
        assert!(!items[0].enabled);
        assert!(items[1].enabled, "a float zoom is accepted in Monocle");
        let unzoom = items_for(&pane(true, ZoomMode::Fullscreen, true), &no_hints);
        assert!(unzoom[0].enabled, "releasing a zoom always works");
    }

    #[test]
    fn rows_carry_the_live_keybinding() {
        let hint = |a: &str| (a == "PaneClose").then(|| "Ctrl-a p x".to_string());
        let items = items_for(&pane(false, ZoomMode::Fullscreen, false), &hint);
        let close = items.iter().find(|i| i.label == "Close").unwrap();
        assert_eq!(close.hint.as_deref(), Some("Ctrl-a p x"));
        assert!(items.iter().filter(|i| i.hint.is_some()).count() == 1);
    }

    #[test]
    fn a_tab_menu_names_its_tab_and_disables_a_move_past_either_end() {
        let tab = |index, count| ContextTarget::Tab {
            tab_id: 42,
            index,
            count,
        };
        let first = items_for(&tab(0, 3), &no_hints);
        assert_eq!(
            labels(&first),
            ["Rename", "Close", "Move left", "Move right"]
        );
        assert_eq!(first[0].action, MenuAction::Rename(RenameFor::Tab(42)));
        assert_eq!(
            first[1].action,
            MenuAction::Command(on_tab(42, RemuxCommand::TabClose))
        );
        assert!(first[1].confirm, "closing a tab asks first");
        assert!(!first[2].enabled && first[3].enabled);
        assert_eq!(
            first[3].action,
            MenuAction::Command(on_tab(42, RemuxCommand::TabMoveBy(1)))
        );
        let last = items_for(&tab(2, 3), &no_hints);
        assert!(last[2].enabled && !last[3].enabled);
        assert_eq!(
            last[2].action,
            MenuAction::Command(on_tab(42, RemuxCommand::TabMoveBy(-1)))
        );
        let only = items_for(&tab(0, 1), &no_hints);
        assert!(!only[2].enabled && !only[3].enabled);
    }

    #[test]
    fn a_rename_applies_to_the_entity_it_was_opened_on() {
        assert_eq!(
            RenameFor::Pane(9).command("x".into()),
            (None, on_pane(9, RemuxCommand::PaneRename("x".into())))
        );
        assert_eq!(
            RenameFor::Tab(4).command("x".into()),
            (None, on_tab(4, RemuxCommand::TabRename("x".into())))
        );
        assert_eq!(
            RenameFor::SessionTab {
                conn: ConnId::Local,
                session: "s".into(),
                tab_id: 4
            }
            .command("x".into()),
            (
                Some(ConnId::Local),
                RemuxCommand::TabRenameById {
                    session: "s".into(),
                    tab_id: 4,
                    name: "x".into()
                }
            )
        );
        assert_eq!(
            RenameFor::Session {
                conn: ConnId::Local,
                name: "old".into()
            }
            .command("new".into()),
            (
                Some(ConnId::Local),
                RemuxCommand::SessionRenameByName {
                    old: "old".into(),
                    new: "new".into()
                }
            )
        );
    }

    #[test]
    fn a_menu_taller_than_the_screen_never_highlights_a_row_it_does_not_draw() {
        let items: Vec<MenuItem> = (0..10)
            .map(|i| cmd(&format!("row {i}"), RemuxCommand::PaneClose))
            .collect();
        let mut m = ContextMenu::open(items, (0, 0), (40, 5)).unwrap();
        assert_eq!(m.rect().height, 5);
        for _ in 0..12 {
            m.key(key(KeyCode::Char('j')));
            assert!(m.selected().unwrap() < 3, "{:?}", m.selected());
        }
        m.key(key(KeyCode::Char('k')));
        assert!(m.selected().unwrap() < 3);
    }

    #[test]
    fn an_empty_menu_does_not_open() {
        assert!(ContextMenu::open(Vec::new(), (3, 3), (80, 24)).is_none());
    }

    #[test]
    fn opens_at_the_pointer_when_it_fits() {
        let m = ContextMenu::open(three(), (10, 5), (80, 24)).unwrap();
        assert_eq!((m.rect().x, m.rect().y), (10, 5));
        assert_eq!(m.rect().height, 5);
    }

    #[test]
    fn is_clamped_to_stay_on_screen() {
        let m = ContextMenu::open(three(), (79, 23), (80, 24)).unwrap();
        let r = m.rect();
        assert!(r.x + r.width <= 80 && r.y + r.height <= 24, "{r:?}");
        assert!(r.x < 79 && r.y < 23);
    }

    #[test]
    fn opens_above_the_pointer_when_it_does_not_fit_below() {
        let m = ContextMenu::open(three(), (10, 22), (80, 24)).unwrap();
        let r = m.rect();
        assert_eq!(r.y + r.height, 22, "the menu ends where the pointer is");
        assert_eq!(r.x, 10);
    }

    #[test]
    fn highlight_starts_on_the_first_enabled_row() {
        let mut items = three();
        items[0].enabled = false;
        let m = ContextMenu::open(items, (0, 0), (80, 24)).unwrap();
        assert_eq!(m.selected(), Some(1));
    }

    #[test]
    fn j_k_and_arrows_move_and_skip_disabled_rows() {
        let mut items = three();
        items[1].enabled = false;
        let mut m = ContextMenu::open(items, (0, 0), (80, 24)).unwrap();
        assert_eq!(m.selected(), Some(0));
        assert_eq!(m.key(key(KeyCode::Char('j'))), MenuOutcome::Redraw);
        assert_eq!(m.selected(), Some(2));
        assert_eq!(m.key(key(KeyCode::Down)), MenuOutcome::Redraw);
        assert_eq!(m.selected(), Some(0), "wraps past the end");
        assert_eq!(m.key(key(KeyCode::Char('k'))), MenuOutcome::Redraw);
        assert_eq!(m.selected(), Some(2));
        assert_eq!(m.key(key(KeyCode::Up)), MenuOutcome::Redraw);
        assert_eq!(m.selected(), Some(0));
    }

    #[test]
    fn enter_runs_the_highlighted_row_and_esc_closes() {
        let mut m = ContextMenu::open(three(), (0, 0), (80, 24)).unwrap();
        m.key(key(KeyCode::Char('j')));
        assert_eq!(
            m.key(key(KeyCode::Enter)),
            MenuOutcome::Run(MenuAction::Command(RemuxCommand::PaneSplitVertical))
        );
        assert_eq!(m.key(key(KeyCode::Esc)), MenuOutcome::Close);
    }

    #[test]
    fn other_keys_are_swallowed() {
        let mut m = ContextMenu::open(three(), (0, 0), (80, 24)).unwrap();
        assert_eq!(m.key(key(KeyCode::Char('x'))), MenuOutcome::Ignored);
    }

    #[test]
    fn hover_moves_the_highlight_to_the_row_under_the_pointer() {
        let mut m = ContextMenu::open(three(), (10, 5), (80, 24)).unwrap();
        assert_eq!(m.hover(12, 7), MenuOutcome::Redraw);
        assert_eq!(m.selected(), Some(1));
        assert_eq!(
            m.hover(12, 7),
            MenuOutcome::Ignored,
            "no change, no repaint"
        );
        assert_eq!(m.hover(10, 7), MenuOutcome::Ignored, "the frame");
    }

    #[test]
    fn a_click_on_a_row_runs_it_and_on_a_disabled_row_does_nothing() {
        let mut items = three();
        items[2].enabled = false;
        let mut m = ContextMenu::open(items, (10, 5), (80, 24)).unwrap();
        assert_eq!(
            m.click(12, 6),
            MenuOutcome::Run(MenuAction::Command(RemuxCommand::PaneToggleZoom))
        );
        assert_eq!(m.click(12, 8), MenuOutcome::Ignored);
    }

    #[test]
    fn a_click_outside_closes() {
        let mut m = ContextMenu::open(three(), (10, 5), (80, 24)).unwrap();
        assert_eq!(m.click(0, 0), MenuOutcome::Close);
        assert_eq!(
            m.click(10, 5),
            MenuOutcome::Ignored,
            "the frame belongs to the menu"
        );
    }

    #[test]
    fn a_confirming_row_asks_first_and_cancel_is_the_default() {
        let items = vec![cmd("Close", RemuxCommand::PaneClose).confirm()];
        let mut m = ContextMenu::open(items, (0, 0), (80, 24)).unwrap();
        assert_eq!(m.key(key(KeyCode::Enter)), MenuOutcome::Redraw);
        assert_eq!(m.items().len(), 2);
        assert_eq!(m.selected(), Some(1));
        let mut cancelled = m.clone();
        assert_eq!(cancelled.key(key(KeyCode::Enter)), MenuOutcome::Close);
        m.key(key(KeyCode::Char('k')));
        assert_eq!(
            m.key(key(KeyCode::Enter)),
            MenuOutcome::Run(MenuAction::Command(RemuxCommand::PaneClose))
        );
    }

    #[test]
    fn rows_show_the_hint_right_aligned_inside_the_frame() {
        let m = ContextMenu::open(three(), (0, 0), (80, 24)).unwrap();
        let cmds = m.render(&Theme::default());
        assert_eq!(cmds.len(), 5);
        let widths: Vec<usize> = cmds.iter().map(|c| c.text.chars().count()).collect();
        assert!(
            widths.iter().all(|&w| w == usize::from(m.rect().width)),
            "{widths:?}"
        );
        assert!(cmds[2].text.contains("Split right") && cmds[2].text.contains("Alt-v \u{2502}"));
    }
}
