//! Layout engine for Remux terminal multiplexer.
//!
//! This module implements a tree-based layout system where panes can be
//! arranged in splits (horizontal/vertical) and stacks (tabbed panes).
//! The layout engine is pure -- no I/O, no async -- just data structure
//! manipulation and geometric computation.

use serde::{Deserialize, Serialize};

/// Unique identifier for a pane.
pub type PaneId = u64;

/// Direction of a split: Horizontal divides top/bottom, Vertical divides left/right.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Horizontal,
    Vertical,
}

/// Direction for focus navigation (4-directional).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

/// A node in the layout tree.
///
/// The tree is a binary tree where internal nodes are `Split` and leaf nodes
/// are `Stack`. Each `Stack` holds one or more panes, with one being active
/// (visible).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LayoutNode {
    /// A split divides space between two child nodes.
    Split {
        direction: Direction,
        /// Ratio allocated to `first` child (0.0..=1.0).
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
    /// A stack holds one or more panes in a tabbed arrangement.
    Stack {
        panes: Vec<PaneId>,
        /// Display names for each pane, parallel to `panes`.
        #[serde(default)]
        names: Vec<String>,
        /// Custom names set by the user, parallel to `panes`.
        /// `Some(name)` means the user set a custom name; `None` means
        /// auto-detect from the running process.
        #[serde(default)]
        custom_names: Vec<Option<String>>,
        /// Index into `panes` for the currently visible pane.
        active: usize,
    },
}

/// An axis-aligned rectangle for pane geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Minimum pane dimension in either axis.
const MIN_PANE_SIZE: u16 = 2;

/// Minimum ratio for a split (prevents invisible panes).
const MIN_RATIO: f32 = 0.1;

/// Maximum ratio for a split.
const MAX_RATIO: f32 = 0.9;

impl LayoutNode {
    /// Create a new stack containing a single pane.
    pub fn new_stack(pane_id: PaneId) -> Self {
        LayoutNode::Stack {
            panes: vec![pane_id],
            names: vec![String::new()],
            custom_names: vec![None],
            active: 0,
        }
    }

    /// Split the stack containing `target_pane` vertically (left/right).
    ///
    /// The original stack becomes the left child, and a new stack with
    /// `new_pane` becomes the right child.
    ///
    /// Returns `true` if the split was performed.
    pub fn split_vertical(&mut self, target_pane: PaneId, new_pane: PaneId) -> bool {
        self.split_at(target_pane, new_pane, Direction::Vertical)
    }

    /// Split the stack containing `target_pane` horizontally (top/bottom).
    ///
    /// The original stack becomes the top child, and a new stack with
    /// `new_pane` becomes the bottom child.
    ///
    /// Returns `true` if the split was performed.
    pub fn split_horizontal(&mut self, target_pane: PaneId, new_pane: PaneId) -> bool {
        self.split_at(target_pane, new_pane, Direction::Horizontal)
    }

    /// Generic split helper. Finds the leaf containing `target_pane` and
    /// replaces it with a Split node.
    fn split_at(&mut self, target_pane: PaneId, new_pane: PaneId, direction: Direction) -> bool {
        match self {
            LayoutNode::Stack { panes, .. } => {
                if panes.contains(&target_pane) {
                    let original = std::mem::replace(self, LayoutNode::new_stack(new_pane));
                    let new_stack = std::mem::replace(self, LayoutNode::new_stack(0));
                    *self = LayoutNode::Split {
                        direction,
                        ratio: 0.5,
                        first: Box::new(original),
                        second: Box::new(new_stack),
                    };
                    true
                } else {
                    false
                }
            }
            LayoutNode::Split { first, second, .. } => {
                if first.split_at(target_pane, new_pane, direction.clone()) {
                    return true;
                }
                second.split_at(target_pane, new_pane, direction)
            }
        }
    }

    /// Adjust the ratio of the nearest ancestor Split of the pane in the
    /// given direction.
    ///
    /// `delta` is the change to apply (positive increases the first child's
    /// share). The ratio is clamped to \[0.1, 0.9\].
    ///
    /// Returns `true` if a matching split was found and adjusted.
    pub fn resize(&mut self, pane_id: PaneId, direction: Direction, delta: f32) -> bool {
        match self {
            LayoutNode::Stack { .. } => false,
            LayoutNode::Split {
                direction: split_dir,
                ratio,
                first,
                second,
            } => {
                let in_first = contains_pane(first, pane_id);
                let in_second = contains_pane(second, pane_id);

                if !in_first && !in_second {
                    return false;
                }

                // If this split matches the requested direction, try to
                // recurse into a deeper matching split first.
                if *split_dir == direction {
                    if in_first && first.resize(pane_id, direction.clone(), delta) {
                        return true;
                    }
                    if in_second && second.resize(pane_id, direction.clone(), delta) {
                        return true;
                    }

                    // No deeper match -- adjust this split's ratio.
                    let new_ratio = if in_first {
                        *ratio + delta
                    } else {
                        *ratio - delta
                    };
                    *ratio = new_ratio.clamp(MIN_RATIO, MAX_RATIO);
                    return true;
                }

                // Direction doesn't match -- recurse into the branch that
                // contains the pane.
                if in_first {
                    return first.resize(pane_id, direction, delta);
                }
                second.resize(pane_id, direction, delta)
            }
        }
    }

    /// Remove a pane from the layout.
    ///
    /// - If the pane's stack still has other panes, the next pane becomes active.
    /// - If the stack becomes empty, the parent Split is collapsed (replaced
    ///   by the sibling node).
    ///
    /// Returns the pane that should receive focus, or `None` if the layout
    /// is now empty.
    pub fn close_pane(&mut self, pane_id: PaneId) -> Option<PaneId> {
        match self {
            LayoutNode::Stack {
                panes,
                names,
                custom_names,
                active,
            } => {
                let pos = panes.iter().position(|&p| p == pane_id)?;
                panes.remove(pos);
                if pos < names.len() {
                    names.remove(pos);
                }
                if pos < custom_names.len() {
                    custom_names.remove(pos);
                }
                if panes.is_empty() {
                    return None;
                }
                if *active >= panes.len() {
                    *active = panes.len() - 1;
                } else if *active > pos {
                    *active -= 1;
                }
                Some(panes[*active])
            }
            LayoutNode::Split { first, second, .. } => {
                let in_first = contains_pane(first, pane_id);
                let in_second = contains_pane(second, pane_id);

                if in_first {
                    let result = first.close_pane(pane_id);
                    if result.is_some() {
                        return result;
                    }
                    // First child is now empty -- collapse to second.
                    let sibling = *second.clone();
                    *self = sibling;
                    return self.active_pane();
                }

                if in_second {
                    let result = second.close_pane(pane_id);
                    if result.is_some() {
                        return result;
                    }
                    // Second child is now empty -- collapse to first.
                    let sibling = *first.clone();
                    *self = sibling;
                    return self.active_pane();
                }

                None
            }
        }
    }

    /// Add a pane to the stack containing `target_pane` and make it active.
    ///
    /// Returns `true` if the target stack was found and the pane was added.
    pub fn add_to_stack(&mut self, target_pane: PaneId, new_pane: PaneId) -> bool {
        match self {
            LayoutNode::Stack {
                panes,
                names,
                custom_names,
                active,
            } => {
                if panes.contains(&target_pane) {
                    panes.push(new_pane);
                    names.push(String::new());
                    custom_names.push(None);
                    *active = panes.len() - 1;
                    true
                } else {
                    false
                }
            }
            LayoutNode::Split { first, second, .. } => {
                if first.add_to_stack(target_pane, new_pane) {
                    return true;
                }
                second.add_to_stack(target_pane, new_pane)
            }
        }
    }

    /// Cycle to the next pane in the stack containing `current_pane` (wraps).
    ///
    /// Returns the new active pane ID, or `None` if the pane was not found.
    pub fn stack_next(&mut self, current_pane: PaneId) -> Option<PaneId> {
        match self {
            LayoutNode::Stack { panes, active, .. } => {
                let pos = panes.iter().position(|&p| p == current_pane)?;
                if panes.len() <= 1 {
                    return Some(current_pane);
                }
                *active = (pos + 1) % panes.len();
                Some(panes[*active])
            }
            LayoutNode::Split { first, second, .. } => first
                .stack_next(current_pane)
                .or_else(|| second.stack_next(current_pane)),
        }
    }

    /// Cycle to the previous pane in the stack containing `current_pane` (wraps).
    ///
    /// Returns the new active pane ID, or `None` if the pane was not found.
    pub fn stack_prev(&mut self, current_pane: PaneId) -> Option<PaneId> {
        match self {
            LayoutNode::Stack { panes, active, .. } => {
                let pos = panes.iter().position(|&p| p == current_pane)?;
                if panes.len() <= 1 {
                    return Some(current_pane);
                }
                *active = if pos == 0 { panes.len() - 1 } else { pos - 1 };
                Some(panes[*active])
            }
            LayoutNode::Split { first, second, .. } => first
                .stack_prev(current_pane)
                .or_else(|| second.stack_prev(current_pane)),
        }
    }

    /// Get the active pane ID of the first (leftmost/topmost) stack.
    pub fn active_pane(&self) -> Option<PaneId> {
        match self {
            LayoutNode::Stack { panes, active, .. } => {
                if panes.is_empty() {
                    None
                } else {
                    Some(panes[*active])
                }
            }
            LayoutNode::Split { first, .. } => first.active_pane(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pane naming helpers
// ---------------------------------------------------------------------------

/// Set the display name for a pane in the layout tree.
///
/// Returns `true` if the pane was found and its name was set.
pub fn set_pane_name(node: &mut LayoutNode, pane_id: PaneId, name: &str) -> bool {
    match node {
        LayoutNode::Stack { panes, names, .. } => {
            if let Some(pos) = panes.iter().position(|&p| p == pane_id) {
                // Ensure names vec is long enough.
                while names.len() <= pos {
                    names.push(String::new());
                }
                names[pos] = name.to_string();
                true
            } else {
                false
            }
        }
        LayoutNode::Split { first, second, .. } => {
            if set_pane_name(first, pane_id, name) {
                return true;
            }
            set_pane_name(second, pane_id, name)
        }
    }
}

/// Set a custom (user-assigned) name for a pane. When a custom name is set,
/// the daemon will use it instead of auto-detecting from the process.
///
/// Returns `true` if the pane was found and its custom name was set.
pub fn set_pane_custom_name(node: &mut LayoutNode, pane_id: PaneId, name: &str) -> bool {
    match node {
        LayoutNode::Stack {
            panes,
            custom_names,
            ..
        } => {
            if let Some(pos) = panes.iter().position(|&p| p == pane_id) {
                // Ensure custom_names vec is long enough.
                while custom_names.len() <= pos {
                    custom_names.push(None);
                }
                custom_names[pos] = Some(name.to_string());
                true
            } else {
                false
            }
        }
        LayoutNode::Split { first, second, .. } => {
            if set_pane_custom_name(first, pane_id, name) {
                return true;
            }
            set_pane_custom_name(second, pane_id, name)
        }
    }
}

/// Get the custom name for a pane, if any.
///
/// Returns `Some(Some(name))` if the pane has a user-set custom name,
/// `Some(None)` if the pane exists but has no custom name (auto-detect),
/// or `None` if the pane was not found.
pub fn get_pane_custom_name(node: &LayoutNode, pane_id: PaneId) -> Option<Option<String>> {
    match node {
        LayoutNode::Stack {
            panes,
            custom_names,
            ..
        } => panes
            .iter()
            .position(|&p| p == pane_id)
            .map(|pos| custom_names.get(pos).cloned().unwrap_or(None)),
        LayoutNode::Split { first, second, .. } => {
            get_pane_custom_name(first, pane_id).or_else(|| get_pane_custom_name(second, pane_id))
        }
    }
}

/// Get the display name for a pane.
///
/// Returns `Some(name)` if the pane was found, `None` otherwise.
pub fn get_pane_name(node: &LayoutNode, pane_id: PaneId) -> Option<String> {
    match node {
        LayoutNode::Stack { panes, names, .. } => panes
            .iter()
            .position(|&p| p == pane_id)
            .map(|pos| names.get(pos).cloned().unwrap_or_default()),
        LayoutNode::Split { first, second, .. } => {
            get_pane_name(first, pane_id).or_else(|| get_pane_name(second, pane_id))
        }
    }
}

/// Check whether a layout node (or any of its descendants) contains the
/// given pane.
fn contains_pane(node: &LayoutNode, pane_id: PaneId) -> bool {
    match node {
        LayoutNode::Stack { panes, .. } => panes.contains(&pane_id),
        LayoutNode::Split { first, second, .. } => {
            contains_pane(first, pane_id) || contains_pane(second, pane_id)
        }
    }
}

// ---------------------------------------------------------------------------
// Layout computation
// ---------------------------------------------------------------------------

/// Compute the screen rectangles for all *active* panes in the layout.
///
/// Walks the layout tree recursively. For Split nodes the available area is
/// divided according to `ratio` and `direction`. Minimum pane sizes (2x2)
/// are enforced.
///
/// NOTE: For more sophisticated constraint solving (e.g., minimum sizes per
/// pane, fixed-size status bars), the cassowary crate can be integrated here.
/// The current implementation uses straightforward ratio-based division with
/// min-size clamping.
pub fn compute_layout(node: &LayoutNode, area: Rect, gap_size: u16) -> Vec<(PaneId, Rect)> {
    let mut result = Vec::new();
    compute_layout_inner(node, area, gap_size, &mut result);
    result
}

fn compute_layout_inner(
    node: &LayoutNode,
    area: Rect,
    gap_size: u16,
    out: &mut Vec<(PaneId, Rect)>,
) {
    match node {
        LayoutNode::Stack { panes, active, .. } => {
            if let Some(&pane_id) = panes.get(*active) {
                out.push((pane_id, area));
            }
        }
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (first_area, second_area) = split_rect(area, direction, *ratio, gap_size);
            compute_layout_inner(first, first_area, gap_size, out);
            compute_layout_inner(second, second_area, gap_size, out);
        }
    }
}

/// Divide a rectangle according to a direction and ratio, enforcing minimum
/// pane sizes. When `gap_size > 0`, the gap is subtracted from the available
/// space before dividing. The first child ends before the gap and the second
/// child starts after it.
fn split_rect(area: Rect, direction: &Direction, ratio: f32, gap_size: u16) -> (Rect, Rect) {
    match direction {
        Direction::Vertical => {
            let total = area.width;
            let usable = total.saturating_sub(gap_size);
            let first_width = compute_split_size(usable, ratio);
            let second_width = usable.saturating_sub(first_width);

            let first = Rect {
                x: area.x,
                y: area.y,
                width: first_width,
                height: area.height,
            };
            let second = Rect {
                x: area.x.saturating_add(first_width).saturating_add(gap_size),
                y: area.y,
                width: second_width,
                height: area.height,
            };
            (first, second)
        }
        Direction::Horizontal => {
            let total = area.height;
            let usable = total.saturating_sub(gap_size);
            let first_height = compute_split_size(usable, ratio);
            let second_height = usable.saturating_sub(first_height);

            let first = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: first_height,
            };
            let second = Rect {
                x: area.x,
                y: area.y.saturating_add(first_height).saturating_add(gap_size),
                width: area.width,
                height: second_height,
            };
            (first, second)
        }
    }
}

/// Compute the size of the first child in a split, enforcing minimum sizes.
fn compute_split_size(total: u16, ratio: f32) -> u16 {
    if total < MIN_PANE_SIZE * 2 {
        return total;
    }

    let raw = (f32::from(total) * ratio).round() as u16;
    raw.max(MIN_PANE_SIZE)
        .min(total.saturating_sub(MIN_PANE_SIZE))
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Get all pane IDs in the layout, including hidden (non-active) stacked panes.
pub fn all_pane_ids(node: &LayoutNode) -> Vec<PaneId> {
    let mut result = Vec::new();
    collect_all_pane_ids(node, &mut result);
    result
}

fn collect_all_pane_ids(node: &LayoutNode, out: &mut Vec<PaneId>) {
    match node {
        LayoutNode::Stack { panes, .. } => {
            out.extend(panes);
        }
        LayoutNode::Split { first, second, .. } => {
            collect_all_pane_ids(first, out);
            collect_all_pane_ids(second, out);
        }
    }
}

/// Get the active (visible) pane ID from each stack in the layout.
pub fn active_pane_ids(node: &LayoutNode) -> Vec<PaneId> {
    let mut result = Vec::new();
    collect_active_pane_ids(node, &mut result);
    result
}

fn collect_active_pane_ids(node: &LayoutNode, out: &mut Vec<PaneId>) {
    match node {
        LayoutNode::Stack { panes, active, .. } => {
            if let Some(&pane_id) = panes.get(*active) {
                out.push(pane_id);
            }
        }
        LayoutNode::Split { first, second, .. } => {
            collect_active_pane_ids(first, out);
            collect_active_pane_ids(second, out);
        }
    }
}

/// Find the stack (as a list of pane IDs) that contains the given pane.
pub fn find_stack_for_pane(node: &LayoutNode, pane_id: PaneId) -> Option<Vec<PaneId>> {
    match node {
        LayoutNode::Stack { panes, .. } => {
            if panes.contains(&pane_id) {
                Some(panes.clone())
            } else {
                None
            }
        }
        LayoutNode::Split { first, second, .. } => {
            find_stack_for_pane(first, pane_id).or_else(|| find_stack_for_pane(second, pane_id))
        }
    }
}

/// Find the display name for a given pane by walking the layout tree.
///
/// Returns the name from the `names` vector at the same index as the pane in
/// its stack, or a default empty string if not found.
pub fn find_pane_name(node: &LayoutNode, pane_id: PaneId) -> Option<String> {
    match node {
        LayoutNode::Stack { panes, names, .. } => {
            let idx = panes.iter().position(|&p| p == pane_id)?;
            Some(names.get(idx).cloned().unwrap_or_default())
        }
        LayoutNode::Split { first, second, .. } => {
            find_pane_name(first, pane_id).or_else(|| find_pane_name(second, pane_id))
        }
    }
}

/// Find the stack info for a given pane: (names, pane_ids, active_index).
///
/// Returns `None` if the pane is not found in any stack.
pub fn find_stack_names(
    node: &LayoutNode,
    pane_id: PaneId,
) -> Option<(Vec<String>, Vec<PaneId>, usize)> {
    match node {
        LayoutNode::Stack {
            panes,
            names,
            active,
            ..
        } => {
            if panes.contains(&pane_id) {
                Some((names.clone(), panes.clone(), *active))
            } else {
                None
            }
        }
        LayoutNode::Split { first, second, .. } => {
            find_stack_names(first, pane_id).or_else(|| find_stack_names(second, pane_id))
        }
    }
}

// ---------------------------------------------------------------------------
// Directional focus navigation
// ---------------------------------------------------------------------------

/// Find the neighbor pane when moving in `direction` from `current_pane`.
///
/// Computes all pane rectangles, locates the current pane, then finds the
/// nearest pane in the requested direction based on center-point distance.
pub fn find_neighbor(
    layout: &LayoutNode,
    area: Rect,
    current_pane: PaneId,
    direction: FocusDirection,
    gap_size: u16,
) -> Option<PaneId> {
    let rects = compute_layout(layout, area, gap_size);

    let current_rect = rects.iter().find(|(id, _)| *id == current_pane)?.1;
    let (cx, cy) = rect_center(current_rect);

    let mut best: Option<(PaneId, f64)> = None;

    for &(pane_id, rect) in &rects {
        if pane_id == current_pane {
            continue;
        }

        let (px, py) = rect_center(rect);

        let is_candidate = match direction {
            FocusDirection::Left => px < cx,
            FocusDirection::Right => px > cx,
            FocusDirection::Up => py < cy,
            FocusDirection::Down => py > cy,
        };

        if !is_candidate {
            continue;
        }

        let dist = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();

        if best.is_none() || dist < best.as_ref().map(|b| b.1).unwrap_or(f64::MAX) {
            best = Some((pane_id, dist));
        }
    }

    best.map(|(id, _)| id)
}

/// Compute the center point of a rectangle as (x, y) in f64.
fn rect_center(r: Rect) -> (f64, f64) {
    (
        f64::from(r.x) + f64::from(r.width) / 2.0,
        f64::from(r.y) + f64::from(r.height) / 2.0,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_stack() {
        let node = LayoutNode::new_stack(1);
        match &node {
            LayoutNode::Stack {
                panes,
                names,
                active,
                ..
            } => {
                assert_eq!(panes, &[1]);
                assert_eq!(names, &[""]);
                assert_eq!(*active, 0);
            }
            _ => panic!("expected Stack"),
        }
    }

    #[test]
    fn test_split_vertical() {
        let mut node = LayoutNode::new_stack(1);
        assert!(node.split_vertical(1, 2));

        match &node {
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                assert_eq!(*direction, Direction::Vertical);
                assert!((ratio - 0.5).abs() < f32::EPSILON);
                assert!(matches!(first.as_ref(), LayoutNode::Stack { panes, .. } if panes == &[1]));
                assert!(
                    matches!(second.as_ref(), LayoutNode::Stack { panes, .. } if panes == &[2])
                );
            }
            _ => panic!("expected Split"),
        }
    }

    #[test]
    fn test_split_horizontal() {
        let mut node = LayoutNode::new_stack(1);
        assert!(node.split_horizontal(1, 2));

        match &node {
            LayoutNode::Split { direction, .. } => {
                assert_eq!(*direction, Direction::Horizontal);
            }
            _ => panic!("expected Split"),
        }
    }

    #[test]
    fn test_split_nonexistent_pane() {
        let mut node = LayoutNode::new_stack(1);
        assert!(!node.split_vertical(99, 2));
    }

    #[test]
    fn test_nested_split() {
        let mut node = LayoutNode::new_stack(1);
        assert!(node.split_vertical(1, 2));
        assert!(node.split_horizontal(2, 3));

        let ids = all_pane_ids(&node);
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));
    }

    #[test]
    fn test_compute_layout_single() {
        let node = LayoutNode::new_stack(1);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let rects = compute_layout(&node, area, 0);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], (1, area));
    }

    #[test]
    fn test_compute_layout_vertical_split() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let rects = compute_layout(&node, area, 0);
        assert_eq!(rects.len(), 2);

        let (id1, r1) = rects[0];
        let (id2, r2) = rects[1];
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(r1.x, 0);
        assert_eq!(r1.width, 40);
        assert_eq!(r2.x, 40);
        assert_eq!(r2.width, 40);
        assert_eq!(r1.height, 24);
        assert_eq!(r2.height, 24);
    }

    #[test]
    fn test_compute_layout_horizontal_split() {
        let mut node = LayoutNode::new_stack(1);
        node.split_horizontal(1, 2);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let rects = compute_layout(&node, area, 0);
        assert_eq!(rects.len(), 2);

        let (_, r1) = rects[0];
        let (_, r2) = rects[1];
        assert_eq!(r1.y, 0);
        assert_eq!(r1.height, 12);
        assert_eq!(r2.y, 12);
        assert_eq!(r2.height, 12);
    }

    #[test]
    fn test_compute_layout_min_size() {
        let node = LayoutNode::Split {
            direction: Direction::Vertical,
            ratio: 0.01,
            first: Box::new(LayoutNode::new_stack(1)),
            second: Box::new(LayoutNode::new_stack(2)),
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let rects = compute_layout(&node, area, 0);
        assert!(rects[0].1.width >= MIN_PANE_SIZE);
        assert!(rects[1].1.width >= MIN_PANE_SIZE);
    }

    #[test]
    fn test_all_pane_ids() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);
        node.add_to_stack(1, 3);

        let ids = all_pane_ids(&node);
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));
    }

    #[test]
    fn test_active_pane_ids() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);
        node.add_to_stack(1, 3);

        let active = active_pane_ids(&node);
        assert_eq!(active.len(), 2);
        assert!(active.contains(&3));
        assert!(active.contains(&2));
    }

    #[test]
    fn test_find_stack_for_pane() {
        let mut node = LayoutNode::new_stack(1);
        node.add_to_stack(1, 2);
        node.split_vertical(1, 3);

        let stack = find_stack_for_pane(&node, 2);
        assert!(stack.is_some());
        let panes = stack.unwrap();
        assert!(panes.contains(&1));
        assert!(panes.contains(&2));

        assert!(find_stack_for_pane(&node, 99).is_none());
    }

    #[test]
    fn test_resize() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);

        assert!(node.resize(1, Direction::Vertical, 0.1));

        match &node {
            LayoutNode::Split { ratio, .. } => {
                assert!((ratio - 0.6).abs() < f32::EPSILON);
            }
            _ => panic!("expected Split"),
        }
    }

    #[test]
    fn test_resize_clamp() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);

        assert!(node.resize(1, Direction::Vertical, 10.0));
        match &node {
            LayoutNode::Split { ratio, .. } => {
                assert!((ratio - MAX_RATIO).abs() < f32::EPSILON);
            }
            _ => panic!("expected Split"),
        }
    }

    #[test]
    fn test_resize_wrong_direction() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);

        assert!(!node.resize(1, Direction::Horizontal, 0.1));
    }

    #[test]
    fn test_close_pane_stack_has_others() {
        let mut node = LayoutNode::new_stack(1);
        node.add_to_stack(1, 2);

        let next = node.close_pane(1);
        assert_eq!(next, Some(2));

        match &node {
            LayoutNode::Stack { panes, active, .. } => {
                assert_eq!(panes, &[2]);
                assert_eq!(*active, 0);
            }
            _ => panic!("expected Stack"),
        }
    }

    #[test]
    fn test_close_pane_simplifies_tree() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);

        let next = node.close_pane(1);
        assert_eq!(next, Some(2));

        match &node {
            LayoutNode::Stack { panes, .. } => {
                assert_eq!(panes, &[2]);
            }
            _ => panic!("expected Stack after tree simplification"),
        }
    }

    #[test]
    fn test_close_last_pane() {
        let mut node = LayoutNode::new_stack(1);
        let next = node.close_pane(1);
        assert_eq!(next, None);
    }

    #[test]
    fn test_add_to_stack() {
        let mut node = LayoutNode::new_stack(1);
        assert!(node.add_to_stack(1, 2));

        match &node {
            LayoutNode::Stack {
                panes,
                names,
                active,
                ..
            } => {
                assert_eq!(panes, &[1, 2]);
                assert_eq!(names, &["", ""]);
                assert_eq!(*active, 1);
            }
            _ => panic!("expected Stack"),
        }
    }

    #[test]
    fn test_stack_next() {
        let mut node = LayoutNode::new_stack(1);
        node.add_to_stack(1, 2);
        node.add_to_stack(2, 3);

        assert_eq!(node.stack_next(3), Some(1));
        assert_eq!(node.stack_next(1), Some(2));
    }

    #[test]
    fn test_stack_prev() {
        let mut node = LayoutNode::new_stack(1);
        node.add_to_stack(1, 2);
        node.add_to_stack(2, 3);

        assert_eq!(node.stack_prev(1), Some(3));
        assert_eq!(node.stack_prev(3), Some(2));
    }

    #[test]
    fn test_stack_single_pane_cycle() {
        let mut node = LayoutNode::new_stack(1);
        assert_eq!(node.stack_next(1), Some(1));
        assert_eq!(node.stack_prev(1), Some(1));
    }

    #[test]
    fn test_active_pane() {
        let node = LayoutNode::new_stack(42);
        assert_eq!(node.active_pane(), Some(42));
    }

    #[test]
    fn test_find_neighbor_left_right() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };

        assert_eq!(
            find_neighbor(&node, area, 1, FocusDirection::Right, 0),
            Some(2)
        );
        assert_eq!(
            find_neighbor(&node, area, 2, FocusDirection::Left, 0),
            Some(1)
        );
        assert_eq!(find_neighbor(&node, area, 1, FocusDirection::Left, 0), None);
        assert_eq!(
            find_neighbor(&node, area, 2, FocusDirection::Right, 0),
            None
        );
    }

    #[test]
    fn test_find_neighbor_up_down() {
        let mut node = LayoutNode::new_stack(1);
        node.split_horizontal(1, 2);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };

        assert_eq!(
            find_neighbor(&node, area, 1, FocusDirection::Down, 0),
            Some(2)
        );
        assert_eq!(
            find_neighbor(&node, area, 2, FocusDirection::Up, 0),
            Some(1)
        );
    }

    #[test]
    fn test_compute_layout_vertical_split_with_gaps() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let rects = compute_layout(&node, area, 2);
        assert_eq!(rects.len(), 2);

        let (id1, r1) = rects[0];
        let (id2, r2) = rects[1];
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        // 80 - 2 gap = 78 usable, split 50/50 = 39 each
        assert_eq!(r1.width, 39);
        assert_eq!(r2.width, 39);
        // First pane starts at 0, second starts at 39 + 2 gap = 41
        assert_eq!(r1.x, 0);
        assert_eq!(r2.x, 41);
    }

    #[test]
    fn test_compute_layout_horizontal_split_with_gaps() {
        let mut node = LayoutNode::new_stack(1);
        node.split_horizontal(1, 2);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let rects = compute_layout(&node, area, 2);
        assert_eq!(rects.len(), 2);

        let (_, r1) = rects[0];
        let (_, r2) = rects[1];
        // 24 - 2 gap = 22 usable, split 50/50 = 11 each
        assert_eq!(r1.height, 11);
        assert_eq!(r2.height, 11);
        assert_eq!(r1.y, 0);
        assert_eq!(r2.y, 13); // 11 + 2 gap
    }

    #[test]
    fn test_compute_layout_nested_splits_with_gaps() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);
        node.split_horizontal(2, 3);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let rects = compute_layout(&node, area, 1);
        assert_eq!(rects.len(), 3);
        // All rects should have positive dimensions
        for (_, r) in &rects {
            assert!(r.width > 0);
            assert!(r.height > 0);
        }
    }

    #[test]
    fn test_single_pane_no_gaps_applied() {
        let node = LayoutNode::new_stack(1);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        // Even with gap_size > 0, a single pane should occupy the full area
        let rects = compute_layout(&node, area, 5);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], (1, area));
    }

    #[test]
    fn test_gap_with_min_size_enforcement() {
        // Very small area with a large gap -- min pane sizes should be enforced
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);
        let area = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 10,
        };
        let rects = compute_layout(&node, area, 2);
        assert_eq!(rects.len(), 2);
        // Both panes should still meet minimum size
        assert!(rects[0].1.width >= MIN_PANE_SIZE);
        assert!(rects[1].1.width >= MIN_PANE_SIZE);
    }

    #[test]
    fn test_set_pane_name() {
        let mut node = LayoutNode::new_stack(1);
        node.add_to_stack(1, 2);

        assert!(set_pane_name(&mut node, 1, "bash"));
        assert_eq!(get_pane_name(&node, 1), Some("bash".to_string()));
        // Pane 2 should still have default empty name.
        assert_eq!(get_pane_name(&node, 2), Some(String::new()));
    }

    #[test]
    fn test_set_pane_custom_name() {
        let mut node = LayoutNode::new_stack(1);
        assert!(set_pane_custom_name(&mut node, 1, "my-custom-name"));
        assert_eq!(
            get_pane_custom_name(&node, 1),
            Some(Some("my-custom-name".to_string()))
        );
    }

    #[test]
    fn test_name_sync_on_add_to_stack() {
        let mut node = LayoutNode::new_stack(1);
        set_pane_name(&mut node, 1, "first");
        node.add_to_stack(1, 2);

        match &node {
            LayoutNode::Stack {
                panes,
                names,
                custom_names,
                ..
            } => {
                assert_eq!(panes.len(), 2);
                assert_eq!(names.len(), 2);
                assert_eq!(custom_names.len(), 2);
                assert_eq!(names[0], "first");
                assert_eq!(names[1], ""); // newly added pane gets empty name
            }
            _ => panic!("expected Stack"),
        }
    }

    #[test]
    fn test_name_sync_on_close_pane() {
        let mut node = LayoutNode::new_stack(1);
        node.add_to_stack(1, 2);
        node.add_to_stack(2, 3);
        set_pane_name(&mut node, 1, "first");
        set_pane_name(&mut node, 2, "second");
        set_pane_name(&mut node, 3, "third");

        // Close the middle pane.
        let next = node.close_pane(2);
        assert!(next.is_some());

        match &node {
            LayoutNode::Stack {
                panes,
                names,
                custom_names,
                ..
            } => {
                assert_eq!(panes.len(), 2);
                assert_eq!(names.len(), 2);
                assert_eq!(custom_names.len(), 2);
                assert_eq!(panes, &[1, 3]);
                assert_eq!(names, &["first", "third"]);
            }
            _ => panic!("expected Stack"),
        }
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut node = LayoutNode::new_stack(1);
        node.split_vertical(1, 2);
        node.add_to_stack(1, 3);

        let json = serde_json::to_string(&node).expect("serialize");
        let deserialized: LayoutNode = serde_json::from_str(&json).expect("deserialize");

        let original_ids = all_pane_ids(&node);
        let deser_ids = all_pane_ids(&deserialized);
        assert_eq!(original_ids, deser_ids);
    }
}
