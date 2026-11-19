//! Bounded query text shared by FILES and the sidebar docks.
//!
//! Matching stays with [`FuzzyQuery`]. Callers keep their own commit, cancel,
//! and row-activation rules.

use super::{FuzzyField, FuzzyQuery, PreparedText};

/// FILES and the sidebar docks both cap a query at 256 Unicode scalars.
pub const QUERY_CHAR_CAP: usize = 256;

/// Append non-control characters until the shared character cap.
pub fn append_query(query: &mut String, text: &str) {
    let remaining = QUERY_CHAR_CAP.saturating_sub(query.chars().count());
    if remaining == 0 {
        return;
    }
    query.extend(text.chars().filter(|ch| !ch.is_control()).take(remaining));
}

/// Delete one Unicode scalar, not one byte.
pub fn backspace_query(query: &mut String) {
    query.pop();
}

pub fn clear_query(query: &mut String) {
    query.clear();
}

/// The shared query line. `prompt` is `/` for search or `\` for a hiding filter.
pub fn query_label(prompt: &str, query: &str, status: &str) -> String {
    format!("{prompt}: {query}{status}")
}

/// Locate a match in an already filtered list. `inclusive` keeps the cursor
/// when it already matches. Next and previous wrap inside the matches and
/// return nothing when the list has no match, so the caller does not adopt a
/// different row.
pub fn locate_match(
    len: usize,
    cursor: usize,
    forward: bool,
    inclusive: bool,
    mut matches: impl FnMut(usize) -> bool,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let cursor = cursor.min(len - 1);
    let start = usize::from(!inclusive);
    for step in start..len + start {
        let index = if forward {
            (cursor + step) % len
        } else {
            (cursor + len - step % len) % len
        };
        if matches(index) {
            return Some(index);
        }
    }
    None
}

/// In-memory sidebar query. Empty and whitespace-only queries match everything
/// because [`FuzzyQuery`] has no tokens; FILES keeps its own empty-query worker
/// behavior and does not use this type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DockTextQuery {
    query: String,
    editing: bool,
    checkpoint: String,
}

impl DockTextQuery {
    pub fn editing(&self) -> bool {
        self.editing
    }

    pub fn text(&self) -> &str {
        &self.query
    }

    pub fn active(&self) -> bool {
        !self.fuzzy().is_empty()
    }

    pub fn fuzzy(&self) -> FuzzyQuery {
        FuzzyQuery::new(&self.query, false)
    }

    pub fn begin(&mut self) {
        self.checkpoint.clone_from(&self.query);
        self.editing = true;
    }

    /// Leave editing and keep the typed query.
    pub fn commit(&mut self) {
        self.checkpoint.clone_from(&self.query);
        self.editing = false;
    }

    /// Drop uncommitted typing and leave editing.
    pub fn cancel_edit(&mut self) {
        self.query.clone_from(&self.checkpoint);
        self.editing = false;
    }

    /// Clear a committed query. The next Esc can leave the dock.
    pub fn clear_committed(&mut self) {
        self.query.clear();
        self.checkpoint.clear();
        self.editing = false;
    }

    /// Ctrl+U while editing. Esc still restores the checkpoint.
    pub fn clear_editing(&mut self) {
        self.query.clear();
    }

    pub fn append(&mut self, text: &str) {
        append_query(&mut self.query, text);
    }

    pub fn backspace(&mut self) {
        backspace_query(&mut self.query);
    }
}

pub fn prepared_hit(query: &FuzzyQuery, fields: &[PreparedText]) -> bool {
    if query.is_empty() || fields.is_empty() {
        return query.is_empty();
    }
    let weighted: Vec<FuzzyField<'_>> = fields
        .iter()
        .map(|text| FuzzyField { text, weight: 1 })
        .collect();
    query.score(&weighted).is_some()
}

/// Label text keeps short acronym matches. Absolute paths only count when the
/// hit is exact, a prefix, contiguous, or dense enough to be `output_safe`.
pub fn prepared_mixed_hit(query: &FuzzyQuery, labels: &[&str], paths: &[&str]) -> bool {
    if query.is_empty() {
        return true;
    }
    let labels: Vec<PreparedText> = labels.iter().copied().map(PreparedText::new).collect();
    let paths: Vec<PreparedText> = paths.iter().copied().map(PreparedText::new).collect();
    let mut fields = Vec::with_capacity(labels.len() + paths.len());
    let mut strict = Vec::with_capacity(labels.len() + paths.len());
    for text in &labels {
        fields.push(FuzzyField { text, weight: 1 });
        strict.push(false);
    }
    for text in &paths {
        fields.push(FuzzyField { text, weight: 1 });
        strict.push(true);
    }
    if fields.is_empty() {
        return false;
    }
    query.accepts(&fields, &strict)
}

/// Workspace text match. Name and branch are labels; `cwd` is a strict path.
pub fn prepared_workspace_hit(
    query: &FuzzyQuery,
    name: &str,
    cwd: &str,
    branch: Option<&str>,
) -> bool {
    let mut labels = vec![name];
    if let Some(branch) = branch {
        labels.push(branch);
    }
    prepared_mixed_hit(query, &labels, &[cwd])
}

/// Keep linked-worktree order. A non-matching parent stays only as context for
/// a matching child and keeps its own workspace index.
pub fn project_workspace_order(
    order: &[(usize, bool)],
    matches: impl Fn(usize) -> bool,
) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < order.len() {
        let start = index;
        index += 1;
        while index < order.len() && order[index].1 {
            index += 1;
        }
        let group = &order[start..index];
        if !group.iter().any(|(workspace, _)| matches(*workspace)) {
            continue;
        }
        for &(workspace, nested) in group {
            if !nested || matches(workspace) {
                out.push((workspace, nested));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_edit_is_unicode_capped_and_drops_controls() {
        let mut query = String::new();
        append_query(&mut query, "你\n好\u{0007}");
        assert_eq!(query, "你好");
        backspace_query(&mut query);
        assert_eq!(query, "你");
        let wide = "界".repeat(QUERY_CHAR_CAP);
        append_query(&mut query, &wide);
        assert_eq!(query.chars().count(), QUERY_CHAR_CAP);
        assert!(query.starts_with('你'));
        clear_query(&mut query);
        assert!(query.is_empty());
    }

    #[test]
    fn dock_query_esc_reverts_uncommitted_text_and_clear_is_committed() {
        let mut query = DockTextQuery::default();
        query.begin();
        query.append("alpha");
        query.commit();
        query.begin();
        query.append("-beta");
        query.cancel_edit();
        assert_eq!(query.text(), "alpha");
        assert!(!query.editing());
        query.begin();
        query.clear_editing();
        query.cancel_edit();
        assert_eq!(query.text(), "alpha");
        query.clear_committed();
        assert!(!query.active());
    }

    #[test]
    fn fuzzy_match_preserves_parent_context_without_reordering() {
        let order = vec![(0, false), (1, true), (2, false), (3, true)];
        let names = ["root", "feature", "other", "topic"];
        let query = FuzzyQuery::new("feat", false);
        let prepared: Vec<Vec<PreparedText>> = names
            .iter()
            .map(|name| vec![PreparedText::new(name)])
            .collect();
        let rows = project_workspace_order(&order, |index| prepared_hit(&query, &prepared[index]));
        assert_eq!(rows, vec![(0, false), (1, true)]);
        let none = project_workspace_order(&order, |_| false);
        assert!(none.is_empty());
    }

    #[test]
    fn locate_match_wraps_and_does_not_invent_a_row() {
        let rows = ["alpha", "beta", "alpha-2"];
        let hit = |name: &str| name.starts_with("alpha");
        assert_eq!(
            locate_match(rows.len(), 0, true, true, |i| hit(rows[i])),
            Some(0)
        );
        assert_eq!(
            locate_match(rows.len(), 0, true, false, |i| hit(rows[i])),
            Some(2)
        );
        assert_eq!(
            locate_match(rows.len(), 2, true, false, |i| hit(rows[i])),
            Some(0)
        );
        assert_eq!(
            locate_match(rows.len(), 2, false, false, |i| hit(rows[i])),
            Some(0)
        );
        assert_eq!(
            locate_match(rows.len(), 1, false, false, |i| hit(rows[i])),
            Some(0)
        );
        assert_eq!(locate_match(rows.len(), 0, true, false, |_| false), None);
    }

    #[test]
    fn workspace_path_accepts_later_dense_alignments() {
        let query = FuzzyQuery::new("bld", false);
        for (name, path) in [
            ("app", "/home/bob/projects/build/app"),
            ("service", "/backup/build/service"),
        ] {
            assert!(
                prepared_workspace_hit(&query, name, path, Some("main")),
                "{path}"
            );
        }

        // Three query characters can span six characters, but not seven.
        assert!(prepared_mixed_hit(&query, &[], &["/backup/b12l3d"]));
        assert!(!prepared_mixed_hit(&query, &[], &["/backup/b12l34d"]));
        assert!(
            prepared_mixed_hit(&query, &["b12l34d"], &[]),
            "labels still allow sparse hits"
        );
    }

    #[test]
    fn workspace_path_density_counts_folded_unicode_scalars() {
        let query = FuzzyQuery::new("σσk", false);
        assert!(prepared_mixed_hit(&query, &[], &["/Σ-backup/ΣééςéK"]));
        assert!(!prepared_mixed_hit(&query, &[], &["/Σ-backup/ΣééςééK"]));
        assert!(!prepared_mixed_hit(
            &FuzzyQuery::new("σσk", true),
            &[],
            &["/Σ-backup/ΣééςéK"]
        ));
        assert!(prepared_mixed_hit(
            &FuzzyQuery::new("ΣςK", true),
            &[],
            &["/Σ-backup/ΣééςéK"]
        ));
        assert!(!prepared_mixed_hit(
            &FuzzyQuery::new("ss", false),
            &[],
            &["/ß"]
        ));
    }

    #[test]
    fn workspace_path_rejects_sparse_absolute_matches() {
        let skills = FuzzyQuery::new("skills", false);
        let ivc = "/Users/misaka/Developer/InfinityValley/ivc.feat-user-birthday-7121999519";
        assert!(
            !prepared_workspace_hit(
                &skills,
                "ivc.feat-user-birthday-7121999519",
                ivc,
                Some("develop")
            ),
            "letters gathered across an absolute path are not a workspace match"
        );
        assert!(prepared_workspace_hit(
            &skills,
            "skills",
            "/Users/misaka/Developer/Personal/skills",
            None,
        ));
        assert!(prepared_workspace_hit(
            &FuzzyQuery::new("secret", false),
            "ivc.feat-secret-ugc",
            "/Users/misaka/Developer/InfinityValley/ivc.feat-secret-ugc",
            None,
        ));
        assert!(prepared_workspace_hit(
            &FuzzyQuery::new("personal skills", false),
            "skills",
            "/Users/misaka/Developer/Personal/skills",
            None,
        ));
        assert!(prepared_workspace_hit(
            &FuzzyQuery::new("gsc", false),
            "GlobalSearchCatalog",
            "/tmp/catalog",
            None,
        ));
    }
}
