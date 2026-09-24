//! Shared state and Unicode-safe literal matching for pane-local content search.
//!
//! Owners retain their own viewport, cursor, and cancellation semantics. This
//! module only owns query editing, case mode, match selection, and text spans.

use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSearch<M> {
    pub query: String,
    pub editing: bool,
    pub case_sensitive: bool,
    pub matches: Vec<M>,
    pub current: usize,
}

impl<M> Default for LocalSearch<M> {
    fn default() -> Self {
        Self {
            query: String::new(),
            editing: false,
            case_sensitive: false,
            matches: Vec::new(),
            current: 0,
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
    }

    pub fn push(&mut self, ch: char) {
        if self.editing {
            self.query.push(ch);
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
        true
    }

    pub fn replace_matches(&mut self, matches: Vec<M>, current: usize) {
        self.matches = matches;
        self.current = if self.matches.is_empty() {
            0
        } else {
            current.min(self.matches.len() - 1)
        };
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

/// Find non-overlapping literal matches with UTF-8 byte and display-cell spans.
pub fn match_spans(line: &str, query: &str, case_sensitive: bool) -> Vec<TextMatch> {
    let needle: Vec<char> = query.chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    if chars.len() < needle.len() {
        return Vec::new();
    }
    let mut columns = Vec::with_capacity(chars.len() + 1);
    columns.push(0usize);
    for (_, ch) in &chars {
        columns.push(columns.last().copied().unwrap_or(0) + ch.width().unwrap_or(0));
    }
    let mut matches = Vec::new();
    let mut start = 0usize;
    while start + needle.len() <= chars.len() {
        let end = start + needle.len();
        let equal = chars[start..end]
            .iter()
            .map(|(_, ch)| ch)
            .zip(&needle)
            .all(|(hay, wanted)| {
                hay == wanted || (!case_sensitive && hay.to_lowercase().eq(wanted.to_lowercase()))
            });
        if equal {
            let byte_start = chars[start].0;
            let byte_end = chars.get(end).map_or(line.len(), |(offset, _)| *offset);
            matches.push(TextMatch {
                byte_start,
                byte_end,
                column: columns[start],
                width: columns[end].saturating_sub(columns[start]).max(1),
            });
            start = end;
        } else {
            start += 1;
        }
    }
    matches
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
        search.replace_matches(vec![10, 20, 30], 1);
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
