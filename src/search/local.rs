//! Shared state and Unicode-safe literal matching for pane-local content search.
//!
//! Owners retain their own viewport, cursor, and cancellation semantics. This
//! module only owns bounded query editing, match selection, and text spans.

use std::collections::VecDeque;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::casefold::fold_char;

pub const LOCAL_MATCH_CAP: usize = 10_000;
pub const LOCAL_QUERY_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSearch<M> {
    pub query: String,
    pub editing: bool,
    pub case_sensitive: bool,
    pub matches: Vec<M>,
    pub current: usize,
    pub truncated: bool,
}

impl<M> Default for LocalSearch<M> {
    fn default() -> Self {
        Self {
            query: String::new(),
            editing: false,
            case_sensitive: false,
            matches: Vec::new(),
            current: 0,
            truncated: false,
        }
    }
}

impl<M> LocalSearch<M> {
    pub fn editing() -> Self {
        Self {
            editing: true,
            ..Self::default()
        }
    }

    /// Finish query editing. An empty query cannot become committed.
    pub fn commit(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }
        self.editing = false;
        true
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.editing = true;
        self.matches.clear();
        self.current = 0;
        self.truncated = false;
    }

    pub fn push(&mut self, ch: char) {
        if self.editing && self.query.len().saturating_add(ch.len_utf8()) <= LOCAL_QUERY_BYTES {
            self.query.push(ch);
        }
    }

    pub fn extend_text(&mut self, text: &str) {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            self.push(ch);
        }
    }

    pub fn backspace(&mut self) {
        if self.editing {
            self.query.pop();
        }
    }

    /// Toggle case mode and report whether committed results need rebuilding.
    pub fn toggle_case(&mut self) -> bool {
        self.case_sensitive = !self.case_sensitive;
        !self.editing && !self.query.is_empty()
    }

    pub fn invalidate_matches(&mut self) -> bool {
        if self.editing {
            return false;
        }
        self.editing = true;
        self.matches.clear();
        self.current = 0;
        self.truncated = false;
        true
    }

    pub fn replace_matches(&mut self, matches: Vec<M>, current: usize, truncated: bool) {
        self.matches = matches;
        self.current = if self.matches.is_empty() {
            0
        } else {
            current.min(self.matches.len() - 1)
        };
        self.truncated = truncated;
    }

    pub fn step(&mut self, forward: bool) -> bool {
        if self.editing || self.matches.is_empty() {
            return false;
        }
        let len = self.matches.len();
        self.current = if forward {
            (self.current + 1) % len
        } else {
            (self.current + len - 1) % len
        };
        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextMatch {
    pub byte_start: usize,
    pub byte_end: usize,
    pub column: usize,
    pub width: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RowMatch {
    pub row: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub column: usize,
    pub width: usize,
}

impl RowMatch {
    pub fn at(row: usize, search_match: TextMatch) -> Self {
        Self {
            row,
            byte_start: search_match.byte_start,
            byte_end: search_match.byte_end,
            column: search_match.column,
            width: search_match.width,
        }
    }
}

pub struct LiteralMatcher {
    pattern: Vec<char>,
    failure: Vec<usize>,
    case_sensitive: bool,
}

impl LiteralMatcher {
    pub fn new(query: &str, case_sensitive: bool) -> Option<Self> {
        if query.len() > LOCAL_QUERY_BYTES {
            return None;
        }
        Self::compile(query, case_sensitive)
    }

    fn compile(query: &str, case_sensitive: bool) -> Option<Self> {
        if query.is_empty() {
            return None;
        }
        let pattern = query
            .chars()
            .map(|ch| fold_char(ch, case_sensitive))
            .collect::<Vec<_>>();
        let mut failure = vec![0; pattern.len()];
        let mut prefix = 0usize;
        for index in 1..pattern.len() {
            while prefix > 0 && pattern[index] != pattern[prefix] {
                prefix = failure[prefix - 1];
            }
            if pattern[index] == pattern[prefix] {
                prefix += 1;
                failure[index] = prefix;
            }
        }
        Some(Self {
            pattern,
            failure,
            case_sensitive,
        })
    }

    pub fn has_match(&self, line: &str) -> bool {
        self.raw_spans(line, 0).1
    }

    /// Return the first source UTF-8 byte without computing display geometry.
    pub(crate) fn first_byte_start(&self, line: &str) -> Option<usize> {
        self.raw_spans(line, 1)
            .0
            .into_iter()
            .next()
            .map(|(start, _end)| start)
    }

    /// Find at most `limit` non-overlapping literal matches. Byte ranges stay
    /// exact for FILE/DIFF fragment projection. Cell geometry snaps the start
    /// and end to grapheme boundaries so terminal overlays never split a ZWJ
    /// or combining sequence.
    pub fn spans(&self, line: &str, limit: usize) -> (Vec<TextMatch>, bool) {
        let (raw, truncated) = self.raw_spans(line, limit);
        if raw.is_empty() {
            return (Vec::new(), truncated);
        }

        let mut geometry = vec![None; raw.len()];
        let mut match_index = 0usize;
        let mut column = 0usize;
        for (grapheme_start, grapheme) in UnicodeSegmentation::grapheme_indices(line, true) {
            if match_index >= raw.len() {
                break;
            }
            let grapheme_end = grapheme_start + grapheme.len();
            let next_column = column + UnicodeWidthStr::width(grapheme);
            let mut index = match_index;
            while index < raw.len() && raw[index].0 < grapheme_end {
                if raw[index].1 > grapheme_start {
                    let (start_column, _) = geometry[index].unwrap_or((column, column));
                    geometry[index] = Some((start_column, next_column));
                }
                if raw[index].1 > grapheme_end {
                    break;
                }
                index += 1;
            }
            column = next_column;
            while match_index < raw.len() && raw[match_index].1 <= grapheme_end {
                match_index += 1;
            }
        }

        let matches = raw
            .iter()
            .zip(geometry)
            .filter_map(|(&(byte_start, byte_end), geometry)| {
                let (column, end_column) = geometry?;
                Some(TextMatch {
                    byte_start,
                    byte_end,
                    column,
                    width: end_column.saturating_sub(column).max(1),
                })
            })
            .collect();
        (matches, truncated)
    }

    /// Stream the haystack through KMP so an unusually long line does not need
    /// a second line-sized allocation. The ring holds only enough source byte
    /// ranges to recover the current match.
    fn raw_spans(&self, line: &str, limit: usize) -> (Vec<(usize, usize)>, bool) {
        let mut spans = Vec::with_capacity(limit.min(64));
        let mut source = VecDeque::with_capacity(self.pattern.len());
        let mut matched = 0usize;
        for (byte_start, ch) in line.char_indices() {
            let byte_end = byte_start + ch.len_utf8();
            if source.len() == self.pattern.len() {
                source.pop_front();
            }
            source.push_back((byte_start, byte_end));
            let folded = fold_char(ch, self.case_sensitive);
            while matched > 0 && folded != self.pattern[matched] {
                matched = self.failure[matched - 1];
            }
            if folded == self.pattern[matched] {
                matched += 1;
            }
            if matched == self.pattern.len() {
                if spans.len() == limit {
                    return (spans, true);
                }
                let start = source[source.len() - self.pattern.len()].0;
                spans.push((start, byte_end));
                matched = 0;
                source.clear();
            }
        }
        (spans, false)
    }
}

#[cfg(test)]
/// Convenience wrapper for focused one-line matcher tests. Multi-line owners
/// compile one [`LiteralMatcher`] and reuse it across their bounded scan.
pub fn match_spans(line: &str, query: &str, case_sensitive: bool) -> Vec<TextMatch> {
    LiteralMatcher::new(query, case_sensitive)
        .map(|matcher| matcher.spans(line, LOCAL_MATCH_CAP).0)
        .unwrap_or_default()
}

pub fn first_at_or_after<M>(
    matches: &[M],
    origin: (usize, usize),
    position: impl Fn(&M) -> (usize, usize),
) -> usize {
    matches
        .iter()
        .position(|item| position(item) >= origin)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_matching_reports_utf8_bytes_and_display_cells() {
        let matches = match_spans("a界Foo FOO", "foo", false);

        assert_eq!(matches.len(), 2);
        assert_eq!(
            matches[0],
            TextMatch {
                byte_start: 4,
                byte_end: 7,
                column: 3,
                width: 3,
            }
        );
        assert_eq!(
            &"a界Foo FOO"[matches[1].byte_start..matches[1].byte_end],
            "FOO"
        );
        assert!(match_spans("a界Foo FOO", "foo", true).is_empty());
        assert_eq!(match_spans("a界Foo FOO", "Foo", true).len(), 1);
    }

    #[test]
    fn unicode_case_and_grapheme_cell_spans_are_preserved() {
        assert_eq!(match_spans("ΟΣ", "ος", false).len(), 1);
        assert_eq!(match_spans("σςΣ", "σσσ", false).len(), 1);
        assert!(match_spans("straße", "strasse", false).is_empty());
        assert!(match_spans("ı", "i", false).is_empty());
        assert!(match_spans("i", "ı", false).is_empty());
        assert_eq!(match_spans("ſ", "s", false).len(), 1);
        let found = match_spans("👩‍💻foo", "foo", false)[0];
        assert_eq!(found.column, 2);
        let emoji = match_spans("👩‍💻foo", "👩", false)[0];
        assert_eq!((emoji.byte_start, emoji.byte_end, emoji.width), (0, 4, 2));
    }

    #[test]
    fn matcher_caps_results_and_reports_truncation() {
        let matcher = LiteralMatcher::new("a", true).unwrap();
        let (matches, truncated) = matcher.spans("aaaa", 2);
        assert_eq!(matches.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn query_input_is_bounded_at_a_utf8_boundary() {
        let mut search = LocalSearch::<()>::editing();
        search.extend_text(&"a".repeat(LOCAL_QUERY_BYTES - 1));
        search.push('界');
        assert_eq!(search.query.len(), LOCAL_QUERY_BYTES - 1);
        search.push('a');
        assert_eq!(search.query.len(), LOCAL_QUERY_BYTES);
    }

    #[test]
    fn matching_is_non_overlapping_and_empty_queries_never_match() {
        assert_eq!(match_spans("aaaa", "aa", true).len(), 2);
        assert!(match_spans("anything", "", false).is_empty());
    }

    #[test]
    fn local_state_edits_toggles_case_and_wraps_navigation() {
        let mut search = LocalSearch::editing();
        search.push('x');
        assert!(!search.toggle_case(), "editing has no results to rebuild");
        assert!(search.case_sensitive);
        assert!(search.commit());
        search.replace_matches(vec![10, 20, 30], 1, false);
        assert!(search.step(true));
        assert_eq!(search.current, 2);
        assert!(search.step(true));
        assert_eq!(search.current, 0);
        assert!(search.step(false));
        assert_eq!(search.current, 2);
        assert!(search.toggle_case(), "committed results need rebuilding");
        assert!(search.invalidate_matches());
        assert!(search.editing);
        assert_eq!(search.query, "x");
        assert!(search.matches.is_empty());
        assert!(!search.truncated);
        assert!(
            !search.invalidate_matches(),
            "editing state is already valid"
        );
        search.clear();
        assert!(search.editing);
        assert!(search.query.is_empty());
        assert!(search.matches.is_empty());
    }

    #[test]
    fn first_match_at_origin_wraps_when_origin_is_after_all_matches() {
        let positions = [(1, 2), (3, 4), (5, 6)];
        assert_eq!(first_at_or_after(&positions, (3, 0), |item| *item), 1);
        assert_eq!(first_at_or_after(&positions, (9, 0), |item| *item), 0);
    }
}
