//! Binary space-partition tiling tree. Panes are leaves; splits are internal
//! nodes with a ratio. See docs/04-data-model.md §4.

use std::collections::{HashMap, HashSet};

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use crate::ids::PaneId;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// Children side by side (vertical divider).
    Col,
    /// Children stacked (horizontal divider).
    Row,
}

enum Node {
    Leaf(PaneId),
    Split {
        axis: Axis,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

pub struct PaneInfo {
    pub id: PaneId,
    pub rect: Rect,
}

/// A resizable boundary between two panes: the split node it belongs to (as a
/// root→node path of A/B turns), its axis, the cell line it sits on, and the
/// perpendicular extent it spans. Produced by [`TileLayout::dividers`].
#[derive(Clone)]
pub struct Divider {
    /// Turns from the root to the owning `Split` (`false` → child A, `true` → B).
    pub path: Vec<bool>,
    pub axis: Axis,
    /// Col: the divider column x. Row: the divider row y.
    pub line: u16,
    /// Perpendicular extent `[lo, hi)` the divider spans.
    pub span: (u16, u16),
}

/// Minimum cells a pane keeps along the split axis, so a resize can't crush it.
pub const MIN_PANE: u16 = 4;

pub struct TileLayout {
    root: Node,
    pub focus: PaneId,
}

/// Serializable mirror of the tree (leaves carry the runtime pane id at save
/// time). Used by persistence; see docs/09.
#[derive(Clone, Serialize, Deserialize)]
pub enum LayoutTree {
    Leaf(u32),
    Split {
        axis: u8, // 0 = Col, 1 = Row
        ratio: f32,
        a: Box<LayoutTree>,
        b: Box<LayoutTree>,
    },
}

impl TileLayout {
    pub fn new(root: PaneId) -> Self {
        TileLayout {
            root: Node::Leaf(root),
            focus: root,
        }
    }

    pub fn to_tree(&self) -> LayoutTree {
        node_to_tree(&self.root)
    }

    /// Rebuild a layout from a saved tree, mapping old raw ids to freshly
    /// allocated panes. Returns `None` if no leaf survives.
    pub fn from_tree(
        tree: &LayoutTree,
        remap: &HashMap<u32, PaneId>,
        focus_raw: u32,
    ) -> Option<Self> {
        let root = build_node(tree, remap)?;
        let focus = remap
            .get(&focus_raw)
            .copied()
            .unwrap_or_else(|| first_leaf(&root));
        Some(TileLayout { root, focus })
    }

    pub fn len(&self) -> usize {
        count(&self.root)
    }

    /// Leaves in left-to-right / top-to-bottom order.
    pub fn leaves(&self) -> Vec<PaneId> {
        let mut v = Vec::new();
        collect_leaves(&self.root, &mut v);
        v
    }

    /// Whether a pane belongs to this layout, without allocating a leaf list.
    pub fn contains(&self, id: PaneId) -> bool {
        contains_leaf(&self.root, id)
    }

    /// Geometry for every pane within `area`.
    pub fn panes(&self, area: Rect) -> Vec<PaneInfo> {
        let mut v = Vec::new();
        collect(&self.root, area, &mut v);
        v
    }

    /// Replace the focused leaf with a split and focus the new pane.
    pub fn split_focused(&mut self, axis: Axis, new_id: PaneId) {
        split_at(&mut self.root, self.focus, axis, new_id);
        self.focus = new_id;
    }

    /// Remove a pane, collapsing its parent split. Returns `true` if the tree
    /// is now empty.
    pub fn remove(&mut self, id: PaneId) -> bool {
        let root = std::mem::replace(&mut self.root, Node::Leaf(id));
        match remove_node(root, id) {
            Some(n) => {
                self.root = n;
                if self.focus == id {
                    self.focus = first_leaf(&self.root);
                }
                false
            }
            None => true,
        }
    }

    /// Move focus to the nearest pane in `dir` (geometric).
    pub fn focus_dir(&mut self, area: Rect, dir: Dir) {
        if let Some(id) = self.find_in_direction_from(area, self.focus, dir) {
            self.focus = id;
        }
    }

    /// Query directional navigation without changing focus. Socket inspection
    /// uses exactly the same geometric rule as the TUI.
    pub fn neighbor(&self, area: Rect, pane: PaneId, dir: Dir) -> Option<PaneId> {
        self.find_in_direction_from(area, pane, dir)
    }

    pub fn pane_rect(&self, area: Rect, pane: PaneId) -> Option<Rect> {
        self.panes(area)
            .into_iter()
            .find(|candidate| candidate.id == pane)
            .map(|candidate| candidate.rect)
    }

    /// Swap two leaf identities without moving or recreating either PTY.
    pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        if first == second || !self.contains(first) || !self.contains(second) {
            return false;
        }
        swap_leaf_ids(&mut self.root, first, second);
        true
    }

    /// Replace the tree only when it contains every current pane exactly once.
    /// This makes `layout.apply` atomic and preserves all pane-owned state.
    pub fn apply_tree(&mut self, tree: &LayoutTree, focus: PaneId) -> Result<(), &'static str> {
        let current: HashSet<u32> = self.leaves().into_iter().map(|id| id.0).collect();
        let mut requested = Vec::new();
        collect_raw_leaves(tree, &mut requested);
        let requested_set: HashSet<u32> = requested.iter().copied().collect();
        if requested_set.len() != requested.len() {
            return Err("layout contains a duplicate pane");
        }
        if requested_set != current {
            return Err("layout must contain every pane in the tab exactly once");
        }
        if !requested_set.contains(&focus.0) {
            return Err("layout focus is not part of the tree");
        }
        self.root = build_live_node(tree, Rect::new(0, 0, 10_000, 10_000));
        self.focus = focus;
        Ok(())
    }

    /// The pane to move focus to, or `None` when there is nothing that way.
    ///
    /// Movement is geometric, and the rule that makes it feel right is that a
    /// candidate must actually **face** the current pane: it has to begin beyond
    /// the edge we are leaving, *and* its span on the perpendicular axis has to
    /// overlap ours.
    ///
    /// Comparing centres alone (what this used to do) let a pane win merely for
    /// sitting further along the axis, however far off to the side it was. With
    /// one pane on the left and two stacked on the right, Down from the top-right
    /// pane jumped back to the **left** pane: its centre was lower, and the cost
    /// function weighted along-axis distance 1000x against sideways offset, so
    /// being in a completely different column cost almost nothing.
    fn find_in_direction_from(&self, area: Rect, from: PaneId, dir: Dir) -> Option<PaneId> {
        let panes = self.panes(area);
        let cur = panes.iter().find(|p| p.id == from)?;
        let (c_lo, c_hi) = perp_span(cur.rect, dir);

        // Ordered by (gap along the axis, widest overlap, then topmost/leftmost)
        // — nearest first, then the pane most face-to-face with us, with a
        // positional tiebreak so the choice is always deterministic.
        let mut best: Option<((i64, i64, i64), PaneId)> = None;
        // Only used when nothing overlaps at all, which a ragged layout or a
        // configured gap can produce. Without it such a move would silently do
        // nothing, which reads as a dead keypress.
        let mut fallback: Option<((i64, i64), PaneId)> = None;

        for p in &panes {
            if p.id == from {
                continue;
            }
            let gap = gap_towards(cur.rect, p.rect, dir);
            if gap < 0 {
                continue; // behind us, or straddling the edge we are leaving
            }
            let (p_lo, p_hi) = perp_span(p.rect, dir);
            let overlap = c_hi.min(p_hi) - c_lo.max(p_lo);
            if overlap > 0 {
                let key = (gap, -overlap, p_lo);
                if best.as_ref().is_none_or(|(k, _)| key < *k) {
                    best = Some((key, p.id));
                }
            } else {
                let key = (c_lo.max(p_lo) - c_hi.min(p_hi), gap);
                if fallback.as_ref().is_none_or(|(k, _)| key < *k) {
                    fallback = Some((key, p.id));
                }
            }
        }
        best.map(|(_, id)| id).or(fallback.map(|(_, id)| id))
    }

    // ── resize (docs/27) ────────────────────────────────────────────────────

    /// Every inter-pane divider within `area` (the same recursion as `panes`).
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        let mut out = Vec::new();
        let mut path = Vec::new();
        collect_dividers(&self.root, area, &mut path, &mut out);
        out
    }

    /// The divider within `tol` cells of `(c, r)` (nearest wins), or `None`.
    pub fn divider_at(&self, area: Rect, c: u16, r: u16, tol: u16) -> Option<Divider> {
        let mut best: Option<(u16, Divider)> = None;
        for d in self.dividers(area) {
            let (on_span, dist) = match d.axis {
                Axis::Col => (
                    r >= d.span.0 && r < d.span.1,
                    (c as i32 - d.line as i32).unsigned_abs() as u16,
                ),
                Axis::Row => (
                    c >= d.span.0 && c < d.span.1,
                    (r as i32 - d.line as i32).unsigned_abs() as u16,
                ),
            };
            if on_span && dist <= tol && best.as_ref().is_none_or(|(bd, _)| dist < *bd) {
                best = Some((dist, d));
            }
        }
        best.map(|(_, d)| d)
    }

    /// The divider nearest `(c, r)` regardless of distance (the Ctrl+drag path).
    pub fn nearest_divider(&self, area: Rect, c: u16, r: u16) -> Option<Divider> {
        self.dividers(area)
            .into_iter()
            .min_by_key(|d| match d.axis {
                Axis::Col => (c as i32 - d.line as i32).unsigned_abs(),
                Axis::Row => (r as i32 - d.line as i32).unsigned_abs(),
            })
    }

    /// The rect the split node at `path` currently occupies (root walk with live
    /// ratios) — converts a mouse position into a ratio during a drag.
    pub fn node_rect(&self, area: Rect, path: &[bool]) -> Option<Rect> {
        let mut node = &self.root;
        let mut rect = area;
        for &side in path {
            match node {
                Node::Split { axis, ratio, a, b } => {
                    let (ra, rb) = split_rect(rect, *axis, *ratio);
                    if side {
                        node = b;
                        rect = rb;
                    } else {
                        node = a;
                        rect = ra;
                    }
                }
                Node::Leaf(_) => return None,
            }
        }
        Some(rect)
    }

    /// Set the ratio of the split node at `path`, clamped so both children keep
    /// at least [`MIN_PANE`] cells.
    pub fn set_ratio(&mut self, area: Rect, path: &[bool], ratio: f32) {
        let Some(rect) = self.node_rect(area, path) else {
            return;
        };
        let mut node = &mut self.root;
        for &side in path {
            match node {
                Node::Split { a, b, .. } => node = if side { b } else { a },
                Node::Leaf(_) => return,
            }
        }
        if let Node::Split { axis, ratio: r, .. } = node {
            let len = match axis {
                Axis::Col => rect.width,
                Axis::Row => rect.height,
            };
            if len == 0 {
                return;
            }
            // Cap `min` at 0.5 so a node too small for two MIN_PANE children just
            // centers (a valid clamp range) instead of panicking.
            let min = (MIN_PANE as f32 / len as f32).min(0.5);
            *r = ratio.clamp(min, 1.0 - min);
        }
    }

    /// Validated ratio mutation for the Socket API. Returns false when `path`
    /// does not identify a split; a failed request never mutates the tree.
    pub fn try_set_ratio(&mut self, area: Rect, path: &[bool], ratio: f32) -> bool {
        if !ratio.is_finite()
            || !matches!(self.node_at(path), Some(Node::Split { .. }))
            || self.node_rect(area, path).is_none()
        {
            return false;
        }
        self.set_ratio(area, path, ratio);
        true
    }

    /// Keyboard resize: grow the focused pane toward `dir` by `delta` cells, by
    /// nudging the nearest ancestor split whose divider is on that side. Returns
    /// whether a split moved.
    pub fn resize_focused(&mut self, area: Rect, dir: Dir, delta: i16) -> bool {
        // (axis to move, which side of that split the focus must be on, sign).
        let (want_axis, want_side, sign): (Axis, bool, f32) = match dir {
            Dir::Right => (Axis::Col, false, 1.0), // focus in child A, grow A
            Dir::Left => (Axis::Col, true, -1.0),  // focus in child B, grow B
            Dir::Down => (Axis::Row, false, 1.0),
            Dir::Up => (Axis::Row, true, -1.0),
        };
        let mut leaf_path = Vec::new();
        if !path_to_leaf(&self.root, self.focus, &mut leaf_path) {
            return false;
        }
        // Deepest ancestor split matching (axis, side) — closest to the focus.
        for k in (0..leaf_path.len()).rev() {
            if leaf_path[k] != want_side {
                continue;
            }
            let prefix = leaf_path[..k].to_vec();
            let (axis, cur) = match self.node_at(&prefix) {
                Some(Node::Split { axis, ratio, .. }) => (*axis, *ratio),
                _ => continue,
            };
            if axis != want_axis {
                continue;
            }
            let Some(rect) = self.node_rect(area, &prefix) else {
                continue;
            };
            let len = match want_axis {
                Axis::Col => rect.width,
                Axis::Row => rect.height,
            };
            if len == 0 {
                return false;
            }
            self.set_ratio(area, &prefix, cur + sign * (delta as f32 / len as f32));
            return true;
        }
        false
    }

    /// Reset every split to an even 50/50 (the `=` action in resize mode).
    pub fn equalize(&mut self) {
        equalize_node(&mut self.root);
    }

    fn node_at(&self, path: &[bool]) -> Option<&Node> {
        let mut node = &self.root;
        for &side in path {
            match node {
                Node::Split { a, b, .. } => node = if side { b } else { a },
                Node::Leaf(_) => return None,
            }
        }
        Some(node)
    }
}

fn count(node: &Node) -> usize {
    match node {
        Node::Leaf(_) => 1,
        Node::Split { a, b, .. } => count(a) + count(b),
    }
}

fn collect_leaves(node: &Node, out: &mut Vec<PaneId>) {
    match node {
        Node::Leaf(id) => out.push(*id),
        Node::Split { a, b, .. } => {
            collect_leaves(a, out);
            collect_leaves(b, out);
        }
    }
}

fn swap_leaf_ids(node: &mut Node, first: PaneId, second: PaneId) {
    match node {
        Node::Leaf(id) if *id == first => *id = second,
        Node::Leaf(id) if *id == second => *id = first,
        Node::Leaf(_) => {}
        Node::Split { a, b, .. } => {
            swap_leaf_ids(a, first, second);
            swap_leaf_ids(b, first, second);
        }
    }
}

fn collect_raw_leaves(tree: &LayoutTree, out: &mut Vec<u32>) {
    match tree {
        LayoutTree::Leaf(raw) => out.push(*raw),
        LayoutTree::Split { a, b, .. } => {
            collect_raw_leaves(a, out);
            collect_raw_leaves(b, out);
        }
    }
}

fn build_live_node(tree: &LayoutTree, area: Rect) -> Node {
    match tree {
        LayoutTree::Leaf(raw) => Node::Leaf(PaneId(*raw)),
        LayoutTree::Split { axis, ratio, a, b } => {
            let axis = if *axis == 0 { Axis::Col } else { Axis::Row };
            let extent = match axis {
                Axis::Col => area.width,
                Axis::Row => area.height,
            };
            let gap = match axis {
                Axis::Col => GAP_COL.load(Ordering::Relaxed),
                Axis::Row => GAP_ROW.load(Ordering::Relaxed),
            };
            let available = extent.saturating_sub(gap);
            let min_ratio = if available >= MIN_PANE.saturating_mul(2) {
                MIN_PANE as f32 / available as f32
            } else {
                0.5
            };
            let ratio = ratio.clamp(min_ratio, 1.0 - min_ratio);
            let (a_area, b_area) = split_rect(area, axis, ratio);
            Node::Split {
                axis,
                ratio,
                a: Box::new(build_live_node(a, a_area)),
                b: Box::new(build_live_node(b, b_area)),
            }
        }
    }
}

fn contains_leaf(node: &Node, id: PaneId) -> bool {
    match node {
        Node::Leaf(pane) => *pane == id,
        Node::Split { a, b, .. } => contains_leaf(a, id) || contains_leaf(b, id),
    }
}

fn collect(node: &Node, area: Rect, out: &mut Vec<PaneInfo>) {
    match node {
        Node::Leaf(id) => out.push(PaneInfo {
            id: *id,
            rect: area,
        }),
        Node::Split { axis, ratio, a, b } => {
            let (ra, rb) = split_rect(area, *axis, *ratio);
            collect(a, ra, out);
            collect(b, rb, out);
        }
    }
}

/// Gap (in cells) between split children — configurable via Settings (Pane
/// Layout). Defaults: a one-column gap between left/right panes, none between
/// top/bottom. Stored as atomics so the recursive split math can read them
/// without threading config through every call.
use std::sync::atomic::{AtomicU16, Ordering};
static GAP_COL: AtomicU16 = AtomicU16::new(1);
static GAP_ROW: AtomicU16 = AtomicU16::new(0);

/// Set the inter-pane gaps (from `config.layout`). Clamped to 0..=1.
pub fn set_gaps(col: u16, row: u16) {
    GAP_COL.store(col.min(1), Ordering::Relaxed);
    GAP_ROW.store(row.min(1), Ordering::Relaxed);
}

fn split_rect(area: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    let gap_col = GAP_COL.load(Ordering::Relaxed);
    let gap_row = GAP_ROW.load(Ordering::Relaxed);
    match axis {
        Axis::Col => {
            let avail = area.width.saturating_sub(gap_col);
            let w1 = ((avail as f32) * ratio)
                .round()
                .clamp(1.0, (avail.saturating_sub(1)).max(1) as f32) as u16;
            let w2 = avail.saturating_sub(w1);
            (
                Rect::new(area.x, area.y, w1, area.height),
                Rect::new(area.x + w1 + gap_col, area.y, w2, area.height),
            )
        }
        Axis::Row => {
            let avail = area.height.saturating_sub(gap_row);
            let h1 = ((avail as f32) * ratio)
                .round()
                .clamp(1.0, (avail.saturating_sub(1)).max(1) as f32) as u16;
            let h2 = avail.saturating_sub(h1);
            (
                Rect::new(area.x, area.y, area.width, h1),
                Rect::new(area.x, area.y + h1 + gap_row, area.width, h2),
            )
        }
    }
}

fn split_at(node: &mut Node, target: PaneId, axis: Axis, new_id: PaneId) -> bool {
    match node {
        Node::Leaf(id) if *id == target => {
            let old = *id;
            *node = Node::Split {
                axis,
                ratio: 0.5,
                a: Box::new(Node::Leaf(old)),
                b: Box::new(Node::Leaf(new_id)),
            };
            true
        }
        Node::Leaf(_) => false,
        Node::Split { a, b, .. } => {
            split_at(a, target, axis, new_id) || split_at(b, target, axis, new_id)
        }
    }
}

fn remove_node(node: Node, id: PaneId) -> Option<Node> {
    match node {
        Node::Leaf(x) => {
            if x == id {
                None
            } else {
                Some(Node::Leaf(x))
            }
        }
        Node::Split { axis, ratio, a, b } => {
            let na = remove_node(*a, id);
            let nb = remove_node(*b, id);
            match (na, nb) {
                (Some(a), Some(b)) => Some(Node::Split {
                    axis,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                // One child removed → collapse to the survivor.
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        }
    }
}

fn first_leaf(node: &Node) -> PaneId {
    match node {
        Node::Leaf(id) => *id,
        Node::Split { a, .. } => first_leaf(a),
    }
}

fn node_to_tree(node: &Node) -> LayoutTree {
    match node {
        Node::Leaf(id) => LayoutTree::Leaf(id.0),
        Node::Split { axis, ratio, a, b } => LayoutTree::Split {
            axis: match axis {
                Axis::Col => 0,
                Axis::Row => 1,
            },
            ratio: *ratio,
            a: Box::new(node_to_tree(a)),
            b: Box::new(node_to_tree(b)),
        },
    }
}

fn build_node(tree: &LayoutTree, remap: &HashMap<u32, PaneId>) -> Option<Node> {
    match tree {
        LayoutTree::Leaf(raw) => remap.get(raw).map(|id| Node::Leaf(*id)),
        LayoutTree::Split { axis, ratio, a, b } => {
            let na = build_node(a, remap);
            let nb = build_node(b, remap);
            match (na, nb) {
                (Some(a), Some(b)) => Some(Node::Split {
                    axis: if *axis == 0 { Axis::Col } else { Axis::Row },
                    ratio: *ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        }
    }
}

/// How far `other` begins beyond the edge of `cur` that we are moving off.
/// Negative means it is not in that direction at all, so it is not a candidate.
fn gap_towards(cur: Rect, other: Rect, dir: Dir) -> i64 {
    let (c, o) = (cur, other);
    match dir {
        Dir::Right => o.x as i64 - (c.x as i64 + c.width as i64),
        Dir::Left => c.x as i64 - (o.x as i64 + o.width as i64),
        Dir::Down => o.y as i64 - (c.y as i64 + c.height as i64),
        Dir::Up => c.y as i64 - (o.y as i64 + o.height as i64),
    }
}

/// A rect's extent on the axis **perpendicular** to `dir`, as `(start, end)`.
/// Moving left/right, that is its vertical span; moving up/down, horizontal.
fn perp_span(r: Rect, dir: Dir) -> (i64, i64) {
    match dir {
        Dir::Left | Dir::Right => (r.y as i64, r.y as i64 + r.height as i64),
        Dir::Up | Dir::Down => (r.x as i64, r.x as i64 + r.width as i64),
    }
}

/// Emit every split's divider (line + perpendicular span), tagging each with the
/// path taken to reach it, then recurse into both children.
fn collect_dividers(node: &Node, area: Rect, path: &mut Vec<bool>, out: &mut Vec<Divider>) {
    if let Node::Split { axis, ratio, a, b } = node {
        let (ra, rb) = split_rect(area, *axis, *ratio);
        let (line, span) = match axis {
            // The boundary sits just past child A (gap-aware, since `split_rect`
            // already subtracted the gap when sizing A).
            Axis::Col => (ra.right(), (area.y, area.bottom())),
            Axis::Row => (ra.bottom(), (area.x, area.right())),
        };
        out.push(Divider {
            path: path.clone(),
            axis: *axis,
            line,
            span,
        });
        path.push(false);
        collect_dividers(a, ra, path, out);
        path.pop();
        path.push(true);
        collect_dividers(b, rb, path, out);
        path.pop();
    }
}

/// Fill `out` with the A/B turns from the root to the leaf `target`. Returns
/// `false` (leaving `out` restored) if the leaf isn't in this subtree.
fn path_to_leaf(node: &Node, target: PaneId, out: &mut Vec<bool>) -> bool {
    match node {
        Node::Leaf(id) => *id == target,
        Node::Split { a, b, .. } => {
            out.push(false);
            if path_to_leaf(a, target, out) {
                return true;
            }
            out.pop();
            out.push(true);
            if path_to_leaf(b, target, out) {
                return true;
            }
            out.pop();
            false
        }
    }
}

fn equalize_node(node: &mut Node) {
    if let Node::Split { ratio, a, b, .. } = node {
        *ratio = 0.5;
        equalize_node(a);
        equalize_node(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_navigate() {
        let a = PaneId::alloc();
        let mut l = TileLayout::new(a);
        assert_eq!(l.len(), 1);

        let b = PaneId::alloc();
        l.split_focused(Axis::Col, b); // a | b
        assert_eq!(l.len(), 2);
        assert!(l.contains(a));
        assert!(l.contains(b));
        assert_eq!(l.focus, b);

        let area = Rect::new(0, 0, 80, 24);
        l.focus_dir(area, Dir::Left);
        assert_eq!(l.focus, a);
        l.focus_dir(area, Dir::Right);
        assert_eq!(l.focus, b);

        assert!(!l.remove(b)); // back to just `a`
        assert_eq!(l.len(), 1);
        assert_eq!(l.focus, a);
        assert!(!l.contains(b));
        assert!(l.remove(a)); // empty
    }

    #[test]
    fn socket_queries_and_tree_apply_preserve_live_leaf_identity() {
        let first = PaneId::alloc();
        let second = PaneId::alloc();
        let mut layout = TileLayout::new(first);
        layout.split_focused(Axis::Col, second);
        let area = Rect::new(0, 0, 100, 40);

        let focus = layout.focus;
        assert_eq!(layout.neighbor(area, second, Dir::Left), Some(first));
        assert_eq!(layout.focus, focus, "inspection never changes focus");

        assert!(layout.swap_panes(first, second));
        assert_eq!(
            layout.focus, focus,
            "swapping positions preserves focus identity"
        );

        let before = serde_json::to_value(layout.to_tree()).unwrap();
        let duplicate = LayoutTree::Split {
            axis: 0,
            ratio: 0.5,
            a: Box::new(LayoutTree::Leaf(first.0)),
            b: Box::new(LayoutTree::Leaf(first.0)),
        };
        assert!(layout.apply_tree(&duplicate, first).is_err());
        assert_eq!(
            serde_json::to_value(layout.to_tree()).unwrap(),
            before,
            "invalid replacement is atomic"
        );

        let extreme = LayoutTree::Split {
            axis: 0,
            ratio: 0.0,
            a: Box::new(LayoutTree::Leaf(first.0)),
            b: Box::new(LayoutTree::Leaf(second.0)),
        };
        layout.apply_tree(&extreme, first).unwrap();
        let LayoutTree::Split { ratio, .. } = layout.to_tree() else {
            panic!("applied split must remain a split");
        };
        assert!(ratio > 0.0 && ratio < 1.0);
        assert!(layout
            .panes(Rect::new(0, 0, 10_000, 10_000))
            .iter()
            .all(|pane| pane.rect.width >= MIN_PANE));
    }

    /// The reported bug: one pane on the left, two stacked on the right. From
    /// the top-right pane, Down jumped back to the *left* pane instead of the
    /// one directly underneath.
    #[test]
    fn navigates_to_the_pane_actually_in_that_direction() {
        let left = PaneId::alloc();
        let mut l = TileLayout::new(left);
        let top_right = PaneId::alloc();
        l.split_focused(Axis::Col, top_right); // left | top_right
        let bottom_right = PaneId::alloc();
        l.split_focused(Axis::Row, bottom_right); // right side stacked
        let area = Rect::new(0, 0, 80, 24);

        l.focus = top_right;
        l.focus_dir(area, Dir::Down);
        assert_eq!(l.focus, bottom_right, "Down from top-right goes underneath");

        l.focus_dir(area, Dir::Up);
        assert_eq!(l.focus, top_right, "and Up comes back");

        l.focus = bottom_right;
        l.focus_dir(area, Dir::Left);
        assert_eq!(
            l.focus, left,
            "Left from bottom-right reaches the left pane"
        );

        // Moving into a stacked column lands on the pane it faces, and moving
        // off the outer edge does nothing rather than jumping somewhere odd.
        l.focus = left;
        l.focus_dir(area, Dir::Right);
        assert_eq!(
            l.focus, top_right,
            "Right from the left pane enters the column"
        );
        l.focus_dir(area, Dir::Right);
        assert_eq!(l.focus, top_right, "nothing further right, so no movement");
        l.focus = left;
        l.focus_dir(area, Dir::Up);
        assert_eq!(l.focus, left, "nothing above the full-height left pane");
    }

    /// A four-pane grid: two columns, each split in two. Every move must land on
    /// the neighbour that shares an edge, never diagonally across the layout.
    #[test]
    fn grid_navigation_never_moves_diagonally() {
        let tl = PaneId::alloc();
        let mut l = TileLayout::new(tl);
        let tr = PaneId::alloc();
        l.split_focused(Axis::Col, tr); // tl | tr
        let br = PaneId::alloc();
        l.split_focused(Axis::Row, br); // right column: tr / br
        l.focus = tl;
        let bl = PaneId::alloc();
        l.split_focused(Axis::Row, bl); // left column: tl / bl
        let area = Rect::new(0, 0, 80, 24);

        for (from, dir, want, what) in [
            (tl, Dir::Right, tr, "top-left -> top-right"),
            (tl, Dir::Down, bl, "top-left -> bottom-left"),
            (tr, Dir::Left, tl, "top-right -> top-left"),
            (tr, Dir::Down, br, "top-right -> bottom-right"),
            (bl, Dir::Right, br, "bottom-left -> bottom-right"),
            (bl, Dir::Up, tl, "bottom-left -> top-left"),
            (br, Dir::Left, bl, "bottom-right -> bottom-left"),
            (br, Dir::Up, tr, "bottom-right -> top-right"),
        ] {
            l.focus = from;
            l.focus_dir(area, dir);
            assert_eq!(l.focus, want, "{what}");
        }
    }

    /// Three panes stacked in one column beside a full-height pane: stepping
    /// down must visit each in turn instead of skipping or bouncing sideways.
    #[test]
    fn stepping_through_a_tall_column_visits_every_pane() {
        let left = PaneId::alloc();
        let mut l = TileLayout::new(left);
        let r1 = PaneId::alloc();
        l.split_focused(Axis::Col, r1);
        let r2 = PaneId::alloc();
        l.split_focused(Axis::Row, r2);
        let r3 = PaneId::alloc();
        l.split_focused(Axis::Row, r3);
        let area = Rect::new(0, 0, 80, 30);

        // Top to bottom of the right column, one step at a time.
        l.focus = r1;
        let mut seen = vec![l.focus];
        for _ in 0..2 {
            l.focus_dir(area, Dir::Down);
            seen.push(l.focus);
        }
        assert_eq!(seen, vec![r1, r2, r3], "each Down goes one pane down");

        // And back up again.
        for _ in 0..2 {
            l.focus_dir(area, Dir::Up);
        }
        assert_eq!(l.focus, r1, "Up retraces the same path");
    }

    #[test]
    fn dividers_and_set_ratio() {
        let a = PaneId::alloc();
        let mut l = TileLayout::new(a);
        let b = PaneId::alloc();
        l.split_focused(Axis::Col, b); // a | b, 50/50
        let area = Rect::new(0, 0, 80, 24);

        let divs = l.dividers(area);
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].axis, Axis::Col);
        // The divider sits at child A's right edge.
        let a_rect = l.panes(area).into_iter().find(|p| p.id == a).unwrap().rect;
        assert_eq!(divs[0].line, a_rect.right());

        // Widen A and confirm the rects follow.
        let path = divs[0].path.clone();
        let width = |l: &TileLayout, id| {
            l.panes(area)
                .into_iter()
                .find(|p| p.id == id)
                .unwrap()
                .rect
                .width
        };
        l.set_ratio(area, &path, 0.75);
        assert!(width(&l, a) > width(&l, b), "A wider after ratio 0.75");

        // Absurd ratios clamp so neither child drops below MIN_PANE.
        l.set_ratio(area, &path, 9.0);
        assert!(
            width(&l, b) >= MIN_PANE,
            "B keeps >= MIN_PANE, got {}",
            width(&l, b)
        );
        l.set_ratio(area, &path, -9.0);
        assert!(
            width(&l, a) >= MIN_PANE,
            "A keeps >= MIN_PANE, got {}",
            width(&l, a)
        );
    }

    #[test]
    fn keyboard_resize_grows_focus_then_equalizes() {
        let a = PaneId::alloc();
        let mut l = TileLayout::new(a);
        let b = PaneId::alloc();
        l.split_focused(Axis::Col, b); // focus = b (right child)
        let area = Rect::new(0, 0, 80, 24);
        let width = |l: &TileLayout, id| {
            l.panes(area)
                .into_iter()
                .find(|p| p.id == id)
                .unwrap()
                .rect
                .width
        };

        // Focus b (child B): Left grows it (divider moves left).
        let before = width(&l, b);
        assert!(l.resize_focused(area, Dir::Left, 6));
        assert!(width(&l, b) > before, "focused B grew leftward");

        // Equalize restores a near-even split.
        l.equalize();
        let (wa, wb) = (width(&l, a) as i32, width(&l, b) as i32);
        assert!((wa - wb).abs() <= 1, "equalized: {wa} vs {wb}");
    }

    #[test]
    fn resize_ratio_survives_serialization() {
        let a = PaneId::alloc();
        let mut l = TileLayout::new(a);
        let b = PaneId::alloc();
        l.split_focused(Axis::Row, b);
        let area = Rect::new(0, 0, 40, 40);
        let path = l.dividers(area)[0].path.clone();
        l.set_ratio(area, &path, 0.3);
        let want = l.panes(area).into_iter().find(|p| p.id == a).unwrap().rect;

        // Round-trip through the serializable mirror.
        let tree = l.to_tree();
        let remap: HashMap<u32, PaneId> = [(a.0, a), (b.0, b)].into_iter().collect();
        let l2 = TileLayout::from_tree(&tree, &remap, a.0).unwrap();
        let got = l2.panes(area).into_iter().find(|p| p.id == a).unwrap().rect;
        assert_eq!(want, got, "resized ratio persisted");
    }
}
