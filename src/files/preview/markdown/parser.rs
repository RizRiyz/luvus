use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::super::document::{Block, Inline, InlineRole};
use super::super::mermaid;

enum ActiveKind {
    Paragraph,
    Heading(u8),
    Quote(usize),
    ListItem {
        depth: usize,
        marker: String,
        checked: Option<bool>,
    },
    Code {
        language: Option<String>,
    },
}

struct Active {
    kind: ActiveKind,
    content: Vec<Inline>,
    code: String,
    range: Range<usize>,
}

impl Active {
    fn new(kind: ActiveKind, start: usize) -> Self {
        Self {
            kind,
            content: Vec::new(),
            code: String::new(),
            range: start..start,
        }
    }
}

struct ListState {
    next: Option<u64>,
}

#[derive(Default)]
struct TableBuilder {
    header: Vec<Vec<Inline>>,
    rows: Vec<Vec<Vec<Inline>>>,
    current_row: Vec<Vec<Inline>>,
    current_cell: Vec<Inline>,
    in_header: bool,
    range: Range<usize>,
}

struct ImageBuilder {
    target: String,
    alt: String,
    range: Range<usize>,
}

pub fn parse(source: &str) -> Vec<Block> {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parser = Parser::new_ext(source, options).into_offset_iter();
    let mut blocks = Vec::new();
    let mut active: Option<Active> = None;
    let mut lists = Vec::<ListState>::new();
    let mut quote_depth = 0usize;
    let mut roles = Vec::<InlineRole>::new();
    let mut links = Vec::<String>::new();
    let mut table: Option<TableBuilder> = None;
    let mut image: Option<ImageBuilder> = None;

    for (event, range) in parser {
        if let Some(active) = active.as_mut() {
            active.range.end = active.range.end.max(range.end);
        }
        if let Some(table) = table.as_mut() {
            table.range.end = table.range.end.max(range.end);
        }
        if let Some(image) = image.as_mut() {
            image.range.end = image.range.end.max(range.end);
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if active.is_none() && table.is_none() {
                        active = Some(Active::new(
                            if quote_depth > 0 {
                                ActiveKind::Quote(quote_depth)
                            } else {
                                ActiveKind::Paragraph
                            },
                            range.start,
                        ));
                    }
                }
                Tag::Heading { level, .. } => {
                    flush(&mut active, &mut blocks);
                    active = Some(Active::new(
                        ActiveKind::Heading(level_number(level)),
                        range.start,
                    ));
                }
                Tag::BlockQuote(_) => quote_depth += 1,
                Tag::CodeBlock(kind) => {
                    flush(&mut active, &mut blocks);
                    let language = match kind {
                        CodeBlockKind::Indented => None,
                        CodeBlockKind::Fenced(info) => info
                            .split_whitespace()
                            .next()
                            .filter(|value| !value.is_empty())
                            .map(|value| value.to_ascii_lowercase()),
                    };
                    active = Some(Active::new(ActiveKind::Code { language }, range.start));
                }
                Tag::List(start) => lists.push(ListState { next: start }),
                Tag::Item => {
                    flush(&mut active, &mut blocks);
                    let depth = lists.len().saturating_sub(1);
                    let marker = match lists.last_mut().and_then(|list| list.next.as_mut()) {
                        Some(number) => {
                            let marker = format!("{number}.");
                            *number = number.saturating_add(1);
                            marker
                        }
                        None => "•".into(),
                    };
                    active = Some(Active::new(
                        ActiveKind::ListItem {
                            depth,
                            marker,
                            checked: None,
                        },
                        range.start,
                    ));
                }
                Tag::Emphasis => roles.push(InlineRole::Emphasis),
                Tag::Strong => roles.push(InlineRole::Strong),
                Tag::Strikethrough => roles.push(InlineRole::Strikethrough),
                Tag::Link { dest_url, .. } => {
                    roles.push(InlineRole::Link);
                    links.push(dest_url.into_string());
                }
                Tag::Image { dest_url, .. } => {
                    image = Some(ImageBuilder {
                        target: dest_url.into_string(),
                        alt: String::new(),
                        range: range.clone(),
                    });
                }
                Tag::Table(_) => {
                    flush(&mut active, &mut blocks);
                    table = Some(TableBuilder {
                        range: range.start..range.end,
                        ..TableBuilder::default()
                    });
                }
                Tag::TableHead => {
                    if let Some(table) = table.as_mut() {
                        table.in_header = true;
                    }
                }
                Tag::TableRow => {
                    if let Some(table) = table.as_mut() {
                        table.current_row.clear();
                    }
                }
                Tag::TableCell => {
                    if let Some(table) = table.as_mut() {
                        table.current_cell.clear();
                    }
                }
                Tag::FootnoteDefinition(_) => {
                    flush(&mut active, &mut blocks);
                    active = Some(Active::new(ActiveKind::Paragraph, range.start));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if matches!(
                        active.as_ref().map(|active| &active.kind),
                        Some(ActiveKind::Paragraph | ActiveKind::Quote(_))
                    ) {
                        flush(&mut active, &mut blocks);
                    }
                }
                TagEnd::Heading(_) | TagEnd::CodeBlock | TagEnd::Item => {
                    flush(&mut active, &mut blocks)
                }
                TagEnd::BlockQuote(_) => quote_depth = quote_depth.saturating_sub(1),
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    roles.pop();
                }
                TagEnd::Link => {
                    roles.pop();
                    links.pop();
                }
                TagEnd::Image => {
                    if let Some(image) = image.take() {
                        flush(&mut active, &mut blocks);
                        blocks.push(Block::ImagePlaceholder {
                            alt: image.alt,
                            target: image.target,
                            range: image.range,
                        });
                    }
                }
                TagEnd::TableCell => {
                    if let Some(table) = table.as_mut() {
                        table
                            .current_row
                            .push(std::mem::take(&mut table.current_cell));
                    }
                }
                TagEnd::TableRow => {
                    if let Some(table) = table.as_mut() {
                        let row = std::mem::take(&mut table.current_row);
                        if table.in_header {
                            table.header = row;
                        } else {
                            table.rows.push(row);
                        }
                    }
                }
                TagEnd::TableHead => {
                    if let Some(table) = table.as_mut() {
                        table.in_header = false;
                    }
                }
                TagEnd::Table => {
                    if let Some(table) = table.take() {
                        blocks.push(Block::Table {
                            header: table.header,
                            rows: table.rows,
                            range: table.range,
                        });
                    }
                }
                TagEnd::FootnoteDefinition => flush(&mut active, &mut blocks),
                _ => {}
            },
            Event::Text(text) => {
                if let Some(image) = image.as_mut() {
                    image.alt.push_str(&text);
                } else if let Some(table) = table.as_mut() {
                    push_inline(&mut table.current_cell, text.into_string(), &roles, &links);
                } else {
                    ensure_text_active(&mut active, quote_depth, range.start);
                    if let Some(active) = active.as_mut() {
                        match active.kind {
                            ActiveKind::Code { .. } => active.code.push_str(&text),
                            _ => {
                                push_inline(&mut active.content, text.into_string(), &roles, &links)
                            }
                        }
                    }
                }
            }
            Event::Code(code) => {
                ensure_text_active(&mut active, quote_depth, range.start);
                if let Some(active) = active.as_mut() {
                    active.content.push(Inline {
                        text: code.into_string(),
                        role: InlineRole::Code,
                        link: links.last().cloned(),
                    });
                }
            }
            Event::SoftBreak => push_break(&mut active, " "),
            Event::HardBreak => push_break(&mut active, "\n"),
            Event::Rule => {
                flush(&mut active, &mut blocks);
                blocks.push(Block::Rule { range });
            }
            Event::TaskListMarker(checked) => {
                if let Some(Active {
                    kind: ActiveKind::ListItem { checked: state, .. },
                    ..
                }) = active.as_mut()
                {
                    *state = Some(checked);
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                ensure_text_active(&mut active, quote_depth, range.start);
                if let Some(active) = active.as_mut() {
                    active.content.push(Inline {
                        text: html.into_string(),
                        role: InlineRole::Muted,
                        link: None,
                    });
                }
            }
            Event::FootnoteReference(reference) => {
                ensure_text_active(&mut active, quote_depth, range.start);
                if let Some(active) = active.as_mut() {
                    active
                        .content
                        .push(Inline::plain(format!("[^{reference}]")));
                }
            }
            _ => {}
        }
    }
    flush(&mut active, &mut blocks);
    blocks
}

fn level_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn ensure_text_active(active: &mut Option<Active>, quote_depth: usize, start: usize) {
    if active.is_none() {
        *active = Some(Active::new(
            if quote_depth > 0 {
                ActiveKind::Quote(quote_depth)
            } else {
                ActiveKind::Paragraph
            },
            start,
        ));
    }
}

fn push_inline(content: &mut Vec<Inline>, text: String, roles: &[InlineRole], links: &[String]) {
    let role = roles.last().copied().unwrap_or(InlineRole::Normal);
    let link = links.last().cloned();
    if let Some(last) = content
        .last_mut()
        .filter(|inline| inline.role == role && inline.link == link)
    {
        last.text.push_str(&text);
    } else {
        content.push(Inline { text, role, link });
    }
}

fn push_break(active: &mut Option<Active>, text: &str) {
    if let Some(active) = active.as_mut() {
        match active.kind {
            ActiveKind::Code { .. } => active.code.push_str(text),
            _ => active.content.push(Inline::plain(text)),
        }
    }
}

fn flush(active: &mut Option<Active>, blocks: &mut Vec<Block>) {
    let Some(active) = active.take() else {
        return;
    };
    let range = active.range;
    match active.kind {
        ActiveKind::Paragraph if !active.content.is_empty() => blocks.push(Block::Paragraph {
            content: active.content,
            range,
        }),
        ActiveKind::Heading(level) => blocks.push(Block::Heading {
            level,
            content: active.content,
            range,
        }),
        ActiveKind::Quote(depth) if !active.content.is_empty() => blocks.push(Block::Quote {
            depth,
            content: active.content,
            range,
        }),
        ActiveKind::ListItem {
            depth,
            marker,
            checked,
        } => blocks.push(Block::ListItem {
            depth,
            marker,
            checked,
            content: active.content,
            range,
        }),
        ActiveKind::Code { language } => {
            if language.as_deref() == Some("mermaid") {
                let source = active.code;
                match mermaid::parse(&source) {
                    Ok(diagram) => blocks.push(Block::Mermaid { diagram, range }),
                    Err(diagnostic) => blocks.push(Block::SourceFallback {
                        source,
                        diagnostic,
                        range,
                    }),
                }
            } else {
                blocks.push(Block::Code {
                    language,
                    text: active.code,
                    range,
                });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gfm_blocks_and_embedded_mermaid() {
        let source = "# Title\n\n- [x] done\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n```mermaid\nflowchart LR\nA-->B\n```\n";
        let blocks = parse(source);
        assert!(matches!(
            blocks.first(),
            Some(Block::Heading { level: 1, .. })
        ));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::ListItem {
                checked: Some(true),
                ..
            }
        )));
        assert!(blocks
            .iter()
            .any(|block| matches!(block, Block::Table { .. })));
        assert!(blocks
            .iter()
            .any(|block| matches!(block, Block::Mermaid { .. })));
    }

    #[test]
    fn raw_html_remains_text() {
        let blocks = parse("<script>alert(1)</script>");
        let Block::Paragraph { content, .. } = &blocks[0] else {
            panic!("html remains a paragraph");
        };
        assert_eq!(content[0].text, "<script>alert(1)</script>");
    }
}
