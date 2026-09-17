//! Content JSON API handlers.

use super::*;
use super::{params::*, projection::*};

impl App {
    pub(super) fn api_search_capabilities(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        Ok(json!({
            "type": "search_capabilities",
            "version": 1,
            "methods": ["search.query", "search.activate"],
            "scopes": ["all", "navigate", "files", "output"],
            "max_results": crate::search::RESULT_CAP,
            "max_response_bytes": crate::search::federation::MAX_SESSION_RESPONSE_BYTES,
        }))
    }

    // Global scrollback search (docs/63): scan every pane's retained
    // output. Returns matches with the scroll offset that lands on each,
    // plus the total found (which may exceed the returned, capped, list).
    pub(super) fn api_search(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let query = p.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
            let case_sensitive = p
                .get("case_sensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let (hits, total) = self.search_all(query, case_sensitive);
            let matches: Vec<Value> = hits
                .iter()
                .map(|h| {
                    json!({
                        "pane": h.pane.0.to_string(),
                        "workspace": h.ws,
                        "workspace_name": h.ws_name,
                        "line_offset": h.offset,
                        "text": h.line,
                        "col": h.col,
                    })
                })
                .collect();
            Ok(json!({
                "type": "search",
                "query": query,
                "total": total,
                "shown": matches.len(),
                "matches": matches,
            }))
        }
    }

    // ── DIFF review (docs/88) ────────────────────────────────────
    pub(super) fn api_diff_refresh(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            let generation = self
                .diff
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.generation)
                .unwrap_or_default();
            Ok(json!({"type":"ok","refresh":"complete","generation":generation}))
        }
    }

    pub(super) fn api_diff_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            let layer = p
                .get("layer")
                .and_then(|value| value.as_str())
                .map(parse_diff_layer)
                .transpose()?;
            let snapshot = self
                .diff
                .snapshot
                .as_ref()
                .ok_or_else(|| diff_err("DIFF is not ready".to_string()))?;
            let files: Vec<Value> = snapshot
                .files
                .iter()
                .filter(|file| layer.as_ref().is_none_or(|layer| &file.key.layer == layer))
                .map(diff_file_json)
                .collect();
            Ok(json!({
                "type":"diff_list",
                "repo": snapshot.repo_root,
                "branch": snapshot.branch,
                "generation": snapshot.generation,
                "fingerprint": snapshot.fingerprint,
                "omitted": snapshot.omitted_files,
                "refreshing": self.diff.status_inflight,
                "files": files,
            }))
        }
    }

    pub(super) fn api_diff_open(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            let target = match p
                .get("placement")
                .or_else(|| p.get("target"))
                .and_then(|v| v.as_str())
            {
                Some("tab") => crate::app::files::OpenTarget::Tab,
                Some("pane") => crate::app::files::OpenTarget::Pane,
                Some("preview") | None => crate::app::files::OpenTarget::Preview,
                Some(_) => {
                    return Err(diff_err(
                        "placement must be preview, pane, or tab".to_string(),
                    ))
                }
            };
            let preference = match p.get("view").and_then(Value::as_str) {
                None => None,
                Some("auto") => Some(crate::diff::DiffLayoutPreference::Auto),
                Some("split") => Some(crate::diff::DiffLayoutPreference::Split),
                Some("stack") => Some(crate::diff::DiffLayoutPreference::Stack),
                Some(_) => return Err(diff_err("view must be auto, split, or stack".to_string())),
            };
            let layer = p
                .get("layer")
                .and_then(|value| value.as_str())
                .map(parse_diff_layer)
                .transpose()?;
            let raw = p.get("path").and_then(|value| value.as_str()).unwrap_or("");
            let key = if raw.is_empty() {
                self.diff
                    .selected_file()
                    .or_else(|| {
                        self.diff
                            .snapshot
                            .as_ref()
                            .and_then(|snapshot| snapshot.files.first())
                    })
                    .map(|file| file.key.clone())
                    .ok_or_else(|| diff_err("there are no changed files".to_string()))?
            } else {
                self.diff_file_for_path(raw, layer.as_ref())
                    .map_err(diff_err)?
                    .key
            };
            self.open_diff_view(key.clone(), target);
            let id = self
                .diff_view_showing(&key)
                .ok_or_else(|| diff_err("failed to open diff".to_string()))?;
            if let (Some(preference), Some(crate::app::ViewKind::Diff(view))) =
                (preference, self.views.get_mut(&id))
            {
                view.preference = preference;
            }
            Ok(
                json!({"type":"diff_open","pane":id.0.to_string(),"path":key.display_path(),"layer":key.layer.label()}),
            )
        }
    }

    pub(super) fn api_diff_get(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            let raw = req_str(p, "path")?;
            let layer = p
                .get("layer")
                .and_then(|value| value.as_str())
                .map(parse_diff_layer)
                .transpose()?;
            let file = self
                .diff_file_for_path(raw, layer.as_ref())
                .map_err(diff_err)?;
            let diff = self.load_diff_file_sync(&file).map_err(diff_err)?;
            let include_patch = p
                .get("include_patch")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let hunks: Vec<Value> = diff
                .hunks
                .iter()
                .map(|hunk| {
                    json!({
                        "id":hunk.id,
                        "old_start":hunk.old_start,
                        "new_start":hunk.new_start,
                        "header":hunk.header,
                        "lines": if include_patch {
                            serde_json::to_value(&hunk.lines).unwrap_or(Value::Null)
                        } else {
                            Value::Null
                        },
                    })
                })
                .collect();
            Ok(json!({
                "type":"diff",
                "file":diff_file_json(&file),
                "additions":diff.additions,
                "deletions":diff.deletions,
                "binary":diff.binary,
                "truncated":diff.truncated,
                "omitted_lines":diff.omitted_lines,
                "hunks":hunks,
            }))
        }
    }

    pub(super) fn api_diff_navigate(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = self.resolve_pane_or_focus(p)?;
            let action = req_str(p, "action")?;
            let key = match action {
                "next" | "next_line" => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                "previous" | "previous_line" => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                "next_file" => KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE),
                "previous_file" => KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE),
                "next_hunk" => KeyEvent::new(KeyCode::Char('}'), KeyModifiers::NONE),
                "previous_hunk" => KeyEvent::new(KeyCode::Char('{'), KeyModifiers::NONE),
                "next_note" => KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE),
                "previous_note" => KeyEvent::new(KeyCode::Char('P'), KeyModifiers::NONE),
                "top" => KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                "bottom" => KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                _ => {
                    return Err(diff_err(
                        "action must target a line, file, hunk, note, top, or bottom".to_string(),
                    ))
                }
            };
            if !self.handle_diff_key(id, key) {
                return Err(diff_err("target is not an open DIFF view".to_string()));
            }
            Ok(json!({"type":"ok","pane":id.0.to_string()}))
        }
    }

    pub(super) fn api_diff_note_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            self.ensure_diff_notes_sync().map_err(diff_err)?;
            let state = p
                .get("state")
                .and_then(Value::as_str)
                .map(parse_note_state)
                .transpose()?;
            let path = p
                .get("file")
                .or_else(|| p.get("path"))
                .and_then(Value::as_str);
            let notes: Vec<Value> = self
                .diff
                .notes
                .iter()
                .filter(|note| state.is_none_or(|state| note.state == state))
                .filter(|note| path.is_none_or(|path| note.anchor.diff_key.display_path() == path))
                .map(note_json)
                .collect();
            Ok(json!({"type":"diff_notes","notes":notes}))
        }
    }

    pub(super) fn api_diff_note_apply(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            self.ensure_diff_notes_sync().map_err(diff_err)?;
            let items = p
                .get("notes")
                .and_then(Value::as_array)
                .filter(|items| !items.is_empty())
                .ok_or_else(|| diff_err("notes must be a non-empty array".to_string()))?;
            if self.diff.notes.len().saturating_add(items.len()) > crate::diff::NOTE_CAP {
                return Err(diff_err(format!(
                    "review note limit is {}",
                    crate::diff::NOTE_CAP
                )));
            }
            let mut notes = Vec::with_capacity(items.len());
            for item in items {
                let raw = item
                    .get("file")
                    .or_else(|| item.get("path"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| diff_err("every note needs a file".to_string()))?;
                let layer = item
                    .get("layer")
                    .and_then(Value::as_str)
                    .map(parse_diff_layer)
                    .transpose()?;
                let file = self
                    .diff_file_for_path(raw, layer.as_ref())
                    .map_err(diff_err)?;
                let old = diff_line_param(item, "old_line")?;
                let new = diff_line_param(item, "new_line")?;
                let (side, start) = match (old, new) {
                    (Some(line), None) => (crate::diff::DiffSide::Old, line),
                    (None, Some(line)) => (crate::diff::DiffSide::New, line),
                    _ => {
                        return Err(diff_err(
                            "every note needs exactly one old_line or new_line".to_string(),
                        ))
                    }
                };
                let end = diff_line_param(item, "end_line")?.unwrap_or(start);
                let diff = self.load_diff_file_sync(&file).map_err(diff_err)?;
                let context = crate::diff::notes::anchor_context(&diff, side, start, end)
                    .map_err(diff_err)?;
                let context_sha256 = crate::diff::notes::context_hash(&context);
                let context: String = context.chars().take(512).collect();
                let body = item
                    .get("body")
                    .and_then(Value::as_str)
                    .ok_or_else(|| diff_err("every note needs a body".to_string()))?
                    .to_string();
                let kind =
                    parse_note_kind(item.get("kind").and_then(Value::as_str).unwrap_or("issue"))?;
                let key = file.key;
                let now = crate::diff::notes::now_ms();
                notes.push(crate::diff::ReviewNote {
                    id: crate::diff::notes::note_id(),
                    review_id: crate::diff::notes::review_id(&key),
                    author: "external".to_string(),
                    kind,
                    body,
                    anchor: crate::diff::notes::NoteAnchor {
                        diff_key: key,
                        side,
                        start_line: start,
                        end_line: end,
                        context_sha256,
                        context,
                    },
                    state: crate::diff::NoteState::Open,
                    deliveries: Vec::new(),
                    revision: 1,
                    created_at_ms: now,
                    updated_at_ms: now,
                });
            }
            crate::diff::notes::save_batch_new(&notes).map_err(diff_err)?;
            self.diff.notes.extend(notes.iter().cloned());
            self.refresh_diff_note_counts();
            Ok(json!({
                "type":"diff_notes_applied",
                "notes":notes.iter().map(note_json).collect::<Vec<_>>()
            }))
        }
    }

    pub(super) fn api_diff_note_add(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            self.ensure_diff_notes_sync().map_err(diff_err)?;
            let raw = p
                .get("file")
                .or_else(|| p.get("path"))
                .and_then(Value::as_str)
                .ok_or_else(|| diff_err("file is required".to_string()))?;
            let layer = p
                .get("layer")
                .and_then(Value::as_str)
                .map(parse_diff_layer)
                .transpose()?;
            let file = self
                .diff_file_for_path(raw, layer.as_ref())
                .map_err(diff_err)?;
            let (side, start) = match (
                diff_line_param(p, "old_line")?,
                diff_line_param(p, "new_line")?,
            ) {
                (Some(line), None) => (crate::diff::DiffSide::Old, line),
                (None, Some(line)) => (crate::diff::DiffSide::New, line),
                _ => {
                    return Err(diff_err(
                        "pass exactly one of old_line or new_line".to_string(),
                    ))
                }
            };
            let end = diff_line_param(p, "end_line")?.unwrap_or(start);
            let diff = self.load_diff_file_sync(&file).map_err(diff_err)?;
            let context =
                crate::diff::notes::anchor_context(&diff, side, start, end).map_err(diff_err)?;
            let context_sha256 = crate::diff::notes::context_hash(&context);
            let context: String = context.chars().take(512).collect();
            let body = req_str(p, "body")?.to_string();
            let kind = parse_note_kind(p.get("kind").and_then(Value::as_str).unwrap_or("issue"))?;
            let key = file.key;
            let now = crate::diff::notes::now_ms();
            let note = crate::diff::ReviewNote {
                id: crate::diff::notes::note_id(),
                review_id: crate::diff::notes::review_id(&key),
                author: "external".to_string(),
                kind,
                body,
                anchor: crate::diff::notes::NoteAnchor {
                    diff_key: key,
                    side,
                    start_line: start,
                    end_line: end,
                    context_sha256,
                    context,
                },
                state: crate::diff::NoteState::Open,
                deliveries: Vec::new(),
                revision: 1,
                created_at_ms: now,
                updated_at_ms: now,
            };
            crate::diff::notes::save(&note, None).map_err(diff_err)?;
            self.apply_diff_note_saved(note.clone(), Ok(()));
            Ok(json!({"type":"diff_note","note":note_json(&note)}))
        }
    }

    pub(super) fn api_diff_note_edit(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            self.ensure_diff_notes_sync().map_err(diff_err)?;
            let id = req_str(p, "id")?;
            let note = self
                .diff
                .notes
                .iter()
                .find(|note| note.id == id)
                .cloned()
                .ok_or_else(|| diff_err("review note not found".to_string()))?;
            let mut updated = note.clone();
            match method {
                "diff.note.edit" => updated.body = req_str(p, "body")?.to_string(),
                "diff.note.resolve" => updated.state = crate::diff::NoteState::Resolved,
                "diff.note.reopen" => updated.state = crate::diff::NoteState::Open,
                _ => unreachable!(),
            }
            updated.revision = updated.revision.saturating_add(1);
            updated.updated_at_ms = crate::diff::notes::now_ms();
            crate::diff::notes::save(&updated, Some(note.revision)).map_err(diff_err)?;
            self.apply_diff_note_saved(updated.clone(), Ok(()));
            Ok(json!({"type":"diff_note","note":note_json(&updated)}))
        }
    }

    pub(super) fn api_diff_note_remove(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            self.ensure_diff_notes_sync().map_err(diff_err)?;
            let id = req_str(p, "id")?;
            let note = self
                .diff
                .notes
                .iter()
                .find(|note| note.id == id)
                .cloned()
                .ok_or_else(|| diff_err("review note not found".to_string()))?;
            crate::diff::notes::remove(&note, Some(note.revision)).map_err(diff_err)?;
            self.apply_diff_note_removed(id.to_string(), Ok(()));
            Ok(json!({"type":"ok","removed":id}))
        }
    }

    pub(super) fn api_diff_note_send(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.ensure_diff_snapshot().map_err(diff_err)?;
            self.ensure_diff_notes_sync().map_err(diff_err)?;
            let target = req_str(p, "to")?;
            let all_open = p.get("all_open").and_then(Value::as_bool).unwrap_or(false);
            let ids: Vec<&str> = p
                .get("ids")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let selected: Vec<crate::diff::ReviewNote> = self
                .diff
                .notes
                .iter()
                .filter(|note| {
                    if all_open {
                        note.state == crate::diff::NoteState::Open
                    } else {
                        ids.contains(&note.id.as_str())
                    }
                })
                .cloned()
                .collect();
            if selected.is_empty() {
                return Err(diff_err("select at least one review note".to_string()));
            }
            let params = json!({"target":target});
            let pane_id = self.resolve_agent_target(&params)?;
            let selected_ids: Vec<String> = selected.iter().map(|note| note.id.clone()).collect();
            let count = self
                .deliver_diff_notes(pane_id, target, &selected_ids)
                .map_err(diff_err)?;
            Ok(
                json!({"type":"diff_note_send","pane":pane_id.0.to_string(),"target":target,"count":count}),
            )
        }
    }

    // ── git (docs/17) — fast local-git reads + open the git tab ──
    pub(super) fn api_git_status(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let cwd = self.git_workspace_cwd(p);
            let s = crate::git::local::status(&cwd).map_err(git_err)?;
            let files = |v: &[crate::git::model::FileChange]| -> Vec<Value> {
                v.iter()
                    .map(|c| json!({"code": c.code.to_string(), "path": c.path}))
                    .collect()
            };
            Ok(json!({
                "type": "git_status", "branch": s.branch, "upstream": s.upstream,
                "ahead": s.ahead, "behind": s.behind,
                "staged": files(&s.staged), "unstaged": files(&s.unstaged),
                "untracked": s.untracked, "stashes": s.stashes,
            }))
        }
    }

    pub(super) fn api_git_branches(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let cwd = self.git_workspace_cwd(p);
            let v = crate::git::local::branches(&cwd).map_err(git_err)?;
            let arr: Vec<Value> = v
                .iter()
                .map(|b| json!({"name": b.name, "head": b.is_head, "ahead": b.ahead, "behind": b.behind, "subject": b.subject}))
                .collect();
            Ok(json!({"type":"git_branches","branches":arr}))
        }
    }

    pub(super) fn api_git_log(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let cwd = self.git_workspace_cwd(p);
            let n = param_usize(p, "n").unwrap_or(30);
            let v = crate::git::local::commits(&cwd, n, false).map_err(git_err)?;
            let arr: Vec<Value> = v
                .iter()
                .map(|c| json!({"sha": c.sha, "subject": c.subject, "author": c.author, "when": c.when, "refs": c.refs}))
                .collect();
            Ok(json!({"type":"git_log","commits":arr}))
        }
    }

    pub(super) fn api_git_open(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let i = param_usize(p, "workspace")
                .or_else(|| param_usize(p, "node"))
                .unwrap_or(self.active_ws);
            self.open_git_tab(i);
            Ok(json!({"type":"ok","git": self.active_is_git()}))
        }
    }

    // ── file viewer (docs/38) ──
    pub(super) fn api_files_open(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let raw = p.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if raw.is_empty() {
                return Err(("bad_request".into(), "path required".into()));
            }
            let path = self.resolve_file_path(raw);
            let target = match p.get("target").and_then(|v| v.as_str()) {
                Some("tab") => crate::app::files::OpenTarget::Tab,
                Some("pane") => crate::app::files::OpenTarget::Pane,
                _ => crate::app::files::OpenTarget::Preview,
            };
            self.open_file_view(path, target);
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_files_tree(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.prepare_file_tree_api(false);
            let rows: Vec<Value> = self
                .file_tree
                .visible_rows()
                .iter()
                .map(|r| {
                    json!({
                        "path": r.path.to_string_lossy(),
                        "name": r.name,
                        "depth": r.depth,
                        "dir": r.is_dir,
                        "expanded": r.expanded,
                    })
                })
                .collect();
            Ok(json!({
                "type": "file_tree",
                "root": self.file_tree.root().to_string_lossy(),
                "rows": rows,
            }))
        }
    }

    pub(super) fn api_files_reveal(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let raw = p.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if raw.is_empty() {
                return Err(("bad_request".into(), "path required".into()));
            }
            let path = self.resolve_file_path(raw);
            self.file_tree.reveal(&path);
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_files_refresh(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.prepare_file_tree_api(true);
            Ok(json!({"type":"ok"}))
        }
    }
}

impl App {
    /// The cwd of the `workspace` param (else the active workspace) for git.* methods.
    pub(in crate::app::dispatch) fn git_workspace_cwd(&self, p: &Value) -> PathBuf {
        let i = param_usize(p, "workspace")
            .or_else(|| param_usize(p, "node"))
            .unwrap_or(self.active_ws);
        self.workspaces
            .get(i)
            .map(|w| w.cwd.clone())
            .unwrap_or_else(|| self.ws().cwd.clone())
    }
}
