use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::client::command_palette::CommandPaletteState;
use crate::config::keybindings::{
    parse_command, InterceptAction, KeyNode, KeybindingTree, ShortcutBindings,
};
use crate::protocol::RemuxCommand;

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

/// The current input mode.
///
/// - **Normal** (default): all keys are forwarded to the PTY except the
///   leader key, which transitions to Command mode.
/// - **Command**: keybinding tree navigation (which-key).
/// - **Visual**: scrollback navigation and text selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Command,
    Visual,
    CommandPalette,
    Search,
    SessionManager,
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
    /// Number of visible columns in the pane (set by the caller).
    pub visible_cols: usize,
    /// The pane's x position in the composited screen buffer.
    pub pane_offset_x: u16,
    /// The pane's y position in the composited screen buffer.
    pub pane_offset_y: u16,
}

impl VisualState {
    /// Create a new `VisualState` positioned at the bottom of the scrollback.
    pub fn new(visible_rows: usize, total_lines: usize) -> Self {
        Self::with_cols(visible_rows, total_lines, 80)
    }

    /// Create a new `VisualState` with explicit column count.
    pub fn with_cols(visible_rows: usize, total_lines: usize, visible_cols: usize) -> Self {
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
            visible_cols,
            pane_offset_x: 0,
            pane_offset_y: 0,
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

    /// Move the cursor left by one column.
    pub fn cursor_left(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    /// Move the cursor right by one column, clamped to the visible width.
    pub fn cursor_right(&mut self, max_col: usize) {
        if self.cursor_col < max_col.saturating_sub(1) {
            self.cursor_col += 1;
        }
    }

    /// Get the cursor position in scrollback coordinates.
    pub fn scrollback_cursor_pos(&self) -> (usize, usize) {
        let row = self
            .total_lines
            .saturating_sub(self.scroll_offset + self.visible_rows)
            + self.cursor_row;
        (row, self.cursor_col)
    }

    /// Return the selection range as `(start, end)` in scrollback coordinates,
    /// ordered so that `start <= end`. Returns `None` if no selection is active.
    pub fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let start = self.selection_start?;
        if self.selection_mode == SelectionMode::None {
            return None;
        }
        let end = self.scrollback_cursor_pos();
        if start <= end {
            Some((start, end))
        } else {
            Some((end, start))
        }
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
// SearchPhase / SearchState
// ---------------------------------------------------------------------------

/// The phase of a search-mode interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPhase {
    /// The user is typing a search query.
    Prompt,
    /// The user is navigating between matches.
    Navigation,
}

/// State for Search mode.
#[derive(Debug, Clone)]
pub struct SearchState {
    /// The query buffer (characters typed so far).
    pub query_buffer: String,
    /// The confirmed query (after Enter).
    pub confirmed_query: Option<String>,
    /// Match positions as (line, column) in the scrollback content.
    pub matches: Vec<(usize, usize)>,
    /// Index of the currently highlighted match.
    pub current_match: usize,
    /// Current search phase.
    pub phase: SearchPhase,
}

impl SearchState {
    /// Create a new `SearchState` in the Prompt phase.
    pub fn new() -> Self {
        Self {
            query_buffer: String::new(),
            confirmed_query: None,
            matches: Vec::new(),
            current_match: 0,
            phase: SearchPhase::Prompt,
        }
    }

    /// Move to the next match (with wrapping).
    pub fn next_match(&mut self) {
        if !self.matches.is_empty() {
            self.current_match = (self.current_match + 1) % self.matches.len();
        }
    }

    /// Move to the previous match (with wrapping).
    pub fn prev_match(&mut self) {
        if !self.matches.is_empty() {
            self.current_match = if self.current_match == 0 {
                self.matches.len() - 1
            } else {
                self.current_match - 1
            };
        }
    }

    /// Compute substring matches given scrollback lines and a query.
    pub fn compute_matches(lines: &[String], query: &str) -> Vec<(usize, usize)> {
        if query.is_empty() {
            return Vec::new();
        }
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        for (line_idx, line) in lines.iter().enumerate() {
            let line_lower = line.to_lowercase();
            let mut start = 0;
            while let Some(pos) = line_lower[start..].find(&query_lower) {
                results.push((line_idx, start + pos));
                start += pos + 1;
            }
        }
        results
    }
}

// ---------------------------------------------------------------------------
// InputAction
// ---------------------------------------------------------------------------

/// The result of processing a single key event.
#[derive(Debug, Clone, PartialEq)]
pub enum InputAction {
    /// Send raw bytes to the active pane's PTY (Normal mode).
    SendToPty(Vec<u8>),
    /// Execute one or more Remux commands (Command/Visual mode).
    Execute(RemuxCommand),
    /// Execute an action chain (multiple commands in sequence).
    ExecuteChain(Vec<RemuxCommand>),
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
    /// Search query confirmed -- request scrollback from server.
    SearchConfirm(String),
    /// Search cancelled -- exit search mode.
    SearchCancel,
    /// Search match navigation changed -- re-render prompt.
    SearchNavigate,
    /// Update the visual mode scroll offset.
    VisualScroll { offset: usize },
    /// Activate the rename overlay for pane or tab renaming.
    ActivateRenameOverlay,
    /// The rename overlay buffer was updated (for status bar display).
    RenameUpdate(String),
    /// Open the command palette.
    CommandPaletteOpen,
    /// The command palette input/filter was updated.
    CommandPaletteUpdate,
    /// Tab completion was triggered in the command palette.
    CommandPaletteComplete,
    /// A command was selected/executed from the command palette.
    CommandPaletteExecute,
    /// The command palette was closed.
    CommandPaletteClose,
    /// Open the session manager overlay.
    SessionManagerOpen,
    /// Close the session manager overlay.
    SessionManagerClose,
    /// An action from the session manager (switch, delete, create, etc.).
    SessionManagerAction(crate::client::session_manager::SessionManagerAction),
    /// Re-render the session manager overlay (after sub-mode state changes).
    SessionManagerUpdate,
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

    /// Initialize the state at a given tree path (for entering Command mode
    /// at a non-root group via modifier shortcuts).
    pub fn set_path(&mut self, path: &[char]) {
        self.current_path = path.to_vec();
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
    /// The leader key that transitions from Normal to Command mode.
    leader_key: KeyEvent,
    /// State for multi-key Command-mode sequences.
    keybinding_state: KeybindingState,
    /// The keybinding tree (owned clone from config).
    keybinding_tree: KeybindingTree,
    /// State for Visual mode scrollback navigation.
    pub visual_state: Option<VisualState>,
    /// Pending 'g' key for gg motion in visual mode.
    pending_g: bool,
    /// Rename overlay state. When `Some`, keystrokes are captured for inline
    /// text input rather than being dispatched to the normal mode handler.
    pub rename_overlay: Option<RenameOverlay>,
    /// Modifier shortcut bindings for Normal mode (e.g. Alt-h, Alt-p).
    shortcut_bindings: ShortcutBindings,
    /// Command palette state. When `Some`, the palette overlay is active.
    pub command_palette: Option<CommandPaletteState>,
    /// State for Search mode.
    pub search_state: Option<SearchState>,
    /// State for Session Manager mode.
    pub session_manager: Option<crate::client::session_manager::SessionManagerState>,
}

/// Inline text input overlay state used for rename operations.
/// This is not a separate mode -- it sits on top of the current mode.
#[derive(Debug, Clone)]
pub struct RenameOverlay {
    /// The text buffer being edited.
    pub buffer: String,
    /// Cursor position within the buffer.
    pub cursor: usize,
    /// The command to execute when the user confirms (Enter).
    /// `PaneRename` or `TabRename`.
    pub target: RenameTarget,
}

/// What entity the rename overlay is targeting.
#[derive(Debug, Clone, PartialEq)]
pub enum RenameTarget {
    Pane,
    Tab,
}

impl InputHandler {
    /// Create a new `InputHandler` with the given keybinding tree, leader key,
    /// and modifier shortcut bindings.
    pub fn new(
        keybinding_tree: KeybindingTree,
        leader_key: KeyEvent,
        shortcut_bindings: ShortcutBindings,
    ) -> Self {
        Self {
            mode: Mode::Normal,
            leader_key,
            keybinding_state: KeybindingState::new(),
            keybinding_tree,
            visual_state: None,
            pending_g: false,
            rename_overlay: None,
            shortcut_bindings,
            command_palette: None,
            search_state: None,
            session_manager: None,
        }
    }

    /// Create a new `InputHandler` with defaults (Ctrl-a as leader, default
    /// keybinding tree).
    pub fn with_defaults() -> Self {
        use crossterm::event::{KeyEventKind, KeyEventState};
        let leader = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        Self::new(
            KeybindingTree::default(),
            leader,
            ShortcutBindings::default(),
        )
    }

    /// Process a key event and return the appropriate action.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        // If the rename overlay is active, capture keystrokes for it.
        if self.rename_overlay.is_some() {
            return self.handle_rename_overlay_key(key);
        }

        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Command => self.handle_command_key(key),
            Mode::Visual => self.handle_visual_key(key),
            Mode::CommandPalette => self.handle_command_palette_key(key),
            Mode::Search => self.handle_search_key(key),
            Mode::SessionManager => self.handle_session_manager_key(key),
        }
    }

    /// Initialize visual mode with the given scrollback dimensions.
    pub fn enter_visual_mode(&mut self, visible_rows: usize, total_lines: usize) {
        self.enter_visual_mode_with_cols(visible_rows, total_lines, 80);
    }

    /// Initialize visual mode with explicit column count.
    pub fn enter_visual_mode_with_cols(
        &mut self,
        visible_rows: usize,
        total_lines: usize,
        visible_cols: usize,
    ) {
        self.mode = Mode::Visual;
        self.visual_state = Some(VisualState::with_cols(
            visible_rows,
            total_lines,
            visible_cols,
        ));
        self.pending_g = false;
    }

    // -----------------------------------------------------------------------
    // Normal mode
    // -----------------------------------------------------------------------

    fn handle_normal_key(&mut self, key: KeyEvent) -> InputAction {
        // Check modifier shortcut bindings first.
        // Clone the action to release the immutable borrow on self before
        // calling methods that require &mut self.
        if let Some(action) = self.shortcut_bindings.lookup(&key).cloned() {
            match action {
                InterceptAction::Command(ref actions) => {
                    // Execute command chain, staying in Normal mode.
                    return self.execute_shortcut_commands(actions);
                }
                InterceptAction::GroupPrefix(ref path) => {
                    // Enter Command mode at the specified group.
                    self.mode = Mode::Command;
                    self.keybinding_state.set_path(path);
                    if let Some(children) = self.keybinding_tree.children_at(path) {
                        let label = match self.keybinding_tree.lookup(path) {
                            Some(KeyNode::Group { label, .. }) => label.clone(),
                            _ => "Remux".to_string(),
                        };
                        return InputAction::ShowWhichKey(label, children);
                    }
                    return InputAction::ModeChanged(Mode::Command);
                }
            }
        }

        // Check for leader key -- enter Command mode.
        if self.is_leader_key(&key) {
            self.mode = Mode::Command;
            // Show root-level which-key popup immediately.
            if let Some(children) = self.keybinding_tree.children_at(&[]) {
                return InputAction::ShowWhichKey("Remux".to_string(), children);
            }
            return InputAction::ModeChanged(Mode::Command);
        }

        // All other keys are forwarded to the PTY.
        match key_event_to_bytes(&key) {
            Some(bytes) => InputAction::SendToPty(bytes),
            Option::None => InputAction::None,
        }
    }

    /// Execute a shortcut command chain. Unlike `execute_action_chain`, this
    /// keeps the mode as Normal and doesn't interact with the keybinding tree
    /// state.
    fn execute_shortcut_commands(&mut self, actions: &[String]) -> InputAction {
        // Intercept mode-changing shortcuts before parsing to RemuxCommand.
        if actions.len() == 1 && actions[0] == "OpenSessionManager" {
            self.mode = Mode::SessionManager;
            self.session_manager = Some(crate::client::session_manager::SessionManagerState::new(
                None,
            ));
            return InputAction::SessionManagerOpen;
        }

        let mut commands: Vec<RemuxCommand> = Vec::new();
        for action_str in actions {
            match parse_command(action_str) {
                Some(cmd) => commands.push(cmd),
                None => log::error!("Failed to parse shortcut action: {}", action_str),
            }
        }
        match commands.len() {
            0 => InputAction::None,
            1 => InputAction::Execute(commands.remove(0)),
            _ => InputAction::ExecuteChain(commands),
        }
    }

    /// Check if a key event matches the configured leader key.
    fn is_leader_key(&self, key: &KeyEvent) -> bool {
        key.code == self.leader_key.code && key.modifiers == self.leader_key.modifiers
    }

    // -----------------------------------------------------------------------
    // Command mode
    // -----------------------------------------------------------------------

    fn handle_command_key(&mut self, key: KeyEvent) -> InputAction {
        // Escape always returns to Normal from any tree depth.
        if key.code == KeyCode::Esc {
            self.keybinding_state.reset();
            self.mode = Mode::Normal;
            return InputAction::ModeChanged(Mode::Normal);
        }

        // We only handle character keys in command mode.
        let ch = match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                c
            }
            _ => {
                // Check if this is the leader key at root (leader-leader normal mode).
                if self.keybinding_state.is_at_root() && self.is_leader_key(&key) {
                    // Look up leader-leader binding in tree.
                    // The tree should have a binding for the leader character.
                    // Fall through to the tree lookup below if it's a char key.
                    if let KeyCode::Char(c) = key.code {
                        // Let it go through normal tree lookup path with modifiers stripped
                        // But leader-leader is handled via the tree binding
                        self.keybinding_state.push(c);
                        let path = self.keybinding_state.path().to_vec();
                        return self.resolve_tree_path(&path);
                    }
                }
                return InputAction::None;
            }
        };

        self.keybinding_state.push(ch);
        let path = self.keybinding_state.path().to_vec();

        self.resolve_tree_path(&path)
    }

    /// Resolve a keybinding tree path and return the appropriate action.
    fn resolve_tree_path(&mut self, path: &[char]) -> InputAction {
        match self.keybinding_tree.lookup(path) {
            Some(KeyNode::Leaf { action, .. }) => {
                let actions = action.clone();
                self.keybinding_state.reset();
                self.execute_action_chain(&actions)
            }
            Some(KeyNode::Group { label, .. }) => {
                // We have entered a group -- show which-key popup.
                if let Some(children) = self.keybinding_tree.children_at(path) {
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
    // Action chain execution
    // -----------------------------------------------------------------------

    /// Execute an action chain. Parses each action string, handles mode
    /// transitions, and returns the appropriate InputAction.
    fn execute_action_chain(&mut self, actions: &[String]) -> InputAction {
        if actions.is_empty() {
            return InputAction::None;
        }

        let mut commands: Vec<RemuxCommand> = Vec::new();
        let mut final_action: Option<InputAction> = None;

        for action_str in actions {
            // Handle client-only actions that are not RemuxCommand variants.
            if action_str == "CommandPaletteOpen" {
                self.mode = Mode::CommandPalette;
                self.command_palette = Some(CommandPaletteState::new());
                return InputAction::CommandPaletteOpen;
            }

            match parse_command(action_str) {
                Some(cmd) => {
                    match &cmd {
                        RemuxCommand::EnterNormal => {
                            self.mode = Mode::Normal;
                            final_action = Some(InputAction::ModeChanged(Mode::Normal));
                        }
                        RemuxCommand::EnterCommandMode => {
                            self.mode = Mode::Command;
                            // No ModeChanged emitted -- we're already conceptually in command
                        }
                        RemuxCommand::EnterVisualMode => {
                            self.mode = Mode::Visual;
                            self.visual_state = Some(VisualState::with_cols(24, 1000, 80));
                            final_action = Some(InputAction::ModeChanged(Mode::Visual));
                        }
                        RemuxCommand::EnterSearchMode => {
                            self.mode = Mode::Search;
                            self.search_state = Some(SearchState::new());
                            final_action = Some(InputAction::ModeChanged(Mode::Search));
                        }
                        RemuxCommand::OpenSessionManager => {
                            self.mode = Mode::SessionManager;
                            self.session_manager = Some(
                                crate::client::session_manager::SessionManagerState::new(None),
                            );
                            return InputAction::SessionManagerOpen;
                        }
                        RemuxCommand::PaneRename(_) => {
                            // Activate rename overlay instead of executing directly.
                            self.rename_overlay = Some(RenameOverlay {
                                buffer: String::new(),
                                cursor: 0,
                                target: RenameTarget::Pane,
                            });
                            return InputAction::ActivateRenameOverlay;
                        }
                        RemuxCommand::TabRename(_) => {
                            // Activate rename overlay instead of executing directly.
                            self.rename_overlay = Some(RenameOverlay {
                                buffer: String::new(),
                                cursor: 0,
                                target: RenameTarget::Tab,
                            });
                            return InputAction::ActivateRenameOverlay;
                        }
                        _ => {
                            commands.push(cmd);
                        }
                    }
                }
                None => {
                    log::error!("Failed to parse action: {}", action_str);
                }
            }
        }

        // After chain completion, if no EnterNormal was in the chain,
        // stay in Command mode but reset tree to root.
        // (keybinding_state was already reset above)

        // If we have a mode-change as final action and commands to execute,
        // we need to return the chain.
        if let Some(mode_action) = final_action {
            if commands.is_empty() {
                return mode_action;
            }
            // Add the mode transition to the command list for the caller to handle.
            // Return ExecuteChain with all commands, the caller handles mode change
            // by checking the last ModeChanged we set on self.mode.
            // Actually, we return the chain and the caller sees self.mode changed.
            if commands.len() == 1 {
                return InputAction::Execute(commands.remove(0));
            }
            return InputAction::ExecuteChain(commands);
        }

        match commands.len() {
            0 => InputAction::None,
            1 => InputAction::Execute(commands.remove(0)),
            _ => InputAction::ExecuteChain(commands),
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
            'h' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.cursor_left();
                    return InputAction::VisualScroll {
                        offset: vs.scroll_offset,
                    };
                }
            }
            'l' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    let max_col = vs.visible_cols;
                    vs.cursor_right(max_col);
                    return InputAction::VisualScroll {
                        offset: vs.scroll_offset,
                    };
                }
            }
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
                    return InputAction::VisualScroll {
                        offset: vs.scroll_offset,
                    };
                }
                return InputAction::None;
            }
            'V' => {
                if let Some(vs) = self.visual_state.as_mut() {
                    vs.start_line_selection();
                    return InputAction::VisualScroll {
                        offset: vs.scroll_offset,
                    };
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
    // Search mode
    // -----------------------------------------------------------------------

    fn handle_search_key(&mut self, key: KeyEvent) -> InputAction {
        let search = match self.search_state.as_mut() {
            Some(s) => s,
            None => {
                // No search state -- exit to Normal.
                self.mode = Mode::Normal;
                return InputAction::ModeChanged(Mode::Normal);
            }
        };

        match search.phase {
            SearchPhase::Prompt => {
                match key.code {
                    KeyCode::Esc => {
                        // Cancel search.
                        self.search_state = None;
                        self.mode = Mode::Normal;
                        InputAction::SearchCancel
                    }
                    KeyCode::Enter => {
                        let query = search.query_buffer.clone();
                        if query.is_empty() {
                            // Empty query -- cancel.
                            self.search_state = None;
                            self.mode = Mode::Normal;
                            InputAction::SearchCancel
                        } else {
                            search.confirmed_query = Some(query.clone());
                            search.phase = SearchPhase::Navigation;
                            InputAction::SearchConfirm(query)
                        }
                    }
                    KeyCode::Backspace => {
                        search.query_buffer.pop();
                        InputAction::SearchPrompt
                    }
                    KeyCode::Char(c) => {
                        search.query_buffer.push(c);
                        InputAction::SearchPrompt
                    }
                    _ => InputAction::None,
                }
            }
            SearchPhase::Navigation => {
                match key.code {
                    KeyCode::Esc => {
                        // Exit search mode.
                        self.search_state = None;
                        self.mode = Mode::Normal;
                        InputAction::SearchCancel
                    }
                    KeyCode::Char('n') => {
                        search.next_match();
                        InputAction::SearchNavigate
                    }
                    KeyCode::Char('N') => {
                        search.prev_match();
                        InputAction::SearchNavigate
                    }
                    _ => InputAction::None,
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Session Manager mode
    // -----------------------------------------------------------------------

    fn handle_session_manager_key(&mut self, key: KeyEvent) -> InputAction {
        use crate::client::session_manager::{SessionManagerAction, SubMode};

        let sm = match self.session_manager.as_mut() {
            Some(s) => s,
            None => {
                self.mode = Mode::Normal;
                return InputAction::ModeChanged(Mode::Normal);
            }
        };

        // Handle sub-modes first.
        match &sm.sub_mode {
            SubMode::ConfirmDelete(_) => {
                return match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        let action = sm.handle_confirm_delete(true);
                        InputAction::SessionManagerAction(action)
                    }
                    _ => {
                        let _ = sm.handle_confirm_delete(false);
                        InputAction::SessionManagerUpdate
                    }
                };
            }
            SubMode::CreateFolder(_) => {
                return match key.code {
                    KeyCode::Esc => {
                        sm.sub_mode = SubMode::Navigate;
                        InputAction::SessionManagerUpdate
                    }
                    KeyCode::Enter => {
                        if let SubMode::CreateFolder(name) = &sm.sub_mode {
                            let name = name.clone();
                            sm.sub_mode = SubMode::Navigate;
                            if name.is_empty() {
                                InputAction::SessionManagerUpdate
                            } else {
                                InputAction::SessionManagerAction(
                                    SessionManagerAction::CreateFolder(name),
                                )
                            }
                        } else {
                            InputAction::None
                        }
                    }
                    KeyCode::Backspace => {
                        if let SubMode::CreateFolder(ref mut buf) = sm.sub_mode {
                            buf.pop();
                        }
                        InputAction::SessionManagerUpdate
                    }
                    KeyCode::Char(c) => {
                        if let SubMode::CreateFolder(ref mut buf) = sm.sub_mode {
                            buf.push(c);
                        }
                        InputAction::SessionManagerUpdate
                    }
                    _ => InputAction::None,
                };
            }
            SubMode::CreateSession { .. } => {
                return self.handle_session_manager_create_session_key(key);
            }
            SubMode::MoveSession { .. } => {
                return self.handle_session_manager_move_key(key);
            }
            SubMode::Navigate => {} // Fall through to navigation keys.
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.session_manager = None;
                self.mode = Mode::Normal;
                InputAction::SessionManagerClose
            }
            KeyCode::Char('j') | KeyCode::Down => {
                sm.select_next();
                InputAction::SessionManagerUpdate
            }
            KeyCode::Char('k') | KeyCode::Up => {
                sm.select_prev();
                InputAction::SessionManagerUpdate
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                let action = sm.handle_enter();
                match action {
                    SessionManagerAction::None => InputAction::SessionManagerUpdate,
                    _ => InputAction::SessionManagerAction(action),
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                sm.collapse_selected();
                InputAction::SessionManagerUpdate
            }
            KeyCode::Char('+') => {
                sm.expand_selected();
                InputAction::SessionManagerUpdate
            }
            KeyCode::Char('-') => {
                sm.collapse_selected();
                InputAction::SessionManagerUpdate
            }
            KeyCode::Char('d') => {
                let action = sm.handle_delete_key();
                match action {
                    SessionManagerAction::None => InputAction::SessionManagerUpdate,
                    _ => InputAction::SessionManagerAction(action),
                }
            }
            KeyCode::Char('c') => {
                sm.handle_create_folder_key();
                InputAction::SessionManagerUpdate
            }
            KeyCode::Char('n') => {
                sm.handle_create_session_key();
                InputAction::SessionManagerUpdate
            }
            KeyCode::Char('m') => {
                sm.handle_move_key();
                InputAction::SessionManagerUpdate
            }
            _ => InputAction::None,
        }
    }

    fn handle_session_manager_create_session_key(&mut self, key: KeyEvent) -> InputAction {
        use crate::client::session_manager::{CreatePhase, SessionManagerAction, SubMode};

        let sm = match self.session_manager.as_mut() {
            Some(s) => s,
            None => return InputAction::None,
        };

        // Determine current phase and collect data before mutation.
        let is_enter_name = matches!(
            sm.sub_mode,
            SubMode::CreateSession {
                phase: CreatePhase::EnterName,
                ..
            }
        );
        let is_select_folder = matches!(
            sm.sub_mode,
            SubMode::CreateSession {
                phase: CreatePhase::SelectFolder { .. },
                ..
            }
        );

        if is_enter_name {
            match key.code {
                KeyCode::Esc => {
                    sm.sub_mode = SubMode::Navigate;
                    InputAction::SessionManagerUpdate
                }
                KeyCode::Enter => {
                    // Extract name and folder list before mutation.
                    let current_name = if let SubMode::CreateSession { ref name, .. } = sm.sub_mode
                    {
                        name.clone()
                    } else {
                        String::new()
                    };
                    if current_name.is_empty() {
                        sm.sub_mode = SubMode::Navigate;
                        InputAction::SessionManagerUpdate
                    } else {
                        let mut folders = sm.folder_names();
                        folders.sort();
                        folders.insert(0, "(none)".to_string());
                        sm.sub_mode = SubMode::CreateSession {
                            name: current_name,
                            phase: CreatePhase::SelectFolder {
                                folders,
                                selected: 0,
                            },
                        };
                        InputAction::SessionManagerUpdate
                    }
                }
                KeyCode::Backspace => {
                    if let SubMode::CreateSession { ref mut name, .. } = sm.sub_mode {
                        name.pop();
                    }
                    InputAction::SessionManagerUpdate
                }
                KeyCode::Char(c) => {
                    if let SubMode::CreateSession { ref mut name, .. } = sm.sub_mode {
                        name.push(c);
                    }
                    InputAction::SessionManagerUpdate
                }
                _ => InputAction::None,
            }
        } else if is_select_folder {
            match key.code {
                KeyCode::Esc => {
                    sm.sub_mode = SubMode::Navigate;
                    InputAction::SessionManagerUpdate
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let SubMode::CreateSession {
                        phase:
                            CreatePhase::SelectFolder {
                                ref folders,
                                ref mut selected,
                            },
                        ..
                    } = sm.sub_mode
                    {
                        if !folders.is_empty() {
                            *selected = (*selected + 1) % folders.len();
                        }
                    }
                    InputAction::SessionManagerUpdate
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let SubMode::CreateSession {
                        phase:
                            CreatePhase::SelectFolder {
                                ref folders,
                                ref mut selected,
                            },
                        ..
                    } = sm.sub_mode
                    {
                        if !folders.is_empty() {
                            if *selected == 0 {
                                *selected = folders.len() - 1;
                            } else {
                                *selected -= 1;
                            }
                        }
                    }
                    InputAction::SessionManagerUpdate
                }
                KeyCode::Enter => {
                    let (session_name, folder) = if let SubMode::CreateSession {
                        ref name,
                        phase:
                            CreatePhase::SelectFolder {
                                ref folders,
                                selected,
                            },
                    } = sm.sub_mode
                    {
                        let folder_name = folders.get(selected).cloned();
                        let folder =
                            folder_name.and_then(|f| if f == "(none)" { None } else { Some(f) });
                        (name.clone(), folder)
                    } else {
                        (String::new(), None)
                    };
                    sm.sub_mode = SubMode::Navigate;
                    InputAction::SessionManagerAction(SessionManagerAction::CreateSession {
                        name: session_name,
                        folder,
                    })
                }
                _ => InputAction::None,
            }
        } else {
            InputAction::None
        }
    }

    fn handle_session_manager_move_key(&mut self, key: KeyEvent) -> InputAction {
        use crate::client::session_manager::{SessionManagerAction, SubMode};

        let sm = match self.session_manager.as_mut() {
            Some(s) => s,
            None => return InputAction::None,
        };

        if let SubMode::MoveSession {
            ref session,
            ref folders,
            ref mut selected,
        } = sm.sub_mode
        {
            match key.code {
                KeyCode::Esc => {
                    sm.sub_mode = SubMode::Navigate;
                    InputAction::SessionManagerUpdate
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if !folders.is_empty() {
                        *selected = (*selected + 1) % folders.len();
                    }
                    InputAction::SessionManagerUpdate
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !folders.is_empty() {
                        if *selected == 0 {
                            *selected = folders.len() - 1;
                        } else {
                            *selected -= 1;
                        }
                    }
                    InputAction::SessionManagerUpdate
                }
                KeyCode::Enter => {
                    let folder_name = folders.get(*selected).cloned();
                    let folder =
                        folder_name.and_then(|f| if f == "(none)" { None } else { Some(f) });
                    let session_name = session.clone();
                    sm.sub_mode = SubMode::Navigate;
                    InputAction::SessionManagerAction(SessionManagerAction::MoveSession {
                        session: session_name,
                        folder,
                    })
                }
                _ => InputAction::None,
            }
        } else {
            InputAction::None
        }
    }

    // -----------------------------------------------------------------------
    // Rename overlay
    // -----------------------------------------------------------------------

    fn handle_rename_overlay_key(&mut self, key: KeyEvent) -> InputAction {
        let overlay = match self.rename_overlay.as_mut() {
            Some(o) => o,
            None => return InputAction::None,
        };

        match key.code {
            KeyCode::Esc => {
                // Cancel rename, close overlay.
                self.rename_overlay = None;
                // Return to Normal after cancelling.
                self.mode = Mode::Normal;
                InputAction::ModeChanged(Mode::Normal)
            }
            KeyCode::Enter => {
                // Confirm rename: send the appropriate command.
                let name = overlay.buffer.clone();
                let target = overlay.target.clone();
                self.rename_overlay = None;
                self.mode = Mode::Normal;
                let cmd = match target {
                    RenameTarget::Pane => RemuxCommand::PaneRename(name),
                    RenameTarget::Tab => RemuxCommand::TabRename(name),
                };
                InputAction::Execute(cmd)
            }
            KeyCode::Backspace => {
                overlay.buffer.pop();
                if overlay.cursor > 0 {
                    overlay.cursor -= 1;
                }
                InputAction::RenameUpdate(overlay.buffer.clone())
            }
            KeyCode::Char(c) => {
                overlay.buffer.push(c);
                overlay.cursor += 1;
                InputAction::RenameUpdate(overlay.buffer.clone())
            }
            _ => InputAction::None,
        }
    }

    // -----------------------------------------------------------------------
    // Command palette
    // -----------------------------------------------------------------------

    fn handle_command_palette_key(&mut self, key: KeyEvent) -> InputAction {
        let palette = match self.command_palette.as_mut() {
            Some(p) => p,
            None => return InputAction::None,
        };

        match key.code {
            KeyCode::Esc => {
                self.command_palette = None;
                self.mode = Mode::Command;
                InputAction::CommandPaletteClose
            }
            KeyCode::Enter => {
                let input = palette.current_input();
                self.command_palette = None;
                self.mode = Mode::Normal;
                if let Some(cmd) = parse_command(&input) {
                    // Check for mode-transition commands.
                    match &cmd {
                        RemuxCommand::EnterNormal => {
                            self.mode = Mode::Normal;
                        }
                        RemuxCommand::EnterCommandMode => {
                            self.mode = Mode::Command;
                        }
                        RemuxCommand::EnterVisualMode => {
                            self.mode = Mode::Visual;
                            self.visual_state = Some(VisualState::with_cols(24, 1000, 80));
                        }
                        _ => {}
                    }
                    InputAction::Execute(cmd)
                } else {
                    InputAction::CommandPaletteClose
                }
            }
            KeyCode::Tab => {
                palette.tab_complete(false);
                InputAction::CommandPaletteComplete
            }
            KeyCode::BackTab => {
                palette.tab_complete(true);
                InputAction::CommandPaletteComplete
            }
            KeyCode::Backspace => {
                palette.backspace();
                InputAction::CommandPaletteUpdate
            }
            KeyCode::Up => {
                palette.select_prev();
                InputAction::CommandPaletteUpdate
            }
            KeyCode::Down => {
                palette.select_next();
                InputAction::CommandPaletteUpdate
            }
            KeyCode::Char(c) => {
                palette.insert_char(c);
                InputAction::CommandPaletteUpdate
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
    fn normal_to_command_on_leader() {
        let mut handler = InputHandler::with_defaults();
        assert_eq!(handler.mode, Mode::Normal);

        // Default leader is Ctrl-a. Now shows which-key popup at root.
        let action = handler.handle_key(ctrl_key('a'));
        assert_eq!(handler.mode, Mode::Command);
        assert!(matches!(action, InputAction::ShowWhichKey(..)));
    }

    #[test]
    fn command_to_normal_on_esc() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

        let action = handler.handle_key(esc_key());
        assert_eq!(handler.mode, Mode::Normal);
        assert_eq!(action, InputAction::ModeChanged(Mode::Normal));
    }

    #[test]
    fn command_to_visual_on_v() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

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

    // -- Normal mode --------------------------------------------------------

    #[test]
    fn normal_sends_char_to_pty() {
        let mut handler = InputHandler::with_defaults();
        let action = handler.handle_key(char_key('a'));
        assert_eq!(action, InputAction::SendToPty(b"a".to_vec()));
    }

    #[test]
    fn normal_sends_enter_to_pty() {
        let mut handler = InputHandler::with_defaults();
        let action = handler.handle_key(enter_key());
        assert_eq!(action, InputAction::SendToPty(vec![b'\r']));
    }

    #[test]
    fn normal_sends_ctrl_c_to_pty() {
        let mut handler = InputHandler::with_defaults();
        let key = make_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = handler.handle_key(key);
        assert_eq!(action, InputAction::SendToPty(vec![0x03]));
    }

    #[test]
    fn normal_sends_arrow_keys() {
        let mut handler = InputHandler::with_defaults();
        let action = handler.handle_key(make_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(action, InputAction::SendToPty(vec![0x1b, b'[', b'A']));
    }

    #[test]
    fn normal_leader_enters_command() {
        let mut handler = InputHandler::with_defaults();
        assert_eq!(handler.mode, Mode::Normal);
        // Ctrl-a is the default leader. Now shows which-key popup at root.
        let action = handler.handle_key(ctrl_key('a'));
        assert_eq!(handler.mode, Mode::Command);
        assert!(matches!(action, InputAction::ShowWhichKey(..)));
    }

    #[test]
    fn normal_non_leader_ctrl_passes_to_pty() {
        let mut handler = InputHandler::with_defaults();
        // Ctrl-b is not the leader, should pass through.
        let action = handler.handle_key(ctrl_key('b'));
        assert_eq!(action, InputAction::SendToPty(vec![0x02]));
        assert_eq!(handler.mode, Mode::Normal);
    }

    // -- Command mode keybinding sequences ----------------------------------

    #[test]
    fn command_mode_group_shows_which_key() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

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
    fn command_mode_full_sequence_executes() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

        // Press 't' to enter tab group.
        let _ = handler.handle_key(char_key('t'));
        // Press 'n' to execute tab:new. Default is action chain ["TabNew", "EnterNormal"].
        let action = handler.handle_key(char_key('n'));
        // The chain should execute TabNew and transition to Normal.
        assert_eq!(action, InputAction::Execute(RemuxCommand::TabNew));
        assert_eq!(handler.mode, Mode::Normal);
    }

    #[test]
    fn command_mode_esc_cancels_partial_sequence() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

        // Enter a group.
        let _ = handler.handle_key(char_key('t'));
        // Press Esc to cancel and return to Normal.
        let action = handler.handle_key(esc_key());
        assert_eq!(action, InputAction::ModeChanged(Mode::Normal));
        assert!(handler.keybinding_state.is_at_root());
        assert_eq!(handler.mode, Mode::Normal);
    }

    #[test]
    fn command_mode_unknown_key_resets() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

        let action = handler.handle_key(char_key('z'));
        assert_eq!(action, InputAction::HideWhichKey);
        assert!(handler.keybinding_state.is_at_root());
    }

    #[test]
    fn command_mode_esc_at_root_returns_to_normal() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

        let action = handler.handle_key(esc_key());
        assert_eq!(handler.mode, Mode::Normal);
        assert_eq!(action, InputAction::ModeChanged(Mode::Normal));
    }

    #[test]
    fn command_mode_esc_at_any_depth_returns_to_normal() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

        // Enter a group.
        let _ = handler.handle_key(char_key('t'));
        // Esc should return to Normal even from inside a group.
        let action = handler.handle_key(esc_key());
        assert_eq!(handler.mode, Mode::Normal);
        assert_eq!(action, InputAction::ModeChanged(Mode::Normal));
        assert!(handler.keybinding_state.is_at_root());
    }

    // -- Action chain tests -------------------------------------------------

    #[test]
    fn action_chain_single_command() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

        // Session detach is a single action (no EnterNormal in chain).
        // Session group is now under 'x'.
        let _ = handler.handle_key(char_key('x'));
        let action = handler.handle_key(char_key('d'));
        assert_eq!(action, InputAction::Execute(RemuxCommand::SessionDetach));
        // Should stay in Command mode since no EnterNormal in chain.
        assert_eq!(handler.mode, Mode::Command);
    }

    #[test]
    fn action_chain_with_enter_normal() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;

        // Tab new has chain ["TabNew", "EnterNormal"].
        let _ = handler.handle_key(char_key('t'));
        let action = handler.handle_key(char_key('n'));
        assert_eq!(action, InputAction::Execute(RemuxCommand::TabNew));
        assert_eq!(handler.mode, Mode::Normal);
    }

    #[test]
    fn leader_leader_sends_raw_key() {
        let mut handler = InputHandler::with_defaults();
        // Manually add leader-leader binding (Ctrl-a = 'a' with ctrl).
        // In the default tree, we need to insert the leader-leader binding.
        handler.keybinding_tree.root.insert(
            'a',
            crate::config::keybindings::KeyNode::Leaf {
                label: "send leader".to_string(),
                action: vec!["SendKey Ctrl-a".to_string(), "EnterNormal".to_string()],
            },
        );
        handler.mode = Mode::Command;

        // Press 'a' (the leader char without modifiers, which is how it's
        // looked up in the tree).
        let action = handler.handle_key(char_key('a'));
        // Should send the raw Ctrl-a byte (0x01) and switch to normal.
        assert_eq!(
            action,
            InputAction::Execute(RemuxCommand::SendKey(vec![0x01]))
        );
        assert_eq!(handler.mode, Mode::Normal);
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

    #[test]
    fn visual_mode_h_moves_cursor_left() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        // Move right first, then left.
        handler.handle_key(char_key('l'));
        handler.handle_key(char_key('l'));
        let vs = handler.visual_state.as_ref().unwrap();
        assert_eq!(vs.cursor_col, 2);

        handler.handle_key(char_key('h'));
        let vs = handler.visual_state.as_ref().unwrap();
        assert_eq!(vs.cursor_col, 1);
    }

    #[test]
    fn visual_mode_l_moves_cursor_right() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);

        let action = handler.handle_key(char_key('l'));
        assert!(matches!(action, InputAction::VisualScroll { .. }));
        let vs = handler.visual_state.as_ref().unwrap();
        assert_eq!(vs.cursor_col, 1);
    }

    #[test]
    fn visual_mode_h_clamps_at_zero() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);

        // Already at col 0, moving left should stay at 0.
        handler.handle_key(char_key('h'));
        let vs = handler.visual_state.as_ref().unwrap();
        assert_eq!(vs.cursor_col, 0);
    }

    #[test]
    fn visual_mode_v_returns_visual_scroll() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        let action = handler.handle_key(char_key('v'));
        // Should return VisualScroll so the overlay gets redrawn.
        assert!(matches!(action, InputAction::VisualScroll { .. }));
    }

    #[test]
    fn visual_mode_big_v_returns_visual_scroll() {
        let mut handler = InputHandler::with_defaults();
        handler.enter_visual_mode(24, 100);
        let action = handler.handle_key(char_key('V'));
        assert!(matches!(action, InputAction::VisualScroll { .. }));
    }

    // -- VisualState unit tests ---------------------------------------------

    #[test]
    fn visual_state_cursor_left_clamps() {
        let mut vs = VisualState::new(24, 100);
        assert_eq!(vs.cursor_col, 0);
        vs.cursor_left();
        assert_eq!(vs.cursor_col, 0);
    }

    #[test]
    fn visual_state_cursor_right_clamps() {
        let mut vs = VisualState::with_cols(24, 100, 10);
        for _ in 0..20 {
            vs.cursor_right(vs.visible_cols);
        }
        assert_eq!(vs.cursor_col, 9); // max_col - 1
    }

    #[test]
    fn visual_state_selection_range_char() {
        let mut vs = VisualState::new(24, 24);
        vs.start_char_selection();
        // Cursor starts at bottom-right-ish position.
        let start_pos = vs.scrollback_cursor_pos();
        assert!(vs.selection_start.is_some());

        // Move cursor down (should stay at bottom since we're already there).
        vs.cursor_col = 5;
        let range = vs.selection_range();
        assert!(range.is_some());
        let ((sr, sc), (er, ec)) = range.unwrap();
        assert_eq!(sr, start_pos.0);
        assert_eq!(sc, start_pos.1);
        assert_eq!(er, start_pos.0); // same row
        assert_eq!(ec, 5);
    }

    #[test]
    fn visual_state_selection_range_none_when_no_selection() {
        let vs = VisualState::new(24, 100);
        assert!(vs.selection_range().is_none());
    }

    #[test]
    fn visual_state_selection_range_orders_correctly() {
        let mut vs = VisualState::new(24, 48);
        // Move cursor to middle, start selection, then move up.
        vs.cursor_row = 12;
        vs.cursor_col = 5;
        vs.start_char_selection();
        let start_pos = vs.scrollback_cursor_pos();

        // Move cursor up.
        vs.cursor_row = 5;
        vs.cursor_col = 2;
        let end_pos = vs.scrollback_cursor_pos();

        let range = vs.selection_range();
        assert!(range.is_some());
        let ((sr, sc), (er, ec)) = range.unwrap();
        // Should be ordered: end_pos (row 5) < start_pos (row 12).
        assert_eq!((sr, sc), (end_pos.0, end_pos.1));
        assert_eq!((er, ec), (start_pos.0, start_pos.1));
    }

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

    // -- Normal mode passes all non-leader keys -------------------------

    fn alt_key(c: char) -> KeyEvent {
        make_key(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn normal_alt_keys_intercept_bindings() {
        let mut handler = InputHandler::with_defaults();
        assert_eq!(handler.mode, Mode::Normal);
        // Alt-h is intercepted by default shortcut bindings (PaneFocusLeft).
        let action = handler.handle_key(alt_key('h'));
        assert_eq!(action, InputAction::Execute(RemuxCommand::PaneFocusLeft));
        assert_eq!(handler.mode, Mode::Normal);
    }

    #[test]
    fn normal_alt_keys_unbound_pass_to_pty() {
        let mut handler = InputHandler::with_defaults();
        assert_eq!(handler.mode, Mode::Normal);
        // Alt-z has no shortcut binding, so it passes through to PTY.
        let action = handler.handle_key(alt_key('z'));
        assert_eq!(action, InputAction::SendToPty(vec![0x1b, b'z']));
        assert_eq!(handler.mode, Mode::Normal);
    }

    #[test]
    fn normal_esc_passes_to_pty() {
        let mut handler = InputHandler::with_defaults();
        assert_eq!(handler.mode, Mode::Normal);
        // Esc is not the leader, should pass through.
        let action = handler.handle_key(esc_key());
        assert_eq!(action, InputAction::SendToPty(vec![0x1b]));
        assert_eq!(handler.mode, Mode::Normal);
    }

    // -- Rename overlay tests -----------------------------------------------

    #[test]
    fn rename_overlay_enter_confirms() {
        let mut handler = InputHandler::with_defaults();
        handler.mode = Mode::Command;
        // Simulate activating rename overlay for pane.
        handler.rename_overlay = Some(RenameOverlay {
            buffer: String::new(),
            cursor: 0,
            target: RenameTarget::Pane,
        });
        // Type some text.
        handler.handle_key(char_key('t'));
        handler.handle_key(char_key('e'));
        handler.handle_key(char_key('s'));
        handler.handle_key(char_key('t'));
        // Press Enter to confirm.
        let action = handler.handle_key(enter_key());
        assert_eq!(
            action,
            InputAction::Execute(RemuxCommand::PaneRename("test".to_string()))
        );
        assert!(handler.rename_overlay.is_none());
        assert_eq!(handler.mode, Mode::Normal);
    }

    #[test]
    fn rename_overlay_esc_cancels() {
        let mut handler = InputHandler::with_defaults();
        handler.rename_overlay = Some(RenameOverlay {
            buffer: "partial".to_string(),
            cursor: 7,
            target: RenameTarget::Tab,
        });
        let action = handler.handle_key(esc_key());
        assert_eq!(action, InputAction::ModeChanged(Mode::Normal));
        assert!(handler.rename_overlay.is_none());
    }

    #[test]
    fn rename_overlay_backspace_removes_char() {
        let mut handler = InputHandler::with_defaults();
        handler.rename_overlay = Some(RenameOverlay {
            buffer: "ab".to_string(),
            cursor: 2,
            target: RenameTarget::Pane,
        });
        let action = handler.handle_key(make_key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(action, InputAction::RenameUpdate("a".to_string()));
        assert_eq!(handler.rename_overlay.as_ref().unwrap().buffer, "a");
    }

    // -- Protocol round-trip tests for new commands -------------------------

    #[test]
    fn round_trip_send_key() {
        use crate::protocol::decode_message_length;
        use crate::protocol::encode_message;
        let msg = crate::protocol::ClientMessage::Command(RemuxCommand::SendKey(vec![0x01]));
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: crate::protocol::ClientMessage =
            serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            crate::protocol::ClientMessage::Command(RemuxCommand::SendKey(bytes)) => {
                assert_eq!(bytes, vec![0x01]);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_enter_normal() {
        use crate::protocol::decode_message_length;
        use crate::protocol::encode_message;
        let msg = crate::protocol::ClientMessage::Command(RemuxCommand::EnterNormal);
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: crate::protocol::ClientMessage =
            serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            crate::protocol::ClientMessage::Command(RemuxCommand::EnterNormal) => {}
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_enter_command_mode() {
        use crate::protocol::decode_message_length;
        use crate::protocol::encode_message;
        let msg = crate::protocol::ClientMessage::Command(RemuxCommand::EnterCommandMode);
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: crate::protocol::ClientMessage =
            serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            crate::protocol::ClientMessage::Command(RemuxCommand::EnterCommandMode) => {}
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    // -- Shortcut group entry tests -----------------------------------------

    #[test]
    fn alt_p_enters_command_mode_at_pane_group() {
        let mut handler = InputHandler::with_defaults();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('p'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        let action = handler.handle_key(key);
        assert_eq!(handler.mode, Mode::Command);
        match action {
            InputAction::ShowWhichKey(label, children) => {
                assert_eq!(label, "Pane");
                let keys: Vec<char> = children.iter().map(|(k, _)| *k).collect();
                assert!(keys.contains(&'n'));
                assert!(keys.contains(&'h'));
            }
            other => panic!("expected ShowWhichKey for Pane, got {other:?}"),
        }
    }

    #[test]
    fn alt_h_executes_command_stays_normal() {
        let mut handler = InputHandler::with_defaults();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('h'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        let action = handler.handle_key(key);
        assert_eq!(handler.mode, Mode::Normal);
        assert_eq!(action, InputAction::Execute(RemuxCommand::PaneFocusLeft));
    }

    #[test]
    fn escape_from_group_shortcut_returns_to_normal() {
        let mut handler = InputHandler::with_defaults();
        // Enter command mode at Pane group via Alt-p
        let alt_p = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('p'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        handler.handle_key(alt_p);
        assert_eq!(handler.mode, Mode::Command);

        // Escape returns to Normal
        let esc = KeyEvent::new_with_kind_and_state(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        let action = handler.handle_key(esc);
        assert_eq!(handler.mode, Mode::Normal);
        assert_eq!(action, InputAction::ModeChanged(Mode::Normal));
    }

    #[test]
    fn alt_p_then_n_creates_pane() {
        let mut handler = InputHandler::with_defaults();
        // Alt-p enters Pane group
        let alt_p = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('p'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        handler.handle_key(alt_p);
        assert_eq!(handler.mode, Mode::Command);

        // 'n' creates a new pane (default: "PaneNew; EnterNormal")
        let n = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('n'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        let action = handler.handle_key(n);
        assert_eq!(action, InputAction::Execute(RemuxCommand::PaneNew));
        assert_eq!(handler.mode, Mode::Normal);
    }
}
