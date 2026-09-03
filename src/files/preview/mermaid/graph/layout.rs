use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, VecDeque};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{Direction, Flowchart, Node, NodeShape};
use crate::files::preview::mermaid::canvas::Canvas;
use crate::files::preview::mermaid::glyphs::Glyphs;

const MIN_DIAGRAM_WIDTH: usize = 24;
const SIDE_MARGIN: usize = 2;
const NODE_GAP: usize = 3;
const RANK_GAP: usize = 5;
const HORIZONTAL_RANK_GAP: usize = 6;
const MIN_NODE_WIDTH: usize = 9;
const MAX_NODE_WIDTH: usize = 28;
const MAX_LABEL_LINES: usize = 3;
const NODE_ROW_GAP: usize = 2;
const MAX_CANVAS_CELLS: usize = 120_000;
const ROUTE_DIRECTIONS: [(isize, isize); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)];
const START_DIRECTION: usize = 4;
const STATE_DIRECTIONS: usize = 5;

const NORTH: u8 = 1;
const EAST: u8 = 2;
const SOUTH: u8 = 4;
const WEST: u8 = 8;

#[derive(Clone, Debug)]
struct RankedGraph {
    ranks: Vec<Vec<usize>>,
    back_edges: Vec<bool>,
}

#[derive(Clone, Debug)]
struct NodeBox {
    node: usize,
    rank: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    lines: Vec<String>,
}

impl NodeBox {
    fn left(&self) -> usize {
        self.x
    }

    fn right(&self) -> usize {
        self.x + self.width.saturating_sub(1)
    }

    fn top(&self) -> usize {
        self.y
    }

    fn bottom(&self) -> usize {
        self.y + self.height.saturating_sub(1)
    }

    fn center_x(&self) -> usize {
        self.x + self.width / 2
    }

    fn center_y(&self) -> usize {
        self.y + self.height / 2
    }
}

#[derive(Clone, Debug)]
struct EdgeLabel {
    x: usize,
    y: usize,
    text: String,
}

#[derive(Clone, Debug)]
struct Arrow {
    x: usize,
    y: usize,
    glyph: char,
}

struct RouteGrid {
    width: usize,
    height: usize,
    cells: Vec<u8>,
    dotted: Vec<bool>,
}

struct RouteScratch {
    distance: Vec<usize>,
    previous: Vec<usize>,
    stamps: Vec<u32>,
    generation: u32,
    queue: BinaryHeap<Reverse<(usize, usize, usize)>>,
}

impl RouteScratch {
    fn new(state_count: usize) -> Self {
        Self {
            distance: vec![0; state_count],
            previous: vec![0; state_count],
            stamps: vec![0; state_count],
            generation: 0,
            queue: BinaryHeap::new(),
        }
    }

    fn begin_route(&mut self) {
        if self.generation == u32::MAX {
            self.stamps.fill(0);
            self.generation = 1;
        } else {
            self.generation += 1;
        }
        self.queue.clear();
    }

    fn distance(&self, state: usize) -> usize {
        if self.stamps[state] == self.generation {
            self.distance[state]
        } else {
            usize::MAX
        }
    }

    fn previous(&self, state: usize) -> Option<usize> {
        (self.stamps[state] == self.generation).then_some(self.previous[state])
    }

    fn set(&mut self, state: usize, distance: usize, previous: usize) {
        self.stamps[state] = self.generation;
        self.distance[state] = distance;
        self.previous[state] = previous;
    }
}

impl RouteGrid {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![0; width.saturating_mul(height)],
            dotted: vec![false; width.saturating_mul(height)],
        }
    }

    fn index(&self, x: usize, y: usize) -> Option<usize> {
        (x < self.width && y < self.height).then_some(y * self.width + x)
    }

    fn connect(&mut self, from: (usize, usize), to: (usize, usize), dotted: bool) {
        if from.0 == to.0 {
            let x = from.0;
            let (start, end) = if from.1 <= to.1 {
                (from.1, to.1)
            } else {
                (to.1, from.1)
            };
            for y in start..end {
                self.add(x, y, SOUTH, dotted);
                self.add(x, y + 1, NORTH, dotted);
            }
        } else if from.1 == to.1 {
            let y = from.1;
            let (start, end) = if from.0 <= to.0 {
                (from.0, to.0)
            } else {
                (to.0, from.0)
            };
            for x in start..end {
                self.add(x, y, EAST, dotted);
                self.add(x + 1, y, WEST, dotted);
            }
        }
    }

    fn polyline(&mut self, points: &[(usize, usize)], dotted: bool) {
        for pair in points.windows(2) {
            self.connect(pair[0], pair[1], dotted);
        }
    }

    fn add(&mut self, x: usize, y: usize, direction: u8, dotted: bool) {
        let Some(index) = self.index(x, y) else {
            return;
        };
        self.cells[index] |= direction;
        self.dotted[index] |= dotted;
    }

    fn paint(&self, canvas: &mut Canvas, ascii: bool) {
        for y in 0..self.height {
            for x in 0..self.width {
                let index = y * self.width + x;
                let mask = self.cells[index];
                if mask != 0 {
                    canvas.put(x, y, route_glyph(mask, self.dotted[index], ascii));
                }
            }
        }
    }

    fn occupied(&self, x: usize, y: usize) -> bool {
        self.index(x, y).is_some_and(|index| self.cells[index] != 0)
    }
}

#[derive(Clone, Copy)]
enum LayoutAxis {
    Vertical,
    Horizontal,
}

pub(super) fn render(flow: &Flowchart, width: usize, ascii: bool) -> Option<Vec<String>> {
    if width < MIN_DIAGRAM_WIDTH {
        return None;
    }
    let ranked = rank_graph(flow);
    match flow.direction {
        Direction::TopDown => render_vertical(flow, &ranked, width, ascii, false),
        Direction::BottomUp => render_vertical(flow, &ranked, width, ascii, true),
        Direction::LeftRight => render_horizontal(flow, &ranked, width, ascii, false)
            .or_else(|| render_vertical(flow, &ranked, width, ascii, false)),
        Direction::RightLeft => render_horizontal(flow, &ranked, width, ascii, true)
            .or_else(|| render_vertical(flow, &ranked, width, ascii, true)),
    }
}

pub(super) fn supports_spatial_layout(width: usize) -> bool {
    width >= MIN_DIAGRAM_WIDTH
}

fn rank_graph(flow: &Flowchart) -> RankedGraph {
    let back_edges = find_back_edges(flow);
    let mut indegree = vec![0usize; flow.nodes.len()];
    for (edge_index, edge) in flow.edges.iter().enumerate() {
        if !back_edges[edge_index] {
            indegree[edge.to] = indegree[edge.to].saturating_add(1);
        }
    }

    let mut queue = VecDeque::new();
    for (node, degree) in indegree.iter().enumerate() {
        if *degree == 0 {
            queue.push_back(node);
        }
    }
    let mut node_rank = vec![0usize; flow.nodes.len()];
    let mut visited = 0usize;
    while let Some(node) = queue.pop_front() {
        visited += 1;
        for (edge_index, edge) in flow.edges.iter().enumerate() {
            if back_edges[edge_index] || edge.from != node {
                continue;
            }
            node_rank[edge.to] = node_rank[edge.to].max(node_rank[node].saturating_add(1));
            indegree[edge.to] = indegree[edge.to].saturating_sub(1);
            if indegree[edge.to] == 0 {
                queue.push_back(edge.to);
            }
        }
    }

    if visited != flow.nodes.len() {
        // Defensive fallback. Removing DFS back edges should always produce a
        // DAG, but keep malformed internal state bounded and deterministic.
        for (node, rank) in node_rank.iter_mut().enumerate() {
            *rank = node;
        }
    } else {
        // A source used as side input belongs beside the stage it feeds, not
        // necessarily in the first row. This keeps event inputs beside their
        // trigger while true entry points such as User stay at rank zero.
        for node in 0..flow.nodes.len() {
            let has_forward_input = flow
                .edges
                .iter()
                .enumerate()
                .any(|(index, edge)| !back_edges[index] && edge.to == node);
            if has_forward_input {
                continue;
            }
            let nearest_target = flow
                .edges
                .iter()
                .enumerate()
                .filter(|(index, edge)| !back_edges[*index] && edge.from == node)
                .map(|(_, edge)| node_rank[edge.to])
                .min();
            if let Some(target_rank) = nearest_target {
                node_rank[node] = target_rank.saturating_sub(1);
            }
        }
    }

    let rank_count = node_rank.iter().copied().max().unwrap_or(0) + 1;
    let mut ranks = vec![Vec::new(); rank_count];
    for (node, rank) in node_rank.iter().copied().enumerate() {
        ranks[rank].push(node);
    }
    minimize_crossings(flow, &back_edges, &node_rank, &mut ranks);
    RankedGraph { ranks, back_edges }
}

fn find_back_edges(flow: &Flowchart) -> Vec<bool> {
    struct Tarjan<'a> {
        flow: &'a Flowchart,
        next_index: usize,
        indices: Vec<Option<usize>>,
        lowlink: Vec<usize>,
        stack: Vec<usize>,
        on_stack: Vec<bool>,
        component: Vec<usize>,
        component_count: usize,
    }

    impl Tarjan<'_> {
        fn visit(&mut self, node: usize) {
            let index = self.next_index;
            self.next_index += 1;
            self.indices[node] = Some(index);
            self.lowlink[node] = index;
            self.stack.push(node);
            self.on_stack[node] = true;

            for edge in self.flow.edges.iter().filter(|edge| edge.from == node) {
                if self.indices[edge.to].is_none() {
                    self.visit(edge.to);
                    self.lowlink[node] = self.lowlink[node].min(self.lowlink[edge.to]);
                } else if self.on_stack[edge.to] {
                    self.lowlink[node] = self.lowlink[node]
                        .min(self.indices[edge.to].expect("visited node has an index"));
                }
            }

            if self.lowlink[node] == index {
                loop {
                    let member = self.stack.pop().expect("component root is on stack");
                    self.on_stack[member] = false;
                    self.component[member] = self.component_count;
                    if member == node {
                        break;
                    }
                }
                self.component_count += 1;
            }
        }
    }

    let count = flow.nodes.len();
    let mut tarjan = Tarjan {
        flow,
        next_index: 0,
        indices: vec![None; count],
        lowlink: vec![0; count],
        stack: Vec::new(),
        on_stack: vec![false; count],
        component: vec![0; count],
        component_count: 0,
    };
    for node in 0..count {
        if tarjan.indices[node].is_none() {
            tarjan.visit(node);
        }
    }

    flow.edges
        .iter()
        .map(|edge| {
            tarjan.component[edge.from] == tarjan.component[edge.to] && edge.to <= edge.from
        })
        .collect()
}

fn minimize_crossings(
    flow: &Flowchart,
    back_edges: &[bool],
    node_rank: &[usize],
    ranks: &mut [Vec<usize>],
) {
    for _ in 0..4 {
        let positions = rank_positions(ranks, flow.nodes.len());
        for nodes in ranks.iter_mut().skip(1) {
            sort_by_neighbors(nodes, &positions, |node| {
                flow.edges
                    .iter()
                    .enumerate()
                    .filter(|(index, edge)| {
                        !back_edges[*index]
                            && edge.to == node
                            && node_rank[edge.from] < node_rank[node]
                    })
                    .map(|(_, edge)| edge.from)
                    .collect()
            });
        }

        let positions = rank_positions(ranks, flow.nodes.len());
        for nodes in ranks.iter_mut().rev().skip(1) {
            sort_by_neighbors(nodes, &positions, |node| {
                flow.edges
                    .iter()
                    .enumerate()
                    .filter(|(index, edge)| {
                        !back_edges[*index]
                            && edge.from == node
                            && node_rank[edge.to] > node_rank[node]
                    })
                    .map(|(_, edge)| edge.to)
                    .collect()
            });
        }
    }
}

fn rank_positions(ranks: &[Vec<usize>], count: usize) -> Vec<usize> {
    let mut positions = vec![0; count];
    for rank in ranks {
        for (position, node) in rank.iter().copied().enumerate() {
            positions[node] = position;
        }
    }
    positions
}

fn sort_by_neighbors(
    nodes: &mut [usize],
    positions: &[usize],
    neighbors: impl Fn(usize) -> Vec<usize>,
) {
    nodes.sort_by(|left, right| {
        let left_center = barycenter(neighbors(*left), positions);
        let right_center = barycenter(neighbors(*right), positions);
        compare_barycenters(left_center, right_center).then_with(|| left.cmp(right))
    });
}

fn barycenter(neighbors: Vec<usize>, positions: &[usize]) -> Option<(usize, usize)> {
    (!neighbors.is_empty()).then(|| {
        let count = neighbors.len();
        let sum = neighbors.into_iter().map(|node| positions[node]).sum();
        (sum, count)
    })
}

fn compare_barycenters(left: Option<(usize, usize)>, right: Option<(usize, usize)>) -> Ordering {
    match (left, right) {
        (Some((left_sum, left_count)), Some((right_sum, right_count))) => {
            (left_sum * right_count).cmp(&(right_sum * left_count))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn render_vertical(
    flow: &Flowchart,
    ranked: &RankedGraph,
    width: usize,
    ascii: bool,
    reverse: bool,
) -> Option<Vec<String>> {
    let mut boxes = vec![None; flow.nodes.len()];
    let rank_order: Vec<usize> = if reverse {
        (0..ranked.ranks.len()).rev().collect()
    } else {
        (0..ranked.ranks.len()).collect()
    };
    let mut y = 1usize;

    for rank in rank_order {
        let nodes = &ranked.ranks[rank];
        let widths = fit_rank_widths(flow, nodes, width)?;
        let measured: Vec<(Vec<String>, usize)> = nodes
            .iter()
            .zip(widths.iter())
            .map(|(node, width)| measure_node(&flow.nodes[*node], *width, ascii))
            .collect();
        let rank_height = measured
            .iter()
            .map(|(_, height)| *height)
            .max()
            .unwrap_or(3);
        let used =
            widths.iter().sum::<usize>() + NODE_GAP.saturating_mul(nodes.len().saturating_sub(1));
        let mut x = width.saturating_sub(used) / 2;
        for ((node, node_width), (lines, height)) in nodes.iter().zip(widths).zip(measured) {
            boxes[*node] = Some(NodeBox {
                node: *node,
                rank,
                x,
                y,
                width: node_width,
                height,
                lines,
            });
            x += node_width + NODE_GAP;
        }
        y += rank_height + RANK_GAP;
    }

    let height = y.saturating_sub(RANK_GAP).saturating_add(1);
    if width.checked_mul(height)? > MAX_CANVAS_CELLS {
        return None;
    }
    let boxes: Vec<NodeBox> = boxes.into_iter().flatten().collect();
    Some(paint_vertical(flow, ranked, &boxes, width, height, ascii))
}

fn fit_rank_widths(flow: &Flowchart, nodes: &[usize], width: usize) -> Option<Vec<usize>> {
    if nodes.is_empty() {
        return Some(Vec::new());
    }
    let budget = width.saturating_sub(SIDE_MARGIN * 2);
    let gaps = NODE_GAP.saturating_mul(nodes.len().saturating_sub(1));
    let available = budget.saturating_sub(gaps);
    if available / nodes.len() < MIN_NODE_WIDTH {
        return None;
    }
    let natural: Vec<usize> = nodes
        .iter()
        .map(|node| natural_node_width(&flow.nodes[*node]))
        .collect();
    if natural.iter().sum::<usize>() <= available {
        return Some(natural);
    }

    let even = available / nodes.len();
    let mut widths: Vec<usize> = natural.iter().map(|width| (*width).min(even)).collect();
    let mut left = available.saturating_sub(widths.iter().sum());
    while left > 0 {
        let mut changed = false;
        for (width, natural) in widths.iter_mut().zip(natural.iter()) {
            if *width < *natural && left > 0 {
                *width += 1;
                left -= 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Some(widths)
}

fn natural_node_width(node: &Node) -> usize {
    let shape_padding = if matches!(node.shape, NodeShape::Decision) {
        6
    } else {
        4
    };
    node.label
        .width()
        .saturating_add(shape_padding)
        .clamp(MIN_NODE_WIDTH, MAX_NODE_WIDTH)
}

fn measure_node(node: &Node, width: usize, ascii: bool) -> (Vec<String>, usize) {
    let content_width = width.saturating_sub(if matches!(node.shape, NodeShape::Decision) {
        6
    } else {
        4
    });
    let lines = wrap_label(&node.label, content_width.max(1), ascii);
    let height = lines.len()
        + if matches!(node.shape, NodeShape::Decision) {
            4
        } else {
            2
        };
    (lines, height)
}

fn paint_vertical(
    flow: &Flowchart,
    ranked: &RankedGraph,
    boxes: &[NodeBox],
    width: usize,
    height: usize,
    ascii: bool,
) -> Vec<String> {
    paint_spatial(
        flow,
        ranked,
        boxes,
        width,
        height,
        ascii,
        LayoutAxis::Vertical,
    )
}

fn render_horizontal(
    flow: &Flowchart,
    ranked: &RankedGraph,
    width: usize,
    ascii: bool,
    reverse: bool,
) -> Option<Vec<String>> {
    let rank_order: Vec<usize> = if reverse {
        (0..ranked.ranks.len()).rev().collect()
    } else {
        (0..ranked.ranks.len()).collect()
    };
    let rank_widths: Vec<usize> = rank_order
        .iter()
        .map(|rank| {
            ranked.ranks[*rank]
                .iter()
                .map(|node| natural_node_width(&flow.nodes[*node]))
                .max()
                .unwrap_or(MIN_NODE_WIDTH)
        })
        .collect();
    let needed_width = rank_widths.iter().sum::<usize>()
        + HORIZONTAL_RANK_GAP.saturating_mul(rank_widths.len().saturating_sub(1))
        + SIDE_MARGIN * 2;
    if needed_width > width {
        return None;
    }

    let mut columns = Vec::new();
    let mut max_height = 0usize;
    for (rank, rank_width) in rank_order.iter().zip(rank_widths.iter()) {
        let measured: Vec<(usize, Vec<String>, usize)> = ranked.ranks[*rank]
            .iter()
            .map(|node| {
                let (lines, height) = measure_node(&flow.nodes[*node], *rank_width, ascii);
                (*node, lines, height)
            })
            .collect();
        let height = measured.iter().map(|(_, _, height)| *height).sum::<usize>()
            + NODE_ROW_GAP.saturating_mul(measured.len().saturating_sub(1));
        max_height = max_height.max(height);
        columns.push((*rank, *rank_width, measured, height));
    }
    let canvas_height = max_height.saturating_add(SIDE_MARGIN * 2).max(3);
    if width.checked_mul(canvas_height)? > MAX_CANVAS_CELLS {
        return None;
    }
    let mut boxes = vec![None; flow.nodes.len()];
    let mut x = SIDE_MARGIN;
    for (rank, rank_width, measured, column_height) in columns {
        let mut y = canvas_height.saturating_sub(column_height) / 2;
        for (node, lines, height) in measured {
            boxes[node] = Some(NodeBox {
                node,
                rank,
                x,
                y,
                width: rank_width,
                height,
                lines,
            });
            y += height + NODE_ROW_GAP;
        }
        x += rank_width + HORIZONTAL_RANK_GAP;
    }
    let boxes: Vec<NodeBox> = boxes.into_iter().flatten().collect();
    Some(paint_horizontal(
        flow,
        ranked,
        &boxes,
        width,
        canvas_height,
        ascii,
    ))
}

fn paint_horizontal(
    flow: &Flowchart,
    ranked: &RankedGraph,
    boxes: &[NodeBox],
    width: usize,
    height: usize,
    ascii: bool,
) -> Vec<String> {
    paint_spatial(
        flow,
        ranked,
        boxes,
        width,
        height,
        ascii,
        LayoutAxis::Horizontal,
    )
}

#[allow(clippy::too_many_arguments)]
fn paint_spatial(
    flow: &Flowchart,
    ranked: &RankedGraph,
    boxes: &[NodeBox],
    width: usize,
    height: usize,
    ascii: bool,
    axis: LayoutAxis,
) -> Vec<String> {
    let by_node = boxes_by_node(boxes, flow.nodes.len());
    let blocked = node_obstacles(boxes, width, height);
    let glyphs = Glyphs::for_ascii(ascii);
    let mut routes = RouteGrid::new(width, height);
    let mut route_scratch = RouteScratch::new(
        width
            .saturating_mul(height)
            .saturating_mul(STATE_DIRECTIONS),
    );
    let mut labels = Vec::new();
    let mut pending_labels = Vec::new();
    let mut arrows = Vec::new();
    let mut edge_order: Vec<usize> = (0..flow.edges.len()).collect();
    edge_order.sort_by_key(|edge_index| {
        let edge = &flow.edges[*edge_index];
        (
            ranked.back_edges[*edge_index],
            by_node[edge.from].rank.abs_diff(by_node[edge.to].rank),
            *edge_index,
        )
    });

    for edge_index in edge_order {
        let edge = &flow.edges[edge_index];
        let source = by_node[edge.from];
        let target = by_node[edge.to];
        let (start, end, arrow) = edge_ports(source, target, axis, glyphs);
        let Some(path) = shortest_route(
            start,
            end,
            &blocked,
            &routes,
            width,
            height,
            &mut route_scratch,
        ) else {
            continue;
        };
        routes.polyline(&path, edge.dotted);
        if edge.directed {
            arrows.push(Arrow {
                x: end.0,
                y: end.1,
                glyph: arrow,
            });
        }
        if let Some(label) = edge.label.as_deref() {
            pending_labels.push((end, label));
        }
    }
    for (end, label) in pending_labels {
        if let Some(label) =
            place_route_label(end, label, &blocked, &labels, &arrows, width, height, ascii)
        {
            labels.push(label);
        }
    }

    let mut canvas = Canvas::new(width, height);
    routes.paint(&mut canvas, ascii);
    paint_labels(&mut canvas, &labels, width);
    for arrow in arrows {
        canvas.put(arrow.x, arrow.y, arrow.glyph);
    }
    for node_box in boxes {
        draw_node(&mut canvas, node_box, &flow.nodes[node_box.node], ascii);
    }
    canvas.into_lines()
}

fn node_obstacles(boxes: &[NodeBox], width: usize, height: usize) -> Vec<bool> {
    let mut blocked = vec![false; width.saturating_mul(height)];
    for node in boxes {
        for y in node.top()..=node.bottom().min(height.saturating_sub(1)) {
            for x in node.left()..=node.right().min(width.saturating_sub(1)) {
                blocked[y * width + x] = true;
            }
        }
    }
    blocked
}

fn edge_ports(
    source: &NodeBox,
    target: &NodeBox,
    axis: LayoutAxis,
    glyphs: Glyphs,
) -> ((usize, usize), (usize, usize), char) {
    match axis {
        LayoutAxis::Vertical if source.rank != target.rank => {
            if target.center_y() > source.center_y() {
                (
                    (source.center_x(), source.bottom().saturating_add(1)),
                    (target.center_x(), target.top().saturating_sub(1)),
                    glyphs.arrow_down,
                )
            } else {
                (
                    (source.center_x(), source.top().saturating_sub(1)),
                    (target.center_x(), target.bottom().saturating_add(1)),
                    glyphs.arrow_up,
                )
            }
        }
        LayoutAxis::Horizontal if source.rank != target.rank => {
            if target.center_x() > source.center_x() {
                (
                    (source.right().saturating_add(1), source.center_y()),
                    (target.left().saturating_sub(1), target.center_y()),
                    glyphs.arrow_right,
                )
            } else {
                (
                    (source.left().saturating_sub(1), source.center_y()),
                    (target.right().saturating_add(1), target.center_y()),
                    glyphs.arrow_left,
                )
            }
        }
        _ => {
            if target.center_x() > source.center_x() {
                (
                    (source.right().saturating_add(1), source.center_y()),
                    (target.left().saturating_sub(1), target.center_y()),
                    glyphs.arrow_right,
                )
            } else {
                (
                    (source.left().saturating_sub(1), source.center_y()),
                    (target.right().saturating_add(1), target.center_y()),
                    glyphs.arrow_left,
                )
            }
        }
    }
}

fn shortest_route(
    start: (usize, usize),
    end: (usize, usize),
    blocked: &[bool],
    routes: &RouteGrid,
    width: usize,
    height: usize,
    scratch: &mut RouteScratch,
) -> Option<Vec<(usize, usize)>> {
    if start.0 >= width || start.1 >= height || end.0 >= width || end.1 >= height {
        return None;
    }
    scratch.begin_route();
    let start_state = route_state(start.0, start.1, START_DIRECTION, width);
    scratch.set(start_state, 0, start_state);
    scratch
        .queue
        .push(Reverse((manhattan(start, end) * 10, 0usize, start_state)));
    let mut end_state = None;

    while let Some(Reverse((_, cost, state))) = scratch.queue.pop() {
        if cost != scratch.distance(state) {
            continue;
        }
        let (x, y, direction) = route_state_parts(state, width);
        if (x, y) == end {
            end_state = Some(state);
            break;
        }
        for (next_direction, (dx, dy)) in ROUTE_DIRECTIONS.iter().copied().enumerate() {
            let Some(next_x) = x.checked_add_signed(dx) else {
                continue;
            };
            let Some(next_y) = y.checked_add_signed(dy) else {
                continue;
            };
            if next_x >= width || next_y >= height {
                continue;
            }
            let cell = next_y * width + next_x;
            if blocked[cell] && (next_x, next_y) != end {
                continue;
            }
            let turn_cost =
                usize::from(direction != START_DIRECTION && direction != next_direction) * 4;
            let occupied_cost = usize::from(routes.occupied(next_x, next_y)) * 7;
            let border_cost = usize::from(
                next_x == 0 || next_y == 0 || next_x + 1 == width || next_y + 1 == height,
            ) * 2;
            let next_cost = cost + 10 + turn_cost + occupied_cost + border_cost;
            let next_state = route_state(next_x, next_y, next_direction, width);
            if next_cost < scratch.distance(next_state) {
                scratch.set(next_state, next_cost, state);
                let estimate = next_cost + manhattan((next_x, next_y), end) * 10;
                scratch
                    .queue
                    .push(Reverse((estimate, next_cost, next_state)));
            }
        }
    }

    let mut state = end_state?;
    let mut path = Vec::new();
    loop {
        let (x, y, _) = route_state_parts(state, width);
        path.push((x, y));
        if state == start_state {
            break;
        }
        state = scratch.previous(state)?;
    }
    path.reverse();
    Some(path)
}

fn route_state(x: usize, y: usize, direction: usize, width: usize) -> usize {
    (y * width + x) * STATE_DIRECTIONS + direction
}

fn route_state_parts(state: usize, width: usize) -> (usize, usize, usize) {
    let direction = state % STATE_DIRECTIONS;
    let cell = state / STATE_DIRECTIONS;
    (cell % width, cell / width, direction)
}

fn manhattan(left: (usize, usize), right: (usize, usize)) -> usize {
    left.0.abs_diff(right.0) + left.1.abs_diff(right.1)
}

fn place_route_label(
    end: (usize, usize),
    label: &str,
    blocked: &[bool],
    existing: &[EdgeLabel],
    arrows: &[Arrow],
    width: usize,
    height: usize,
    ascii: bool,
) -> Option<EdgeLabel> {
    let text = clip_text(label, width.saturating_sub(2), ascii);
    let text_width = text.width();
    let right = end.0.saturating_add(2);
    let left = end.0.saturating_sub(text_width.saturating_add(2));
    let centered = end.0.saturating_sub(text_width / 2);
    let candidates = [
        (right, end.1.saturating_sub(1)),
        (left, end.1.saturating_sub(1)),
        (right, end.1.saturating_add(1)),
        (left, end.1.saturating_add(1)),
        (centered, end.1.saturating_sub(1)),
        (centered, end.1.saturating_add(1)),
    ];
    if let Some(label) = candidates.into_iter().find_map(|(x, y)| {
        label_slot_clear(
            (x, y),
            text_width,
            blocked,
            existing,
            arrows,
            (width, height),
        )
        .then(|| EdgeLabel {
            x,
            y,
            text: text.clone(),
        })
    }) {
        return Some(label);
    }

    // Dense diagrams may occupy every slot immediately beside the arrow.
    // Search outward deterministically so an edge label is never silently
    // dropped merely because its preferred route is crowded.
    let mut rows: Vec<usize> = (0..height).collect();
    rows.sort_by_key(|row| row.abs_diff(end.1));
    for y in rows {
        let mut columns: Vec<usize> = (0..=width.saturating_sub(text_width)).collect();
        columns.sort_by_key(|column| column.abs_diff(end.0));
        for x in columns {
            if label_slot_clear(
                (x, y),
                text_width,
                blocked,
                existing,
                arrows,
                (width, height),
            ) {
                return Some(EdgeLabel {
                    x,
                    y,
                    text: text.clone(),
                });
            }
        }
    }
    None
}

fn label_slot_clear(
    position: (usize, usize),
    label_width: usize,
    blocked: &[bool],
    existing: &[EdgeLabel],
    arrows: &[Arrow],
    size: (usize, usize),
) -> bool {
    let (x, y) = position;
    let (width, height) = size;
    if y >= height || x.saturating_add(label_width) > width {
        return false;
    }
    if (x..x + label_width).any(|column| blocked[y * width + column]) {
        return false;
    }
    if arrows
        .iter()
        .any(|arrow| arrow.y == y && (x..x + label_width).contains(&arrow.x))
    {
        return false;
    }
    !existing.iter().any(|label| {
        label.y == y
            && x < label.x.saturating_add(label.text.width())
            && label.x < x.saturating_add(label_width)
    })
}

fn boxes_by_node(boxes: &[NodeBox], count: usize) -> Vec<&NodeBox> {
    let mut by_node = vec![None; count];
    for node_box in boxes {
        by_node[node_box.node] = Some(node_box);
    }
    by_node
        .into_iter()
        .map(|node| node.expect("every ranked node has geometry"))
        .collect()
}

fn paint_labels(canvas: &mut Canvas, labels: &[EdgeLabel], width: usize) {
    for label in labels {
        if label.y < canvas.height() {
            canvas.write(label.x.min(width.saturating_sub(1)), label.y, &label.text);
        }
    }
}

fn draw_node(canvas: &mut Canvas, geometry: &NodeBox, node: &Node, ascii: bool) {
    match node.shape {
        NodeShape::Decision => draw_decision(canvas, geometry, ascii),
        NodeShape::Rectangle | NodeShape::Rounded | NodeShape::Terminal => {
            draw_rectangle(canvas, geometry, node.shape, ascii)
        }
    }
}

fn draw_rectangle(canvas: &mut Canvas, geometry: &NodeBox, shape: NodeShape, ascii: bool) {
    let glyphs = Glyphs::for_ascii(ascii);
    let rounded = !ascii && matches!(shape, NodeShape::Rounded | NodeShape::Terminal);
    let (top_left, top_right, bottom_left, bottom_right) = if rounded {
        ('╭', '╮', '╰', '╯')
    } else {
        (
            glyphs.top_left,
            glyphs.top_right,
            glyphs.bottom_left,
            glyphs.bottom_right,
        )
    };
    canvas.put(geometry.left(), geometry.top(), top_left);
    canvas.hline(
        geometry.left() + 1,
        geometry.right().saturating_sub(1),
        geometry.top(),
        glyphs.horizontal,
    );
    canvas.put(geometry.right(), geometry.top(), top_right);
    canvas.put(geometry.left(), geometry.bottom(), bottom_left);
    canvas.hline(
        geometry.left() + 1,
        geometry.right().saturating_sub(1),
        geometry.bottom(),
        glyphs.horizontal,
    );
    canvas.put(geometry.right(), geometry.bottom(), bottom_right);
    canvas.vline(
        geometry.left(),
        geometry.top() + 1,
        geometry.bottom().saturating_sub(1),
        glyphs.vertical,
    );
    canvas.vline(
        geometry.right(),
        geometry.top() + 1,
        geometry.bottom().saturating_sub(1),
        glyphs.vertical,
    );
    draw_centered_lines(canvas, geometry, 1);
}

fn draw_decision(canvas: &mut Canvas, geometry: &NodeBox, ascii: bool) {
    let (upper_left, upper_right, lower_left, lower_right, horizontal, vertical) = if ascii {
        ('/', '\\', '\\', '/', '-', '|')
    } else {
        ('╱', '╲', '╲', '╱', '─', '│')
    };
    let left = geometry.left();
    let right = geometry.right();
    let top = geometry.top();
    let bottom = geometry.bottom();
    canvas.put(left + 1, top, upper_left);
    canvas.hline(left + 2, right.saturating_sub(2), top, horizontal);
    canvas.put(right.saturating_sub(1), top, upper_right);
    canvas.put(left, top + 1, upper_left);
    canvas.put(right, top + 1, upper_right);
    if bottom > top + 3 {
        canvas.vline(left, top + 2, bottom.saturating_sub(2), vertical);
        canvas.vline(right, top + 2, bottom.saturating_sub(2), vertical);
    }
    canvas.put(left, bottom.saturating_sub(1), lower_left);
    canvas.put(right, bottom.saturating_sub(1), lower_right);
    canvas.put(left + 1, bottom, lower_left);
    canvas.hline(left + 2, right.saturating_sub(2), bottom, horizontal);
    canvas.put(right.saturating_sub(1), bottom, lower_right);
    draw_centered_lines(canvas, geometry, 2);
}

fn draw_centered_lines(canvas: &mut Canvas, geometry: &NodeBox, top_padding: usize) {
    for (line_index, line) in geometry.lines.iter().enumerate() {
        let x = geometry
            .left()
            .saturating_add(geometry.width.saturating_sub(line.width()) / 2);
        canvas.write(x, geometry.top() + top_padding + line_index, line);
    }
}

fn wrap_label(text: &str, width: usize, ascii: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if word.width() > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let mut rest = word;
            while !rest.is_empty() && lines.len() + 1 < MAX_LABEL_LINES {
                let (part, tail) = split_at_width(rest, width);
                lines.push(part.to_string());
                rest = tail;
            }
            if !rest.is_empty() {
                current = clip_with_ellipsis(rest, width, ascii);
            }
            continue;
        }
        let candidate_width = if current.is_empty() {
            word.width()
        } else {
            current.width() + 1 + word.width()
        };
        if candidate_width <= width {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
        if lines.len() == MAX_LABEL_LINES {
            break;
        }
    }
    if lines.len() < MAX_LABEL_LINES && !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    if lines.len() > MAX_LABEL_LINES {
        lines.truncate(MAX_LABEL_LINES);
    }
    if text.width() > lines.iter().map(|line| line.width()).sum::<usize>() + lines.len() {
        if let Some(last) = lines.last_mut() {
            *last = clip_with_ellipsis(last, width, ascii);
        }
    }
    lines
}

fn split_at_width(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0usize;
    let mut split = 0usize;
    for (index, ch) in text.char_indices() {
        let next = used + ch.width().unwrap_or(0);
        if next > width {
            break;
        }
        used = next;
        split = index + ch.len_utf8();
    }
    if split == 0 {
        let ch = text.chars().next().expect("non-empty text");
        split = ch.len_utf8();
    }
    text.split_at(split)
}

fn clip_with_ellipsis(text: &str, width: usize, ascii: bool) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let marker = if ascii {
        ".".repeat(width.min(3))
    } else if width > 1 {
        "…".to_string()
    } else {
        String::new()
    };
    let body_width = width.saturating_sub(marker.width());
    if body_width == 0 {
        return marker;
    }
    let (body, _) = split_at_width(text, body_width);
    format!("{body}{marker}")
}

fn clip_text(text: &str, width: usize, ascii: bool) -> String {
    if text.width() <= width {
        text.to_string()
    } else {
        clip_with_ellipsis(text, width, ascii)
    }
}

fn route_glyph(mask: u8, dotted: bool, ascii: bool) -> char {
    if ascii {
        return match mask {
            1 | 4 => {
                if dotted {
                    ':'
                } else {
                    '|'
                }
            }
            2 | 8 => {
                if dotted {
                    '.'
                } else {
                    '-'
                }
            }
            10 => {
                if dotted {
                    '.'
                } else {
                    '-'
                }
            }
            5 => {
                if dotted {
                    ':'
                } else {
                    '|'
                }
            }
            15 => '+',
            _ if mask.count_ones() > 1 => '+',
            _ => '-',
        };
    }
    match mask {
        1 | 4 => {
            if dotted {
                '┆'
            } else {
                '│'
            }
        }
        2 | 8 => {
            if dotted {
                '┄'
            } else {
                '─'
            }
        }
        10 => {
            if dotted {
                '┄'
            } else {
                '─'
            }
        }
        5 => {
            if dotted {
                '┆'
            } else {
                '│'
            }
        }
        6 => '┌',
        12 => '┐',
        3 => '└',
        9 => '┘',
        7 => '├',
        13 => '┤',
        14 => '┬',
        11 => '┴',
        15 => '┼',
        _ => '─',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::preview::mermaid::graph;

    #[test]
    fn branching_flowchart_keeps_spatial_nodes_and_routes() {
        let flow = graph::parse(
            "flowchart TB\n User([User]) --> Choice{Ready?}\n Choice -- yes --> Run[Run agent]\n Choice -- no --> Wait[Wait]\n Run --> Done([Done])\n Wait --> Choice",
        )
        .unwrap();
        let output = render(&flow, 80, false).unwrap();
        let text = output.join("\n");

        assert!(text.contains("╭"));
        assert!(text.contains("Ready?"));
        assert!(text.contains("yes"));
        assert!(text.contains("no"));
        assert!(text.contains('▼'));
        assert!(output.iter().all(|line| line.width() <= 80));
        assert!(!text.contains("◆ User · User"));
    }

    #[test]
    fn horizontal_branching_flowchart_uses_columns_when_it_fits() {
        let flow =
            graph::parse("flowchart LR\n A[Start] --> B{Choice}\n B --> C[One]\n B --> D[Two]")
                .unwrap();
        let output = render(&flow, 100, false).unwrap();
        let start_row = output
            .iter()
            .position(|line| line.contains("Start"))
            .unwrap();
        let choice_row = output
            .iter()
            .position(|line| line.contains("Choice"))
            .unwrap();

        assert!(start_row.abs_diff(choice_row) < 8);
        assert!(output.join("\n").contains('▶'));
    }

    #[test]
    fn reverse_directions_put_arrows_toward_the_requested_flow() {
        let bottom_up =
            graph::parse("flowchart BT\n A[Start] --> B{Choice}\n B --> C[Done]").unwrap();
        let right_left =
            graph::parse("flowchart RL\n A[Start] --> B{Choice}\n B --> C[Done]").unwrap();

        assert!(render(&bottom_up, 80, false)
            .unwrap()
            .join("\n")
            .contains('▲'));
        assert!(render(&right_left, 100, false)
            .unwrap()
            .join("\n")
            .contains('◀'));
    }

    #[test]
    fn ascii_mode_keeps_the_spatial_layout_without_unicode_glyphs() {
        let flow = graph::parse(
            "flowchart TB\n A([Start]) --> B{Ready?}\n B -- yes --> C[ABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZAB]\n B -- no --> D[Wait]",
        )
        .unwrap();
        let output = render(&flow, 80, true).unwrap().join("\n");

        assert!(output.contains("Ready?"));
        assert!(output.contains("..."));
        assert!(output.contains('+'));
        assert!(output.contains('v'));
        assert!(output.is_ascii());
    }

    #[test]
    fn dense_rank_falls_back_instead_of_exceeding_the_pane() {
        let flow = graph::parse(
            "flowchart TB\n A --> B\n A --> C\n A --> D\n A --> E\n A --> F\n A --> G",
        )
        .unwrap();

        assert!(render(&flow, 48, false).is_none());
    }

    #[test]
    fn oversized_spatial_canvas_uses_the_bounded_chain_fallback() {
        let mut source = String::from("flowchart TB\n");
        for node in 0..100 {
            source.push_str(&format!(" N{node} --> N{}\n", node + 1));
        }
        let flow = graph::parse(&source).unwrap();

        assert!(render(&flow, 200, false).is_none());
        let fallback = graph::render(&flow, 200, false);
        let text = fallback.join("\n");
        assert!(text.contains("N0"));
        assert!(text.contains("N100"));
        assert!(text.contains('▼'));
        assert!(!fallback[0].starts_with('◆'));
        assert!(fallback.iter().all(|line| line.width() <= 200));
    }

    #[test]
    fn narrow_diagrams_decline_to_the_existing_outline_fallback() {
        let flow = graph::parse("flowchart TB\n A --> B\n A --> C").unwrap();
        assert!(render(&flow, 20, false).is_none());
    }

    #[test]
    fn rank_assignment_breaks_cycles_without_losing_edges() {
        let flow = graph::parse("flowchart TB\n A --> B\n B --> C\n C --> A").unwrap();
        let ranked = rank_graph(&flow);

        assert_eq!(ranked.back_edges.iter().filter(|edge| **edge).count(), 1);
        assert_eq!(ranked.ranks.iter().map(Vec::len).sum::<usize>(), 3);
    }
}
