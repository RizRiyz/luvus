//! The file-view model (docs/38 FILE-3): one open file rendered natively inside
//! a pane or a tab. Pure state; the bytes are read on a worker thread and folded
//! in via [`FileView::apply`]. Rendering is O(visible rows) — the renderer slices
//! `lines` to the viewport.

use std::path::{Path, PathBuf};

/// Files larger than this are not read into memory — a viewer is not an excuse
/// to allocate hundreds of MB on a whim.
pub const SIZE_CAP: u64 = 5 * 1024 * 1024;

/// How many leading bytes decide "binary": a NUL in here means don't try to
/// render it as text.
const SNIFF: usize = 8192;

/// The outcome of reading a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileLoad {
    /// The read is in flight.
    Loading,
    /// Decoded text, one entry per line (tabs already expanded).
    Text(Vec<String>),
    /// Binary content (a NUL byte was found); carries the byte size.
    Binary(u64),
    /// Over [`SIZE_CAP`]; carries the byte size.
    TooLarge(u64),
    /// The read failed; carries a human-readable reason.
    Error(String),
}

/// A live in-file search over the loaded text.
#[derive(Clone, Debug, Default)]
pub struct Search {
    /// The query being typed / active (lowercased match is case-insensitive).
    pub query: String,
    /// True while the user is still typing the query (before Enter).
    pub editing: bool,
    /// `(line, start_col)` of every match, in document order.
    pub matches: Vec<(usize, usize)>,
    /// Index into `matches` of the current hit.
    pub current: usize,
}

/// One open file: what it is, and where the viewport sits.
pub struct FileView {
    pub path: PathBuf,
    pub load: FileLoad,
    /// First visible line.
    pub scroll: usize,
    /// Horizontal scroll (columns), ignored when `wrap`.
    pub hscroll: u16,
    /// Soft-wrap long lines instead of clipping + horizontal scroll.
    pub wrap: bool,
    /// The file's mtime at the last read — drives live refresh (docs/38 FILE-5).
    pub mtime: Option<std::time::SystemTime>,
    /// In-file search state (docs/38 FILE-6), `None` when not searching.
    pub search: Option<Search>,
    /// Per-line change markers vs HEAD (docs/38 + docs/30), sorted by `start`.
    /// Empty for a clean file, an untracked file, or outside a repo — markers are
    /// an enhancement, never a requirement.
    pub changes: Vec<crate::git::local::ChangeSpan>,
    /// Which scheduled read this view is waiting for (the `request_token` idea
    /// `DiffView` already uses). Every read carries the token it was issued
    /// with, and only a match may be applied, so a slow read cannot land after
    /// a newer one — including a re-read of the *same* file, which a path check
    /// alone cannot tell apart. `0` means no read has been scheduled yet, so no
    /// event can ever match it.
    pub read_token: u64,
}

impl FileView {
    /// The change marker for **1-based** file line `line`, if any.
    ///
    /// Binary search over the sorted spans: the render path calls this once per
    /// visible row, so it must not scan (docs/41 — O(visible), never O(file)).
    pub fn change_at(&self, line: usize) -> Option<crate::git::local::ChangeKind> {
        let line = line as u32;
        let i = self
            .changes
            .partition_point(|s| s.end <= line)
            .min(self.changes.len().saturating_sub(1));
        let s = self.changes.get(i)?;
        (line >= s.start && line < s.end).then_some(s.kind)
    }

    pub fn new(path: PathBuf) -> Self {
        FileView {
            path,
            load: FileLoad::Loading,
            scroll: 0,
            hscroll: 0,
            // Soft-wrap on by default: a reader should never hide content off the
            // right edge. `w` toggles to no-wrap + horizontal scroll for code.
            wrap: true,
            mtime: None,
            changes: Vec::new(),
            search: None,
            read_token: 0,
        }
    }

    /// Fold a finished read in. Scroll is **kept** (clamped to the new content),
    /// so a live refresh (docs/38 FILE-5) doesn't yank the reader back to the
    /// top; a fresh open already has scroll at 0. An active search is
    /// re-evaluated against the new text.
    pub fn apply(&mut self, load: FileLoad) {
        self.load = load;
        let max = self.line_count().saturating_sub(1);
        self.scroll = self.scroll.min(max);
        self.hscroll = 0;
        if let Some(s) = self.search.take() {
            if !s.query.is_empty() {
                self.run_search(&s.query);
            }
        }
    }

    pub fn line_count(&self) -> usize {
        match &self.load {
            FileLoad::Text(lines) => lines.len(),
            _ => 0,
        }
    }

    /// The largest `scroll` that still fills the viewport — i.e. the top line of
    /// the last page.
    ///
    /// Without wrapping this is just `lines - viewport`, one file line per row.
    /// **With** wrapping a single file line can occupy many rows, so the
    /// line-based form is badly wrong: a 44-line changelog whose paragraphs are
    /// 1,500 characters each fills hundreds of rows, but `44 - viewport` pinned
    /// the view a few lines from the top and the rest was unreachable. Walk back
    /// from the end accumulating real screen rows instead.
    ///
    /// `text_w` is the text column width; 0 means "unknown", which falls back to
    /// the line-based clamp rather than guessing.
    pub fn last_top(&self, viewport: usize, text_w: usize) -> usize {
        let lines = match &self.load {
            FileLoad::Text(l) => l,
            _ => return 0,
        };
        let viewport = viewport.max(1);
        if !self.wrap || text_w == 0 {
            return lines.len().saturating_sub(viewport);
        }
        let mut rows = 0usize;
        for (i, line) in lines.iter().enumerate().rev() {
            rows += wrap_rows(line, text_w);
            if rows >= viewport {
                return i;
            }
        }
        0
    }

    /// Scroll vertically by `delta` lines, clamped so at least one line stays on
    /// screen. `viewport` is the number of text rows currently visible and
    /// `text_w` the text column width (needed to measure wrapped lines).
    pub fn scroll_by(&mut self, delta: i32, viewport: usize, text_w: usize) {
        let max = self.line_count().saturating_sub(1);
        let next = (self.scroll as i32 + delta).clamp(0, max as i32) as usize;
        self.scroll = next;
        // Also clamp so the last page doesn't scroll into empty space.
        let last_top = self.last_top(viewport, text_w);
        if self.scroll > last_top {
            self.scroll = last_top;
        }
    }

    pub fn goto_top(&mut self) {
        self.scroll = 0;
    }

    pub fn goto_bottom(&mut self, viewport: usize, text_w: usize) {
        self.scroll = self.last_top(viewport, text_w);
    }

    pub fn scroll_right(&mut self, delta: i16) {
        if self.wrap {
            return;
        }
        self.hscroll = (self.hscroll as i16 + delta).max(0) as u16;
    }

    // ── search (docs/38 FILE-6) ──────────────────────────────────────────────

    /// Begin typing a query.
    pub fn search_begin(&mut self) {
        self.search = Some(Search {
            editing: true,
            ..Default::default()
        });
    }

    /// A char typed into the active query.
    pub fn search_push(&mut self, c: char) {
        if let Some(s) = self.search.as_mut().filter(|s| s.editing) {
            s.query.push(c);
        }
    }

    /// Backspace in the active query.
    pub fn search_backspace(&mut self) {
        if let Some(s) = self.search.as_mut().filter(|s| s.editing) {
            s.query.pop();
        }
    }

    /// Commit the query: compute matches and jump to the first at/after the
    /// current scroll position.
    pub fn search_commit(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            return;
        };
        if query.is_empty() {
            self.search = None;
            return;
        }
        self.run_search(&query);
    }

    /// Cancel search entirely.
    pub fn search_cancel(&mut self) {
        self.search = None;
    }

    /// Step to the next (`forward`) / previous match, wrapping, and scroll it
    /// into view.
    pub fn search_step(&mut self, forward: bool, viewport: usize) {
        let (len, next) = match self.search.as_ref() {
            Some(s) if !s.matches.is_empty() => {
                let n = s.matches.len();
                let cur = s.current;
                (
                    n,
                    if forward {
                        (cur + 1) % n
                    } else {
                        (cur + n - 1) % n
                    },
                )
            }
            _ => return,
        };
        if let Some(s) = self.search.as_mut() {
            s.current = next;
        }
        let _ = len;
        self.reveal_current_match(viewport);
    }

    fn run_search(&mut self, query: &str) {
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        if let FileLoad::Text(lines) = &self.load {
            for (li, line) in lines.iter().enumerate() {
                let hay = line.to_lowercase();
                let mut from = 0;
                while let Some(rel) = hay[from..].find(&needle) {
                    let col = from + rel;
                    matches.push((li, col));
                    from = col + needle.len().max(1);
                }
            }
        }
        // Jump to the first match at/after the current viewport top.
        let current = matches
            .iter()
            .position(|(l, _)| *l >= self.scroll)
            .unwrap_or(0);
        self.search = Some(Search {
            query: query.to_string(),
            editing: false,
            matches,
            current,
        });
    }

    fn reveal_current_match(&mut self, viewport: usize) {
        if let Some(s) = &self.search {
            if let Some((line, _)) = s.matches.get(s.current).copied() {
                // Center-ish: keep the match on screen.
                if line < self.scroll || line >= self.scroll + viewport.max(1) {
                    self.scroll = line.saturating_sub(viewport / 2);
                }
            }
        }
    }
}

/// The text column width inside a file view `pane_width` columns wide — the
/// renderer's `text_w`. One definition, used by the renderer's layout and by the
/// scroll clamp, so the clamp can never measure wrapping against a width the
/// view was not actually drawn at.
pub fn view_text_w(v: &FileView, pane_width: u16) -> usize {
    let g = gutter_width(v.line_count());
    pane_width.saturating_sub(g + 1) as usize
}

/// The line-number gutter width for a file of `line_count` lines. Shared by the
/// renderer and mouse-selection extraction so their column math agrees.
pub fn gutter_width(line_count: usize) -> u16 {
    (line_count.max(1).to_string().len() as u16 + 1).max(4)
}

/// Character ranges `(start, end)` of each visual segment when `line` is
/// soft-wrapped to `width` columns. Breaks on the last space inside the window
/// when there is one (word wrap), else hard-splits at the width. Always returns
/// at least one range, so an empty line still occupies a row. Shared by the
/// renderer and mouse-selection so a wrapped view maps screen rows to file
/// columns identically in both.
pub fn wrap_ranges(line: &str, width: usize) -> Vec<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    if width == 0 || n <= width {
        return vec![(0, n)];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < n {
        if n - start <= width {
            out.push((start, n));
            break;
        }
        let hard_end = start + width;
        let mut brk = hard_end;
        // Prefer a word boundary: the last space in the window, if it isn't the
        // very first column (which would make an empty segment).
        if let Some(pos) = chars[start..hard_end].iter().rposition(|&c| c == ' ') {
            let abs = start + pos;
            if abs > start {
                brk = abs;
            }
        }
        out.push((start, brk));
        // Swallow the space we broke on so it doesn't lead the next row.
        start = if brk < n && chars[brk] == ' ' {
            brk + 1
        } else {
            brk
        };
    }
    if out.is_empty() {
        out.push((0, n));
    }
    out
}

/// How many screen rows `line` occupies when soft-wrapped to `width`.
///
/// Must agree exactly with `wrap_ranges(..).len()` — the renderer lays rows out
/// with that, and the scroll clamp counts them with this, so a disagreement
/// would let the view scroll past its own last row (or stop short of it). Pinned
/// by `wrap_rows_matches_wrap_ranges`. Counts without allocating, because the
/// clamp runs on every keypress and wheel tick.
pub fn wrap_rows(line: &str, width: usize) -> usize {
    let n = line.chars().count();
    if width == 0 || n <= width {
        return 1;
    }
    let chars: Vec<char> = line.chars().collect();
    let mut rows = 0usize;
    let mut start = 0usize;
    while start < n {
        rows += 1;
        if n - start <= width {
            break;
        }
        let hard_end = start + width;
        let mut brk = hard_end;
        if let Some(pos) = chars[start..hard_end].iter().rposition(|&c| c == ' ') {
            let abs = start + pos;
            if abs > start {
                brk = abs;
            }
        }
        start = if brk < n && chars[brk] == ' ' {
            brk + 1
        } else {
            brk
        };
    }
    rows.max(1)
}

/// Slice the `(start, end)` char range out of `line`.
pub fn seg_text(line: &str, range: (usize, usize)) -> String {
    line.chars().skip(range.0).take(range.1 - range.0).collect()
}

/// Return the rows currently rendered by a native file view, aligned to its
/// complete pane-content rectangle. The line-number gutter is represented by
/// spaces so mouse cell coordinates continue to address the source text. This
/// projection is built only for a double-click token lookup.
pub fn token_rows(
    v: &FileView,
    content: ratatui::layout::Rect,
    mobile: bool,
) -> Option<Vec<String>> {
    let FileLoad::Text(lines) = &v.load else {
        return None;
    };
    let show_footer = !mobile || v.search.is_some();
    let body_rows = content.height.saturating_sub(u16::from(show_footer)) as usize;
    let gutter = gutter_width(lines.len());
    let prefix = " ".repeat((gutter + 1) as usize);
    let text_width = content.width.saturating_sub(gutter + 1) as usize;
    if text_width == 0 {
        return None;
    }

    let mut rows = Vec::with_capacity(body_rows);
    if v.wrap {
        'lines: for line in lines.iter().skip(v.scroll) {
            for range in wrap_ranges(line, text_width) {
                rows.push(format!("{prefix}{}", seg_text(line, range)));
                if rows.len() >= body_rows {
                    break 'lines;
                }
            }
        }
    } else {
        rows.extend(lines.iter().skip(v.scroll).take(body_rows).map(|line| {
            let visible: String = line
                .chars()
                .skip(v.hscroll as usize)
                .take(text_width)
                .collect();
            format!("{prefix}{visible}")
        }));
    }
    Some(rows)
}

/// Extract the text under a mouse selection over a file view (docs/38), so
/// drag-to-copy works like a pane. `content` is the view's content rect and
/// `((sx,sy),(ex,ey))` the selection in reading order (terminal cells).
///
/// Maps each selected screen row to a file line (via `scroll`) and each column
/// past the line-number gutter to a text column (via `hscroll`). Soft-wrap makes
/// the row→line mapping non-linear, so a wrapped view copies whole lines in the
/// row range rather than a precise sub-range.
pub fn selection_text(
    v: &FileView,
    content: ratatui::layout::Rect,
    ordered: ((u16, u16), (u16, u16)),
) -> Option<String> {
    let FileLoad::Text(lines) = &v.load else {
        return None;
    };
    let ((sx, sy), (ex, ey)) = ordered;
    let gutter = gutter_width(lines.len());
    let text_x = content.x + gutter + 1;

    // Build the same screen-row → (file line, segment char range) map the
    // renderer draws, so a drag maps to the right columns in both wrap modes.
    // No-wrap is one full-width segment per line (with horizontal scroll);
    // wrap breaks each line into its visual segments.
    let text_w = content.width.saturating_sub(gutter + 1) as usize;
    let rows = content.height as usize;
    let mut rowmap: Vec<(usize, usize, usize)> = Vec::new(); // (line, seg_start, seg_end)
    let mut li = v.scroll;
    'build: while li < lines.len() {
        if v.wrap {
            for (s, e) in wrap_ranges(&lines[li], text_w) {
                rowmap.push((li, s, e));
                if rowmap.len() >= rows {
                    break 'build;
                }
            }
        } else {
            let n = lines[li].chars().count();
            rowmap.push((li, 0, n));
            if rowmap.len() >= rows {
                break 'build;
            }
        }
        li += 1;
    }

    let mut out = String::new();
    let mut previous: Option<(usize, usize)> = None;
    for ty in sy..=ey {
        let vi = (ty.saturating_sub(content.y)) as usize;
        let Some(&(line, seg_s, seg_e)) = rowmap.get(vi) else {
            continue;
        };
        let chars: Vec<char> = lines
            .get(line)
            .map(|l| l.chars().collect())
            .unwrap_or_default();
        // Screen column → char within this segment. No-wrap adds horizontal scroll.
        let to_col = |screen_x: u16| {
            (screen_x.saturating_sub(text_x)) as usize + if v.wrap { 0 } else { v.hscroll as usize }
        };
        let start = seg_s + if ty == sy { to_col(sx) } else { 0 };
        let end = if ty == ey {
            seg_s + to_col(ex) + 1
        } else {
            seg_e
        };
        let (start, end) = (
            start.min(seg_e).min(chars.len()),
            end.min(seg_e).min(chars.len()),
        );
        let seg: String = if start < end {
            chars[start..end].iter().collect()
        } else {
            String::new()
        };
        if let Some((previous_line, previous_end)) = previous {
            if previous_line == line {
                out.extend(chars[previous_end.min(chars.len())..start].iter().copied());
            } else {
                out.push('\n');
            }
        }
        out.push_str(&seg);
        previous = Some((line, end));
    }
    let out = out.trim_end_matches('\n').to_string();
    (!out.is_empty()).then_some(out)
}

/// Read `path` off the loop into a [`FileLoad`]. Never panics: a missing file,
/// permission error, oversize file, or binary content each becomes a variant.
pub fn read_file(path: &Path) -> FileLoad {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return FileLoad::Error(e.to_string()),
    };
    if meta.len() > SIZE_CAP {
        return FileLoad::TooLarge(meta.len());
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return FileLoad::Error(e.to_string()),
    };
    if bytes.iter().take(SNIFF).any(|&b| b == 0) {
        return FileLoad::Binary(meta.len());
    }
    // Lossy UTF-8, split on \n, strip a trailing \r, expand tabs to 4 columns so
    // horizontal scroll and width math stay simple.
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).replace('\t', "    "))
        .collect();
    // A trailing newline yields a final empty element; drop it so the line count
    // matches what an editor shows.
    let lines = if lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
        lines[..lines.len() - 1].to_vec()
    } else {
        lines
    };
    FileLoad::Text(lines)
}

#[cfg(test)]
mod tests {

    /// `change_at` is the render hot path (once per visible row), so it binary
    /// searches rather than scanning — and it must be exact at span boundaries.
    #[test]
    fn change_at_maps_lines_to_their_span() {
        use crate::git::local::{ChangeKind, ChangeSpan};
        let mut v = FileView::new(PathBuf::from("/x.rs"));
        v.changes = vec![
            ChangeSpan {
                start: 2,
                end: 4,
                kind: ChangeKind::Added,
            },
            ChangeSpan {
                start: 10,
                end: 11,
                kind: ChangeKind::Removed,
            },
            ChangeSpan {
                start: 20,
                end: 23,
                kind: ChangeKind::Modified,
            },
        ];
        assert_eq!(v.change_at(1), None, "before the first span");
        assert_eq!(v.change_at(2), Some(ChangeKind::Added), "span start");
        assert_eq!(v.change_at(3), Some(ChangeKind::Added), "inside");
        assert_eq!(v.change_at(4), None, "end is exclusive");
        assert_eq!(v.change_at(9), None, "between spans");
        assert_eq!(v.change_at(10), Some(ChangeKind::Removed));
        assert_eq!(v.change_at(19), None);
        assert_eq!(
            v.change_at(22),
            Some(ChangeKind::Modified),
            "last line inside"
        );
        assert_eq!(v.change_at(23), None, "past the last span");
        assert_eq!(v.change_at(9999), None, "far past the end");
    }

    /// A file with no diff (clean, untracked, or outside a repo) must never mark.
    #[test]
    fn no_changes_means_no_markers() {
        let v = FileView::new(PathBuf::from("/x.rs"));
        assert!(v.changes.is_empty());
        for line in [0usize, 1, 2, 100] {
            assert_eq!(v.change_at(line), None);
        }
    }
    use super::*;

    #[test]
    fn reads_text_binary_and_oversize() {
        let dir = std::env::temp_dir().join(format!("luvus-fv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("t.txt"), b"a\nb\tc\n").unwrap();
        assert_eq!(
            read_file(&dir.join("t.txt")),
            FileLoad::Text(vec!["a".into(), "b    c".into()]),
            "tabs expanded, trailing newline dropped"
        );

        std::fs::write(dir.join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        assert!(matches!(read_file(&dir.join("b.bin")), FileLoad::Binary(4)));

        assert!(matches!(
            read_file(&dir.join("missing")),
            FileLoad::Error(_)
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrap_ranges_word_wraps_and_hard_splits() {
        // Word wrap: breaks on the last space in the window, swallowing it.
        let r = wrap_ranges("the quick brown fox", 10);
        let segs: Vec<String> = r
            .iter()
            .map(|&rg| seg_text("the quick brown fox", rg))
            .collect();
        assert_eq!(segs, vec!["the quick", "brown fox"]);
        // No spaces (e.g. a long token / code): hard split at the width, nothing lost.
        let r = wrap_ranges("abcdefghijk", 4);
        let segs: Vec<String> = r.iter().map(|&rg| seg_text("abcdefghijk", rg)).collect();
        assert_eq!(segs, vec!["abcd", "efgh", "ijk"]);
        assert_eq!(
            segs.concat(),
            "abcdefghijk",
            "every character survives the wrap"
        );
        // Short line and empty line each stay a single row.
        assert_eq!(wrap_ranges("hi", 10), vec![(0, 2)]);
        assert_eq!(wrap_ranges("", 10), vec![(0, 0)]);
    }

    #[test]
    fn wrapped_selection_joins_visual_rows_without_inventing_a_newline() {
        let mut view = FileView::new(PathBuf::from("article.txt"));
        view.apply(FileLoad::Text(vec!["the quick brown fox".into()]));
        let content = ratatui::layout::Rect::new(0, 0, 15, 3);
        let text_x = gutter_width(1) + 1;

        assert_eq!(
            selection_text(&view, content, ((text_x, 0), (text_x + 8, 1))).as_deref(),
            Some("the quick brown fox")
        );
    }

    #[test]
    fn selection_preserves_real_file_line_breaks() {
        let mut view = FileView::new(PathBuf::from("source.txt"));
        view.apply(FileLoad::Text(vec!["alpha".into(), "beta".into()]));
        let content = ratatui::layout::Rect::new(0, 0, 15, 3);
        let text_x = gutter_width(2) + 1;

        assert_eq!(
            selection_text(&view, content, ((text_x, 0), (text_x + 3, 1))).as_deref(),
            Some("alpha\nbeta")
        );
    }

    #[test]
    fn wrap_is_the_default() {
        assert!(
            FileView::new(PathBuf::from("/x")).wrap,
            "a file opens wrapped"
        );
    }

    #[test]
    fn scroll_clamps_to_content() {
        let mut v = FileView::new(PathBuf::from("/x"));
        v.apply(FileLoad::Text((0..10).map(|i| i.to_string()).collect()));
        v.scroll_by(100, 4, 0); // viewport 4 rows, 10 lines → last top is 6
        assert_eq!(v.scroll, 6);
        v.scroll_by(-100, 4, 0);
        assert_eq!(v.scroll, 0);
    }

    /// `wrap_rows` is the scroll clamp's view of how tall a line is and
    /// `wrap_ranges` is the renderer's. If they ever disagree the view scrolls
    /// past its own last row, or stops short of it.
    #[test]
    fn wrap_rows_matches_wrap_ranges() {
        let cases = [
            "",
            "short",
            "exactly ten",
            "a much longer line that will certainly need to wrap several times over",
            "nospacesatallsothisonlyhardsplitsrepeatedlyacrossmanyrows",
            "trailing space ",
            "  leading spaces and then a good deal more text to force wrapping  ",
        ];
        for line in cases {
            for w in [0usize, 1, 2, 5, 10, 11, 40, 200] {
                assert_eq!(
                    wrap_rows(line, w),
                    wrap_ranges(line, w).len(),
                    "line {line:?} at width {w}"
                );
            }
        }
    }

    /// Regression: a file of few but very long lines (a changelog whose
    /// paragraphs are ~1,500 characters) was unscrollable with wrap on. The
    /// clamp measured file lines, so `44 - viewport` pinned the view a few lines
    /// from the top while the wrapped content ran hundreds of rows past it.
    #[test]
    fn wrapped_long_lines_scroll_to_the_end() {
        let long = "word ".repeat(300); // ~1500 chars, like a changelog bullet
        let lines: Vec<String> = (0..44).map(|_| long.clone()).collect();
        let mut v = FileView::new(std::path::PathBuf::from("changelog.md"));
        v.load = FileLoad::Text(lines);
        v.wrap = true;
        let (viewport, text_w) = (40usize, 80usize);

        // Line-based clamp would have stopped here; the real last page is far past it.
        let naive = v.line_count().saturating_sub(viewport);
        let last = v.last_top(viewport, text_w);
        assert!(
            last > naive,
            "wrapped clamp must reach further than the line-based one ({last} vs {naive})"
        );

        v.goto_bottom(viewport, text_w);
        assert_eq!(v.scroll, last, "G lands on the last page");

        // And the last page really is a full screen of rows, not empty space.
        let rows: usize = (v.scroll..v.line_count())
            .map(|i| match &v.load {
                FileLoad::Text(l) => wrap_rows(&l[i], text_w),
                _ => 0,
            })
            .sum();
        assert!(
            rows >= viewport,
            "last page fills the viewport ({rows} rows)"
        );

        // Paging down from the top must be able to reach it.
        v.goto_top();
        for _ in 0..500 {
            v.scroll_by(viewport as i32, viewport, text_w);
        }
        assert_eq!(v.scroll, last, "paging down reaches the end");
    }

    /// Without wrapping the clamp is still the plain line-based one.
    #[test]
    fn unwrapped_clamp_is_line_based() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let mut v = FileView::new(std::path::PathBuf::from("x.rs"));
        v.load = FileLoad::Text(lines);
        v.wrap = false;
        assert_eq!(v.last_top(20, 80), 80);
        v.goto_bottom(20, 80);
        assert_eq!(v.scroll, 80);
    }

    #[test]
    fn search_finds_navigates_and_reveals() {
        let mut v = FileView::new(PathBuf::from("/x"));
        v.apply(FileLoad::Text(vec![
            "let foo = 1;".into(),
            "// nothing here".into(),
            "foo(foo, FOO);".into(), // 3 hits (case-insensitive) on line 2
        ]));
        v.search_begin();
        for c in "foo".chars() {
            v.search_push(c);
        }
        v.search_commit();
        let s = v.search.as_ref().unwrap();
        // 1 on line 0, 3 on line 2 (foo, foo, FOO) = 4 total.
        assert_eq!(s.matches.len(), 4);
        assert_eq!(s.current, 0);

        // Next wraps through all four; N goes back.
        v.search_step(true, 2);
        assert_eq!(v.search.as_ref().unwrap().current, 1);
        v.search_step(false, 2);
        assert_eq!(v.search.as_ref().unwrap().current, 0);

        // Stepping to a match far down scrolls it into view.
        v.goto_top();
        v.search_step(true, 1); // current -> 1, which is on line 2
        assert!(v.scroll >= 1, "the match line was revealed");

        // A refreshed read re-evaluates the query against new text.
        v.apply(FileLoad::Text(vec!["only foo".into()]));
        assert_eq!(v.search.as_ref().unwrap().matches.len(), 1);

        v.search_cancel();
        assert!(v.search.is_none());
    }
}
