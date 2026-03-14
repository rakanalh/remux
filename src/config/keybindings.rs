use std::collections::HashMap;

use crate::protocol::RemuxCommand;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// A node in the keybinding tree. Either a group of sub-keys or a leaf that
/// maps to an action string (e.g. `"tab:new"`).
#[derive(Debug, Clone)]
pub enum KeyNode {
    /// An intermediate group that contains sub-keys.
    Group {
        label: String,
        children: HashMap<char, KeyNode>,
    },
    /// A terminal binding that maps to an action string.
    Leaf {
        label: String,
        /// Action descriptor, e.g. `"tab:new"`, `"resize:left 5"`.
        action: String,
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

/// Helper to build a leaf node.
fn leaf(label: &str, action: &str) -> KeyNode {
    KeyNode::Leaf {
        label: label.to_string(),
        action: action.to_string(),
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
                ('n', leaf("new", "tab:new")),
                ('c', leaf("close", "tab:close")),
                ('m', leaf("move", "tab:move")),
                ('r', leaf("rename", "tab:rename")),
                ('l', leaf("list", "tab:list")),
            ],
        ),
    );

    // p: Pane
    root.insert(
        'p',
        group(
            "Pane",
            vec![
                ('n', leaf("new", "pane:new")),
                ('c', leaf("close", "pane:close")),
                ('s', leaf("split vertical", "pane:split_vertical")),
                ('S', leaf("split horizontal", "pane:split_horizontal")),
                ('h', leaf("focus left", "pane:focus_left")),
                ('j', leaf("focus down", "pane:focus_down")),
                ('k', leaf("focus up", "pane:focus_up")),
                ('l', leaf("focus right", "pane:focus_right")),
                ('a', leaf("stack add", "pane:stack_add")),
                (']', leaf("stack next", "pane:stack_next")),
                ('[', leaf("stack prev", "pane:stack_prev")),
                ('r', leaf("rename", "pane:rename")),
            ],
        ),
    );

    // s: Session
    root.insert(
        's',
        group(
            "Session",
            vec![
                ('n', leaf("new", "session:new")),
                ('d', leaf("detach", "session:detach")),
                ('r', leaf("rename", "session:rename")),
                ('l', leaf("list", "session:list")),
            ],
        ),
    );

    // f: Folder
    root.insert(
        'f',
        group(
            "Folder",
            vec![
                ('n', leaf("new", "folder:new")),
                ('d', leaf("delete", "folder:delete")),
                ('l', leaf("list", "folder:list")),
                ('m', leaf("move session", "folder:move_session")),
            ],
        ),
    );

    // b: Buffer
    root.insert(
        'b',
        group(
            "Buffer",
            vec![
                ('e', leaf("edit in editor", "buffer:edit_in_editor")),
                ('/', leaf("search", "buffer:search")),
            ],
        ),
    );

    // r: Resize
    root.insert(
        'r',
        group(
            "Resize",
            vec![
                ('h', leaf("left", "resize:left 5")),
                ('j', leaf("down", "resize:down 5")),
                ('k', leaf("up", "resize:up 5")),
                ('l', leaf("right", "resize:right 5")),
            ],
        ),
    );

    // Direct mode-switch bindings at the root level.
    root.insert('i', leaf("insert mode", "enter_insert_mode"));
    root.insert('v', leaf("visual mode", "enter_visual_mode"));

    // Layout toggle bindings.
    root.insert('g', leaf("toggle gaps", "toggle_gaps"));

    root
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

impl KeybindingTree {
    /// Look up the node at the given key path (e.g. `['t', 'n']` maps to the
    /// `tab:new` leaf).
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
                (key, label)
            })
            .collect();
        result.sort_by_key(|(k, _)| *k);
        Some(result)
    }

    /// Merge another tree on top of this one. Keys in `overrides` replace keys
    /// in `self`. Groups are merged recursively; leaves are replaced outright.
    pub fn merge(&mut self, overrides: &KeybindingTree) {
        merge_maps(&mut self.root, &overrides.root);
    }
}

fn merge_maps(base: &mut HashMap<char, KeyNode>, overrides: &HashMap<char, KeyNode>) {
    for (key, override_node) in overrides {
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
// TOML config parsing
// ---------------------------------------------------------------------------

impl KeybindingTree {
    /// Parse a keybinding tree from the `[modes.normal.keys]` section of the
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
            toml::Value::String(action) => KeyNode::Leaf {
                label: action.clone(),
                action: action.clone(),
            },
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
// Action string -> RemuxCommand parsing
// ---------------------------------------------------------------------------

/// Parse an action string (e.g. `"tab:new"`, `"resize:left 5"`) into a
/// [`RemuxCommand`].
///
/// Returns `None` if the action string is not recognised.
pub fn parse_action(action: &str) -> Option<RemuxCommand> {
    // Split on ':' to get category and detail.
    let (category, detail) = if let Some(idx) = action.find(':') {
        (&action[..idx], action[idx + 1..].trim())
    } else {
        // Handle bare commands without a colon.
        return match action.trim() {
            "enter_insert_mode" => Some(RemuxCommand::EnterInsertMode),
            "enter_normal_mode" => Some(RemuxCommand::EnterNormalMode),
            "enter_visual_mode" => Some(RemuxCommand::EnterVisualMode),
            "session_save" => Some(RemuxCommand::SessionSave),
            "toggle_gaps" => Some(RemuxCommand::ToggleGaps),
            _ => None,
        };
    };

    match category {
        "tab" => match detail {
            "new" => Some(RemuxCommand::TabNew),
            "close" => Some(RemuxCommand::TabClose),
            "next" => Some(RemuxCommand::TabNext),
            "prev" => Some(RemuxCommand::TabPrev),
            "list" => Some(RemuxCommand::TabNext), // list navigates tabs
            _ if detail.starts_with("rename") => {
                let name = detail.strip_prefix("rename")?.trim().to_string();
                Some(RemuxCommand::TabRename(name))
            }
            _ if detail.starts_with("goto ") => {
                let idx = detail.strip_prefix("goto ")?.trim().parse().ok()?;
                Some(RemuxCommand::TabGoto(idx))
            }
            _ if detail.starts_with("move ") => {
                let idx = detail.strip_prefix("move ")?.trim().parse().ok()?;
                Some(RemuxCommand::TabMove(idx))
            }
            // bare "move" without argument defaults to 0
            "move" => Some(RemuxCommand::TabMove(0)),
            _ => None,
        },
        "pane" => match detail {
            "new" => Some(RemuxCommand::PaneNew),
            "close" => Some(RemuxCommand::PaneClose),
            "split_vertical" => Some(RemuxCommand::PaneSplitVertical),
            "split_horizontal" => Some(RemuxCommand::PaneSplitHorizontal),
            "focus_left" => Some(RemuxCommand::PaneFocusLeft),
            "focus_right" => Some(RemuxCommand::PaneFocusRight),
            "focus_up" => Some(RemuxCommand::PaneFocusUp),
            "focus_down" => Some(RemuxCommand::PaneFocusDown),
            "stack_add" => Some(RemuxCommand::PaneStackAdd),
            "stack_next" => Some(RemuxCommand::PaneStackNext),
            "stack_prev" => Some(RemuxCommand::PaneStackPrev),
            "rename" => Some(RemuxCommand::PaneRename(String::new())),
            _ => None,
        },
        "session" => match detail {
            "new" => Some(RemuxCommand::SessionNew {
                name: String::new(),
                folder: None,
            }),
            "detach" => Some(RemuxCommand::SessionDetach),
            "list" => Some(RemuxCommand::SessionList),
            "save" => Some(RemuxCommand::SessionSave),
            _ if detail.starts_with("rename") => {
                let name = detail
                    .strip_prefix("rename")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                Some(RemuxCommand::SessionRename(name))
            }
            _ => None,
        },
        "folder" => match detail {
            "list" => Some(RemuxCommand::FolderList),
            _ if detail.starts_with("new") => {
                let name = detail.strip_prefix("new").unwrap_or("").trim().to_string();
                Some(RemuxCommand::FolderNew(name))
            }
            _ if detail.starts_with("delete") => {
                let name = detail
                    .strip_prefix("delete")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                Some(RemuxCommand::FolderDelete(name))
            }
            "move_session" => Some(RemuxCommand::FolderMoveSession {
                session: String::new(),
                folder: None,
            }),
            _ => None,
        },
        "buffer" => match detail {
            "edit_in_editor" => Some(RemuxCommand::BufferEditInEditor),
            "search" => Some(RemuxCommand::BufferSearch),
            _ => None,
        },
        "resize" => {
            // Format: "left", "left 5", etc.
            let parts: Vec<&str> = detail.splitn(2, ' ').collect();
            let direction = parts[0];
            let amount: u16 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            match direction {
                "left" => Some(RemuxCommand::ResizeLeft(amount)),
                "right" => Some(RemuxCommand::ResizeRight(amount)),
                "up" => Some(RemuxCommand::ResizeUp(amount)),
                "down" => Some(RemuxCommand::ResizeDown(amount)),
                _ => None,
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tree_has_expected_groups() {
        let tree = KeybindingTree::default();
        assert!(tree.root.contains_key(&'t'));
        assert!(tree.root.contains_key(&'p'));
        assert!(tree.root.contains_key(&'s'));
        assert!(tree.root.contains_key(&'f'));
        assert!(tree.root.contains_key(&'b'));
        assert!(tree.root.contains_key(&'r'));
        assert!(tree.root.contains_key(&'i'));
        assert!(tree.root.contains_key(&'v'));
        assert!(tree.root.contains_key(&'g'));
    }

    #[test]
    fn lookup_leaf() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&['t', 'n']).unwrap();
        match node {
            KeyNode::Leaf { action, .. } => assert_eq!(action, "tab:new"),
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
        assert!(children.iter().any(|(k, _)| *k == 'i'));
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
    fn parse_action_tab_new() {
        assert_eq!(parse_action("tab:new"), Some(RemuxCommand::TabNew));
    }

    #[test]
    fn parse_action_resize_with_amount() {
        assert_eq!(
            parse_action("resize:left 5"),
            Some(RemuxCommand::ResizeLeft(5))
        );
    }

    #[test]
    fn parse_action_resize_default_amount() {
        assert_eq!(parse_action("resize:up"), Some(RemuxCommand::ResizeUp(1)));
    }

    #[test]
    fn parse_action_bare_mode() {
        assert_eq!(
            parse_action("enter_insert_mode"),
            Some(RemuxCommand::EnterInsertMode)
        );
        assert_eq!(
            parse_action("enter_visual_mode"),
            Some(RemuxCommand::EnterVisualMode)
        );
    }

    #[test]
    fn parse_action_stack_add() {
        assert_eq!(
            parse_action("pane:stack_add"),
            Some(RemuxCommand::PaneStackAdd)
        );
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
    fn default_tree_has_toggle_gaps() {
        let tree = KeybindingTree::default();
        assert!(tree.root.contains_key(&'g'));
        let node = tree.lookup(&['g']).unwrap();
        match node {
            KeyNode::Leaf { action, label, .. } => {
                assert_eq!(action, "toggle_gaps");
                assert_eq!(label, "toggle gaps");
            }
            other => panic!("expected leaf for 'g', got {other:?}"),
        }
    }

    #[test]
    fn parse_action_toggle_gaps() {
        assert_eq!(parse_action("toggle_gaps"), Some(RemuxCommand::ToggleGaps));
    }

    #[test]
    fn pane_group_has_rename_leaf() {
        let tree = KeybindingTree::default();
        let node = tree.lookup(&['p', 'r']).unwrap();
        match node {
            KeyNode::Leaf { action, label, .. } => {
                assert_eq!(action, "pane:rename");
                assert_eq!(label, "rename");
            }
            other => panic!("expected leaf for 'p' -> 'r', got {other:?}"),
        }
    }

    #[test]
    fn parse_action_pane_rename() {
        let result = parse_action("pane:rename");
        assert_eq!(result, Some(RemuxCommand::PaneRename(String::new())));
    }

    #[test]
    fn parse_action_unknown() {
        assert_eq!(parse_action("nonexistent:thing"), None);
    }

    #[test]
    fn merge_adds_new_keys() {
        let mut base = KeybindingTree::default();
        let overrides = KeybindingTree {
            root: HashMap::from([(
                'x',
                KeyNode::Leaf {
                    label: "custom".into(),
                    action: "custom:action".into(),
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
                'i',
                KeyNode::Leaf {
                    label: "custom insert".into(),
                    action: "custom:insert".into(),
                },
            )]),
        };
        base.merge(&overrides);
        match &base.root[&'i'] {
            KeyNode::Leaf { action, .. } => assert_eq!(action, "custom:insert"),
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
                            action: "tab:extra".into(),
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

    #[test]
    fn from_toml_basic() {
        let toml_str = r#"
            [t]
            _label = "Tab"
            n = "tab:new"
            c = "tab:close"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let tree = KeybindingTree::from_toml(&value).unwrap();
        let node = tree.lookup(&['t', 'n']).unwrap();
        match node {
            KeyNode::Leaf { action, .. } => assert_eq!(action, "tab:new"),
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    #[test]
    fn from_toml_nested_groups() {
        let toml_str = r#"
            [t]
            _label = "Tab"
            n = "tab:new"
            [t.s]
            _label = "Sub"
            a = "tab:sub_a"
        "#;
        let value: toml::Value = toml_str.parse().unwrap();
        let tree = KeybindingTree::from_toml(&value).unwrap();
        let node = tree.lookup(&['t', 's', 'a']).unwrap();
        match node {
            KeyNode::Leaf { action, .. } => assert_eq!(action, "tab:sub_a"),
            other => panic!("expected leaf, got {other:?}"),
        }
    }
}
