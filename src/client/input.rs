use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::keybindings::{parse_command, InsertBindings, KeyNode, KeybindingTree};
use crate::protocol::RemuxCommand;

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

/// The current input mode (vim-style modal editing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Insert,
    Normal,
    Visual,
    Rename,
}

// ---------------------------------------------------------------------------
// SelectionMode
// ---------------------------------------------------------------------------

/// The type of selection active in Visual mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// No selection active.
    None,
    /// Character-wise selection.
    Character,
    /// Line-wise selection.
    Line,
}

// ---------------------------------------------------------------------------
// VisualState
// ---------------------------------------------------------------------------

/// State for Visual mode scrollback navigation and selection.
#[derive(Debug, Clone)]
pub struct VisualState {
    /// Number of lines scrolled up from the bottom of the scrollback buffer.
    pub scroll_offset: usize,
    /// Cursor row within the visible area.
    pub cursor_row: usize,
    /// Cursor column.
    pub cursor_col: usize,
    /// Start position of the selection in scrollback coordinates (row, col).
    pub selection_start: Option<(usize, usize)>,
    /// The current selection mode.
    pub selection_mode: SelectionMode,
    /// Current search query, if any.
    pub search_query: Option<String>,
    /// Positions of search matches as (row, col) in scrollback coordinates.
    pub search_matches: Vec<(usize, usize)>,
    /// Index into `search_matches` for the currently highlighted match.
    pub current_match: usize,
    /// Total number of scrollback lines available (set by the caller).
    pub total_lines: usize,
    /// Number of visible rows in the pane (set by the caller).
    pub visible_rows: usize,
}

impl VisualState {
    /// Create a new `VisualState` positioned at the bottom of the scrollback.
    pub fn new(visible_rows: usize, total_lines: usize) -> Self {
        Self {
            scroll_offset: 0,
            cursor_row: visible_rows.saturating_sub(1),
            cursor_col: 0,
            selection_start: None,
            selection_mode: SelectionMode::None,
            search_query: None,
            search_matches: Vec::new(),
            current_match: 0,
            total_lines,
            visible_rows,
        }
    }

    /// Move the cursor up by one line, scrolling if needed.
    pub fn cursor_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
        } else {
            self.scroll_up(1);
        }
    }

    /// Move the cursor down by one line, scrolling if needed.
    pub fn cursor_down(&mut self) {
        if self.cursor_row < self.visible_rows.saturating_sub(1) {
            self.cursor_row += 1;
        } else {
            self.scroll_down(1);
        }
    }

    /// Scroll up by `n` lines.
    pub fn scroll_up(&mut self, n: usize) {
        let max_offset = self.total_lines.saturating_sub(self.visible_rows);
        self.scroll_offset = (self.scroll_offset + n).min(max_offset);
    }

    /// Scroll down by `n` lines.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    /// Scroll up by half a page.
    pub fn half_page_up(&mut self) {
        let half = self.visible_rows / 2;
        self.scroll_up(half);
    }

    /// Scroll down by half a page.
    pub fn half_page_down(&mut self) {
        let half = self.visible_rows / 2;
        self.scroll_down(half);
    }

    /// Jump to the top of the scrollback buffer.
    pub fn jump_to_top(&mut self) {
        let max_offset = self.total_lines.saturating_sub(self.visible_rows);
        self.scroll_offset = max_offset;
        self.cursor_row = 0;
    }

    /// Jump to the bottom of the scrollback buffer.
    pub fn jump_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.cursor_row = self.visible_rows.saturating_sub(1);
    }

    /// Start or toggle character-wise selection.
    pub fn start_char_selection(&mut self) {
        if self.selection_mode == SelectionMode::Character {
            self.selection_mode = SelectionMode::None;
            self.selection_start = None;
        } else {
            self.selection_mode = SelectionMode::Character;
            self.selection_start = Some(self.scrollback_cursor_pos());
        }
    }

    /// Start or toggle line-wise selection.
    pub fn start_line_selection(&mut self) {
        if self.selection_mode == SelectionMode::Line {
            self.selection_mode = SelectionMode::None;
            self.selection_start = None;
        } else {
            self.selection_mode = SelectionMode::Line;
            self.selection_start = Some(self.scrollback_cursor_pos());
        }
    }

    /// Move to the next search match.
    pub fn next_match(&mut self) {
        if !self.search_matches.is_empty() {
            self.current_match = (self.current_match + 1) % self.search_matches.len();
        }
    }

    /// Move to the previous search match.
    pub fn prev_match(&mut self) {
        if !self.search_matches.is_empty() {
            self.current_match = if self.current_match == 0 {
                self.search_matches.len() - 1
            } else {
                self.current_match - 1
            };
        }
    }

    /// Get the cursor position in scrollback coordinates.
    fn scrollback_cursor_pos(&self) -> (usize, usize) {
        let row = self
            .total_lines
            .saturating_sub(self.scroll_offset + self.visible_rows)
            + self.cursor_row;
        (row, self.cursor_col)
    }

    /// Reset the visual state (selection and search).
    pub fn reset(&mut self) {
        self.selection_mode = SelectionMode::None;
        self.selection_start = None;
        self.search_query = None;
        self.search_matches.clear();
        self.current_match = 0;
    }
}

// ---------------------------------------------------------------------------
// InputAction
// ---------------------------------------------------------------------------

/// The result of processing a single key event.
#[derive(Debug, Clone, PartialEq)]
pub enum InputAction {
    /// Send raw bytes to the active pane's PTY (Insert mode).
    SendToPty(Vec<u8>),
    /// Execute a Remux command (Normal/Visual mode).
    Execute(RemuxCommand),
    /// The input mode changed.
    ModeChanged(Mode),
    /// Show the which-key popup for a group.
    ShowWhichKey(String, Vec<(char, String)>),
    /// Hide the which-key popup.
    HideWhichKey,
    /// Yank (copy) the selected text to the clipboard.
    YankToClipboard(String),
    /// Open the scrollback in the user's editor.
    EditInEditor,
    /// Enter search mode (prompt the user for a search query).
    SearchPrompt,
    /// Update the visual mode scroll offset.
    VisualScroll { offset: usize },
    /// The rename buffer was updated (for status bar display).
    RenameUpdate(String),
    /// No action to take.
    None,
}

// ---------------------------------------------------------------------------
// KeybindingState
// ---------------------------------------------------------------------------

/// Tracks the current position in the keybinding tree during a multi-key
/// sequence in Normal mode.
#[derive(Debug, Clone)]
pub struct KeybindingState {
    current_path: Vec<char>,
}

impl KeybindingState {
    pub fn new() -> Self {
        Self {
            current_path: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.current_path.clear();
    }

    pub fn is_at_root(&self) -> bool {
        self.current_path.is_empty()
    }

    pub fn push(&mut self, key: char) {
        self.current_path.push(key);
    }

    pub fn path(&self) -> &[char] {
        &self.current_path
    }
}

impl Default for KeybindingState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// InputHandler
// ---------------------------------------------------------------------------

/// Processes raw key events and produces [`InputAction`]s based on the current
/// mode and keybinding configuration.
#[derive(Debug)]
pub struct InputHandler {
    /// Current input mode.
    pub mode: Mode,
    /// The key that switches from Insert to Normal mode.
    mode_switch_key: KeyCode,
    /// State for multi-key Normal-mode sequences.
    keybinding_state: KeybindingState,
    /// The keybinding tree (owned clone from config).
    keybinding_tree: KeybindingTree,
    /// Flat insert-mode bindings (modifier keys → commands).
    insert_bindings: InsertBindings,
    /// State for Visual mode scrollback navigation.
    pub visual_state: Option<VisualState>,
    /// Pending 'g' key for gg motion in visual mode.
    pending_g: bool,
    /// Buffer for the rename input mode.
    pub rename_buffer: String,
}

impl InputHandler {
    /// Create a new `InputHandler` with the given keybinding tree, insert
    /// bindings, and mode-switch key.
    pub fn new(
        keybinding_tree: KeybindingTree,
        insert_bindings: InsertBindings,
        mode_switch_key: KeyCode,
    ) -> Self {
        Self {
            mode: Mode::Insert,
            mode_switch_key,
            keybinding_state: KeybindingState::new(),
            keybinding_tree,
            insert_bindings,
            visual_state: None,
            pending_g: false,
            rename_buffer: String::new(),
        }
    }

    /// Create a new `InputHandler` with defaults (Esc to switch modes, default
    /// keybinding tree and insert bindings).
    pub fn with_defaults() -> Self {
        Self::new(
            KeybindingTree::default(),
            InsertBindings::default(),
            KeyCode::Esc,
        )
    }

    /// Process a key event and return the appropriate action.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        match self.mode {
            Mode::Insert => self.handle_insert_key(key),
            Mode::Normal => self.handle_normal_key(key),
            Mode::Visual => self.handle_visual_key(key),
            Mode::Rename => self.handle_rename_key(key),
        }
    }

    /// Initialize visual mode with the given scrollback dimensions.
    pub fn enter_visual_mode(&mut self, visible_rows: usize, total_lines: usize) {
        self.mode = Mode::Visual;
        self.visual_state = Some(VisualState::new(visible_rows, total_lines));
        self.pending_g = false;
    }

    // -----------------------------------------------------------------------
    // Insert mode
    // -----------------------------------------------------------------------

    fn handle_insert_key(&mut self, key: KeyEvent) -> InputAction {
        // Check for mode switch key first.
        // Only require that no Ctrl/Alt/Shift modifiers are held; ignore
        // SUPER/HYPER/META which some terminals may report spuriously.
        let dominated = KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT;
        if key.code == self.mode_switch_key && !key.modifiers.intersects(dominated) {
            self.mode = Mode::Normal;
            return InputAction::ModeChanged(Mode::Normal);
        }

        // Check insert mode bindings before forwarding to PTY.
        if let Some(cmd) = self.insert_bindings.lookup(&key) {
            return InputAction::Execute(cmd.clone());
        }

        // Convert the key event to bytes for the PTY.
        match key_event_to_bytes(&key) {
            Some(bytes) => InputAction::SendToPty(bytes),
            Option::None => InputAction::None,
        }
    }

    // -----------------------------------------------------------------------
    // Normal mode
    // -----------------------------------------------------------------------

    fn handle_normal_key(&mut self, key: KeyEvent) -> InputAction {
        // Escape cancels any partial sequence and hides which-key.
        if key.code == KeyCode::Esc {
            if !self.keybinding_state.is_at_root() {
                self.keybinding_state.reset();
                return InputAction::HideWhichKey;
            }
            return InputAction::None;
        }

        // We only handle character keys and Enter in normal mode.
        let ch = match key.code {
            KeyCode::Char(c) => c,
            KeyCode::Enter => {
                self.mode = Mode::Insert;
                return InputAction::ModeChanged(Mode::Insert);
            }
            _ => return InputAction::None,
        };

        self.keybinding_state.push(ch);
        let path = self.keybinding_state.path().to_vec();

        match self.keybinding_tree.lookup(&path) {
            Some(KeyNode::Leaf { action, .. }) => {
                self.keybinding_state.reset();
                if let Some(cmd) = parse_command(action) {
                    // Handle mode-switch commands.
                    match &cmd {
                        RemuxCommand::EnterInsertMode => {
                            self.mode = Mode::Insert;
                            return InputAction::ModeChanged(Mode::Insert);
                        }
                        RemuxCommand::EnterVisualMode => {
                            self.mode = Mode::Visual;
                            self.visual_state = Some(VisualState::new(24, 1000));
                            return InputAction::ModeChanged(Mode::Visual);
                        }
                        RemuxCommand::PaneRename(_) => {
                            self.mode = Mode::Rename;
                            self.rename_buffer.clear();
                            return InputAction::ModeChanged(Mode::Rename);
                        }
                        _ => {}
                    }
                    InputAction::Execute(cmd)
                } else {
                    InputAction::None
                }
            }
            Some(KeyNode::Group { label, .. }) => {
                // We have entered a group -- show which-key popup.
                if let Some(children) = self.keybinding_tree.children_at(&path) {
                    InputAction::ShowWhichKey(label.clone(), children)
                } else {
                    InputAction::None
                }
            }
            Option::None => {
                // No match -- reset and ignore.
                self.keybinding_state.reset();
                InputAction::HideWhichKey
            }
        }
    }

    // -----------------------------------------------------------------------
    // Visual mode
    // -----------------------------------------------------------------------

    fn handle_visual_key(&mut self, key: KeyEvent) -> InputAction {
        // Escape returns to Normal mode.
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            self.keybinding_state.reset();
            if let Some(vs) = self.visual_state.as_mut() {
                vs.reset();
            }
            self.visual_state = None;
            self.pending_g = false;
            return InputAction::ModeChanged(Mode::Normal);
        }

        // Handle Ctrl-d / Ctrl-u for half-page scroll.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                match c {
                    'd' => {
                        if let Some(vs) = self.visual_state.as_mut() {
                            vs.half_page_down();
                            return InputAction::VisualScroll {
                                offset: vs.scroll_offset,
                            };
                        }
                    }
                    'u' => {
                        if let Some(vs) = self.visual_state.as_mut() {
                            vs.half_page_up();
                            return InputAction::VisualScroll {
                                offset: vs.scroll_offset,
                            };
                        }
                    }
                    _ => {}
                }
                return InputAction::None;
            }
        }

        let ch = match key.code {
            KeyCode::Char(c) => c,
            _ => return InputAction::None,
        };

        // Handle 'gg' motion.
        if self.pending_g {
            self.pending_g = false;
            if ch == 'g' {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.jump_to_top();
                    return InputAction::VisualScroll {
                        offset: vs.scroll_offset,
                    };
                }
            }
            return InputAction::None;
        }

        match ch {
            'j' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.cursor_down();
                    return InputAction::VisualScroll {
                        offset: vs.scroll_offset,
                    };
                }
            }
            'k' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.cursor_up();
                    return InputAction::VisualScroll {
                        offset: vs.scroll_offset,
                    };
                }
            }
            'G' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.jump_to_bottom();
                    return InputAction::VisualScroll {
                        offset: vs.scroll_offset,
                    };
                }
            }
            'g' => {
                self.pending_g = true;
                return InputAction::None;
            }
            'v' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.start_char_selection();
                }
                return InputAction::None;
            }
            'V' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.start_line_selection();
                }
                return InputAction::None;
            }
            'y' => {
                // Yank selection. For now, return the fact that yank was requested.
                // The actual text extraction happens at a higher level that has
                // access to the scrollback buffer.
                return InputAction::YankToClipboard(String::new());
            }
            '/' => {
                return InputAction::SearchPrompt;
            }
            'n' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.next_match();
                }
                return InputAction::None;
            }
            'N' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.prev_match();
                }
                return InputAction::None;
            }
            'e' => {
                return InputAction::EditInEditor;
            }
            _ => {}
        }

        InputAction::None
    }

    // -----------------------------------------------------------------------
    // Rename mode
    // -----------------------------------------------------------------------

    fn handle_rename_key(&mut self, key: KeyEvent) -> InputAction {
        match key.code {
            KeyCode::Esc => {
                // Cancel rename, return to Normal mode.
                self.rename_buffer.clear();
                self.mode = Mode::Normal;
                InputAction::Execute(RemuxCommand::PaneRenameCancel)
            }
            KeyCode::Enter => {
                // Confirm rename: send the command with the buffer contents.
                let name = self.rename_buffer.clone();
                self.rename_buffer.clear();
                self.mode = Mode::Normal;
                InputAction::Execute(RemuxCommand::PaneRename(name))
            }
            KeyCode::Backspace => {
                self.rename_buffer.pop();
                InputAction::RenameUpdate(self.rename_buffer.clone())
            }
            KeyCode::Char(c) => {
                self.rename_buffer.push(c);
                InputAction::RenameUpdate(self.rename_buffer.clone())
            }
            _ => InputAction::None,
        }
    }
}

// ---------------------------------------------------------------------------
// Key event -> byte conversion (for Insert mode PTY forwarding)
// ---------------------------------------------------------------------------

/// Convert a crossterm `KeyEvent` to the byte sequence that should be sent to
/// a PTY. Returns `None` for key events that have no PTY representation.
fn key_event_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char(c) if ctrl => {
            // Ctrl+A..Z -> 0x01..0x1A
            let byte = c.to_ascii_lowercase();
            if byte.is_ascii_lowercase() {
                let ctrl_byte = byte as u8 - b'a' + 1;
                Some(wrap_alt(alt, ctrl_byte))
            } else {
                Option::None
            }
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            if alt {
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(s.as_bytes());
                Some(bytes)
            } else {
                Some(s.as_bytes().to_vec())
            }
        }
        KeyCode::Enter => Some(wrap_alt(alt, b'\r')),
        KeyCode::Tab => Some(wrap_alt(alt, b'\t')),
        KeyCode::Backspace => Some(wrap_alt(alt, 0x7f)),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(escape_seq(alt, b"[A")),
        KeyCode::Down => Some(escape_seq(alt, b"[B")),
        KeyCode::Right => Some(escape_seq(alt, b"[C")),
        KeyCode::Left => Some(escape_seq(alt, b"[D")),
        KeyCode::Home => Some(escape_seq(alt, b"[H")),
        KeyCode::End => Some(escape_seq(alt, b"[F")),
        KeyCode::PageUp => Some(escape_seq(alt, b"[5~")),
        KeyCode::PageDown => Some(escape_seq(alt, b"[6~")),
        KeyCode::Insert => Some(escape_seq(alt, b"[2~")),
        KeyCode::Delete => Some(escape_seq(alt, b"[3~")),
        KeyCode::F(n) => {
            let seq = match n {
                1 => b"OP".as_slice(),
                2 => b"OQ",
                3 => b"OR",
                4 => b"OS",
                5 => b"[15~",
                6 => b"[17~",
                7 => b"[18~",
                8 => b"[19~",
                9 => b"[20~",
                10 => b"[21~",
                11 => b"[23~",
                12 => b"[24~",
                _ => return Option::None,
            };
            Some(escape_seq(alt, seq))
        }
        _ => Option::None,
    }
}

/// Wrap a single byte with an optional Alt prefix (ESC).
fn wrap_alt(alt: bool, byte: u8) -> Vec<u8> {
    if alt {
        vec![0x1b, byte]
    } else {
        vec![byte]
    }
}

/// Build an escape sequence, optionally prefixed with ESC for Alt.
fn escape_seq(alt: bool, suffix: &[u8]) -> Vec<u8> {
    let mut seq = Vec::with_capacity(if alt { 1 } else { 0 } + 1 + suffix.len());
    if alt {
        seq.push(0x1b);
    }
    seq.push(0x1b);
    seq.extend_from_slice(suffix);
    seq
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn make_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn char_key(c: char) -> KeyEvent {
        make_key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn esc_key() -> KeyEvent {
        make_key(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn enter_key() -> KeyEvent {
        make_key(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn ctrl_key(c: char) -> KeyEvent {
        make_key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    // -- Mode transitions ---------------------------------------------------

    #[test]
    fn insert_to_normal_on_esc() {
        let mut handler = InputHandler::with_defaults();
        assert_eq!(handler.mode, Mode::Insert);

        let action = handler.handle_key(esc_key());
        assert_eq!(handler.mode, Mode::Normal);
        assert_eq!(action, InputAction::ModeChanged(Mode::Normal));
    }

    #[test]
    fn normal_to_insert_on_i() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Normal;

        let action = handler.handle_key(char_key('i'));
        assert_eq!(handler.mode, Mode::Insert);
        assert_eq!(action, InputAction::ModeChanged(Mode::Insert));
    }

    #[test]
    fn normal_to_insert_on_enter() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Normal;

        let action = handler.handle_key(enter_key());
        assert_eq!(handler.mode, Mode::Insert);
        assert_eq!(action, InputAction::ModeChanged(Mode::Insert));
    }

    #[test]
    fn normal_to_visual_on_v() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Normal;

        let action = handler.handle_key(char_key('v'));
        assert_eq!(handler.mode, Mode::Visual);
        assert_eq!(action, InputAction::ModeChanged(Mode::Visual));
    }

    #[test]
    fn visual_to_normal_on_esc() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 1000);

        let action = handler.handle_key(esc_key());
        assert_eq!(handler.mode, Mode::Normal);
        assert_eq!(action, InputAction::ModeChanged(Mode::Normal));
        assert!(handler.visual_state.is_none());
    }

    // -- Insert mode --------------------------------------------------------

    #[test]
    fn insert_mode_sends_char_to_pty() {
        let mut handler = InputHandler::with_defaults();
        let action = handler.handle_key(char_key('a'));
        assert_eq!(action, InputAction::SendToPty(b"a".to_vec()));
    }

    #[test]
    fn insert_mode_sends_enter_to_pty() {
        let mut handler = InputHandler::with_defaults();
        let action = handler.handle_key(enter_key());
        assert_eq!(action, InputAction::SendToPty(vec![b'\r']));
    }

    #[test]
    fn insert_mode_sends_ctrl_c_to_pty() {
        let mut handler = InputHandler::with_defaults();
        let key = make_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = handler.handle_key(key);
        assert_eq!(action, InputAction::SendToPty(vec![0x03]));
    }

    #[test]
    fn insert_mode_sends_arrow_keys() {
        let mut handler = InputHandler::with_defaults();
        let action = handler.handle_key(make_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(action, InputAction::SendToPty(vec![0x1b, b'[', b'A']));
    }

    // -- Normal mode keybinding sequences -----------------------------------

    #[test]
    fn normal_mode_group_shows_which_key() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Normal;

        let action = handler.handle_key(char_key('t'));
        match action {
            InputAction::ShowWhichKey(label, children) => {
                assert_eq!(label, "Tab");
                assert!(!children.is_empty());
            }
            other => panic!("expected ShowWhichKey, got {other:?}"),
        }
    }

    #[test]
    fn normal_mode_full_sequence_executes() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Normal;

        // Press 't' to enter tab group.
        let _ = handler.handle_key(char_key('t'));
        // Press 'n' to execute tab:new.
        let action = handler.handle_key(char_key('n'));
        assert_eq!(action, InputAction::Execute(RemuxCommand::TabNew));
    }

    #[test]
    fn normal_mode_esc_cancels_partial_sequence() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Normal;

        // Enter a group.
        let _ = handler.handle_key(char_key('t'));
        // Press Esc to cancel.
        let action = handler.handle_key(esc_key());
        assert_eq!(action, InputAction::HideWhichKey);
        assert!(handler.keybinding_state.is_at_root());
    }

    #[test]
    fn normal_mode_unknown_key_resets() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Normal;

        let action = handler.handle_key(char_key('z'));
        assert_eq!(action, InputAction::HideWhichKey);
        assert!(handler.keybinding_state.is_at_root());
    }

    #[test]
    fn normal_mode_esc_at_root_is_none() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Normal;

        let action = handler.handle_key(esc_key());
        assert_eq!(action, InputAction::None);
    }

    // -- Visual mode --------------------------------------------------------

    #[test]
    fn visual_mode_j_scrolls_down() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);

        let action = handler.handle_key(char_key('j'));
        assert!(matches!(action, InputAction::VisualScroll { .. }));
    }

    #[test]
    fn visual_mode_k_scrolls_up() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        // Move cursor to top first, then scroll.
        for _ in 0..24 {
            handler.handle_key(char_key('k'));
        }
        let action = handler.handle_key(char_key('k'));
        assert!(matches!(action, InputAction::VisualScroll { .. }));
    }

    #[test]
    fn visual_mode_gg_jumps_to_top() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);

        let action = handler.handle_key(char_key('g'));
        assert_eq!(action, InputAction::None); // pending
        let action = handler.handle_key(char_key('g'));
        match action {
            InputAction::VisualScroll { offset } => {
                // Should be at max offset.
                assert_eq!(offset, 76); // 100 - 24
            }
            other => panic!("expected VisualScroll, got {other:?}"),
        }
    }

    #[test]
    fn visual_mode_big_g_jumps_to_bottom() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        // First jump to top.
        handler.handle_key(char_key('g'));
        handler.handle_key(char_key('g'));
        // Then jump to bottom.
        let action = handler.handle_key(char_key('G'));
        match action {
            InputAction::VisualScroll { offset } => {
                assert_eq!(offset, 0);
            }
            other => panic!("expected VisualScroll, got {other:?}"),
        }
    }

    #[test]
    fn visual_mode_ctrl_d_half_page_down() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        // First scroll up.
        handler.handle_key(char_key('g'));
        handler.handle_key(char_key('g'));
        let action = handler.handle_key(ctrl_key('d'));
        assert!(matches!(action, InputAction::VisualScroll { .. }));
    }

    #[test]
    fn visual_mode_ctrl_u_half_page_up() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        let action = handler.handle_key(ctrl_key('u'));
        assert!(matches!(action, InputAction::VisualScroll { .. }));
    }

    #[test]
    fn visual_mode_y_yanks() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        let action = handler.handle_key(char_key('y'));
        assert_eq!(action, InputAction::YankToClipboard(String::new()));
    }

    #[test]
    fn visual_mode_slash_opens_search() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        let action = handler.handle_key(char_key('/'));
        assert_eq!(action, InputAction::SearchPrompt);
    }

    #[test]
    fn visual_mode_e_opens_editor() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        let action = handler.handle_key(char_key('e'));
        assert_eq!(action, InputAction::EditInEditor);
    }

    #[test]
    fn visual_mode_v_toggles_char_selection() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        handler.handle_key(char_key('v'));
        assert_eq!(
            handler.visual_state.as_ref().unwrap().selection_mode,
            SelectionMode::Character
        );
        handler.handle_key(char_key('v'));
        assert_eq!(
            handler.visual_state.as_ref().unwrap().selection_mode,
            SelectionMode::None
        );
    }

    #[test]
    fn visual_mode_big_v_toggles_line_selection() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        handler.handle_key(char_key('V'));
        assert_eq!(
            handler.visual_state.as_ref().unwrap().selection_mode,
            SelectionMode::Line
        );
    }

    // -- VisualState unit tests ---------------------------------------------

    #[test]
    fn visual_state_scroll_up_clamps() {
        let mut vs = VisualState::new(24, 30);
        vs.scroll_up(100);
        assert_eq!(vs.scroll_offset, 6); // max = 30 - 24
    }

    #[test]
    fn visual_state_scroll_down_clamps() {
        let mut vs = VisualState::new(24, 30);
        vs.scroll_down(100);
        assert_eq!(vs.scroll_offset, 0);
    }

    #[test]
    fn visual_state_search_match_navigation() {
        let mut vs = VisualState::new(24, 100);
        vs.search_matches = vec![(0, 0), (5, 3), (10, 7)];
        vs.current_match = 0;

        vs.next_match();
        assert_eq!(vs.current_match, 1);
        vs.next_match();
        assert_eq!(vs.current_match, 2);
        vs.next_match();
        assert_eq!(vs.current_match, 0); // wraps

        vs.prev_match();
        assert_eq!(vs.current_match, 2); // wraps back
    }

    // -- Key event to bytes -------------------------------------------------

    #[test]
    fn key_to_bytes_regular_char() {
        let key = KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_event_to_bytes(&key), Some(b"x".to_vec()));
    }

    #[test]
    fn key_to_bytes_alt_char() {
        let key = KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_event_to_bytes(&key), Some(vec![0x1b, b'x']));
    }

    #[test]
    fn key_to_bytes_function_keys() {
        let key = KeyEvent {
            code: KeyCode::F(1),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_event_to_bytes(&key), Some(vec![0x1b, b'O', b'P']));
    }

    #[test]
    fn key_to_bytes_backspace() {
        let key = KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_event_to_bytes(&key), Some(vec![0x7f]));
    }

    #[test]
    fn key_to_bytes_unicode() {
        let key = KeyEvent {
            code: KeyCode::Char('\u{00e9}'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        let bytes = key_event_to_bytes(&key).unwrap();
        assert_eq!(bytes, "\u{00e9}".as_bytes());
    }

    // -- Insert mode bindings -----------------------------------------------

    fn alt_key(c: char) -> KeyEvent {
        make_key(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn insert_mode_alt_h_executes_pane_focus_left() {
        let mut handler = InputHandler::with_defaults();
        assert_eq!(handler.mode, Mode::Insert);
        let action = handler.handle_key(alt_key('h'));
        assert_eq!(action, InputAction::Execute(RemuxCommand::PaneFocusLeft));
        // Remains in insert mode.
        assert_eq!(handler.mode, Mode::Insert);
    }

    #[test]
    fn insert_mode_alt_l_executes_pane_focus_right() {
        let mut handler = InputHandler::with_defaults();
        let action = handler.handle_key(alt_key('l'));
        assert_eq!(action, InputAction::Execute(RemuxCommand::PaneFocusRight));
        assert_eq!(handler.mode, Mode::Insert);
    }

    #[test]
    fn insert_mode_alt_n_executes_tab_next() {
        let mut handler = InputHandler::with_defaults();
        let action = handler.handle_key(alt_key('n'));
        assert_eq!(action, InputAction::Execute(RemuxCommand::TabNext));
        assert_eq!(handler.mode, Mode::Insert);
    }

    #[test]
    fn insert_mode_unbound_key_passes_to_pty() {
        let mut handler = InputHandler::with_defaults();
        // Alt-x is not bound by default.
        let action = handler.handle_key(alt_key('x'));
        // Should pass through to PTY as ESC + 'x'.
        assert_eq!(action, InputAction::SendToPty(vec![0x1b, b'x']));
        assert_eq!(handler.mode, Mode::Insert);
    }

    #[test]
    fn insert_mode_regular_char_passes_to_pty() {
        let mut handler = InputHandler::with_defaults();
        let action = handler.handle_key(char_key('a'));
        assert_eq!(action, InputAction::SendToPty(b"a".to_vec()));
        assert_eq!(handler.mode, Mode::Insert);
    }
}
