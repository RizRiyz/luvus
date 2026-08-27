//! App-layer orchestration for the native DIFF review surface (docs/88).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;

use crate::app::{App, DiffMenu, DiffMenuItem, Tab, ViewKind};
use crate::diff::{DiffKey, DiffListRow, DiffLoad, DiffView, FilesMode};
use crate::event::AppEvent;
use crate::ids::PaneId;
use crate::layout::{Axis, TileLayout};

use super::files::OpenTarget;

impl App {
    /// Route API methods that need repository status through the same off-loop
    /// scan used by FILES and the interactive DIFF dock. A cached `diff.list`
    /// answers immediately and merely schedules a cadence-gated refresh;
    /// first-use methods and explicit `diff.refresh` park their reply until the
    /// worker result returns to the single writer.
    pub(crate) fn prepare_diff_api(
        &mut self,
        req: crate::ipc::api::ApiRequest,
    ) -> Option<crate::ipc::api::ApiRequest> {
        if !req.method.starts_with("diff.") || req.method == "diff.navigate" {
            return Some(req);
        }
        let refresh = req.method == "diff.refresh";
        let ready = self.diff_snapshot_matches_active_workspace();
        if ready && !refresh {
            if req.method == "diff.list" {
                self.refresh_diff_status(false);
            }
            return Some(req);
        }
        let root = self.ws().cwd.clone();
        self.pending_diff_api.push((root, req));
        self.refresh_diff_status(true);
        None
    }

    /// Complete parked DIFF API calls only after the accepted status result has
    /// been applied. Requests targeting a workspace that lost focus while Git
    /// was running fail closed instead of being redirected to the new root.
    pub(crate) fn finish_pending_diff_api(&mut self) {
        let active_root = self.ws().cwd.clone();
        let scan_complete = !self.diff.status_inflight;
        let mut pending = std::mem::take(&mut self.pending_diff_api);
        for (root, req) in pending.drain(..) {
            if !crate::platform::same_path(&root, &active_root) {
                let _ = req.reply.send(
                    serde_json::json!({"id":req.id,"error":{
                        "code":"diff_error",
                        "message":"active workspace changed while DIFF was refreshing"
                    }})
                    .to_string(),
                );
            } else if scan_complete {
                let response = self.handle_api(&req);
                let _ = req.reply.send(response);
            } else {
                self.pending_diff_api.push((root, req));
            }
        }
    }

    /// Fail parked DIFF requests for one workspace while preserving requests
    /// targeting other open workspace roots.
    pub(crate) fn fail_pending_diff_api_for_root(&mut self, closed_root: &Path, message: &str) {
        let mut pending = std::mem::take(&mut self.pending_diff_api);
        for (root, req) in pending.drain(..) {
            if crate::platform::same_path(&root, closed_root) {
                let _ = req.reply.send(
                    serde_json::json!({"id":req.id,"error":{
                        "code":"diff_error",
                        "message":message
                    }})
                    .to_string(),
                );
            } else {
                self.pending_diff_api.push((root, req));
            }
        }
    }

    /// Fail every parked DIFF request when no workspace remains.
    pub(crate) fn fail_pending_diff_api(&mut self, message: &str) {
        for (_, req) in self.pending_diff_api.drain(..) {
            let _ = req.reply.send(
                serde_json::json!({"id":req.id,"error":{
                    "code":"diff_error",
                    "message":message
                }})
                .to_string(),
            );
        }
    }

    pub(crate) fn ensure_diff_snapshot(&self) -> Result<(), String> {
        if self.diff_snapshot_matches_active_workspace() {
            return Ok(());
        }
        Err(self
            .diff
            .error
            .clone()
            .unwrap_or_else(|| "DIFF is not ready".to_string()))
    }

    pub(crate) fn ensure_diff_notes_sync(&mut self) -> Result<(), String> {
        let snapshot = self
            .diff
            .snapshot
            .as_ref()
            .ok_or_else(|| "DIFF is not ready".to_string())?;
        let review_id = crate::diff::notes::review_id_for(&snapshot.repo_id, &snapshot.worktree_id);
        if self.diff.loaded_review.as_deref() == Some(review_id.as_str()) {
            return Ok(());
        }
        self.diff.notes = crate::diff::notes::load(&snapshot.repo_id, &review_id)?;
        self.diff.progress = crate::diff::notes::load_progress(&snapshot.repo_id, &review_id)?;
        self.diff.loaded_review = Some(review_id);
        self.apply_diff_progress();
        self.refresh_diff_note_counts();
        Ok(())
    }

    pub fn set_files_mode(&mut self, mode: FilesMode) {
        if self.files_mode == mode {
            return;
        }
        self.files_mode = mode;
        if mode == FilesMode::Diff {
            self.refresh_diff_status(true);
        }
    }

    pub fn refresh_diff_status(&mut self, force: bool) {
        let visible_root = self.ws().cwd.clone();
        let root_changed = self
            .diff
            .status_root
            .as_deref()
            .is_none_or(|root| !crate::platform::same_path(root, &visible_root));
        if self.git_status_inflight && !root_changed {
            return;
        }
        if !force
            && !root_changed
            && std::time::Instant::now().duration_since(self.last_git_status_at)
                < std::time::Duration::from_secs(2)
        {
            return;
        }
        if root_changed {
            // The list, its row hit targets, review state, and FILES tint all
            // belong to the previous workspace. Drop them before the new scan
            // finishes so a fast click cannot open workspace A's change in B.
            self.diff.snapshot = None;
            self.diff.rows.clear();
            self.diff.selected_key = None;
            self.diff.cursor = 0;
            self.diff.scroll = 0;
            self.diff.error = None;
            self.diff.loaded_review = None;
            self.diff.notes.clear();
            self.diff.progress = crate::diff::notes::ReviewProgress::default();
            self.diff.selected_notes.clear();
            self.file_git_status.clear();
        }
        self.last_git_status_at = std::time::Instant::now();
        self.git_status_inflight = true;
        self.diff.status_inflight = true;
        self.diff.status_root = Some(visible_root.clone());
        self.diff.status_generation = self.diff.status_generation.wrapping_add(1);
        let token = self.diff.status_generation;
        let scan_root = visible_root.clone();
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let result = crate::diff::git::scan(&scan_root, token);
            let _ = tx.send(AppEvent::DiffStatus {
                token,
                visible_root,
                result,
            });
        });
    }

    pub fn apply_diff_status(
        &mut self,
        token: u64,
        visible_root: PathBuf,
        result: Result<crate::diff::DiffSnapshot, String>,
    ) -> bool {
        if token != self.diff.status_generation {
            return false;
        }
        if !crate::platform::same_path(&visible_root, &self.ws().cwd) {
            // A workspace switch can win the race with the scan result. Never
            // publish that stale result; immediately request the active root.
            self.git_status_inflight = false;
            self.diff.status_inflight = false;
            self.diff.status_root = None;
            self.refresh_diff_status(true);
            return true;
        }
        self.git_status_inflight = false;
        self.diff.status_inflight = false;
        self.diff.status_root = Some(visible_root.clone());
        match result {
            Ok(mut snapshot) => {
                let old_fingerprints = self
                    .diff
                    .snapshot
                    .as_ref()
                    .map(|old| {
                        old.files
                            .iter()
                            .map(|file| (file.key.clone(), file.fingerprint.clone()))
                            .collect::<std::collections::HashMap<_, _>>()
                    })
                    .unwrap_or_default();
                if let Some(old) = self.diff.snapshot.as_ref() {
                    let previous_by_key: HashMap<_, _> =
                        old.files.iter().map(|file| (&file.key, file)).collect();
                    for file in &mut snapshot.files {
                        if let Some(previous) = previous_by_key.get(&file.key) {
                            file.viewed_fingerprint = previous.viewed_fingerprint.clone();
                            file.unresolved_notes = previous.unresolved_notes;
                        }
                    }
                }
                apply_note_counts(&mut snapshot, &self.diff.notes);
                let tint = crate::diff::git::tree_tint(&snapshot, &visible_root);
                let changed = self.diff.snapshot.as_ref().map(|old| &old.fingerprint)
                    != Some(&snapshot.fingerprint)
                    || self.file_git_status != tint;
                self.file_git_status = tint;
                self.diff.error = None;
                let review =
                    crate::diff::notes::review_id_for(&snapshot.repo_id, &snapshot.worktree_id);
                if self.diff.loaded_review.as_deref() != Some(review.as_str()) {
                    self.diff.notes.clear();
                    self.diff.progress = crate::diff::notes::ReviewProgress::default();
                    for file in &mut snapshot.files {
                        file.unresolved_notes = 0;
                    }
                    self.schedule_diff_notes_load(snapshot.repo_id.clone(), review);
                } else {
                    self.reconcile_missing_diff_notes(&snapshot);
                    apply_note_counts(&mut snapshot, &self.diff.notes);
                }
                self.diff.snapshot = Some(snapshot);
                self.apply_diff_progress();
                self.diff.rebuild_rows();
                let live_refresh = self.config.layout.diff_live_refresh;
                let visible: std::collections::HashSet<PaneId> =
                    self.layout().leaves().into_iter().collect();
                let current = self
                    .diff
                    .snapshot
                    .as_ref()
                    .map(|snapshot| {
                        snapshot
                            .files
                            .iter()
                            .map(|file| (file.key.clone(), file.fingerprint.clone()))
                            .collect::<std::collections::HashMap<_, _>>()
                    })
                    .unwrap_or_default();
                let mut reload = Vec::new();
                for (id, view) in &mut self.views {
                    let ViewKind::Diff(view) = view else { continue };
                    let changed_file = current.get(&view.key).is_none_or(|fingerprint| {
                        old_fingerprints.get(&view.key) != Some(fingerprint)
                    });
                    if changed_file {
                        view.dirty = true;
                        if live_refresh && visible.contains(id) && current.contains_key(&view.key) {
                            reload.push(*id);
                        }
                    }
                }
                for id in reload {
                    self.schedule_diff_read(id);
                }
                changed
            }
            Err(error) => {
                let changed = self.diff.error.as_deref() != Some(error.as_str())
                    || self.diff.snapshot.is_some()
                    || !self.file_git_status.is_empty();
                self.diff.error = Some(if error.trim().is_empty() {
                    "not a Git repository".to_string()
                } else {
                    error
                });
                self.diff.snapshot = None;
                self.diff.rows.clear();
                self.file_git_status.clear();
                changed
            }
        }
    }

    pub fn diff_scroll_by(&mut self, delta: isize) {
        self.diff.scroll_detached = true;
        if delta < 0 {
            self.diff.scroll = self.diff.scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.diff.scroll = self.diff.scroll.saturating_add(delta as usize);
        }
    }

    pub fn open_diff_menu(&mut self, row: usize, col: u16, screen_row: u16) {
        if !self.diff_snapshot_matches_active_workspace() {
            self.refresh_diff_status(true);
            return;
        }
        let Some(key) = self.diff.rows.get(row).and_then(|entry| {
            let DiffListRow::File(index) = entry else {
                return None;
            };
            self.diff
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.files.get(*index))
                .map(|file| file.key.clone())
        }) else {
            return;
        };
        self.diff_menu = Some(DiffMenu {
            key,
            anchor: (col, screen_row),
            items: Vec::new(),
        });
    }

    pub fn diff_menu_click(&mut self, col: u16, row: u16) {
        let hit = self.diff_menu.as_ref().and_then(|menu| {
            menu.items
                .iter()
                .find(|(_, rect)| {
                    col >= rect.x && col < rect.right() && row >= rect.y && row < rect.bottom()
                })
                .map(|(item, _)| *item)
        });
        match hit {
            Some(item) => self.diff_menu_action(item),
            None => self.diff_menu = None,
        }
    }

    fn diff_menu_action(&mut self, item: DiffMenuItem) {
        let Some(menu) = self.diff_menu.take() else {
            return;
        };
        match item {
            DiffMenuItem::OpenPreview => self.open_diff_view(menu.key, OpenTarget::Preview),
            DiffMenuItem::OpenPane => self.open_diff_view(menu.key, OpenTarget::Pane),
            DiffMenuItem::OpenTab => self.open_diff_view(menu.key, OpenTarget::Tab),
            DiffMenuItem::CopyPath => {
                self.pending_clipboard = Some(menu.key.display_path().to_string());
                self.show_toast("path copied".to_string());
            }
        }
    }

    pub fn diff_row_activate(&mut self, row: usize, target: OpenTarget) {
        if !self.diff_snapshot_matches_active_workspace() {
            self.refresh_diff_status(true);
            return;
        }
        self.diff.cursor = row.min(self.diff.rows.len().saturating_sub(1));
        self.diff.scroll_detached = false;
        self.diff.ensure_cursor_visible();
        let Some(file) = self.diff.selected_file().cloned() else {
            return;
        };
        self.diff.selected_key = Some(file.key.clone());
        self.open_diff_view(file.key, target);
    }

    pub(crate) fn diff_file_for_path(
        &self,
        raw: &str,
        layer: Option<&crate::diff::DiffLayer>,
    ) -> Result<crate::diff::DiffFile, String> {
        let snapshot = self
            .diff
            .snapshot
            .as_ref()
            .ok_or_else(|| "DIFF is not ready".to_string())?;
        let path = canonicalize_with_missing_tail(&self.resolve_file_path(raw));
        let repo_root = canonicalize_with_missing_tail(&snapshot.repo_root);
        let relative = path
            .strip_prefix(&repo_root)
            .map_err(|_| "change path must be inside the active repository".to_string())?;
        let wanted = crate::diff::model::RepoPath::from_path(relative)?;
        let matches: Vec<_> = snapshot
            .files
            .iter()
            .filter(|file| {
                layer.is_none_or(|layer| &file.key.layer == layer)
                    && file.key.new_path.as_ref().or(file.key.old_path.as_ref()) == Some(&wanted)
            })
            .cloned()
            .collect();
        match matches.as_slice() {
            [] => Err("path is not present in the selected diff layer".to_string()),
            [file] => Ok(file.clone()),
            _ => Err("path has more than one change layer; pass --layer".to_string()),
        }
    }

    pub(crate) fn load_diff_file_sync(
        &mut self,
        file: &crate::diff::DiffFile,
    ) -> Result<crate::diff::model::FileDiff, String> {
        let root = self
            .diff
            .snapshot
            .as_ref()
            .ok_or_else(|| "DIFF is not ready".to_string())?
            .repo_root
            .clone();
        let context = self.config.diff_context_lines();
        if let Some(cached) = self.diff.cache_get(&file.key, context, &file.fingerprint) {
            return Ok(cached);
        }
        let loaded = crate::diff::git::load_diff(&root, file, context)?;
        self.diff
            .cache_insert(context, file.fingerprint.clone(), loaded.clone());
        Ok(loaded)
    }

    pub fn open_diff_view(&mut self, key: DiffKey, target: OpenTarget) {
        let dashboard_active =
            self.active_is_git() || self.active_is_orch() || self.active_is_mission();
        let mut effective_target = target;
        let dashboard_diff = if target == OpenTarget::Preview && dashboard_active {
            // Dashboard layouts contain only an invisible placeholder leaf. A
            // preview may reuse a dedicated DIFF tab, but never a DIFF pane
            // embedded beside terminals in a normal tab.
            let existing = self.diff_tab_in_active_workspace();
            if existing.is_none() {
                effective_target = OpenTarget::Tab;
            }
            existing
        } else {
            None
        };
        let focused = self.layout().focus;
        let replace_focused = effective_target == OpenTarget::Preview
            && matches!(self.views.get(&focused), Some(ViewKind::Diff(_)));
        if !dashboard_active && !replace_focused && dashboard_diff.is_none() {
            if let Some(id) = self.diff_view_showing(&key) {
                self.focus_pane_global(id);
                return;
            }
        }
        let root = self
            .diff
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.repo_root.clone())
            .unwrap_or_else(|| self.ws().cwd.clone());
        let view = DiffView::new(
            root,
            key,
            self.config.layout.diff_layout,
            self.config.diff_context_lines(),
            self.config.layout.diff_show_line_numbers,
            self.config.layout.diff_wrap,
        );
        if let Some(id) = dashboard_diff {
            if let Some(previous) = self.active_preview_view() {
                self.preview_views.remove(&previous);
            }
            self.views.insert(id, ViewKind::Diff(Box::new(view)));
            self.preview_views.insert(id);
            self.focus_pane_global(id);
            self.schedule_diff_read(id);
            self.mode = super::Mode::Normal;
            return;
        }
        if effective_target == OpenTarget::Preview {
            if replace_focused {
                // A DIFF view is already the user's active browsing surface.
                // Replace it in place instead of jumping to an older preview
                // pane elsewhere in this workspace.
                if let Some(previous) = self.active_preview_view() {
                    self.preview_views.remove(&previous);
                }
                self.views.insert(focused, ViewKind::Diff(Box::new(view)));
                self.preview_views.insert(focused);
                self.schedule_diff_read(focused);
                self.mode = super::Mode::Normal;
                return;
            }
            if let Some(id) = self.active_preview_view() {
                self.views.insert(id, ViewKind::Diff(Box::new(view)));
                self.focus_pane_global(id);
                self.schedule_diff_read(id);
                return;
            }
        }
        let id = PaneId::alloc();
        self.views.insert(id, ViewKind::Diff(Box::new(view)));
        match effective_target {
            OpenTarget::Tab => {
                let ws = &mut self.workspaces[self.active_ws];
                ws.tabs.push(Tab::panes(TileLayout::new(id)));
                ws.active_tab = ws.tabs.len() - 1;
            }
            OpenTarget::Preview | OpenTarget::Pane => {
                self.layout_mut().split_focused(Axis::Col, id);
                self.layout_mut().focus = id;
            }
        }
        if effective_target == OpenTarget::Preview {
            self.preview_views.insert(id);
        }
        self.schedule_diff_read(id);
        self.mode = super::Mode::Normal;
    }

    pub fn schedule_diff_read(&mut self, id: PaneId) {
        let Some(ViewKind::Diff(existing)) = self.views.get(&id) else {
            return;
        };
        let key = existing.key.clone();
        let context = existing.context_lines;
        let file = self
            .diff
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.files.iter().find(|file| file.key == key))
            .cloned();
        let cached = file
            .as_ref()
            .and_then(|file| self.diff.cache_get(&key, context, &file.fingerprint));
        let Some(ViewKind::Diff(view)) = self.views.get_mut(&id) else {
            return;
        };
        view.request_token = view.request_token.wrapping_add(1);
        view.load = DiffLoad::Loading;
        view.dirty = false;
        let token = view.request_token;
        let root = view.root.clone();
        let mut notes: Vec<_> = self
            .diff
            .notes
            .iter()
            .filter(|note| note.anchor.diff_key == key)
            .cloned()
            .collect();
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let result = file
                .ok_or_else(|| "change disappeared before the diff loaded".to_string())
                .and_then(|file| {
                    cached.map_or_else(|| crate::diff::git::load_diff(&root, &file, context), Ok)
                })
                .map(|diff| {
                    crate::diff::notes::reconcile(&mut notes, &diff);
                    crate::diff::LoadedDiff {
                        diff,
                        reconciled_notes: notes,
                    }
                });
            let _ = tx.send(AppEvent::DiffLoaded { id, token, result });
        });
    }

    pub fn apply_diff_loaded(
        &mut self,
        id: PaneId,
        token: u64,
        result: Result<crate::diff::LoadedDiff, String>,
    ) -> bool {
        let Some(ViewKind::Diff(view)) = self.views.get(&id) else {
            return false;
        };
        if token != view.request_token {
            return false;
        }
        let context = view.context_lines;
        let old_anchor = self
            .selected_diff_source(id)
            .map(|(side, line, _)| (side, line));
        let Some(ViewKind::Diff(view)) = self.views.get_mut(&id) else {
            return false;
        };
        let mut cache = None;
        let mut reconciled = Vec::new();
        view.load = match result {
            Ok(loaded) => {
                let diff = loaded.diff;
                reconciled = loaded.reconciled_notes;
                view.stack_rows = crate::diff::rows::stack_rows(&diff);
                view.split_rows = crate::diff::rows::split_rows(&diff);
                view.rebuild_row_indices();
                cache = Some(diff.clone());
                DiffLoad::Ready(Box::new(diff))
            }
            Err(error) if view.key.layer == crate::diff::DiffLayer::Conflict => {
                DiffLoad::Conflict(error)
            }
            Err(error) => DiffLoad::Error(error),
        };
        view.scroll = 0;
        view.selected = old_anchor
            .and_then(|anchor| {
                view.stack_rows
                    .iter()
                    .position(|line| source_anchor(line) == Some(anchor))
            })
            .unwrap_or(0);
        if let Some(diff) = cache {
            let mut fingerprint = String::new();
            if let Some(file) =
                self.diff.snapshot.as_mut().and_then(|snapshot| {
                    snapshot.files.iter_mut().find(|file| file.key == diff.key)
                })
            {
                fingerprint = file.fingerprint.clone();
                file.additions = Some(diff.additions);
                file.deletions = Some(diff.deletions);
                file.binary = diff.binary;
            }
            self.diff.cache_insert(context, fingerprint, diff);
        }
        for updated in reconciled {
            if let Some(index) = self
                .diff
                .notes
                .iter()
                .position(|note| note.id == updated.id)
            {
                let previous = self.diff.notes[index].revision;
                let reconciled_from = updated.revision.saturating_sub(1);
                if previous == reconciled_from && updated.revision != previous {
                    self.diff.notes[index] = updated.clone();
                    self.save_diff_note_async(updated, Some(previous));
                }
            }
        }
        self.refresh_diff_note_counts();
        true
    }

    pub fn diff_view_showing(&self, key: &DiffKey) -> Option<PaneId> {
        self.ws()
            .tabs
            .iter()
            .flat_map(|tab| tab.layout.leaves())
            .find(|id| matches!(self.views.get(id), Some(ViewKind::Diff(view)) if &view.key == key))
    }

    fn diff_tab_in_active_workspace(&self) -> Option<PaneId> {
        self.ws().tabs.iter().find_map(|tab| {
            let leaves = tab.layout.leaves();
            let id = (leaves.len() == 1).then_some(leaves[0])?;
            matches!(self.views.get(&id), Some(ViewKind::Diff(_))).then_some(id)
        })
    }

    pub(crate) fn diff_snapshot_matches_active_workspace(&self) -> bool {
        self.diff.snapshot.as_ref().is_some_and(|snapshot| {
            crate::platform::same_path(&snapshot.visible_root, &self.ws().cwd)
        })
    }

    pub fn mark_active_diff_viewed(&mut self) {
        let focus = self.layout().focus;
        let Some(ViewKind::Diff(view)) = self.views.get(&focus) else {
            return;
        };
        let key = view.key.clone();
        if let Some(file) = self
            .diff
            .snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.files.iter_mut().find(|file| file.key == key))
        {
            file.viewed_fingerprint = Some(file.fingerprint.clone());
            let key = file.key.clone();
            let fingerprint = file.fingerprint.clone();
            let repo_id = key.repo_id.clone();
            let review_id = crate::diff::notes::review_id(&key);
            if let Some(existing) = self
                .diff
                .progress
                .viewed
                .iter_mut()
                .find(|entry| entry.key == key)
            {
                existing.fingerprint = fingerprint.clone();
                existing.viewed_at_ms = crate::diff::notes::now_ms();
            } else {
                self.diff
                    .progress
                    .viewed
                    .push(crate::diff::notes::ViewedChange {
                        key: key.clone(),
                        fingerprint: fingerprint.clone(),
                        viewed_at_ms: crate::diff::notes::now_ms(),
                    });
            }
            let tx = self.app_tx.clone();
            std::thread::spawn(move || {
                let result =
                    crate::diff::notes::mark_viewed(&repo_id, &review_id, &key, &fingerprint);
                let _ = tx.send(AppEvent::DiffProgressSaved { result });
            });
            self.diff.rebuild_rows();
            self.show_toast("marked viewed".to_string());
        }
    }

    fn schedule_diff_notes_load(&mut self, repo_id: String, review_id: String) {
        self.diff.loaded_review = Some(review_id.clone());
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let result = crate::diff::notes::load(&repo_id, &review_id).and_then(|notes| {
                crate::diff::notes::load_progress(&repo_id, &review_id)
                    .map(|progress| (notes, progress))
            });
            let _ = tx.send(AppEvent::DiffNotesLoaded { review_id, result });
        });
    }

    pub fn apply_diff_notes_loaded(
        &mut self,
        review_id: String,
        result: Result<
            (
                Vec<crate::diff::ReviewNote>,
                crate::diff::notes::ReviewProgress,
            ),
            String,
        >,
    ) -> bool {
        if self.diff.loaded_review.as_deref() != Some(review_id.as_str()) {
            return false;
        }
        match result {
            Ok((notes, progress)) => {
                self.diff.notes = notes;
                self.diff.progress = progress;
                self.apply_diff_progress();
                self.refresh_diff_note_counts();
            }
            Err(error) => self.show_toast(format!("cannot load review notes: {error}")),
        }
        true
    }

    pub fn apply_diff_note_saved(
        &mut self,
        note: crate::diff::ReviewNote,
        result: Result<(), String>,
    ) -> bool {
        match result {
            Ok(()) => {
                if let Some(existing) = self.diff.notes.iter_mut().find(|n| n.id == note.id) {
                    *existing = note;
                } else {
                    self.diff.notes.push(note);
                }
                self.refresh_diff_note_counts();
                self.show_toast("review note saved".to_string());
            }
            Err(error) => self.show_toast(format!("note not saved: {error}")),
        }
        true
    }

    pub fn apply_diff_note_removed(&mut self, id: String, result: Result<(), String>) -> bool {
        match result {
            Ok(()) => {
                self.diff.notes.retain(|note| note.id != id);
                self.refresh_diff_note_counts();
                self.show_toast("review note removed".to_string());
            }
            Err(error) => self.show_toast(format!("note not removed: {error}")),
        }
        true
    }

    pub(crate) fn refresh_diff_note_counts(&mut self) {
        if let Some(snapshot) = self.diff.snapshot.as_mut() {
            apply_note_counts(snapshot, &self.diff.notes);
        }
        self.diff.rebuild_rows();
    }

    fn apply_diff_progress(&mut self) {
        let progress = &self.diff.progress.viewed;
        if let Some(snapshot) = self.diff.snapshot.as_mut() {
            for file in &mut snapshot.files {
                file.viewed_fingerprint = progress
                    .iter()
                    .find(|entry| entry.key == file.key)
                    .map(|entry| entry.fingerprint.clone());
            }
        }
    }

    fn reconcile_missing_diff_notes(&mut self, snapshot: &crate::diff::DiffSnapshot) {
        let exact_keys: HashSet<_> = snapshot.files.iter().map(|file| &file.key).collect();
        let mut by_path: HashMap<
            (&crate::diff::DiffLayer, &crate::diff::RepoPath),
            Vec<&crate::diff::DiffFile>,
        > = HashMap::new();
        for file in &snapshot.files {
            for path in [file.key.old_path.as_ref(), file.key.new_path.as_ref()]
                .into_iter()
                .flatten()
            {
                by_path
                    .entry((&file.key.layer, path))
                    .or_default()
                    .push(file);
            }
        }
        let mut saves = Vec::new();
        for note in &mut self.diff.notes {
            if note.anchor.diff_key.repo_id != snapshot.repo_id
                || note.anchor.diff_key.worktree_id != snapshot.worktree_id
                || exact_keys.contains(&note.anchor.diff_key)
            {
                continue;
            }
            let mut candidates = Vec::new();
            for path in [
                note.anchor.diff_key.old_path.as_ref(),
                note.anchor.diff_key.new_path.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                for candidate in by_path
                    .get(&(&note.anchor.diff_key.layer, path))
                    .into_iter()
                    .flatten()
                {
                    if !candidates
                        .iter()
                        .any(|existing: &&crate::diff::DiffFile| existing.key == candidate.key)
                    {
                        candidates.push(*candidate);
                    }
                }
            }
            let previous = note.revision;
            if let [candidate] = candidates.as_slice() {
                note.anchor.diff_key = candidate.key.clone();
                note.state = crate::diff::NoteState::Outdated;
            } else if note.state != crate::diff::NoteState::Orphaned {
                note.state = crate::diff::NoteState::Orphaned;
            } else {
                continue;
            }
            note.revision = note.revision.saturating_add(1);
            note.updated_at_ms = crate::diff::notes::now_ms();
            saves.push((note.clone(), previous));
        }
        for (note, previous) in saves {
            self.save_diff_note_async(note, Some(previous));
        }
    }

    fn save_diff_note_async(&mut self, note: crate::diff::ReviewNote, expected: Option<u64>) {
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let result = crate::diff::notes::save(&note, expected);
            let _ = tx.send(AppEvent::DiffNoteSaved { note, result });
        });
    }

    fn selected_diff_source(&self, id: PaneId) -> Option<(crate::diff::DiffSide, u32, String)> {
        let ViewKind::Diff(view) = self.views.get(&id)? else {
            return None;
        };
        let width = self
            .pane_content_rects
            .iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, rect)| rect.width)
            .unwrap_or(120);
        let split = !view.wrap
            && match view.preference {
                crate::diff::DiffLayoutPreference::Stack => false,
                crate::diff::DiffLayoutPreference::Split => width >= 96,
                crate::diff::DiffLayoutPreference::Auto => width >= 96,
            };
        if split {
            let anchor = view.stack_rows.get(view.selected).and_then(source_anchor)?;
            let row = view.split_rows.iter().find(|row| {
                row.new.as_ref().and_then(source_anchor) == Some(anchor)
                    || row.old.as_ref().and_then(source_anchor) == Some(anchor)
            })?;
            let preferred = match view.selected_side {
                crate::diff::DiffSide::Old => row.old.as_ref().and_then(|line| {
                    line.old_line
                        .map(|number| (crate::diff::DiffSide::Old, number, line.text.clone()))
                }),
                crate::diff::DiffSide::New => row.new.as_ref().and_then(|line| {
                    line.new_line
                        .map(|number| (crate::diff::DiffSide::New, number, line.text.clone()))
                }),
            };
            if preferred.is_some() {
                return preferred;
            }
            row.new
                .as_ref()
                .and_then(|line| {
                    line.new_line
                        .map(|number| (crate::diff::DiffSide::New, number, line.text.clone()))
                })
                .or_else(|| {
                    row.old.as_ref().and_then(|line| {
                        line.old_line
                            .map(|number| (crate::diff::DiffSide::Old, number, line.text.clone()))
                    })
                })
        } else {
            let line = view.stack_rows.get(view.selected)?;
            match view.selected_side {
                crate::diff::DiffSide::Old => line
                    .old_line
                    .map(|number| (crate::diff::DiffSide::Old, number, line.text.clone())),
                crate::diff::DiffSide::New => line
                    .new_line
                    .map(|number| (crate::diff::DiffSide::New, number, line.text.clone())),
            }
            .or_else(|| {
                line.new_line
                    .map(|number| (crate::diff::DiffSide::New, number, line.text.clone()))
            })
            .or_else(|| {
                line.old_line
                    .map(|number| (crate::diff::DiffSide::Old, number, line.text.clone()))
            })
        }
    }

    pub(crate) fn press_diff_source(
        &mut self,
        pane: PaneId,
        row: usize,
        side: crate::diff::DiffSide,
    ) {
        self.selection = None;
        self.focus_pane_global(pane);
        let Some(ViewKind::Diff(view)) = self.views.get_mut(&pane) else {
            return;
        };
        view.selected = row;
        view.selected_side = side;
        if !view.note_selecting {
            return;
        }

        let Some(anchor) = view
            .stack_rows
            .get(row)
            .and_then(|line| source_anchor_on_side(line, side))
        else {
            return;
        };
        view.range_anchor = Some(anchor);
        self.diff_note_drag = Some(pane);
    }

    pub(crate) fn drag_diff_source(
        &mut self,
        pane: PaneId,
        row: usize,
        side: crate::diff::DiffSide,
    ) {
        if self.diff_note_drag != Some(pane) {
            return;
        }
        let Some(ViewKind::Diff(view)) = self.views.get_mut(&pane) else {
            return;
        };
        if view.range_anchor.map(|(anchor_side, _)| anchor_side) != Some(side)
            || view
                .stack_rows
                .get(row)
                .and_then(|line| source_anchor_on_side(line, side))
                .is_none()
        {
            return;
        }
        view.selected = row;
        view.selected_side = side;
    }

    pub(crate) fn finish_diff_source_drag(&mut self) {
        let Some(pane) = self.diff_note_drag.take() else {
            return;
        };
        let content = self
            .pane_content_rects
            .iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, rect)| *rect)
            .unwrap_or(Rect::new(0, 0, 80, 22));
        let viewport = content.height.saturating_sub(2) as usize;
        let marker_style = self.config.layout.diff_marker_style;
        let Some(ViewKind::Diff(view)) = self.views.get_mut(&pane) else {
            return;
        };
        view.note_selecting = false;
        view.note_draft = Some(String::new());
        view.scroll = view.selected.saturating_sub(viewport.saturating_sub(9));
        let split = view.effective_split(content.width);
        view.ensure_horizontal_visible(content.width, marker_style, split);
    }

    fn commit_diff_note(&mut self, id: PaneId, body: String) {
        let edit_id = self.views.get_mut(&id).and_then(|view| match view {
            ViewKind::Diff(view) => view.note_edit_id.take(),
            _ => None,
        });
        if let Some(edit_id) = edit_id {
            let Some(existing) = self
                .diff
                .notes
                .iter()
                .find(|note| note.id == edit_id)
                .cloned()
            else {
                self.show_toast("review note disappeared".to_string());
                return;
            };
            let mut updated = existing.clone();
            updated.body = body;
            updated.revision = updated.revision.saturating_add(1);
            updated.updated_at_ms = crate::diff::notes::now_ms();
            self.save_diff_note_async(updated, Some(existing.revision));
            return;
        }
        let Some((side, line, context)) = self.selected_diff_source(id) else {
            self.show_toast("select a source line first".to_string());
            return;
        };
        let Some(ViewKind::Diff(view)) = self.views.get(&id) else {
            return;
        };
        let mut start = line;
        let mut end = line;
        if let Some((range_side, range_start)) = view.range_anchor {
            if range_side == side {
                start = range_start.min(line);
                end = range_start.max(line);
            }
        }
        let key = view.key.clone();
        let review_id = crate::diff::notes::review_id(&key);
        let now = crate::diff::notes::now_ms();
        let note = crate::diff::ReviewNote {
            id: crate::diff::notes::note_id(),
            review_id,
            author: "user".to_string(),
            kind: crate::diff::NoteKind::Issue,
            body,
            anchor: crate::diff::notes::NoteAnchor {
                diff_key: key,
                side,
                start_line: start,
                end_line: end,
                context_sha256: crate::diff::notes::context_hash(&context),
                context: context.chars().take(512).collect(),
            },
            state: crate::diff::NoteState::Open,
            deliveries: Vec::new(),
            revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        if let Some(ViewKind::Diff(view)) = self.views.get_mut(&id) {
            view.range_anchor = None;
            view.note_selecting = false;
        }
        self.save_diff_note_async(note, None);
    }

    fn current_diff_note_ids(&self, id: PaneId) -> Vec<String> {
        let Some((side, line, _)) = self.selected_diff_source(id) else {
            return Vec::new();
        };
        let Some(ViewKind::Diff(view)) = self.views.get(&id) else {
            return Vec::new();
        };
        self.diff
            .notes
            .iter()
            .filter(|note| {
                note.anchor.diff_key == view.key
                    && note.anchor.side == side
                    && line >= note.anchor.start_line
                    && line <= note.anchor.end_line
            })
            .map(|note| note.id.clone())
            .collect()
    }

    fn toggle_current_diff_note_selection(&mut self, id: PaneId) {
        let Some(note_id) = self.current_diff_note_ids(id).into_iter().next() else {
            self.show_toast("no review note on this line".to_string());
            return;
        };
        if !self.diff.selected_notes.remove(&note_id) {
            self.diff.selected_notes.insert(note_id);
        }
    }

    fn edit_current_diff_note(&mut self, id: PaneId) {
        let Some(note_id) = self.current_diff_note_ids(id).into_iter().next() else {
            self.show_toast("no review note on this line".to_string());
            return;
        };
        self.edit_diff_note(id, &note_id);
    }

    pub(crate) fn edit_diff_note(&mut self, id: PaneId, note_id: &str) {
        let Some(note) = self
            .diff
            .notes
            .iter()
            .find(|note| note.id == note_id)
            .cloned()
        else {
            self.show_toast("review note disappeared".to_string());
            return;
        };
        let Some(selected) = self.views.get(&id).and_then(|view| match view {
            ViewKind::Diff(view) if note.anchor.diff_key == view.key => {
                view.stack_rows.iter().position(|line| {
                    source_anchor_on_side(line, note.anchor.side)
                        == Some((note.anchor.side, note.anchor.end_line))
                })
            }
            _ => None,
        }) else {
            self.show_toast("review note source is no longer visible".to_string());
            return;
        };
        self.selection = None;
        self.diff_note_drag = None;
        self.focus_pane_global(id);
        let viewport = self
            .pane_content_rects
            .iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, rect)| rect.height.saturating_sub(2) as usize)
            .unwrap_or(20);
        if let Some(ViewKind::Diff(view)) = self.views.get_mut(&id) {
            view.selected = selected;
            view.selected_side = note.anchor.side;
            view.range_anchor = Some((note.anchor.side, note.anchor.start_line));
            view.scroll = selected.saturating_sub(viewport.saturating_sub(9));
            view.note_selecting = false;
            view.note_edit_id = Some(note.id);
            view.note_draft = Some(note.body);
        }
    }

    fn toggle_current_diff_note_resolved(&mut self, id: PaneId) {
        let Some(note_id) = self.current_diff_note_ids(id).into_iter().next() else {
            self.show_toast("no review note on this line".to_string());
            return;
        };
        let Some(existing) = self
            .diff
            .notes
            .iter()
            .find(|note| note.id == note_id)
            .cloned()
        else {
            return;
        };
        let mut updated = existing.clone();
        updated.state = if existing.state == crate::diff::NoteState::Resolved {
            crate::diff::NoteState::Open
        } else {
            crate::diff::NoteState::Resolved
        };
        updated.revision = updated.revision.saturating_add(1);
        updated.updated_at_ms = crate::diff::notes::now_ms();
        self.save_diff_note_async(updated, Some(existing.revision));
    }

    fn remove_current_diff_note(&mut self, id: PaneId) {
        let Some(note_id) = self.current_diff_note_ids(id).into_iter().next() else {
            self.show_toast("no review note on this line".to_string());
            return;
        };
        let Some(note) = self
            .diff
            .notes
            .iter()
            .find(|note| note.id == note_id)
            .cloned()
        else {
            return;
        };
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let result = crate::diff::notes::remove(&note, Some(note.revision));
            let _ = tx.send(AppEvent::DiffNoteRemoved {
                id: note.id,
                result,
            });
        });
    }

    fn open_diff_agent_picker(&mut self, id: PaneId) {
        let default_scope = if self.current_diff_note_ids(id).is_empty() {
            crate::diff::DiffSendScope::CurrentFile
        } else {
            crate::diff::DiffSendScope::CurrentNote
        };
        let mut choices = Vec::new();
        for (pane, status) in &self.status {
            if !self.is_agent_pane(*pane) || !self.panes.contains_key(pane) {
                continue;
            }
            let location = self
                .workspaces
                .iter()
                .enumerate()
                .find_map(|(wi, workspace)| {
                    workspace.tabs.iter().enumerate().find_map(|(ti, tab)| {
                        tab.layout
                            .leaves()
                            .contains(pane)
                            .then(|| (wi, ti, workspace.name.clone()))
                    })
                });
            let (workspace_index, tab, workspace) =
                location.unwrap_or((usize::MAX, 0, "unknown".to_string()));
            let name = self
                .agent_name_for(*pane)
                .map(str::to_string)
                .unwrap_or_else(|| format!("p{}", pane.0));
            choices.push((
                workspace_index != self.active_ws,
                workspace_index,
                crate::diff::DiffAgentChoice {
                    pane: *pane,
                    label: format!(
                        "{name} · {} · {} · {workspace}/tab {}",
                        status.agent,
                        status.state.label(),
                        tab + 1
                    ),
                },
            ));
        }
        choices.sort_by_key(|(outside, workspace, choice)| (*outside, *workspace, choice.pane.0));
        let choices: Vec<_> = choices.into_iter().map(|(_, _, choice)| choice).collect();
        if choices.is_empty() {
            self.show_toast("no live agents available".to_string());
            return;
        }
        self.diff_agent_picker = Some(crate::diff::DiffAgentPicker {
            view: id,
            choices,
            cursor: 0,
            scope: default_scope,
        });
    }

    fn picker_note_ids(&self, picker: &crate::diff::DiffAgentPicker) -> Vec<String> {
        let key = self.views.get(&picker.view).and_then(|view| match view {
            ViewKind::Diff(view) => Some(&view.key),
            _ => None,
        });
        match picker.scope {
            crate::diff::DiffSendScope::CurrentNote => self
                .current_diff_note_ids(picker.view)
                .into_iter()
                .take(1)
                .collect(),
            crate::diff::DiffSendScope::SelectedNotes => {
                self.diff.selected_notes.iter().cloned().collect()
            }
            crate::diff::DiffSendScope::CurrentFile => self
                .diff
                .notes
                .iter()
                .filter(|note| Some(&note.anchor.diff_key) == key)
                .filter(|note| note.state == crate::diff::NoteState::Open)
                .map(|note| note.id.clone())
                .collect(),
            crate::diff::DiffSendScope::EntireReview => self
                .diff
                .notes
                .iter()
                .filter(|note| note.state == crate::diff::NoteState::Open)
                .map(|note| note.id.clone())
                .collect(),
        }
    }

    fn handle_diff_agent_picker_key(&mut self, key: KeyEvent) -> bool {
        let Some(picker) = self.diff_agent_picker.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.diff_agent_picker = None,
            KeyCode::Tab => picker.scope = picker.scope.cycle(),
            KeyCode::Up | KeyCode::Char('k') => picker.cursor = picker.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                picker.cursor = (picker.cursor + 1).min(picker.choices.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                let Some(picker) = self.diff_agent_picker.take() else {
                    return true;
                };
                let Some(choice) = picker.choices.get(picker.cursor) else {
                    return true;
                };
                let ids = self.picker_note_ids(&picker);
                match self.deliver_diff_notes(choice.pane, &choice.label, &ids) {
                    Ok(count) => self.show_toast(format!("sent {count} review note(s)")),
                    Err(error) => self.show_toast(format!("review notes not sent: {error}")),
                }
            }
            _ => {}
        }
        true
    }

    pub(crate) fn deliver_diff_notes(
        &mut self,
        target: PaneId,
        target_label: &str,
        ids: &[String],
    ) -> Result<usize, String> {
        if !self.is_agent_pane(target) {
            return Err("target pane is not a running agent".to_string());
        }
        let selected: Vec<_> = self
            .diff
            .notes
            .iter()
            .filter(|note| ids.contains(&note.id))
            .cloned()
            .collect();
        let repo = self
            .diff
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.repo_root.display().to_string())
            .unwrap_or_default();
        let message = crate::diff::message::build_handoff(&repo, &selected)?;
        let pane = self
            .panes
            .get(&target)
            .ok_or_else(|| "target pane closed before delivery".to_string())?;
        pane.try_send_paste(&message)?;
        pane.send_after(b"\r".to_vec(), std::time::Duration::from_millis(45));
        let delivered_at = crate::diff::notes::now_ms();
        for selected_note in &selected {
            if let Some(index) = self
                .diff
                .notes
                .iter()
                .position(|note| note.id == selected_note.id)
            {
                let previous = self.diff.notes[index].revision;
                let mut updated = self.diff.notes[index].clone();
                updated.deliveries.push(crate::diff::notes::NoteDelivery {
                    target: target_label.to_string(),
                    delivered_at_ms: delivered_at,
                });
                updated.revision = updated.revision.saturating_add(1);
                updated.updated_at_ms = delivered_at;
                self.diff.notes[index] = updated.clone();
                self.save_diff_note_async(updated, Some(previous));
            }
        }
        Ok(selected.len())
    }

    fn navigate_diff_file(&mut self, id: PaneId, delta: isize) {
        let Some(ViewKind::Diff(view)) = self.views.get(&id) else {
            return;
        };
        let current = view.key.clone();
        let Some(snapshot) = self.diff.snapshot.as_ref() else {
            return;
        };
        let Some(index) = snapshot.files.iter().position(|file| file.key == current) else {
            return;
        };
        let next = (index as isize + delta)
            .clamp(0, snapshot.files.len().saturating_sub(1) as isize) as usize;
        let key = snapshot.files[next].key.clone();
        if key == current {
            return;
        }
        if let Some(ViewKind::Diff(view)) = self.views.get_mut(&id) {
            view.key = key.clone();
            view.range_anchor = None;
            view.note_draft = None;
            view.note_edit_id = None;
        }
        self.diff.selected_key = Some(key);
        self.schedule_diff_read(id);
    }

    fn navigate_diff_note(&mut self, id: PaneId, delta: isize) {
        let Some(ViewKind::Diff(view)) = self.views.get(&id) else {
            return;
        };
        let mut rows: Vec<usize> = self
            .diff
            .notes
            .iter()
            .filter(|note| {
                note.anchor.diff_key == view.key && note.state != crate::diff::NoteState::Resolved
            })
            .filter_map(|note| {
                view.stack_rows.iter().position(|line| {
                    source_anchor(line).is_some_and(|(side, number)| {
                        side == note.anchor.side
                            && number >= note.anchor.start_line
                            && number <= note.anchor.end_line
                    })
                })
            })
            .collect();
        rows.sort_unstable();
        rows.dedup();
        let pane_width = self
            .pane_content_rects
            .iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, rect)| rect.width)
            .unwrap_or(80);
        let marker_style = self.config.layout.diff_marker_style;
        let is_split = view.effective_split(pane_width);
        let next = if delta > 0 {
            rows.into_iter().find(|row| *row > view.selected)
        } else {
            rows.into_iter().rev().find(|row| *row < view.selected)
        };
        if let (Some(row), Some(ViewKind::Diff(view))) = (next, self.views.get_mut(&id)) {
            view.selected = row;
            view.scroll = row.saturating_sub(2);
            view.ensure_horizontal_visible(pane_width, marker_style, is_split);
        }
    }

    pub fn handle_diff_key(&mut self, id: PaneId, key: KeyEvent) -> bool {
        if self.diff_agent_picker.is_some() {
            return self.handle_diff_agent_picker_key(key);
        }
        let current_anchor = self
            .selected_diff_source(id)
            .map(|(side, line, _)| (side, line));
        let viewport = self
            .pane_content_rects
            .iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, rect)| rect.height.saturating_sub(2) as usize)
            .unwrap_or(20);
        let pane_width = self
            .pane_content_rects
            .iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, rect)| rect.width)
            .unwrap_or(80);
        let marker_style = self.config.layout.diff_marker_style;
        let is_split = {
            let Some(ViewKind::Diff(view)) = self.views.get(&id) else {
                return false;
            };
            view.effective_split(pane_width)
        };

        enum Deferred {
            None,
            Save(String),
            Refresh,
            Viewed,
            Filter,
            SelectNote,
            EditNote,
            ResolveNote,
            RemoveNote,
            Send,
            Context(i16),
            File(isize),
            Note(isize),
            Close,
        }
        let mut deferred = Deferred::None;
        {
            let Some(ViewKind::Diff(view)) = self.views.get_mut(&id) else {
                return false;
            };
            if view.note_draft.is_some() {
                match key.code {
                    KeyCode::Char(c) => {
                        if let Some(draft) = view.note_draft.as_mut() {
                            draft.push(c);
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(draft) = view.note_draft.as_mut() {
                            draft.pop();
                        }
                    }
                    KeyCode::Enter
                        if key
                            .modifiers
                            .contains(ratatui::crossterm::event::KeyModifiers::SHIFT) =>
                    {
                        if let Some(draft) = view.note_draft.as_mut() {
                            draft.push('\n');
                        }
                    }
                    KeyCode::Enter => {
                        let body = view.note_draft.take().unwrap_or_default();
                        if !body.trim().is_empty() {
                            deferred = Deferred::Save(body);
                        } else if view.note_edit_id.is_none() {
                            view.range_anchor = None;
                        }
                    }
                    KeyCode::Esc => {
                        view.note_draft = None;
                        if view.note_edit_id.take().is_none() {
                            view.range_anchor = None;
                        }
                    }
                    _ => return false,
                }
            } else if view.note_selecting {
                let max = view.stack_rows.len().saturating_sub(1);
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        view.selected = (view.selected + 1).min(max)
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        view.selected = view.selected.saturating_sub(1)
                    }
                    KeyCode::Char('d') | KeyCode::PageDown => {
                        view.selected = (view.selected + viewport / 2).min(max)
                    }
                    KeyCode::Char('u') | KeyCode::PageUp => {
                        view.selected = view.selected.saturating_sub(viewport / 2)
                    }
                    KeyCode::Char('g') | KeyCode::Home => view.selected = 0,
                    KeyCode::Char('G') | KeyCode::End => view.selected = max,
                    KeyCode::Left => view.selected_side = crate::diff::DiffSide::Old,
                    KeyCode::Right => view.selected_side = crate::diff::DiffSide::New,
                    KeyCode::Enter => {
                        if selected_view_anchor(view).is_some() {
                            view.note_selecting = false;
                            view.note_draft = Some(String::new());
                            view.scroll = view.selected.saturating_sub(viewport.saturating_sub(9));
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        view.note_selecting = false;
                        view.range_anchor = None;
                    }
                    _ => return false,
                }
                if view.selected < view.scroll {
                    view.scroll = view.selected;
                } else if view.selected >= view.scroll.saturating_add(viewport) {
                    view.scroll = view.selected.saturating_sub(viewport.saturating_sub(1));
                }
                view.ensure_horizontal_visible(pane_width, marker_style, is_split);
            } else if view.search_editing {
                match key.code {
                    KeyCode::Char(c) => view.search.get_or_insert_with(String::new).push(c),
                    KeyCode::Backspace => {
                        view.search.get_or_insert_with(String::new).pop();
                    }
                    KeyCode::Enter => {
                        view.search_editing = false;
                        if let Some(query) =
                            view.search.as_deref().filter(|query| !query.is_empty())
                        {
                            if let Some(index) = view
                                .stack_rows
                                .iter()
                                .position(|line| line.text.contains(query))
                            {
                                view.selected = index;
                                view.scroll = index.saturating_sub(viewport / 2);
                                view.ensure_horizontal_visible(pane_width, marker_style, is_split);
                            }
                        }
                    }
                    KeyCode::Esc => {
                        view.search = None;
                        view.search_editing = false;
                    }
                    _ => return false,
                }
            } else {
                let row_count = view.stack_rows.len();
                let max = row_count.saturating_sub(1);
                let old_selected = view.selected;
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        view.selected = (view.selected + 1).min(max)
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        view.selected = view.selected.saturating_sub(1)
                    }
                    KeyCode::Char('d') | KeyCode::PageDown => {
                        view.selected = (view.selected + viewport / 2).min(max)
                    }
                    KeyCode::Char('u') | KeyCode::PageUp => {
                        view.selected = view.selected.saturating_sub(viewport / 2)
                    }
                    KeyCode::Char('g') | KeyCode::Home => view.selected = 0,
                    KeyCode::Char('G') | KeyCode::End => view.selected = max,
                    KeyCode::Left => view.selected_side = crate::diff::DiffSide::Old,
                    KeyCode::Right => view.selected_side = crate::diff::DiffSide::New,
                    KeyCode::Char('h') => view.horizontal = view.horizontal.saturating_sub(8),
                    KeyCode::Char('l') => view.horizontal = view.horizontal.saturating_add(8),
                    KeyCode::Char('s') => {
                        // When Auto resolves to Split (wide enough, no wrap),
                        // skip directly to Stack so the user doesn't need to
                        // press 's' twice to see a visual change.
                        let was_effectively_split = !view.wrap
                            && matches!(
                                view.preference,
                                crate::diff::DiffLayoutPreference::Split
                                    | crate::diff::DiffLayoutPreference::Auto
                            )
                            && pane_width >= 96;
                        if was_effectively_split
                            && view.preference == crate::diff::DiffLayoutPreference::Auto
                        {
                            view.preference = crate::diff::DiffLayoutPreference::Stack;
                        } else {
                            view.preference = view.preference.cycle();
                        }
                    }
                    KeyCode::Char('w') => view.wrap = !view.wrap,
                    KeyCode::Char('+') | KeyCode::Char('=') => deferred = Deferred::Context(1),
                    KeyCode::Char('-') => deferred = Deferred::Context(-1),
                    KeyCode::Char('J') => deferred = Deferred::File(1),
                    KeyCode::Char('K') => deferred = Deferred::File(-1),
                    KeyCode::Char('N') => deferred = Deferred::Note(1),
                    KeyCode::Char('P') => deferred = Deferred::Note(-1),
                    KeyCode::Char('}') => {
                        if let Some(next) = view
                            .stack_rows
                            .iter()
                            .enumerate()
                            .skip(view.selected.saturating_add(1))
                            .find_map(|(index, line)| {
                                (line.kind == crate::diff::DiffLineKind::Header).then_some(index)
                            })
                        {
                            view.selected = next;
                            view.ensure_horizontal_visible(pane_width, marker_style, is_split);
                        }
                    }
                    KeyCode::Char('{') => {
                        if let Some(previous) = view
                            .stack_rows
                            .iter()
                            .enumerate()
                            .take(view.selected)
                            .rev()
                            .find_map(|(index, line)| {
                                (line.kind == crate::diff::DiffLineKind::Header).then_some(index)
                            })
                        {
                            view.selected = previous;
                            view.ensure_horizontal_visible(pane_width, marker_style, is_split);
                        }
                    }
                    KeyCode::Char('/') => {
                        view.search = Some(String::new());
                        view.search_editing = true;
                    }
                    KeyCode::Char('v') => {
                        view.range_anchor = match (view.range_anchor, current_anchor) {
                            (Some(_), _) => None,
                            (None, anchor) => anchor,
                        };
                    }
                    KeyCode::Char('n') => {
                        view.note_selecting = true;
                        view.note_edit_id = None;
                        view.range_anchor = current_anchor;
                    }
                    KeyCode::Char(' ') => deferred = Deferred::SelectNote,
                    KeyCode::Char('e') => deferred = Deferred::EditNote,
                    KeyCode::Char('x') => deferred = Deferred::ResolveNote,
                    KeyCode::Char('D') => deferred = Deferred::RemoveNote,
                    KeyCode::Char('a') => deferred = Deferred::Send,
                    KeyCode::Char('r') => deferred = Deferred::Refresh,
                    KeyCode::Char('m') => deferred = Deferred::Viewed,
                    KeyCode::Char('f') => deferred = Deferred::Filter,
                    KeyCode::Char('q') => deferred = Deferred::Close,
                    KeyCode::Esc => {
                        if view.search.take().is_none() {
                            deferred = Deferred::Close;
                        }
                    }
                    _ => return false,
                }
                if view.selected < view.scroll {
                    view.scroll = view.selected;
                } else if view.selected >= view.scroll.saturating_add(viewport) {
                    view.scroll = view.selected.saturating_sub(viewport.saturating_sub(1));
                }
                // Recompute effective split after s/w may have changed
                // preference or wrap.
                let now_split = view.effective_split(pane_width);
                if view.selected != old_selected
                    || now_split != is_split
                    || matches!(key.code, KeyCode::Char('h' | 'l'))
                {
                    view.ensure_horizontal_visible(pane_width, marker_style, now_split);
                }
            }
        }
        match deferred {
            Deferred::None => {}
            Deferred::Save(body) => self.commit_diff_note(id, body),
            Deferred::Refresh => self.schedule_diff_read(id),
            Deferred::Viewed => self.mark_active_diff_viewed(),
            Deferred::Filter => {
                self.diff.filter = self.diff.filter.cycle();
                self.diff.rebuild_rows();
            }
            Deferred::SelectNote => self.toggle_current_diff_note_selection(id),
            Deferred::EditNote => self.edit_current_diff_note(id),
            Deferred::ResolveNote => self.toggle_current_diff_note_resolved(id),
            Deferred::RemoveNote => self.remove_current_diff_note(id),
            Deferred::Send => self.open_diff_agent_picker(id),
            Deferred::Context(delta) => {
                if let Some(ViewKind::Diff(view)) = self.views.get_mut(&id) {
                    view.context_lines = (view.context_lines as i32 + delta as i32)
                        .clamp(0, i32::from(crate::diff::MAX_CONTEXT_LINES))
                        as u16;
                }
                self.schedule_diff_read(id);
            }
            Deferred::File(delta) => self.navigate_diff_file(id, delta),
            Deferred::Note(delta) => self.navigate_diff_note(id, delta),
            Deferred::Close => self.close_pane(id),
        }
        true
    }
}

/// Canonicalize the longest existing prefix, retaining a missing filename or
/// directory tail. Git can report deleted paths, while macOS may spell the
/// workspace and `git rev-parse` root through different `/var` aliases.
fn canonicalize_with_missing_tail(path: &Path) -> PathBuf {
    let mut cursor = path;
    let mut tail = Vec::new();
    loop {
        if let Ok(mut resolved) = cursor.canonicalize() {
            for component in tail.iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        let (Some(name), Some(parent)) = (cursor.file_name(), cursor.parent()) else {
            return path.to_path_buf();
        };
        tail.push(name.to_os_string());
        cursor = parent;
    }
}

fn apply_note_counts(snapshot: &mut crate::diff::DiffSnapshot, notes: &[crate::diff::ReviewNote]) {
    let mut counts: HashMap<&crate::diff::DiffKey, usize> = HashMap::new();
    for note in notes {
        if note.state != crate::diff::NoteState::Resolved {
            *counts.entry(&note.anchor.diff_key).or_default() += 1;
        }
    }
    for file in &mut snapshot.files {
        file.unresolved_notes = counts.get(&file.key).copied().unwrap_or(0);
    }
}

fn source_anchor(line: &crate::diff::DiffLine) -> Option<(crate::diff::DiffSide, u32)> {
    line.new_line
        .map(|number| (crate::diff::DiffSide::New, number))
        .or_else(|| {
            line.old_line
                .map(|number| (crate::diff::DiffSide::Old, number))
        })
}

fn source_anchor_on_side(
    line: &crate::diff::DiffLine,
    side: crate::diff::DiffSide,
) -> Option<(crate::diff::DiffSide, u32)> {
    match side {
        crate::diff::DiffSide::Old => line
            .old_line
            .map(|number| (crate::diff::DiffSide::Old, number)),
        crate::diff::DiffSide::New => line
            .new_line
            .map(|number| (crate::diff::DiffSide::New, number)),
    }
}

fn selected_view_anchor(view: &crate::diff::DiffView) -> Option<(crate::diff::DiffSide, u32)> {
    let line = view.stack_rows.get(view.selected)?;
    match view.selected_side {
        crate::diff::DiffSide::Old => line
            .old_line
            .map(|number| (crate::diff::DiffSide::Old, number)),
        crate::diff::DiffSide::New => line
            .new_line
            .map(|number| (crate::diff::DiffSide::New, number)),
    }
    .or_else(|| source_anchor(line))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;

    use super::*;
    use crate::diff::model::{DiffHunk, FileDiff};
    use crate::diff::{
        DiffFile, DiffFileStatus, DiffLayer, DiffLine, DiffLineKind, DiffLoad, DiffSnapshot,
        RepoPath,
    };

    fn run_git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn install_snapshot(app: &mut App) -> DiffKey {
        let path = RepoPath::from_path(Path::new("src/lib.rs")).unwrap();
        let key = DiffKey {
            repo_id: "repo".into(),
            worktree_id: "tree".into(),
            layer: DiffLayer::Worktree,
            old_path: Some(path.clone()),
            new_path: Some(path),
        };
        app.diff.snapshot = Some(DiffSnapshot {
            generation: 1,
            fingerprint: "snapshot".into(),
            repo_id: "repo".into(),
            worktree_id: "tree".into(),
            visible_root: app.ws().cwd.clone(),
            repo_root: app.ws().cwd.clone(),
            branch: "main".into(),
            files: vec![DiffFile {
                key: key.clone(),
                status: DiffFileStatus::Modified,
                additions: Some(1),
                deletions: Some(1),
                binary: false,
                unresolved_notes: 0,
                viewed_fingerprint: None,
                fingerprint: "file".into(),
            }],
            omitted_files: 0,
        });
        app.diff.status_root = Some(app.ws().cwd.clone());
        app.diff.rebuild_rows();
        key
    }

    fn add_snapshot_file(app: &mut App, path: &str) -> DiffKey {
        let path = RepoPath::from_path(Path::new(path)).unwrap();
        let snapshot = app.diff.snapshot.as_mut().expect("installed snapshot");
        let key = DiffKey {
            repo_id: snapshot.repo_id.clone(),
            worktree_id: snapshot.worktree_id.clone(),
            layer: DiffLayer::Worktree,
            old_path: Some(path.clone()),
            new_path: Some(path),
        };
        snapshot.files.push(DiffFile {
            key: key.clone(),
            status: DiffFileStatus::Modified,
            additions: Some(1),
            deletions: Some(1),
            binary: false,
            unresolved_notes: 0,
            viewed_fingerprint: None,
            fingerprint: format!("file-{}", snapshot.files.len()),
        });
        app.diff.rebuild_rows();
        key
    }

    fn add_test_workspace(app: &mut App, cwd: PathBuf) -> usize {
        let id = PaneId::alloc();
        app.workspaces.push(crate::app::Workspace {
            id: crate::ids::public_id("workspace"),
            name: cwd
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace")
                .to_string(),
            cwd,
            branch: None,
            git_ahead_behind: None,
            worktree: None,
            tabs: vec![Tab::panes(TileLayout::new(id))],
            active_tab: 0,
            pinned: false,
        });
        app.workspaces.len() - 1
    }

    #[test]
    fn diff_api_status_scan_is_off_loop_and_cached_lists_answer_immediately() {
        let _env = crate::persist::test_env("diff-api-off-loop");
        let repo = PathBuf::from(std::env::var_os("LUVUS_HOME").unwrap()).join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["config", "user.name", "Luvus Test"]);
        run_git(&repo, &["config", "user.email", "luvus@example.invalid"]);
        std::fs::write(repo.join("file.txt"), "base\n").unwrap();
        run_git(&repo, &["add", "file.txt"]);
        run_git(&repo, &["commit", "-q", "-m", "base"]);
        std::fs::write(repo.join("file.txt"), "changed\n").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.workspaces[0].cwd = repo;
        let (reply, response) = std::sync::mpsc::channel();
        let request = crate::ipc::api::ApiRequest {
            id: "first-list".to_string(),
            method: "diff.list".to_string(),
            params: serde_json::json!({}),
            reply,
        };

        assert!(
            app.prepare_diff_api(request).is_none(),
            "first list is parked"
        );
        assert!(app.diff.status_inflight, "the worker scan is in flight");
        assert!(
            response
                .recv_timeout(std::time::Duration::from_millis(10))
                .is_err(),
            "the reply waits for the app loop to apply the worker result"
        );
        assert!(app.dispatch("ping", &serde_json::json!({})).is_ok());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let event = rx
                .recv_timeout(std::time::Duration::from_millis(250))
                .expect("status worker event");
            let completed = matches!(&event, AppEvent::DiffStatus { .. });
            app.handle_event(event);
            if completed {
                break;
            }
        }
        let first: serde_json::Value = serde_json::from_str(
            &response
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("parked list response"),
        )
        .unwrap();
        assert_eq!(first["result"]["files"][0]["path"], "file.txt");
        assert_eq!(first["result"]["refreshing"], false);

        let (reply, _response) = std::sync::mpsc::channel();
        let cached = crate::ipc::api::ApiRequest {
            id: "cached-list".to_string(),
            method: "diff.list".to_string(),
            params: serde_json::json!({}),
            reply,
        };
        assert!(
            app.prepare_diff_api(cached).is_some(),
            "a matching cached snapshot never waits on Git"
        );
    }

    #[test]
    fn opening_diff_from_context_menu_creates_a_native_leaf_without_a_pty() {
        let _env = crate::persist::test_env("diff-native-view");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let key = install_snapshot(&mut app);
        let pane_count = app.panes.len();

        app.open_diff_menu(1, 10, 5);
        assert_eq!(app.diff_menu.as_ref().map(|menu| &menu.key), Some(&key));
        app.diff_menu_action(crate::app::DiffMenuItem::OpenPane);

        assert_eq!(app.panes.len(), pane_count);
        assert!(app
            .views
            .values()
            .any(|view| matches!(view, ViewKind::Diff(diff) if diff.key == key)));
    }

    #[test]
    fn matching_diff_views_are_reused_only_inside_the_active_workspace() {
        let _env = crate::persist::test_env("diff-view-workspace-scope");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let key = install_snapshot(&mut app);
        app.open_diff_view(key.clone(), OpenTarget::Tab);
        let first_view = app.diff_view_showing(&key).expect("workspace A view");
        let workspace_b = add_test_workspace(&mut app, PathBuf::from("/workspace-b"));

        app.active_ws = workspace_b;
        assert_eq!(
            app.diff_view_showing(&key),
            None,
            "workspace A's view must not steal focus from workspace B"
        );

        app.open_diff_view(key.clone(), OpenTarget::Tab);
        let second_view = app.diff_view_showing(&key).expect("workspace B view");
        assert_ne!(second_view, first_view);
        assert_eq!(app.active_ws, workspace_b);
    }

    #[test]
    fn diff_preview_is_reused_only_inside_its_workspace() {
        let _env = crate::persist::test_env("diff-preview-workspace-scope");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        let key = install_snapshot(&mut app);
        app.open_diff_view(key.clone(), OpenTarget::Preview);
        let preview_a = app.active_preview_view().expect("workspace A preview");

        let workspace_b = add_test_workspace(&mut app, PathBuf::from("/workspace-b"));
        app.active_ws = workspace_b;
        install_snapshot(&mut app);
        app.open_diff_view(key, OpenTarget::Preview);
        let preview_b = app.active_preview_view().expect("workspace B preview");

        assert_eq!(app.active_ws, workspace_b, "opening B must not focus A");
        assert_ne!(preview_b, preview_a, "each workspace owns its preview");
        assert_eq!(app.preview_views.len(), 2);
        assert!(app.workspaces[0]
            .tabs
            .iter()
            .any(|tab| tab.layout.contains(preview_a)));
        assert!(app.workspaces[workspace_b]
            .tabs
            .iter()
            .any(|tab| tab.layout.contains(preview_b)));

        app.active_ws = 0;
        assert_eq!(app.active_preview_view(), Some(preview_a));
    }

    #[test]
    fn preview_click_replaces_the_focused_diff_tab_in_place() {
        let _env = crate::persist::test_env("diff-focused-tab-preview");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let preview_key = install_snapshot(&mut app);
        let tab_key = add_snapshot_file(&mut app, "src/tab.rs");
        let next_key = add_snapshot_file(&mut app, "src/next.rs");

        app.open_diff_view(preview_key, OpenTarget::Preview);
        let old_preview = app.active_preview_view().expect("initial preview");
        app.open_diff_view(tab_key, OpenTarget::Tab);
        let tab_index = app.workspaces[app.active_ws].active_tab;
        let tab_view = app.layout().focus;
        assert_ne!(tab_view, old_preview);

        app.open_diff_view(next_key.clone(), OpenTarget::Preview);

        assert_eq!(app.workspaces[app.active_ws].active_tab, tab_index);
        assert_eq!(app.layout().focus, tab_view);
        assert_eq!(app.active_preview_view(), Some(tab_view));
        assert!(!app.preview_views.contains(&old_preview));
        assert!(matches!(
            app.views.get(&tab_view),
            Some(ViewKind::Diff(view)) if view.key == next_key
        ));
    }

    #[test]
    fn dashboard_diff_click_opens_a_tab_then_reuses_it() {
        let _env = crate::persist::test_env("diff-dashboard-tab");

        for dashboard in ["git", "orch", "mission"] {
            let (tx, _rx) = std::sync::mpsc::channel();
            let mut app = App::new(120, 40, tx).unwrap();
            let pane_key = install_snapshot(&mut app);
            let first_key = add_snapshot_file(&mut app, "src/dedicated.rs");
            let next_key = add_snapshot_file(&mut app, &format!("src/{dashboard}.rs"));

            app.open_diff_view(pane_key, OpenTarget::Preview);
            let normal_tab = app.ws().active_tab;
            let pane_diff = app.layout().focus;
            assert!(app.layout().len() > 1, "normal tab contains a DIFF pane");

            match dashboard {
                "git" => app.open_git_tab(app.active_ws),
                "orch" => app.open_orch_board(),
                "mission" => app.open_mission_control(app.active_ws),
                _ => unreachable!(),
            }
            assert!(
                app.active_is_git() || app.active_is_orch() || app.active_is_mission(),
                "{dashboard} dashboard opened"
            );
            let dashboard_tab = app.ws().active_tab;
            let before = app.ws().tabs.len();

            app.open_diff_view(first_key, OpenTarget::Preview);

            assert_eq!(app.ws().tabs.len(), before + 1, "{dashboard}: new tab");
            assert_ne!(app.ws().active_tab, dashboard_tab);
            assert_ne!(app.ws().active_tab, normal_tab);
            let diff_tab = app.ws().active_tab;
            let diff_id = app.layout().focus;
            assert_ne!(diff_id, pane_diff, "do not reuse the normal-tab pane");
            assert!(matches!(app.views.get(&diff_id), Some(ViewKind::Diff(_))));

            app.workspaces[app.active_ws].active_tab = dashboard_tab;
            app.open_diff_view(next_key.clone(), OpenTarget::Preview);

            assert_eq!(
                app.ws().tabs.len(),
                before + 1,
                "{dashboard}: reuse the existing DIFF tab"
            );
            assert_eq!(app.ws().active_tab, diff_tab);
            assert_eq!(app.layout().focus, diff_id);
            assert!(matches!(
                app.views.get(&diff_id),
                Some(ViewKind::Diff(view)) if view.key == next_key
            ));
        }
    }

    #[test]
    fn normal_tab_diff_click_opens_a_pane_in_the_current_tab() {
        let _env = crate::persist::test_env("diff-normal-tab-pane");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let key = install_snapshot(&mut app);
        let tab = app.ws().active_tab;
        let tabs_before = app.ws().tabs.len();
        let leaves_before = app.layout().len();

        app.open_diff_view(key.clone(), OpenTarget::Preview);

        assert_eq!(app.ws().active_tab, tab);
        assert_eq!(app.ws().tabs.len(), tabs_before);
        assert_eq!(app.layout().len(), leaves_before + 1);
        assert!(matches!(
            app.views.get(&app.layout().focus),
            Some(ViewKind::Diff(view)) if view.key == key
        ));
    }

    #[test]
    fn changing_workspace_replaces_an_inflight_status_scan_and_clears_stale_rows() {
        let _env = crate::persist::test_env("diff-workspace-refresh");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        install_snapshot(&mut app);
        let old_root = PathBuf::from("/workspace-a");
        app.workspaces[0].cwd = old_root.clone();
        if let Some(snapshot) = app.diff.snapshot.as_mut() {
            snapshot.visible_root = old_root.clone();
        }
        app.diff.status_root = Some(old_root);
        app.diff.status_generation = 41;
        app.diff.status_inflight = true;
        app.git_status_inflight = true;

        let current_root = std::env::current_dir().unwrap();
        let workspace_b = add_test_workspace(&mut app, current_root.clone());
        app.active_ws = workspace_b;
        app.refresh_diff_status(false);

        assert_eq!(app.diff.status_generation, 42);
        assert!(app.diff.status_inflight);
        assert!(app.git_status_inflight);
        assert_eq!(
            app.diff.status_root.as_deref(),
            Some(current_root.as_path())
        );
        assert!(app.diff.snapshot.is_none());
        assert!(app.diff.rows.is_empty());
        assert!(app.diff.selected_key.is_none());
    }

    #[test]
    fn activating_a_stale_diff_row_refreshes_without_leaving_the_workspace() {
        let _env = crate::persist::test_env("diff-stale-row-activation");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        install_snapshot(&mut app);
        let old_root = PathBuf::from("/workspace-a");
        app.workspaces[0].cwd = old_root.clone();
        if let Some(snapshot) = app.diff.snapshot.as_mut() {
            snapshot.visible_root = old_root.clone();
        }
        app.diff.status_root = Some(old_root);
        let stale_row = app
            .diff
            .rows
            .iter()
            .position(|row| matches!(row, DiffListRow::File(_)))
            .expect("stale file row");
        let view_count = app.views.len();

        let workspace_b = add_test_workspace(&mut app, std::env::current_dir().unwrap());
        app.active_ws = workspace_b;
        app.diff_row_activate(stale_row, OpenTarget::Preview);

        assert_eq!(app.active_ws, workspace_b);
        assert_eq!(app.views.len(), view_count);
        assert!(app.diff.snapshot.is_none());
        assert!(app.diff.rows.is_empty());
        assert!(app.diff.status_inflight);
    }

    #[test]
    fn note_mode_supports_single_line_click_and_multi_line_drag() {
        let _env = crate::persist::test_env("diff-source-click");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(180, 40, tx).unwrap();
        let key = install_snapshot(&mut app);
        app.open_diff_view(key.clone(), OpenTarget::Tab);
        let id = app.layout().focus;
        let old = DiffLine {
            kind: DiffLineKind::Deletion,
            old_line: Some(7),
            new_line: None,
            text: "old".into(),
        };
        let new = DiffLine {
            kind: DiffLineKind::Addition,
            old_line: None,
            new_line: Some(8),
            text: "new".into(),
        };
        let new_second = DiffLine {
            kind: DiffLineKind::Addition,
            old_line: None,
            new_line: Some(9),
            text: "new second".into(),
        };
        let file_diff = FileDiff {
            key,
            status: DiffFileStatus::Modified,
            additions: 2,
            deletions: 1,
            binary: false,
            truncated: false,
            omitted_lines: 0,
            hunks: vec![DiffHunk {
                id: "hunk".into(),
                old_start: 7,
                new_start: 8,
                header: "@@ -7 +8 @@".into(),
                lines: vec![old, new, new_second],
            }],
        };
        let stack_rows = crate::diff::rows::stack_rows(&file_diff);
        let split_rows = crate::diff::rows::split_rows(&file_diff);
        let Some(ViewKind::Diff(view)) = app.views.get_mut(&id) else {
            panic!("native DIFF view");
        };
        view.preference = crate::diff::DiffLayoutPreference::Split;
        view.stack_rows = stack_rows;
        view.split_rows = split_rows;
        view.rebuild_row_indices();
        view.horizontal = usize::MAX;
        view.load = DiffLoad::Ready(Box::new(file_diff));

        let mut terminal = Terminal::new(TestBackend::new(180, 40)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let mut new_hits: Vec<_> = app
            .diff_source_rects
            .iter()
            .filter(|(pane, _, side, _)| *pane == id && *side == crate::diff::DiffSide::New)
            .copied()
            .collect();
        new_hits.sort_by_key(|(_, row, _, _)| *row);
        let (_, first_row, _, first_rect) = new_hits[0];
        let (_, last_row, _, last_rect) = *new_hits.last().expect("second new-side source hit");
        assert!(app.handle_diff_key(id, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)));
        assert!(matches!(
            app.views.get(&id),
            Some(ViewKind::Diff(view)) if view.note_selecting && view.note_draft.is_none()
        ));
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: first_rect.x + 1,
            row: first_rect.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(matches!(
            app.views.get(&id),
            Some(ViewKind::Diff(view))
                if view.selected == first_row
                    && view.note_selecting
                    && view.note_draft.is_none()
                    && view.range_anchor == Some((crate::diff::DiffSide::New, 8))
        ));
        assert_eq!(app.diff_note_drag, Some(id));

        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: last_rect.x + 1,
            row: last_rect.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(matches!(
            app.views.get(&id),
            Some(ViewKind::Diff(view))
                if view.selected == last_row
                    && view.note_selecting
                    && view.note_draft.is_none()
                    && view.range_anchor == Some((crate::diff::DiffSide::New, 8))
        ));

        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: last_rect.x + 1,
            row: last_rect.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.diff_note_drag, None);
        assert!(matches!(
            app.views.get(&id),
            Some(ViewKind::Diff(view))
                if view.selected == last_row
                    && view.note_draft.as_deref() == Some("")
                    && view.horizontal < view.stack_rows[last_row].text.chars().count()
        ));

        // A press and release on the same row is the one-line form of the same
        // gesture and opens the editor without inventing a range.
        assert!(app.handle_diff_key(id, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(app.handle_diff_key(id, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)));
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.handle_event(AppEvent::Mouse(MouseEvent {
                kind,
                column: last_rect.x + 1,
                row: last_rect.y,
                modifiers: KeyModifiers::NONE,
            }));
        }
        assert!(matches!(
            app.views.get(&id),
            Some(ViewKind::Diff(view))
                if view.note_draft.as_deref() == Some("")
                    && view.range_anchor == Some((crate::diff::DiffSide::New, 9))
        ));
    }

    #[test]
    fn repeated_horizontal_navigation_stays_inside_the_selected_line() {
        let _env = crate::persist::test_env("diff-horizontal-keys");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(40, 20, tx).unwrap();
        let key = install_snapshot(&mut app);
        app.open_diff_view(key, OpenTarget::Tab);
        let id = app.layout().focus;
        app.pane_content_rects = vec![(id, Rect::new(0, 0, 20, 12))];
        let Some(ViewKind::Diff(view)) = app.views.get_mut(&id) else {
            panic!("native DIFF view");
        };
        view.preference = crate::diff::DiffLayoutPreference::Stack;
        view.stack_rows = vec![DiffLine {
            kind: DiffLineKind::Context,
            old_line: Some(1),
            new_line: Some(1),
            text: "abcdefghijklmnopqrstuvwxyz".into(),
        }];

        for _ in 0..20 {
            assert!(app.handle_diff_key(id, KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
        }
        let max = match app.views.get(&id) {
            Some(ViewKind::Diff(view)) => view.horizontal,
            _ => unreachable!(),
        };
        assert!(max < 26, "selected line retains visible text");

        for _ in 0..20 {
            assert!(app.handle_diff_key(id, KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)));
        }
        assert!(matches!(
            app.views.get(&id),
            Some(ViewKind::Diff(view)) if view.horizontal == 0
        ));
    }

    #[test]
    fn split_wheel_scroll_maps_visible_rows_back_to_stack_selection() {
        let _env = crate::persist::test_env("diff-split-wheel");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 20, tx).unwrap();
        let key = install_snapshot(&mut app);
        app.open_diff_view(key.clone(), OpenTarget::Tab);
        let id = app.layout().focus;
        let file_diff = FileDiff {
            key,
            status: DiffFileStatus::Modified,
            additions: 1,
            deletions: 2,
            binary: false,
            truncated: false,
            omitted_lines: 0,
            hunks: vec![DiffHunk {
                id: "hunk".into(),
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
        let Some(ViewKind::Diff(view)) = app.views.get_mut(&id) else {
            panic!("native DIFF view");
        };
        view.preference = crate::diff::DiffLayoutPreference::Split;
        view.stack_rows = crate::diff::rows::stack_rows(&file_diff);
        view.split_rows = crate::diff::rows::split_rows(&file_diff);
        view.selected_side = crate::diff::DiffSide::New;
        view.rebuild_row_indices();
        view.load = DiffLoad::Ready(Box::new(file_diff));
        app.pane_content_rects = vec![(id, Rect::new(0, 0, 120, 4))];

        assert!(app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        })));
        assert!(matches!(
            app.views.get(&id),
            Some(ViewKind::Diff(view)) if view.selected == 3 && view.scroll == 3
        ));
    }

    #[test]
    fn clicking_saved_note_card_opens_that_note_in_the_inline_editor() {
        let _env = crate::persist::test_env("diff-note-card-click");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 32, tx).unwrap();
        let key = install_snapshot(&mut app);
        app.open_diff_view(key.clone(), OpenTarget::Tab);
        let id = app.layout().focus;
        let changed = DiffLine {
            kind: DiffLineKind::Addition,
            old_line: None,
            new_line: Some(8),
            text: "let corrected = true;".into(),
        };
        let file_diff = FileDiff {
            key: key.clone(),
            status: DiffFileStatus::Modified,
            additions: 1,
            deletions: 0,
            binary: false,
            truncated: false,
            omitted_lines: 0,
            hunks: vec![DiffHunk {
                id: "hunk".into(),
                old_start: 7,
                new_start: 8,
                header: "@@ -7,0 +8 @@".into(),
                lines: vec![changed],
            }],
        };
        let stack_rows = crate::diff::rows::stack_rows(&file_diff);
        let split_rows = crate::diff::rows::split_rows(&file_diff);
        let selected = stack_rows
            .iter()
            .position(|line| line.new_line == Some(8))
            .expect("new source row");
        let Some(ViewKind::Diff(view)) = app.views.get_mut(&id) else {
            panic!("native DIFF view");
        };
        view.preference = crate::diff::DiffLayoutPreference::Stack;
        view.stack_rows = stack_rows;
        view.split_rows = split_rows;
        view.load = DiffLoad::Ready(Box::new(file_diff));
        app.diff.notes.push(crate::diff::ReviewNote {
            id: "clicked-note".into(),
            review_id: "review".into(),
            author: "user".into(),
            kind: crate::diff::NoteKind::Issue,
            body: "Please keep this behavior".into(),
            anchor: crate::diff::notes::NoteAnchor {
                diff_key: key,
                side: crate::diff::DiffSide::New,
                start_line: 8,
                end_line: 8,
                context: "let corrected = true;".into(),
                context_sha256: "hash".into(),
            },
            state: crate::diff::NoteState::Open,
            deliveries: Vec::new(),
            revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
        });

        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let (_, note_id, rect) = app
            .diff_note_rects
            .iter()
            .find(|(pane, note_id, _)| *pane == id && note_id == "clicked-note")
            .cloned()
            .expect("visible note card hit target");

        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y + 1,
            modifiers: KeyModifiers::NONE,
        }));

        assert_eq!(note_id, "clicked-note");
        assert!(matches!(
            app.views.get(&id),
            Some(ViewKind::Diff(view))
                if view.selected == selected
                    && view.selected_side == crate::diff::DiffSide::New
                    && view.note_edit_id.as_deref() == Some("clicked-note")
                    && view.note_draft.as_deref() == Some("Please keep this behavior")
                    && view.range_anchor == Some((crate::diff::DiffSide::New, 8))
        ));
    }
}
