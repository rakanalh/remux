//! The `files` plugin: a real file manager (`yazi`, `nnn`, `ranger`, …) hosted
//! in a sidebar panel, showing the focused pane's directory.
//!
//! A file manager is a full-screen TUI, so it needs a PTY, and the server owns
//! every PTY. The panel is therefore backed by a server-spawned **auxiliary
//! pane** -- a pane in no layout tree -- streamed back over exactly the
//! machinery a View cell uses: `SpawnAuxPane` → `SubscribePane` → `PaneContent`,
//! with keys routed by `InputToPane`. None of that addressing appears here: the
//! panel says what it wants through [`PluginRequest`] and the client resolves
//! the connection and the pane id (see [`PluginRequest`]'s own note).
//!
//! What is left in this file is a state machine and a header row. The content is
//! painted by [`blit_snapshot`], the SAME function that paints a View cell,
//! because a file-manager panel is a pane snapshot in a rect and nothing more.

use crossterm::event::{KeyCode, KeyEvent, MouseEventKind};

use super::{blank_grid, draw_text, PluginAction, PluginEvent, PluginRequest, SidebarPlugin};
use crate::client::view::{blit_snapshot, draw_centered, PaneSnapshot};
use crate::config::theme::CompositorTheme;
use crate::protocol::RenderCell;

/// Rows the panel keeps for its own header (the directory being shown).
const HEADER_ROWS: u16 = 1;

/// The key that restarts an exited file manager.
const RESTART_KEY: char = 'r';

/// Where the panel's aux pane is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuxState {
    /// No pane, and none asked for. The panel spawns out of this state as soon
    /// as it is laid out at a usable size, so a hidden panel costs nothing.
    Idle,
    /// A `Spawn` is out; waiting for [`PluginEvent::AuxPaneReady`].
    Spawning,
    /// The pane exists. It may not have produced a snapshot yet.
    Live,
    /// The program exited. Deliberately NOT auto-respawned: a file manager that
    /// dies instantly (a bad `command`, a missing binary) would otherwise spin
    /// forever. The user restarts it with [`RESTART_KEY`].
    Exited,
}

pub struct FilesPlugin {
    /// The program to run. Required by config; there is no default.
    command: String,
    state: AuxState,
    /// The directory the CURRENT pane was spawned in. Compared against
    /// [`FilesPlugin::target_cwd`] to decide whether a focus change actually
    /// moved directory -- without it, every focus move between two panes in the
    /// same directory would kill and respawn the file manager and throw away the
    /// user's navigation.
    spawned_cwd: Option<String>,
    /// The directory the focused pane is in, as last reported.
    target_cwd: Option<String>,
    /// Whether a [`PluginEvent::FocusedCwd`] has arrived at all.
    ///
    /// The first spawn waits for it. The panel is laid out before the first
    /// session tree comes back, so spawning on the first rect would start the
    /// program in the wrong directory and then immediately kill and respawn it
    /// -- a visible flash and a pointless PTY. `None` is a legitimate answer
    /// (an unreadable cwd), which is why this is a separate flag rather than
    /// `target_cwd.is_some()`.
    cwd_known: bool,
    /// The last size the chrome laid this panel out at, and the size the aux
    /// pane is currently subscribed at. They differ for exactly one pass after a
    /// resize, which is what triggers the re-subscribe.
    size: (u16, u16),
    subscribed_size: Option<(u16, u16)>,
    snapshot: Option<PaneSnapshot>,
    pending: Vec<PluginRequest>,
}

impl FilesPlugin {
    pub fn new(command: String) -> Self {
        Self {
            command,
            state: AuxState::Idle,
            spawned_cwd: None,
            target_cwd: None,
            cwd_known: false,
            size: (0, 0),
            subscribed_size: None,
            snapshot: None,
            pending: Vec::new(),
        }
    }

    /// The `(cols, rows)` the aux pane should be, given the panel's size: the
    /// panel minus its header row.
    fn content_size(&self) -> (u16, u16) {
        (self.size.0, self.size.1.saturating_sub(HEADER_ROWS))
    }

    /// Ask for a pane at the current target directory.
    fn spawn(&mut self) {
        let (cols, rows) = self.content_size();
        if cols == 0 || rows == 0 {
            return;
        }
        self.snapshot = None;
        self.subscribed_size = None;
        self.spawned_cwd = self.target_cwd.clone();
        self.state = AuxState::Spawning;
        self.pending.push(PluginRequest::Spawn {
            cols,
            rows,
            command: self.command.clone(),
            cwd: self.target_cwd.clone(),
        });
    }
}

impl SidebarPlugin for FilesPlugin {
    fn title(&self) -> &str {
        "Files"
    }

    fn min_size(&self) -> (u16, u16) {
        // One header row plus enough content for a file manager to draw
        // something recognisable.
        (12, 4)
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

        // The header tracks focus with the SAME theme roles the panel frame
        // does, exactly as every other panel's does.
        let header_fg = crate::server::compositor::border_fg(theme, focused);
        let header = match &self.spawned_cwd {
            Some(cwd) => shorten_path(cwd, cols as usize),
            None => self.title().to_string(),
        };
        draw_text(&mut grid, 0, 0, &header, header_fg, bg);

        let ih = rows.saturating_sub(HEADER_ROWS) as usize;
        if ih == 0 {
            return grid;
        }
        let iy = HEADER_ROWS as usize;
        let iw = cols as usize;
        match (&self.snapshot, self.state) {
            (_, AuxState::Exited) => {
                draw_centered(
                    &mut grid,
                    0,
                    iy,
                    iw,
                    ih,
                    &format!("exited — {RESTART_KEY} to restart"),
                );
            }
            (Some(snap), _) => blit_snapshot(&mut grid, 0, iy, iw, ih, snap),
            (None, _) => draw_centered(
                &mut grid,
                0,
                iy,
                iw,
                ih,
                &format!("starting {}…", self.command),
            ),
        }
        grid
    }

    fn on_key(&mut self, key: KeyEvent) -> PluginAction {
        if self.state == AuxState::Exited {
            if key.code == KeyCode::Char(RESTART_KEY) {
                self.spawn();
                return PluginAction::Redraw;
            }
            return PluginAction::None;
        }
        if self.state != AuxState::Live {
            return PluginAction::None;
        }
        // Encoded by the ONE key encoder the client has, with this pane's own
        // DECCKM state -- the same rule a focused View cell follows, so arrows
        // and navigation keys reach the file manager encoded the way it asked
        // for them rather than the way the foreground session did.
        let ack = self
            .snapshot
            .as_ref()
            .map(|s| s.application_cursor_keys)
            .unwrap_or(false);
        match crate::client::input::key_event_to_bytes(&key, ack) {
            Some(data) => {
                self.pending.push(PluginRequest::Input { data });
                // The repaint comes from the `PaneContent` the keystroke
                // provokes, not from here: nothing local changed.
                PluginAction::None
            }
            None => PluginAction::None,
        }
    }

    fn on_mouse(&mut self, _x: u16, _y: u16, _kind: MouseEventKind) -> PluginAction {
        // Deliberately inert. Forwarding a click means translating panel
        // coordinates into the pane's and speaking its mouse-tracking mode; the
        // file managers this hosts are all keyboard-driven, so the plumbing
        // would be untested weight.
        PluginAction::None
    }

    fn on_event(&mut self, ev: &PluginEvent) {
        match ev {
            PluginEvent::FocusedCwd { cwd } => {
                let first = !self.cwd_known;
                self.cwd_known = true;
                self.target_cwd = cwd.clone();
                if first {
                    // The panel was waiting for this to make its first spawn;
                    // `on_size` picks it up on the next pass.
                    return;
                }
                // Re-target only when the directory actually changed. Focus
                // moves between panes far more often than it moves between
                // directories, and a respawn costs the user their place.
                if self.state != AuxState::Idle && *cwd != self.spawned_cwd {
                    self.pending.push(PluginRequest::Kill);
                    self.state = AuxState::Idle;
                    self.snapshot = None;
                    self.subscribed_size = None;
                }
            }
            PluginEvent::AuxPaneReady => {
                self.state = AuxState::Live;
                // Subscribe at whatever size the panel is NOW: the panel may
                // have been resized between the request and the answer.
                let (cols, rows) = self.content_size();
                if cols > 0 && rows > 0 {
                    self.subscribed_size = Some((cols, rows));
                    self.pending.push(PluginRequest::Subscribe { cols, rows });
                }
            }
            PluginEvent::AuxPaneContent { snapshot } => {
                self.snapshot = Some((**snapshot).clone());
            }
            PluginEvent::AuxPaneExited => {
                self.state = AuxState::Exited;
                self.snapshot = None;
                self.subscribed_size = None;
            }
            PluginEvent::SessionTree { .. } | PluginEvent::ConnectionLost { .. } => {}
        }
    }

    fn on_size(&mut self, cols: u16, rows: u16) {
        self.size = (cols, rows);
        let (ccols, crows) = self.content_size();
        if ccols == 0 || crows == 0 {
            return;
        }
        match self.state {
            // Lazy spawn: the first pass that gives this panel a usable rect
            // AND a directory to open in.
            AuxState::Idle if self.cwd_known => self.spawn(),
            // Re-subscribe at the new size, exactly as a View cell does, so the
            // pane reflows to the panel through the server's existing
            // min-across-viewers sizing rather than a second policy.
            AuxState::Live if self.subscribed_size != Some((ccols, crows)) => {
                self.subscribed_size = Some((ccols, crows));
                self.pending.push(PluginRequest::Subscribe {
                    cols: ccols,
                    rows: crows,
                });
            }
            _ => {}
        }
    }

    fn take_requests(&mut self) -> Vec<PluginRequest> {
        std::mem::take(&mut self.pending)
    }

    /// Yes -- indirectly. This panel never looks at a `SessionTree`, but
    /// [`PluginEvent::FocusedCwd`] is derived from one, so a client whose only
    /// panel is this one must still subscribe or the directory it exists to
    /// follow would never arrive.
    fn wants_session_tree(&self) -> bool {
        true
    }
}

/// Fit a path into `width` columns, keeping the END of it -- the leaf directory
/// is what identifies where you are, and it is the part a left-truncating
/// header would throw away first.
fn shorten_path(path: &str, width: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= width || width == 0 {
        return path.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let tail: String = chars[chars.len() - (width - 1)..].iter().collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::sidebar::make_plugin;
    use crate::config::sidebar::PanelConfig;
    use crate::protocol::RenderCell;

    fn cfg(plugin: &str, command: Option<&str>) -> PanelConfig {
        PanelConfig {
            plugin: plugin.to_string(),
            weight: 1,
            command: command.map(str::to_string),
        }
    }

    fn snapshot(rows: &[&str], cols: u16) -> PaneSnapshot {
        let cells: Vec<Vec<RenderCell>> = rows
            .iter()
            .map(|r| {
                let mut row: Vec<RenderCell> = r
                    .chars()
                    .map(|c| RenderCell {
                        c,
                        ..RenderCell::default()
                    })
                    .collect();
                row.resize(cols as usize, RenderCell::default());
                row
            })
            .collect();
        PaneSnapshot {
            cols,
            rows: rows.len() as u16,
            cells,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: false,
            application_cursor_keys: false,
            session_visible: false,
        }
    }

    fn text(grid: &[Vec<RenderCell>]) -> Vec<String> {
        grid.iter()
            .map(|r| {
                r.iter()
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_files_panel_without_a_command_is_refused() {
        assert!(make_plugin(&cfg("files", None)).is_none());
        assert!(make_plugin(&cfg("files", Some("yazi"))).is_some());
    }

    #[test]
    fn a_command_on_another_plugin_is_ignored_not_rejected() {
        assert!(make_plugin(&cfg("sessions", Some("yazi"))).is_some());
    }

    #[test]
    fn nothing_is_spawned_before_a_directory_is_known() {
        let mut p = FilesPlugin::new("yazi".into());
        p.on_size(20, 6);
        assert!(
            p.take_requests().is_empty(),
            "spawning before the first FocusedCwd would open the wrong directory              and then immediately respawn"
        );
        p.on_event(&PluginEvent::FocusedCwd { cwd: None });
        p.on_size(20, 6);
        assert!(matches!(
            p.take_requests().as_slice(),
            [PluginRequest::Spawn { .. }]
        ));
    }

    #[test]
    fn the_first_layout_asks_for_a_pane_at_the_focused_cwd() {
        let mut p = FilesPlugin::new("yazi".into());
        p.on_event(&PluginEvent::FocusedCwd {
            cwd: Some("/tmp/here".into()),
        });
        assert!(p.take_requests().is_empty(), "no rect yet, no pane");
        p.on_size(20, 6);
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::Spawn {
                cols: 20,
                rows: 5, // the header row is not the pane's
                command: "yazi".into(),
                cwd: Some("/tmp/here".into()),
            }]
        );
        // Still only one request: a laid-out panel must not re-ask every pass.
        p.on_size(20, 6);
        assert!(p.take_requests().is_empty());
    }

    #[test]
    fn a_hidden_panel_asks_for_nothing() {
        let mut p = FilesPlugin::new("yazi".into());
        p.on_event(&PluginEvent::FocusedCwd { cwd: None });
        p.on_size(0, 0);
        p.on_size(20, 1); // header only, no content rows
        assert!(p.take_requests().is_empty());
    }

    #[test]
    fn a_resize_resubscribes_at_the_new_size() {
        let mut p = FilesPlugin::new("yazi".into());
        p.on_event(&PluginEvent::FocusedCwd { cwd: None });
        p.on_size(20, 6);
        p.take_requests();
        p.on_event(&PluginEvent::AuxPaneReady);
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::Subscribe { cols: 20, rows: 5 }]
        );
        p.on_size(20, 6);
        assert!(p.take_requests().is_empty(), "same size, no re-subscribe");
        p.on_size(30, 9);
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::Subscribe { cols: 30, rows: 8 }]
        );
    }

    #[test]
    fn the_same_directory_does_not_respawn() {
        let mut p = FilesPlugin::new("yazi".into());
        p.on_event(&PluginEvent::FocusedCwd {
            cwd: Some("/a".into()),
        });
        p.on_size(20, 6);
        p.take_requests();
        p.on_event(&PluginEvent::AuxPaneReady);
        p.take_requests();
        // Focus moved to another pane in the SAME directory.
        p.on_event(&PluginEvent::FocusedCwd {
            cwd: Some("/a".into()),
        });
        p.on_size(20, 6);
        assert!(
            p.take_requests().is_empty(),
            "a focus move within one directory must not restart the file manager"
        );
    }

    #[test]
    fn a_new_directory_kills_and_respawns() {
        let mut p = FilesPlugin::new("yazi".into());
        p.on_event(&PluginEvent::FocusedCwd {
            cwd: Some("/a".into()),
        });
        p.on_size(20, 6);
        p.take_requests();
        p.on_event(&PluginEvent::AuxPaneReady);
        p.take_requests();
        p.on_event(&PluginEvent::FocusedCwd {
            cwd: Some("/b".into()),
        });
        assert_eq!(p.take_requests(), vec![PluginRequest::Kill]);
        p.on_size(20, 6);
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::Spawn {
                cols: 20,
                rows: 5,
                command: "yazi".into(),
                cwd: Some("/b".into()),
            }]
        );
    }

    #[test]
    fn keys_are_forwarded_only_while_the_pane_is_live() {
        use crossterm::event::{KeyEventKind, KeyModifiers};
        let key = |c: char| KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        let mut p = FilesPlugin::new("yazi".into());
        p.on_event(&PluginEvent::FocusedCwd { cwd: None });
        p.on_key(key('j'));
        assert!(p.take_requests().is_empty(), "no pane, no input");

        p.on_size(20, 6);
        p.take_requests();
        p.on_event(&PluginEvent::AuxPaneReady);
        p.take_requests();
        p.on_key(key('j'));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::Input {
                data: b"j".to_vec()
            }]
        );
    }

    #[test]
    fn an_exited_pane_shows_a_restart_hint_and_restarts_on_the_key() {
        use crossterm::event::{KeyEventKind, KeyModifiers};
        let theme = CompositorTheme::default();
        let mut p = FilesPlugin::new("yazi".into());
        p.on_event(&PluginEvent::FocusedCwd { cwd: None });
        p.on_size(24, 6);
        p.take_requests();
        p.on_event(&PluginEvent::AuxPaneReady);
        p.take_requests();
        p.on_event(&PluginEvent::AuxPaneExited);
        let rendered = text(&p.render(24, 6, true, &theme));
        assert!(
            rendered.iter().any(|r| r.contains("exited")),
            "an exited panel must say so, not sit blank: {rendered:?}"
        );
        // No auto-respawn: a command that dies instantly must not spin.
        p.on_size(24, 6);
        assert!(p.take_requests().is_empty());

        p.on_key(KeyEvent {
            code: KeyCode::Char(RESTART_KEY),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(matches!(
            p.take_requests().as_slice(),
            [PluginRequest::Spawn { .. }]
        ));
    }

    #[test]
    fn the_snapshot_is_painted_below_the_header() {
        let theme = CompositorTheme::default();
        let mut p = FilesPlugin::new("yazi".into());
        p.on_event(&PluginEvent::FocusedCwd {
            cwd: Some("/tmp/proj".into()),
        });
        p.on_size(16, 4);
        p.take_requests();
        p.on_event(&PluginEvent::AuxPaneReady);
        p.on_event(&PluginEvent::AuxPaneContent {
            snapshot: Box::new(snapshot(&["alpha", "beta", "gamma"], 16)),
        });
        let rendered = text(&p.render(16, 4, false, &theme));
        assert_eq!(rendered[0], "/tmp/proj", "the header names the directory");
        assert_eq!(
            &rendered[1..],
            &["alpha", "beta", "gamma"],
            "the pane's content is painted under it"
        );
    }

    #[test]
    fn a_long_path_keeps_its_tail() {
        assert_eq!(shorten_path("/a/b/c", 10), "/a/b/c");
        assert_eq!(shorten_path("/very/long/path/here", 8), "…th/here");
    }

    #[test]
    fn the_panel_needs_the_session_tree_push() {
        // Indirectly: the panel never reads a `SessionTree`, but `FocusedCwd`
        // is derived from one, so a client with only this panel must still
        // subscribe or the directory would never be learned.
        assert!(FilesPlugin::new("yazi".into()).wants_session_tree());
    }
}
