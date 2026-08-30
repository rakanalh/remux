//! The `browser` panel: a minimal file browser with no external configuration.
//!
//! Vim-like navigation of a directory tree, and `Enter` on a file opens it in a
//! **split running `$EDITOR`**. That is the whole feature, and the reason it can
//! exist at all is that this panel is INSIDE the client: it already has a socket
//! to the server, so "open a split" is a message, not a CLI invocation. A hosted
//! `nnn` or `yazi` -- what the [`files`](super::files) panel runs -- cannot do
//! this without an opener hook (`NNN_OPENER`, `rifle.conf`) and a `remux split`
//! subcommand to point it at, which is exactly the external configuration this
//! panel exists to avoid.
//!
//! `files` stays. `browser` is the zero-config default; `files` is the
//! bring-your-own-tool escape hatch.
//!
//! ## Two things are deliberately NOT done here
//!
//! **The listing is not read locally.** [`PluginRequest::ListDirectory`] goes to
//! the server over the wire, because the panel follows the FOCUSED pane's
//! directory and that pane is routinely on a remote. A `std::fs::read_dir` here
//! would list the machine the user is sitting at and look entirely plausible
//! while describing a different filesystem. `files` gets this right for free by
//! running its file manager on the server; this panel has to earn it.
//!
//! **The editor is not resolved here.** The server picks it (`command`, else its
//! own `$EDITOR`, else `vi`), because the editor must exist where the FILE is.
//!
//! ## What is borrowed
//!
//! Navigation, selection, the scrolled window, click hit-testing and the
//! selection that survives a refresh by IDENTITY are all [`super::nav`], the
//! same code the agents panel uses. This panel is the second consumer that
//! extraction was made for; the identity here is the entry NAME, which is what
//! makes a selection survive a file appearing above it.

use crossterm::event::{KeyCode, KeyEvent, MouseEventKind};

use super::nav::{self, Hit, NavKey, NavList, HEADER_ROWS};
use super::{
    blank_grid, draw_text, shorten_path, PluginAction, PluginEvent, PluginRequest, SidebarPlugin,
};
use crate::client::registry::ConnId;
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, DirEntry, RenderCell};

/// The key that toggles hidden entries.
const HIDDEN_KEY: char = '.';

pub struct BrowserPlugin {
    /// The editor override from `[[sidebar.panel]]`'s `command`, or `None` to
    /// let the server choose. Optional, unlike `files`' -- a browser panel that
    /// names no editor is the intended configuration.
    editor: Option<String>,
    /// The connection the panel is showing a directory ON. Half of a listing's
    /// identity: two machines routinely have a `/home/you`, and a panel matching
    /// on the path alone would accept the wrong server's answer.
    conn: Option<ConnId>,
    /// The directory being shown, absolute.
    cwd: Option<String>,
    /// The directory the FOCUSED PANE was last reported to be in.
    ///
    /// The whole of ruling 5 lives in the comparison `cwd == followed`: while it
    /// holds, the panel is still showing the pane's own directory and follows
    /// the focus; once the user has navigated anywhere else it stops, and a
    /// focus change does not yank them back. Losing a user's place in a tree is
    /// the exact cost the `files` panel's dedup exists to avoid, and this is the
    /// same cost one level up.
    followed: Option<String>,
    /// Everything the server sent, unfiltered -- hidden entries included, so the
    /// `.` toggle is instant rather than a round trip.
    entries: Vec<DirEntry>,
    /// The rows actually rendered: [`BrowserPlugin::entries`] after the hidden
    /// filter. Rebuilt whenever either changes.
    rows: Vec<DirEntry>,
    /// Why the current directory could not be listed. Shown, not swallowed.
    error: Option<String>,
    /// Whether the server capped this listing.
    truncated: bool,
    show_hidden: bool,
    /// The `(conn, path)` of the listing request in flight, if any. A
    /// `DirectoryListing` that does not match it belongs to another panel.
    pending: Option<(ConnId, String)>,
    /// The entry name the NEXT listing should land the selection on, if any.
    ///
    /// Set by `h`: the directory being left is where the selection belongs once
    /// the parent's listing arrives, so `h` then `l` is a round trip rather than
    /// a jump to the top of the parent. It is a separate field rather than a
    /// pre-seeded row because a row would be PAINTED -- a fabricated entry sitting
    /// in a directory whose real contents have not arrived yet.
    pending_select: Option<String>,
    nav: NavList,
    requests: Vec<PluginRequest>,
}

impl BrowserPlugin {
    pub fn new(editor: Option<String>) -> Self {
        Self {
            editor,
            conn: None,
            cwd: None,
            followed: None,
            entries: Vec::new(),
            rows: Vec::new(),
            error: None,
            truncated: false,
            show_hidden: false,
            pending: None,
            pending_select: None,
            nav: NavList::new(),
            requests: Vec::new(),
        }
    }

    /// Rebuild the rendered rows, keeping the selection on the entry it was on.
    ///
    /// By NAME, which is the entry's identity within one directory. An index
    /// would move the selection onto a different file whenever one above it was
    /// created or removed -- and here that means `Enter` opening a file the user
    /// did not choose, in an editor.
    fn rebuild(&mut self) {
        let previous = self
            .pending_select
            .take()
            .or_else(|| self.rows.get(self.nav.selected()).map(|e| e.name.clone()));
        self.rows = self
            .entries
            .iter()
            .filter(|e| self.show_hidden || !e.name.starts_with('.'))
            .cloned()
            .collect();
        let keys: Vec<String> = self.rows.iter().map(|e| e.name.clone()).collect();
        self.nav.reselect(&keys, previous.as_ref());
    }

    /// Show `path`, asking the server for its contents.
    ///
    /// The old contents are dropped rather than left on screen under a new
    /// header: a listing takes a round trip, and over a remote at 200ms RTT that
    /// is long enough for a user to press `Enter` on a row belonging to the
    /// directory they just left.
    fn open_dir(&mut self, path: String) {
        self.cwd = Some(path.clone());
        self.entries.clear();
        self.rows.clear();
        self.error = None;
        self.truncated = false;
        self.pending_select = None;
        self.nav.set_selected(0);
        let Some(conn) = self.conn.clone() else {
            return;
        };
        self.pending = Some((conn, path.clone()));
        self.requests.push(PluginRequest::ListDirectory { path });
    }

    /// The absolute path of `name` inside the current directory.
    fn child_path(&self, name: &str) -> Option<String> {
        let cwd = self.cwd.as_deref()?;
        Some(if cwd.ends_with('/') {
            format!("{cwd}{name}")
        } else {
            format!("{cwd}/{name}")
        })
    }

    /// Go up one level, if there is one.
    ///
    /// Lexical, not `canonicalize`: the path is on the SERVER, so there is
    /// nothing here to resolve it against. It also matches what a shell's `cd
    /// ..` does through a symlinked directory, which is the behaviour a user of
    /// this panel already has in the pane beside it.
    fn go_up(&mut self) -> PluginAction {
        let Some(parent) = self.cwd.as_deref().and_then(parent_of) else {
            return PluginAction::None;
        };
        // The directory being left is what the selection should land on once the
        // parent's listing arrives, so `h` then `l` is a round trip rather than
        // a jump to the parent's first entry.
        let leaving = self
            .cwd
            .as_deref()
            .map(|c| c.trim_end_matches('/'))
            .and_then(|c| c.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        self.open_dir(parent);
        self.pending_select = leaving;
        PluginAction::Redraw
    }

    /// `Enter`, or a second click: descend into a directory, or open a file.
    fn activate(&mut self) -> PluginAction {
        let Some(entry) = self.rows.get(self.nav.selected()).cloned() else {
            return PluginAction::None;
        };
        let Some(path) = self.child_path(&entry.name) else {
            return PluginAction::None;
        };
        if entry.is_dir {
            self.open_dir(path);
            return PluginAction::Redraw;
        }
        self.requests.push(PluginRequest::OpenInSplit {
            path,
            command: self.editor.clone(),
            vertical: true,
        });
        // Leave the sidebar. The user asked for this file to be OPEN; a split
        // running an editor that their keystrokes cannot reach -- because `j`
        // and `k` are still scrolling this panel -- is the same as not having
        // opened it.
        PluginAction::Leave
    }

    /// The lines drawn below the list: the listing error and the cap notice.
    ///
    /// Explanations, not destinations. They are deliberately not rows, so the
    /// selection can never land on one and `Enter` can never act on one -- the
    /// lesson the agents panel's "needs Linux" note cost two review rounds.
    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(e) = &self.error {
            notes.push(e.clone());
        }
        if self.truncated {
            notes.push("… list truncated".to_string());
        }
        notes
    }

    /// The label for one row: `dir/`, `file`, `link@`, `dirlink/@`.
    fn label(entry: &DirEntry) -> String {
        let mut s = entry.name.clone();
        if entry.is_dir {
            s.push('/');
        }
        if entry.is_symlink {
            s.push('@');
        }
        s
    }
}

impl SidebarPlugin for BrowserPlugin {
    fn title(&self) -> &str {
        "Browser"
    }

    fn min_size(&self) -> (u16, u16) {
        (8, 3)
    }

    /// Yes -- indirectly, exactly as `files` does. This panel never reads a
    /// `SessionTree`, but [`PluginEvent::FocusedCwd`] is derived from one, so a
    /// client whose only panel is this one must still subscribe or the directory
    /// it starts in would never arrive.
    fn wants_session_tree(&self) -> bool {
        true
    }

    fn render(
        &self,
        cols: u16,
        rows: u16,
        focused: bool,
        theme: &CompositorTheme,
    ) -> Vec<Vec<RenderCell>> {
        let bg = theme.frame_bg.clone().unwrap_or(CellColor::Default);
        let mut grid = blank_grid(cols, rows, bg.clone());
        if grid.is_empty() {
            return grid;
        }
        let header = match self.cwd.as_deref() {
            Some(cwd) => shorten_path(cwd, cols as usize),
            None => self.title().to_string(),
        };
        nav::draw_header(&mut grid, &header, focused, theme, &bg);

        // The same budget rule the agents panel arrived at: at most half the
        // rows when there is a list to crowd out, all of them when there is not.
        // An error means there are no entries anyway, so this only ever binds on
        // the truncation notice -- which appears exactly when the list is
        // longest.
        let capacity = (rows as usize).saturating_sub(HEADER_ROWS);
        let all_notes = self.notes();
        let budget = if self.rows.is_empty() {
            capacity
        } else {
            capacity / 2
        };
        let notes: Vec<String> = all_notes.into_iter().take(budget).collect();

        if self.rows.is_empty() && notes.is_empty() {
            // Said out loud. An empty panel and a broken one look identical, and
            // "waiting for the server" and "this directory is empty" are
            // different answers a user is entitled to tell apart.
            let msg = if self.pending.is_some() {
                "loading…"
            } else if self.cwd.is_some() {
                "empty"
            } else {
                "no directory"
            };
            draw_text(
                &mut grid,
                0,
                HEADER_ROWS as u16,
                msg,
                theme.status_bar_fg.clone(),
                bg,
            );
            return grid;
        }

        // Space for the notes is RESERVED, not left over: painting them only
        // where the rows ran out drops them entirely whenever the list fills the
        // panel, which for the truncation notice is every single time. The list
        // gets the shorter height, so its scrolling accounts for the reservation
        // and `nav`'s hit test records the smaller painted window.
        let note_rows = notes.len();
        let list_height = rows.saturating_sub(note_rows as u16);
        let list_capacity = (list_height as usize).saturating_sub(HEADER_ROWS);

        let top = self.nav.top_for(list_height, self.rows.len());
        for i in 0..list_capacity {
            let Some(entry) = self.rows.get(top + i) else {
                break;
            };
            let y = (HEADER_ROWS + i) as u16;
            let selected = top + i == self.nav.selected();
            let (fg, row_bg) = nav::row_colors(theme, focused, selected, &bg);
            if selected {
                nav::fill_row(&mut grid, y, cols, &fg, &row_bg);
            }
            draw_text(&mut grid, 0, y, &Self::label(entry), fg, row_bg);
        }

        let painted = self.rows.len().saturating_sub(top).min(list_capacity);
        for (n, note) in notes.iter().enumerate() {
            let y = (HEADER_ROWS + painted + n) as u16;
            draw_text(
                &mut grid,
                0,
                y,
                note,
                theme.status_bar_fg.clone(),
                bg.clone(),
            );
        }
        grid
    }

    fn on_key(&mut self, key: KeyEvent) -> PluginAction {
        // `h`/`l` before `nav_key`, which has no opinion about either. `l`
        // descends only into a directory: on a file it is not "open", because
        // `Enter` is, and a key that sometimes opens an editor is a key nobody
        // presses confidently.
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => return self.go_up(),
            KeyCode::Char('l') | KeyCode::Right => {
                let is_dir = self
                    .rows
                    .get(self.nav.selected())
                    .map(|e| e.is_dir)
                    .unwrap_or(false);
                return if is_dir {
                    self.activate()
                } else {
                    PluginAction::None
                };
            }
            KeyCode::Char(HIDDEN_KEY) => {
                self.show_hidden = !self.show_hidden;
                // Through `rebuild`, so the selection stays on the entry it was
                // on: hiding the dotfiles above it must not slide it onto
                // another file.
                self.rebuild();
                return PluginAction::Redraw;
            }
            _ => {}
        }
        match nav::nav_key(&key) {
            Some(NavKey::Activate) => self.activate(),
            Some(cmd) => {
                self.nav.apply(cmd, self.rows.len());
                PluginAction::Redraw
            }
            None => PluginAction::None,
        }
    }

    fn on_mouse(&mut self, _x: u16, y: u16, kind: MouseEventKind) -> PluginAction {
        if !nav::is_select_click(kind) {
            return PluginAction::None;
        }
        match self.nav.hit(y, self.rows.len()) {
            Hit::Nothing => PluginAction::None,
            Hit::Select(idx) => {
                self.nav.set_selected(idx);
                PluginAction::Redraw
            }
            Hit::Activate(_) => self.activate(),
        }
    }

    fn on_event(&mut self, ev: &PluginEvent) {
        match ev {
            PluginEvent::FocusedCwd { conn, cwd } => {
                // An unreadable cwd is UNKNOWN, not a move. The client
                // broadcasts on every foreground tree push and one can carry
                // `None` (the focused pane's shell just exited, or the push
                // landed mid-switch); treating that as a directory change would
                // throw the user out of wherever they were.
                let Some(dir) = cwd.as_deref() else {
                    return;
                };
                let conn_changed = self.conn.as_ref() != Some(conn);
                // Read BEFORE `followed` is updated: it is the comparison
                // between where the panel is and where the pane WAS.
                let anchored = self.cwd == self.followed;
                self.followed = Some(dir.to_string());
                // A different machine always re-targets, whatever the user was
                // browsing: the tree on screen belongs to a filesystem the
                // focused pane is no longer on, and going on showing it is not
                // "keeping their place", it is showing them the wrong computer.
                if conn_changed {
                    self.conn = Some(conn.clone());
                } else if !anchored || self.cwd.as_deref() == Some(dir) {
                    return;
                }
                self.open_dir(dir.to_string());
            }
            PluginEvent::DirectoryListing {
                conn,
                path,
                entries,
                error,
                truncated,
            } => {
                // Claim only the answer to the request THIS panel has out. The
                // event is broadcast, so every other browser panel's listing
                // arrives here too.
                if self.pending.as_ref() != Some(&(conn.clone(), path.clone())) {
                    return;
                }
                self.pending = None;
                self.entries = entries.clone();
                self.error = error.clone();
                self.truncated = *truncated;
                self.rebuild();
            }
            PluginEvent::ConnectionLost { conn } => {
                if self.conn.as_ref() == Some(conn) {
                    // Everything on screen described that machine. Keeping it
                    // would leave a browsable-looking tree that answers nothing.
                    self.conn = None;
                    self.cwd = None;
                    self.followed = None;
                    self.entries.clear();
                    self.rows.clear();
                    self.error = None;
                    self.truncated = false;
                    self.pending = None;
                    self.nav.set_selected(0);
                }
            }
            // The session tree is the sessions panel's, the agent list the
            // agents panel's, and the aux-pane events belong to `files`.
            PluginEvent::SessionTree { .. }
            | PluginEvent::Agents { .. }
            | PluginEvent::AuxPaneReady
            | PluginEvent::AuxPaneContent { .. }
            | PluginEvent::AuxPaneExited => {}
        }
    }

    fn take_requests(&mut self) -> Vec<PluginRequest> {
        std::mem::take(&mut self.requests)
    }
}

/// The parent of an absolute path, lexically. `None` at the root.
fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        // `path` was "/" (or ""): there is nowhere above it.
        return None;
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(i) => Some(trimmed[..i].to_string()),
        // A relative path with no separator. Not something the server sends,
        // but answering "nowhere above" beats fabricating one.
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::sidebar::make_plugin;
    use crate::config::sidebar::PanelConfig;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers, MouseButton};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn entry(name: &str, is_dir: bool) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size: 0,
        }
    }

    fn cwd(conn: ConnId, dir: &str) -> PluginEvent {
        PluginEvent::FocusedCwd {
            conn,
            cwd: Some(dir.to_string()),
        }
    }

    fn listing(conn: ConnId, path: &str, entries: Vec<DirEntry>) -> PluginEvent {
        PluginEvent::DirectoryListing {
            conn,
            path: path.to_string(),
            entries,
            error: None,
            truncated: false,
        }
    }

    /// The panel's visible rows, from a real render.
    fn painted(p: &BrowserPlugin, cols: u16, rows: u16) -> Vec<String> {
        let theme = CompositorTheme::default();
        p.render(cols, rows, true, &theme)
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Drive a panel to a listed directory, returning it ready to navigate.
    fn at(dir: &str, entries: Vec<DirEntry>) -> BrowserPlugin {
        let mut p = BrowserPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, dir));
        p.take_requests();
        p.on_event(&listing(ConnId::Local, dir, entries));
        p
    }

    #[test]
    fn a_browser_panel_needs_no_command() {
        let cfg = PanelConfig {
            plugin: "browser".to_string(),
            weight: 1,
            command: None,
        };
        assert!(
            make_plugin(&cfg).is_some(),
            "zero configuration is the whole point of this panel"
        );
    }

    #[test]
    fn the_first_focused_cwd_asks_the_server_for_that_directory() {
        let mut p = BrowserPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/home/me"));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                path: "/home/me".to_string()
            }],
            "the listing must go over the wire, not through a local read_dir"
        );
    }

    #[test]
    fn entries_render_with_directories_marked() {
        let p = at("/w", vec![entry("src", true), entry("main.rs", false)]);
        let rows = painted(&p, 20, 5);
        assert_eq!(rows[0], "/w");
        assert_eq!(rows[1], "src/");
        assert_eq!(rows[2], "main.rs");
    }

    #[test]
    fn a_symlink_is_marked_and_a_symlinked_directory_is_marked_as_both() {
        let mut link = entry("link", false);
        link.is_symlink = true;
        let mut dirlink = entry("dirlink", true);
        dirlink.is_symlink = true;
        let p = at("/w", vec![dirlink, link]);
        let rows = painted(&p, 20, 5);
        assert_eq!(rows[1], "dirlink/@");
        assert_eq!(rows[2], "link@");
    }

    #[test]
    fn hidden_entries_are_hidden_until_the_toggle() {
        let mut p = at("/w", vec![entry(".git", true), entry("src", true)]);
        assert_eq!(painted(&p, 20, 5)[1], "src/");
        p.on_key(key(KeyCode::Char(HIDDEN_KEY)));
        let rows = painted(&p, 20, 5);
        assert_eq!(rows[1], ".git/");
        assert_eq!(rows[2], "src/");
        p.on_key(key(KeyCode::Char(HIDDEN_KEY)));
        assert_eq!(painted(&p, 20, 5)[1], "src/");
    }

    /// Toggling must not move the cursor onto a different file. With the
    /// dotfiles ABOVE the selection, an index-preserving toggle slides the
    /// selection down -- and here the selection decides which file `Enter`
    /// opens in an editor.
    #[test]
    fn the_toggle_keeps_the_selection_on_its_entry() {
        let mut p = at(
            "/w",
            vec![
                entry(".a", false),
                entry(".b", false),
                entry("keep.rs", false),
                entry("other.rs", false),
            ],
        );
        p.on_key(key(KeyCode::Char('j'))); // -> other.rs (rows: keep.rs, other.rs)
        assert_eq!(p.rows[p.nav.selected()].name, "other.rs");
        p.on_key(key(KeyCode::Char(HIDDEN_KEY)));
        assert_eq!(
            p.rows[p.nav.selected()].name,
            "other.rs",
            "two dotfiles appeared above it; an index would have slid the cursor"
        );
    }

    #[test]
    fn enter_on_a_directory_descends_and_asks_for_it() {
        let mut p = at("/w", vec![entry("src", true)]);
        assert_eq!(p.on_key(key(KeyCode::Enter)), PluginAction::Redraw);
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                path: "/w/src".to_string()
            }]
        );
        assert_eq!(painted(&p, 20, 5)[0], "/w/src");
    }

    #[test]
    fn enter_on_a_file_opens_a_split_and_leaves_the_sidebar() {
        let mut p = at("/w", vec![entry("main.rs", false)]);
        assert_eq!(
            p.on_key(key(KeyCode::Enter)),
            PluginAction::Leave,
            "an editor the keyboard cannot reach is not an opened file"
        );
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::OpenInSplit {
                path: "/w/main.rs".to_string(),
                command: None,
                vertical: true,
            }]
        );
    }

    #[test]
    fn a_configured_editor_travels_with_the_request_but_is_not_resolved_here() {
        let mut p = BrowserPlugin::new(Some("hx".to_string()));
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        p.on_event(&listing(ConnId::Local, "/w", vec![entry("f", false)]));
        p.on_key(key(KeyCode::Enter));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::OpenInSplit {
                path: "/w/f".to_string(),
                command: Some("hx".to_string()),
                vertical: true,
            }],
            "the server resolves the editor; the panel only forwards an override"
        );
    }

    #[test]
    fn l_descends_into_a_directory_but_does_nothing_on_a_file() {
        let mut p = at("/w", vec![entry("src", true), entry("f", false)]);
        assert_eq!(p.on_key(key(KeyCode::Char('l'))), PluginAction::Redraw);
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                path: "/w/src".to_string()
            }]
        );

        let mut p = at("/w", vec![entry("f", false)]);
        assert_eq!(p.on_key(key(KeyCode::Char('l'))), PluginAction::None);
        assert!(p.take_requests().is_empty(), "l must not open an editor");
    }

    #[test]
    fn h_goes_up_a_level_and_lands_on_the_directory_it_left() {
        let mut p = at("/home/me/work", vec![entry("f", false)]);
        assert_eq!(p.on_key(key(KeyCode::Char('h'))), PluginAction::Redraw);
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                path: "/home/me".to_string()
            }]
        );
        p.on_event(&listing(
            ConnId::Local,
            "/home/me",
            vec![entry("other", true), entry("work", true)],
        ));
        assert_eq!(
            p.rows[p.nav.selected()].name,
            "work",
            "h then l should be a round trip, not a jump to the top"
        );
    }

    #[test]
    fn h_at_the_root_does_nothing() {
        let mut p = at("/", vec![entry("etc", true)]);
        assert_eq!(p.on_key(key(KeyCode::Char('h'))), PluginAction::None);
        assert!(p.take_requests().is_empty());
    }

    #[test]
    fn a_child_of_the_root_is_not_a_double_slash() {
        let mut p = at("/", vec![entry("etc", true)]);
        p.on_key(key(KeyCode::Enter));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                path: "/etc".to_string()
            }]
        );
    }

    #[test]
    fn the_parent_of_a_path_is_lexical() {
        assert_eq!(parent_of("/a/b/c").as_deref(), Some("/a/b"));
        assert_eq!(parent_of("/a").as_deref(), Some("/"));
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of("/a/b/").as_deref(), Some("/a"));
    }

    #[test]
    fn a_listing_error_is_shown_rather_than_looking_like_an_empty_directory() {
        let mut p = at("/w", vec![entry("secret", true)]);
        p.on_key(key(KeyCode::Enter));
        p.take_requests();
        p.on_event(&PluginEvent::DirectoryListing {
            conn: ConnId::Local,
            path: "/w/secret".to_string(),
            entries: Vec::new(),
            error: Some("permission denied".to_string()),
            truncated: false,
        });
        let rows = painted(&p, 24, 5);
        assert_eq!(rows[1], "permission denied");
        assert_ne!(rows[1], "empty");
    }

    #[test]
    fn a_genuinely_empty_directory_says_empty_and_a_pending_one_says_loading() {
        let mut p = BrowserPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/w"));
        assert_eq!(painted(&p, 20, 4)[1], "loading…");
        p.on_event(&listing(ConnId::Local, "/w", Vec::new()));
        assert_eq!(painted(&p, 20, 4)[1], "empty");
    }

    /// The truncation notice appears exactly when the list is longest, so it
    /// must survive a panel the list already fills -- the trap the agents panel
    /// fell into, where every test had slack and none could fail.
    #[test]
    fn the_truncation_notice_survives_a_panel_the_list_already_fills() {
        let mut p = BrowserPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        p.on_event(&PluginEvent::DirectoryListing {
            conn: ConnId::Local,
            path: "/w".to_string(),
            entries: (0..20).map(|i| entry(&format!("f{i}"), false)).collect(),
            error: None,
            truncated: true,
        });
        // Header + 2 rows + the note: the list alone would fill this and more.
        let rows = painted(&p, 24, 4);
        assert_eq!(
            rows[3], "… list truncated",
            "a full list must not push the notice off the panel, got {rows:?}"
        );
        assert!(
            rows[1].starts_with("f0"),
            "and the rows still paint: {rows:?}"
        );
    }

    /// The notice is an explanation, not a destination.
    #[test]
    fn a_click_on_the_truncation_notice_selects_nothing() {
        let mut p = BrowserPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        p.on_event(&PluginEvent::DirectoryListing {
            conn: ConnId::Local,
            path: "/w".to_string(),
            entries: (0..20).map(|i| entry(&format!("f{i}"), false)).collect(),
            error: None,
            truncated: true,
        });
        let _ = painted(&p, 24, 4);
        let down = MouseEventKind::Down(MouseButton::Left);
        // Rows are painted at y=1..2; the notice is at y=3.
        assert_eq!(p.on_mouse(0, 2, down), PluginAction::Redraw);
        assert_eq!(
            p.on_mouse(0, 3, down),
            PluginAction::None,
            "the notice is not a row, however many rows exist below the fold"
        );
    }

    #[test]
    fn a_click_selects_and_a_second_click_activates() {
        let mut p = at("/w", vec![entry("a", false), entry("b", false)]);
        let _ = painted(&p, 24, 5);
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(p.on_mouse(0, 2, down), PluginAction::Redraw);
        assert_eq!(p.on_mouse(0, 2, down), PluginAction::Leave);
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::OpenInSplit {
                path: "/w/b".to_string(),
                command: None,
                vertical: true,
            }]
        );
        assert_eq!(p.on_mouse(0, 0, down), PluginAction::None, "not the header");
    }

    // -- Ruling 5: following the focused pane, and knowing when to stop -------

    #[test]
    fn a_focus_move_within_one_directory_does_not_disturb_the_panel() {
        let mut p = at("/w", vec![entry("a", false), entry("b", false)]);
        p.on_key(key(KeyCode::Char('j')));
        p.on_event(&cwd(ConnId::Local, "/w"));
        assert!(
            p.take_requests().is_empty(),
            "the same directory is not a move"
        );
        assert_eq!(p.rows[p.nav.selected()].name, "b");
    }

    #[test]
    fn the_panel_follows_the_focused_pane_while_it_is_still_showing_its_directory() {
        let mut p = at("/w", vec![entry("a", false)]);
        p.on_event(&cwd(ConnId::Local, "/elsewhere"));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                path: "/elsewhere".to_string()
            }]
        );
    }

    /// Ruling 5, the one that matters: once the user has navigated away, a
    /// focus change must NOT yank them back.
    #[test]
    fn once_the_user_has_navigated_away_a_focus_change_does_not_yank_them_back() {
        let mut p = at("/w", vec![entry("src", true)]);
        p.on_key(key(KeyCode::Enter)); // -> /w/src, by the user's own hand
        p.take_requests();
        p.on_event(&listing(ConnId::Local, "/w/src", vec![entry("f", false)]));
        // The focused pane moves to a third directory.
        p.on_event(&cwd(ConnId::Local, "/elsewhere"));
        assert!(
            p.take_requests().is_empty(),
            "the user's place must survive a focus move"
        );
        assert_eq!(painted(&p, 20, 5)[0], "/w/src");
    }

    /// ...and following RESUMES once the panel is showing the pane's directory
    /// again -- here because the pane caught up to where the user browsed.
    #[test]
    fn following_resumes_when_the_panel_and_the_pane_agree_again() {
        let mut p = at("/w", vec![entry("src", true)]);
        p.on_key(key(KeyCode::Enter)); // -> /w/src, by the user's own hand
        p.take_requests();
        p.on_event(&listing(ConnId::Local, "/w/src", Vec::new()));
        // The pane cds to where the user is browsing. Nothing to re-list...
        p.on_event(&cwd(ConnId::Local, "/w/src"));
        assert!(
            p.take_requests().is_empty(),
            "the panel is already showing it"
        );
        // ...but the panel is anchored again, so the pane's NEXT move is
        // followed.
        p.on_event(&cwd(ConnId::Local, "/third"));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                path: "/third".to_string()
            }]
        );
    }

    /// A different MACHINE re-targets whatever the user was browsing: the tree
    /// on screen belongs to a filesystem the focus is no longer on.
    #[test]
    fn a_move_to_another_server_re_targets_even_mid_navigation() {
        let mut p = at("/w", vec![entry("src", true)]);
        p.on_key(key(KeyCode::Enter));
        p.take_requests();
        p.on_event(&listing(ConnId::Local, "/w/src", Vec::new()));
        p.on_event(&cwd(ConnId::Remote("pi".to_string()), "/w/src"));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                path: "/w/src".to_string()
            }],
            "the same PATH on another machine is a different directory"
        );
    }

    #[test]
    fn an_unreadable_cwd_is_not_a_move() {
        let mut p = at("/w", vec![entry("a", false)]);
        p.on_event(&PluginEvent::FocusedCwd {
            conn: ConnId::Local,
            cwd: None,
        });
        assert!(p.take_requests().is_empty());
        assert_eq!(painted(&p, 20, 4)[0], "/w");
    }

    // -- Correlation ---------------------------------------------------------

    #[test]
    fn a_listing_for_another_path_is_not_claimed() {
        let mut p = BrowserPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        p.on_event(&listing(
            ConnId::Local,
            "/somewhere/else",
            vec![entry("x", false)],
        ));
        assert_eq!(
            painted(&p, 20, 4)[1],
            "loading…",
            "another panel's answer must not populate this one"
        );
    }

    /// The same path on two machines is two different directories, and the
    /// panel must not accept the wrong server's answer for it.
    #[test]
    fn a_listing_from_another_server_for_the_same_path_is_not_claimed() {
        let mut p = BrowserPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/home/me"));
        p.take_requests();
        p.on_event(&listing(
            ConnId::Remote("pi".to_string()),
            "/home/me",
            vec![entry("wrong-machine", false)],
        ));
        assert_eq!(painted(&p, 24, 4)[1], "loading…");
        p.on_event(&listing(
            ConnId::Local,
            "/home/me",
            vec![entry("right-machine", false)],
        ));
        assert_eq!(painted(&p, 24, 4)[1], "right-machine");
    }

    #[test]
    fn a_dropped_connection_clears_the_tree_it_described() {
        let mut p = at("/w", vec![entry("a", false)]);
        p.on_event(&PluginEvent::ConnectionLost {
            conn: ConnId::Local,
        });
        assert_eq!(painted(&p, 20, 4)[1], "no directory");
    }

    #[test]
    fn another_connection_dropping_leaves_the_panel_alone() {
        let mut p = at("/w", vec![entry("a", false)]);
        p.on_event(&PluginEvent::ConnectionLost {
            conn: ConnId::Remote("pi".to_string()),
        });
        assert_eq!(painted(&p, 20, 4)[1], "a");
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_selection_visible() {
        let p_entries: Vec<DirEntry> = (1..=10).map(|i| entry(&format!("f{i}"), false)).collect();
        let mut p = at("/w", p_entries);
        p.on_key(key(KeyCode::Char('G')));
        // 4 rows tall: header + 3 list rows, showing f8, f9, f10.
        let rows = painted(&p, 20, 4);
        assert_eq!(rows[3], "f10", "the selection must be on screen: {rows:?}");
    }

    #[test]
    fn the_header_keeps_the_end_of_a_path_too_long_to_fit() {
        let p = at("/home/me/a/very/deep/place", vec![entry("f", false)]);
        assert_eq!(painted(&p, 8, 3)[0], "…p/place");
    }
}
