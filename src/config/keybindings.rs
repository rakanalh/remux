use std::collections::{HashMap, HashSet};

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
        /// When true, executing a leaf inside this group keeps the which-key
        /// menu open (the state drops back to this group) so the user can keep
        /// triggering the group's actions (e.g. the Resize submenu). Only ESC
        /// or an unmatched key exits.
        sticky: bool,
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

/// Helper to build a group node (non-sticky).
fn group(label: &str, children: Vec<(char, KeyNode)>) -> KeyNode {
    KeyNode::Group {
        label: label.to_string(),
        children: children.into_iter().collect(),
        sticky: false,
    }
}

/// Helper to build a sticky group node. Executing a leaf in a sticky group
/// keeps the which-key menu open so the user can keep triggering actions.
fn group_sticky(label: &str, children: Vec<(char, KeyNode)>) -> KeyNode {
    KeyNode::Group {
        label: label.to_string(),
        children: children.into_iter().collect(),
        sticky: true,
    }
}

fn build_default_tree() -> HashMap<char, KeyNode> {
    let mut root = HashMap::new();

    // p: Pane
    root.insert(
        'p',
        group(
            "Pane",
            vec![
                ('n', leaf_chain("new", &["PaneNew", "EnterNormal"])),
                ('x', leaf_chain("close", &["PaneClose", "EnterNormal"])),
                (
                    'v',
                    leaf_chain("split vertical", &["PaneSplitVertical", "EnterNormal"]),
                ),
                (
                    's',
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
                    'H',
                    leaf_chain("move left", &["PaneMoveLeft", "EnterNormal"]),
                ),
                (
                    'J',
                    leaf_chain("move down", &["PaneMoveDown", "EnterNormal"]),
                ),
                ('K', leaf_chain("move up", &["PaneMoveUp", "EnterNormal"])),
                (
                    'L',
                    leaf_chain("move right", &["PaneMoveRight", "EnterNormal"]),
                ),
                ('z', leaf_chain("zoom", &["PaneToggleZoom", "EnterNormal"])),
                ('m', leaf_chain("set master", &["SetMaster", "EnterNormal"])),
                ('r', leaf("rename", "PaneRename")),
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
                    'R',
                    group_sticky(
                        "Resize",
                        vec![
                            ('h', leaf("left", "ResizeLeft 5")),
                            ('j', leaf("down", "ResizeDown 5")),
                            ('k', leaf("up", "ResizeUp 5")),
                            ('l', leaf("right", "ResizeRight 5")),
                        ],
                    ),
                ),
            ],
        ),
    );

    // t: Tab
    root.insert(
        't',
        group(
            "Tab",
            vec![
                ('n', leaf_chain("new", &["TabNew", "EnterNormal"])),
                ('x', leaf_chain("close", &["TabClose", "EnterNormal"])),
                ('r', leaf("rename", "TabRename")),
                (']', leaf_chain("next", &["TabNext", "EnterNormal"])),
                ('[', leaf_chain("prev", &["TabPrev", "EnterNormal"])),
                ('m', leaf("move", "TabMove")),
                ('1', leaf_chain("tab 1", &["TabGoto 0", "EnterNormal"])),
                ('2', leaf_chain("tab 2", &["TabGoto 1", "EnterNormal"])),
                ('3', leaf_chain("tab 3", &["TabGoto 2", "EnterNormal"])),
                ('4', leaf_chain("tab 4", &["TabGoto 3", "EnterNormal"])),
                ('5', leaf_chain("tab 5", &["TabGoto 4", "EnterNormal"])),
                ('6', leaf_chain("tab 6", &["TabGoto 5", "EnterNormal"])),
                ('7', leaf_chain("tab 7", &["TabGoto 6", "EnterNormal"])),
                ('8', leaf_chain("tab 8", &["TabGoto 7", "EnterNormal"])),
                ('9', leaf_chain("tab 9", &["TabGoto 8", "EnterNormal"])),
            ],
        ),
    );

    // Quick tab navigation: Shift+] / Shift+[ (next / previous tab).
    root.insert('}', leaf_chain("next tab", &["TabNext", "EnterNormal"]));
    root.insert('{', leaf_chain("prev tab", &["TabPrev", "EnterNormal"]));

    // x: Session
    root.insert(
        'x',
        group(
            "Session",
            vec![
                ('s', leaf("switch", "SessionQuickSwitch")),
                ('o', leaf("last session", "SessionSwitchLast")),
                ('n', leaf("new", "SessionNew")),
                ('r', leaf("rename", "SessionRename")),
                ('d', leaf("detach", "SessionDetach")),
                ('m', leaf("manager", "OpenSessionManager")),
                ('f', leaf("move to folder", "SessionMoveToFolder")),
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
                ('e', leaf("open in editor", "BufferEditInEditor")),
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
    root.insert(
        'f',
        leaf_chain("zoom pane", &["PaneToggleZoom", "EnterNormal"]),
    );

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
                sticky: base_sticky,
            }) => {
                if let KeyNode::Group {
                    label,
                    children: override_children,
                    sticky: override_sticky,
                } = override_node
                {
                    // Both are groups: update label and merge children.
                    *base_label = label.clone();
                    // Preserve stickiness: an override that re-declares a group
                    // should not silently unstick it (a user config that
                    // re-opens the group leaves `sticky` at its false default),
                    // so the group stays sticky if either side declares it.
                    *base_sticky = *base_sticky || *override_sticky;
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

    /// Enumerate the shortcut bindings as `(key_notation, label)` pairs, sorted
    /// by notation. Used to display the global shortcuts in the which-key popup.
    ///
    /// - `Command` actions are humanized from their first (non-`EnterNormal`)
    ///   command, e.g. `PaneFocusLeft` -> `"focus left"`.
    /// - `GroupPrefix` actions are labelled `"→ <path>"` (the group's own label
    ///   is not resolvable without the tree here, so the path is shown).
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut result: Vec<(String, String)> = self
            .bindings
            .iter()
            .map(|(key, action)| {
                let notation = format_key_notation(key);
                let label = match action {
                    InterceptAction::Command(cmds) => {
                        let cmd = cmds
                            .iter()
                            .find(|c| c.as_str() != "EnterNormal")
                            .or_else(|| cmds.first());
                        match cmd {
                            Some(c) => humanize_command(c),
                            None => String::new(),
                        }
                    }
                    InterceptAction::GroupPrefix(path) => {
                        let path_str: String = path.iter().collect();
                        format!("\u{2192} {path_str}")
                    }
                };
                (notation, label)
            })
            .collect();
        result.sort_by(|(a, _), (b, _)| a.cmp(b));
        result
    }
}

/// Format a [`NormalizedKeyEvent`] back into key notation (the inverse of
/// [`parse_key_notation`]), e.g. `Alt` + `h` -> `"Alt-h"`, `Alt` + `,` ->
/// `"Alt-,"`, `Alt` + `H` -> `"Alt-H"`.
pub fn format_key_notation(ev: &NormalizedKeyEvent) -> String {
    let mut prefix = String::new();
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        prefix.push_str("Ctrl-");
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        prefix.push_str("Alt-");
    }
    // Shift is only rendered explicitly for keys whose case does not already
    // encode it (character keys carry Shift via their uppercase form).
    let shift_char = matches!(ev.code, KeyCode::Char(_));
    if ev.modifiers.contains(KeyModifiers::SHIFT) && !shift_char && ev.code != KeyCode::BackTab {
        prefix.push_str("Shift-");
    }

    let key = match ev.code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Shift-Tab".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    };

    format!("{prefix}{key}")
}

/// Split a PascalCase identifier into lowercase, space-separated words, e.g.
/// `"FocusLeft"` -> `"focus left"`.
fn split_pascal(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// Humanize a command string into a short, friendly label for the which-key
/// popup, e.g. `"PaneFocusLeft"` -> `"focus left"`, `"TabNext"` -> `"next tab"`,
/// `"TabGoto 0"` -> `"tab 1"`, `"SessionQuickSwitch"` -> `"switch session"`.
///
/// Reasonable and concise; it does not aim to be perfect.
pub fn humanize_command(command: &str) -> String {
    let mut parts = command.split_whitespace();
    let name = parts.next().unwrap_or("");
    let arg = parts.next();

    // `TabGoto N` -> `tab N+1` (bindings are 0-indexed, display is 1-indexed).
    if name == "TabGoto" {
        if let Some(n) = arg.and_then(|a| a.parse::<usize>().ok()) {
            return format!("tab {}", n + 1);
        }
    }

    // Friendlier phrasings for a few verbose command names.
    match name {
        "SessionQuickSwitch" => return "switch session".to_string(),
        "SessionSwitchLast" => return "last session".to_string(),
        "LayoutNext" => return "next layout".to_string(),
        "SetMaster" => return "set master".to_string(),
        _ => {}
    }

    // `Tab*` / `Session*` read better with the noun trailing ("next tab"),
    // while `Pane*` reads better with the prefix simply dropped ("focus left").
    if let Some(rest) = name.strip_prefix("Tab") {
        if !rest.is_empty() {
            return format!("{} tab", split_pascal(rest));
        }
    }
    if let Some(rest) = name.strip_prefix("Session") {
        if !rest.is_empty() {
            return format!("{} session", split_pascal(rest));
        }
    }
    if let Some(rest) = name.strip_prefix("Pane") {
        if !rest.is_empty() {
            return split_pascal(rest);
        }
    }

    split_pascal(name)
}

/// Helper to create a `NormalizedKeyEvent` with the Alt modifier.
fn alt_key(c: char) -> NormalizedKeyEvent {
    NormalizedKeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

/// Helper to build an `InterceptAction::Command` from a single command string.
fn cmd(command: &str) -> InterceptAction {
    InterceptAction::Command(vec![command.to_string()])
}

impl Default for ShortcutBindings {
    fn default() -> Self {
        let mut bindings = HashMap::new();

        // Focus panes directionally (Alt-h/j/k/l).
        bindings.insert(alt_key('h'), cmd("PaneFocusLeft"));
        bindings.insert(alt_key('j'), cmd("PaneFocusDown"));
        bindings.insert(alt_key('k'), cmd("PaneFocusUp"));
        bindings.insert(alt_key('l'), cmd("PaneFocusRight"));

        // Move panes directionally (Alt-Shift-h/j/k/l -> Alt-H/J/K/L). These
        // match `parse_key_notation("Alt-H")`, i.e. an uppercase char with the
        // Alt modifier.
        bindings.insert(alt_key('H'), cmd("PaneMoveLeft"));
        bindings.insert(alt_key('J'), cmd("PaneMoveDown"));
        bindings.insert(alt_key('K'), cmd("PaneMoveUp"));
        bindings.insert(alt_key('L'), cmd("PaneMoveRight"));

        // Tab navigation (Alt-, / Alt-.) and direct jumps (Alt-1..Alt-9).
        bindings.insert(alt_key(','), cmd("TabPrev"));
        bindings.insert(alt_key('.'), cmd("TabNext"));
        bindings.insert(alt_key('1'), cmd("TabGoto 0"));
        bindings.insert(alt_key('2'), cmd("TabGoto 1"));
        bindings.insert(alt_key('3'), cmd("TabGoto 2"));
        bindings.insert(alt_key('4'), cmd("TabGoto 3"));
        bindings.insert(alt_key('5'), cmd("TabGoto 4"));
        bindings.insert(alt_key('6'), cmd("TabGoto 5"));
        bindings.insert(alt_key('7'), cmd("TabGoto 6"));
        bindings.insert(alt_key('8'), cmd("TabGoto 7"));
        bindings.insert(alt_key('9'), cmd("TabGoto 8"));

        // Misc quick actions.
        bindings.insert(alt_key('t'), cmd("TabNew"));
        bindings.insert(alt_key('s'), cmd("SessionQuickSwitch"));
        bindings.insert(alt_key('o'), cmd("SessionSwitchLast"));
        bindings.insert(alt_key('z'), cmd("PaneToggleZoom"));
        bindings.insert(alt_key(' '), cmd("LayoutNext"));
        bindings.insert(alt_key('m'), cmd("SetMaster"));

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
                    // User-defined groups are non-sticky. Stickiness is a
                    // built-in property (e.g. Resize); the `sticky` merge rule
                    // preserves it when a user re-declares such a group.
                    sticky: false,
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
        "PaneMoveLeft" => Some(RemuxCommand::PaneMoveLeft),
        "PaneMoveRight" => Some(RemuxCommand::PaneMoveRight),
        "PaneMoveUp" => Some(RemuxCommand::PaneMoveUp),
        "PaneMoveDown" => Some(RemuxCommand::PaneMoveDown),
        "SessionDetach" => Some(RemuxCommand::SessionDetach),
        "SessionList" => Some(RemuxCommand::SessionList),
        "FolderList" => Some(RemuxCommand::FolderList),
        "BufferEditInEditor" => Some(RemuxCommand::BufferEditInEditor),
        "EnterSearchMode" => Some(RemuxCommand::EnterSearchMode),
        "OpenSessionManager" => Some(RemuxCommand::OpenSessionManager),
        "SessionMoveToFolder" => Some(RemuxCommand::SessionMoveToFolder),
        "SessionSwitchLast" => Some(RemuxCommand::SessionSwitchLast),
        "ToggleStyle" => Some(RemuxCommand::ToggleStyle),
        "LayoutNext" => Some(RemuxCommand::LayoutNext),
        "SetMaster" => Some(RemuxCommand::SetMaster),
        "PaneToggleZoom" => Some(RemuxCommand::PaneToggleZoom),
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
        // A destination/alias is a single token (no spaces), so `args.first()`
        // is correct here -- returns `None` when no argument is given.
        "RemoteConnect" => args.first().map(|s| RemuxCommand::RemoteConnect(s.clone())),

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
// Session-manager chord bindings
// ---------------------------------------------------------------------------

/// The set of actions a session-manager chord can trigger. Each maps to one of
/// the phase-1 explicit-target structural commands or an existing overlay flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionManagerBinding {
    TabNew,
    TabClose,
    TabRename,
    TabMoveLeft,
    TabMoveRight,
    PaneNew,
    PaneClose,
    PaneRename,
    SessionNew,
    SessionClose,
    SessionRename,
    SessionMove,
    FolderNew,
    FolderDelete,
    FolderRename,
}

impl SessionManagerBinding {
    /// Parse an action name (as written in the config) into a binding.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "TabNew" => Self::TabNew,
            "TabClose" => Self::TabClose,
            "TabRename" => Self::TabRename,
            "TabMoveLeft" => Self::TabMoveLeft,
            "TabMoveRight" => Self::TabMoveRight,
            "PaneNew" => Self::PaneNew,
            "PaneClose" => Self::PaneClose,
            "PaneRename" => Self::PaneRename,
            "SessionNew" => Self::SessionNew,
            "SessionClose" => Self::SessionClose,
            "SessionRename" => Self::SessionRename,
            "SessionMove" => Self::SessionMove,
            "FolderNew" => Self::FolderNew,
            "FolderDelete" => Self::FolderDelete,
            "FolderRename" => Self::FolderRename,
            _ => return None,
        })
    }
}

/// The default session-manager chord map (also used when the config section is
/// absent). Chord string -> action.
fn default_session_manager_chords() -> Vec<(&'static str, SessionManagerBinding)> {
    use SessionManagerBinding::*;
    vec![
        ("tn", TabNew),
        ("tx", TabClose),
        ("tr", TabRename),
        ("th", TabMoveLeft),
        ("tl", TabMoveRight),
        ("pn", PaneNew),
        ("px", PaneClose),
        ("pr", PaneRename),
        ("sn", SessionNew),
        ("sx", SessionClose),
        ("sr", SessionRename),
        ("sm", SessionMove),
        ("fn", FolderNew),
        ("fx", FolderDelete),
        ("fr", FolderRename),
    ]
}

/// Whether `s` is a valid chord: 1 or 2 characters, each printable (not
/// whitespace, not a control char).
fn is_valid_chord(s: &str) -> bool {
    let count = s.chars().count();
    if count == 0 || count > 2 {
        return false;
    }
    s.chars().all(|c| !c.is_whitespace() && !c.is_control())
}

/// Configurable one- or two-key chord bindings for the session-manager overlay.
///
/// Two-char chords take priority: if a char begins any 2-char chord it is a
/// "prefix", and any single-char binding using that same char is dropped
/// (prefix wins). This is computed over the *merged* (defaults + user) map so
/// user additions/overrides are reflected.
#[derive(Debug, Clone)]
pub struct SessionManagerBindings {
    /// Full chord string (1 or 2 chars) -> action.
    map: HashMap<String, SessionManagerBinding>,
    /// First chars of any configured 2-char chord (a pending prefix).
    prefixes: HashSet<char>,
}

impl SessionManagerBindings {
    /// Build from a chord->action map. Computes the prefix set and drops any
    /// single-char binding shadowed by a 2-char prefix (logging the conflict).
    fn from_map(mut map: HashMap<String, SessionManagerBinding>) -> Self {
        let mut prefixes = HashSet::new();
        for chord in map.keys() {
            let mut chars = chord.chars();
            if let (Some(first), Some(_)) = (chars.next(), chars.next()) {
                prefixes.insert(first);
            }
        }
        // Prefix wins: drop single-char chords shadowed by a 2-char prefix.
        map.retain(|chord, _| {
            let mut chars = chord.chars();
            let first = chars.next();
            let is_single = first.is_some() && chars.next().is_none();
            if is_single {
                if let Some(c) = first {
                    if prefixes.contains(&c) {
                        log::warn!(
                            "session_manager keybinding '{c}' is shadowed by a 2-char chord \
                             starting with '{c}'; ignoring the single-key binding"
                        );
                        return false;
                    }
                }
            }
            true
        });
        Self { map, prefixes }
    }

    /// Whether `c` begins a configured 2-char chord (a pending prefix).
    pub fn is_prefix(&self, c: char) -> bool {
        self.prefixes.contains(&c)
    }

    /// Look up a single-char binding. Only meaningful when `c` is not a prefix.
    pub fn single(&self, c: char) -> Option<SessionManagerBinding> {
        let mut buf = [0u8; 4];
        self.map.get(c.encode_utf8(&mut buf) as &str).copied()
    }

    /// Look up a full 2-char chord.
    pub fn chord(&self, first: char, second: char) -> Option<SessionManagerBinding> {
        let mut s = String::with_capacity(first.len_utf8() + second.len_utf8());
        s.push(first);
        s.push(second);
        self.map.get(&s).copied()
    }

    /// Number of bindings (after prefix-shadow pruning). Test helper.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether there are no bindings.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterate the effective chord bindings as `(chord, binding)` pairs. Order
    /// is unspecified (backed by a hash map); callers that need a stable order
    /// must sort. Used by the session-manager overlay to render its help footer
    /// so that user overrides are reflected.
    pub fn iter(&self) -> impl Iterator<Item = (&str, SessionManagerBinding)> + '_ {
        self.map.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Parse the `[keybindings.session_manager]` table, starting from the
    /// defaults and applying user entries on top. Invalid chords (not 1-2
    /// printable chars) and unknown action names are logged and skipped. An
    /// empty action string unbinds a (possibly default) chord.
    pub fn from_toml(value: &toml::Value) -> Self {
        let mut chords: HashMap<String, SessionManagerBinding> = default_session_manager_chords()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();

        if let Some(table) = value.as_table() {
            for (chord, val) in table {
                if !is_valid_chord(chord) {
                    log::warn!(
                        "session_manager keybinding '{chord}': chord must be 1-2 printable \
                         characters, skipping"
                    );
                    continue;
                }
                let action_name = match val.as_str() {
                    Some(s) => s,
                    None => {
                        log::warn!(
                            "session_manager keybinding '{chord}': value must be an action \
                             name string, skipping"
                        );
                        continue;
                    }
                };
                // An empty action string unbinds the chord.
                if action_name.is_empty() {
                    chords.remove(chord);
                    continue;
                }
                match SessionManagerBinding::from_name(action_name) {
                    Some(binding) => {
                        chords.insert(chord.clone(), binding);
                    }
                    None => {
                        log::warn!(
                            "session_manager keybinding '{chord}': unknown action \
                             '{action_name}', skipping"
                        );
                    }
                }
            }
        }

        Self::from_map(chords)
    }
}

impl Default for SessionManagerBindings {
    fn default() -> Self {
        let map = default_session_manager_chords()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        Self::from_map(map)
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
    fn parse_command_remote_connect() {
        assert_eq!(
            parse_command("RemoteConnect user@host"),
            Some(RemuxCommand::RemoteConnect("user@host".into()))
        );
        // A bare alias (single token) also works.
        assert_eq!(
            parse_command("RemoteConnect pi"),
            Some(RemuxCommand::RemoteConnect("pi".into()))
        );
    }

    #[test]
    fn parse_command_remote_connect_missing_arg() {
        // No argument -> None, so an empty command does nothing.
        assert_eq!(parse_command("RemoteConnect"), None);
    }

    #[test]
    fn parse_command_unknown() {
        assert_eq!(parse_command("NonexistentCommand"), None);
    }

    #[test]
    fn parse_command_session_switch_last() {
        assert_eq!(
            parse_command("SessionSwitchLast"),
            Some(RemuxCommand::SessionSwitchLast)
        );
    }

    #[test]
    fn session_group_has_last_session_leaf() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&['x', 'o']).unwrap();
        match node {
            KeyNode::Leaf { action, label, .. } => {
                assert_eq!(action, &vec!["SessionSwitchLast".to_string()]);
                assert_eq!(label, "last session");
            }
            other => panic!("expected leaf for 'x' -> 'o', got {other:?}"),
        }
    }

    #[test]
    fn default_shortcut_binds_alt_o_to_last_session() {
        let bindings = ShortcutBindings::default();
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('o'),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        match bindings.lookup(&key) {
            Some(InterceptAction::Command(cmds)) => {
                assert_eq!(cmds, &vec!["SessionSwitchLast".to_string()]);
            }
            other => panic!("expected Alt-o -> SessionSwitchLast, got {other:?}"),
        }
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
        assert!(tree.root.contains_key(&'f')); // Zoom pane toggle
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
        assert!(keys.contains(&'x'));
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
        let node = tree.lookup(&['p', 'r']).unwrap();
        match node {
            KeyNode::Leaf { action, label, .. } => {
                assert_eq!(action, &vec!["PaneRename".to_string()]);
                assert_eq!(label, "rename");
            }
            other => panic!("expected leaf for 'p' -> 'r', got {other:?}"),
        }
    }

    #[test]
    fn pane_group_has_move_and_resize() {
        let tree = KeybindingTree::default();
        // 'H' is now a move-pane leaf.
        let mv = tree.lookup(&['p', 'H']).unwrap();
        match mv {
            KeyNode::Leaf { action, label, .. } => {
                assert!(action.contains(&"PaneMoveLeft".to_string()));
                assert_eq!(label, "move left");
            }
            other => panic!("expected leaf for 'p' -> 'H', got {other:?}"),
        }
        // 'R' is now the Resize group.
        let resize = tree.lookup(&['p', 'R']).unwrap();
        assert!(matches!(resize, KeyNode::Group { .. }));
        let down = tree.lookup(&['p', 'R', 'j']).unwrap();
        match down {
            KeyNode::Leaf { action, .. } => assert_eq!(action, &vec!["ResizeDown 5".to_string()]),
            other => panic!("expected leaf for 'p' -> 'R' -> 'j', got {other:?}"),
        }
    }

    #[test]
    fn tab_group_has_close_and_goto() {
        let tree = KeybindingTree::default();
        // 'x' closes the tab.
        let close = tree.lookup(&['t', 'x']).unwrap();
        match close {
            KeyNode::Leaf { action, .. } => assert!(action.contains(&"TabClose".to_string())),
            other => panic!("expected leaf for 't' -> 'x', got {other:?}"),
        }
        // '1' jumps to the first tab (0-indexed).
        let goto = tree.lookup(&['t', '1']).unwrap();
        match goto {
            KeyNode::Leaf { action, .. } => assert!(action.contains(&"TabGoto 0".to_string())),
            other => panic!("expected leaf for 't' -> '1', got {other:?}"),
        }
    }

    #[test]
    fn session_group_has_switch_leaf() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&['x', 's']).unwrap();
        match node {
            KeyNode::Leaf { action, label, .. } => {
                assert_eq!(action, &vec!["SessionQuickSwitch".to_string()]);
                assert_eq!(label, "switch");
            }
            other => panic!("expected leaf for 'x' -> 's', got {other:?}"),
        }
    }

    #[test]
    fn default_tree_has_quick_tab_navigation() {
        let tree = KeybindingTree::default();

        let next = tree.lookup(&['}']).unwrap();
        match next {
            KeyNode::Leaf { action, label, .. } => {
                assert!(action.contains(&"TabNext".to_string()));
                assert_eq!(label, "next tab");
            }
            other => panic!("expected leaf for '}}', got {other:?}"),
        }

        let prev = tree.lookup(&['{']).unwrap();
        match prev {
            KeyNode::Leaf { action, label, .. } => {
                assert!(action.contains(&"TabPrev".to_string()));
                assert_eq!(label, "prev tab");
            }
            other => panic!("expected leaf for '{{', got {other:?}"),
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
                    sticky: false,
                },
            )]),
        };
        base.merge(&overrides);
        // 'n' should be removed from the Tab group.
        assert!(base.lookup(&['t', 'n']).is_none());
        // Other Tab children should still exist.
        assert!(base.lookup(&['t', 'x']).is_some());
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
                    sticky: false,
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

    /// Helper: look up an Alt-modified char in the default shortcut set.
    fn lookup_alt(bindings: &ShortcutBindings, c: char) -> Option<InterceptAction> {
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char(c),
            KeyModifiers::ALT,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        bindings.lookup(&key).cloned()
    }

    #[test]
    fn default_shortcuts_has_alt_tab_next() {
        let bindings = ShortcutBindings::default();
        match lookup_alt(&bindings, '.').unwrap() {
            InterceptAction::Command(cmds) => assert_eq!(cmds, &["TabNext"]),
            other => panic!("expected Command(TabNext) for Alt-., got {other:?}"),
        }
    }

    #[test]
    fn default_shortcuts_has_alt_tab_prev() {
        let bindings = ShortcutBindings::default();
        match lookup_alt(&bindings, ',').unwrap() {
            InterceptAction::Command(cmds) => assert_eq!(cmds, &["TabPrev"]),
            other => panic!("expected Command(TabPrev) for Alt-,, got {other:?}"),
        }
    }

    #[test]
    fn default_shortcuts_has_alt_move_left() {
        let bindings = ShortcutBindings::default();
        // Capital 'H' with the Alt modifier (matches parse_key_notation("Alt-H")).
        match lookup_alt(&bindings, 'H').unwrap() {
            InterceptAction::Command(cmds) => assert_eq!(cmds, &["PaneMoveLeft"]),
            other => panic!("expected Command(PaneMoveLeft) for Alt-H, got {other:?}"),
        }
    }

    #[test]
    fn default_shortcuts_has_alt_tab_goto() {
        let bindings = ShortcutBindings::default();
        // Alt-1 jumps to the first tab (0-indexed).
        match lookup_alt(&bindings, '1').unwrap() {
            InterceptAction::Command(cmds) => assert_eq!(cmds, &["TabGoto 0"]),
            other => panic!("expected Command(TabGoto 0) for Alt-1, got {other:?}"),
        }
        // Alt-9 jumps to the ninth tab.
        match lookup_alt(&bindings, '9').unwrap() {
            InterceptAction::Command(cmds) => assert_eq!(cmds, &["TabGoto 8"]),
            other => panic!("expected Command(TabGoto 8) for Alt-9, got {other:?}"),
        }
    }

    #[test]
    fn default_shortcuts_has_alt_extras() {
        let bindings = ShortcutBindings::default();
        assert!(matches!(
            lookup_alt(&bindings, 't').unwrap(),
            InterceptAction::Command(cmds) if cmds == ["TabNew"]
        ));
        assert!(matches!(
            lookup_alt(&bindings, 's').unwrap(),
            InterceptAction::Command(cmds) if cmds == ["SessionQuickSwitch"]
        ));
        assert!(matches!(
            lookup_alt(&bindings, 'z').unwrap(),
            InterceptAction::Command(cmds) if cmds == ["PaneToggleZoom"]
        ));
        assert!(matches!(
            lookup_alt(&bindings, ' ').unwrap(),
            InterceptAction::Command(cmds) if cmds == ["LayoutNext"]
        ));
    }

    #[test]
    fn default_shortcuts_no_group_prefixes() {
        // The new default set is entirely command shortcuts -- no @-group
        // prefixes (the old Alt-p/Alt-t group defaults were removed).
        let bindings = ShortcutBindings::default();
        assert!(
            !bindings
                .bindings
                .values()
                .any(|a| matches!(a, InterceptAction::GroupPrefix(_))),
            "default shortcuts should not contain any GroupPrefix bindings"
        );
    }

    #[test]
    fn default_shortcuts_unbound_returns_none() {
        let bindings = ShortcutBindings::default();
        // Alt-w is not part of the default set.
        assert!(lookup_alt(&bindings, 'w').is_none());
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

    // -- entries / humanize / format_key_notation tests -----------------------

    #[test]
    fn shortcut_entries_include_expected_pairs() {
        let entries = ShortcutBindings::default().entries();
        assert!(
            entries.contains(&("Alt-h".to_string(), "focus left".to_string())),
            "expected Alt-h -> focus left, got: {entries:?}"
        );
        assert!(
            entries.contains(&("Alt-.".to_string(), "next tab".to_string())),
            "expected Alt-. -> next tab, got: {entries:?}"
        );
    }

    #[test]
    fn shortcut_entries_are_sorted_by_notation() {
        let entries = ShortcutBindings::default().entries();
        let mut sorted = entries.clone();
        sorted.sort_by(|(a, _), (b, _)| a.cmp(b));
        assert_eq!(entries, sorted);
    }

    #[test]
    fn shortcut_entries_group_prefix_uses_path_label() {
        let mut bindings = ShortcutBindings::default();
        bindings
            .bindings
            .insert(alt_key('p'), InterceptAction::GroupPrefix(vec!['p']));
        let entries = bindings.entries();
        assert!(
            entries.contains(&("Alt-p".to_string(), "\u{2192} p".to_string())),
            "expected Alt-p -> '\u{2192} p', got: {entries:?}"
        );
    }

    #[test]
    fn humanize_command_cases() {
        assert_eq!(humanize_command("PaneFocusLeft"), "focus left");
        assert_eq!(humanize_command("PaneMoveLeft"), "move left");
        assert_eq!(humanize_command("PaneToggleZoom"), "toggle zoom");
        assert_eq!(humanize_command("TabNext"), "next tab");
        assert_eq!(humanize_command("TabNew"), "new tab");
        assert_eq!(humanize_command("TabGoto 0"), "tab 1");
        assert_eq!(humanize_command("TabGoto 8"), "tab 9");
        assert_eq!(humanize_command("SessionQuickSwitch"), "switch session");
        assert_eq!(humanize_command("SessionSwitchLast"), "last session");
        assert_eq!(humanize_command("LayoutNext"), "next layout");
    }

    #[test]
    fn format_key_notation_cases() {
        assert_eq!(format_key_notation(&alt_key('h')), "Alt-h");
        assert_eq!(format_key_notation(&alt_key('H')), "Alt-H");
        assert_eq!(format_key_notation(&alt_key(',')), "Alt-,");
        assert_eq!(format_key_notation(&alt_key('.')), "Alt-.");
        assert_eq!(format_key_notation(&alt_key(' ')), "Alt-Space");
        assert_eq!(
            format_key_notation(&NormalizedKeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL
            )),
            "Ctrl-b"
        );
    }

    // -- SessionManagerBindings tests -----------------------------------------

    #[test]
    fn session_manager_bindings_defaults() {
        let b = SessionManagerBindings::default();
        // All 15 default chords are present.
        assert_eq!(b.len(), 15);
        assert_eq!(b.chord('t', 'n'), Some(SessionManagerBinding::TabNew));
        assert_eq!(b.chord('t', 'r'), Some(SessionManagerBinding::TabRename));
        assert_eq!(b.chord('s', 'x'), Some(SessionManagerBinding::SessionClose));
        assert_eq!(b.chord('f', 'r'), Some(SessionManagerBinding::FolderRename));
        // First chars of the 2-char chords are prefixes.
        assert!(b.is_prefix('t'));
        assert!(b.is_prefix('p'));
        assert!(b.is_prefix('s'));
        assert!(b.is_prefix('f'));
        // Legacy nav keys are NOT prefixes (so they still fall through).
        assert!(!b.is_prefix('j'));
        assert!(!b.is_prefix('d'));
        assert!(!b.is_prefix('q'));
    }

    #[test]
    fn session_manager_bindings_absent_section_is_defaults() {
        // An empty table (section absent) yields exactly the defaults.
        let value = toml::Value::Table(toml::map::Map::new());
        let b = SessionManagerBindings::from_toml(&value);
        assert_eq!(b.len(), 15);
        assert_eq!(b.chord('t', 'n'), Some(SessionManagerBinding::TabNew));
    }

    #[test]
    fn session_manager_bindings_user_override_and_extend() {
        // Override an existing chord and add a brand-new one.
        let toml_str = r#"
            tn = "SessionNew"
            gg = "FolderNew"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let b = SessionManagerBindings::from_toml(&value);
        // Overridden.
        assert_eq!(b.chord('t', 'n'), Some(SessionManagerBinding::SessionNew));
        // Extended.
        assert_eq!(b.chord('g', 'g'), Some(SessionManagerBinding::FolderNew));
        assert!(b.is_prefix('g'));
        // Other defaults still present.
        assert_eq!(b.chord('p', 'x'), Some(SessionManagerBinding::PaneClose));
    }

    #[test]
    fn session_manager_bindings_invalid_chord_skipped() {
        // A 3-char chord is invalid and must be skipped, leaving defaults intact.
        let toml_str = r#"
            abc = "TabNew"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let b = SessionManagerBindings::from_toml(&value);
        assert_eq!(b.len(), 15);
        assert_eq!(b.chord('t', 'n'), Some(SessionManagerBinding::TabNew));
    }

    #[test]
    fn session_manager_bindings_unknown_action_skipped() {
        // Unknown action name -> the chord keeps its default binding.
        let toml_str = r#"
            tn = "NotARealAction"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let b = SessionManagerBindings::from_toml(&value);
        assert_eq!(b.chord('t', 'n'), Some(SessionManagerBinding::TabNew));
    }

    #[test]
    fn session_manager_bindings_prefix_shadows_single_char() {
        // 't' begins the default 2-char chords, so a single-char 't' binding is
        // dropped (prefix wins).
        let toml_str = r#"
            t = "SessionNew"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let b = SessionManagerBindings::from_toml(&value);
        assert!(b.is_prefix('t'));
        // The single-char 't' binding was pruned.
        assert_eq!(b.single('t'), None);
        // 2-char 't' chords still resolve.
        assert_eq!(b.chord('t', 'n'), Some(SessionManagerBinding::TabNew));
    }

    #[test]
    fn session_manager_bindings_empty_value_unbinds() {
        let toml_str = r#"
            tn = ""
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let b = SessionManagerBindings::from_toml(&value);
        assert_eq!(b.chord('t', 'n'), None);
        // Other 't' chords remain, so 't' is still a prefix.
        assert!(b.is_prefix('t'));
        assert_eq!(b.chord('t', 'r'), Some(SessionManagerBinding::TabRename));
    }

    #[test]
    fn session_manager_bindings_single_char_binding_fires() {
        // A single-char chord whose char is NOT a prefix survives and resolves
        // via `single`.
        let toml_str = r#"
            z = "SessionNew"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let b = SessionManagerBindings::from_toml(&value);
        assert!(!b.is_prefix('z'));
        assert_eq!(b.single('z'), Some(SessionManagerBinding::SessionNew));
    }
}
