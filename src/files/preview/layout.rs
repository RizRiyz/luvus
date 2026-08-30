use std::sync::Arc;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::document::{Block, Inline, InlineRole, PreviewDocument};
use super::mermaid;

pub const LAYOUT_CACHE_CAP: usize = 3;
pub const MAX_LAYOUT_ROWS: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayoutKey {
    pub width: u16,
    pub ascii: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRole {
    Normal,
    Heading(u8),
    Emphasis,
    Strong,
    Strikethrough,
    Code,
    Link,
    Quote,
    Marker,
    TableHeader,
    Mermaid,
    Muted,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSpan {
    pub text: String,
    pub role: TextRole,
    pub link: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StyledRow {
    pub spans: Vec<StyledSpan>,
    pub source_line: Option<usize>,
    /// `Some(n)` when this row visually continues the previous logical row,
    /// where `n` spaces were hidden at the wrap boundary. `None` means a real
    /// rendered-row boundary. Kept numeric so every cached row stays bounded
    /// and allocation-free.
    pub soft_wrap_spaces: Option<u16>,
}

impl StyledRow {
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    pub fn single(text: impl Into<String>, role: TextRole, source_line: Option<usize>) -> Self {
        Self {
            spans: vec![StyledSpan {
                text: text.into(),
                role,
                link: None,
            }],
            source_line,
            soft_wrap_spaces: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreviewLayout {
    pub rows: Vec<StyledRow>,
}

pub fn build(document: Arc<PreviewDocument>, key: LayoutKey) -> PreviewLayout {
    let width = usize::from(key.width.max(1));
    let mut rows = Vec::new();
    for (index, block) in document.blocks.iter().enumerate() {
        if index > 0 && !matches!(block, Block::ListItem { .. }) {
            rows.push(StyledRow::default());
        }
        let source_line = Some(document.source_line_for(block.range().start));
        match block {
            Block::Heading { level, content, .. } => {
                let heading_rows = wrap(heading_spans(*level, content), width, source_line);
                let rule_width = heading_rows
                    .iter()
                    .map(|row| row.plain_text().width())
                    .max()
                    .unwrap_or_default()
                    .min(width);
                rows.extend(heading_rows);
                if *level <= 2 && rule_width > 0 {
                    let rule = match (*level, key.ascii) {
                        (1, true) => '=',
                        (1, false) => '═',
                        (_, true) => '-',
                        (_, false) => '─',
                    };
                    rows.push(StyledRow::single(
                        rule.to_string().repeat(rule_width),
                        TextRole::Heading(*level),
                        source_line,
                    ));
                }
            }
            Block::Paragraph { content, .. } => {
                rows.extend(wrap(inline_spans(content), width, source_line));
            }
            Block::ListItem {
                depth,
                marker,
                checked,
                content,
                ..
            } => {
                let check = checked.map_or(String::new(), |done| {
                    if done {
                        "[x] ".into()
                    } else {
                        "[ ] ".into()
                    }
                });
                let prefix = format!("{}{}{} ", "  ".repeat(*depth), marker, check);
                rows.extend(wrap(
                    prefixed(prefix, TextRole::Marker, content),
                    width,
                    source_line,
                ));
            }
            Block::Quote { depth, content, .. } => {
                let prefix = format!("{} ", "│".repeat((*depth).max(1)));
                rows.extend(wrap(
                    prefixed(prefix, TextRole::Quote, content),
                    width,
                    source_line,
                ));
            }
            Block::Rule { .. } => rows.push(StyledRow::single(
                if key.ascii { "-" } else { "─" }.repeat(width.min(80)),
                TextRole::Muted,
                source_line,
            )),
            Block::Code { language, text, .. } => {
                if let Some(language) = language.as_ref().filter(|lang| !lang.is_empty()) {
                    rows.push(StyledRow::single(
                        format!(" {language} "),
                        TextRole::Muted,
                        source_line,
                    ));
                }
                for line in text.lines().chain(text.is_empty().then_some("")) {
                    rows.extend(wrap(
                        vec![StyledSpan {
                            text: format!("  {line}"),
                            role: TextRole::Code,
                            link: None,
                        }],
                        width,
                        source_line,
                    ));
                }
            }
            Block::Table {
                header, rows: body, ..
            } => {
                render_table(&mut rows, header, body, width, source_line, key.ascii);
            }
            Block::ImagePlaceholder { alt, target, .. } => rows.extend(wrap(
                vec![StyledSpan {
                    text: format!("Image: {alt} ({target})"),
                    role: TextRole::Muted,
                    link: None,
                }],
                width,
                source_line,
            )),
            Block::Mermaid { diagram, .. } => {
                rows.extend(mermaid::render(diagram, key.width, key.ascii, source_line));
            }
            Block::SourceFallback {
                source, diagnostic, ..
            } => {
                rows.extend(wrap(
                    vec![StyledSpan {
                        text: format!("Mermaid preview unavailable: {diagnostic}"),
                        role: TextRole::Error,
                        link: None,
                    }],
                    width,
                    source_line,
                ));
                for line in source.lines() {
                    rows.extend(wrap(
                        vec![StyledSpan {
                            text: line.to_string(),
                            role: TextRole::Code,
                            link: None,
                        }],
                        width,
                        source_line,
                    ));
                }
            }
        }
        if rows.len() >= MAX_LAYOUT_ROWS {
            rows.truncate(MAX_LAYOUT_ROWS.saturating_sub(1));
            rows.push(StyledRow::single(
                "preview truncated at the rendered-row safety limit; open the source normally for the complete file",
                TextRole::Error,
                source_line,
            ));
            break;
        }
    }
    if rows.is_empty() {
        rows.push(StyledRow::default());
    }
    PreviewLayout { rows }
}

fn inline_spans(inlines: &[Inline]) -> Vec<StyledSpan> {
    inlines
        .iter()
        .map(|inline| StyledSpan {
            text: inline.text.clone(),
            role: match inline.role {
                InlineRole::Normal => TextRole::Normal,
                InlineRole::Emphasis => TextRole::Emphasis,
                InlineRole::Strong => TextRole::Strong,
                InlineRole::Strikethrough => TextRole::Strikethrough,
                InlineRole::Code => TextRole::Code,
                InlineRole::Link => TextRole::Link,
                InlineRole::Muted => TextRole::Muted,
            },
            link: inline.link.clone(),
        })
        .collect()
}

fn heading_spans(level: u8, inlines: &[Inline]) -> Vec<StyledSpan> {
    inline_spans(inlines)
        .into_iter()
        .map(|mut span| {
            if !matches!(span.role, TextRole::Code | TextRole::Link) {
                span.role = TextRole::Heading(level);
            }
            span
        })
        .collect()
}

fn prefixed(prefix: String, role: TextRole, inlines: &[Inline]) -> Vec<StyledSpan> {
    let mut spans = vec![StyledSpan {
        text: prefix,
        role,
        link: None,
    }];
    spans.extend(inline_spans(inlines));
    spans
}

#[derive(Clone)]
struct Cell {
    ch: char,
    role: TextRole,
    link: Option<String>,
}

/// Width-aware wrapping shared by every semantic block. Cells retain their
/// role while wrapping, then contiguous roles are compressed back into spans.
fn wrap(spans: Vec<StyledSpan>, width: usize, source_line: Option<usize>) -> Vec<StyledRow> {
    let mut logical = Vec::<Vec<Cell>>::new();
    logical.push(Vec::new());
    for span in spans {
        for ch in span.text.chars() {
            if ch == '\n' {
                logical.push(Vec::new());
            } else {
                logical.last_mut().expect("one logical row").push(Cell {
                    ch,
                    role: span.role,
                    link: span.link.clone(),
                });
            }
        }
    }
    let mut out = Vec::new();
    for cells in logical {
        if cells.is_empty() {
            out.push(StyledRow {
                spans: Vec::new(),
                source_line,
                soft_wrap_spaces: None,
            });
            continue;
        }
        let mut start = 0usize;
        let mut soft_wrap_spaces = None;
        while start < cells.len() && out.len() < MAX_LAYOUT_ROWS {
            let mut used = 0usize;
            let mut end = start;
            let mut last_space = None;
            for (index, cell) in cells.iter().enumerate().skip(start) {
                let cell_width = cell.ch.width().unwrap_or(0);
                if used + cell_width > width && end > start {
                    break;
                }
                used += cell_width;
                end = index + 1;
                if cell.ch.is_whitespace() {
                    last_space = Some(end);
                }
                if used >= width {
                    break;
                }
            }
            if end < cells.len() {
                if let Some(space) = last_space.filter(|space| *space > start) {
                    end = space;
                }
            }
            end = end.max(start + 1).min(cells.len());
            let mut part = cells[start..end].to_vec();
            while part.last().is_some_and(|cell| cell.ch.is_whitespace()) {
                part.pop();
            }
            let visible_end = start + part.len();
            let mut next_start = end;
            while cells
                .get(next_start)
                .is_some_and(|cell| cell.ch.is_whitespace())
            {
                next_start += 1;
            }
            out.push(cells_to_row(part, source_line, soft_wrap_spaces));
            soft_wrap_spaces = Some(
                cells[visible_end..next_start]
                    .iter()
                    .map(|cell| cell.ch.width().unwrap_or(0))
                    .sum::<usize>()
                    .min(u16::MAX as usize) as u16,
            );
            start = next_start;
        }
    }
    out
}

fn cells_to_row(
    cells: Vec<Cell>,
    source_line: Option<usize>,
    soft_wrap_spaces: Option<u16>,
) -> StyledRow {
    let mut spans: Vec<StyledSpan> = Vec::new();
    for cell in cells {
        if let Some(last) = spans
            .last_mut()
            .filter(|span| span.role == cell.role && span.link == cell.link)
        {
            last.text.push(cell.ch);
        } else {
            spans.push(StyledSpan {
                text: cell.ch.to_string(),
                role: cell.role,
                link: cell.link,
            });
        }
    }
    StyledRow {
        spans,
        source_line,
        soft_wrap_spaces,
    }
}

fn render_table(
    out: &mut Vec<StyledRow>,
    header: &[Vec<Inline>],
    body: &[Vec<Vec<Inline>>],
    width: usize,
    source_line: Option<usize>,
    ascii: bool,
) {
    let columns = header
        .len()
        .max(body.iter().map(Vec::len).max().unwrap_or_default());
    if columns == 0 {
        return;
    }
    let available = width.saturating_sub(columns + 1);
    let column_width = (available / columns).max(3);
    let separator = if ascii { "+" } else { "┼" };
    let rule = (0..columns)
        .map(|_| if ascii { "-" } else { "─" }.repeat(column_width))
        .collect::<Vec<_>>()
        .join(separator);
    let emit = |cells: &[Vec<Inline>], role: TextRole, out: &mut Vec<StyledRow>| {
        let text = (0..columns)
            .map(|index| {
                let text: String = cells
                    .get(index)
                    .into_iter()
                    .flat_map(|cell| cell.iter())
                    .map(|inline| inline.text.as_str())
                    .collect();
                let mut used = 0usize;
                let clipped: String = text
                    .chars()
                    .take_while(|ch| {
                        let next = used + ch.width().unwrap_or(0);
                        if next > column_width {
                            false
                        } else {
                            used = next;
                            true
                        }
                    })
                    .collect();
                format!(
                    "{clipped}{}",
                    " ".repeat(column_width.saturating_sub(clipped.width()))
                )
            })
            .collect::<Vec<_>>()
            .join(if ascii { "|" } else { "│" });
        out.push(StyledRow::single(text, role, source_line));
    };
    if !header.is_empty() {
        emit(header, TextRole::TableHeader, out);
        out.push(StyledRow::single(
            rule.clone(),
            TextRole::Muted,
            source_line,
        ));
    }
    for row in body {
        if out.len() >= MAX_LAYOUT_ROWS {
            return;
        }
        emit(row, TextRole::Normal, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn wrapping_is_width_bounded_and_preserves_roles() {
        let rows = wrap(
            vec![
                StyledSpan {
                    text: "alpha beta ".into(),
                    role: TextRole::Normal,
                    link: None,
                },
                StyledSpan {
                    text: "gamma".into(),
                    role: TextRole::Strong,
                    link: None,
                },
            ],
            10,
            Some(1),
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.plain_text().width() <= 10));
        assert_eq!(rows[1].spans.last().unwrap().role, TextRole::Strong);
        assert_eq!(rows[1].soft_wrap_spaces, Some(1));
    }

    #[test]
    fn headings_hide_markdown_markers_and_keep_visual_hierarchy() {
        let document = Arc::new(PreviewDocument::new(
            Arc::<str>::from("# Project"),
            vec![Block::Heading {
                level: 1,
                content: vec![Inline::plain("Project")],
                range: 0..9,
            }],
        ));
        let layout = build(
            document,
            LayoutKey {
                width: 40,
                ascii: false,
            },
        );

        assert_eq!(layout.rows[0].plain_text(), "Project");
        assert_eq!(layout.rows[0].spans[0].role, TextRole::Heading(1));
        assert_eq!(layout.rows[1].plain_text(), "═══════");
        assert!(layout
            .rows
            .iter()
            .all(|row| !row.plain_text().contains('#')));
    }

    #[test]
    fn table_emission_stops_at_the_layout_row_limit() {
        let mut rows = vec![StyledRow::default(); MAX_LAYOUT_ROWS - 1];
        let body = vec![
            vec![vec![Inline::plain("first")]],
            vec![vec![Inline::plain("second")]],
        ];

        render_table(&mut rows, &[], &body, 20, Some(1), false);

        assert_eq!(rows.len(), MAX_LAYOUT_ROWS);
        assert_eq!(
            rows.last()
                .map(StyledRow::plain_text)
                .as_deref()
                .map(str::trim_end),
            Some("first")
        );
    }
}
