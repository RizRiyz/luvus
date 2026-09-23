//! UTF-8-safe, bounded composer state and caret editing.

use crate::ids::PaneId;
use std::path::PathBuf;

const MAX_DRAFT_CHARS: usize = 16_384;

#[derive(Default, Debug)]
pub(crate) struct CommandCenter {
    /// The strip stays visible when focus moves to a tab, pane, or other UI.
    pub(crate) focused: bool,
    pub draft: String,
    /// UTF-8 byte boundary in `draft`.
    pub cursor: usize,
    pub(crate) selection_anchor: Option<usize>,
    pub(crate) pending_clipboard: Option<String>,
    pub receipt: Option<String>,
    pub delivery_results: Vec<String>,
    pub delivery_index: usize,
    /// Resolved on edits, not every paint. The renderer reads current state
    /// for these identities; dispatch independently resolves the typed tokens.
    pub preview: Vec<PaneId>,
    /// Images staged for this draft are deleted if it is abandoned before
    /// delivery. Delivered paths are released to the receiving terminal.
    pub(crate) staged_images: Vec<PathBuf>,
}

impl CommandCenter {
    pub(crate) fn track_staged_image(&mut self, path: PathBuf) {
        self.staged_images.push(path);
    }

    pub(crate) fn release_staged_images(&mut self) {
        self.staged_images.clear();
    }

    fn prune_staged_images(&mut self) {
        self.staged_images.retain(|path| {
            if self.draft.contains(&path.to_string_lossy().to_string()) {
                true
            } else {
                crate::clipboard_image::discard_staged_png(path);
                false
            }
        });
    }

    pub(crate) fn selection(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.selection_anchor?;
        (anchor != self.cursor).then_some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    pub(crate) fn move_cursor(&mut self, next: usize, selecting: bool) {
        if selecting {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        self.cursor = next;
    }

    pub(crate) fn clear_receipt(&mut self) {
        self.receipt = None;
        self.delivery_results.clear();
    }

    pub(crate) fn insert(&mut self, input: &str) -> bool {
        let clean: String = input
            .chars()
            .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
            .collect();
        if clean.is_empty() {
            return true;
        }
        let selected = self.selection().unwrap_or(self.cursor..self.cursor);
        if self
            .draft
            .chars()
            .count()
            .saturating_sub(self.draft[selected.clone()].chars().count())
            .saturating_add(clean.chars().count())
            > MAX_DRAFT_CHARS
        {
            self.receipt = Some(format!("Prompt limit: {MAX_DRAFT_CHARS} characters"));
            return false;
        }
        self.draft.replace_range(selected.clone(), &clean);
        self.cursor = selected.start + clean.len();
        self.selection_anchor = None;
        self.clear_receipt();
        self.prune_staged_images();
        true
    }

    pub(crate) fn backspace(&mut self, word: bool) {
        if let Some(selected) = self.selection() {
            self.draft.replace_range(selected.clone(), "");
            self.cursor = selected.start;
            self.selection_anchor = None;
            self.clear_receipt();
            self.prune_staged_images();
            return;
        }
        let start = if word {
            previous_word(&self.draft, self.cursor)
        } else {
            self.draft[..self.cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index)
        };
        self.draft.drain(start..self.cursor);
        self.cursor = start;
        self.clear_receipt();
        self.prune_staged_images();
    }

    pub(crate) fn delete(&mut self, word: bool) {
        if self.selection().is_some() {
            self.backspace(false);
            return;
        }
        let end = if word {
            next_word(&self.draft, self.cursor)
        } else {
            self.draft[self.cursor..]
                .chars()
                .next()
                .map_or(self.cursor, |c| self.cursor + c.len_utf8())
        };
        self.draft.drain(self.cursor..end);
        self.clear_receipt();
        self.prune_staged_images();
    }

    pub(crate) fn delete_to(&mut self, end: usize) {
        let range = self
            .selection()
            .unwrap_or(self.cursor.min(end)..self.cursor.max(end));
        self.draft.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.selection_anchor = None;
        self.clear_receipt();
        self.prune_staged_images();
    }

    pub(crate) fn clear_all(&mut self) {
        self.draft.clear();
        self.cursor = 0;
        self.selection_anchor = None;
        self.clear_receipt();
        self.prune_staged_images();
    }

    pub(crate) fn copy_selection(&mut self, cut: bool) {
        if let Some(range) = self.selection() {
            self.pending_clipboard = Some(self.draft[range.clone()].to_string());
            if cut {
                self.delete_to(range.start);
            }
        }
    }
}

impl Drop for CommandCenter {
    fn drop(&mut self) {
        for path in &self.staged_images {
            crate::clipboard_image::discard_staged_png(path);
        }
    }
}

pub(crate) fn previous_word(text: &str, cursor: usize) -> usize {
    let mut start = cursor;
    for (index, character) in text[..start].char_indices().rev() {
        if !character.is_whitespace() {
            break;
        }
        start = index;
    }
    let category = text[..start]
        .chars()
        .next_back()
        .map(|c| c.is_alphanumeric() || c == '_');
    for (index, character) in text[..start].char_indices().rev() {
        if character.is_whitespace()
            || Some(character.is_alphanumeric() || character == '_') != category
        {
            break;
        }
        start = index;
    }
    start
}

pub(crate) fn next_word(text: &str, cursor: usize) -> usize {
    let mut end = cursor;
    for character in text[end..].chars() {
        if !character.is_whitespace() {
            break;
        }
        end += character.len_utf8();
    }
    let category = text[end..]
        .chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_');
    for character in text[end..].chars() {
        if character.is_whitespace()
            || Some(character.is_alphanumeric() || character == '_') != category
        {
            break;
        }
        end += character.len_utf8();
    }
    end
}

pub(crate) fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |at| at + 1)
}

pub(crate) fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .find('\n')
        .map_or(text.len(), |at| cursor + at)
}
