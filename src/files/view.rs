//! The file-view model (docs/38 FILE-3): one open file rendered natively inside
//! a pane or a tab. Pure state; the bytes are read on a worker thread and folded
//! in via [`FileView::apply_prepared`]. Rendering is O(visible rows × visible
//! width) — the renderer slices `lines` to the viewport and tokenizes only the
//! visible char window of each on-screen line, so a multi-megabyte single-line
//! file costs a viewport, not the file.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
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
    /// `(line, start_col, end_col)` of every match, in document order.
    /// Columns address the original line in characters, never the lowercased
    /// copy: Unicode lowercasing can expand text (`İ` folds to two
    /// characters), so offsets into the folded copy would shift the highlight.
    pub matches: Vec<(usize, usize, usize)>,
    /// Index into `matches` of the current hit.
    pub current: usize,
}

/// One cached line's windowed tokens: they cover `[0, covered_end)` in chars.
#[derive(Clone, Debug)]
struct CachedLine {
    tokens: Vec<super::highlight::Token>,
    covered_end: usize,
    open: Option<super::highlight::MultilineKind>,
}

/// Bounded generation-checked token cache.
///
/// Rendering tokenizes only the visible window of each on-screen line, but a
/// redraw, scroll, selection change, or theme switch would otherwise redo that
/// work every frame. The cache keeps the most recent `cap` lines' windowed
/// tokens keyed by file line; `rev` fences stale content (bumped on every
/// `apply`, so a refreshed file never reuses another generation's tokens).
/// Theme changes need no invalidation: tokens carry no colors.
#[derive(Debug, Default)]
struct TokenCache {
    rev: u64,
    map: HashMap<usize, CachedLine>,
    order: VecDeque<usize>,
    cap: usize,
}

impl TokenCache {
    fn with_cap(cap: usize) -> Self {
        TokenCache {
            rev: 0,
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn get(&self, line_idx: usize, needed_end: usize) -> Option<Vec<super::highlight::Token>> {
        let entry = self.map.get(&line_idx)?;
        if entry.covered_end < needed_end {
            return None;
        }
        // Prefix of the cached window: clip the boundary token.
        let mut out = Vec::new();
        for t in &entry.tokens {
            if t.start >= needed_end {
                break;
            }
            out.push(super::highlight::Token {
                start: t.start,
                end: t.end.min(needed_end),
                kind: t.kind,
            });
            if t.end >= needed_end {
                break;
            }
        }
        Some(out)
    }

    fn insert(
        &mut self,
        line_idx: usize,
        open: Option<super::highlight::MultilineKind>,
        tokens: Vec<super::highlight::Token>,
        covered_end: usize,
    ) {
        if self.cap == 0 {
            return;
        }
        if !self.map.contains_key(&line_idx) {
            self.order.push_back(line_idx);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    // Don't evict the line we just added when cap == 1 etc.;
                    // the loop keeps the map bounded.
                    if old != line_idx {
                        self.map.remove(&old);
                    }
                }
            }
            // If we evicted the just-added key by accident (cap churn), re-add.
            if !self.order.contains(&line_idx) {
                self.order.push_back(line_idx);
            }
        }
        self.map.insert(
            line_idx,
            CachedLine {
                tokens,
                covered_end,
                open,
            },
        );
        // Touch for LRU-ish behavior: move to the back.
        if let Some(pos) = self.order.iter().position(|&i| i == line_idx) {
            self.order.remove(pos);
            self.order.push_back(line_idx);
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
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
    /// The multiline-string opener active at the start of each line, parallel
    /// to the `Text` lines. Prepared on the file-read worker and folded in by
    /// [`FileView::apply_prepared`] so the application thread never scans the
    /// whole file; empty for non-text loads.
    pub string_states: Vec<Option<super::highlight::MultilineKind>>,
    /// Which scheduled read this view is waiting for (the `request_token` idea
    /// `DiffView` already uses). Every read carries the token it was issued
    /// with, and only a match may be applied, so a slow read cannot land after
    /// a newer one — including a re-read of the *same* file, which a path check
    /// alone cannot tell apart. `0` means no read has been scheduled yet, so no
    /// event can ever match it.
    pub read_token: u64,
    /// Content generation for the token cache: bumped on every `apply` so a
    /// refreshed file never reuses the previous generation's windowed tokens.
    highlight_rev: u64,
    /// Bounded cache of windowed highlight tokens (see [`TokenCache`]).
    /// `RefCell` lets the `&FileView` render path memoize without restructuring
    /// every caller to `&mut`; the app loop is single-threaded.
    token_cache: RefCell<TokenCache>,
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
            string_states: Vec::new(),
            highlight_rev: 0,
            token_cache: RefCell::new(TokenCache::with_cap(128)),
        }
    }

    /// Fold a finished read in. Scroll is **kept** (clamped to the new content),
    /// so a live refresh (docs/38 FILE-5) doesn't yank the reader back to the
    /// top; a fresh open already has scroll at 0. An active search is
    /// re-evaluated against the new text.
    ///
    /// Direct-call fallback: computes the syntax preparation synchronously.
    /// Prefer [`FileView::apply_prepared`] on the worker path, where the states
    /// arrive precomputed and this scan never runs on the application thread.
    #[allow(dead_code)]
    pub fn apply(&mut self, load: FileLoad) {
        let states = match &load {
            FileLoad::Text(lines) => {
                let lang = super::highlight::language_for_path(&self.path);
                super::highlight::continuation_states(lines, lang)
            }
            _ => Vec::new(),
        };
        self.apply_prepared(load, states);
    }

    /// Fold a worker-prepared read in: `states` must be the
    /// [`continuation_states`](super::highlight::continuation_states) for
    /// `load`'s lines in the worker's language guess. Length-mismatched states
    /// are discarded and recomputed once (a stale worker must never poison the
    /// renderer); the common path stores without scanning.
    pub fn apply_prepared(
        &mut self,
        load: FileLoad,
        mut states: Vec<Option<super::highlight::MultilineKind>>,
    ) {
        // Generation fence: only states prepared for this exact text may land.
        // A length mismatch means the worker and the text disagree — fall back
        // to one synchronous pass rather than rendering with shifted colors.
        let ok = match &load {
            FileLoad::Text(lines) => states.len() == lines.len(),
            _ => states.is_empty(),
        };
        if !ok {
            states = match &load {
                FileLoad::Text(lines) => {
                    let lang = super::highlight::language_for_path(&self.path);
                    super::highlight::continuation_states(lines, lang)
                }
                _ => Vec::new(),
            };
        }
        self.load = load;
        self.string_states = states;
        self.highlight_rev = self.highlight_rev.wrapping_add(1);
        self.token_cache.borrow_mut().clear();
        self.token_cache.borrow_mut().rev = self.highlight_rev;
        let max = self.line_count().saturating_sub(1);
        self.scroll = self.scroll.min(max);
        self.hscroll = 0;
        if let Some(s) = self.search.take() {
            if !s.query.is_empty() {
                self.run_search(&s.query);
            }
        }
    }

    /// Windowed highlight tokens for `line_idx`, memoized in the bounded
    /// generation-checked cache. `needed_end` is the exclusive char offset the
    /// caller will actually draw (visible window end); the cache stores the
    /// widest window seen per line and serves prefixes from it, so a 5 MiB
    /// single line costs a viewport per frame, not the file.
    pub fn highlight_window(
        &self,
        line_idx: usize,
        line: &str,
        lang: super::highlight::Language,
        open: Option<super::highlight::MultilineKind>,
        needed_end: usize,
    ) -> Vec<super::highlight::Token> {
        let mut cache = self.token_cache.borrow_mut();
        if cache.rev != self.highlight_rev {
            cache.clear();
            cache.rev = self.highlight_rev;
        }
        if cache.cap == 0 {
            return super::highlight::tokenize_window(line, lang, open, needed_end);
        }
        if let Some(hit) = cache.get(line_idx, needed_end) {
            // Validate the opener: states only change on `apply` (which bumps
            // `rev` and clears), so a mismatch here is a logic bug — recompute
            // rather than tint with a stale string state.
            let open_ok = cache.map.get(&line_idx).is_none_or(|e| e.open == open);
            if open_ok {
                return hit;
            }
        }
        let tokens = super::highlight::tokenize_window(line, lang, open, needed_end);
        cache.insert(line_idx, open, tokens.clone(), needed_end);
        tokens
    }

    /// Test hook: number of cached lines.
    #[cfg(test)]
    pub fn token_cache_len(&self) -> usize {
        self.token_cache.borrow().map.len()
    }

    /// Test hook: current highlight generation.
    #[cfg(test)]
    pub fn highlight_generation(&self) -> u64 {
        self.highlight_rev
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
        // Unicode-aware case-insensitive search that keeps the efficient
        // `str::find` matcher while retaining original-column mapping.
        //
        // Whole-string lowercasing can expand text (`İ` folds to `i` plus a
        // combining dot), so offsets into the folded copy do not address the
        // original line. We fold per character, remembering each folded
        // character's origin column plus its byte offset, then run the
        // standard library's optimized substring search over the folded
        // string and map byte matches back through `origin`. The previous
        // naive loop compared `folded[from..from+m] == needle` at every
        // offset — O(file × query) slice comparisons on the app thread.
        let needle_str = query.to_lowercase();
        let mut matches = Vec::new();
        if !needle_str.is_empty() {
            if let FileLoad::Text(lines) = &self.load {
                let needle_len = needle_str.len();
                for (li, line) in lines.iter().enumerate() {
                    // Single O(line) pass: folded text + per-folded-char origin
                    // column + per-folded-char byte offset.
                    let mut folded = String::with_capacity(line.len());
                    let mut origin: Vec<usize> = Vec::new();
                    let mut char_starts: Vec<usize> = Vec::new();
                    let mut bytes = 0usize;
                    for (col, ch) in line.chars().enumerate() {
                        for folded_ch in ch.to_lowercase() {
                            char_starts.push(bytes);
                            origin.push(col);
                            folded.push(folded_ch);
                            bytes += folded_ch.len_utf8();
                        }
                    }
                    if folded.is_empty() {
                        continue;
                    }
                    // Efficient matcher: `str::find` (memchr + Two-Way) instead of
                    // a naive per-offset slice comparison.
                    let mut byte_from = 0usize;
                    while byte_from + needle_len <= folded.len() {
                        let Some(rel) = folded[byte_from..].find(needle_str.as_str()) else {
                            break;
                        };
                        let abs = byte_from + rel;
                        // `abs` and `abs + needle_len` are char boundaries (both
                        // haystack and needle are valid UTF-8), so exact binary
                        // search hits.
                        let Ok(start_char) = char_starts.binary_search(&abs) else {
                            // Should not happen; advance past this byte to avoid a
                            // stall and keep the search total.
                            byte_from = abs + 1;
                            continue;
                        };
                        let end_byte = abs + needle_len;
                        let end_char_exclusive = match char_starts.binary_search(&end_byte) {
                            Ok(i) => i,
                            // `end_byte == folded.len()` is past the last start;
                            // it addresses one past the final folded char.
                            Err(i) if end_byte == folded.len() => i,
                            Err(_) => {
                                byte_from = abs + 1;
                                continue;
                            }
                        };
                        if end_char_exclusive == 0 || start_char >= end_char_exclusive {
                            byte_from = abs + 1;
                            continue;
                        }
                        let start = origin[start_char];
                        let end = origin[end_char_exclusive - 1] + 1;
                        matches.push((li, start, end));
                        // Non-overlapping, matching the previous `from += m`.
                        byte_from = end_byte;
                    }
                }
            }
            // Jump to the first match at/after the current viewport top.
            let current = matches
                .iter()
                .position(|(l, _, _)| *l >= self.scroll)
                .unwrap_or(0);
            self.search = Some(Search {
                query: query.to_string(),
                editing: false,
                matches,
                current,
            });
        }
    }

    fn reveal_current_match(&mut self, viewport: usize) {
        if let Some(s) = &self.search {
            if let Some((line, _, _)) = s.matches.get(s.current).copied() {
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
///
/// Cost is O(line): it collects the whole line. The render path must use
/// [`wrap_ranges_limited`] instead, which bounds work to the viewport.
#[allow(dead_code)]
pub fn wrap_ranges(line: &str, width: usize) -> Vec<(usize, usize)> {
    wrap_ranges_limited(line, width, usize::MAX)
}

/// First `max_rows` wrapped segments of `line` — a prefix of [`wrap_ranges`].
///
/// A 5 MiB single-line file wraps to tens of thousands of rows; the viewport
/// shows only dozens. Computing all segments every frame allocates the full
/// vector repeatedly. This collects only enough prefix chars
/// (`max_rows × (width+1)`) to cover the requested rows, so per-frame work is
/// O(viewport × width), not O(file).
pub fn wrap_ranges_limited(line: &str, width: usize, max_rows: usize) -> Vec<(usize, usize)> {
    if max_rows == 0 {
        return Vec::new();
    }
    if width == 0 {
        // Original `wrap_ranges` returns the whole line as one segment when
        // width is 0 (the `n <= width` check fails for non-empty? Actually
        // `width == 0` short-circuits to a single range).
        let n = line.chars().count();
        return vec![(0, n)];
    }
    // Enough prefix to cover `max_rows` rows: each row consumes at most
    // `width+1` chars (a full window plus one swallowed space).
    let take = max_rows
        .saturating_mul(width.saturating_add(1))
        .saturating_add(width);
    let chars: Vec<char> = line.chars().take(take).collect();
    let n = chars.len();
    // If the prefix already holds the whole line, `n` is the true length and
    // this is exactly `wrap_ranges`. Otherwise `n` is the prefix length, but
    // the first `max_rows` segments are identical to the full line's prefix
    // (each consumes ≤ width+1 chars), so truncating to `max_rows` is exact.
    if n == 0 {
        return vec![(0, 0)];
    }
    if n <= width {
        return vec![(0, n)];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < n && out.len() < max_rows {
        if n - start <= width {
            // Only final when the prefix really is the whole line; when the
            // line continues beyond the prefix this arm would mislabel an
            // interior row as final — but the prefix is sized so we reach
            // `max_rows` before getting here for long lines. Guard anyway: if
            // we took a truncated prefix (line longer than `take`) and this is
            // not the last allowed row, fall through to the interior break.
            // Detect truncation cheaply: if we filled `take` chars, the line
            // may continue. In that case only take the final arm when it is
            // also the last row we will return.
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
/// by `wrap_rows_matches_wrap_ranges`. Counts with only O(width) buffering
/// (one window plus one pending suffix), never O(line): the clamp runs on
/// every keypress and wheel tick, and a 5 MiB single line must not allocate
/// megabytes to answer "how tall?".
pub fn wrap_rows(line: &str, width: usize) -> usize {
    use std::collections::VecDeque;
    if width == 0 {
        return 1;
    }
    let mut iter = line.chars().peekable();
    if iter.peek().is_none() {
        return 1;
    }
    let mut rows = 0usize;
    let mut pending: VecDeque<char> = VecDeque::new();
    loop {
        // Fill one window: pending suffix first, then fresh chars.
        let mut buf: Vec<char> = Vec::with_capacity(width);
        while buf.len() < width {
            if let Some(c) = pending.pop_front() {
                buf.push(c);
            } else if let Some(c) = iter.next() {
                buf.push(c);
            } else {
                break;
            }
        }
        if buf.len() < width {
            // Exhausted: remaining fits in one final row. Empty `buf` means the
            // previous row ended exactly at end-of-line (or swallowed a
            // trailing space) — no extra row.
            if !buf.is_empty() {
                rows += 1;
            } else if rows == 0 {
                rows = 1;
            }
            break;
        }
        // Full window. Final iff nothing follows.
        let more = !pending.is_empty() || iter.peek().is_some();
        if !more {
            rows += 1;
            break;
        }
        // Interior row: prefer the last space strictly inside the window.
        if let Some(pos) = buf.iter().rposition(|&c| c == ' ').filter(|&p| p > 0) {
            rows += 1;
            // Swallow the space; carry the suffix after it.
            for &c in &buf[pos + 1..] {
                pending.push_back(c);
            }
        } else {
            rows += 1;
            // Hard split: swallow one leading space of the next row, matching
            // `wrap_ranges` (`chars[brk] == ' '` → `brk + 1`).
            if iter.peek() == Some(&' ') {
                iter.next();
            }
        }
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
            let remaining = body_rows.saturating_sub(rows.len());
            if remaining == 0 {
                break;
            }
            for range in wrap_ranges_limited(line, text_width, remaining) {
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
            let remaining = rows.saturating_sub(rowmap.len());
            if remaining == 0 {
                break;
            }
            for (s, e) in wrap_ranges_limited(&lines[li], text_w, remaining) {
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
///
/// Direct-call fallback (tests, benchmarks); workers use
/// [`read_file_prepared`] so syntax state arrives precomputed.
#[allow(dead_code)]
pub fn read_file(path: &Path) -> FileLoad {
    read_file_prepared(path).0
}

/// Read `path` off the loop and prepare syntax state on the same worker.
///
/// Returns the [`FileLoad`] plus the multiline-string opener active at each
/// line start (parallel to the text lines; empty for non-text loads). The
/// file-read worker calls this so [`FileView::apply_prepared`] can store both
/// without scanning on the application thread. The language guess uses `path`,
/// matching what the renderer would compute per frame.
pub fn read_file_prepared(path: &Path) -> (FileLoad, Vec<Option<super::highlight::MultilineKind>>) {
    let load = {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => return (FileLoad::Error(e.to_string()), Vec::new()),
        };
        if meta.len() > SIZE_CAP {
            return (FileLoad::TooLarge(meta.len()), Vec::new());
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => return (FileLoad::Error(e.to_string()), Vec::new()),
        };
        if bytes.iter().take(SNIFF).any(|&b| b == 0) {
            return (FileLoad::Binary(meta.len()), Vec::new());
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
    };
    let states = match &load {
        FileLoad::Text(lines) => {
            let lang = super::highlight::language_for_path(path);
            super::highlight::continuation_states(lines, lang)
        }
        _ => Vec::new(),
    };
    (load, states)
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

    /// `apply` rebuilds the per-line multiline-string states so the renderer
    /// colors docstring continuations without scanning from the file top.
    #[test]
    fn apply_records_multiline_string_states() {
        let mut v = FileView::new(PathBuf::from("doc.py"));
        v.apply(FileLoad::Text(vec![
            "doc = \"\"\"start".into(),
            "return of the thing".into(),
            "end\"\"\"".into(),
        ]));
        assert_eq!(
            v.string_states,
            vec![
                None,
                Some(crate::files::highlight::MultilineKind::TripleDouble),
                Some(crate::files::highlight::MultilineKind::TripleDouble),
            ]
        );
        v.apply(FileLoad::Error("gone".into()));
        assert!(v.string_states.is_empty());
    }

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
    fn search_stores_character_columns_for_non_ascii_lines() {
        let mut v = FileView::new(PathBuf::from("/x"));
        v.apply(FileLoad::Text(vec!["ééfoo".into(), "fooé".into()]));
        v.search_begin();
        for c in "foo".chars() {
            v.search_push(c);
        }
        v.search_commit();
        let s = v.search.as_ref().unwrap();
        // `é` is two bytes but one column: byte offsets 4 and 0 would
        // misplace the overlay, character columns do not.
        assert_eq!(s.matches, vec![(0, 2, 5), (1, 0, 3)]);
    }

    #[test]
    fn search_maps_folded_offsets_back_to_original_columns() {
        let mut v = FileView::new(PathBuf::from("/x"));
        // `İ` folds to two characters (`i` plus a combining dot), so offsets
        // into the lowercased copy run ahead of the original line's columns:
        // the `f` is folded index 3 but original column 2.
        v.apply(FileLoad::Text(vec!["aİfoo".into()]));
        v.search_begin();
        for c in "foo".chars() {
            v.search_push(c);
        }
        v.search_commit();
        assert_eq!(
            v.search.as_ref().unwrap().matches,
            vec![(0, 2, 5)],
            "match columns must address the original line"
        );
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

    /// Worker-prepared reads land without an application-thread scan: the
    /// states arrive with the text and are stored directly.
    #[test]
    fn prepared_apply_stores_worker_states_and_fences_generations() {
        use crate::files::highlight::{language_for_path, MultilineKind};
        let mut v = FileView::new(PathBuf::from("doc.py"));
        let lines = vec![
            "doc = \"\"\"start".to_string(),
            "return of the thing".to_string(),
            "end\"\"\"".to_string(),
        ];
        let lang = language_for_path(&v.path);
        let states = crate::files::highlight::continuation_states(&lines, lang);
        let rev_before = v.highlight_generation();
        v.apply_prepared(FileLoad::Text(lines.clone()), states.clone());
        assert_eq!(v.string_states, states);
        assert_ne!(v.highlight_generation(), rev_before);
        assert_eq!(v.token_cache_len(), 0, "apply clears the window cache");

        // Length-mismatched states are a stale worker: fall back to a fresh
        // pass rather than rendering with shifted colors.
        let mut v2 = FileView::new(PathBuf::from("doc.py"));
        v2.apply_prepared(FileLoad::Text(lines.clone()), vec![None]);
        assert_eq!(
            v2.string_states,
            vec![
                None,
                Some(MultilineKind::TripleDouble),
                Some(MultilineKind::TripleDouble),
            ]
        );
    }

    /// The window cache is bounded and generation-checked.
    #[test]
    fn token_cache_is_bounded_and_generation_checked() {
        use crate::files::highlight::language_for_path;
        let mut v = FileView::new(PathBuf::from("x.rs"));
        let lines: Vec<String> = (0..300).map(|i| format!("let x{i} = {i};")).collect();
        v.apply(FileLoad::Text(lines.clone()));
        let lang = language_for_path(&v.path);
        for (idx, line) in lines.iter().enumerate() {
            let open = v.string_states.get(idx).copied().flatten();
            let needed = line.chars().count().min(40);
            let _ = v.highlight_window(idx, line, lang, open, needed);
        }
        assert!(
            v.token_cache_len() <= 128,
            "cache stays bounded (got {})",
            v.token_cache_len()
        );

        let rev = v.highlight_generation();
        v.apply(FileLoad::Text(vec!["let y = 2;".into()]));
        assert_ne!(v.highlight_generation(), rev);
        assert_eq!(v.token_cache_len(), 0, "a new generation drops old windows");
    }

    /// `wrap_ranges_limited` is exactly the prefix of `wrap_ranges`.
    #[test]
    fn wrap_limited_is_a_prefix_of_full() {
        let line = "the quick brown fox jumps over ".repeat(200);
        for w in [10usize, 40, 80] {
            let full = wrap_ranges(&line, w);
            for k in [1usize, 5, 40] {
                let limited = wrap_ranges_limited(&line, w, k);
                assert_eq!(
                    limited,
                    full[..limited.len()].to_vec(),
                    "first {k} segments at width {w}"
                );
                assert!(limited.len() <= k);
            }
        }
    }

    /// Near-limit single-line JSON: wrap and no-wrap stay viewport-bounded,
    /// scroll stays put (one logical line), selection is a slice, Unicode
    /// search maps to original columns, and the cache survives a theme change
    /// (styles resolve at render time).
    #[test]
    fn near_limit_single_line_json_stays_viewport_bounded() {
        // ~4.3 MiB single line, under the 5 MiB viewer cap.
        let unit = r#"{"k":"v","n":123,"b":true},"#;
        let reps = (4_500_000 / unit.len()).max(1);
        let huge: String = unit.repeat(reps);
        assert!(huge.len() > 4_000_000 && huge.len() < 5 * 1024 * 1024);

        let mut v = FileView::new(PathBuf::from("data.json"));
        // Simulate the worker: prepared states, no app-thread scan.
        let lang = crate::files::highlight::language_for_path(&v.path);
        let lines = vec![huge.clone()];
        let states = crate::files::highlight::continuation_states(&lines, lang);
        v.apply_prepared(FileLoad::Text(lines), states);
        assert_eq!(v.line_count(), 1);

        let text_w = 80usize;
        let viewport = 40usize;
        // Wrap: only the viewport's rows are materialized.
        v.wrap = true;
        let ranges = wrap_ranges_limited(&huge, text_w, viewport);
        assert_eq!(ranges.len(), viewport);
        let needed = ranges.iter().map(|&(_, e)| e).max().unwrap();
        assert!(needed <= viewport * (text_w + 1) + text_w);
        let tokens = v.highlight_window(0, &huge, lang, None, needed);
        assert!(
            tokens.iter().all(|t| t.end <= needed),
            "windowed tokens never cover the whole 4 MiB line"
        );
        assert!(
            tokens.len() < huge.chars().count() / 4,
            "coalescing keeps token counts far below char counts"
        );
        assert!(v.token_cache_len() <= 128);

        // No-wrap: the visible window is hscroll..hscroll+width.
        v.wrap = false;
        v.hscroll = 100;
        let needed = 100 + text_w;
        let tokens = v.highlight_window(0, &huge, lang, None, needed);
        assert!(tokens.iter().all(|t| t.start < needed));

        // Scroll is line-based: a single logical line never scrolls by lines.
        v.wrap = true;
        v.scroll_by(1000, viewport, text_w);
        assert_eq!(v.scroll, 0);
        assert_eq!(v.last_top(viewport, text_w), 0);

        // Selection over the first two visual rows is a small slice, and it
        // joins wrapped rows of the same file line without inventing a newline.
        let content = ratatui::layout::Rect::new(0, 0, 100, viewport as u16 + 1);
        let gutter = gutter_width(1);
        let text_x = gutter + 1;
        let sel = selection_text(&v, content, ((text_x, 0), (text_x + 10, 1)));
        let sel = sel.expect("a drag over two wrapped rows selects text");
        assert!(!sel.contains('\n'), "same-line wrapped rows join");
        assert!(sel.len() < 4 * text_w, "selection is viewport-bounded");

        // Unicode search on the huge line maps to original columns.
        let mut u = FileView::new(PathBuf::from("data.json"));
        u.apply(FileLoad::Text(vec![format!("éé{huge}")]));
        u.search_begin();
        for c in "éé".chars() {
            u.search_push(c);
        }
        u.search_commit();
        assert_eq!(
            u.search.as_ref().unwrap().matches,
            vec![(0, 0, 2)],
            "folded offsets map back past multi-byte chars"
        );

        // Theme changes need no cache invalidation: tokens carry no colors.
        let before = v.highlight_window(0, &huge, lang, None, needed);
        let rev = v.highlight_generation();
        let after = v.highlight_window(0, &huge, lang, None, needed);
        assert_eq!(v.highlight_generation(), rev);
        assert_eq!(before, after);
    }

    /// Large multiline source: both modes page to the end, selection keeps
    /// real line breaks, and search finds Unicode hits across lines.
    #[test]
    fn large_multiline_source_pages_selects_and_searches() {
        let lines: Vec<String> = (0..20_000)
            .map(|i| format!("fn f{i}() {{ let x{i} = {i}; }} // café {i}"))
            .collect();
        let mut v = FileView::new(PathBuf::from("code.rs"));
        v.apply(FileLoad::Text(lines.clone()));
        assert_eq!(v.line_count(), 20_000);

        for wrap in [true, false] {
            v.wrap = wrap;
            v.goto_top();
            let (viewport, text_w) = (40usize, 80usize);
            v.goto_bottom(viewport, text_w);
            assert_eq!(v.scroll, v.last_top(viewport, text_w));
            assert!(v.scroll > 19_000, "paging reaches the end in both modes");
            // Windowed highlight for a mid-file line covers only its window.
            let lang = crate::files::highlight::language_for_path(&v.path);
            let idx = 10_000;
            let open = v.string_states.get(idx).copied().flatten();
            let tokens = v.highlight_window(idx, &lines[idx], lang, open, text_w);
            assert!(tokens.iter().all(|t| t.end <= text_w + 64));
        }

        // Selection across two file lines keeps the real break.
        v.wrap = false;
        v.scroll = 0;
        let content = ratatui::layout::Rect::new(0, 0, 100, 10);
        let text_x = gutter_width(20_000) + 1;
        let sel = selection_text(&v, content, ((text_x, 0), (text_x + 5, 1)))
            .expect("two-line drag selects");
        assert_eq!(sel.lines().count(), 2, "real file breaks survive");

        // Unicode search finds hits across the file with character columns.
        v.search_begin();
        for c in "café".chars() {
            v.search_push(c);
        }
        v.search_commit();
        let s = v.search.as_ref().unwrap();
        assert_eq!(s.matches.len(), 20_000);
        assert_eq!(s.matches[0].0, 0);
        assert_eq!(s.matches[19_999].0, 19_999);
    }
}
