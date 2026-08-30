use std::collections::{HashMap, HashSet};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::canvas::Canvas;
use super::glyphs::Glyphs;
use super::routing;

const MAX_NODES: usize = 128;
const MAX_EDGES: usize = 256;
const MAX_LABEL: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    TopDown,
    BottomUp,
    LeftRight,
    RightLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeShape {
    Rectangle,
    Rounded,
    Decision,
    Terminal,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub shape: NodeShape,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub label: Option<String>,
    pub dotted: bool,
    pub directed: bool,
}

#[derive(Clone, Debug)]
pub struct Flowchart {
    pub direction: Direction,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

pub fn parse(source: &str) -> Result<Flowchart, String> {
    let mut statements = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("%%"));
    let header = statements.next().ok_or("empty Mermaid source")?;
    let mut words = header.split_whitespace();
    let kind = words.next().unwrap_or_default();
    if !kind.eq_ignore_ascii_case("flowchart") && !kind.eq_ignore_ascii_case("graph") {
        return Err("expected flowchart or graph".into());
    }
    let direction = match words.next().unwrap_or("TD").to_ascii_uppercase().as_str() {
        "TD" | "TB" => Direction::TopDown,
        "BT" => Direction::BottomUp,
        "LR" => Direction::LeftRight,
        "RL" => Direction::RightLeft,
        other => return Err(format!("unsupported flowchart direction {other}")),
    };
    let mut nodes = Vec::<Node>::new();
    let mut ids = HashMap::<String, usize>::new();
    let mut pending = Vec::<(String, String, Option<String>, bool, bool)>::new();
    for line in statements {
        for statement in line.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            if statement.starts_with("subgraph") || statement == "end" {
                return Err("subgraphs are not supported yet".into());
            }
            if let Some((left, right, label, dotted, directed)) = split_edge(statement) {
                let from = ensure_node(left, &mut nodes, &mut ids)?;
                let to = ensure_node(right, &mut nodes, &mut ids)?;
                pending.push((from, to, label, dotted, directed));
                if pending.len() > MAX_EDGES {
                    return Err(format!("diagram exceeds {MAX_EDGES} edges"));
                }
            } else {
                ensure_node(statement, &mut nodes, &mut ids)?;
            }
        }
    }
    let edges = pending
        .into_iter()
        .map(|(from, to, label, dotted, directed)| Edge {
            from: ids[&from],
            to: ids[&to],
            label,
            dotted,
            directed,
        })
        .collect();
    Ok(Flowchart {
        direction,
        nodes,
        edges,
    })
}

fn split_edge(statement: &str) -> Option<(&str, &str, Option<String>, bool, bool)> {
    // Mermaid accepts labels both between the edge strokes (`A -- label --> B`)
    // and after the operator (`A -->|label| B`). Parse the former before the
    // plain operator search so the label cannot become part of the source ID.
    for (start, end, dotted, directed) in [
        (" -- ", " -->", false, true),
        (" -. ", " .->", true, true),
        (" -- ", " ---", false, false),
    ] {
        let Some((left, tail)) = statement.split_once(start) else {
            continue;
        };
        let Some((label, right)) = tail.split_once(end) else {
            continue;
        };
        if !left.trim().is_empty() && !right.trim().is_empty() && !label.trim().is_empty() {
            return Some((
                left.trim(),
                right.trim(),
                Some(label.trim().chars().take(MAX_LABEL).collect()),
                dotted,
                directed,
            ));
        }
    }
    for (operator, dotted, directed) in [
        ("-.->", true, true),
        ("-->", false, true),
        ("==>", false, true),
        ("---", false, false),
    ] {
        if let Some(index) = statement.find(operator) {
            let left = statement[..index].trim();
            let (right, label) = peel_edge_label(statement[index + operator.len()..].trim());
            return Some((left, right, label, dotted, directed));
        }
    }
    None
}

fn peel_edge_label(right: &str) -> (&str, Option<String>) {
    let Some(labelled) = right.strip_prefix('|') else {
        return (right, None);
    };
    let Some(end) = labelled.find('|') else {
        return (right, None);
    };
    let label = labelled[..end].trim();
    let target = labelled[end + 1..].trim();
    (
        target,
        (!label.is_empty()).then(|| label.chars().take(MAX_LABEL).collect()),
    )
}

fn ensure_node(
    token: &str,
    nodes: &mut Vec<Node>,
    ids: &mut HashMap<String, usize>,
) -> Result<String, String> {
    let (id, label, shape) = parse_node(token)?;
    if let Some(index) = ids.get(&id).copied() {
        if label != id {
            nodes[index].label = label;
            nodes[index].shape = shape;
        }
        return Ok(id);
    }
    if nodes.len() >= MAX_NODES {
        return Err(format!("diagram exceeds {MAX_NODES} nodes"));
    }
    ids.insert(id.clone(), nodes.len());
    nodes.push(Node {
        id: id.clone(),
        label,
        shape,
    });
    Ok(id)
}

fn parse_node(token: &str) -> Result<(String, String, NodeShape), String> {
    let token = token.trim();
    let open = token.find(['[', '(', '{']);
    let id = token[..open.unwrap_or(token.len())].trim();
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Err(format!("invalid node identifier {id:?}"));
    }
    let Some(open) = open else {
        return Ok((id.into(), id.into(), NodeShape::Rectangle));
    };
    let rest = &token[open..];
    let (shape, prefix, suffix) = if rest.starts_with("([") && rest.ends_with("])") {
        (NodeShape::Terminal, 2, 2)
    } else if rest.starts_with('[') && rest.ends_with(']') {
        (NodeShape::Rectangle, 1, 1)
    } else if rest.starts_with('(') && rest.ends_with(')') {
        (NodeShape::Rounded, 1, 1)
    } else if rest.starts_with('{') && rest.ends_with('}') {
        (NodeShape::Decision, 1, 1)
    } else {
        return Err(format!("unsupported node shape for {id}"));
    };
    let raw = rest[prefix..rest.len() - suffix].trim();
    let label = raw
        .trim_matches(|ch| ch == '"' || ch == '\'')
        .chars()
        .take(MAX_LABEL)
        .collect::<String>();
    Ok((
        id.into(),
        if label.is_empty() { id.into() } else { label },
        shape,
    ))
}

pub fn render(flow: &Flowchart, width: usize, ascii: bool) -> Vec<String> {
    if flow.nodes.is_empty() {
        return vec!["empty flowchart".into()];
    }
    let mut order: Vec<usize> = (0..flow.nodes.len()).collect();
    if matches!(flow.direction, Direction::RightLeft | Direction::BottomUp) {
        order.reverse();
    }
    let chain = is_chain(flow);
    if chain {
        let ordered = chain_order(flow).unwrap_or(order);
        if matches!(flow.direction, Direction::LeftRight | Direction::RightLeft)
            && horizontal_chain_width(flow, &ordered, ascii) <= width
        {
            return render_horizontal_chain(flow, &ordered, width, ascii);
        }
        return render_vertical_chain(flow, &ordered, width, ascii);
    }
    render_structured(flow, &order, width, ascii)
}

fn is_chain(flow: &Flowchart) -> bool {
    flow.edges.len() == flow.nodes.len().saturating_sub(1)
        && (0..flow.nodes.len()).all(|index| {
            flow.edges.iter().filter(|edge| edge.from == index).count() <= 1
                && flow.edges.iter().filter(|edge| edge.to == index).count() <= 1
        })
}

fn chain_order(flow: &Flowchart) -> Option<Vec<usize>> {
    let start = (0..flow.nodes.len()).find(|index| !flow.edges.iter().any(|e| e.to == *index))?;
    let mut order = Vec::with_capacity(flow.nodes.len());
    let mut seen = HashSet::new();
    let mut current = start;
    loop {
        if !seen.insert(current) {
            return None;
        }
        order.push(current);
        let Some(edge) = flow.edges.iter().find(|edge| edge.from == current) else {
            break;
        };
        current = edge.to;
    }
    (order.len() == flow.nodes.len()).then_some(order)
}

fn display_label(node: &Node, ascii: bool) -> String {
    match node.shape {
        NodeShape::Rectangle => node.label.clone(),
        NodeShape::Rounded => format!("({})", node.label),
        NodeShape::Decision if ascii => format!("? {}", node.label),
        NodeShape::Decision => format!("◇ {}", node.label),
        NodeShape::Terminal if ascii => format!("> {}", node.label),
        NodeShape::Terminal => format!("› {}", node.label),
    }
}

fn box_width(node: &Node, ascii: bool) -> usize {
    display_label(node, ascii).width().clamp(3, 20) + 4
}

fn chain_edge(flow: &Flowchart, from: usize, to: usize) -> Option<&Edge> {
    flow.edges
        .iter()
        .find(|edge| edge.from == from && edge.to == to)
}

fn edge_gap(edge: Option<&Edge>) -> usize {
    edge.and_then(|edge| edge.label.as_deref())
        .map_or(5, |label| label.width().saturating_add(2).max(5))
}

fn horizontal_chain_width(flow: &Flowchart, order: &[usize], ascii: bool) -> usize {
    let nodes = order
        .iter()
        .map(|index| box_width(&flow.nodes[*index], ascii))
        .sum::<usize>();
    let gaps = order
        .windows(2)
        .map(|pair| edge_gap(chain_edge(flow, pair[0], pair[1])))
        .sum::<usize>();
    nodes + gaps
}

fn draw_box(
    canvas: &mut Canvas,
    x: usize,
    y: usize,
    width: usize,
    node: &Node,
    glyphs: Glyphs,
    ascii: bool,
) {
    let (top_left, top_right, bottom_left, bottom_right) =
        if !ascii && matches!(node.shape, NodeShape::Rounded | NodeShape::Terminal) {
            ('╭', '╮', '╰', '╯')
        } else {
            (
                glyphs.top_left,
                glyphs.top_right,
                glyphs.bottom_left,
                glyphs.bottom_right,
            )
        };
    canvas.put(x, y, top_left);
    canvas.hline(x + 1, x + width.saturating_sub(2), y, glyphs.horizontal);
    canvas.put(x + width.saturating_sub(1), y, top_right);
    canvas.put(x, y + 1, glyphs.vertical);
    canvas.put(x + width.saturating_sub(1), y + 1, glyphs.vertical);
    canvas.put(x, y + 2, bottom_left);
    canvas.hline(x + 1, x + width.saturating_sub(2), y + 2, glyphs.horizontal);
    canvas.put(x + width.saturating_sub(1), y + 2, bottom_right);
    let label = clip(&display_label(node, ascii), width.saturating_sub(4));
    let offset = width.saturating_sub(label.width()) / 2;
    canvas.write(x + offset, y + 1, &label);
}

fn render_horizontal_chain(
    flow: &Flowchart,
    order: &[usize],
    width: usize,
    ascii: bool,
) -> Vec<String> {
    let glyphs = Glyphs::for_ascii(ascii);
    let needed = horizontal_chain_width(flow, order, ascii).min(width);
    let mut canvas = Canvas::new(needed.max(1), 4);
    let mut x = 0;
    for (position, index) in order.iter().enumerate() {
        let node_width = box_width(&flow.nodes[*index], ascii);
        draw_box(
            &mut canvas,
            x,
            1,
            node_width,
            &flow.nodes[*index],
            glyphs,
            ascii,
        );
        if position + 1 < order.len() {
            let edge = chain_edge(flow, *index, order[position + 1]);
            let gap = edge_gap(edge);
            let start = x + node_width;
            let end = start + gap.saturating_sub(2);
            if edge.is_some_and(|edge| edge.directed) {
                routing::horizontal_arrow(
                    &mut canvas,
                    start,
                    end,
                    2,
                    edge.is_some_and(|edge| edge.dotted),
                    glyphs,
                );
            } else {
                canvas.hline(start, end, 2, glyphs.horizontal);
            }
            if let Some(label) = edge.and_then(|edge| edge.label.as_deref()) {
                let label_x = start + gap.saturating_sub(label.width()) / 2;
                canvas.write(label_x, 0, label);
            }
            x += gap;
        }
        x += node_width;
    }
    canvas.into_lines()
}

fn render_vertical_chain(
    flow: &Flowchart,
    order: &[usize],
    width: usize,
    ascii: bool,
) -> Vec<String> {
    let glyphs = Glyphs::for_ascii(ascii);
    let widest = order
        .iter()
        .map(|index| box_width(&flow.nodes[*index], ascii))
        .max()
        .unwrap_or(3)
        .min(width.max(3));
    let height = order.len() * 5 - 2;
    let mut canvas = Canvas::new(width.max(widest).max(1), height.max(1));
    for (position, index) in order.iter().enumerate() {
        let node_width = box_width(&flow.nodes[*index], ascii).min(width.max(3));
        let x = width.saturating_sub(node_width) / 2;
        let y = position * 5;
        draw_box(
            &mut canvas,
            x,
            y,
            node_width,
            &flow.nodes[*index],
            glyphs,
            ascii,
        );
        if position + 1 < order.len() {
            let edge = chain_edge(flow, *index, order[position + 1]);
            if edge.is_some_and(|edge| edge.directed) {
                routing::vertical_arrow(
                    &mut canvas,
                    width / 2,
                    y + 3,
                    y + 4,
                    edge.is_some_and(|edge| edge.dotted),
                    glyphs,
                );
            } else {
                canvas.vline(width / 2, y + 3, y + 4, glyphs.vertical);
            }
            if let Some(label) = edge.and_then(|edge| edge.label.as_deref()) {
                canvas.write(width / 2 + 2, y + 3, label);
            }
        }
    }
    canvas.into_lines()
}

fn render_structured(flow: &Flowchart, order: &[usize], width: usize, ascii: bool) -> Vec<String> {
    let branch = if ascii { "+->" } else { "└─▶" };
    let node_mark = if ascii { "*" } else { "◆" };
    let mut lines = Vec::new();
    for index in order {
        let node = &flow.nodes[*index];
        lines.push(clip(
            &format!("{node_mark} {} · {}", node.id, node.label),
            width,
        ));
        for edge in flow.edges.iter().filter(|edge| edge.from == *index) {
            let arrow = if edge.directed {
                branch
            } else if ascii {
                "+--"
            } else {
                "└──"
            };
            let dotted = if edge.dotted { " dotted" } else { "" };
            let label = edge
                .label
                .as_deref()
                .map(|label| format!(" · {label}"))
                .unwrap_or_default();
            lines.push(clip(
                &format!("  {arrow} {}{label}{dotted}", flow.nodes[edge.to].id),
                width,
            ));
        }
    }
    lines
}

fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|ch| {
            let next = used + ch.width().unwrap_or(0);
            if next > width {
                false
            } else {
                used = next;
                true
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_a_chain() {
        let graph = parse("flowchart LR\n A[Files] --> B(Markdown)\n B --> C{Preview}").unwrap();
        assert_eq!(graph.nodes.len(), 3);
        let rendered = render(&graph, 80, false).join("\n");
        assert!(rendered.contains("Files"));
        assert!(rendered.contains('▶'));
    }

    #[test]
    fn cycles_are_bounded_and_deterministic() {
        let graph = parse("graph TD\n A-->B\n B-->A").unwrap();
        let first = render(&graph, 30, false);
        assert_eq!(first, render(&graph, 30, false));
        assert!(first.len() < 10);
    }

    #[test]
    fn parses_edge_label_forms_and_preserves_node_shapes() {
        let graph =
            parse("flowchart LR\n A([Shell]) -- launch --> B{Ready}\n B -.->|retry| C[Agent]")
                .unwrap();
        assert_eq!(graph.edges[0].label.as_deref(), Some("launch"));
        assert_eq!(graph.edges[1].label.as_deref(), Some("retry"));
        assert_eq!(graph.nodes[0].shape, NodeShape::Terminal);
        assert_eq!(graph.nodes[1].shape, NodeShape::Decision);

        let rendered = render(&graph, 120, false).join("\n");
        assert!(rendered.contains("launch"));
        assert!(rendered.contains('›'));
        assert!(rendered.contains('◇'));
    }

    #[test]
    fn wide_node_labels_do_not_overwrite_box_borders() {
        let graph = parse("flowchart LR\n A[你好你好你好你好你好你好]").unwrap();
        let rendered = render(&graph, 24, false);

        assert_eq!(rendered[2].chars().next(), Some('│'));
        assert_eq!(rendered[2].chars().last(), Some('│'));
        assert!(rendered.iter().all(|line| line.width() <= 24));
    }
}
