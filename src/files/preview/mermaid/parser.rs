use super::graph;
use super::sequence;
use super::MermaidDiagram;

pub fn parse(source: &str) -> Result<MermaidDiagram, String> {
    let first = source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("%%"))
        .ok_or("empty Mermaid source")?;
    let kind = first.split_whitespace().next().unwrap_or_default();
    if kind.eq_ignore_ascii_case("flowchart") || kind.eq_ignore_ascii_case("graph") {
        graph::parse(source).map(MermaidDiagram::Flowchart)
    } else if kind.eq_ignore_ascii_case("sequenceDiagram") {
        sequence::parse(source).map(MermaidDiagram::Sequence)
    } else {
        Err(format!("unsupported Mermaid diagram type {kind:?}"))
    }
}
