use super::model::{DiffLine, DiffLineKind, FileDiff};

#[derive(Clone, Debug)]
pub struct SplitRow {
    pub old: Option<DiffLine>,
    pub new: Option<DiffLine>,
}

pub fn stack_rows(diff: &FileDiff) -> Vec<DiffLine> {
    diff.hunks
        .iter()
        .flat_map(|hunk| {
            std::iter::once(DiffLine {
                kind: DiffLineKind::Header,
                old_line: None,
                new_line: None,
                text: hunk.header.clone(),
            })
            .chain(hunk.lines.iter().cloned())
        })
        .collect()
}

/// Derive a side-by-side projection from the normalized hunk stream. Runs when
/// a diff result lands or a view is rendered, never by reparsing Git output.
pub fn split_rows(diff: &FileDiff) -> Vec<SplitRow> {
    let mut out = Vec::new();
    for hunk in &diff.hunks {
        let header = DiffLine {
            kind: DiffLineKind::Header,
            old_line: None,
            new_line: None,
            text: hunk.header.clone(),
        };
        out.push(SplitRow {
            old: Some(header.clone()),
            new: Some(header),
        });
        let mut i = 0;
        while i < hunk.lines.len() {
            match hunk.lines[i].kind {
                DiffLineKind::Deletion => {
                    let del_start = i;
                    while i < hunk.lines.len() && hunk.lines[i].kind == DiffLineKind::Deletion {
                        i += 1;
                    }
                    let add_start = i;
                    while i < hunk.lines.len() && hunk.lines[i].kind == DiffLineKind::Addition {
                        i += 1;
                    }
                    let dels = &hunk.lines[del_start..add_start];
                    let adds = &hunk.lines[add_start..i];
                    for index in 0..dels.len().max(adds.len()) {
                        out.push(SplitRow {
                            old: dels.get(index).cloned(),
                            new: adds.get(index).cloned(),
                        });
                    }
                }
                DiffLineKind::Addition => {
                    out.push(SplitRow {
                        old: None,
                        new: Some(hunk.lines[i].clone()),
                    });
                    i += 1;
                }
                _ => {
                    let line = hunk.lines[i].clone();
                    out.push(SplitRow {
                        old: Some(line.clone()),
                        new: Some(line),
                    });
                    i += 1;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::diff::model::DiffHunk;
    use crate::diff::{DiffFileStatus, DiffKey, DiffLayer, RepoPath};

    #[test]
    fn split_projection_aligns_replacement_blocks_without_dropping_lines() {
        let diff = FileDiff {
            key: DiffKey {
                repo_id: "repo".into(),
                worktree_id: "tree".into(),
                layer: DiffLayer::Worktree,
                old_path: Some(RepoPath::from_path(Path::new("src/lib.rs")).unwrap()),
                new_path: Some(RepoPath::from_path(Path::new("src/lib.rs")).unwrap()),
            },
            status: DiffFileStatus::Modified,
            additions: 1,
            deletions: 2,
            binary: false,
            truncated: false,
            omitted_lines: 0,
            hunks: vec![DiffHunk {
                id: "h1".into(),
                old_start: 1,
                new_start: 1,
                header: "@@ -1,2 +1 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Deletion,
                        old_line: Some(1),
                        new_line: None,
                        text: "old one".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Deletion,
                        old_line: Some(2),
                        new_line: None,
                        text: "old two".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Addition,
                        old_line: None,
                        new_line: Some(1),
                        text: "new".into(),
                    },
                ],
            }],
        };

        let rows = split_rows(&diff);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].old.as_ref().unwrap().text, "old one");
        assert_eq!(rows[1].new.as_ref().unwrap().text, "new");
        assert_eq!(rows[2].old.as_ref().unwrap().text, "old two");
        assert!(rows[2].new.is_none());
    }
}
