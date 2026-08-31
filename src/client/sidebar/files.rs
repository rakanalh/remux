//! The `files` panel: a file browser with no external configuration.
//!
//! Vim-like navigation of a directory tree, and `Enter` on a file opens it in a
//! **split running `$EDITOR`**. That is the whole feature, and the reason it can
//! exist at all is that this panel is INSIDE the client: it already has a socket
//! to the server, so "open a split" is a message, not a CLI invocation.
//!
//! ## There used to be two of these
//!
//! `files` hosted a real file manager -- `nnn`, `yazi`, `ranger` -- in an
//! auxiliary pane, and `browser` was this. They have been merged: this one took
//! the `files` name, and the hosted-file-manager plugin is gone along with the
//! whole aux-pane machinery on both sides of the wire that existed for it alone.
//!
//! The field that was supposed to choose between them is what made the pair
//! actively harmful rather than merely redundant: `command` meant "the file
//! manager to run" to one and "the editor to open a file with" to the other, so
//! a config written for the first silently taught the second to open every file
//! in `nnn`. That is the bug that prompted the merge. The field is now `editor`,
//! which can only mean one thing; see `make_plugin` for what happens to a
//! `command` left over from before.
//!
//! What is actually lost is a hosted file manager's own key bindings and
//! previews. What is kept is a panel that works with nothing configured, over a
//! remote, without an opener hook (`NNN_OPENER`, yazi's `[opener]`,
//! `rifle.conf`) pointed at a `remux split` -- which is exactly the external
//! configuration the built-in browser existed to avoid.
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

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, MouseEventKind};

use super::nav::{self, NavKey, NavList, HEADER_ROWS};
use super::{
    blank_grid, draw_text, shorten_path, tilde_path, PluginAction, PluginEvent, PluginRequest,
    SidebarPlugin,
};
use crate::client::registry::ConnId;
use crate::config::theme::CompositorTheme;
use crate::protocol::{CellColor, DirEntry, RenderCell};

/// The key that toggles hidden entries.
const HIDDEN_KEY: char = '.';

/// The key that re-lists the current directory NOW.
///
/// It exists even though the panel re-lists on its own, and that is the point:
/// an automatic refresh that has silently stopped -- a reply lost, a request
/// that went out during a connection change -- is worse than no automatic
/// refresh at all, because the panel looks live and is not. A key that always
/// works is the floor under the timer.
const REFRESH_KEY: char = 'r';

/// How often a VISIBLE panel re-lists the directory it is showing.
///
/// Polling rather than a filesystem watcher (`notify` is already a dependency,
/// so the watcher was available). Three reasons, in order of weight:
///
/// * **The directory is on the SERVER, and routinely on a remote.** The listing
///   already goes over the wire, so a poll is one more message on a path that
///   works everywhere; a watcher would have to be a server-side subscription
///   with its own protocol messages, its own lifetime, and its own teardown on
///   disconnect.
/// * **A watcher's lifetime follows the user's navigation.** `h`/`l` change the
///   watched directory on every keystroke, and every one of those is a watch to
///   register and an old one to drop -- on a remote, asynchronously. A leaked
///   watch is a file descriptor and an inotify slot per directory ever visited.
/// * **Polling stops when the panel does.** `poll_after` is asked only of placed
///   panels, so a hidden sidebar costs exactly nothing, with no state to unwind.
///
/// What polling costs, honestly: up to one `read_dir` every two seconds per
/// visible panel, and a change can take that long to appear. For a sidebar
/// watching a directory a build writes into, two seconds is not noticeable; for
/// a directory with a hundred thousand entries it is a real cost, which is what
/// the server's entry cap bounds.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// How long a request may be outstanding before the panel gives up on it and
/// asks again.
///
/// Without this the panel polls only when nothing is in flight -- which is
/// right, or a 200ms-RTT remote would stack requests -- and then one lost reply
/// stops the refresh FOR EVER, leaving a panel that looks live and is frozen.
/// Long enough that a slow remote is never mistaken for a lost reply.
const PENDING_TIMEOUT: Duration = Duration::from_secs(10);

pub struct FilesPlugin {
    /// The editor override from `[[sidebar.panel]]`'s `editor`, or `None` to let
    /// the server choose. OPTIONAL, and normally absent: a panel that names no
    /// editor is the intended configuration, and the server already knows
    /// `$EDITOR`.
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
    /// The rows actually rendered: [`FilesPlugin::entries`] after the hidden
    /// filter. Rebuilt whenever either changes.
    rows: Vec<DirEntry>,
    /// Why the current directory could not be listed. Shown, not swallowed.
    error: Option<String>,
    /// Whether the server capped this listing.
    truncated: bool,
    /// The home directory of the machine [`FilesPlugin::conn`] names, as the
    /// last claimed listing reported it -- what turns `/home/you/Work` into
    /// `~/Work` in the header.
    ///
    /// Kept across navigation on the same connection so the header does not
    /// flash the absolute path for the round trip of every `l`, and cleared the
    /// moment the connection changes: a remote's home is a different string and
    /// possibly a different LAYOUT (`/Users/x`), and pairing one machine's home
    /// with another's path is the mistake this field exists to avoid rather than
    /// to make. `None` until the first listing arrives, and from a server too
    /// old to send one -- the header then shows the full path.
    home: Option<String>,
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
    /// When the most recent `ListDirectory` went out, which is what the refresh
    /// deadline is measured from.
    ///
    /// An ANCHOR, not an interval: `poll_after` is asked on every pass of the
    /// client's event loop -- so on every keystroke and every frame -- and a
    /// panel that answered "two seconds from now" each time would have its
    /// deadline pushed back for ever by a busy pane.
    last_request: Option<Instant>,
    nav: NavList,
    requests: Vec<PluginRequest>,
}

impl FilesPlugin {
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
            home: None,
            show_hidden: false,
            pending: None,
            pending_select: None,
            last_request: None,
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
        // The SAME conn goes into `pending` and into the request, from one
        // clone. That is the whole fix for the routing race: the client used to
        // resolve the destination itself, from `foreground()`, which moves
        // before `FocusedCwd` does.
        self.pending = Some((conn.clone(), path.clone()));
        self.last_request = Some(Instant::now());
        self.requests
            .push(PluginRequest::ListDirectory { conn, path });
    }

    /// Ask for the CURRENT directory again, keeping everything on screen.
    ///
    /// Deliberately not `open_dir(self.cwd)`: that one drops the entries, the
    /// selection and the scroll position, which is right when the user asked to
    /// go somewhere else and catastrophic on a two-second timer -- the panel
    /// would blink empty and lose the cursor twice a minute. The old rows stay
    /// up until the new ones arrive, and `rebuild` re-points the selection by
    /// entry NAME, so a file appearing above the cursor does not move it.
    ///
    /// Safe to call with a request already in flight: `pending` is overwritten
    /// with the same `(conn, path)` it already held, so the reply is still
    /// claimed. The timer avoids doing so anyway (see [`Self::refresh_due`]);
    /// the manual key does not, because "refresh now" must mean now.
    fn refresh(&mut self) {
        let (Some(conn), Some(path)) = (self.conn.clone(), self.cwd.clone()) else {
            return;
        };
        self.pending = Some((conn.clone(), path.clone()));
        self.last_request = Some(Instant::now());
        self.requests
            .push(PluginRequest::ListDirectory { conn, path });
    }

    /// How long until the automatic re-list is due, `Some(ZERO)` if it is due
    /// now, or `None` if this panel does not want one.
    ///
    /// `None` before the panel has a directory: there is nothing to re-list, and
    /// a panel waiting for its first `FocusedCwd` must not arm the client's
    /// timer to discover that twice a second.
    ///
    /// While a request is in flight the deadline is [`PENDING_TIMEOUT`] rather
    /// than [`REFRESH_INTERVAL`] -- so a slow remote is waited for instead of
    /// being piled on, but a reply that never comes cannot freeze the panel for
    /// the rest of the session.
    fn refresh_due(&self) -> Option<Duration> {
        self.cwd.as_ref()?;
        self.conn.as_ref()?;
        let sent = self.last_request?;
        let deadline = if self.pending.is_some() {
            PENDING_TIMEOUT
        } else {
            REFRESH_INTERVAL
        };
        Some(deadline.saturating_sub(sent.elapsed()))
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
        // The panel's own conn, not the client's foreground. `self.cwd` came
        // from a listing on THIS connection, so `path` names a file on that
        // machine and nowhere else; opening it anywhere else creates a file
        // rather than editing one. `None` cannot happen with rows on screen --
        // rows only ever arrive from a listing, which needs a conn -- but a
        // `let else` that returns is the honest shape for "there is nothing to
        // address this to".
        let Some(conn) = self.conn.clone() else {
            return PluginAction::None;
        };
        self.requests.push(PluginRequest::OpenInSplit {
            conn,
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
    /// lesson the agents panel's "no detection" note cost two review rounds.
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

impl SidebarPlugin for FilesPlugin {
    fn title(&self) -> &str {
        "Files"
    }

    fn min_size(&self) -> (u16, u16) {
        (8, 3)
    }

    /// Yes -- indirectly. This panel never reads a `SessionTree`, but
    /// [`PluginEvent::FocusedCwd`] is derived from one, so a client whose only
    /// panel is this one must still subscribe or the directory it starts in
    /// would never arrive.
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
            // Substituted BEFORE truncating: `~` is worth ten or more columns
            // here, and they go to the end of the path -- the part that says
            // which directory this is.
            Some(cwd) => shorten_path(&tilde_path(cwd, self.home.as_deref()), cols as usize),
            None => self.title().to_string(),
        };
        nav::draw_header(&mut grid, &header, focused, theme, &bg);

        // The same budget rule the agents panel arrived at: at most half the
        // rows when there is a list to crowd out, all of them when there is not.
        // An error means there are no entries anyway, so this only ever binds on
        // the truncation notice -- which appears exactly when the list is
        // longest.
        //
        // ...but never fewer than ONE row while there is a row at all. Plain
        // `capacity / 2` is 0 in a two-row panel (one header, one line), and
        // `take(0)` dropped the truncation notice entirely -- silently, in the
        // one case where `DirectoryListing::truncated` promises it is "reported,
        // never silent". A single line showing "… list truncated" instead of one
        // filename is a poor panel; a single line showing one filename out of
        // five thousand with nothing to say so is a WRONG one.
        //
        // The agents panel does not have this bug despite the shared rule: its
        // note appears only when it has no rows, so the halving never binds
        // there. That is why this floor lives here and not in `nav`.
        let capacity = (rows as usize).saturating_sub(HEADER_ROWS);
        let all_notes = self.notes();
        let budget = if self.rows.is_empty() {
            capacity
        } else {
            (capacity / 2).max(1).min(capacity)
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
            KeyCode::Char(REFRESH_KEY) => {
                self.refresh();
                return PluginAction::Redraw;
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
        match self.nav.click(y, self.rows.len()) {
            nav::HitOutcome::Ignore => PluginAction::None,
            nav::HitOutcome::Moved => PluginAction::Redraw,
            nav::HitOutcome::Activate => self.activate(),
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
                    // The old machine's home says nothing about the new one's
                    // paths. Dropped here rather than left to be overwritten by
                    // the next listing: the header renders in between.
                    self.home = None;
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
                home,
            } => {
                // Claim only the answer to the request THIS panel has out. The
                // event is broadcast, so every other files panel's listing
                // arrives here too.
                if self.pending.as_ref() != Some(&(conn.clone(), path.clone())) {
                    return;
                }
                self.pending = None;
                self.entries = entries.clone();
                self.error = error.clone();
                self.truncated = *truncated;
                // Taken from the message that carried the path it applies to,
                // which is what makes the pairing safe.
                self.home = home.clone();
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
                    self.home = None;
                    self.pending = None;
                    self.last_request = None;
                    self.nav.set_selected(0);
                }
            }
            // The session tree is the sessions panel's and the agent list the
            // agents panel's.
            PluginEvent::SessionTree { .. } | PluginEvent::Agents { .. } => {}
        }
    }

    fn poll_after(&self) -> Option<Duration> {
        self.refresh_due()
    }

    fn tick(&mut self) {
        // Every placed panel is ticked whenever ANY panel's timer fires, so
        // check our own deadline rather than assume it is ours.
        if self.refresh_due().is_some_and(|d| d.is_zero()) {
            self.refresh();
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
        listing_from(conn, path, entries, None)
    }

    /// A listing that also reports the answering server's home -- what the
    /// header needs to say `~`.
    fn listing_from(
        conn: ConnId,
        path: &str,
        entries: Vec<DirEntry>,
        home: Option<&str>,
    ) -> PluginEvent {
        PluginEvent::DirectoryListing {
            conn,
            path: path.to_string(),
            entries,
            error: None,
            truncated: false,
            home: home.map(str::to_string),
        }
    }

    /// The panel's visible rows, from a real render.
    fn painted(p: &FilesPlugin, cols: u16, rows: u16) -> Vec<String> {
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
    fn at(dir: &str, entries: Vec<DirEntry>) -> FilesPlugin {
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, dir));
        p.take_requests();
        p.on_event(&listing(ConnId::Local, dir, entries));
        p
    }

    fn panel(plugin: &str, editor: Option<&str>, command: Option<&str>) -> PanelConfig {
        PanelConfig {
            plugin: plugin.to_string(),
            weight: 1,
            editor: editor.map(str::to_string),
            command: command.map(str::to_string),
        }
    }

    // -----------------------------------------------------------------
    // the automatic refresh
    // -----------------------------------------------------------------

    /// A panel with nothing to list must not arm the client's timer.
    #[test]
    fn a_panel_with_no_directory_asks_for_no_tick() {
        let p = FilesPlugin::new(None);
        assert_eq!(p.poll_after(), None);
    }

    #[test]
    fn a_panel_showing_a_directory_asks_to_be_ticked() {
        let mut p = at("/w", vec![entry("a", false)]);
        let d = p.poll_after().expect("a panel with a listing must poll");
        assert!(
            d <= REFRESH_INTERVAL && !d.is_zero(),
            "the deadline must count DOWN from the last request, not restart: {d:?}"
        );
        // Asked again immediately, the answer must be no larger -- an anchor,
        // not a fresh interval. A panel that re-armed here would be pushed past
        // its deadline for ever by a busy pane.
        let again = p.poll_after().expect("still polling");
        assert!(
            again <= d,
            "the deadline moved forwards: {d:?} then {again:?}"
        );
        // Not yet due, so a tick must ask for nothing.
        p.tick();
        assert_eq!(p.take_requests(), vec![], "the panel re-listed early");
    }

    /// The refresh keeps the rows and the cursor. Losing either twice a minute
    /// is worse than not refreshing at all.
    #[test]
    fn a_refresh_keeps_the_rows_and_the_selection_until_the_answer_arrives() {
        let mut p = at("/w", vec![entry("a", false), entry("b", false)]);
        p.on_key(key(KeyCode::Char('j')));
        assert_eq!(painted(&p, 20, 5)[2], "b");

        p.on_key(key(KeyCode::Char(REFRESH_KEY)));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                conn: ConnId::Local,
                path: "/w".to_string()
            }]
        );
        let rows = painted(&p, 20, 5);
        assert_eq!(
            (rows[1].as_str(), rows[2].as_str()),
            ("a", "b"),
            "the panel blanked while waiting for the refresh: {rows:?}"
        );

        // A file appears ABOVE the selection: the cursor stays on `b`.
        p.on_event(&listing(
            ConnId::Local,
            "/w",
            vec![entry("a", false), entry("aa", false), entry("b", false)],
        ));
        let rows = painted(&p, 20, 6);
        assert_eq!(rows[1..4], ["a", "aa", "b"], "{rows:?}");
        p.on_key(key(KeyCode::Enter));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::OpenInSplit {
                conn: ConnId::Local,
                path: "/w/b".to_string(),
                command: None,
                vertical: true,
            }],
            "the refresh moved the selection off the entry the user was on"
        );
    }

    /// The manual key works with a request already outstanding -- "refresh now"
    /// has to mean now, and it is the floor under a timer that has stopped.
    #[test]
    fn the_refresh_key_works_even_with_a_request_in_flight() {
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests(); // the initial listing, still unanswered
        p.on_key(key(KeyCode::Char(REFRESH_KEY)));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                conn: ConnId::Local,
                path: "/w".to_string()
            }]
        );
    }

    /// While a request is outstanding the deadline is the LOST-REPLY timeout,
    /// not the refresh interval: a slow remote is waited for rather than piled
    /// on, and a reply that never comes still cannot freeze the panel.
    #[test]
    fn an_outstanding_request_is_waited_for_but_not_for_ever() {
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        let d = p
            .poll_after()
            .expect("a panel awaiting a listing still polls");
        assert!(
            d > REFRESH_INTERVAL,
            "a request in flight must not be re-sent on the refresh interval: {d:?}"
        );
        assert!(d <= PENDING_TIMEOUT, "{d:?}");
    }

    /// A connection going away takes the anchor with it, so a panel that has
    /// nothing to list stops arming the timer.
    #[test]
    fn a_lost_connection_stops_the_polling() {
        let mut p = at("/w", vec![entry("a", false)]);
        assert!(p.poll_after().is_some());
        p.on_event(&PluginEvent::ConnectionLost {
            conn: ConnId::Local,
        });
        assert_eq!(
            p.poll_after(),
            None,
            "the panel kept polling for a machine it is no longer talking to"
        );
    }

    #[test]
    fn a_files_panel_needs_no_configuration_at_all() {
        assert!(
            make_plugin(&panel("files", None, None)).is_some(),
            "zero configuration is the whole point of this panel"
        );
    }

    /// The old name for THIS panel. It has to keep working: falling through to
    /// the unknown-plugin rule would SKIP the panel, so a user who upgrades
    /// would find their sidebar quietly missing a panel.
    #[test]
    fn the_old_browser_name_still_loads_this_panel() {
        assert!(
            make_plugin(&panel("browser", None, None)).is_some(),
            "`plugin = \"browser\"` must still resolve, or an upgrade silently \
             drops the panel it names"
        );
    }

    /// The old aux-pane `files` plugin required `command` and is gone. A config
    /// still naming a file manager there must LOAD -- and must not treat `nnn`
    /// as an editor, which is the bug that prompted the merge.
    #[test]
    fn a_leftover_command_is_ignored_rather_than_read_as_the_editor() {
        for plugin in ["files", "browser"] {
            let mut p = make_plugin(&panel(plugin, None, Some("nnn")))
                .unwrap_or_else(|| panic!("`{plugin}` with a stale `command` must still load"));
            p.on_event(&cwd(ConnId::Local, "/w"));
            p.take_requests();
            p.on_event(&listing(ConnId::Local, "/w", vec![entry("f", false)]));
            p.on_key(key(KeyCode::Enter));
            assert_eq!(
                p.take_requests(),
                vec![PluginRequest::OpenInSplit {
                    conn: ConnId::Local,
                    path: "/w/f".to_string(),
                    command: None,
                    vertical: true,
                }],
                "`command = \"nnn\"` was read as the editor, which is exactly the \
                 report this merge exists to fix: Enter on a file opened nnn"
            );
        }
    }

    /// And `editor`, the field that replaced it, IS read.
    #[test]
    fn the_editor_field_is_what_overrides_the_editor_now() {
        let mut p = make_plugin(&panel("files", Some("hx"), None)).expect("panel must load");
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        p.on_event(&listing(ConnId::Local, "/w", vec![entry("f", false)]));
        p.on_key(key(KeyCode::Enter));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::OpenInSplit {
                conn: ConnId::Local,
                path: "/w/f".to_string(),
                command: Some("hx".to_string()),
                vertical: true,
            }]
        );
    }

    #[test]
    fn the_first_focused_cwd_asks_the_server_for_that_directory() {
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/home/me"));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::ListDirectory {
                conn: ConnId::Local,
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
                conn: ConnId::Local,
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
                conn: ConnId::Local,
                path: "/w/main.rs".to_string(),
                command: None,
                vertical: true,
            }]
        );
    }

    #[test]
    fn a_configured_editor_travels_with_the_request_but_is_not_resolved_here() {
        let mut p = FilesPlugin::new(Some("hx".to_string()));
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        p.on_event(&listing(ConnId::Local, "/w", vec![entry("f", false)]));
        p.on_key(key(KeyCode::Enter));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::OpenInSplit {
                conn: ConnId::Local,
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
                conn: ConnId::Local,
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
                conn: ConnId::Local,
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
                conn: ConnId::Local,
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

    /// The header says `~/Work`, and it says it because the SERVER said where
    /// its home is.
    #[test]
    fn the_header_shortens_the_servers_home_to_a_tilde() {
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/home/me/Work"));
        p.take_requests();
        p.on_event(&listing_from(
            ConnId::Local,
            "/home/me/Work",
            vec![entry("a.txt", false)],
            Some("/home/me"),
        ));
        assert_eq!(painted(&p, 30, 4)[0], "~/Work");

        // The home itself, not a directory below it.
        p.on_event(&cwd(ConnId::Local, "/home/me"));
        p.take_requests();
        p.on_event(&listing_from(
            ConnId::Local,
            "/home/me",
            Vec::new(),
            Some("/home/me"),
        ));
        assert_eq!(painted(&p, 30, 4)[0], "~");
    }

    /// A server that sends no home -- one built before the field existed -- gets
    /// the header the panel has always had, rather than a `~` guessed from the
    /// CLIENT's own home.
    #[test]
    fn without_a_home_from_the_server_the_header_is_the_full_path() {
        let p = at("/home/me/Work", vec![entry("a.txt", false)]);
        assert_eq!(painted(&p, 30, 4)[0], "/home/me/Work");
    }

    /// The header shortens where the panel IS, so the substitution has to be
    /// worth something on a panel too narrow for the absolute path -- which is
    /// the width the request was actually about.
    #[test]
    fn the_tilde_survives_the_width_the_absolute_path_would_not() {
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/home/me/Work/Personal/Remux"));
        p.take_requests();
        p.on_event(&listing_from(
            ConnId::Local,
            "/home/me/Work/Personal/Remux",
            Vec::new(),
            Some("/home/me"),
        ));
        assert_eq!(painted(&p, 22, 4)[0], "~/Work/Personal/Remux");
    }

    /// The failure mode the field is per-message to prevent: the foreground
    /// moves to another machine, and until that machine's first listing lands
    /// the panel holds a path from one computer and a home from another. It must
    /// not put them together -- a remote whose home is `/Users/them` renders
    /// nothing shortened, and one that merely shares a prefix would render a `~`
    /// for a directory that is nobody's home there.
    #[test]
    fn a_home_never_outlives_the_connection_it_came_from() {
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/home/me/Work"));
        p.take_requests();
        p.on_event(&listing_from(
            ConnId::Local,
            "/home/me/Work",
            Vec::new(),
            Some("/home/me"),
        ));
        assert_eq!(painted(&p, 30, 4)[0], "~/Work");

        // The foreground moves to a remote that happens to use the same paths.
        p.on_event(&cwd(ConnId::Remote("box".to_string()), "/home/me/Other"));
        assert_eq!(
            painted(&p, 30, 4)[0],
            "/home/me/Other",
            "the local server's home must not be applied to the remote's path"
        );
        p.take_requests();
        p.on_event(&listing_from(
            ConnId::Remote("box".to_string()),
            "/home/me/Other",
            Vec::new(),
            Some("/home/me"),
        ));
        assert_eq!(
            painted(&p, 30, 4)[0],
            "~/Other",
            "and once the remote says so itself, it shortens"
        );

        // A dropped connection takes the home with the rest of that machine.
        p.on_event(&PluginEvent::ConnectionLost {
            conn: ConnId::Remote("box".to_string()),
        });
        assert!(p.home.is_none());
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
            home: None,
        });
        let rows = painted(&p, 24, 5);
        assert_eq!(rows[1], "permission denied");
        assert_ne!(rows[1], "empty");
    }

    #[test]
    fn a_genuinely_empty_directory_says_empty_and_a_pending_one_says_loading() {
        let mut p = FilesPlugin::new(None);
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
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        p.on_event(&PluginEvent::DirectoryListing {
            conn: ConnId::Local,
            path: "/w".to_string(),
            entries: (0..20).map(|i| entry(&format!("f{i}"), false)).collect(),
            error: None,
            truncated: true,
            home: None,
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

    /// ...and it survives a panel with only ONE line to give it.
    ///
    /// `capacity / 2` is 0 at two rows, and `take(0)` dropped the notice
    /// entirely -- exactly the silence `DirectoryListing::truncated` exists to
    /// prevent, in the smallest panel a user can drag to.
    #[test]
    fn the_truncation_notice_survives_a_panel_with_one_line() {
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        p.on_event(&PluginEvent::DirectoryListing {
            conn: ConnId::Local,
            path: "/w".to_string(),
            entries: (0..20).map(|i| entry(&format!("f{i}"), false)).collect(),
            error: None,
            truncated: true,
            home: None,
        });
        // Header + exactly one content line.
        let rows = painted(&p, 24, 2);
        assert_eq!(
            rows[1], "… list truncated",
            "the one line goes to the notice, not to one filename out of five \
             thousand with nothing to say so: got {rows:?}"
        );
    }

    /// The notice is an explanation, not a destination.
    #[test]
    fn a_click_on_the_truncation_notice_selects_nothing() {
        let mut p = FilesPlugin::new(None);
        p.on_event(&cwd(ConnId::Local, "/w"));
        p.take_requests();
        p.on_event(&PluginEvent::DirectoryListing {
            conn: ConnId::Local,
            path: "/w".to_string(),
            entries: (0..20).map(|i| entry(&format!("f{i}"), false)).collect(),
            error: None,
            truncated: true,
            home: None,
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
                conn: ConnId::Local,
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
                conn: ConnId::Local,
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
                conn: ConnId::Local,
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
                conn: ConnId::Remote("pi".to_string()),
                path: "/w/src".to_string()
            }],
            "the same PATH on another machine is a different directory, and the \
             request must be ADDRESSED to that machine -- the client used to \
             resolve the destination from its own foreground instead"
        );
    }

    /// The worst case in the plugin surface, pinned: `Enter` must address the
    /// machine the PANEL is on.
    ///
    /// The client used to route this to its own `foreground()`, on the argument
    /// that the two are equal by construction. They are equal in the steady
    /// state; the construction has a window, because `foreground()` moves on the
    /// switch and the panel only learns of it from the new connection's first
    /// tree push. An `OpenInSplit` sent into that window opens an editor on the
    /// NEW machine at a path from the OLD one -- which does not fail, it creates
    /// an empty file with an entirely plausible name.
    #[test]
    fn enter_opens_the_file_on_the_machine_the_panel_is_looking_at() {
        let mut p = FilesPlugin::new(None);
        let pi = ConnId::Remote("pi".to_string());
        p.on_event(&cwd(pi.clone(), "/w"));
        p.take_requests();
        p.on_event(&listing(pi.clone(), "/w", vec![entry("f", false)]));
        p.on_key(key(KeyCode::Enter));
        assert_eq!(
            p.take_requests(),
            vec![PluginRequest::OpenInSplit {
                conn: pi,
                path: "/w/f".to_string(),
                command: None,
                vertical: true,
            }],
            "the request must name the panel's own connection; a path from one \
             machine opened on another creates a file rather than editing one"
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
        let mut p = FilesPlugin::new(None);
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
        let mut p = FilesPlugin::new(None);
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
