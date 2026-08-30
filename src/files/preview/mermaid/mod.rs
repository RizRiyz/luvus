mod canvas;
mod glyphs;
mod graph;
mod parser;
mod routing;
mod sequence;

use super::layout::{StyledRow, TextRole};

#[derive(Clone, Debug)]
pub enum MermaidDiagram {
    Flowchart(graph::Flowchart),
    Sequence(sequence::Sequence),
}

pub const MAX_SOURCE_BYTES: usize = 256 * 1024;

pub fn parse(source: &str) -> Result<MermaidDiagram, String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "diagram exceeds the {} KB source limit",
            MAX_SOURCE_BYTES / 1024
        ));
    }
    parser::parse(source)
}

pub fn render(
    diagram: &MermaidDiagram,
    width: u16,
    ascii: bool,
    source_line: Option<usize>,
) -> Vec<StyledRow> {
    let lines = match diagram {
        MermaidDiagram::Flowchart(flowchart) => graph::render(flowchart, usize::from(width), ascii),
        MermaidDiagram::Sequence(sequence) => sequence::render(sequence, usize::from(width), ascii),
    };
    lines
        .into_iter()
        .map(|line| StyledRow::single(line, TextRole::Mermaid, source_line))
        .collect()
}
