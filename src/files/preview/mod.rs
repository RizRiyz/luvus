//! Explicit terminal-native Markdown and Mermaid previews.
//!
//! Normal file opening never enters this module. An explicit preview action
//! reads and parses once on a transient worker; width-specific layouts are also
//! transient worker jobs and are cached with a strict per-view bound.

mod document;
pub mod layout;
mod markdown;
pub mod mermaid;

use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use document::{Block, PreviewDocument};
pub use layout::{LayoutKey, PreviewLayout, TextRole};

use crate::files::SIZE_CAP;
use crate::search::local::{first_at_or_after, match_spans, LocalSearch, RowMatch};

const SNIFF: usize = 8192;
const MAX_PENDING_LAYOUTS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewKind {
    Markdown,
    Mermaid,
}

impl PreviewKind {
    pub fn for_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        if extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown") {
            Some(Self::Markdown)
        } else if extension.eq_ignore_ascii_case("mermaid") || extension.eq_ignore_ascii_case("mmd")
        {
            Some(Self::Mermaid)
        } else {
            None
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Mermaid => "Mermaid",
        }
    }
}

#[derive(Clone, Debug)]
pub enum PreviewLoad {
    Loading,
    Ready(Arc<PreviewDocument>),
    Binary(u64),
    TooLarge(u64),
    Error(String),
}

pub type PreviewSearch = LocalSearch<RowMatch>;

pub struct DocumentView {
    pub path: PathBuf,
    pub kind: PreviewKind,
    pub load: PreviewLoad,
    pub scroll: usize,
    pub mtime: Option<std::time::SystemTime>,
    pub read_token: u64,
    pub search: Option<PreviewSearch>,
    search_layout: Option<LayoutKey>,
    layouts: VecDeque<(LayoutKey, Arc<PreviewLayout>)>,
    pending_layouts: HashSet<LayoutKey>,
    scroll_anchor_line: Option<usize>,
}

impl DocumentView {
    pub fn new(path: PathBuf, kind: PreviewKind) -> Self {
        Self {
            path,
            kind,
            load: PreviewLoad::Loading,
            scroll: 0,
            mtime: None,
            read_token: 0,
            search: None,
            search_layout: None,
            layouts: VecDeque::new(),
            pending_layouts: HashSet::new(),
            scroll_anchor_line: None,
        }
    }

    pub fn apply(&mut self, load: PreviewLoad) {
        self.capture_scroll_anchor();
        let search = self.search.take();
        self.load = load;
        self.layouts.clear();
        self.pending_layouts.clear();
        self.search_layout = None;
        self.search = search.map(|mut search| {
            if !search.editing {
                search.matches.clear();
                search.current = 0;
            }
            search
        });
    }

    pub fn document(&self) -> Option<Arc<PreviewDocument>> {
        match &self.load {
            PreviewLoad::Ready(document) => Some(Arc::clone(document)),
            _ => None,
        }
    }

    pub fn layout(&self, key: LayoutKey) -> Option<&Arc<PreviewLayout>> {
        self.layouts
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, layout)| layout)
    }

    pub fn begin_layout(&mut self, key: LayoutKey) -> Option<Arc<PreviewDocument>> {
        if self.layout(key).is_some()
            || self.pending_layouts.contains(&key)
            || self.pending_layouts.len() >= MAX_PENDING_LAYOUTS
        {
            return None;
        }
        self.pending_layouts.insert(key);
        self.capture_scroll_anchor();
        self.document()
    }

    #[cfg(test)]
    pub fn apply_layout(&mut self, key: LayoutKey, layout: Arc<PreviewLayout>) {
        self.apply_layout_with_viewport(key, layout, None);
    }

    pub fn apply_layout_for_viewport(
        &mut self,
        key: LayoutKey,
        layout: Arc<PreviewLayout>,
        viewport: usize,
    ) {
        self.apply_layout_with_viewport(key, layout, Some(viewport.max(1)));
    }

    fn apply_layout_with_viewport(
        &mut self,
        key: LayoutKey,
        layout: Arc<PreviewLayout>,
        viewport: Option<usize>,
    ) {
        self.pending_layouts.remove(&key);
        self.layouts.retain(|(candidate, _)| *candidate != key);
        self.layouts.push_front((key, layout));
        self.layouts.truncate(layout::LAYOUT_CACHE_CAP);
        if let Some(anchor) = self.scroll_anchor_line.take() {
            if let Some(current) = self.layout(key) {
                self.scroll = current
                    .rows
                    .iter()
                    .position(|row| row.source_line.is_some_and(|line| line >= anchor))
                    .unwrap_or_else(|| current.rows.len().saturating_sub(1));
            }
        }
        if let Some(current) = self.layout(key) {
            self.scroll = self.scroll.min(current.rows.len().saturating_sub(1));
        }
        let committed_query = self.search.as_ref().and_then(|search| {
            (!search.editing && !search.query.is_empty()).then(|| search.query.clone())
        });
        if let Some(query) = committed_query {
            self.rebuild_search(key, query);
            if let Some(viewport) = viewport {
                self.reveal_search(viewport);
            }
        }
    }

    pub fn layout_pending(&self, key: LayoutKey) -> bool {
        self.pending_layouts.contains(&key)
    }

    pub fn search_begin(&mut self) {
        self.search = Some(PreviewSearch::editing());
        self.search_layout = None;
    }

    pub fn search_push(&mut self, ch: char) {
        if let Some(search) = self.search.as_mut() {
            search.push(ch);
        }
    }

    pub fn search_backspace(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.backspace();
        }
    }

    pub fn search_cancel(&mut self) {
        self.search = None;
        self.search_layout = None;
    }

    pub fn search_commit(&mut self, key: LayoutKey, viewport: usize) {
        let Some(query) = self.search.as_ref().map(|search| search.query.clone()) else {
            return;
        };
        if query.is_empty() {
            self.search = None;
            return;
        }
        if let Some(search) = self.search.as_mut() {
            search.commit();
        }
        self.rebuild_search(key, query);
        self.reveal_search(viewport);
    }

    pub fn search_clear(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.clear();
            self.search_layout = None;
        }
    }

    pub fn search_toggle_case(&mut self, key: LayoutKey, viewport: usize) {
        let rebuild = self
            .search
            .as_mut()
            .is_some_and(|search| search.toggle_case());
        if rebuild {
            if let Some(query) = self.search.as_ref().map(|search| search.query.clone()) {
                self.rebuild_search(key, query);
                self.reveal_search(viewport);
            }
        }
    }

    fn rebuild_search(&mut self, key: LayoutKey, query: String) {
        let case_sensitive = self
            .search
            .as_ref()
            .is_some_and(|search| search.case_sensitive);
        let matches = self
            .layout(key)
            .map(|layout| {
                layout
                    .rows
                    .iter()
                    .enumerate()
                    .flat_map(|(row, rendered)| {
                        match_spans(&rendered.plain_text(), &query, case_sensitive)
                            .into_iter()
                            .map(move |search_match| RowMatch::at(row, search_match))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let current = first_at_or_after(&matches, (self.scroll, 0), |search_match| {
            (search_match.row, search_match.column)
        });
        if let Some(search) = self.search.as_mut() {
            search.query = query;
            search.editing = false;
            search.replace_matches(matches, current);
            self.search_layout = Some(key);
        }
    }

    pub fn sync_search_layout(&mut self, key: LayoutKey, viewport: usize) {
        if self.search_layout == Some(key) || self.layout(key).is_none() {
            return;
        }
        let committed_query = self.search.as_ref().and_then(|search| {
            (!search.editing && !search.query.is_empty()).then(|| search.query.clone())
        });
        if let Some(query) = committed_query {
            self.rebuild_search(key, query);
            self.reveal_search(viewport);
        }
    }

    pub fn search_step(&mut self, forward: bool, viewport: usize) {
        if self
            .search
            .as_mut()
            .is_some_and(|search| search.step(forward))
        {
            self.reveal_search(viewport);
        }
    }

    fn reveal_search(&mut self, viewport: usize) {
        let Some(row) = self
            .search
            .as_ref()
            .and_then(|search| search.matches.get(search.current))
            .map(|search_match| search_match.row)
        else {
            return;
        };
        if row < self.scroll || row >= self.scroll + viewport.max(1) {
            self.scroll = row.saturating_sub(viewport / 2);
        }
    }

    pub fn scroll_by(&mut self, delta: i32, viewport: usize, key: LayoutKey) {
        let count = self.layout(key).map_or(0, |layout| layout.rows.len());
        let max = count.saturating_sub(viewport.max(1));
        self.scroll = (self.scroll as i32 + delta).clamp(0, max as i32) as usize;
    }

    pub fn goto_bottom(&mut self, viewport: usize, key: LayoutKey) {
        self.scroll = self.layout(key).map_or(0, |layout| {
            layout.rows.len().saturating_sub(viewport.max(1))
        });
    }

    fn capture_scroll_anchor(&mut self) {
        if self.scroll_anchor_line.is_some() {
            return;
        }
        self.scroll_anchor_line = self
            .layouts
            .front()
            .and_then(|(_, layout)| layout.rows.get(self.scroll))
            .and_then(|row| row.source_line);
    }
}

pub fn read(path: &Path, kind: PreviewKind) -> PreviewLoad {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return PreviewLoad::Error(error.to_string()),
    };
    if metadata.len() > SIZE_CAP {
        return PreviewLoad::TooLarge(metadata.len());
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return PreviewLoad::Error(error.to_string()),
    };
    let bytes = match read_at_most_cap_plus_one(file, metadata.len()) {
        Ok(bytes) => bytes,
        Err(error) => return PreviewLoad::Error(error.to_string()),
    };
    if bytes.len() as u64 > SIZE_CAP {
        return PreviewLoad::TooLarge(metadata.len().max(bytes.len() as u64));
    }
    if bytes.iter().take(SNIFF).any(|byte| *byte == 0) {
        return PreviewLoad::Binary(metadata.len());
    }
    let source = match String::from_utf8(bytes) {
        Ok(source) => Arc::<str>::from(source),
        Err(_) => return PreviewLoad::Error("preview requires UTF-8 text".into()),
    };
    let blocks = match kind {
        PreviewKind::Markdown => markdown::parse(&source),
        PreviewKind::Mermaid => vec![match mermaid::parse(&source) {
            Ok(diagram) => Block::Mermaid {
                diagram,
                range: 0..source.len(),
            },
            Err(diagnostic) => Block::SourceFallback {
                source: source.to_string(),
                diagnostic,
                range: 0..source.len(),
            },
        }],
    };
    PreviewLoad::Ready(Arc::new(PreviewDocument::new(source, blocks)))
}

fn read_at_most_cap_plus_one(reader: impl Read, capacity_hint: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(capacity_hint.min(SIZE_CAP) as usize);
    reader
        .take(SIZE_CAP.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_detection_is_explicit_and_case_insensitive() {
        assert_eq!(
            PreviewKind::for_path(Path::new("README.MD")),
            Some(PreviewKind::Markdown)
        );
        assert_eq!(
            PreviewKind::for_path(Path::new("flow.MERMAID")),
            Some(PreviewKind::Mermaid)
        );
        assert_eq!(
            PreviewKind::for_path(Path::new("flow.MmD")),
            Some(PreviewKind::Mermaid)
        );
        assert_eq!(PreviewKind::for_path(Path::new("component.mdx")), None);
    }

    #[test]
    fn document_view_keeps_only_three_width_layouts() {
        let source = Arc::<str>::from("hello");
        let document = Arc::new(PreviewDocument::new(
            source,
            vec![Block::Paragraph {
                content: vec![document::Inline::plain("hello")],
                range: 0..5,
            }],
        ));
        let mut view = DocumentView::new(PathBuf::from("README.md"), PreviewKind::Markdown);
        view.apply(PreviewLoad::Ready(Arc::clone(&document)));
        for width in [20, 30, 40, 50] {
            let key = LayoutKey {
                width,
                ascii: false,
            };
            view.apply_layout(key, Arc::new(layout::build(Arc::clone(&document), key)));
        }
        assert!(view
            .layout(LayoutKey {
                width: 20,
                ascii: false
            })
            .is_none());
        assert!(view
            .layout(LayoutKey {
                width: 50,
                ascii: false
            })
            .is_some());
        assert_eq!(view.layouts.len(), layout::LAYOUT_CACHE_CAP);
    }

    #[test]
    fn committed_search_reveals_an_offscreen_match_when_layout_arrives() {
        let source = (0..30)
            .map(|index| {
                if index == 25 {
                    "Needle".to_string()
                } else {
                    format!("line {index}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let document = Arc::new(PreviewDocument::new(
            Arc::<str>::from(source.as_str()),
            vec![Block::Code {
                language: None,
                text: source.clone(),
                range: 0..source.len(),
            }],
        ));
        let key = LayoutKey {
            width: 40,
            ascii: false,
        };
        let mut view = DocumentView::new(PathBuf::from("README.md"), PreviewKind::Markdown);
        view.apply(PreviewLoad::Ready(Arc::clone(&document)));
        view.search_begin();
        for ch in "needle".chars() {
            view.search_push(ch);
        }
        view.search_commit(key, 5);
        assert_eq!(view.scroll, 0, "no layout exists yet");

        view.apply_layout_for_viewport(key, Arc::new(layout::build(document, key)), 5);

        let search = view.search.as_ref().expect("committed search");
        assert_eq!(search.matches.len(), 1);
        assert!(
            view.scroll > 0,
            "the asynchronously discovered offscreen match must become visible"
        );
        assert!(
            search.matches[search.current].row >= view.scroll,
            "current match starts inside the revealed viewport"
        );
    }

    #[test]
    fn preview_layout_refresh_preserves_editing_and_rebuilds_committed_search() {
        let source = Arc::<str>::from("Needle needle");
        let document = Arc::new(PreviewDocument::new(
            Arc::clone(&source),
            vec![Block::Paragraph {
                content: vec![document::Inline::plain(source.as_ref())],
                range: 0..source.len(),
            }],
        ));
        let key = LayoutKey {
            width: 40,
            ascii: false,
        };
        let mut view = DocumentView::new(PathBuf::from("README.md"), PreviewKind::Markdown);
        view.apply(PreviewLoad::Ready(Arc::clone(&document)));
        view.search_begin();
        for ch in "needle".chars() {
            view.search_push(ch);
        }
        view.apply_layout(key, Arc::new(layout::build(Arc::clone(&document), key)));
        assert!(view.search.as_ref().unwrap().editing);
        assert!(view.search.as_ref().unwrap().matches.is_empty());

        view.search_commit(key, 10);
        assert_eq!(view.search.as_ref().unwrap().matches.len(), 2);
        let wide_matches = view.search.as_ref().unwrap().matches.clone();
        let narrow = LayoutKey {
            width: 8,
            ascii: false,
        };
        view.apply_layout(
            narrow,
            Arc::new(layout::build(Arc::clone(&document), narrow)),
        );
        assert!(!view.search.as_ref().unwrap().editing);
        assert_eq!(view.search.as_ref().unwrap().matches.len(), 2);
        assert_eq!(view.search_layout, Some(narrow));

        view.sync_search_layout(key, 10);

        assert_eq!(view.search_layout, Some(key));
        assert_eq!(view.search.as_ref().unwrap().matches, wide_matches);
    }

    #[test]
    fn checked_in_preview_examples_remain_parseable_and_width_bounded() {
        let markdown = include_str!("../../../examples/preview/README.md");
        let blocks = markdown::parse(markdown);
        assert!(blocks
            .iter()
            .any(|block| matches!(block, Block::Mermaid { .. })));
        assert!(!blocks
            .iter()
            .any(|block| matches!(block, Block::SourceFallback { .. })));

        for source in [
            include_str!("../../../examples/preview/workflow.mmd"),
            include_str!("../../../examples/preview/agent-session.mermaid"),
        ] {
            assert!(mermaid::parse(source).is_ok());
        }

        let document = Arc::new(PreviewDocument::new(Arc::<str>::from(markdown), blocks));
        for width in [40, 100] {
            let rendered = layout::build(
                Arc::clone(&document),
                LayoutKey {
                    width,
                    ascii: false,
                },
            );
            assert!(!rendered.rows.is_empty());
            assert!(rendered.rows.iter().all(|row| {
                unicode_width::UnicodeWidthStr::width(row.plain_text().as_str())
                    <= usize::from(width)
            }));
        }
    }

    #[test]
    fn bounded_reader_never_consumes_more_than_cap_plus_one() {
        let input = vec![b'x'; SIZE_CAP as usize + 128];
        let bytes = read_at_most_cap_plus_one(std::io::Cursor::new(input), 0).unwrap();

        assert_eq!(bytes.len() as u64, SIZE_CAP + 1);
    }
}
