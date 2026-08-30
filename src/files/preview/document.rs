use std::ops::Range;
use std::sync::Arc;

use super::mermaid::MermaidDiagram;

pub type SourceRange = Range<usize>;

/// Semantic inline content. Colors are deliberately absent: the UI maps these
/// roles through the active Luvus theme without reparsing the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    pub role: InlineRole,
    pub link: Option<String>,
}

impl Inline {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            role: InlineRole::Normal,
            link: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InlineRole {
    #[default]
    Normal,
    Emphasis,
    Strong,
    Strikethrough,
    Code,
    Link,
    Muted,
}

/// One source-anchored document block shared by Markdown previews and
/// standalone Mermaid previews.
#[derive(Clone, Debug)]
pub enum Block {
    Heading {
        level: u8,
        content: Vec<Inline>,
        range: SourceRange,
    },
    Paragraph {
        content: Vec<Inline>,
        range: SourceRange,
    },
    ListItem {
        depth: usize,
        marker: String,
        checked: Option<bool>,
        content: Vec<Inline>,
        range: SourceRange,
    },
    Quote {
        depth: usize,
        content: Vec<Inline>,
        range: SourceRange,
    },
    Rule {
        range: SourceRange,
    },
    Code {
        language: Option<String>,
        text: String,
        range: SourceRange,
    },
    Table {
        header: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
        range: SourceRange,
    },
    ImagePlaceholder {
        alt: String,
        target: String,
        range: SourceRange,
    },
    Mermaid {
        diagram: MermaidDiagram,
        range: SourceRange,
    },
    SourceFallback {
        source: String,
        diagnostic: String,
        range: SourceRange,
    },
}

impl Block {
    pub fn range(&self) -> &SourceRange {
        match self {
            Self::Heading { range, .. }
            | Self::Paragraph { range, .. }
            | Self::ListItem { range, .. }
            | Self::Quote { range, .. }
            | Self::Rule { range }
            | Self::Code { range, .. }
            | Self::Table { range, .. }
            | Self::ImagePlaceholder { range, .. }
            | Self::Mermaid { range, .. }
            | Self::SourceFallback { range, .. } => range,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreviewDocument {
    pub source: Arc<str>,
    pub blocks: Vec<Block>,
    line_starts: Vec<usize>,
}

impl PreviewDocument {
    pub fn new(source: Arc<str>, blocks: Vec<Block>) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            source
                .bytes()
                .enumerate()
                .filter_map(|(offset, byte)| (byte == b'\n').then_some(offset + 1)),
        );
        Self {
            source,
            blocks,
            line_starts,
        }
    }

    pub fn source_line_for(&self, offset: usize) -> usize {
        self.line_starts
            .partition_point(|start| *start <= offset.min(self.source.len()))
            .max(1)
    }
}
