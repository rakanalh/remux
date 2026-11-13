use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

use crate::protocol::{action_spec, Action, RemuxCommand};

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// An arrow key, in h/j/k/l order so that sorted arrows read like the letters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Arrow {
    Left,
    Down,
    Up,
    Right,
}

impl Arrow {
    fn name(self) -> &'static str {
        match self {
            Arrow::Left => "Left",
            Arrow::Down => "Down",
            Arrow::Up => "Up",
            Arrow::Right => "Right",
        }
    }

    fn glyph(self) -> char {
        match self {
            Arrow::Left => '\u{2190}',
            Arrow::Down => '\u{2193}',
            Arrow::Up => '\u{2191}',
            Arrow::Right => '\u{2192}',
        }
    }

    fn from_code(code: KeyCode) -> Option<Self> {
        match code {
            KeyCode::Left => Some(Arrow::Left),
            KeyCode::Down => Some(Arrow::Down),
            KeyCode::Up => Some(Arrow::Up),
            KeyCode::Right => Some(Arrow::Right),
            _ => None,
        }
    }
}

/// One key of a prefix-tree path: a character, or an arrow with or without
/// Shift.
///
/// Characters order before arrows. `path_for` breaks ties on the lowest key,
/// so a binding hint names `h` rather than `Left` when both run an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TreeKey {
    Char(char),
    Arrow { arrow: Arrow, shift: bool },
}

impl TreeKey {
    /// The tree key a key press in Command mode stands for. Ctrl and Alt
    /// never reach the tree, and only an arrow keeps a Shift bit, because a
    /// character's case already records Shift.
    pub fn from_key_event(key: &KeyEvent) -> Option<Self> {
        let chord = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if chord {
            return None;
        }
        if let KeyCode::Char(c) = key.code {
            return Some(TreeKey::Char(c));
        }
        let arrow = Arrow::from_code(key.code)?;
        Some(TreeKey::Arrow {
            arrow,
            shift: key.modifiers.contains(KeyModifiers::SHIFT),
        })
    }

    /// Parse a TOML key of `[keybindings.command]`: one character, or an
    /// arrow name (`Left`, `Shift-Left`, ...). Anything else is not a tree
    /// key, which keeps `leader` and other multi-character keys out of the
    /// tree.
    pub fn parse(s: &str) -> Option<Self> {
        let mut chars = s.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return Some(TreeKey::Char(c));
        }
        let (name, shift) = match s.strip_prefix("Shift-") {
            Some(rest) => (rest, true),
            None => (s, false),
        };
        let arrow = [Arrow::Left, Arrow::Down, Arrow::Up, Arrow::Right]
            .into_iter()
            .find(|a| a.name() == name)?;
        Some(TreeKey::Arrow { arrow, shift })
    }

    /// The key as the which-key popup shows it.
    pub fn label(self) -> String {
        match self {
            TreeKey::Char(' ') => "\u{2423}".to_string(),
            TreeKey::Char(c) => c.to_string(),
            TreeKey::Arrow { arrow, shift } => arrow_glyph(arrow, shift),
        }
    }
}

/// The config spelling, the inverse of [`TreeKey::parse`].
impl std::fmt::Display for TreeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TreeKey::Char(c) => write!(f, "{c}"),
            TreeKey::Arrow { arrow, shift: true } => write!(f, "Shift-{}", arrow.name()),
            TreeKey::Arrow {
                arrow,
                shift: false,
            } => f.write_str(arrow.name()),
        }
    }
}

fn arrow_glyph(arrow: Arrow, shift: bool) -> String {
    if shift {
        format!("\u{21e7}{}", arrow.glyph())
    } else {
        arrow.glyph().to_string()
    }
}

/// A prefix-tree path written as characters, for tests.
#[cfg(test)]
pub fn tree_path(keys: &str) -> Vec<TreeKey> {
    keys.chars().map(TreeKey::Char).collect()
}

/// A node in the keybinding tree. Either a group of sub-keys or a leaf that
/// maps to one or more action strings (an action chain).
#[derive(Debug, Clone)]
pub enum KeyNode {
    /// An intermediate group that contains sub-keys.
    Group {
        label: String,
        children: HashMap<TreeKey, KeyNode>,
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
    pub root: HashMap<TreeKey, KeyNode>,
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
        children: char_keyed(children),
        sticky: false,
    }
}

/// A group whose h/j/k/l (and H/J/K/L) leaves are also bound to the arrows
/// (and Shift+arrows), so a direction can be picked either way.
fn group_with_arrows(label: &str, children: Vec<(char, KeyNode)>) -> KeyNode {
    KeyNode::Group {
        label: label.to_string(),
        children: with_arrow_aliases(children),
        sticky: false,
    }
}

fn char_keyed(children: Vec<(char, KeyNode)>) -> HashMap<TreeKey, KeyNode> {
    children
        .into_iter()
        .map(|(c, node)| (TreeKey::Char(c), node))
        .collect()
}

fn with_arrow_aliases(children: Vec<(char, KeyNode)>) -> HashMap<TreeKey, KeyNode> {
    let mut map = HashMap::new();
    for (c, node) in children {
        if let Some(alias) = arrow_alias(c) {
            map.insert(alias, node.clone());
        }
        map.insert(TreeKey::Char(c), node);
    }
    map
}

/// The vim direction key for an arrow, the inverse of [`arrow_alias`].
fn arrow_letter(arrow: Arrow, shift: bool) -> char {
    let letter = match arrow {
        Arrow::Left => 'h',
        Arrow::Down => 'j',
        Arrow::Up => 'k',
        Arrow::Right => 'l',
    };
    if shift {
        letter.to_ascii_uppercase()
    } else {
        letter
    }
}

/// The arrow that means the same direction as a vim direction key.
fn arrow_alias(c: char) -> Option<TreeKey> {
    let arrow = match c.to_ascii_lowercase() {
        'h' => Arrow::Left,
        'j' => Arrow::Down,
        'k' => Arrow::Up,
        'l' => Arrow::Right,
        _ => return None,
    };
    Some(TreeKey::Arrow {
        arrow,
        shift: c.is_ascii_uppercase(),
    })
}

/// Helper to build a sticky group node. Executing a leaf in a sticky group
/// keeps the which-key menu open so the user can keep triggering actions.
fn group_sticky_with_arrows(label: &str, children: Vec<(char, KeyNode)>) -> KeyNode {
    KeyNode::Group {
        label: label.to_string(),
        children: with_arrow_aliases(children),
        sticky: true,
    }
}

fn build_default_tree() -> HashMap<TreeKey, KeyNode> {
    let mut root = HashMap::new();

    // p: Pane
    root.insert(
        'p',
        group_with_arrows(
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
                    leaf_chain("move/eject left", &["PaneMoveLeft", "EnterNormal"]),
                ),
                (
                    'J',
                    leaf_chain("move/eject down", &["PaneMoveDown", "EnterNormal"]),
                ),
                (
                    'K',
                    leaf_chain("move/eject up", &["PaneMoveUp", "EnterNormal"]),
                ),
                (
                    'L',
                    leaf_chain("move/eject right", &["PaneMoveRight", "EnterNormal"]),
                ),
                ('z', leaf_chain("zoom", &["PaneToggleZoom", "EnterNormal"])),
                ('o', leaf_chain("popup", &["PopupToggle", "EnterNormal"])),
                ('m', leaf_chain("set master", &["SetMaster", "EnterNormal"])),
                ('r', leaf("rename", "PaneRename")),
                ('t', leaf("move to tab", "PaneMoveToTab")),
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
                    'S',
                    group_with_arrows(
                        "Stack into",
                        vec![
                            (
                                'h',
                                leaf_chain("left", &["PaneStackIntoLeft", "EnterNormal"]),
                            ),
                            (
                                'j',
                                leaf_chain("down", &["PaneStackIntoDown", "EnterNormal"]),
                            ),
                            ('k', leaf_chain("up", &["PaneStackIntoUp", "EnterNormal"])),
                            (
                                'l',
                                leaf_chain("right", &["PaneStackIntoRight", "EnterNormal"]),
                            ),
                        ],
                    ),
                ),
                (
                    'U',
                    group_with_arrows(
                        "Unstack",
                        vec![
                            ('h', leaf_chain("left", &["PaneUnstackLeft", "EnterNormal"])),
                            ('j', leaf_chain("down", &["PaneUnstackDown", "EnterNormal"])),
                            ('k', leaf_chain("up", &["PaneUnstackUp", "EnterNormal"])),
                            (
                                'l',
                                leaf_chain("right", &["PaneUnstackRight", "EnterNormal"]),
                            ),
                        ],
                    ),
                ),
                (
                    'R',
                    group_sticky_with_arrows(
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
                (
                    'm',
                    group_with_arrows(
                        "Move",
                        vec![
                            ('h', leaf_chain("left", &["TabMoveBy -1", "EnterNormal"])),
                            ('l', leaf_chain("right", &["TabMoveBy 1", "EnterNormal"])),
                        ],
                    ),
                ),
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
                ('a', leaf("switch agent", "AgentQuickSwitch")),
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

    // w: View (client-only virtual tabs). `v` is taken by visual mode, so the
    // view group lives on `w` ("view/window"). Every action here is client-only
    // and intercepted in the input handler before command parsing.
    root.insert(
        'w',
        group(
            "View",
            vec![
                ('n', leaf("new view", "ViewNew")),
                ('a', leaf("add pane", "ViewAddPane")),
                ('r', leaf("rename view", "ViewRename")),
                ('x', leaf("remove cell", "ViewRemovePane")),
                (' ', leaf("layout next", "ViewLayoutNext")),
                ('q', leaf("close view", "ViewClose")),
                ('d', leaf("delete view", "ViewDelete")),
            ],
        ),
    );

    // b: sideBar (client-only chrome). `b` was the old Buffer group, freed by
    // the search-mode refactor, and nothing else claims it at root.
    //
    // Cycle sits on `b` rather than the Tab key the plan named: the which-key
    // tree holds only characters and arrows (`TreeKey`), so `KeyCode::Tab`
    // never reaches the tree and a `'\t'` entry would be unreachable. `b b`
    // reads as "sidebar again".
    root.insert(
        'b',
        group(
            "Sidebar",
            vec![
                ('h', leaf("toggle left", "SidebarToggleLeft")),
                ('l', leaf("toggle right", "SidebarToggleRight")),
                ('j', leaf("toggle bottom", "SidebarToggleBottom")),
                // No default FOCUS leaves. `Alt+h/j/k/l` already enter and
                // leave a sidebar exactly as they move between panes, so a
                // second way in would be redundant surface on the busiest
                // group. The `SidebarFocus*` actions stay registered and
                // bindable from config for anyone who wants them.
                ('b', leaf("cycle focus", "SidebarCycle")),
            ],
        ),
    );

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
    root.insert('L', leaf("switch layout", "LayoutQuickSwitch"));
    root.insert(
        'z',
        leaf_chain("zoom pane", &["PaneToggleZoom", "EnterNormal"]),
    );

    // Command palette.
    root.insert(':', leaf("command palette", "CommandPaletteOpen"));

    root.insert('r', leaf("redraw", "Redraw"));

    // Send the prefix key (Ctrl-a) to the terminal.
    root.insert(
        'a',
        leaf_chain("send prefix", &["SendKey Ctrl-a", "EnterNormal"]),
    );

    char_keyed(root.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

impl KeybindingTree {
    /// Look up the node at the given key path (e.g. `t n` maps to the
    /// `TabNew` leaf).
    pub fn lookup(&self, path: &[TreeKey]) -> Option<&KeyNode> {
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

    /// Return the `(key, label)` pairs for all children at the given path,
    /// with each key as the which-key popup shows it.
    ///
    /// An arrow bound to the same action chain as a character sibling is
    /// folded into that sibling's key (`h/\u{2190}`), so the arrow aliases
    /// do not double the popup. An arrow with no such twin, for example
    /// after a user rebinds `h`, keeps a row of its own.
    ///
    /// If `path` is empty, returns the root-level children.
    /// Returns `None` if the path does not lead to a group.
    pub fn children_at(&self, path: &[TreeKey]) -> Option<Vec<(String, String)>> {
        let children = if path.is_empty() {
            &self.root
        } else {
            match self.lookup(path)? {
                KeyNode::Group { children, .. } => children,
                KeyNode::Leaf { .. } => return None,
            }
        };

        let mut sorted: Vec<(&TreeKey, &KeyNode)> = children.iter().collect();
        sorted.sort_by_key(|(key, _)| display_order(**key));

        let mut result: Vec<(TreeKey, String, String)> = Vec::new();
        for (&key, node) in sorted {
            if let (TreeKey::Arrow { arrow, shift }, KeyNode::Leaf { action, .. }) = (key, node) {
                let same_chain = |k: &TreeKey| {
                    matches!(k, TreeKey::Char(_))
                        && matches!(children.get(k), Some(KeyNode::Leaf { action: a, .. }) if a == action)
                };
                // The arrow's own letter wins over another letter that happens
                // to run the same chain, so Left folds into `h`.
                let own = TreeKey::Char(arrow_letter(arrow, shift));
                let twin = match result
                    .iter()
                    .position(|(k, _, _)| *k == own && same_chain(k))
                {
                    Some(i) => Some(i),
                    None => result.iter().position(|(k, _, _)| same_chain(k)),
                };
                if let Some((_, shown, _)) = twin.map(|i| &mut result[i]) {
                    shown.push('/');
                    shown.push_str(&key.label());
                    continue;
                }
            }
            let label = match node {
                KeyNode::Group { label, .. } | KeyNode::Leaf { label, .. } => label.clone(),
            };
            result.push((key, key.label(), label));
        }
        Some(
            result
                .into_iter()
                .map(|(_, shown, label)| (shown, label))
                .collect(),
        )
    }

    /// Collect every leaf in the tree as `(key path, action chain)`, depth
    /// first. Used by config validation to name the binding that carries a
    /// bad action string.
    pub fn leaves(&self) -> Vec<(Vec<TreeKey>, Vec<String>)> {
        let mut out = Vec::new();
        collect_leaves(&self.root, &mut Vec::new(), &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The key path that runs `action`, as the first non-`EnterNormal` action
    /// of a leaf's chain. When several leaves run it, the shortest path wins,
    /// then the lowest keys, so the answer does not depend on map order.
    pub fn path_for(&self, action: &str) -> Option<Vec<TreeKey>> {
        self.leaves()
            .into_iter()
            .filter(|(_, chain)| {
                chain
                    .iter()
                    .find(|a| a.as_str() != "EnterNormal")
                    .map(String::as_str)
                    == Some(action)
            })
            .map(|(path, _)| path)
            .min_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
    }

    /// Merge another tree on top of this one. Keys in `overrides` replace keys
    /// in `self`. Groups are merged recursively; leaves are replaced outright.
    /// A leaf with an empty action string removes that key from the base.
    pub fn merge(&mut self, overrides: &KeybindingTree) {
        merge_maps(&mut self.root, &overrides.root);
    }
}

/// The which-key order: characters by the glyph shown (so the space key's
/// `\u{2423}` sorts after the letters), then arrows.
fn display_order(key: TreeKey) -> (u8, char, Option<(Arrow, bool)>) {
    match key {
        TreeKey::Char(' ') => (0, '\u{2423}', None),
        TreeKey::Char(c) => (0, c, None),
        TreeKey::Arrow { arrow, shift } => (1, ' ', Some((arrow, shift))),
    }
}

fn collect_leaves(
    nodes: &HashMap<TreeKey, KeyNode>,
    path: &mut Vec<TreeKey>,
    out: &mut Vec<(Vec<TreeKey>, Vec<String>)>,
) {
    for (&key, node) in nodes {
        path.push(key);
        match node {
            KeyNode::Group { children, .. } => collect_leaves(children, path, out),
            KeyNode::Leaf { action, .. } => out.push((path.clone(), action.clone())),
        }
        path.pop();
    }
}

fn merge_maps(base: &mut HashMap<TreeKey, KeyNode>, overrides: &HashMap<TreeKey, KeyNode>) {
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
    /// A letter's case is its Shift state, so a cased letter is stored as the
    /// shifted character WITHOUT the `SHIFT` bit.
    ///
    /// Bindings are written as `Alt-L`, which parses to `'L'` + `ALT`, but
    /// crossterm reports the `ESC L` a terminal sends for Alt+Shift+L as
    /// `'L'` + `ALT | SHIFT`, and the CSI-u form `CSI 108;4u` as `'l'` +
    /// `ALT | SHIFT`. Without this the exact-match lookup misses both, and the
    /// keys reach the pane instead. A character without case keeps its `SHIFT`
    /// bit: the shifted character of `'1'` depends on the keyboard layout, and
    /// an uncased letter such as `'あ'` or `'ب'` has no shifted form, so the
    /// `SHIFT` bit is the only record that Shift was held.
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        if let KeyCode::Char(c) = code {
            if modifiers.contains(KeyModifiers::SHIFT) && (c.is_lowercase() || c.is_uppercase()) {
                // `ß` uppercases to `SS`; taking the first char would turn it
                // into an `S` binding, so a multi-char uppercase is left alone.
                let mut upper = c.to_uppercase();
                if let (Some(shifted), None) = (upper.next(), upper.next()) {
                    return Self {
                        code: KeyCode::Char(shifted),
                        modifiers: modifiers - KeyModifiers::SHIFT,
                    };
                }
            }
        }
        Self { code, modifiers }
    }
}

impl From<&KeyEvent> for NormalizedKeyEvent {
    fn from(event: &KeyEvent) -> Self {
        Self::new(event.code, event.modifiers)
    }
}

impl From<KeyEvent> for NormalizedKeyEvent {
    fn from(event: KeyEvent) -> Self {
        Self::new(event.code, event.modifiers)
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterceptAction {
    /// Execute commands and stay in Normal mode.
    Command(Vec<String>),
    /// Enter Command mode at the given keybinding tree group path.
    GroupPrefix(Vec<TreeKey>),
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

    /// The notation of the shortcut that runs `action` alone, as the first
    /// non-`EnterNormal` command of its chain. Ties break on the notation, so
    /// the answer does not depend on map order.
    pub fn notation_for(&self, action: &str) -> Option<String> {
        self.bindings
            .iter()
            .filter_map(|(key, bound)| match bound {
                InterceptAction::Command(cmds)
                    if cmds
                        .iter()
                        .find(|c| c.as_str() != "EnterNormal")
                        .map(String::as_str)
                        == Some(action) =>
                {
                    Some((
                        !matches!(key.code, KeyCode::Char(_)),
                        format_key_notation(key),
                    ))
                }
                _ => None,
            })
            .min()
            .map(|(_, notation)| notation)
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
                        let path_str: String = path.iter().map(TreeKey::to_string).collect();
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
                InterceptAction::GroupPrefix(group_path.chars().map(TreeKey::Char).collect())
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
    /// - An arrow shortcut that runs the same action as a character shortcut
    ///   with the same Ctrl and Alt bits is folded into it (`Alt-h/\u{2190}`),
    ///   so the arrow aliases do not double the popup's Alt section.
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut sorted: Vec<(&NormalizedKeyEvent, &InterceptAction)> =
            self.bindings.iter().collect();
        // Characters first, so an arrow's twin is already in `result`.
        sorted.sort_by_key(|(key, _)| {
            (
                Arrow::from_code(key.code).is_some(),
                format_key_notation(key),
            )
        });

        let mut result: Vec<(&NormalizedKeyEvent, String, String)> = Vec::new();
        for (key, action) in sorted {
            if let Some(arrow) = Arrow::from_code(key.code) {
                let chord = key.modifiers - KeyModifiers::SHIFT;
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let same_action = |k: &NormalizedKeyEvent| {
                    matches!(k.code, KeyCode::Char(_))
                        && k.modifiers == chord
                        && self.bindings.get(k) == Some(action)
                };
                let own = KeyCode::Char(arrow_letter(arrow, shift));
                let twin = match result
                    .iter()
                    .position(|(k, _, _)| k.code == own && same_action(k))
                {
                    Some(i) => Some(i),
                    None => result.iter().position(|(k, _, _)| same_action(k)),
                };
                if let Some((_, notation, _)) = twin.map(|i| &mut result[i]) {
                    notation.push('/');
                    notation.push_str(&arrow_glyph(
                        arrow,
                        key.modifiers.contains(KeyModifiers::SHIFT),
                    ));
                    continue;
                }
            }
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
                    let path_str: String = path.iter().map(TreeKey::to_string).collect();
                    format!("\u{2192} {path_str}")
                }
            };
            result.push((key, format_key_notation(key), label));
        }
        let mut result: Vec<(String, String)> = result
            .into_iter()
            .map(|(_, notation, label)| (notation, label))
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

    // Friendlier phrasings come from the action registry, so there is no
    // separate list of labels to keep in step.
    if let Some(label) = action_spec(name).and_then(|spec| spec.label) {
        return label.to_string();
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

/// Alt+arrow, or Alt+Shift+arrow. Unlike a letter, an arrow keeps its
/// `SHIFT` bit, which matches the `CSI 1;4D` that xterm sends for
/// Alt+Shift+Left.
fn alt_arrow(code: KeyCode, shift: bool) -> NormalizedKeyEvent {
    let modifiers = if shift {
        KeyModifiers::ALT | KeyModifiers::SHIFT
    } else {
        KeyModifiers::ALT
    };
    NormalizedKeyEvent::new(code, modifiers)
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
        bindings.insert(alt_arrow(KeyCode::Left, false), cmd("PaneFocusLeft"));
        bindings.insert(alt_arrow(KeyCode::Down, false), cmd("PaneFocusDown"));
        bindings.insert(alt_arrow(KeyCode::Up, false), cmd("PaneFocusUp"));
        bindings.insert(alt_arrow(KeyCode::Right, false), cmd("PaneFocusRight"));

        // Move panes directionally (Alt-Shift-h/j/k/l -> Alt-H/J/K/L). These
        // match `parse_key_notation("Alt-H")`, i.e. an uppercase char with the
        // Alt modifier.
        bindings.insert(alt_key('H'), cmd("PaneMoveLeft"));
        bindings.insert(alt_key('J'), cmd("PaneMoveDown"));
        bindings.insert(alt_key('K'), cmd("PaneMoveUp"));
        bindings.insert(alt_key('L'), cmd("PaneMoveRight"));
        bindings.insert(alt_arrow(KeyCode::Left, true), cmd("PaneMoveLeft"));
        bindings.insert(alt_arrow(KeyCode::Down, true), cmd("PaneMoveDown"));
        bindings.insert(alt_arrow(KeyCode::Up, true), cmd("PaneMoveUp"));
        bindings.insert(alt_arrow(KeyCode::Right, true), cmd("PaneMoveRight"));

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
        bindings.insert(alt_key('a'), cmd("AgentQuickSwitch"));
        bindings.insert(alt_key('g'), cmd("LayoutQuickSwitch"));
        bindings.insert(alt_key('o'), cmd("SessionSwitchLast"));
        bindings.insert(alt_key('z'), cmd("PaneToggleZoom"));
        bindings.insert(alt_key('p'), cmd("PopupToggle"));
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
        let root = parse_toml_table(table, true);
        Some(KeybindingTree { root })
    }
}

fn parse_toml_table(
    table: &toml::map::Map<String, toml::Value>,
    is_root: bool,
) -> HashMap<TreeKey, KeyNode> {
    let mut map = HashMap::new();

    for (key, value) in table {
        // Skip metadata keys (e.g. `_label`).
        if key.starts_with('_') {
            continue;
        }

        let Some(tree_key) = TreeKey::parse(key) else {
            continue;
        };
        // The root table is shared with the modifier shortcuts, and
        // `ShortcutBindings::from_toml` already reads a root `Shift-Left` as
        // a Normal-mode shortcut. Reading it as a tree key as well would bind
        // one line in two places, so Shift+arrow keys count only inside a
        // group.
        if is_root && matches!(tree_key, TreeKey::Arrow { shift: true, .. }) {
            continue;
        }

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
                let children = parse_toml_table(sub, false);
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

        map.insert(tree_key, node);
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

/// Resolve an action string (e.g. `"TabNew"`, `"ResizeLeft 5"`, `"ViewNew"`)
/// into the [`Action`] it names.
///
/// Every action string in the program -- keybinding leaves, Alt shortcuts and
/// what the user picks in the command palette -- goes through here. The first
/// step is a lookup in [`action_specs`]: a name that is not in the registry
/// resolves to `None`, so the registry (and therefore the palette listing) can
/// never fall out of step with what is actually bindable.
///
/// Returns `None` if the name is unknown or its arguments are unusable.
pub fn resolve_action(input: &str) -> Option<Action> {
    let tokens = tokenize_command(input);
    let name = tokens.first()?;
    let spec = action_spec(name)?;
    match spec.client {
        Some(action) => Some(Action::Client(action)),
        None => build_command(spec.name, &tokens[1..]).map(Action::Server),
    }
}

/// Parse a PascalCase command string (e.g. `"TabNew"`, `"ResizeLeft 5"`) into
/// a [`RemuxCommand`].
///
/// Returns `None` if the command string is not recognised, or if it names a
/// client-only action (use [`resolve_action`] to handle those too).
pub fn parse_command(input: &str) -> Option<RemuxCommand> {
    match resolve_action(input)? {
        Action::Server(cmd) => Some(cmd),
        Action::Client(_) => None,
    }
}

/// Describe what is wrong with a single action string, or `None` if it is
/// fine. An empty string is the "unbind this key" sentinel, not an error.
fn action_problem(action: &str) -> Option<String> {
    let tokens = tokenize_command(action);
    let name = tokens.first()?;
    match action_spec(name) {
        None => Some(format!("unknown action '{name}'")),
        Some(_) if name == "PopupToggle" && resolve_action(action).is_none() => {
            let reason = match crate::config::PopupPlacement::from_args(&tokens[1..]) {
                Err(reason) => reason,
                // `resolve_action` builds the command from the same parse, so
                // it cannot fail where `from_args` succeeds.
                Ok(_) => "arguments that do not resolve".to_string(),
            };
            Some(format!("action 'PopupToggle' has {reason}"))
        }
        Some(_) if resolve_action(action).is_none() => {
            Some(format!("action '{name}' is missing a required argument"))
        }
        Some(_) => None,
    }
}

/// Report every bound action string that does not resolve, naming the binding
/// that carries it.
///
/// This is what turns a typo in `[keybindings.command]` from a key that
/// silently does nothing into an error at config-load time. Returns one
/// message per bad action; an empty vector means every binding resolves.
///
/// `tree` is the EFFECTIVE (merged) tree, so a leaf that came from the
/// deprecated `[keybindings.normal]` alias is reported under
/// `[keybindings.command]` -- which is where the deprecation warning tells the
/// user to move it anyway.
pub fn unresolved_actions(tree: &KeybindingTree, shortcuts: &ShortcutBindings) -> Vec<String> {
    let mut problems = Vec::new();

    for (path, actions) in tree.leaves() {
        let key_path: Vec<String> = path.iter().map(TreeKey::to_string).collect();
        let key_path = key_path.join(" ");
        for action in &actions {
            if let Some(problem) = action_problem(action) {
                problems.push(format!(
                    "invalid keybinding '{key_path}' in [keybindings.command]: {problem}"
                ));
            }
        }
    }

    let mut shortcut_problems: Vec<String> = Vec::new();
    for (key, action) in &shortcuts.bindings {
        let actions = match action {
            InterceptAction::Command(cmds) => cmds,
            // Group prefixes are checked by `validate_group_refs`.
            InterceptAction::GroupPrefix(_) => continue,
        };
        for action in actions {
            if let Some(problem) = action_problem(action) {
                shortcut_problems.push(format!(
                    "invalid shortcut '{}' in [keybindings.command]: {problem}",
                    format_key_notation(key)
                ));
            }
        }
    }
    // `bindings` is a HashMap, so sort for a stable report order.
    shortcut_problems.sort();
    problems.extend(shortcut_problems);

    problems
}

/// Build the [`RemuxCommand`] for a registry name that is not client-only.
///
/// Only ever called with a `name` that [`action_specs`] lists, so the trailing
/// `None` arm is unreachable in practice -- the round-trip test asserts every
/// server entry in the registry is handled here.
fn build_command(name: &str, args: &[String]) -> Option<RemuxCommand> {
    match name {
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
        "PaneStackIntoLeft" => Some(RemuxCommand::PaneStackIntoLeft),
        "PaneStackIntoRight" => Some(RemuxCommand::PaneStackIntoRight),
        "PaneStackIntoUp" => Some(RemuxCommand::PaneStackIntoUp),
        "PaneStackIntoDown" => Some(RemuxCommand::PaneStackIntoDown),
        "PaneUnstackLeft" => Some(RemuxCommand::PaneUnstackLeft),
        "PaneUnstackRight" => Some(RemuxCommand::PaneUnstackRight),
        "PaneUnstackUp" => Some(RemuxCommand::PaneUnstackUp),
        "PaneUnstackDown" => Some(RemuxCommand::PaneUnstackDown),
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
        "PaneToggleZoomFullscreen" => Some(RemuxCommand::PaneToggleZoomFullscreen),
        "PaneToggleZoomFloat" => Some(RemuxCommand::PaneToggleZoomFloat),
        "PopupToggle" => crate::config::PopupPlacement::from_args(args)
            .ok()
            .map(RemuxCommand::PopupToggle),
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
        "LayoutSet" => args.first().map(|s| RemuxCommand::LayoutSet(s.to_string())),
        // A destination/alias is a single token (no spaces), so `args.first()`
        // is correct here -- returns `None` when no argument is given.
        "RemoteConnect" => args.first().map(|s| RemuxCommand::RemoteConnect(s.clone())),
        "RemoteDisconnect" => args
            .first()
            .map(|s| RemuxCommand::RemoteDisconnect(s.clone())),

        // -- usize arg commands -----------------------------------------------
        "TabGoto" => {
            let idx = args.first()?.parse().ok()?;
            Some(RemuxCommand::TabGoto(idx))
        }
        "TabMove" => {
            let idx = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            Some(RemuxCommand::TabMove(idx))
        }

        // -- i32 arg commands -------------------------------------------------
        "TabMoveBy" => {
            let delta = args.first()?.parse().ok()?;
            Some(RemuxCommand::TabMoveBy(delta))
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
    // Modifiers stack in any order (`Alt-Shift-Left`), because
    // `format_key_notation` writes them that way and a user copies a
    // shortcut's spelling from the which-key popup or an error message.
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = notation;
    loop {
        let (bit, tail) = if let Some(tail) = rest.strip_prefix("Ctrl-") {
            (KeyModifiers::CONTROL, tail)
        } else if let Some(tail) = rest.strip_prefix("Alt-") {
            (KeyModifiers::ALT, tail)
        } else if let Some(tail) = rest.strip_prefix("Shift-") {
            (KeyModifiers::SHIFT, tail)
        } else {
            break;
        };
        modifiers |= bit;
        rest = tail;
    }

    // Shift-Tab is represented as BackTab in crossterm.
    let (code, implied) = if modifiers.contains(KeyModifiers::SHIFT) && rest == "Tab" {
        (KeyCode::BackTab, KeyModifiers::NONE)
    } else {
        parse_key_code(rest)?
    };
    Some(KeyEvent::new_with_kind_and_state(
        code,
        modifiers | implied,
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
    TabMoveToSession,
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
    /// Add the marked panes (or the highlighted pane) to a client-only view.
    AddToView,
    /// Rename the highlighted view (or the view of the highlighted cell).
    ViewRename,
    /// Remove the highlighted cell from its view (the pane itself is untouched).
    ViewRemoveCell,
    /// Delete the highlighted view (or the view of the highlighted cell).
    ViewDelete,
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
            "TabMoveToSession" => Self::TabMoveToSession,
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
            "AddToView" => Self::AddToView,
            "ViewRename" => Self::ViewRename,
            "ViewRemoveCell" => Self::ViewRemoveCell,
            "ViewDelete" => Self::ViewDelete,
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
        ("tm", TabMoveToSession),
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
        ("va", AddToView),
        ("vr", ViewRename),
        ("vx", ViewRemoveCell),
        ("vd", ViewDelete),
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
    #[test]
    fn a_shortcut_that_runs_an_action_names_it() {
        let shortcuts = ShortcutBindings::default();
        assert_eq!(
            shortcuts.notation_for("PaneToggleZoom").as_deref(),
            Some("Alt-z")
        );
        assert_eq!(shortcuts.notation_for("PaneRename"), None);
    }

    #[test]
    fn a_leaf_that_runs_an_action_gives_its_key_path() {
        let tree = KeybindingTree::default();
        assert_eq!(tree.path_for("PaneClose"), Some(tree_path("px")));
        assert_eq!(tree.path_for("TabClose"), Some(tree_path("tx")));
        assert_eq!(tree.path_for("PaneRename"), Some(tree_path("pr")));
        assert_eq!(tree.path_for("NoSuchAction"), None);
    }

    use super::*;
    use crate::protocol::{action_specs, command_names, ClientAction};

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
    fn parse_command_popup_toggle_takes_an_optional_position() {
        use crate::config::PopupPosition::*;
        assert_eq!(
            parse_command("PopupToggle"),
            Some(RemuxCommand::PopupToggle(None))
        );
        for (arg, position) in [
            ("float", Float),
            ("left", Left),
            ("right", Right),
            ("top", Top),
            ("bottom", Bottom),
        ] {
            assert_eq!(
                parse_command(&format!("PopupToggle {arg}")),
                Some(RemuxCommand::PopupToggle(Some(position.into()))),
                "{arg}"
            );
        }
        assert_eq!(parse_command("PopupToggle sideways"), None);
    }

    #[test]
    fn a_bad_popup_position_is_a_named_config_error() {
        let toml_str = r#"
            "Alt-p" = "PopupToggle sideways"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let shortcuts = ShortcutBindings::from_toml(&value).unwrap();
        let tree = KeybindingTree::from_toml(&toml::Value::Table(Default::default())).unwrap();
        let problems = unresolved_actions(&tree, &shortcuts);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("'PopupToggle' has an invalid position 'sideways'"),
            "{problems:#?}"
        );
    }

    #[test]
    fn parse_command_popup_toggle_takes_an_optional_size() {
        use crate::config::{PopupPlacement, PopupPosition::*, PopupSize};
        let size = |position, width, height| {
            Some(RemuxCommand::PopupToggle(Some(PopupPlacement {
                position,
                size: Some(PopupSize { width, height }),
            })))
        };
        assert_eq!(
            parse_command("PopupToggle float 80"),
            size(Float, Some(80), Some(80))
        );
        assert_eq!(
            parse_command("PopupToggle float 80x60"),
            size(Float, Some(80), Some(60))
        );
        assert_eq!(
            parse_command("PopupToggle right 40"),
            size(Right, Some(40), None)
        );
        assert_eq!(
            parse_command("PopupToggle bottom 30"),
            size(Bottom, None, Some(30))
        );
        assert_eq!(
            parse_command("PopupToggle float 5"),
            size(Float, Some(20), Some(20)),
            "clamped"
        );
        for bad in [
            "PopupToggle 80",
            "PopupToggle float wide",
            "PopupToggle right 80x60",
            "PopupToggle float 80 60",
        ] {
            assert_eq!(parse_command(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_bad_popup_size_is_a_named_config_error() {
        for (action, want) in [
            (
                "PopupToggle 80",
                "'PopupToggle' has a size '80' that needs a position before it",
            ),
            (
                "PopupToggle float wide",
                "'PopupToggle' has an invalid size 'wide'",
            ),
            (
                "PopupToggle right 80x60",
                "'PopupToggle' has a size '80x60' for 'right' that takes one number, the width",
            ),
        ] {
            let toml_str = format!("\"Alt-p\" = \"{action}\"\n");
            let value: toml::Value = toml_str.parse().unwrap();
            let shortcuts = ShortcutBindings::from_toml(&value).unwrap();
            let tree = KeybindingTree::from_toml(&toml::Value::Table(Default::default())).unwrap();
            let problems = unresolved_actions(&tree, &shortcuts);
            assert_eq!(problems.len(), 1, "{action}: {problems:#?}");
            assert!(problems[0].contains(want), "{action}: {problems:#?}");
        }
    }

    #[test]
    fn the_popup_example_bindings_parse() {
        // The bindings the docs show, as TOML writes them.
        let toml_str = r#"
            "Alt-p" = "PopupToggle"
            "Alt--" = "PopupToggle bottom"
            "Alt-\\" = "PopupToggle right"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let shortcuts = ShortcutBindings::from_toml(&value).unwrap();
        for (c, command) in [
            ('p', "PopupToggle"),
            ('-', "PopupToggle bottom"),
            ('\\', "PopupToggle right"),
        ] {
            assert!(
                matches!(
                    shortcuts.bindings.get(&alt_key(c)),
                    Some(InterceptAction::Command(v)) if v == &[command.to_string()]
                ),
                "Alt-{c}"
            );
        }
        let tree = KeybindingTree::from_toml(&toml::Value::Table(Default::default())).unwrap();
        assert!(unresolved_actions(&tree, &shortcuts).is_empty());
    }

    #[test]
    fn parse_command_tab_move_default() {
        assert_eq!(parse_command("TabMove"), Some(RemuxCommand::TabMove(0)));
    }

    #[test]
    fn parse_command_tab_move_by() {
        assert_eq!(
            parse_command("TabMoveBy -1"),
            Some(RemuxCommand::TabMoveBy(-1))
        );
        assert_eq!(
            parse_command("TabMoveBy 1"),
            Some(RemuxCommand::TabMoveBy(1))
        );
        assert_eq!(parse_command("TabMoveBy"), None);
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
        // Disconnect names a remote from the roster, so the same shape applies:
        // one token, and no argument means no command rather than an empty name.
        assert_eq!(
            parse_command("RemoteDisconnect pi"),
            Some(RemuxCommand::RemoteDisconnect("pi".into()))
        );
        assert_eq!(parse_command("RemoteDisconnect"), None);
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
        let node = tree.lookup(&tree_path("xo")).unwrap();
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
        assert!(tree.root.contains_key(&TreeKey::Char('t')));
        assert!(tree.root.contains_key(&TreeKey::Char('p')));
        assert!(tree.root.contains_key(&TreeKey::Char('s'))); // Search group
        assert!(tree.root.contains_key(&TreeKey::Char('x'))); // Session group (moved from 's')
        assert!(tree.root.contains_key(&TreeKey::Char('z'))); // Zoom pane toggle
        assert!(!tree.root.contains_key(&TreeKey::Char('f'))); // moved to 'z'
        assert!(tree.root.contains_key(&TreeKey::Char('v')));
        assert!(tree.root.contains_key(&TreeKey::Char('g')));
        // 'b' was the Buffer group, removed in the search-mode refactor and
        // reclaimed by the Sidebar group.
        assert!(tree.root.contains_key(&TreeKey::Char('b')));
        // 'i' (EnterInsertMode) was removed in the leader-key-modes refactor.
        assert!(!tree.root.contains_key(&TreeKey::Char('i')));
    }

    /// Every leaf in the Sidebar group must name an action the registry knows,
    /// or the key is silently inert. `contains_key(&TreeKey::Char('b'))` alone would not catch
    /// a typo in an action string.
    #[test]
    fn sidebar_group_leaves_all_resolve() {
        let tree = KeybindingTree::default();
        let expected = [
            ('h', "SidebarToggleLeft"),
            ('l', "SidebarToggleRight"),
            ('j', "SidebarToggleBottom"),
            ('b', "SidebarCycle"),
        ];
        for (key, name) in expected {
            let node = tree
                .lookup(&[TreeKey::Char('b'), TreeKey::Char(key)])
                .unwrap_or_else(|| panic!("no leaf at 'b {key}'"));
            match node {
                KeyNode::Leaf { action, .. } => {
                    assert_eq!(action, &vec![name.to_string()], "'b {key}'");
                    assert!(
                        resolve_action(name).is_some(),
                        "'b {key}' names an action the registry does not have"
                    );
                }
                other => panic!("expected a leaf at 'b {key}', got {other:?}"),
            }
        }
    }

    /// The `SidebarFocus*` actions are deliberately UNBOUND by default --
    /// `Alt+h/j/k/l` already enter and leave a sidebar -- but they stay in the
    /// action registry so a user can bind them from config. Both halves matter:
    /// dropping the leaves while leaving a dangling action name, or removing
    /// the action outright, would each be wrong in a different direction.
    #[test]
    fn sidebar_focus_actions_are_unbound_by_default_but_still_bindable() {
        let tree = KeybindingTree::default();
        for (key, name) in [
            ('H', "SidebarFocusLeft"),
            ('L', "SidebarFocusRight"),
            ('J', "SidebarFocusBottom"),
        ] {
            assert!(
                tree.lookup(&[TreeKey::Char('b'), TreeKey::Char(key)])
                    .is_none(),
                "'b {key}' still has a default binding"
            );
            assert!(
                resolve_action(name).is_some(),
                "{name} is no longer bindable from config"
            );
        }
    }

    #[test]
    fn lookup_leaf() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&tree_path("tn")).unwrap();
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
        let node = tree.lookup(&tree_path("t")).unwrap();
        assert!(matches!(node, KeyNode::Group { .. }));
    }

    #[test]
    fn lookup_missing() {
        let tree = KeybindingTree::default();
        assert!(tree.lookup(&tree_path("q")).is_none());
        assert!(tree.lookup(&tree_path("tq")).is_none());
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
        assert!(children.iter().any(|(k, _)| k == "v"));
    }

    #[test]
    fn children_at_group() {
        let tree = KeybindingTree::default();
        let children = tree.children_at(&tree_path("t")).unwrap();
        let keys: Vec<&str> = children.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"n"));
        assert!(keys.contains(&"x"));
    }

    #[test]
    fn children_at_leaf_returns_none() {
        let tree = KeybindingTree::default();
        assert!(tree.children_at(&tree_path("tn")).is_none());
    }

    #[test]
    fn default_tree_has_toggle_style() {
        let tree = KeybindingTree::default();
        assert!(tree.root.contains_key(&TreeKey::Char('g')));
        let node = tree.lookup(&tree_path("g")).unwrap();
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
        let children = tree.children_at(&tree_path("p")).unwrap();
        assert!(
            children
                .iter()
                .any(|(k, label)| k == "a" && label == "stack add"),
            "expected 'a' -> 'stack add' in Pane group, got: {children:?}"
        );
    }

    #[test]
    fn pane_group_has_rename_leaf() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&tree_path("pr")).unwrap();
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
        let mv = tree.lookup(&tree_path("pH")).unwrap();
        match mv {
            KeyNode::Leaf { action, label, .. } => {
                assert!(action.contains(&"PaneMoveLeft".to_string()));
                assert_eq!(label, "move/eject left");
            }
            other => panic!("expected leaf for 'p' -> 'H', got {other:?}"),
        }
        // 'R' is now the Resize group.
        let resize = tree.lookup(&tree_path("pR")).unwrap();
        assert!(matches!(resize, KeyNode::Group { .. }));
        let down = tree.lookup(&tree_path("pRj")).unwrap();
        match down {
            KeyNode::Leaf { action, .. } => assert_eq!(action, &vec!["ResizeDown 5".to_string()]),
            other => panic!("expected leaf for 'p' -> 'R' -> 'j', got {other:?}"),
        }
    }

    #[test]
    fn tab_group_has_close_and_goto() {
        let tree = KeybindingTree::default();
        // 'x' closes the tab.
        let close = tree.lookup(&tree_path("tx")).unwrap();
        match close {
            KeyNode::Leaf { action, .. } => assert!(action.contains(&"TabClose".to_string())),
            other => panic!("expected leaf for 't' -> 'x', got {other:?}"),
        }
        // '1' jumps to the first tab (0-indexed).
        let goto = tree.lookup(&tree_path("t1")).unwrap();
        match goto {
            KeyNode::Leaf { action, .. } => assert!(action.contains(&"TabGoto 0".to_string())),
            other => panic!("expected leaf for 't' -> '1', got {other:?}"),
        }
    }

    #[test]
    fn session_group_has_switch_leaf() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&tree_path("xs")).unwrap();
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

        let next = tree.lookup(&tree_path("}")).unwrap();
        match next {
            KeyNode::Leaf { action, label, .. } => {
                assert!(action.contains(&"TabNext".to_string()));
                assert_eq!(label, "next tab");
            }
            other => panic!("expected leaf for '}}', got {other:?}"),
        }

        let prev = tree.lookup(&tree_path("{")).unwrap();
        match prev {
            KeyNode::Leaf { action, label, .. } => {
                assert!(action.contains(&"TabPrev".to_string()));
                assert_eq!(label, "prev tab");
            }
            other => panic!("expected leaf for '{{', got {other:?}"),
        }
    }

    #[test]
    fn default_tree_tab_move_is_a_left_right_group() {
        let tree = KeybindingTree::default();
        match tree.lookup(&tree_path("tm")).unwrap() {
            KeyNode::Group {
                label,
                children,
                sticky,
            } => {
                assert_eq!(label, "Move");
                assert!(!sticky);
                let mut keys: Vec<TreeKey> = children.keys().copied().collect();
                keys.sort_unstable();
                let mut expected = tree_path("hl");
                expected.extend([Arrow::Left, Arrow::Right].map(|arrow| TreeKey::Arrow {
                    arrow,
                    shift: false,
                }));
                assert_eq!(keys, expected);
            }
            other => panic!("expected group for 't' -> 'm', got {other:?}"),
        }
        for (key, cmd) in [('h', "TabMoveBy -1"), ('l', "TabMoveBy 1")] {
            match tree
                .lookup(&[TreeKey::Char('t'), TreeKey::Char('m'), TreeKey::Char(key)])
                .unwrap()
            {
                KeyNode::Leaf { action, .. } => {
                    assert_eq!(action, &vec![cmd.to_string(), "EnterNormal".to_string()]);
                }
                other => panic!("expected leaf for 't m {key}', got {other:?}"),
            }
        }
    }

    // -- Unbind via empty string test -----------------------------------------

    #[test]
    fn merge_unbinds_with_empty_action() {
        let mut base = KeybindingTree::default();
        assert!(base.root.contains_key(&TreeKey::Char('v')));
        let overrides = KeybindingTree {
            root: HashMap::from([(
                TreeKey::Char('v'),
                KeyNode::Leaf {
                    label: String::new(),
                    action: vec![String::new()],
                },
            )]),
        };
        base.merge(&overrides);
        assert!(
            !base.root.contains_key(&TreeKey::Char('v')),
            "'v' key should have been removed by empty-action override"
        );
    }

    #[test]
    fn merge_unbinds_within_group() {
        let mut base = KeybindingTree::default();
        // Verify 'n' exists in Tab group.
        assert!(base.lookup(&tree_path("tn")).is_some());

        let overrides = KeybindingTree {
            root: HashMap::from([(
                TreeKey::Char('t'),
                KeyNode::Group {
                    label: "Tab".into(),
                    children: HashMap::from([(
                        TreeKey::Char('n'),
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
        assert!(base.lookup(&tree_path("tn")).is_none());
        // Other Tab children should still exist.
        assert!(base.lookup(&tree_path("tx")).is_some());
    }

    #[test]
    fn merge_unbinds_key_from_toml_override() {
        // The default Pane group binds `p s` (split horizontal) and `p v`
        // (split vertical) to leaves.
        let mut base = KeybindingTree::default();
        assert!(matches!(
            base.lookup(&tree_path("ps")),
            Some(KeyNode::Leaf { .. })
        ));
        assert!(matches!(
            base.lookup(&tree_path("pv")),
            Some(KeyNode::Leaf { .. })
        ));

        // A user override that maps `p s` to an empty string unbinds it. The
        // TOML here is the `[keybindings.command]` sub-table that `from_toml`
        // parses: top-level keys are root chars, so `p` is the Pane group and
        // `s = ""` is an empty-action leaf inside it.
        let toml_str = r#"
            [p]
            s = ""
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let overrides = KeybindingTree::from_toml(&value).unwrap();
        base.merge(&overrides);

        // `p s` is gone; the sibling `p v` (and other Pane children) survive.
        assert!(
            base.lookup(&tree_path("ps")).is_none(),
            "'p s' should have been unbound by the empty-string override"
        );
        assert!(
            base.lookup(&tree_path("pv")).is_some(),
            "sibling 'p v' must remain bound after unbinding 'p s'"
        );
        assert!(base.lookup(&tree_path("pn")).is_some());
    }

    #[test]
    fn merge_nonempty_toml_override_replaces_not_removes() {
        // A non-empty override replaces the leaf rather than removing it.
        let mut base = KeybindingTree::default();
        assert!(base.lookup(&tree_path("ps")).is_some());

        let toml_str = r#"
            [p]
            s = "PaneNew; EnterNormal"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let overrides = KeybindingTree::from_toml(&value).unwrap();
        base.merge(&overrides);

        // Still resolves, now to the replacement action chain.
        match base.lookup(&tree_path("ps")) {
            Some(KeyNode::Leaf { action, .. }) => {
                assert_eq!(
                    action,
                    &vec!["PaneNew".to_string(), "EnterNormal".to_string()]
                );
            }
            other => panic!("expected replaced leaf for 'p s', got {other:?}"),
        }
    }

    // -- Merge tests ----------------------------------------------------------

    #[test]
    fn merge_adds_new_keys() {
        let mut base = KeybindingTree::default();
        let overrides = KeybindingTree {
            root: HashMap::from([(
                TreeKey::Char('x'),
                KeyNode::Leaf {
                    label: "custom".into(),
                    action: vec!["TabNew".into()],
                },
            )]),
        };
        base.merge(&overrides);
        assert!(base.root.contains_key(&TreeKey::Char('x')));
    }

    #[test]
    fn merge_replaces_leaf() {
        let mut base = KeybindingTree::default();
        let overrides = KeybindingTree {
            root: HashMap::from([(
                TreeKey::Char('v'),
                KeyNode::Leaf {
                    label: "custom".into(),
                    action: vec!["SessionDetach".into()],
                },
            )]),
        };
        base.merge(&overrides);
        match &base.root[&TreeKey::Char('v')] {
            KeyNode::Leaf { action, .. } => assert_eq!(action, &vec!["SessionDetach".to_string()]),
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    #[test]
    fn merge_extends_group() {
        let mut base = KeybindingTree::default();
        let overrides = KeybindingTree {
            root: HashMap::from([(
                TreeKey::Char('t'),
                KeyNode::Group {
                    label: "Tab".into(),
                    children: HashMap::from([(
                        TreeKey::Char('x'),
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
        assert!(base.lookup(&tree_path("tn")).is_some());
        // New 'x' should be added.
        assert!(base.lookup(&tree_path("tx")).is_some());
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
        let node = tree.lookup(&tree_path("tn")).unwrap();
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
        let node = tree.lookup(&tree_path("n")).unwrap();
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
        let node = tree.lookup(&tree_path("tsa")).unwrap();
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

    fn lookup_mods(
        bindings: &ShortcutBindings,
        c: char,
        mods: KeyModifiers,
    ) -> Option<Vec<String>> {
        let key = KeyEvent::new_with_kind_and_state(
            KeyCode::Char(c),
            mods,
            KeyEventKind::Press,
            KeyEventState::NONE,
        );
        match bindings.lookup(&key)? {
            InterceptAction::Command(cmds) => Some(cmds.clone()),
            InterceptAction::GroupPrefix(_) => None,
        }
    }

    /// crossterm reports `ESC L` as `'L'` + `ALT | SHIFT`, the CSI-u
    /// `CSI 108;4u` as `'l'` + `ALT | SHIFT`, and `CSI 76;4u` as `'L'` +
    /// `ALT | SHIFT`. All of them are Alt+Shift+<letter>.
    #[test]
    fn alt_shift_hjkl_reach_pane_move_in_every_encoding() {
        let bindings = ShortcutBindings::default();
        let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
        for (lower, upper, command) in [
            ('h', 'H', "PaneMoveLeft"),
            ('j', 'J', "PaneMoveDown"),
            ('k', 'K', "PaneMoveUp"),
            ('l', 'L', "PaneMoveRight"),
        ] {
            let want = Some(vec![command.to_string()]);
            assert_eq!(lookup_mods(&bindings, upper, alt_shift), want, "{upper}");
            assert_eq!(lookup_mods(&bindings, lower, alt_shift), want, "{lower}");
            assert_eq!(
                lookup_mods(&bindings, upper, KeyModifiers::ALT),
                want,
                "{upper}"
            );
        }
        assert_eq!(
            lookup_mods(&bindings, 'l', KeyModifiers::ALT),
            Some(vec!["PaneFocusRight".to_string()])
        );
    }

    #[test]
    fn prefix_p_l_moves_the_pane_right() {
        match KeybindingTree::default().lookup(&tree_path("pL")) {
            Some(KeyNode::Leaf { action, .. }) => {
                assert!(action.contains(&"PaneMoveRight".to_string()))
            }
            other => panic!("expected the p L leaf, got {other:?}"),
        }
    }

    #[test]
    fn alt_g_opens_the_layout_switcher() {
        assert_eq!(
            lookup_mods(&ShortcutBindings::default(), 'g', KeyModifiers::ALT),
            Some(vec!["LayoutQuickSwitch".to_string()])
        );
    }

    /// The shifted character of a digit depends on the keyboard layout, so
    /// Alt+Shift+1 must not be read as Alt-1.
    #[test]
    fn shift_on_a_non_letter_is_kept() {
        let bindings = ShortcutBindings::default();
        assert_eq!(
            lookup_mods(&bindings, '1', KeyModifiers::ALT | KeyModifiers::SHIFT),
            None
        );
    }

    #[test]
    fn shift_on_an_uncased_letter_is_kept() {
        let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
        for c in ['あ', 'ب'] {
            assert_eq!(
                NormalizedKeyEvent::new(KeyCode::Char(c), alt_shift),
                NormalizedKeyEvent {
                    code: KeyCode::Char(c),
                    modifiers: alt_shift,
                },
                "{c}"
            );
        }
        assert_eq!(
            NormalizedKeyEvent::new(KeyCode::Char('g'), alt_shift),
            NormalizedKeyEvent {
                code: KeyCode::Char('G'),
                modifiers: KeyModifiers::ALT,
            }
        );
    }

    #[test]
    fn a_letter_with_a_multi_char_uppercase_is_not_read_as_its_first_char() {
        let bindings = ShortcutBindings::default();
        assert_eq!(
            lookup_mods(&bindings, 'ß', KeyModifiers::ALT | KeyModifiers::SHIFT),
            None
        );
        assert_eq!(
            NormalizedKeyEvent::new(KeyCode::Char('ß'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            NormalizedKeyEvent {
                code: KeyCode::Char('ß'),
                modifiers: KeyModifiers::ALT | KeyModifiers::SHIFT,
            }
        );
    }

    #[test]
    fn prefix_l_opens_the_layout_switcher() {
        match KeybindingTree::default().lookup(&tree_path("L")) {
            Some(KeyNode::Leaf { action, .. }) => {
                assert_eq!(action, &vec!["LayoutQuickSwitch".to_string()])
            }
            other => panic!("expected the L leaf, got {other:?}"),
        }
    }

    #[test]
    fn layout_set_takes_the_layout_name() {
        assert_eq!(
            parse_command("LayoutSet grid"),
            Some(RemuxCommand::LayoutSet("grid".to_string()))
        );
        assert_eq!(parse_command("LayoutSet"), None);
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
            InterceptAction::GroupPrefix(path) => assert_eq!(path, &tree_path("s")),
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
            .insert(alt_key('z'), InterceptAction::GroupPrefix(tree_path("q")));
        let tree = KeybindingTree::default();
        assert!(!bindings.validate_group_refs(&tree));
    }

    // -- entries / humanize / format_key_notation tests -----------------------

    #[test]
    fn shortcut_entries_include_expected_pairs() {
        let entries = ShortcutBindings::default().entries();
        assert!(
            entries.contains(&("Alt-h/\u{2190}".to_string(), "focus left".to_string())),
            "expected Alt-h/\u{2190} -> focus left, got: {entries:?}"
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
            .insert(alt_key('p'), InterceptAction::GroupPrefix(tree_path("p")));
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
        assert_eq!(humanize_command("PaneToggleZoomFloat"), "toggle zoom float");
        assert_eq!(
            humanize_command("PaneToggleZoomFullscreen"),
            "toggle zoom fullscreen"
        );
        assert_eq!(humanize_command("TabNext"), "next tab");
        assert_eq!(humanize_command("TabNew"), "new tab");
        assert_eq!(humanize_command("TabGoto 0"), "tab 1");
        assert_eq!(humanize_command("TabGoto 8"), "tab 9");
        assert_eq!(humanize_command("SessionQuickSwitch"), "switch session");
        assert_eq!(humanize_command("PaneMoveToTab"), "move to tab");
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
        // All 20 default chords are present.
        assert_eq!(b.len(), 20);
        assert_eq!(b.chord('t', 'n'), Some(SessionManagerBinding::TabNew));
        assert_eq!(b.chord('t', 'r'), Some(SessionManagerBinding::TabRename));
        assert_eq!(
            b.chord('t', 'm'),
            Some(SessionManagerBinding::TabMoveToSession)
        );
        assert_eq!(b.chord('s', 'x'), Some(SessionManagerBinding::SessionClose));
        assert_eq!(b.chord('f', 'r'), Some(SessionManagerBinding::FolderRename));
        assert_eq!(b.chord('v', 'a'), Some(SessionManagerBinding::AddToView));
        assert_eq!(b.chord('v', 'r'), Some(SessionManagerBinding::ViewRename));
        assert_eq!(
            b.chord('v', 'x'),
            Some(SessionManagerBinding::ViewRemoveCell)
        );
        assert_eq!(b.chord('v', 'd'), Some(SessionManagerBinding::ViewDelete));
        // ... and each is nameable from a config file.
        for (name, want) in [
            ("ViewRename", SessionManagerBinding::ViewRename),
            ("ViewRemoveCell", SessionManagerBinding::ViewRemoveCell),
            ("ViewDelete", SessionManagerBinding::ViewDelete),
        ] {
            assert_eq!(SessionManagerBinding::from_name(name), Some(want));
        }
        // First chars of the 2-char chords are prefixes.
        assert!(b.is_prefix('t'));
        assert!(b.is_prefix('p'));
        assert!(b.is_prefix('s'));
        assert!(b.is_prefix('f'));
        assert!(b.is_prefix('v'));
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
        assert_eq!(b.len(), 20);
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
        assert_eq!(b.len(), 20);
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

    // -- Action registry: the anti-drift tests ---------------------------------

    /// Every entry in the registry resolves, and resolves to the KIND the
    /// registry declares. This is the check that stops `action_specs()` (which
    /// the command palette lists) from drifting away from what the resolver
    /// actually accepts: adding a name to one without teaching the other
    /// fails here.
    #[test]
    fn every_registry_entry_resolves() {
        for spec in action_specs() {
            let input = match spec.sample_args {
                Some(args) => format!("{} {}", spec.name, args),
                None => spec.name.to_string(),
            };
            match (resolve_action(&input), spec.client) {
                (Some(Action::Client(got)), Some(want)) => assert_eq!(
                    got, want,
                    "registry entry '{}' resolved to the wrong client action",
                    spec.name
                ),
                (Some(Action::Server(_)), None) => {}
                (got, _) => panic!(
                    "registry entry '{}' resolved to {:?}, which does not match \
                     its declared kind (client={:?})",
                    spec.name, got, spec.client
                ),
            }
        }
    }

    /// An entry that advertises an argument hint must carry a sample argument,
    /// otherwise `every_registry_entry_resolves` would silently skip it.
    #[test]
    fn registry_entries_with_hints_carry_samples() {
        for spec in action_specs() {
            assert_eq!(
                spec.hint.is_some(),
                spec.sample_args.is_some(),
                "registry entry '{}' must declare a hint and a sample together",
                spec.name
            );
        }
    }

    /// The resolver accepts NOTHING that is not in the registry -- it looks the
    /// name up there first. Structural commands the session manager issues
    /// internally, and typos, are both rejected.
    #[test]
    fn resolver_rejects_names_outside_the_registry() {
        for name in [
            "NonexistentCommand",
            "PaneFocusRigth",
            "SessionRenameByName",
            "PaneCloseById",
            "TabMoveByIndex",
            "PaneMoveToTabTarget",
        ] {
            assert_eq!(resolve_action(name), None, "'{name}' must not resolve");
            assert_eq!(parse_command(name), None, "'{name}' must not parse");
        }
    }

    /// The palette listing is the registry, filtered -- not a second list.
    #[test]
    fn command_names_is_the_registry_filtered() {
        let expected: Vec<(&str, Option<&str>)> = action_specs()
            .iter()
            .filter(|s| s.palette)
            .map(|s| (s.name, s.hint))
            .collect();
        assert_eq!(command_names(), expected);
        // ... and everything it lists resolves.
        for (name, hint) in command_names() {
            let input = match hint {
                Some(_) => {
                    let sample = action_spec(name).and_then(|s| s.sample_args).unwrap();
                    format!("{name} {sample}")
                }
                None => name.to_string(),
            };
            assert!(
                resolve_action(&input).is_some(),
                "palette lists '{name}' but it does not resolve"
            );
        }
    }

    /// `EnterSearchMode` was parseable but missing from the palette listing.
    #[test]
    fn enter_search_mode_is_bindable_and_listed() {
        assert_eq!(
            parse_command("EnterSearchMode"),
            Some(RemuxCommand::EnterSearchMode)
        );
        assert!(command_names().iter().any(|(n, _)| *n == "EnterSearchMode"));
    }

    /// The nine client-only actions are reachable from the palette, and the
    /// resolver reports them as client actions rather than commands.
    #[test]
    fn client_actions_are_listed_and_client_kinded() {
        for (name, want) in [
            ("CommandPaletteOpen", ClientAction::CommandPaletteOpen),
            ("SessionQuickSwitch", ClientAction::SessionQuickSwitch),
            ("AgentQuickSwitch", ClientAction::AgentQuickSwitch),
            ("LayoutQuickSwitch", ClientAction::LayoutQuickSwitch),
            ("PaneMoveToTab", ClientAction::PaneMoveToTab),
            ("ViewNew", ClientAction::ViewNew),
            ("ViewAddPane", ClientAction::ViewAddPane),
            ("ViewRename", ClientAction::ViewRename),
            ("ViewRemovePane", ClientAction::ViewRemovePane),
            ("ViewLayoutNext", ClientAction::ViewLayoutNext),
            ("ViewClose", ClientAction::ViewClose),
            ("ViewDelete", ClientAction::ViewDelete),
            ("Redraw", ClientAction::Redraw),
        ] {
            assert_eq!(resolve_action(name), Some(Action::Client(want)));
            // A client action is not a server command.
            assert_eq!(parse_command(name), None);
            assert!(
                command_names().iter().any(|(n, _)| *n == name),
                "client action '{name}' is missing from the palette listing"
            );
        }
    }

    /// `SendKey` is the one registry entry deliberately kept out of the
    /// palette: its argument is a key notation, not something to pick.
    #[test]
    fn send_key_is_bindable_but_hidden_from_the_palette() {
        assert_eq!(
            parse_command("SendKey Ctrl-a"),
            Some(RemuxCommand::SendKey(vec![0x01]))
        );
        assert!(!command_names().iter().any(|(n, _)| *n == "SendKey"));
    }

    // -- Binding validation ----------------------------------------------------

    /// EVERY action string in the shipped keybindings resolves -- the default
    /// which-key tree and the default Alt shortcuts. A default binding naming
    /// an action the resolver does not know would be silently inert.
    #[test]
    fn every_default_binding_resolves() {
        let problems = unresolved_actions(&KeybindingTree::default(), &ShortcutBindings::default());
        assert!(
            problems.is_empty(),
            "default bindings do not resolve: {problems:#?}"
        );

        // Belt and braces: the walk actually visited the tree.
        let leaves = KeybindingTree::default().leaves();
        assert!(
            leaves.len() > 40,
            "expected the default tree to have leaves"
        );
        assert!(
            leaves
                .iter()
                .any(|(path, actions)| path == &tree_path("wn")
                    && actions == &["ViewNew".to_string()])
        );
    }

    /// A typo in a binding is reported, naming the key path AND the bad name.
    #[test]
    fn unresolved_actions_names_the_binding_and_the_typo() {
        let mut tree = KeybindingTree::default();
        let toml_str = r#"
            [w]
            n = "ViewNwe"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        tree.merge(&KeybindingTree::from_toml(&value).unwrap());

        let mut shortcuts = ShortcutBindings::default();
        let shortcut_toml: toml::Value = r#"
            "Alt-y" = "PaneFocusRigth"
        "#
        .parse()
        .unwrap();
        shortcuts.merge(&ShortcutBindings::from_toml(&shortcut_toml).unwrap());

        let problems = unresolved_actions(&tree, &shortcuts);
        assert_eq!(problems.len(), 2, "{problems:#?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("binding 'w n'") && p.contains("unknown action 'ViewNwe'")),
            "{problems:#?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("shortcut 'Alt-y'")
                    && p.contains("unknown action 'PaneFocusRigth'")),
            "{problems:#?}"
        );
    }

    /// An action chain is validated element by element, and the unbind
    /// sentinel (an empty action) is not an error.
    #[test]
    fn unresolved_actions_checks_chains_and_ignores_unbinds() {
        let toml_str = r#"
            q = "TabNew; EnterNorml"
            h = ""
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let tree = KeybindingTree::from_toml(&value).unwrap();
        let problems = unresolved_actions(&tree, &ShortcutBindings::default());
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("unknown action 'EnterNorml'"),
            "{problems:#?}"
        );
    }

    /// A known name with a missing required argument is reported differently
    /// from an unknown name -- the user typed a real command, not a typo.
    #[test]
    fn unresolved_actions_flags_missing_arguments() {
        let toml_str = r#"
            q = "TabGoto"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let tree = KeybindingTree::from_toml(&value).unwrap();
        let problems = unresolved_actions(&tree, &ShortcutBindings::default());
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("'TabGoto' is missing a required argument"),
            "{problems:#?}"
        );
    }

    /// Which-key labels come from the registry, so there is no fifth list.
    #[test]
    fn humanize_uses_registry_labels() {
        assert_eq!(humanize_command("SessionQuickSwitch"), "switch session");
        assert_eq!(humanize_command("AgentQuickSwitch"), "switch agent");
        assert_eq!(humanize_command("PaneMoveToTab"), "move to tab");
        assert_eq!(humanize_command("ViewRemovePane"), "remove cell");
        // Names with no label override still fall back to the PascalCase split,
        // including strings that are not in the registry at all.
        assert_eq!(humanize_command("PaneFocusLeft"), "focus left");
        assert_eq!(humanize_command("SomethingUnknown"), "something unknown");
    }

    // -- Arrow keys --------------------------------------------------------------

    fn arrow(arrow: Arrow, shift: bool) -> TreeKey {
        TreeKey::Arrow { arrow, shift }
    }

    fn leaf_actions(tree: &KeybindingTree, path: &[TreeKey]) -> Vec<String> {
        match tree.lookup(path) {
            Some(KeyNode::Leaf { action, .. }) => action.clone(),
            other => panic!("expected a leaf at {path:?}, got {other:?}"),
        }
    }

    #[test]
    fn tree_key_parses_characters_and_arrow_names() {
        assert_eq!(TreeKey::parse("h"), Some(TreeKey::Char('h')));
        assert_eq!(TreeKey::parse("\u{e9}"), Some(TreeKey::Char('\u{e9}')));
        assert_eq!(TreeKey::parse("Left"), Some(arrow(Arrow::Left, false)));
        assert_eq!(TreeKey::parse("Down"), Some(arrow(Arrow::Down, false)));
        assert_eq!(TreeKey::parse("Up"), Some(arrow(Arrow::Up, false)));
        assert_eq!(TreeKey::parse("Right"), Some(arrow(Arrow::Right, false)));
        assert_eq!(TreeKey::parse("Shift-Up"), Some(arrow(Arrow::Up, true)));
        for not_a_key in ["leader", "left", "Shift-h", "Alt-Left", "Enter", ""] {
            assert_eq!(TreeKey::parse(not_a_key), None, "{not_a_key:?}");
        }
        for key in ["h", "Left", "Shift-Right"] {
            assert_eq!(TreeKey::parse(key).unwrap().to_string(), key);
        }
    }

    #[test]
    fn tree_key_from_a_key_event_keeps_shift_only_on_arrows() {
        let ev = |code, modifiers| KeyEvent::new(code, modifiers);
        assert_eq!(
            TreeKey::from_key_event(&ev(KeyCode::Left, KeyModifiers::NONE)),
            Some(arrow(Arrow::Left, false))
        );
        assert_eq!(
            TreeKey::from_key_event(&ev(KeyCode::Left, KeyModifiers::SHIFT)),
            Some(arrow(Arrow::Left, true))
        );
        assert_eq!(
            TreeKey::from_key_event(&ev(KeyCode::Char('H'), KeyModifiers::SHIFT)),
            Some(TreeKey::Char('H'))
        );
        assert_eq!(
            TreeKey::from_key_event(&ev(KeyCode::Left, KeyModifiers::ALT)),
            None
        );
        assert_eq!(
            TreeKey::from_key_event(&ev(KeyCode::Tab, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn every_default_direction_leaf_has_an_arrow_twin() {
        let tree = KeybindingTree::default();
        let pairs = [
            ('h', Arrow::Left),
            ('j', Arrow::Down),
            ('k', Arrow::Up),
            ('l', Arrow::Right),
        ];
        for group in ["p", "pS", "pU", "pR"] {
            for (c, dir) in pairs {
                let mut by_char = tree_path(group);
                by_char.push(TreeKey::Char(c));
                let mut by_arrow = tree_path(group);
                by_arrow.push(arrow(dir, false));
                assert_eq!(
                    leaf_actions(&tree, &by_arrow),
                    leaf_actions(&tree, &by_char),
                    "{group} {c}"
                );
            }
        }
        for (c, dir) in pairs {
            let mut by_char = tree_path("p");
            by_char.push(TreeKey::Char(c.to_ascii_uppercase()));
            let mut by_arrow = tree_path("p");
            by_arrow.push(arrow(dir, true));
            assert_eq!(
                leaf_actions(&tree, &by_arrow),
                leaf_actions(&tree, &by_char)
            );
        }
        assert_eq!(
            leaf_actions(
                &tree,
                &tree_path("pR")
                    .into_iter()
                    .chain([arrow(Arrow::Left, false)])
                    .collect::<Vec<_>>()
            ),
            vec!["ResizeLeft 5".to_string()]
        );
        // The Sidebar group's h/j/l name edges to toggle, not a direction to
        // move in, so it gets no arrows.
        assert!(tree
            .lookup(&[TreeKey::Char('b'), arrow(Arrow::Left, false)])
            .is_none());
    }

    #[test]
    fn which_key_folds_an_arrow_into_its_character_twin() {
        let tree = KeybindingTree::default();
        let resize = tree.children_at(&tree_path("pR")).unwrap();
        let keys: Vec<&str> = resize.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["h/\u{2190}", "j/\u{2193}", "k/\u{2191}", "l/\u{2192}"]
        );

        let pane = tree.children_at(&tree_path("p")).unwrap();
        let keys: Vec<&str> = pane.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"H/\u{21e7}\u{2190}"), "{keys:?}");
        assert!(keys.contains(&"h/\u{2190}"), "{keys:?}");
        assert!(!keys.iter().any(|k| k.starts_with('\u{2190}')), "{keys:?}");
        let Some(KeyNode::Group { children, .. }) = tree.lookup(&tree_path("p")) else {
            panic!("p is a group");
        };
        let letters = children
            .keys()
            .filter(|k| matches!(k, TreeKey::Char(_)))
            .count();
        assert_eq!(pane.len(), letters, "one row per letter: {keys:?}");
    }

    #[test]
    fn a_user_rebound_h_leaves_the_arrow_on_its_own_row() {
        let toml_str = r#"
            [p]
            h = "PaneClose"
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let mut tree = KeybindingTree::default();
        tree.merge(&KeybindingTree::from_toml(&value).unwrap());

        let pane = tree.children_at(&tree_path("p")).unwrap();
        let find = |key: &str| pane.iter().find(|(k, _)| k == key).map(|(_, l)| l.clone());
        assert_eq!(find("h").as_deref(), Some("PaneClose"));
        assert_eq!(find("\u{2190}").as_deref(), Some("focus left"));
        assert_eq!(
            leaf_actions(&tree, &[TreeKey::Char('p'), arrow(Arrow::Left, false)]),
            vec!["PaneFocusLeft".to_string(), "EnterNormal".to_string()]
        );
    }

    #[test]
    fn an_arrow_folds_into_its_own_letter_before_an_earlier_twin() {
        let toml_str = r#"
            [q]
            a = "PaneFocusLeft"
            h = "PaneFocusLeft"
            Left = "PaneFocusLeft"
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let tree = KeybindingTree::from_toml(&value).unwrap();
        let keys: Vec<String> = tree
            .children_at(&tree_path("q"))
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, ["a", "h/\u{2190}"]);
    }

    #[test]
    fn toml_accepts_arrow_keys_in_groups_and_can_unbind_them() {
        let toml_str = r#"
            leader = "Ctrl-b"
            Left = "TabPrev"
            Shift-Left = "PaneMoveLeft"
            [p]
            Up = "PaneClose"
            "Shift-Down" = "PaneNew"
            [p.R]
            Right = ""
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let user = KeybindingTree::from_toml(&value).unwrap();
        assert_eq!(
            leaf_actions(&user, &[arrow(Arrow::Left, false)]),
            vec!["TabPrev".to_string()]
        );
        assert!(
            user.lookup(&[arrow(Arrow::Left, true)]).is_none(),
            "a root Shift+arrow is a Normal-mode shortcut, not a tree key"
        );
        assert!(!user.root.keys().any(|k| k.to_string() == "leader"));

        let mut tree = KeybindingTree::default();
        tree.merge(&user);
        assert_eq!(
            leaf_actions(&tree, &[TreeKey::Char('p'), arrow(Arrow::Up, false)]),
            vec!["PaneClose".to_string()]
        );
        assert_eq!(
            leaf_actions(&tree, &[TreeKey::Char('p'), arrow(Arrow::Down, true)]),
            vec!["PaneNew".to_string()]
        );
        let mut resize_right = tree_path("pR");
        resize_right.push(arrow(Arrow::Right, false));
        assert!(tree.lookup(&resize_right).is_none());
        assert!(tree.lookup(&tree_path("pRl")).is_some());
        // Merging a group the user re-declared keeps it sticky.
        assert!(matches!(
            tree.lookup(&tree_path("pR")),
            Some(KeyNode::Group { sticky: true, .. })
        ));
    }

    #[test]
    fn a_binding_hint_names_the_letter_not_the_arrow() {
        let tree = KeybindingTree::default();
        assert_eq!(tree.path_for("PaneFocusLeft"), Some(tree_path("ph")));
        let shortcuts = ShortcutBindings::default();
        assert_eq!(
            shortcuts.notation_for("PaneFocusLeft").as_deref(),
            Some("Alt-h")
        );
        assert_eq!(
            shortcuts.notation_for("PaneMoveRight").as_deref(),
            Some("Alt-L")
        );
    }

    #[test]
    fn default_shortcuts_bind_alt_arrows_to_focus_and_alt_shift_arrows_to_move() {
        let shortcuts = ShortcutBindings::default();
        let run = |code, modifiers| match shortcuts.lookup(&KeyEvent::new(code, modifiers)) {
            Some(InterceptAction::Command(cmds)) => cmds.clone(),
            other => panic!("{code:?} {modifiers:?}: {other:?}"),
        };
        let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
        for (code, focus, mv) in [
            (KeyCode::Left, "PaneFocusLeft", "PaneMoveLeft"),
            (KeyCode::Down, "PaneFocusDown", "PaneMoveDown"),
            (KeyCode::Up, "PaneFocusUp", "PaneMoveUp"),
            (KeyCode::Right, "PaneFocusRight", "PaneMoveRight"),
        ] {
            assert_eq!(run(code, KeyModifiers::ALT), vec![focus.to_string()]);
            assert_eq!(run(code, alt_shift), vec![mv.to_string()]);
        }
        assert!(shortcuts
            .lookup(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
            .is_none());
    }

    #[test]
    fn the_alt_section_folds_arrows_into_their_letters() {
        let entries = ShortcutBindings::default().entries();
        let get = |n: &str| entries.iter().find(|(k, _)| k == n).map(|(_, l)| l.clone());
        assert_eq!(get("Alt-h/\u{2190}").as_deref(), Some("focus left"));
        assert_eq!(get("Alt-L/\u{21e7}\u{2192}").as_deref(), Some("move right"));
        assert!(
            !entries
                .iter()
                .any(|(k, _)| k.contains("Left") || k.contains("Right")),
            "{entries:?}"
        );
        let arrows = ShortcutBindings::default()
            .bindings
            .keys()
            .filter(|k| Arrow::from_code(k.code).is_some())
            .count();
        assert_eq!(arrows, 8);
        assert_eq!(
            entries.len(),
            ShortcutBindings::default().bindings.len() - arrows
        );
    }

    #[test]
    fn stacked_modifiers_parse_and_round_trip() {
        let ev = parse_key_notation("Alt-Shift-Left").unwrap();
        assert_eq!(ev.code, KeyCode::Left);
        assert_eq!(ev.modifiers, KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert_eq!(
            format_key_notation(&NormalizedKeyEvent::from(ev)),
            "Alt-Shift-Left"
        );
        let ev = parse_key_notation("Ctrl-Alt-x").unwrap();
        assert_eq!(ev.modifiers, KeyModifiers::CONTROL | KeyModifiers::ALT);
        let ev = parse_key_notation("Shift-Tab").unwrap();
        assert_eq!(ev.code, KeyCode::BackTab);
        assert_eq!(
            parse_key_notation("Alt--").unwrap().code,
            KeyCode::Char('-')
        );
        assert!(parse_key_notation("Alt-").is_none());
    }

    #[test]
    fn a_user_can_unbind_the_alt_shift_arrow_defaults() {
        let value: toml::Value = toml::from_str(r#""Alt-Shift-Left" = """#).unwrap();
        let mut shortcuts = ShortcutBindings::default();
        shortcuts.merge(&ShortcutBindings::from_toml(&value).unwrap());
        assert!(shortcuts
            .lookup(&KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::ALT | KeyModifiers::SHIFT
            ))
            .is_none());
        assert!(shortcuts
            .lookup(&KeyEvent::new(KeyCode::Left, KeyModifiers::ALT))
            .is_some());
    }
}
