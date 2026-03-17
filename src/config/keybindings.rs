use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

use crate::protocol::RemuxCommand;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// A node in the keybinding tree. Either a group of sub-keys or a leaf that
/// maps to one or more action strings (an action chain).
#[derive(Debug, Clone)]
pub enum KeyNode {
    /// An intermediate group that contains sub-keys.
    Group {
        label: String,
        children: HashMap<char, KeyNode>,
    },
    /// A terminal binding that maps to an action chain.
    Leaf {
        label: String,
        /// Action chain, e.g. `["TabNew", "EnterNormal"]`.
        action: Vec<String>,
    },
}

/// The full keybinding tree used in Normal mode.
#[derive(Debug, Clone)]
pub struct KeybindingTree {
    pub root: HashMap<char, KeyNode>,
}

// ---------------------------------------------------------------------------
// Default keybinding tree
// ---------------------------------------------------------------------------

impl Default for KeybindingTree {
    fn default() -> Self {
        Self {
            root: build_default_tree(),
        }
    }
}

/// Helper to build a leaf node with a single action.
fn leaf(label: &str, action: &str) -> KeyNode {
    KeyNode::Leaf {
        label: label.to_string(),
        action: vec![action.to_string()],
    }
}

/// Helper to build a leaf node with an action chain.
pub fn leaf_chain(label: &str, actions: &[&str]) -> KeyNode {
    KeyNode::Leaf {
        label: label.to_string(),
        action: actions.iter().map(|s| s.to_string()).collect(),
    }
}

/// Helper to build a group node.
fn group(label: &str, children: Vec<(char, KeyNode)>) -> KeyNode {
    KeyNode::Group {
        label: label.to_string(),
        children: children.into_iter().collect(),
    }
}

fn build_default_tree() -> HashMap<char, KeyNode> {
    let mut root = HashMap::new();

    // t: Tab
    root.insert(
        't',
        group(
            "Tab",
            vec![
                ('n', leaf_chain("new", &["TabNew", "EnterNormal"])),
                ('c', leaf_chain("close", &["TabClose", "EnterNormal"])),
                ('m', leaf("move", "TabMove")),
                ('r', leaf("rename", "TabRename")),
                ('l', leaf_chain("list", &["TabNext", "EnterNormal"])),
            ],
        ),
    );

    // p: Pane
    root.insert(
        'p',
        group(
            "Pane",
            vec![
                ('n', leaf_chain("new", &["PaneNew", "EnterNormal"])),
                ('c', leaf_chain("close", &["PaneClose", "EnterNormal"])),
                (
                    's',
                    leaf_chain("split vertical", &["PaneSplitVertical", "EnterNormal"]),
                ),
                (
                    'S',
                    leaf_chain("split horizontal", &["PaneSplitHorizontal", "EnterNormal"]),
                ),
                (
                    'h',
                    leaf_chain("focus left", &["PaneFocusLeft", "EnterNormal"]),
                ),
                (
                    'j',
                    leaf_chain("focus down", &["PaneFocusDown", "EnterNormal"]),
                ),
                ('k', leaf_chain("focus up", &["PaneFocusUp", "EnterNormal"])),
                (
                    'l',
                    leaf_chain("focus right", &["PaneFocusRight", "EnterNormal"]),
                ),
                (
                    'a',
                    leaf_chain("stack add", &["PaneStackAdd", "EnterNormal"]),
                ),
                (
                    ']',
                    leaf_chain("stack next", &["PaneStackNext", "EnterNormal"]),
                ),
                (
                    '[',
                    leaf_chain("stack prev", &["PaneStackPrev", "EnterNormal"]),
                ),
                (
                    'r',
                    group(
                        "Resize",
                        vec![
                            ('h', leaf("left", "ResizeLeft 5")),
                            ('j', leaf("down", "ResizeDown 5")),
                            ('k', leaf("up", "ResizeUp 5")),
                            ('l', leaf("right", "ResizeRight 5")),
                        ],
                    ),
                ),
                ('R', leaf("rename", "PaneRename")),
            ],
        ),
    );

    // s: Search
    root.insert(
        's',
        group(
            "Search",
            vec![
                ('s', leaf("search", "EnterSearchMode")),
                ('e', leaf("edit in editor", "BufferEditInEditor")),
            ],
        ),
    );

    // x: Session
    root.insert(
        'x',
        group(
            "Session",
            vec![
                ('n', leaf("new", "SessionNew")),
                ('d', leaf("detach", "SessionDetach")),
                ('r', leaf("rename", "SessionRename")),
                ('l', leaf("list", "OpenSessionManager")),
                ('m', leaf("move to folder", "SessionMoveToFolder")),
            ],
        ),
    );

    // Visual mode binding.
    root.insert('v', leaf("visual mode", "EnterVisualMode"));

    // Layout toggle bindings.
    root.insert(
        'g',
        leaf_chain("toggle style", &["ToggleStyle", "EnterNormal"]),
    );

    // Layout mode bindings.
    root.insert(
        ' ',
        leaf_chain("layout next", &["LayoutNext", "EnterNormal"]),
    );
    root.insert('m', leaf_chain("set master", &["SetMaster", "EnterNormal"]));

    // Command palette.
    root.insert(':', leaf("command palette", "CommandPaletteOpen"));

    // Send the prefix key (Ctrl-a) to the terminal.
    root.insert(
        'a',
        leaf_chain("send prefix", &["SendKey Ctrl-a", "EnterNormal"]),
    );

    root
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

impl KeybindingTree {
    /// Look up the node at the given key path (e.g. `['t', 'n']` maps to the
    /// `TabNew` leaf).
    pub fn lookup(&self, path: &[char]) -> Option<&KeyNode> {
        if path.is_empty() {
            return None;
        }

        let mut current = self.root.get(&path[0])?;
        for key in &path[1..] {
            match current {
                KeyNode::Group { children, .. } => {
                    current = children.get(key)?;
                }
                KeyNode::Leaf { .. } => return None,
            }
        }
        Some(current)
    }

    /// Return the `(key, label)` pairs for all children at the given path.
    ///
    /// If `path` is empty, returns the root-level children.
    /// Returns `None` if the path does not lead to a group.
    pub fn children_at(&self, path: &[char]) -> Option<Vec<(char, String)>> {
        let children = if path.is_empty() {
            &self.root
        } else {
            match self.lookup(path)? {
                KeyNode::Group { children, .. } => children,
                KeyNode::Leaf { .. } => return None,
            }
        };

        let mut result: Vec<(char, String)> = children
            .iter()
            .map(|(&key, node)| {
                let label = match node {
                    KeyNode::Group { label, .. } | KeyNode::Leaf { label, .. } => label.clone(),
                };
                // Show ␣ (U+2423) for the space key so it's visible in which-key.
                let display_key = if key == ' ' { '␣' } else { key };
                (display_key, label)
            })
            .collect();
        result.sort_by_key(|(k, _)| *k);
        Some(result)
    }

    /// Merge another tree on top of this one. Keys in `overrides` replace keys
    /// in `self`. Groups are merged recursively; leaves are replaced outright.
    /// A leaf with an empty action string removes that key from the base.
    pub fn merge(&mut self, overrides: &KeybindingTree) {
        merge_maps(&mut self.root, &overrides.root);
    }
}

fn merge_maps(base: &mut HashMap<char, KeyNode>, overrides: &HashMap<char, KeyNode>) {
    for (key, override_node) in overrides {
        // If the override is a leaf with an empty action chain, remove the key.
        if let KeyNode::Leaf { action, .. } = override_node {
            if action.is_empty() || (action.len() == 1 && action[0].is_empty()) {
                base.remove(key);
                continue;
            }
        }

        match base.get_mut(key) {
            Some(KeyNode::Group {
                label: base_label,
                children: base_children,
            }) => {
                if let KeyNode::Group {
                    label,
                    children: override_children,
                } = override_node
                {
                    // Both are groups: update label and merge children.
                    *base_label = label.clone();
                    merge_maps(base_children, override_children);
                } else {
                    // Override is a leaf replacing a group.
                    base.insert(*key, override_node.clone());
                }
            }
            _ => {
                // Base is a leaf (or missing): override wins.
                base.insert(*key, override_node.clone());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NormalizedKeyEvent, InterceptAction, ShortcutBindings
// ---------------------------------------------------------------------------

/// A normalized key event for use as a HashMap key.
/// Strips `kind` and `state` from `crossterm::KeyEvent` to ensure consistent
/// matching across terminals.
#[derive(Debug, Clone, Copy)]
pub struct NormalizedKeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl NormalizedKeyEvent {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }
}

impl From<&KeyEvent> for NormalizedKeyEvent {
    fn from(event: &KeyEvent) -> Self {
        Self {
            code: event.code,
            modifiers: event.modifiers,
        }
    }
}

impl From<KeyEvent> for NormalizedKeyEvent {
    fn from(event: KeyEvent) -> Self {
        Self {
            code: event.code,
            modifiers: event.modifiers,
        }
    }
}

impl PartialEq for NormalizedKeyEvent {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code && self.modifiers == other.modifiers
    }
}
impl Eq for NormalizedKeyEvent {}

impl std::hash::Hash for NormalizedKeyEvent {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Hash the discriminant and inner value of KeyCode.
        std::mem::discriminant(&self.code).hash(state);
        // For Char, hash the character. For F(n), hash the number.
        match self.code {
            KeyCode::Char(c) => c.hash(state),
            KeyCode::F(n) => n.hash(state),
            _ => {}
        }
        self.modifiers.bits().hash(state);
    }
}

/// Action to perform when a modifier shortcut binding matches.
#[derive(Debug, Clone)]
pub enum InterceptAction {
    /// Execute commands and stay in Normal mode.
    Command(Vec<String>),
    /// Enter Command mode at the given keybinding tree group path.
    GroupPrefix(Vec<char>),
}

/// Modifier-based keybindings that are checked in Normal mode before
/// forwarding keys to the PTY.
#[derive(Debug, Clone)]
pub struct ShortcutBindings {
    bindings: HashMap<NormalizedKeyEvent, InterceptAction>,
}

impl ShortcutBindings {
    /// Look up a key event in the shortcut bindings.
    pub fn lookup(&self, key: &KeyEvent) -> Option<&InterceptAction> {
        let normalized = NormalizedKeyEvent::from(key);
        self.bindings.get(&normalized)
    }

    /// Merge another set of bindings on top of this one.
    /// Empty command strings remove the binding (unbind).
    pub fn merge(&mut self, overrides: &ShortcutBindings) {
        for (key, action) in &overrides.bindings {
            match action {
                InterceptAction::Command(cmds)
                    if cmds.is_empty() || (cmds.len() == 1 && cmds[0].is_empty()) =>
                {
                    self.bindings.remove(key);
                }
                _ => {
                    self.bindings.insert(*key, action.clone());
                }
            }
        }
    }

    /// Validate that all `@<key>` group prefix references resolve to actual
    /// groups in the keybinding tree. Logs errors for invalid references.
    /// Returns true if all references are valid.
    pub fn validate_group_refs(&self, tree: &KeybindingTree) -> bool {
        let mut valid = true;
        for action in self.bindings.values() {
            if let InterceptAction::GroupPrefix(path) = action {
                match tree.lookup(path) {
                    Some(KeyNode::Group { .. }) => {}
                    _ => {
                        let path_str: String = path.iter().collect();
                        log::error!(
                            "shortcut binding references invalid group '@{path_str}': \
                             no such group in keybinding tree"
                        );
                        valid = false;
                    }
                }
            }
        }
        valid
    }

    /// Parse shortcut bindings from a TOML table. Each key is a key notation
    /// string (e.g. `"Alt-h"`) and each value is either a command string
    /// (semicolon-separated action chain) or a `@`-prefixed group path
    /// (e.g. `"@p"` to enter the pane group).
    ///
    /// Keys without modifiers are rejected (shortcut bindings must use a
    /// modifier to avoid interfering with normal typing). Table values are
    /// also skipped since shortcut bindings are flat.
    pub fn from_toml(value: &toml::Value) -> Option<Self> {
        let table = value.as_table()?;
        let mut bindings = HashMap::new();

        for (key_str, val) in table {
            // Reject table values — shortcut bindings are flat.
            if val.is_table() {
                log::warn!(
                    "shortcut binding '{key_str}': nested tables are not supported, skipping"
                );
                continue;
            }

            let action_str = match val.as_str() {
                Some(s) => s,
                None => continue,
            };

            // Parse the key notation.
            let key_event = match parse_key_notation(key_str) {
                Some(ev) => ev,
                None => {
                    log::warn!("shortcut binding '{key_str}': unrecognised key notation, skipping");
                    continue;
                }
            };

            // Reject keys without modifiers (plain keys or Shift-only for chars).
            let dominated_by_shift = key_event.modifiers == KeyModifiers::NONE
                || (key_event.modifiers == KeyModifiers::SHIFT
                    && matches!(key_event.code, KeyCode::Char(_)));
            if dominated_by_shift {
                log::warn!(
                    "shortcut binding '{key_str}': modifier required (Alt, Ctrl, etc.), skipping"
                );
                continue;
            }

            let normalized = NormalizedKeyEvent::new(key_event.code, key_event.modifiers);

            let action = if let Some(group_path) = action_str.strip_prefix('@') {
                InterceptAction::GroupPrefix(group_path.chars().collect())
            } else {
                let cmds: Vec<String> = action_str
                    .split(';')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                InterceptAction::Command(cmds)
            };

            bindings.insert(normalized, action);
        }

        Some(ShortcutBindings { bindings })
    }
}

/// Helper to create a `NormalizedKeyEvent` with the Alt modifier.
fn alt_key(c: char) -> NormalizedKeyEvent {
    NormalizedKeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

impl Default for ShortcutBindings {
    fn default() -> Self {
        let mut bindings = HashMap::new();

        bindings.insert(
            alt_key('h'),
            InterceptAction::Command(vec!["PaneFocusLeft".to_string()]),
        );
        bindings.insert(
            alt_key('j'),
            InterceptAction::Command(vec!["PaneFocusDown".to_string()]),
        );
        bindings.insert(
            alt_key('k'),
            InterceptAction::Command(vec!["PaneFocusUp".to_string()]),
        );
        bindings.insert(
            alt_key('l'),
            InterceptAction::Command(vec!["PaneFocusRight".to_string()]),
        );
        bindings.insert(
            alt_key('n'),
            InterceptAction::Command(vec!["TabNext".to_string()]),
        );
        bindings.insert(alt_key('p'), InterceptAction::GroupPrefix(vec!['p']));
        bindings.insert(alt_key('t'), InterceptAction::GroupPrefix(vec!['t']));
        bindings.insert(
            NormalizedKeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            InterceptAction::Command(vec!["OpenSessionManager".to_string()]),
        );

        Self { bindings }
    }
}

// ---------------------------------------------------------------------------
// TOML config parsing
// ---------------------------------------------------------------------------

impl KeybindingTree {
    /// Parse a keybinding tree from the `[keybindings.command]` section of the
    /// TOML config. The `value` should be the table at that path.
    pub fn from_toml(value: &toml::Value) -> Option<Self> {
        let table = value.as_table()?;
        let root = parse_toml_table(table);
        Some(KeybindingTree { root })
    }
}

fn parse_toml_table(table: &toml::map::Map<String, toml::Value>) -> HashMap<char, KeyNode> {
    let mut map = HashMap::new();

    for (key, value) in table {
        // Skip metadata keys (e.g. `_label`).
        if key.starts_with('_') {
            continue;
        }

        // Key must be a single character.
        let ch = match key.chars().next() {
            Some(c) if key.len() == c.len_utf8() => c,
            _ => continue,
        };

        let node = match value {
            toml::Value::String(action_str) => {
                // Split on semicolons to support action chains in TOML.
                let actions: Vec<String> = action_str
                    .split(';')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                KeyNode::Leaf {
                    label: action_str.clone(),
                    action: actions,
                }
            }
            toml::Value::Table(sub) => {
                let sub_label = sub
                    .get("_label")
                    .and_then(|v| v.as_str())
                    .unwrap_or(key)
                    .to_string();
                let children = parse_toml_table(sub);
                KeyNode::Group {
                    label: sub_label,
                    children,
                }
            }
            _ => continue,
        };

        map.insert(ch, node);
    }

    map
}

// ---------------------------------------------------------------------------
// Command string -> RemuxCommand parsing
// ---------------------------------------------------------------------------

/// Parse a whitespace-separated token list from a command string, handling
/// double-quoted arguments (for args containing spaces).
fn tokenize_command(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }

        if c == '"' {
            // Consume the opening quote.
            chars.next();
            let mut token = String::new();
            while let Some(&inner) = chars.peek() {
                if inner == '"' {
                    chars.next();
                    break;
                }
                token.push(inner);
                chars.next();
            }
            tokens.push(token);
        } else {
            let mut token = String::new();
            while let Some(&inner) = chars.peek() {
                if inner.is_whitespace() {
                    break;
                }
                token.push(inner);
                chars.next();
            }
            tokens.push(token);
        }
    }

    tokens
}

/// Parse a PascalCase command string (e.g. `"TabNew"`, `"ResizeLeft 5"`) into
/// a [`RemuxCommand`].
///
/// Returns `None` if the command string is not recognised.
pub fn parse_command(input: &str) -> Option<RemuxCommand> {
    let tokens = tokenize_command(input);
    let name = tokens.first()?;
    let args = &tokens[1..];

    match name.as_str() {
        // -- No-arg commands --------------------------------------------------
        "TabNew" => Some(RemuxCommand::TabNew),
        "TabClose" => Some(RemuxCommand::TabClose),
        "TabNext" => Some(RemuxCommand::TabNext),
        "TabPrev" => Some(RemuxCommand::TabPrev),
        "PaneNew" => Some(RemuxCommand::PaneNew),
        "PaneClose" => Some(RemuxCommand::PaneClose),
        "PaneSplitVertical" => Some(RemuxCommand::PaneSplitVertical),
        "PaneSplitHorizontal" => Some(RemuxCommand::PaneSplitHorizontal),
        "PaneFocusLeft" => Some(RemuxCommand::PaneFocusLeft),
        "PaneFocusRight" => Some(RemuxCommand::PaneFocusRight),
        "PaneFocusUp" => Some(RemuxCommand::PaneFocusUp),
        "PaneFocusDown" => Some(RemuxCommand::PaneFocusDown),
        "PaneStackAdd" => Some(RemuxCommand::PaneStackAdd),
        "PaneStackNext" => Some(RemuxCommand::PaneStackNext),
        "PaneStackPrev" => Some(RemuxCommand::PaneStackPrev),
        "SessionDetach" => Some(RemuxCommand::SessionDetach),
        "SessionList" => Some(RemuxCommand::SessionList),
        "FolderList" => Some(RemuxCommand::FolderList),
        "BufferEditInEditor" => Some(RemuxCommand::BufferEditInEditor),
        "EnterSearchMode" => Some(RemuxCommand::EnterSearchMode),
        "OpenSessionManager" => Some(RemuxCommand::OpenSessionManager),
        "SessionMoveToFolder" => Some(RemuxCommand::SessionMoveToFolder),
        "ToggleStyle" => Some(RemuxCommand::ToggleStyle),
        "LayoutNext" => Some(RemuxCommand::LayoutNext),
        "SetMaster" => Some(RemuxCommand::SetMaster),
        "SessionSave" => Some(RemuxCommand::SessionSave),
        "EnterNormal" => Some(RemuxCommand::EnterNormal),
        "EnterCommandMode" => Some(RemuxCommand::EnterCommandMode),
        "EnterVisualMode" => Some(RemuxCommand::EnterVisualMode),
        "SendKey" => {
            // SendKey takes a key notation argument and converts to bytes.
            let key_notation = args.first().map(|s| s.as_str()).unwrap_or("");
            if let Some(key_event) = parse_key_notation(key_notation) {
                // Convert key event to raw bytes for the PTY.
                let ctrl = key_event.modifiers.contains(KeyModifiers::CONTROL);
                let bytes = if ctrl {
                    if let KeyCode::Char(c) = key_event.code {
                        let byte = c.to_ascii_lowercase();
                        if byte.is_ascii_lowercase() {
                            vec![byte as u8 - b'a' + 1]
                        } else {
                            vec![]
                        }
                    } else {
                        vec![]
                    }
                } else if let KeyCode::Char(c) = key_event.code {
                    let mut buf = [0u8; 4];
                    c.encode_utf8(&mut buf);
                    buf[..c.len_utf8()].to_vec()
                } else {
                    vec![]
                };
                Some(RemuxCommand::SendKey(bytes))
            } else {
                None
            }
        }

        // -- String arg commands ----------------------------------------------
        "TabRename" => {
            let name = args.first().map(|s| s.to_string()).unwrap_or_default();
            Some(RemuxCommand::TabRename(name))
        }
        "SessionRename" => {
            let name = args.first().map(|s| s.to_string()).unwrap_or_default();
            Some(RemuxCommand::SessionRename(name))
        }
        "FolderNew" => {
            let name = args.first().map(|s| s.to_string()).unwrap_or_default();
            Some(RemuxCommand::FolderNew(name))
        }
        "FolderDelete" => {
            let name = args.first().map(|s| s.to_string()).unwrap_or_default();
            Some(RemuxCommand::FolderDelete(name))
        }
        "PaneRename" => {
            let name = args.first().map(|s| s.to_string()).unwrap_or_default();
            Some(RemuxCommand::PaneRename(name))
        }

        // -- usize arg commands -----------------------------------------------
        "TabGoto" => {
            let idx = args.first()?.parse().ok()?;
            Some(RemuxCommand::TabGoto(idx))
        }
        "TabMove" => {
            let idx = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            Some(RemuxCommand::TabMove(idx))
        }

        // -- u16 arg commands (default 1) -------------------------------------
        "ResizeLeft" => {
            let amount = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
            Some(RemuxCommand::ResizeLeft(amount))
        }
        "ResizeRight" => {
            let amount = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
            Some(RemuxCommand::ResizeRight(amount))
        }
        "ResizeUp" => {
            let amount = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
            Some(RemuxCommand::ResizeUp(amount))
        }
        "ResizeDown" => {
            let amount = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
            Some(RemuxCommand::ResizeDown(amount))
        }

        // -- Named-field commands ---------------------------------------------
        "SessionNew" => {
            let name = args.first().map(|s| s.to_string()).unwrap_or_default();
            let folder = args.get(1).map(|s| s.to_string());
            Some(RemuxCommand::SessionNew { name, folder })
        }
        "FolderMoveSession" => {
            let session = args.first().map(|s| s.to_string()).unwrap_or_default();
            let folder = args.get(1).map(|s| s.to_string());
            Some(RemuxCommand::FolderMoveSession { session, folder })
        }

        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Key notation parsing
// ---------------------------------------------------------------------------

/// Parse a key notation string (e.g. `"Ctrl-b"`, `"Alt-n"`, `"Enter"`) into
/// a [`KeyEvent`].
///
/// Returns `None` if the notation is not recognised.
pub fn parse_key_notation(notation: &str) -> Option<KeyEvent> {
    // Check for modifier prefixes.
    if let Some(rest) = notation.strip_prefix("Ctrl-") {
        let (code, mods) = parse_key_code(rest)?;
        return Some(KeyEvent::new_with_kind_and_state(
            code,
            mods | KeyModifiers::CONTROL,
            KeyEventKind::Press,
            KeyEventState::NONE,
        ));
    }
    if let Some(rest) = notation.strip_prefix("Alt-") {
        let (code, mods) = parse_key_code(rest)?;
        return Some(KeyEvent::new_with_kind_and_state(
            code,
            mods | KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        ));
    }
    if let Some(rest) = notation.strip_prefix("Shift-") {
        // Shift-Tab is represented as BackTab in crossterm.
        if rest == "Tab" {
            return Some(KeyEvent::new_with_kind_and_state(
                KeyCode::BackTab,
                KeyModifiers::SHIFT,
                KeyEventKind::Press,
                KeyEventState::NONE,
            ));
        }
        let (code, mods) = parse_key_code(rest)?;
        return Some(KeyEvent::new_with_kind_and_state(
            code,
            mods | KeyModifiers::SHIFT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        ));
    }

    // No modifier prefix: parse as bare key.
    let (code, mods) = parse_key_code(notation)?;
    Some(KeyEvent::new_with_kind_and_state(
        code,
        mods,
        KeyEventKind::Press,
        KeyEventState::NONE,
    ))
}

/// Parse a key code string (without modifier prefix) into a `KeyCode` and any
/// implicit modifiers (e.g. `BackTab` implies `SHIFT`).
fn parse_key_code(s: &str) -> Option<(KeyCode, KeyModifiers)> {
    // Special named keys.
    match s {
        "Enter" => return Some((KeyCode::Enter, KeyModifiers::NONE)),
        "Esc" => return Some((KeyCode::Esc, KeyModifiers::NONE)),
        "Tab" => return Some((KeyCode::Tab, KeyModifiers::NONE)),
        "BackTab" => return Some((KeyCode::BackTab, KeyModifiers::SHIFT)),
        "Space" => return Some((KeyCode::Char(' '), KeyModifiers::NONE)),
        "Backspace" => return Some((KeyCode::Backspace, KeyModifiers::NONE)),
        "Up" => return Some((KeyCode::Up, KeyModifiers::NONE)),
        "Down" => return Some((KeyCode::Down, KeyModifiers::NONE)),
        "Left" => return Some((KeyCode::Left, KeyModifiers::NONE)),
        "Right" => return Some((KeyCode::Right, KeyModifiers::NONE)),
        _ => {}
    }

    // Function keys: F1 through F12.
    if let Some(num_str) = s.strip_prefix('F') {
        let num: u8 = num_str.parse().ok()?;
        if (1..=12).contains(&num) {
            return Some((KeyCode::F(num), KeyModifiers::NONE));
        }
        return None;
    }

    // Single character.
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        // More than one character and not a recognised name.
        return None;
    }
    Some((KeyCode::Char(c), KeyModifiers::NONE))
}

// ---------------------------------------------------------------------------
// Leader key configuration
// ---------------------------------------------------------------------------

/// The default leader key: Ctrl-a.
pub fn default_leader_key() -> KeyEvent {
    KeyEvent::new_with_kind_and_state(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL,
        KeyEventKind::Press,
        KeyEventState::NONE,
    )
}

/// Parse the leader key from a TOML keybindings.command table.
///
/// Looks for a `leader` key in the table. If found, parses it via
/// `parse_key_notation`. Returns the default leader key if not found.
pub fn parse_leader_key(table: &toml::map::Map<String, toml::Value>) -> KeyEvent {
    if let Some(toml::Value::String(notation)) = table.get("leader") {
        parse_key_notation(notation).unwrap_or_else(default_leader_key)
    } else {
        default_leader_key()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_command tests --------------------------------------------------

    #[test]
    fn parse_command_no_args() {
        assert_eq!(parse_command("TabNew"), Some(RemuxCommand::TabNew));
        assert_eq!(parse_command("PaneClose"), Some(RemuxCommand::PaneClose));
        assert_eq!(
            parse_command("EnterNormal"),
            Some(RemuxCommand::EnterNormal)
        );
        assert_eq!(
            parse_command("EnterCommandMode"),
            Some(RemuxCommand::EnterCommandMode)
        );
        assert_eq!(
            parse_command("EnterVisualMode"),
            Some(RemuxCommand::EnterVisualMode)
        );
        assert_eq!(
            parse_command("ToggleStyle"),
            Some(RemuxCommand::ToggleStyle)
        );
        assert_eq!(
            parse_command("PaneStackAdd"),
            Some(RemuxCommand::PaneStackAdd)
        );
    }

    #[test]
    fn parse_command_resize_with_amount() {
        assert_eq!(
            parse_command("ResizeLeft 5"),
            Some(RemuxCommand::ResizeLeft(5))
        );
        assert_eq!(
            parse_command("ResizeDown 10"),
            Some(RemuxCommand::ResizeDown(10))
        );
    }

    #[test]
    fn parse_command_resize_default_amount() {
        assert_eq!(parse_command("ResizeUp"), Some(RemuxCommand::ResizeUp(1)));
        assert_eq!(
            parse_command("ResizeRight"),
            Some(RemuxCommand::ResizeRight(1))
        );
    }

    #[test]
    fn parse_command_string_arg() {
        assert_eq!(
            parse_command("TabRename work"),
            Some(RemuxCommand::TabRename("work".into()))
        );
        assert_eq!(
            parse_command("FolderNew projects"),
            Some(RemuxCommand::FolderNew("projects".into()))
        );
    }

    #[test]
    fn parse_command_quoted_string_arg() {
        assert_eq!(
            parse_command(r#"TabRename "my tab""#),
            Some(RemuxCommand::TabRename("my tab".into()))
        );
    }

    #[test]
    fn parse_command_session_new_multi_arg() {
        assert_eq!(
            parse_command(r#"SessionNew "dev server" "work""#),
            Some(RemuxCommand::SessionNew {
                name: "dev server".into(),
                folder: Some("work".into()),
            })
        );
    }

    #[test]
    fn parse_command_session_new_no_folder() {
        assert_eq!(
            parse_command("SessionNew myproject"),
            Some(RemuxCommand::SessionNew {
                name: "myproject".into(),
                folder: None,
            })
        );
    }

    #[test]
    fn parse_command_folder_move_session() {
        assert_eq!(
            parse_command(r#"FolderMoveSession "my session" "target folder""#),
            Some(RemuxCommand::FolderMoveSession {
                session: "my session".into(),
                folder: Some("target folder".into()),
            })
        );
    }

    #[test]
    fn parse_command_pane_rename_no_arg() {
        assert_eq!(
            parse_command("PaneRename"),
            Some(RemuxCommand::PaneRename(String::new()))
        );
    }

    #[test]
    fn parse_command_tab_goto() {
        assert_eq!(parse_command("TabGoto 3"), Some(RemuxCommand::TabGoto(3)));
    }

    #[test]
    fn parse_command_tab_goto_missing_arg() {
        assert_eq!(parse_command("TabGoto"), None);
    }

    #[test]
    fn parse_command_tab_move_default() {
        assert_eq!(parse_command("TabMove"), Some(RemuxCommand::TabMove(0)));
    }

    #[test]
    fn parse_command_unknown() {
        assert_eq!(parse_command("NonexistentCommand"), None);
    }

    #[test]
    fn parse_command_invalid_arg_type() {
        assert_eq!(parse_command("TabGoto notanumber"), None);
    }

    // -- parse_key_notation tests ---------------------------------------------

    #[test]
    fn key_notation_single_char() {
        let key = parse_key_notation("n").unwrap();
        assert_eq!(key.code, KeyCode::Char('n'));
        assert_eq!(key.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn key_notation_ctrl() {
        let key = parse_key_notation("Ctrl-b").unwrap();
        assert_eq!(key.code, KeyCode::Char('b'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn key_notation_alt() {
        let key = parse_key_notation("Alt-n").unwrap();
        assert_eq!(key.code, KeyCode::Char('n'));
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn key_notation_shift_tab() {
        let key = parse_key_notation("Shift-Tab").unwrap();
        assert_eq!(key.code, KeyCode::BackTab);
        assert_eq!(key.modifiers, KeyModifiers::SHIFT);
    }

    #[test]
    fn key_notation_special_keys() {
        let enter = parse_key_notation("Enter").unwrap();
        assert_eq!(enter.code, KeyCode::Enter);

        let esc = parse_key_notation("Esc").unwrap();
        assert_eq!(esc.code, KeyCode::Esc);

        let tab = parse_key_notation("Tab").unwrap();
        assert_eq!(tab.code, KeyCode::Tab);

        let space = parse_key_notation("Space").unwrap();
        assert_eq!(space.code, KeyCode::Char(' '));

        let backspace = parse_key_notation("Backspace").unwrap();
        assert_eq!(backspace.code, KeyCode::Backspace);
    }

    #[test]
    fn key_notation_arrow_keys() {
        assert_eq!(parse_key_notation("Up").unwrap().code, KeyCode::Up);
        assert_eq!(parse_key_notation("Down").unwrap().code, KeyCode::Down);
        assert_eq!(parse_key_notation("Left").unwrap().code, KeyCode::Left);
        assert_eq!(parse_key_notation("Right").unwrap().code, KeyCode::Right);
    }

    #[test]
    fn key_notation_function_keys() {
        assert_eq!(parse_key_notation("F1").unwrap().code, KeyCode::F(1));
        assert_eq!(parse_key_notation("F12").unwrap().code, KeyCode::F(12));
    }

    #[test]
    fn key_notation_invalid() {
        assert!(parse_key_notation("F0").is_none());
        assert!(parse_key_notation("F13").is_none());
        assert!(parse_key_notation("InvalidKey").is_none());
    }

    // -- Default tree tests ---------------------------------------------------

    #[test]
    fn default_tree_has_expected_groups() {
        let tree = KeybindingTree::default();
        assert!(tree.root.contains_key(&'t'));
        assert!(tree.root.contains_key(&'p'));
        assert!(tree.root.contains_key(&'s')); // Search group
        assert!(tree.root.contains_key(&'x')); // Session group (moved from 's')
                                               // 'f' (Folder group) was removed in the folder-keybinding-removal refactor.
        assert!(!tree.root.contains_key(&'f'));
        assert!(tree.root.contains_key(&'v'));
        assert!(tree.root.contains_key(&'g'));
        // 'b' (Buffer group) was removed in the search-mode refactor.
        assert!(!tree.root.contains_key(&'b'));
        // 'i' (EnterInsertMode) was removed in the leader-key-modes refactor.
        assert!(!tree.root.contains_key(&'i'));
    }

    #[test]
    fn lookup_leaf() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&['t', 'n']).unwrap();
        match node {
            KeyNode::Leaf { action, .. } => {
                assert!(action.contains(&"TabNew".to_string()));
            }
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    #[test]
    fn lookup_group() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&['t']).unwrap();
        assert!(matches!(node, KeyNode::Group { .. }));
    }

    #[test]
    fn lookup_missing() {
        let tree = KeybindingTree::default();
        assert!(tree.lookup(&['z']).is_none());
        assert!(tree.lookup(&['t', 'z']).is_none());
    }

    #[test]
    fn lookup_empty_path() {
        let tree = KeybindingTree::default();
        assert!(tree.lookup(&[]).is_none());
    }

    #[test]
    fn children_at_root() {
        let tree = KeybindingTree::default();
        let children = tree.children_at(&[]).unwrap();
        assert!(!children.is_empty());
        assert!(children.iter().any(|(k, _)| *k == 'v'));
    }

    #[test]
    fn children_at_group() {
        let tree = KeybindingTree::default();
        let children = tree.children_at(&['t']).unwrap();
        let keys: Vec<char> = children.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&'n'));
        assert!(keys.contains(&'c'));
    }

    #[test]
    fn children_at_leaf_returns_none() {
        let tree = KeybindingTree::default();
        assert!(tree.children_at(&['t', 'n']).is_none());
    }

    #[test]
    fn default_tree_has_toggle_style() {
        let tree = KeybindingTree::default();
        assert!(tree.root.contains_key(&'g'));
        let node = tree.lookup(&['g']).unwrap();
        match node {
            KeyNode::Leaf { action, label, .. } => {
                assert!(action.contains(&"ToggleStyle".to_string()));
                assert_eq!(label, "toggle style");
            }
            other => panic!("expected leaf for 'g', got {other:?}"),
        }
    }

    #[test]
    fn pane_group_contains_stack_add() {
        let tree = KeybindingTree::default();
        let children = tree.children_at(&['p']).unwrap();
        assert!(
            children
                .iter()
                .any(|(k, label)| *k == 'a' && label == "stack add"),
            "expected 'a' -> 'stack add' in Pane group, got: {children:?}"
        );
    }

    #[test]
    fn pane_group_has_rename_leaf() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&['p', 'R']).unwrap();
        match node {
            KeyNode::Leaf { action, label, .. } => {
                assert_eq!(action, &vec!["PaneRename".to_string()]);
                assert_eq!(label, "rename");
            }
            other => panic!("expected leaf for 'p' -> 'R', got {other:?}"),
        }
    }

    // -- Unbind via empty string test -----------------------------------------

    #[test]
    fn merge_unbinds_with_empty_action() {
        let mut base = KeybindingTree::default();
        assert!(base.root.contains_key(&'v'));
        let overrides = KeybindingTree {
            root: HashMap::from([(
                'v',
                KeyNode::Leaf {
                    label: String::new(),
                    action: vec![String::new()],
                },
            )]),
        };
        base.merge(&overrides);
        assert!(
            !base.root.contains_key(&'v'),
            "'v' key should have been removed by empty-action override"
        );
    }

    #[test]
    fn merge_unbinds_within_group() {
        let mut base = KeybindingTree::default();
        // Verify 'n' exists in Tab group.
        assert!(base.lookup(&['t', 'n']).is_some());

        let overrides = KeybindingTree {
            root: HashMap::from([(
                't',
                KeyNode::Group {
                    label: "Tab".into(),
                    children: HashMap::from([(
                        'n',
                        KeyNode::Leaf {
                            label: String::new(),
                            action: vec![String::new()],
                        },
                    )]),
                },
            )]),
        };
        base.merge(&overrides);
        // 'n' should be removed from the Tab group.
        assert!(base.lookup(&['t', 'n']).is_none());
        // Other Tab children should still exist.
        assert!(base.lookup(&['t', 'c']).is_some());
    }

    // -- Merge tests ----------------------------------------------------------

    #[test]
    fn merge_adds_new_keys() {
        let mut base = KeybindingTree::default();
        let overrides = KeybindingTree {
            root: HashMap::from([(
                'x',
                KeyNode::Leaf {
                    label: "custom".into(),
                    action: vec!["TabNew".into()],
                },
            )]),
        };
        base.merge(&overrides);
        assert!(base.root.contains_key(&'x'));
    }

    #[test]
    fn merge_replaces_leaf() {
        let mut base = KeybindingTree::default();
        let overrides = KeybindingTree {
            root: HashMap::from([(
                'v',
                KeyNode::Leaf {
                    label: "custom".into(),
                    action: vec!["SessionDetach".into()],
                },
            )]),
        };
        base.merge(&overrides);
        match &base.root[&'v'] {
            KeyNode::Leaf { action, .. } => assert_eq!(action, &vec!["SessionDetach".to_string()]),
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    #[test]
    fn merge_extends_group() {
        let mut base = KeybindingTree::default();
        let overrides = KeybindingTree {
            root: HashMap::from([(
                't',
                KeyNode::Group {
                    label: "Tab".into(),
                    children: HashMap::from([(
                        'x',
                        KeyNode::Leaf {
                            label: "extra".into(),
                            action: vec!["TabNew".into()],
                        },
                    )]),
                },
            )]),
        };
        base.merge(&overrides);
        // Original 'n' should still exist.
        assert!(base.lookup(&['t', 'n']).is_some());
        // New 'x' should be added.
        assert!(base.lookup(&['t', 'x']).is_some());
    }

    // -- TOML parsing tests ---------------------------------------------------

    #[test]
    fn from_toml_basic() {
        let toml_str = r#"
            [t]
            _label = "Tab"
            n = "TabNew"
            c = "TabClose"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let tree = KeybindingTree::from_toml(&value).unwrap();
        let node = tree.lookup(&['t', 'n']).unwrap();
        match node {
            KeyNode::Leaf { action, .. } => assert_eq!(action, &vec!["TabNew".to_string()]),
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    #[test]
    fn from_toml_action_chain() {
        let toml_str = r#"
            n = "TabNew; EnterNormal"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let tree = KeybindingTree::from_toml(&value).unwrap();
        let node = tree.lookup(&['n']).unwrap();
        match node {
            KeyNode::Leaf { action, .. } => {
                assert_eq!(
                    action,
                    &vec!["TabNew".to_string(), "EnterNormal".to_string()]
                );
            }
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    #[test]
    fn from_toml_nested_groups() {
        let toml_str = r#"
            [t]
            _label = "Tab"
            n = "TabNew"
            [t.s]
            _label = "Sub"
            a = "TabClose"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let tree = KeybindingTree::from_toml(&value).unwrap();
        let node = tree.lookup(&['t', 's', 'a']).unwrap();
        match node {
            KeyNode::Leaf { action, .. } => assert_eq!(action, &vec!["TabClose".to_string()]),
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    // -- Leader key tests -----------------------------------------------------

    #[test]
    fn default_leader_key_is_ctrl_a() {
        let leader = default_leader_key();
        assert_eq!(leader.code, KeyCode::Char('a'));
        assert_eq!(leader.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_leader_key_from_toml() {
        let mut table = toml::map::Map::new();
        table.insert("leader".to_string(), toml::Value::String("Ctrl-b".into()));
        let leader = parse_leader_key(&table);
        assert_eq!(leader.code, KeyCode::Char('b'));
        assert_eq!(leader.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_leader_key_fallback() {
        let table = toml::map::Map::new();
        let leader = parse_leader_key(&table);
        // Should fall back to default (Ctrl-a).
        assert_eq!(leader.code, KeyCode::Char('a'));
        assert_eq!(leader.modifiers, KeyModifiers::CONTROL);
    }

    // -- SendKey command parsing ----------------------------------------------

    #[test]
    fn parse_command_send_key_ctrl_a() {
        let cmd = parse_command("SendKey Ctrl-a").unwrap();
        assert_eq!(cmd, RemuxCommand::SendKey(vec![0x01]));
    }

    #[test]
    fn parse_command_send_key_invalid() {
        assert!(parse_command("SendKey").is_none());
    }

    // -- ShortcutBindings tests -----------------------------------------------

    #[test]
    fn default_shortcuts_has_alt_h() {
        let bindings = ShortcutBindings::default();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('h'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        match bindings.lookup(&key).unwrap() {
            InterceptAction::Command(cmds) => assert_eq!(cmds, &["PaneFocusLeft"]),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn default_shortcuts_has_alt_p_group() {
        let bindings = ShortcutBindings::default();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('p'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        match bindings.lookup(&key).unwrap() {
            InterceptAction::GroupPrefix(path) => assert_eq!(path, &['p']),
            other => panic!("expected GroupPrefix, got {other:?}"),
        }
    }

    #[test]
    fn default_shortcuts_unbound_returns_none() {
        let bindings = ShortcutBindings::default();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('z'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        assert!(bindings.lookup(&key).is_none());
    }

    // -- ShortcutBindings TOML parsing tests ----------------------------------

    #[test]
    fn shortcuts_from_toml_command() {
        let toml_str = r#"
            "Alt-x" = "TabNew"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let bindings = ShortcutBindings::from_toml(&value).unwrap();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('x'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        match bindings.lookup(&key).unwrap() {
            InterceptAction::Command(cmds) => assert_eq!(cmds, &["TabNew"]),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn shortcuts_from_toml_group_prefix() {
        let toml_str = r#"
            "Alt-s" = "@s"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let bindings = ShortcutBindings::from_toml(&value).unwrap();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('s'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        match bindings.lookup(&key).unwrap() {
            InterceptAction::GroupPrefix(path) => assert_eq!(path, &['s']),
            other => panic!("expected GroupPrefix, got {other:?}"),
        }
    }

    #[test]
    fn shortcuts_from_toml_action_chain() {
        let toml_str = r#"
            "Alt-x" = "PaneNew; PaneFocusRight"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let bindings = ShortcutBindings::from_toml(&value).unwrap();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('x'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        match bindings.lookup(&key).unwrap() {
            InterceptAction::Command(cmds) => assert_eq!(cmds, &["PaneNew", "PaneFocusRight"]),
            other => panic!("expected Command chain, got {other:?}"),
        }
    }

    #[test]
    fn shortcuts_from_toml_rejects_plain_key() {
        let toml_str = r#"
            "n" = "TabNew"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let bindings = ShortcutBindings::from_toml(&value).unwrap();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('n'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        assert!(bindings.lookup(&key).is_none());
    }

    #[test]
    fn shortcuts_merge_override() {
        let mut base = ShortcutBindings::default();
        let toml_str = r#"
            "Alt-h" = "TabPrev"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let overrides = ShortcutBindings::from_toml(&value).unwrap();
        base.merge(&overrides);
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('h'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        match base.lookup(&key).unwrap() {
            InterceptAction::Command(cmds) => assert_eq!(cmds, &["TabPrev"]),
            other => panic!("expected Command(TabPrev), got {other:?}"),
        }
    }

    #[test]
    fn shortcuts_merge_unbind() {
        let mut base = ShortcutBindings::default();
        let toml_str = r#"
            "Alt-h" = ""
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let overrides = ShortcutBindings::from_toml(&value).unwrap();
        base.merge(&overrides);
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('h'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        assert!(
            base.lookup(&key).is_none(),
            "Alt-h should have been unbound"
        );
    }

    // -- validate_group_refs tests --------------------------------------------

    #[test]
    fn validate_group_refs_valid() {
        let bindings = ShortcutBindings::default();
        let tree = KeybindingTree::default();
        assert!(bindings.validate_group_refs(&tree));
    }

    #[test]
    fn validate_group_refs_invalid() {
        let mut bindings = ShortcutBindings::default();
        bindings
            .bindings
            .insert(alt_key('z'), InterceptAction::GroupPrefix(vec!['z']));
        let tree = KeybindingTree::default();
        assert!(!bindings.validate_group_refs(&tree));
    }
}
