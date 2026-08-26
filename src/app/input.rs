//! Input handling for [`App`](super::App): key & mouse events, the prefix-key
//! command map, and crossterm→PTY key encoding.

use super::*;
use crate::files::view_text_w;

fn copy_word_forward(
    row_count: usize,
    mut row_layout: impl FnMut(usize) -> Option<crate::terminal::vt::RetainedRowLayout>,
    mut at: (usize, usize),
) -> (usize, usize) {
    while at.0 < row_count {
        let Some(layout) = row_layout(at.0) else {
            at.0 += 1;
            at.1 = 0;
            continue;
        };
        let last = layout.last_column();
        while at.1 <= last && !layout.is_whitespace(at.1) {
            at.1 += 1;
        }
        while at.1 <= last && layout.is_whitespace(at.1) {
            at.1 += 1;
        }
        if at.1 <= last {
            return at;
        }
        at.0 += 1;
        at.1 = 0;
    }
    let last = row_count.saturating_sub(1);
    let column = row_layout(last).map_or(0, |layout| layout.last_column());
    (last, column)
}

fn copy_word_back(
    mut row_layout: impl FnMut(usize) -> Option<crate::terminal::vt::RetainedRowLayout>,
    mut at: (usize, usize),
) -> (usize, usize) {
    loop {
        let Some(layout) = row_layout(at.0) else {
            if at.0 == 0 {
                return (0, 0);
            }
            at.0 -= 1;
            at.1 = row_layout(at.0).map_or(0, |layout| layout.last_column().saturating_add(1));
            continue;
        };
        let mut col = at.1.min(layout.last_column().saturating_add(1));
        while col > 0 && layout.is_whitespace(col - 1) {
            col -= 1;
        }
        while col > 0 && !layout.is_whitespace(col - 1) {
            col -= 1;
        }
        if col > 0 || layout.has_text() {
            return (at.0, col.min(layout.last_column()));
        }
        if at.0 == 0 {
            return (0, 0);
        }
        at.0 -= 1;
        at.1 = row_layout(at.0).map_or(0, |layout| layout.last_column().saturating_add(1));
    }
}

fn finish_selected_text(mut out: String) -> Option<String> {
    let trimmed_len = out.trim_end_matches('\n').len();
    out.truncate(trimmed_len);
    (!out.trim().is_empty()).then_some(out)
}

/// Drop the one blank cell which can sit between a pane edge and uniformly
/// aligned prose. This is deliberately narrow: code with its usual two- or
/// four-space indentation is retained exactly as selected.
fn strip_uniform_single_cell_margin(text: String) -> String {
    let mut saw_text = false;
    let uniform_margin = text.lines().filter(|line| !line.is_empty()).all(|line| {
        saw_text = true;
        line.starts_with(' ') && !line.starts_with("  ")
    });
    if !saw_text || !uniform_margin {
        return text;
    }
    text.lines()
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A second left click within this of the first, on the same cell (±1), is a
/// double-click. Terminals emit no native double-click, so luvus times it.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// A run of grid cells on one row: `(row, start_col, end_col)`, `end_col`
/// exclusive — the same shape as [`crate::links::Link::spans`].
type CellSpan = (u16, u16, u16);

/// The whitespace-delimited word covering grid cell (`col`, `row`), and the one
/// span it occupies. `None` on a whitespace or out-of-range cell. Char indices
/// are the columns, matching how [`crate::links::link_at`] and the grid renderer
/// address cells.
fn word_at_grid(rows: &[String], col: u16, row: u16) -> Option<(String, Vec<CellSpan>)> {
    let line = rows.get(row as usize)?;
    let chars: Vec<char> = line.chars().collect();
    let idx = col as usize;
    if idx >= chars.len() || chars[idx].is_whitespace() {
        return None;
    }
    let mut lo = idx;
    while lo > 0 && !chars[lo - 1].is_whitespace() {
        lo -= 1;
    }
    let mut hi = idx + 1;
    while hi < chars.len() && !chars[hi].is_whitespace() {
        hi += 1;
    }
    let text: String = chars[lo..hi].iter().collect();
    if text.is_empty() {
        return None;
    }
    Some((text, vec![(row, lo as u16, hi as u16)]))
}

impl App {
    fn handle_api_request(&mut self, req: crate::ipc::api::ApiRequest) -> bool {
        if req.method == "terminal.backend.create" {
            self.start_backend_create(req);
            return true;
        }
        if req.method == "terminal.backend.capture" {
            self.start_backend_capture(req);
            return true;
        }
        if matches!(
            req.method.as_str(),
            "terminal.backend.wait_change" | "terminal.backend.wait_output"
        ) {
            self.start_backend_wait(req);
            return true;
        }
        if req.method == "search.query" {
            self.start_search_api(req);
            return true;
        }
        if req.method == "search.activate" {
            let response = self.handle_search_activate(&req);
            let _ = req.reply.send(response);
            return true;
        }
        let Some(req) = self.prepare_files_api(req) else {
            return true;
        };
        let Some(req) = self.prepare_diff_api(req) else {
            return true;
        };
        let response = self.handle_api(&req);
        let _ = req.reply.send(response);
        true
    }

    fn handle_theme_reloaded(
        &mut self,
        id: String,
        registry: crate::theme::ThemeRegistry,
        reply: std::sync::mpsc::Sender<String>,
    ) -> bool {
        let count = registry.entries().len();
        let problems = registry.problems().to_vec();
        let selected = self.replace_theme_registry(registry);
        let _ = reply.send(
            json!({"id": id, "result": {
                "type": "themes_reloaded",
                "count": count,
                "selected_available": selected,
                "problems": problems,
            }})
            .to_string(),
        );
        true
    }

    /// Apply an event; returns whether it changed the rendered UI (→ the loop
    /// should redraw). Input forwarded to a pane returns `false` — the screen only
    /// changes when the pane echoes (a separate `PtyData` event), so we don't waste
    /// a full render per keystroke.
    pub fn handle_event(&mut self, ev: AppEvent) -> bool {
        // Theme removal starts in Settings but performs bounded filesystem work
        // off-loop. Apply its completed registry before the empty-workspace guard
        // so the single writer always observes the result.
        let ev = match ev {
            AppEvent::BackendCreateReady {
                id,
                reply,
                pane_id,
                cwd,
                branch,
                worktree,
                commit,
                result,
            } => {
                self.finish_backend_create(
                    id, reply, pane_id, cwd, branch, worktree, commit, result,
                );
                return true;
            }
            AppEvent::PtyReady { id, cwd } => {
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.cwd = cwd;
                }
                self.register_backend_terminal(id);
                crate::logging::event(
                    crate::logging::EventKind::PaneOpen,
                    &[
                        crate::logging::Field::PaneId(u64::from(id.0)),
                        crate::logging::Field::SpawnKind(crate::logging::SpawnKind::Deferred),
                    ],
                );
                return true;
            }
            AppEvent::BackendObserve { params, reply } => {
                let _ = reply.send(self.prepare_backend_observe(&params));
                return false;
            }
            AppEvent::ThemeUninstalled { id, result } => {
                self.finish_theme_uninstall(id, result);
                return true;
            }
            AppEvent::ConfigReloaded { id, config, reply } => {
                let response = match self.apply_socket_config(config) {
                    Ok(()) => {
                        json!({"id":id,"result":{"type":"config_reloaded","config":self.config}})
                    }
                    Err((code, message)) => {
                        json!({"id":id,"error":{"code":code,"message":message}})
                    }
                };
                let _ = reply.send(response.to_string());
                return true;
            }
            AppEvent::ManifestsReloaded {
                id,
                manifests,
                reply,
            } => {
                self.apply_socket_manifests(manifests);
                crate::logging::event(
                    crate::logging::EventKind::ManifestReload,
                    &[crate::logging::Field::Outcome(crate::logging::Outcome::Ok)],
                );
                let _ = reply.send(
                    json!({"id":id,"result":{
                        "type":"agent_manifests_reloaded","rules":self.manifests.rule_count()
                    }})
                    .to_string(),
                );
                return true;
            }
            AppEvent::SearchFilesIndexed { instance, catalogs } => {
                return self.apply_search_files(instance, catalogs);
            }
            AppEvent::SearchResults {
                instance,
                generation,
                matches,
                total,
                capped,
            } => {
                return self.apply_search_results(instance, generation, matches, total, capped);
            }
            AppEvent::SearchFederatedResults {
                instance,
                generation,
                matches,
                total,
                partial,
            } => {
                return self
                    .apply_search_federated_results(instance, generation, matches, total, partial);
            }
            AppEvent::SearchHandoffReady { session, result } => {
                match result {
                    Ok(()) => self.pending_session_switch = Some(session),
                    Err(error) => {
                        log_worker_failed(crate::logging::Worker::Search, "handoff");
                        self.show_toast(format!("session switch failed: {error}"));
                    }
                }
                return true;
            }
            other => other,
        };
        // Control-API requests and parked `wait.output` replies must be answered
        // even with no workspace open. A server that has closed its last node
        // stays alive (docs/43 §3.3), and the methods that reopen one are the
        // only way back; dropping the reply channel here would leave the caller
        // reading EOF instead of a `workspace.open` / `server.stop` answer.
        if self.workspaces.is_empty() {
            match ev {
                AppEvent::ThemeReloaded {
                    id,
                    registry,
                    reply,
                } => return self.handle_theme_reloaded(id, registry, reply),
                AppEvent::Api(req) => {
                    if req.method == "terminal.backend.create" {
                        self.start_backend_create(req);
                        return true;
                    }
                    if req.method == "terminal.backend.capture" {
                        self.start_backend_capture(req);
                        return true;
                    }
                    if matches!(
                        req.method.as_str(),
                        "terminal.backend.wait_change" | "terminal.backend.wait_output"
                    ) {
                        self.start_backend_wait(req);
                        return true;
                    }
                    if req.method == "search.query" {
                        self.start_search_api(req);
                        return true;
                    }
                    if req.method == "search.activate" {
                        let response = self.handle_search_activate(&req);
                        let _ = req.reply.send(response);
                        return true;
                    }
                    let resp = self.handle_api(&req);
                    let _ = req.reply.send(resp);
                    return true;
                }
                AppEvent::WaitOutput { id, reply, .. } => {
                    let _ = reply.send(
                        json!({ "id": id, "error": {
                            "code": "no_session", "message": "no active session"
                        }})
                        .to_string(),
                    );
                    return true;
                }
                AppEvent::AgentWait { id, reply, .. } => {
                    let _ = reply.send(
                        json!({ "id": id, "error": {
                            "code": "no_session", "message": "no active session"
                        }})
                        .to_string(),
                    );
                    return true;
                }
                AppEvent::AgentStart { id, reply, .. }
                | AppEvent::AgentPrompt { id, reply, .. } => {
                    let _ = reply.send(
                        json!({ "id": id, "error": {
                            "code": "no_session", "message": "no active session"
                        }})
                        .to_string(),
                    );
                    return true;
                }
                // Closing the last workspace empties `workspaces` and sets
                // `should_quit`; the loop drains the rest of the event batch
                // before it checks that flag, so ignore everything else here
                // once there's nothing left to act on (`layout()` would
                // otherwise index an empty `workspaces`).
                _ => return false,
            }
        }
        match ev {
            AppEvent::Key(k) => self.handle_key(k),
            AppEvent::Mouse(m) => self.handle_mouse(m),
            AppEvent::Paste(s) => {
                // Copy mode owns input just like scroll mode: never leak a
                // pasted command into the pane while the user is selecting.
                if self.copy_mode.is_some() {
                    return true;
                }
                // A paste while a text-input modal is open (a Settings field, a
                // rename prompt, …) fills that field, not the pane underneath.
                if self.paste_into_modal(&s) {
                    return true; // the modal buffer changed → redraw
                }
                // Otherwise it goes to the focused pane. `send_paste` re-wraps in
                // the bracketed-paste markers crossterm stripped, so a child that
                // distinguishes paste from typing (an agent CLI attaching a
                // dropped file, vim not auto-indenting) still sees a paste.
                if let Some(p) = self.focused() {
                    p.scroll_to_bottom(); // pasting is input → snap to live
                    p.send_paste(&s);
                }
                self.mark_user_input(); // so the echo isn't misread as agent work
                false // goes to the pane; its echo (PtyData) renders it
            }
            AppEvent::Resize => {
                // A resize (or a same-size resize event a terminal emits on a
                // move/expose) may have damaged the screen — force a full repaint.
                self.force_redraw = true;
                true
            }
            AppEvent::PtyData(id) => {
                // The reader's coalescing flag is deliberately NOT cleared here
                // — it re-arms on the frame/detect cadence (`rearm_pty_notify`),
                // so a saturated pane wakes the loop at the render rate, not
                // once per PTY read.
                if let Some(s) = self.status.get_mut(&id) {
                    s.last_activity = Instant::now();
                }
                self.detection_dirty.insert(id);
                // A parked `wait.output` for this pane just got new output to
                // test against — resolve it on the same wake (docs/81).
                self.check_output_waits(id);
                self.backend_output_changed(id);
                true // the pane's screen advanced
            }
            AppEvent::PtyExit(id) => {
                crate::logging::event(
                    crate::logging::EventKind::PtyExit,
                    &[
                        crate::logging::Field::PaneId(u64::from(id.0)),
                        crate::logging::Field::ExitClass(crate::logging::ExitClass::Unknown),
                    ],
                );
                self.emit_backend_terminal_event(id, "terminal.exited", json!({}));
                self.close_pane(id);
                true
            }
            // Control-API requests arrive on the event channel so the loop wakes
            // for them immediately (docs/81). Answer inline: like the old
            // server-side drain, an answered request counts as activity.
            AppEvent::Api(req) => self.handle_api_request(req),
            AppEvent::ThemeReloaded {
                id,
                registry,
                reply,
            } => self.handle_theme_reloaded(id, registry, reply),
            // A `wait.output` connection parks its reply here and blocks until
            // the pane's output matches (docs/81) — no polling on either side.
            AppEvent::WaitOutput {
                id: request_id,
                pane,
                needle,
                reply,
                timeout,
                cancelled,
            } => {
                let params = json!({ "pane": pane });
                match self.resolve_pane(&params) {
                    Some(id) => {
                        self.register_output_wait(
                            id, request_id, needle, reply, timeout, cancelled,
                        );
                    }
                    None => {
                        let _ = reply.send(
                            json!({ "id": request_id, "error": {
                                "code": "not_found", "message": "pane not found"
                            }})
                            .to_string(),
                        );
                    }
                }
                true
            }
            AppEvent::AgentWait {
                id: request_id,
                pane,
                state,
                reply,
                timeout,
                cancelled,
            } => {
                let params = json!({"pane":pane});
                match (
                    self.resolve_pane(&params),
                    crate::app::dispatch::parse_agent_wait_state(&state),
                ) {
                    (Some(id), Some(state)) => {
                        self.register_agent_wait(id, request_id, state, reply, timeout, cancelled);
                    }
                    (None, _) => {
                        let _ = reply.send(
                            json!({"id":request_id,"error":{"code":"not_found","message":"pane not found"}})
                                .to_string(),
                        );
                    }
                    (_, None) => {
                        let _ = reply.send(
                            json!({"id":request_id,"error":{"code":"invalid_request","message":"status must be idle, working, blocked, or done"}})
                                .to_string(),
                        );
                    }
                }
                true
            }
            AppEvent::AgentStart {
                id,
                params,
                reply,
                cancelled,
            } => {
                self.start_agent_launch(id, params, reply, cancelled);
                true
            }
            AppEvent::AgentPrompt {
                id,
                params,
                reply,
                cancelled,
            } => {
                self.start_agent_prompt(id, params, reply, cancelled);
                true
            }
            AppEvent::ModuleCommandFinished {
                log_id,
                code,
                out,
                err,
            } => {
                if code != Some(0) {
                    log_worker_failed(crate::logging::Worker::Module, "exit");
                }
                self.module_command_finished(log_id, code, out, err);
                true
            }
            // Repaint only when the visible sidebar list actually changed —
            // most 4s scans find nothing new.
            AppEvent::SessionsScanned(found) => self.apply_scanned_sessions(found),
            // Process-table churn is only a cache update, but a confirmed agent
            // exit changes the visible sidebar immediately. `apply_proc_scan`
            // distinguishes those cases so the common scan stays render-free.
            AppEvent::ProcScanned(found) => self.apply_proc_scan(found),
            // Mission Control usage (docs/54, MC-2): swap in the fresh cache; the
            // mission render blits it. Repaint so a visible mission tab updates.
            AppEvent::UsageScanned { usage, mtimes } => {
                self.usage_scan_inflight = false;
                self.agent_usage = usage;
                self.usage_mtimes = mtimes;
                // Fleet burn rate: change in total cost since the last scan (docs/54).
                let total: f64 = self.agent_usage.values().filter_map(|u| u.cost).sum();
                let now = std::time::Instant::now();
                if let Some((prev, at)) = self.mission_last_cost {
                    let dt = now.duration_since(at).as_secs_f64();
                    if dt > 1.0 && total >= prev {
                        self.mission_burn = Some((total - prev) / dt * 3600.0);
                    }
                }
                self.mission_last_cost = Some((total, now));
                self.active_is_mission()
            }
            AppEvent::DirRead { path, entries } => {
                self.file_tree.apply_dir(path.clone(), entries);
                self.finish_pending_files_api(&path);
                true
            }
            AppEvent::DiffStatus {
                token,
                visible_root,
                result,
            } => {
                if result.is_err() {
                    log_worker_failed(crate::logging::Worker::Diff, "scan");
                }
                let changed = self.apply_diff_status(token, visible_root, result);
                self.finish_pending_diff_api();
                changed
            }
            AppEvent::DiffLoaded { id, token, result } => {
                if result.is_err() {
                    log_worker_failed(crate::logging::Worker::Diff, "load");
                }
                self.apply_diff_loaded(id, token, result)
            }
            AppEvent::DiffNotesLoaded { review_id, result } => {
                if result.is_err() {
                    log_worker_failed(crate::logging::Worker::Diff, "notes");
                }
                self.apply_diff_notes_loaded(review_id, result)
            }
            AppEvent::DiffNoteSaved { note, result } => {
                if result.is_err() {
                    log_worker_failed(crate::logging::Worker::Diff, "note_save");
                }
                self.apply_diff_note_saved(note, result)
            }
            AppEvent::DiffNoteRemoved { id, result } => {
                if result.is_err() {
                    log_worker_failed(crate::logging::Worker::Diff, "note_remove");
                }
                self.apply_diff_note_removed(id, result)
            }
            AppEvent::DiffProgressSaved { result } => {
                if let Err(error) = result {
                    log_worker_failed(crate::logging::Worker::Diff, "progress_save");
                    self.show_toast(format!("review progress not saved: {error}"));
                }
                true
            }
            AppEvent::FileChanges { id, changes } => {
                if let Some(crate::app::ViewKind::File(v)) = self.views.get_mut(&id) {
                    v.changes = changes;
                    true
                } else {
                    false // the view leaf closed before the diff landed
                }
            }
            AppEvent::FileRead { id, load } => {
                if let Some(crate::app::ViewKind::File(v)) = self.views.get_mut(&id) {
                    v.apply(load);
                    true
                } else {
                    false // the view leaf was closed before its read landed
                }
            }
            AppEvent::GitData { view, payload } => {
                self.git_data(view, payload);
                true
            }
            AppEvent::TaskGateFinished { task, code, out } => {
                self.task_gate_finished(&task, code, out);
                true
            }
            AppEvent::UpdateAvailable(version) => {
                let changed = self.update_available.as_deref() != Some(version.as_str());
                self.update_available = Some(version);
                changed // repaint to show the dot only if it's news
            }
            // The asked-for check always answers, including "nothing new", since a
            // button that can silently do nothing reads as broken.
            AppEvent::UpdateChecked(outcome) => {
                match outcome {
                    crate::update::CheckOutcome::Newer(v) => {
                        let msg = format!("{} v{v}", self.catalog.update_available);
                        self.update_available = Some(v);
                        self.show_toast(msg);
                    }
                    crate::update::CheckOutcome::Current => {
                        self.show_toast(self.catalog.update_current)
                    }
                    crate::update::CheckOutcome::Failed => {
                        self.show_toast(self.catalog.update_failed)
                    }
                }
                true
            }
            // Handled by the server loop; never reaches here at runtime.
            AppEvent::ClientConnected { .. }
            | AppEvent::ClientDetach { .. }
            | AppEvent::ClientInput { .. } => false,
            // Consumed by the pre-dispatch worker-result branch above.
            AppEvent::ThemeUninstalled { .. }
            | AppEvent::ConfigReloaded { .. }
            | AppEvent::ManifestsReloaded { .. }
            | AppEvent::BackendCreateReady { .. }
            | AppEvent::BackendObserve { .. }
            | AppEvent::PtyReady { .. }
            | AppEvent::SearchFilesIndexed { .. }
            | AppEvent::SearchResults { .. }
            | AppEvent::SearchFederatedResults { .. }
            | AppEvent::SearchHandoffReady { .. } => unreachable!(),
        }
    }

    /// The key a click maps to on an open text-input modal's footer: `⏎` on the
    /// commit button, `Esc` on cancel, `None` anywhere else. Lets the mouse drive
    /// the same commit/cancel path as the keyboard.
    fn modal_button_key(
        &self,
        m: &ratatui::crossterm::event::MouseEvent,
    ) -> Option<ratatui::crossterm::event::KeyEvent> {
        use ratatui::crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
        };
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let (c, r) = (m.column, m.row);
            let on = |rect: Option<Rect>| {
                rect.is_some_and(|x| c >= x.x && c < x.right() && r >= x.y && r < x.bottom())
            };
            if on(self.modal_commit_rect) {
                return Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            }
            if on(self.modal_cancel_rect) {
                return Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
        }
        None
    }

    /// Route pasted text into an open text-input modal by replaying it as
    /// keypresses, so a paste fills the field instead of leaking to the pane
    /// underneath. Mirrors `handle_key`'s text-input precedence; returns whether
    /// a modal consumed it. Control chars (newlines/tabs) are dropped — these are
    /// all single-line fields.
    fn paste_into_modal(&mut self, s: &str) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        if self.module_setting_edit.is_some() {
            for character in s.chars().filter(|character| !character.is_control()) {
                self.handle_module_setting_key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                ));
            }
            return true;
        }
        // Search can rank a large catalog, so append the whole paste and launch
        // one recomputation instead of replaying one query per character.
        if self.search.is_some() {
            self.search_paste(s);
            return true;
        }
        // The picker owns both text sub-modes and direct path navigation.
        if self.picker.is_some() {
            self.picker_paste(s);
            return true;
        }
        let handler: fn(&mut Self, KeyEvent) = if self.worktree_prompt.is_some() {
            Self::handle_worktree_prompt_key
        } else if self.tab_rename.is_some() {
            Self::handle_tab_rename_key
        } else if self.file_prompt.is_some() {
            Self::file_prompt_key
        } else if self.ws_rename.is_some() {
            Self::handle_ws_rename_key
        } else if self.pane_rename.is_some() {
            Self::handle_pane_rename_key
        } else if self.orch_form.is_some() {
            Self::handle_orch_form_key
        } else {
            return false;
        };
        for c in s.chars().filter(|c| !c.is_control()) {
            handler(self, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        true
    }

    /// Apply a mouse event and report whether it changed anything Luvus draws.
    ///
    /// Button, drag, release, and wheel events stay conservative because they
    /// are low-frequency interactions and can change state through many modal
    /// handlers. Motion is the hot path: compare only the hover state the
    /// renderer consumes, so moving across ordinary pane cells does not request
    /// frames while links, menus, FILES rows, and resize seams still repaint.
    fn handle_mouse(&mut self, m: ratatui::crossterm::event::MouseEvent) -> bool {
        use ratatui::crossterm::event::MouseEventKind;

        let kind = m.kind;
        // Copy mode is keyboard-owned. Any deliberate mouse action cancels it
        // and restores its saved viewport rather than forwarding a click/wheel
        // into the child while a selection is active.
        if self.copy_mode.is_some() && !matches!(kind, MouseEventKind::Moved) {
            self.cancel_copy_mode();
            return true;
        }
        let hover_before = self.rendered_hover_rect(self.hover);
        let divider_before = self
            .hover_divider
            .as_ref()
            .map(|d| (d.path.clone(), d.axis, d.line, d.span));
        let sidebar_before = self.hover_sidebar;
        let link_before = self.hover_link.clone();

        self.apply_mouse(m);

        if !matches!(kind, MouseEventKind::Moved) {
            return true;
        }

        let divider_after = self
            .hover_divider
            .as_ref()
            .map(|d| (d.path.clone(), d.axis, d.line, d.span));
        hover_before != self.rendered_hover_rect(self.hover)
            || divider_before != divider_after
            || sidebar_before != self.hover_sidebar
            || link_before != self.hover_link
    }

    /// The hover-highlighted rectangle containing `at`, if any. Moving within
    /// one rectangle does not alter the rendered frame; entering, leaving, or
    /// crossing into another one does. Keep this list aligned with renderers
    /// that consume `App.hover`.
    fn rendered_hover_rect(&self, at: Option<(u16, u16)>) -> Option<Rect> {
        let (c, r) = at?;
        let hit = |rect: Rect| c >= rect.x && c < rect.right() && r >= rect.y && r < rect.bottom();
        let first = |rects: &[Rect]| rects.iter().copied().find(|rect| hit(*rect));

        if self.changelog_open {
            return self
                .changelog_check_rect
                .filter(|rect| hit(*rect))
                .or_else(|| {
                    self.changelog_copy_rects
                        .iter()
                        .map(|(rect, _)| *rect)
                        .find(|rect| hit(*rect))
                });
        }
        if let Some(menu) = &self.tab_menu {
            return menu
                .items
                .iter()
                .map(|(_, rect)| *rect)
                .chain(menu.swap_rects.iter().map(|(_, rect)| *rect))
                .find(|rect| hit(*rect));
        }
        if let Some(menu) = &self.pane_menu {
            return menu
                .items
                .iter()
                .map(|(_, rect)| *rect)
                .chain(menu.tab_rects.iter().map(|(_, rect)| *rect))
                .find(|rect| hit(*rect));
        }
        if let Some(menu) = &self.agent_menu {
            return menu
                .items
                .iter()
                .map(|(_, rect)| *rect)
                .find(|rect| hit(*rect));
        }
        if let Some(menu) = &self.ws_menu {
            return menu
                .items
                .iter()
                .map(|(_, rect)| *rect)
                .find(|rect| hit(*rect));
        }
        if let Some(menu) = &self.file_menu {
            return menu
                .items
                .iter()
                .map(|(_, rect)| *rect)
                .find(|rect| hit(*rect));
        }
        if let Some(menu) = &self.diff_menu {
            return menu
                .items
                .iter()
                .map(|(_, rect)| *rect)
                .find(|rect| hit(*rect));
        }
        if let Some(menu) = &self.dock_menu {
            return first(&menu.rects);
        }
        let modal_owns_mouse = self.file_prompt.is_some()
            || self.file_delete.is_some()
            || self.worktree_delete.is_some()
            || self.worktree_prompt.is_some()
            || self.tab_rename.is_some()
            || self.ws_rename.is_some()
            || self.pane_rename.is_some();
        if modal_owns_mouse {
            return [self.modal_commit_rect, self.modal_cancel_rect]
                .into_iter()
                .flatten()
                .find(|rect| hit(*rect));
        }

        if self.switcher {
            return self
                .switcher_rects
                .iter()
                .map(|(_, rect)| *rect)
                .chain(self.switcher_scope_rects.iter().map(|(_, rect)| *rect))
                .chain(self.switcher_close_rect)
                .find(|rect| hit(*rect));
        }

        if let Some(popup) = self.bar.overflow.as_ref() {
            return hit(popup.rect).then_some(popup.rect);
        }
        if let Some(rect) = self
            .bar
            .hits
            .iter()
            .map(|hit| hit.rect)
            .chain(self.bar.overflow_hits.iter().map(|hit| hit.rect))
            .find(|rect| hit(*rect))
        {
            return Some(rect);
        }

        self.file_tree_rects
            .iter()
            .map(|(_, rect)| *rect)
            .chain(self.files_mode_rects.iter().map(|(_, rect)| *rect))
            .chain(self.diff_row_rects.iter().map(|(_, rect)| *rect))
            .chain(
                [
                    self.switcher_button_rect,
                    self.mobile_pane_prev_rect,
                    self.mobile_pane_next_rect,
                    self.sidebar_toggle_rect,
                    self.right_sidebar_toggle_rect,
                    self.version_rect,
                    self.settings_icon_rect,
                ]
                .into_iter()
                .flatten(),
            )
            .find(|rect| hit(*rect))
    }

    fn diff_source_hit_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<(PaneId, usize, crate::diff::DiffSide)> {
        self.diff_source_rects
            .iter()
            .find(|(_, _, _, rect)| {
                column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
            })
            .map(|(pane, source_row, side, _)| (*pane, *source_row, *side))
    }

    fn diff_note_hit_at(&self, column: u16, row: u16) -> Option<(PaneId, String)> {
        self.diff_note_rects
            .iter()
            .find(|(_, _, rect)| {
                column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
            })
            .map(|(pane, note_id, _)| (*pane, note_id.clone()))
    }

    fn apply_mouse(&mut self, m: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        // Track the cursor for hover affordances (e.g. the session delete ✕).
        self.hover = Some((m.column, m.row));
        // Any click dismisses the help overlay.
        if self.help_open {
            if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                self.help_open = false;
            }
            return;
        }
        // The changelog modal owns the mouse. Its content is safe to click without
        // closing; only the close button or a click on the dimmed backdrop dismisses
        // it. References stay openable and the wheel scrolls the notes.
        if self.changelog_open {
            match m.kind {
                MouseEventKind::ScrollUp => {
                    self.changelog_scroll = self.changelog_scroll.saturating_sub(2)
                }
                MouseEventKind::ScrollDown => {
                    self.changelog_scroll = self.changelog_scroll.saturating_add(2)
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let hit_rect = |r: ratatui::layout::Rect| {
                        m.row >= r.y
                            && m.row < r.bottom()
                            && m.column >= r.x
                            && m.column < r.right()
                    };
                    if self.changelog_close_rect.is_some_and(hit_rect) {
                        self.changelog_open = false;
                        return;
                    }
                    // "Check for updates" asks now and leaves the modal up, so the
                    // answer lands where it was asked for.
                    if self.changelog_check_rect.is_some_and(hit_rect) {
                        crate::update::check_now_reporting(self.app_tx.clone());
                        return;
                    }
                    // Installer/update rows copy the exact command, even when a
                    // narrow modal clips its visual representation.
                    if let Some(command) = self
                        .changelog_copy_rects
                        .iter()
                        .find(|(rect, _)| hit_rect(*rect))
                        .map(|(_, command)| command.clone())
                    {
                        self.pending_clipboard = Some(command);
                        let message = self.catalog.copied;
                        self.show_toast(message);
                        return;
                    }
                    // A click on a commit/PR reference (or the website row at the
                    // end) opens it and **leaves the modal up**, so several can be
                    // followed in a row.
                    let hit = self
                        .changelog_link_rects
                        .iter()
                        .find(|(r, _)| hit_rect(*r))
                        .map(|(_, url)| url.clone());
                    match hit {
                        Some(url) => self.open_url(url),
                        None if !self.changelog_modal_rect.is_some_and(hit_rect) => {
                            self.changelog_open = false
                        }
                        None => {}
                    }
                }
                _ => {}
            }
            return;
        }
        // The running-command overlay owns the mouse while open.
        if self.cmd_inspect.is_some() {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => self.close_cmd_inspect(),
                MouseEventKind::ScrollUp => {
                    if let Some(c) = self.cmd_inspect.as_mut() {
                        c.scroll = c.scroll.saturating_sub(2);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(c) = self.cmd_inspect.as_mut() {
                        c.scroll += 2;
                    }
                }
                _ => {}
            }
            return;
        }
        // A module-setting prompt sits on top of the Settings modal: a click
        // anywhere cancels it rather than reaching the rows underneath.
        if self.module_setting_edit.is_some() {
            if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                self.module_setting_edit = None;
            }
            return;
        }
        // While the Settings modal is open it owns the mouse: clicks hit the
        // modal (or dismiss it), and the wheel scrolls the current tab's list by
        // moving the selection (which drives the scroll) — so a long list like the
        // Keys reference doesn't need the arrow keys held down.
        if self.settings.is_some() {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.handle_settings_click(m.column, m.row)
                }
                MouseEventKind::ScrollUp => self.settings_scroll(-1),
                MouseEventKind::ScrollDown => self.settings_scroll(1),
                _ => {}
            }
            return;
        }
        // The board's start-worker picker / task detail own the mouse while
        // open: a click dismisses them, the wheel scrolls the detail.
        if self.orch_start.is_some() || self.orch_detail.is_some() {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.orch_start = None;
                    self.orch_detail = None;
                }
                MouseEventKind::ScrollUp if self.orch_detail.is_some() => {
                    self.orch_detail_scroll = self.orch_detail_scroll.saturating_sub(2)
                }
                MouseEventKind::ScrollDown if self.orch_detail.is_some() => {
                    self.orch_detail_scroll += 2
                }
                _ => {}
            }
            return;
        }
        // The global-search overlay (docs/63) owns the mouse while open: a click
        // on a result jumps to it, a click outside dismisses, the wheel moves the
        // result cursor.
        if self.search.is_some() {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => self.search_click(m.column, m.row),
                MouseEventKind::ScrollUp => self.search_move(-1),
                MouseEventKind::ScrollDown => self.search_move(1),
                _ => {}
            }
            return;
        }
        // The folder picker likewise owns the mouse while open.
        if self.picker.is_some() {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let (c, r) = (m.column, m.row);
                    let hit = self
                        .picker_rects
                        .iter()
                        .find(|(_, rect)| {
                            c >= rect.x && c < rect.right() && r >= rect.y && r < rect.bottom()
                        })
                        .map(|(hit, _)| *hit);
                    match hit {
                        Some(PickerHit::Row(i)) => self.picker_click(i),
                        Some(PickerHit::Hint(k)) => {
                            self.handle_picker_key(KeyEvent::new(k, KeyModifiers::NONE))
                        }
                        Some(PickerHit::Modal) => {}
                        None => self.close_folder_picker(), // click outside cancels
                    }
                }
                // Wheel scrolls the browse list (moves the cursor, which the
                // render keeps in view).
                MouseEventKind::ScrollUp => self.picker_scroll(-1),
                MouseEventKind::ScrollDown => self.picker_scroll(1),
                _ => {}
            }
            return;
        }
        // The tab context menu owns the mouse while open.
        if self.tab_menu.is_some() {
            if let MouseEventKind::Down(_) = m.kind {
                self.tab_menu_click(m.column, m.row);
            }
            return;
        }
        // The workspace context menu / rename modal own the mouse while open.
        if self.ws_menu.is_some() {
            if let MouseEventKind::Down(_) = m.kind {
                self.ws_menu_click(m.column, m.row); // an item, or dismiss
            }
            return;
        }
        // The pane context menu (docs/28) likewise owns the mouse while open.
        if self.pane_menu.is_some() {
            if let MouseEventKind::Down(_) = m.kind {
                self.pane_menu_click(m.column, m.row); // an item, or dismiss
            }
            return;
        }
        // The AGENTS-list context menu (docs/28) owns the mouse while open.
        if self.agent_menu.is_some() {
            if let MouseEventKind::Down(_) = m.kind {
                self.agent_menu_click(m.column, m.row); // an item, or dismiss
            }
            return;
        }
        // FILES-dock menu owns the mouse while open (docs/38); its modals swallow
        // clicks (they own the screen; use the keyboard).
        if self.file_menu.is_some() {
            if let MouseEventKind::Down(_) = m.kind {
                self.file_menu_click(m.column, m.row);
            }
            return;
        }
        if self.diff_menu.is_some() {
            if let MouseEventKind::Down(_) = m.kind {
                self.diff_menu_click(m.column, m.row);
            }
            return;
        }
        // A module dock row's menu (docs/52) owns the mouse while open.
        if self.dock_menu.is_some() {
            if let MouseEventKind::Down(_) = m.kind {
                self.dock_menu_click(m.column, m.row); // an item, or dismiss
            }
            return;
        }
        if self.file_prompt.is_some() {
            if let Some(k) = self.modal_button_key(&m) {
                self.file_prompt_key(k);
            }
            return;
        }
        if self.file_delete.is_some() {
            if let Some(k) = self.modal_button_key(&m) {
                self.file_delete_key(k);
            }
            return;
        }
        if self.worktree_delete.is_some() {
            if let Some(k) = self.modal_button_key(&m) {
                self.worktree_delete_key(k);
            }
            return;
        }
        // The touch switcher overlay (docs/18): tap a row to jump, wheel scrolls
        // (by moving the cursor, which the renderer keeps in view), else dismiss.
        if self.switcher {
            match m.kind {
                MouseEventKind::Down(_) => self.switcher_click(m.column, m.row),
                MouseEventKind::ScrollUp => self.switcher_move(-1),
                MouseEventKind::ScrollDown => self.switcher_move(1),
                _ => {}
            }
            return;
        }
        // Tapping the mobile MENU button opens the full-screen navigator.
        if let (MouseEventKind::Down(MouseButton::Left), Some(r)) =
            (m.kind, self.switcher_button_rect)
        {
            if m.column >= r.x && m.column < r.right() && m.row >= r.y && m.row < r.bottom() {
                self.open_switcher();
                return;
            }
        }
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let hit = |rect: Rect| {
                m.column >= rect.x
                    && m.column < rect.right()
                    && m.row >= rect.y
                    && m.row < rect.bottom()
            };
            if self.mobile_pane_prev_rect.is_some_and(hit) {
                self.cycle_pane(-1);
                return;
            }
            if self.mobile_pane_next_rect.is_some_and(hit) {
                self.cycle_pane(1);
                return;
            }
        }
        // The complete two-row mobile header is chrome. Only MENU acts; every
        // other header tap is consumed instead of leaking into a mouse-aware PTY.
        if self.compact && m.row < self.last_pane_area.y {
            return;
        }
        // Text-input modals: only the ⏎/esc footer buttons respond to the mouse;
        // any other click is swallowed (the centered modal owns the screen).
        if self.worktree_prompt.is_some() {
            if let Some(k) = self.modal_button_key(&m) {
                self.handle_worktree_prompt_key(k);
            }
            return;
        }
        if self.tab_rename.is_some() {
            if let Some(k) = self.modal_button_key(&m) {
                self.handle_tab_rename_key(k);
            }
            return;
        }
        if self.ws_rename.is_some() {
            if let Some(k) = self.modal_button_key(&m) {
                self.handle_ws_rename_key(k);
            }
            return;
        }
        if self.pane_rename.is_some() {
            if let Some(k) = self.modal_button_key(&m) {
                self.handle_pane_rename_key(k);
            }
            return;
        }
        // Bar actions and the read-only overflow popup own their rendered
        // rectangles. This sits below every modal guard: while a modal is open,
        // it owns the screen and a click must never invoke a hidden bar action.
        // An open overflow popup still consumes the next click, closing when it
        // is outside, so input never falls through to a pane behind it.
        let bar_press = matches!(m.kind, MouseEventKind::Down(MouseButton::Left))
            || (self.bar.overflow.is_some() && matches!(m.kind, MouseEventKind::Down(_)));
        if bar_press && self.bar_click(m.column, m.row) {
            return;
        }
        // Track which divider (if any) the cursor is over, for the hover
        // highlight (docs/27, RESIZE-4), plus the sidebar edge seam (docs/29).
        self.update_hover_divider(m.column, m.row);
        self.update_hover_sidebar(m.column, m.row);
        // Right-click a pane tab, WORKSPACES row, agent, file, dock row, or pane
        // to open the matching context menu.
        if let MouseEventKind::Down(MouseButton::Right) = m.kind {
            let (c, r) = (m.column, m.row);
            let hit =
                |rect: Rect| c >= rect.x && c < rect.right() && r >= rect.y && r < rect.bottom();
            if let Some((i, _)) = self.tab_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_tab_menu(*i, c, r);
            } else if let Some((i, _)) = self.ws_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_ws_menu(*i, c, r);
            } else if let Some((id, _)) = self.agent_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_agent_menu(AgentTarget::Live(*id), c, r); // live agent → Close
            } else if let Some((i, _)) = self.session_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_agent_menu(AgentTarget::Session(*i), c, r); // session → Resume/Close
            } else if let Some((row, _)) = self.diff_row_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_diff_menu(*row, c, r);
            } else if let Some((i, _)) = self.file_tree_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_file_menu(*i, c, r); // FILES-dock row → new/rename/delete (docs/38)
            } else if let Some((dock, row_i, _)) = self
                .module_dock_rects
                .iter()
                .find(|(_, _, rect)| hit(*rect))
                .cloned()
            {
                // A module dock row → the menu that row declared (docs/52). Rows
                // without one open nothing, and deliberately do not fall through
                // to the pane menu underneath.
                self.open_dock_menu(&dock, row_i, c, r);
            } else if let Some((id, _)) = self.pane_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_pane_menu(*id, c, r); // no-op on a git/orch dashboard tab
            }
            return;
        }
        // Once `n` arms annotation mode, the source press owns the complete
        // left-button gesture. Dragging extends the range on the same diff
        // side; releasing opens the inline editor. A press/release without
        // movement naturally creates a one-line note.
        if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left))
            && self.diff_note_drag.is_some()
        {
            if let Some((pane, row, side)) = self.diff_source_hit_at(m.column, m.row) {
                self.drag_diff_source(pane, row, side);
            }
            return;
        }
        if matches!(m.kind, MouseEventKind::Up(MouseButton::Left)) && self.diff_note_drag.is_some()
        {
            if let Some((pane, row, side)) = self.diff_source_hit_at(m.column, m.row) {
                self.drag_diff_source(pane, row, side);
            }
            self.finish_diff_source_drag();
            return;
        }
        // ── pane text selection: drag to select, release auto-copies (OSC 52) ──
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A sidebar-edge drag (docs/29) claims the press first: its seam is
                // the sidebar's own `│` column (never a pane), and its neighbour is
                // only grabbed when it isn't pane content, so this can't swallow a
                // click meant for a pane or a mouse-tracking agent.
                if self.begin_sidebar_resize(m.column, m.row) {
                    return;
                }
                // Pane resize (docs/27) takes priority over selection: a divider
                // sits on borders/gaps, outside any content rect, so grabbing one
                // never conflicts. RESIZE-2 = drag the divider directly;
                // RESIZE-5 = `Ctrl`+drag inside a pane grabs the nearest divider.
                if self.begin_resize(m.column, m.row) {
                    return;
                }
                // Saved DIFF note cards own their full visible rectangle. Open
                // the exact clicked note in the existing inline editor before
                // source selection or terminal mouse forwarding can claim it.
                if let Some((pane, note_id)) = self.diff_note_hit_at(m.column, m.row) {
                    self.edit_diff_note(pane, &note_id);
                    return;
                }
                // Native DIFF rows own their source cells. Select the exact
                // stack identity and old/new side before any terminal mouse
                // forwarding or generic text selection can claim the click.
                if let Some((pane, row, side)) = self.diff_source_hit_at(m.column, m.row) {
                    self.press_diff_source(pane, row, side);
                    return;
                }
                if m.modifiers.contains(KeyModifiers::CONTROL) {
                    // A link under the cursor claims the press, but only
                    // provisionally: `Ctrl`+drag is the RESIZE-5 divider grab, so
                    // which gesture this was is decided by whether it moves (see
                    // the Drag and Up arms below).
                    if let Some(h) = self.link_at_screen(m.column, m.row) {
                        self.link_press = Some(LinkPress {
                            target: h.target,
                            at: (m.column, m.row),
                        });
                        return;
                    }
                    if self.begin_resize_nearest(m.column, m.row) {
                        return;
                    }
                }
                // A pane app that tracks the mouse (a TUI agent like Claude
                // Code) gets the click itself — that's how clicking a collapsed
                // tool result expands it, exactly like in a plain terminal. The
                // click still focuses the pane first. `Shift` bypasses
                // forwarding for luvus's own text selection (the standard
                // terminal convention).
                if !m.modifiers.contains(KeyModifiers::SHIFT) && self.begin_mouse_forward(&m, 0) {
                    return;
                }
                // A second left press on (or within one cell of) the first,
                // inside the double-click window, copies and highlights the
                // path / URL / word under the cursor. This point is reached only
                // when the press was *not* forwarded to a mouse-tracking app (its
                // reporting is off, or `Shift` bypassed it), so it never steals a
                // click a pane app wanted.
                let now = Instant::now();
                let is_double = self.last_left_click.take().is_some_and(|(at, when)| {
                    now.duration_since(when) <= DOUBLE_CLICK
                        && m.column.abs_diff(at.0) <= 1
                        && m.row.abs_diff(at.1) <= 1
                });
                if is_double {
                    if self.copy_token_at(m.column, m.row) {
                        // Its release keeps the highlight instead of re-copying.
                        self.dbl_click_release = true;
                        return;
                    }
                } else {
                    self.last_left_click = Some(((m.column, m.row), now));
                }
                // Begin a selection only inside a pane's content; otherwise drop
                // any old one. Falls through to normal click handling (focus/etc).
                self.selection = self
                    .pane_content_at(m.column, m.row)
                    .map(|(pane, content)| {
                        let retained = self
                            .retained_selection_point(pane, content, m.column, m.row)
                            .map(|point| RetainedSelection {
                                anchor: point,
                                cursor: point,
                            });
                        Selection {
                            pane,
                            content,
                            anchor: (m.column, m.row),
                            cursor: (m.column, m.row),
                            retained,
                            scrolled: false,
                            dragging: true,
                        }
                    });
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                // Middle click has no luvus meaning — forward it to a
                // mouse-tracking app (button 1), otherwise ignore it.
                self.begin_mouse_forward(&m, 1);
                return;
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Middle) => {
                // A `Ctrl`+press that began on a link turns into a divider grab
                // the moment it moves; a link only opens on a release that never
                // left its cell.
                if let Some(p) = self.link_press.take() {
                    if (m.column, m.row) == p.at {
                        self.link_press = Some(p);
                        return;
                    }
                    if self.begin_resize_nearest(p.at.0, p.at.1) {
                        self.update_resize(m.column, m.row);
                    }
                    return;
                }
                if self.sidebar_resize.is_some() {
                    self.update_sidebar_resize(m.column, m.row);
                    return;
                }
                if self.resize_drag.is_some() {
                    self.update_resize(m.column, m.row);
                    return;
                }
                // A forwarded press owns its drag — reported with the flags
                // cached at press time (no engine lock), and only when the app
                // asked for drag/motion tracking (a click-only app is left alone).
                if let Some(g) = self.mouse_grab {
                    if g.drag {
                        self.send_grabbed_mouse(g, MouseSeq::Drag, m.column, m.row);
                    }
                    return;
                }
                // Dragging after a double-click turns it back into an ordinary
                // selection, so its release copies what was dragged.
                self.dbl_click_release = false;
                self.update_mouse_selection_cursor(m.column, m.row);
                return;
            }
            MouseEventKind::Up(MouseButton::Left) | MouseEventKind::Up(MouseButton::Middle) => {
                // A double-click already copied and highlighted on its press; its
                // release keeps that selection rather than re-copying it or
                // clearing it the way a plain click would.
                if self.dbl_click_release {
                    self.dbl_click_release = false;
                    return;
                }
                if let Some(p) = self.link_press.take() {
                    if (m.column, m.row) == p.at {
                        self.activate_link(p.target);
                    }
                    return;
                }
                if self.sidebar_resize.is_some() {
                    self.end_sidebar_resize();
                    return;
                }
                if self.resize_drag.is_some() {
                    self.end_resize();
                    return;
                }
                // Close out a forwarded press with its release.
                if let Some(g) = self.mouse_grab.take() {
                    self.send_grabbed_mouse(g, MouseSeq::Release, m.column, m.row);
                    return;
                }
                self.update_mouse_selection_cursor(m.column, m.row);
                if let Some(selection) = self.selection.as_mut() {
                    selection.dragging = false;
                }
                // A real drag copies its text + flashes a toast; a plain click
                // clears the (1-cell) selection so nothing stays highlighted.
                match self.selection_text() {
                    Some(text) => {
                        self.pending_clipboard = Some(text);
                        let msg = self.catalog.copied;
                        self.show_toast(msg);
                    }
                    None => self.selection = None,
                }
                return;
            }
            MouseEventKind::Moved => {
                // Links light up only while `Ctrl` is held, which is both the
                // gesture's own affordance and what keeps this off the hot path:
                // ordinary mouse motion never scans a grid and never takes the
                // engine lock (the PTY reader holds that during output bursts).
                if m.modifiers.contains(KeyModifiers::CONTROL) {
                    // Guarded on the cell *this* resolved for, not on `hover`:
                    // pointing at a link and only then pressing `Ctrl` is the
                    // natural gesture, and it never moves the mouse.
                    if self.link_scan_at != Some((m.column, m.row)) {
                        self.link_scan_at = Some((m.column, m.row));
                        self.hover_link = self.link_at_screen(m.column, m.row);
                    }
                } else if self.link_scan_at.is_some() {
                    self.link_scan_at = None;
                    self.hover_link = None;
                }
                // Hover motion goes only to an any-motion (1003) app under the
                // cursor. Deliberately *not* counted as user input for
                // detection: hover isn't typing, and marking it would mask the
                // agent's working state while the cursor rests on the pane.
                if self.mouse_grab.is_none() {
                    if let Some((id, content)) = self.pane_content_at(m.column, m.row) {
                        if let Some(pane) = self.panes.get(&id) {
                            if let Some(seq) = hover_motion_seq(&m, content, pane.mouse_mode()) {
                                pane.send(&seq);
                            }
                        }
                    }
                }
                return;
            }
            _ => {}
        }
        let scroll: i32 = match m.kind {
            MouseEventKind::Down(MouseButton::Left) => 0,
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => return, // motion / release: hover updated, nothing else to do
        };
        let (c, r) = (m.column, m.row);
        let hit = |rect: Rect| c >= rect.x && c < rect.right() && r >= rect.y && r < rect.bottom();

        if scroll != 0 {
            // Wheel over a sidebar list scrolls it one item per notch (the next
            // render clamps the offset to the list length).
            let step = |off: usize| {
                if scroll < 0 {
                    off.saturating_sub(1)
                } else {
                    off + 1
                }
            };
            if hit(self.workspaces_area) {
                self.workspaces_scroll = step(self.workspaces_scroll);
                return;
            }
            if hit(self.agents_area) {
                self.agents_scroll = step(self.agents_scroll);
                return;
            }
            if hit(self.files_area) {
                if self.files_mode == crate::diff::FilesMode::Diff {
                    self.diff_scroll_by(if scroll < 0 { -1 } else { 1 });
                } else {
                    self.file_tree.scroll = step(self.file_tree.scroll);
                }
                return;
            }
            // Wheel over a git tab scrolls its active view (docs/17).
            if self.active_is_git() && hit(self.last_pane_area) {
                self.git_scroll(scroll);
                return;
            }
            // Wheel over the orchestration board scrolls its list (docs/22).
            if self.active_is_orch() && hit(self.orch_area) {
                self.orch_scroll_by(scroll);
                return;
            }
            // Wheel over Mission Control scrolls its agent list (docs/54).
            if self.active_is_mission() && hit(self.mission_area) {
                let n = self.mission_rows.len();
                self.mission_cursor = match scroll {
                    s if s < 0 => self.mission_cursor.saturating_sub(1),
                    _ if n > 0 => (self.mission_cursor + 1).min(n - 1),
                    _ => self.mission_cursor,
                };
                return;
            }
            // Wheel over a file view (docs/38) scrolls its content.
            if let Some((id, rect)) = self
                .pane_content_rects
                .iter()
                .find(|(id, rect)| self.views.contains_key(id) && hit(*rect))
                .map(|(id, rect)| (*id, *rect))
            {
                let viewport = rect.height.saturating_sub(1) as usize;
                match self.views.get_mut(&id) {
                    Some(crate::app::ViewKind::File(v)) => {
                        let text_w = view_text_w(v, rect.width);
                        v.scroll_by(scroll, viewport, text_w);
                    }
                    Some(crate::app::ViewKind::Diff(v)) => {
                        let rows = v.stack_rows.len().max(v.split_rows.len());
                        if scroll < 0 {
                            v.scroll = v.scroll.saturating_sub(3);
                        } else {
                            v.scroll = v
                                .scroll
                                .saturating_add(3)
                                .min(rows.saturating_sub(viewport));
                        }
                        v.selected = v.scroll;
                    }
                    None => {}
                }
                return;
            }
            // Otherwise the wheel scrolls the pane under the cursor.
            if let Some(id) = self
                .pane_rects
                .iter()
                .find(|(_, rect)| hit(*rect))
                .map(|(id, _)| *id)
            {
                let up = scroll < 0;
                // Pane-local, 1-based coordinates for a forwarded mouse event.
                let content = self
                    .pane_content_rects
                    .iter()
                    .find(|(pid, _)| *pid == id)
                    .map(|(_, r)| *r);
                // Set after the pane borrow ends: `Some(v)` writes `scroll_pane = v`.
                let mut set_scroll: Option<Option<PaneId>> = None;
                // Forwarding the wheel makes the app repaint; that output is the
                // user scrolling, not the agent working (docs/07).
                let mut scrolled_the_app = false;
                let extending_selection = self.selection.is_some_and(|selection| {
                    selection.pane == id && selection.dragging && selection.retained.is_some()
                });
                let mut scrolled_selection = false;
                if let Some(pane) = self.panes.get(&id) {
                    let mm = pane.mouse_mode();
                    if extending_selection && !pane.alt_screen() {
                        // The selection gesture owns primary-screen scrolling,
                        // even when the child reports mouse input. Its endpoints
                        // are retained-history rows, so the original anchor stays
                        // attached to the same text while the cursor extends.
                        pane.scroll(-scroll);
                        set_scroll = Some((pane.scroll_state().0 > 0).then_some(id));
                        scrolled_selection = true;
                    } else if mm.report {
                        // The app tracks the mouse (e.g. a TUI agent like Claude
                        // Code on the alternate screen) — forward the wheel so it
                        // scrolls its own transcript, exactly like a real terminal.
                        let base = content.unwrap_or(Rect::new(0, 0, 1, 1));
                        let col = m.column.saturating_sub(base.x) + 1;
                        let row = m.row.saturating_sub(base.y) + 1;
                        // Preserve the terminal protocol's one-event/one-report
                        // boundary. In particular, Windows ConPTY may coalesce
                        // rapid writes; sending duplicates here can make a TUI
                        // receive several concatenated SGR reports as one input
                        // record and reject the entire wheel action.
                        pane.send(&mouse_wheel_seq(up, col, row, mm.sgr));
                        scrolled_the_app = true;
                    } else if !pane.alt_screen() {
                        // Primary screen with real history: scroll luvus's
                        // scrollback viewport (`scroll` is -3 up / +3 down, and a
                        // positive delta scrolls up into history — so negate it).
                        pane.scroll(-scroll);
                        // Engage keyboard scroll mode while scrolled up (so the
                        // number/j/k keys work); disengage once back at live.
                        set_scroll = Some((pane.scroll_state().0 > 0).then_some(id));
                    } else if mm.alternate_scroll {
                        // The application explicitly requested alternate
                        // scrolling, so translate wheel movement into its
                        // cursor-key scroll input. Without that mode there is
                        // no host history on an alternate screen to move.
                        let seq: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
                        for _ in 0..scroll.abs() {
                            pane.send(seq);
                        }
                        scrolled_the_app = true;
                    }
                }
                if scrolled_the_app {
                    self.mark_input_for(id);
                }
                if let Some(v) = set_scroll {
                    self.scroll_pane = v;
                }
                if scrolled_selection {
                    if let Some(selection) = self.selection.as_mut() {
                        selection.scrolled = true;
                    }
                    self.update_mouse_selection_cursor(m.column, m.row);
                }
            }
            return;
        }

        // The sidebar gear opens Settings.
        if self.settings_icon_rect.is_some_and(hit) {
            self.open_settings();
            return;
        }
        // The version number opens the changelog modal.
        if self.version_rect.is_some_and(hit) {
            self.open_changelog();
            return;
        }
        // The `«`/`»` chevrons show/hide their sidebar — same as ⌃Space b (left)
        // / ⌃Space B (right).
        if self.sidebar_toggle_rect.is_some_and(hit) {
            self.toggle_side(crate::app::Side::Left);
            return;
        }
        if self.right_sidebar_toggle_rect.is_some_and(hit) {
            self.toggle_side(crate::app::Side::Right);
            return;
        }
        // Left click: close/add buttons first, then tabs → agents → ws → panes.
        if let Some((i, _)) = self.tab_close_rects.iter().find(|(_, rect)| hit(*rect)) {
            self.close_tab(*i);
            return;
        }
        // The focused pane's ✕ button closes the active pane.
        if self.pane_close_rect.is_some_and(hit) {
            self.close_pane(self.layout().focus);
            return;
        }
        // Its ⤢ button toggles zoom — the touch equivalent of `Ctrl+Space z`, so
        // a split can be expanded to fullscreen on a phone (docs/18).
        if self.pane_zoom_rect.is_some_and(hit) {
            self.zoomed = !self.zoomed;
            return;
        }
        // Clicking a pane's title strip opens the running-command overlay — the
        // full argv from the OS, since an agent's on-screen `Bash(… …)` is
        // elided before it ever reaches us.
        if let Some((id, _)) = self
            .pane_title_rects
            .iter()
            .find(|(_, rect)| hit(*rect))
            .map(|(id, r)| (*id, *r))
        {
            self.open_cmd_inspect(id);
            return;
        }
        // Tab-bar scroll arrows: step to the previous / next tab.
        if self.tab_prev_rect.is_some_and(hit) {
            let a = self.ws().active_tab;
            if a > 0 {
                self.switch_tab(a - 1);
            }
            return;
        }
        if self.tab_next_rect.is_some_and(hit) {
            let a = self.ws().active_tab;
            if a + 1 < self.ws().tabs.len() {
                self.switch_tab(a + 1);
            }
            return;
        }
        if let Some(rect) = self.new_ws_rect {
            if hit(rect) {
                self.open_folder_picker(); // "+" → choose a folder to open as a workspace
                return;
            }
        }
        if let Some((i, _)) = self.tab_rects.iter().find(|(_, rect)| hit(*rect)) {
            let i = *i;
            if i >= self.ws().tabs.len() {
                self.new_tab(); // the "+" button
            } else {
                self.switch_tab(i);
            }
            return;
        }
        // The AGENTS All/Active filter toggle.
        if let Some((val, _)) = self.agents_filter_rects.iter().find(|(_, rect)| hit(*rect)) {
            let val = *val;
            if self.agents_active_only != val {
                self.agents_active_only = val;
                self.agents_scroll = 0;
            }
            return;
        }
        if let Some((id, _)) = self.agent_rects.iter().find(|(_, rect)| hit(*rect)) {
            let id = *id;
            self.focus_pane_global(id);
            return;
        }
        // Clicking a resumable session row reopens it into a pane.
        if let Some((i, _)) = self.session_rects.iter().find(|(_, rect)| hit(*rect)) {
            let i = *i;
            self.resume_session(i);
            return;
        }
        if let Some((mode, _)) = self.files_mode_rects.iter().find(|(_, rect)| hit(*rect)) {
            self.set_files_mode(*mode);
            return;
        }
        if let Some((row, _)) = self.diff_row_rects.iter().find(|(_, rect)| hit(*rect)) {
            let row = *row;
            let target = if m.modifiers.contains(KeyModifiers::SHIFT) {
                crate::app::files::OpenTarget::Pane
            } else {
                crate::app::files::OpenTarget::Preview
            };
            self.diff_row_activate(row, target);
            return;
        }
        // Clicking a FILES row expands/collapses a folder or opens a file (docs/38).
        // A plain click opens the file in a full tab (the native default); Shift
        // opens it in a pane split beside the focus.
        if let Some((i, _)) = self.file_tree_rects.iter().find(|(_, rect)| hit(*rect)) {
            let i = *i;
            let target = if m.modifiers.contains(KeyModifiers::SHIFT) {
                crate::app::files::OpenTarget::Pane
            } else {
                crate::app::files::OpenTarget::Tab
            };
            self.file_row_activate(i, target);
            return;
        }
        // Clicking a module dock row with an action invokes it (docs/29, DOCK-4).
        if let Some((dock_id, row_i, _)) = self
            .module_dock_rects
            .iter()
            .find(|(_, _, rect)| hit(*rect))
            .cloned()
        {
            if let Some(row) = self
                .module_docks
                .get(&dock_id)
                .and_then(|d| d.rows.get(row_i))
                .cloned()
            {
                if let Some(action) = row.action {
                    let owner = self.module_owning_dock(&dock_id);
                    // Tell the action *which* row was clicked, so one action can
                    // serve a whole list (docs/13 §3.10).
                    let extra = vec![
                        ("LUVUS_MODULE_DOCK_ID".to_string(), dock_id.clone()),
                        ("LUVUS_MODULE_ROW_INDEX".to_string(), row_i.to_string()),
                        ("LUVUS_MODULE_ROW_TEXT".to_string(), row.text.clone()),
                        (
                            "LUVUS_MODULE_ROW_VALUE".to_string(),
                            row.value.unwrap_or(row.text),
                        ),
                    ];
                    let _ = self.module_invoke_dock_action(&action, owner.as_deref(), extra);
                }
            }
            return;
        }
        if let Some((i, _)) = self.ws_rects.iter().find(|(_, rect)| hit(*rect)) {
            let i = (*i).min(self.workspaces.len().saturating_sub(1));
            self.active_ws = i;
            return;
        }
        // Clicking a view-selector tab in the git tab switches section (docs/17).
        if self.active_is_git() {
            if let Some((s, _)) = self.git_section_rects.iter().find(|(_, rect)| hit(*rect)) {
                let s = *s;
                self.git_click_section(s);
                return;
            }
            // The Status view's contributors "show more / show less" row toggles
            // the list between the meaningful-only default and every author.
            if self
                .active_git()
                .and_then(|g| g.contributors_more_rect)
                .is_some_and(hit)
            {
                self.git_toggle_contributors();
                return;
            }
            // Clicking a file row in the Status section opens its diff.
            if let Some((path, staged)) = self.git_status_file_at(m.column, m.row) {
                self.git_open_status_diff_with(Some((path, staged)));
                return;
            }
            // Clicking a list row opens its detail in-tab (docs/17) — commit `git
            // show`, PR panel, or issue detail. `esc` goes back to the list.
            if let Some(idx) = self.git_list_row_at(m.column, m.row) {
                self.git_click_row(idx);
                return;
            }
        }
        // Clicking a task row on the board selects it (docs/22, ORCH-7).
        if self.active_is_orch() {
            let body_top = self.orch_area.y + 2; // header + separator
            if hit(self.orch_area) && m.row >= body_top {
                let idx = self.orch_scroll + (m.row - body_top) as usize;
                if idx < self.orch.tasks.len() {
                    self.orch_cursor = idx;
                }
            }
            return;
        }
        // Clicking an agent row in Mission Control jumps straight to that session's
        // pane (or resumes it), the whole point of the tab (docs/54).
        if self.active_is_mission() {
            // A click dismisses an open overlay (detail / answer) rather than
            // acting behind it.
            if self.mission_detail.take().is_some() || self.mission_answer.take().is_some() {
                return;
            }
            if self.mission_refresh_rect.is_some_and(hit) {
                self.request_mission_usage_refresh();
                return;
            }
            if let Some((scope, _)) = self.mission_scope_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.set_mission_scope(*scope);
                return;
            }
            // Agent rows are intentionally keyboard-only for now. A plain click
            // must never jump away from Mission Control unexpectedly.
            return;
        }
        if let Some((id, _)) = self.pane_rects.iter().find(|(_, rect)| hit(*rect)) {
            let id = *id;
            if self.layout().focus != id {
                // Leave the old pane's viewport exactly where it is. Only drop
                // keyboard ownership so subsequent input follows the new focus.
                self.scroll_pane = None;
            }
            self.layout_mut().focus = id;
            self.mode = Mode::Normal;
        }
    }

    /// Scroll the focused pane's scrollback for a fixed prefix key (PageUp/Down
    /// a page at a time, Home/End to the top / live bottom).
    fn scroll_focused_pane(&mut self, code: KeyCode) {
        self.search_flash = None; // scrolling dismisses the search-jump marker
        let focus = self.layout().focus;
        // A "page" is the visible content height minus one row of overlap.
        let page = self
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == focus)
            .map(|(_, r)| r.height.saturating_sub(1).max(1) as i32)
            .unwrap_or(10);
        if let Some(p) = self.focused() {
            match code {
                KeyCode::PageUp => p.scroll(page),
                KeyCode::PageDown => p.scroll(-page),
                KeyCode::Home => p.scroll_to_top(),
                KeyCode::End => p.scroll_to_bottom(),
                _ => {}
            }
        }
    }

    /// The focused pane's content height minus one row — a "page" for scrolling.
    fn focused_page(&self) -> i32 {
        let focus = self.layout().focus;
        self.pane_content_rects
            .iter()
            .find(|(id, _)| *id == focus)
            .map(|(_, r)| r.height.saturating_sub(1).max(1) as i32)
            .unwrap_or(10)
    }

    /// Enter keyboard scroll mode on the focused pane, scrolling up `lines` to
    /// start. Returns false (no-op) for an alt-screen pane — its history isn't in
    /// luvus's scrollback, so the app owns scrolling there.
    fn enter_scroll_mode(&mut self, lines: i32) -> bool {
        let id = self.layout().focus;
        match self.panes.get(&id) {
            Some(p) if !p.alt_screen() => {
                p.scroll(lines);
                self.scroll_pane = Some(id);
                true
            }
            _ => false,
        }
    }

    /// Handle one key while in keyboard scroll mode; always consumes it. Plain
    /// keys navigate the focused pane's scrollback and never reach the agent:
    /// `j`/`k`/arrows = lines, `f`/`b`/Space/PageUp/Down = pages, `g`/`G` =
    /// top/live, `1`–`9` = jump (1 oldest … 9 newest), `0`/`G`/`q`/`Esc`/typing =
    /// back to live. See [`App::scroll_pane`].
    /// Keyboard resize mode (docs/27, RESIZE-3): arrows / `hjkl` resize the
    /// focused pane, `=`/`0` equalize, anything else (`Esc`/`Enter`/`q`/…) exits.
    fn handle_resize_mode_key(&mut self, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::KeyModifiers;
        const STEP: i16 = 3;
        let big = key.modifiers.contains(KeyModifiers::SHIFT)
            || matches!(key.code, KeyCode::Char('H' | 'J' | 'K' | 'L'));
        let step = if big { STEP * 2 } else { STEP };
        let dir = match key.code {
            KeyCode::Left | KeyCode::Char('h' | 'H') => Some(Dir::Left),
            KeyCode::Down | KeyCode::Char('j' | 'J') => Some(Dir::Down),
            KeyCode::Up | KeyCode::Char('k' | 'K') => Some(Dir::Up),
            KeyCode::Right | KeyCode::Char('l' | 'L') => Some(Dir::Right),
            _ => None,
        };
        if let Some(dir) = dir {
            let area = self.last_pane_area;
            self.layout_mut().resize_focused(area, dir, step);
            return true;
        }
        if matches!(key.code, KeyCode::Char('=' | '+' | '0')) {
            self.layout_mut().equalize();
            return true;
        }
        // Esc / Enter / q / the prefix / any other key leaves resize mode.
        self.mode = Mode::Normal;
        true
    }

    fn handle_scroll_mode_key(&mut self, key: KeyEvent) -> bool {
        let Some(id) = self.scroll_pane else {
            return false;
        };
        let page = self.focused_page();
        let newline = self.config.shift_enter_bytes();
        let mut exit = false;
        if let Some(pane) = self.panes.get(&id) {
            match key.code {
                KeyCode::Char('k') | KeyCode::Up => pane.scroll(1),
                KeyCode::Char('j') | KeyCode::Down => pane.scroll(-1),
                KeyCode::Char('b') | KeyCode::PageUp => pane.scroll(page),
                KeyCode::Char('f') | KeyCode::Char(' ') | KeyCode::PageDown => pane.scroll(-page),
                KeyCode::Char('g') | KeyCode::Home => pane.scroll_to_top(),
                KeyCode::Char('G') | KeyCode::End => {
                    pane.scroll_to_bottom();
                    exit = true;
                }
                KeyCode::Char(d @ '0'..='9') => {
                    let digit = d as i32 - '0' as i32;
                    let (cur, len) = pane.scroll_state();
                    // 1 = oldest (top of history) … 9 = newest; 0 = live bottom.
                    let target = if digit == 0 {
                        0
                    } else {
                        len as i32 * (10 - digit) / 9
                    };
                    pane.scroll(target - cur as i32);
                    if digit == 0 {
                        exit = true;
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                    pane.scroll_to_bottom();
                    exit = true;
                }
                _ => {
                    // Any other key leaves scroll mode (snap to live) and is
                    // forwarded, so typing to the agent resumes with no lost key.
                    pane.scroll_to_bottom();
                    exit = true;
                    if let Some(bytes) = encode_key(&key, newline, pane.application_cursor()) {
                        pane.send(&bytes);
                    }
                }
            }
        } else {
            exit = true; // the pane vanished
        }
        if exit {
            self.scroll_pane = None;
        }
        true
    }

    /// Begin keyboard copy mode at the visible viewport's top-left cell. The
    /// selection uses absolute history rows, so scrolling cannot invalidate it.
    pub(super) fn begin_copy_mode(&mut self) -> bool {
        let id = self.layout().focus;
        let Some(pane) = self.panes.get(&id) else {
            return false;
        };
        let (offset, history) = pane.scroll_state();
        self.selection = None;
        self.scroll_pane = None;
        self.copy_mode = Some(CopyMode {
            pane: id,
            anchor: (history.saturating_sub(offset), 0),
            cursor: (history.saturating_sub(offset), 0),
            saved_scroll: offset,
        });
        true
    }

    /// Leave copy mode without copying, returning to the exact viewport where
    /// it began. This is intentionally different from regular scroll mode,
    /// whose cancel action returns to live output.
    fn cancel_copy_mode(&mut self) {
        if let Some(copy) = self.copy_mode.take() {
            if let Some(pane) = self.panes.get(&copy.pane) {
                pane.scroll_to(copy.saved_scroll);
            }
        }
    }

    /// Keep a copy cursor on screen. The engine's offset is measured from the
    /// live bottom while copy coordinates are measured from retained-history
    /// top, hence `history - row` for the target offset.
    fn reveal_copy_cursor(&mut self) {
        let Some(copy) = self.copy_mode else {
            return;
        };
        let Some(pane) = self.panes.get(&copy.pane) else {
            self.copy_mode = None;
            return;
        };
        let (offset, history) = pane.scroll_state();
        let height = self
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == copy.pane)
            .map(|(_, rect)| rect.height.max(1) as usize)
            .unwrap_or(1);
        let top = history.saturating_sub(offset);
        let bottom = top.saturating_add(height.saturating_sub(1));
        let target_top = if copy.cursor.0 < top {
            copy.cursor.0
        } else if copy.cursor.0 > bottom {
            copy.cursor.0.saturating_add(1).saturating_sub(height)
        } else {
            return;
        };
        pane.scroll_to(history.saturating_sub(target_top));
    }

    /// Copy the keyboard selection by the same clipboard queue as drag-to-copy.
    pub(super) fn finish_copy_mode(&mut self) {
        let Some(copy) = self.copy_mode.take() else {
            return;
        };
        let is_codex = self
            .status
            .get(&copy.pane)
            .is_some_and(|status| status.agent == "codex");
        let text = self
            .panes
            .get(&copy.pane)
            .and_then(|pane| pane.retained_selection_text(copy.ordered()))
            .and_then(finish_selected_text);
        if let Some(text) = text.map(|text| {
            if is_codex {
                strip_uniform_single_cell_margin(text)
            } else {
                text
            }
        }) {
            self.pending_clipboard = Some(text);
            let msg = self.catalog.copied;
            self.show_toast(msg);
        }
        // A successful copy returns to a live terminal, so the next key is
        // immediately visible where the child expects it.
        if let Some(pane) = self.panes.get(&copy.pane) {
            pane.scroll_to_bottom();
        }
    }

    /// Copy-mode navigation starts from the configured prefix command, then hjkl/arrows, word jumps,
    /// page keys, Home/End, and g/G move the visual selection; y copies it.
    fn handle_copy_mode_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut copy) = self.copy_mode else {
            return false;
        };
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.cancel_copy_mode();
            return true;
        }
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
            self.finish_copy_mode();
            return true;
        }
        if matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V')) {
            if let Some(copy) = self.copy_mode.as_mut() {
                copy.anchor = copy.cursor;
            }
            return true;
        }
        let Some(pane) = self.panes.get(&copy.pane) else {
            self.cancel_copy_mode();
            return true;
        };
        let row_count = pane.retained_row_count();
        if row_count == 0 {
            self.cancel_copy_mode();
            return true;
        }
        let last_row = row_count.saturating_sub(1);
        let page = self.focused_page() as usize;
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => copy.cursor.1 = copy.cursor.1.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => {
                let last = pane
                    .retained_row_layout(copy.cursor.0)
                    .map_or(0, |layout| layout.last_column());
                copy.cursor.1 = copy.cursor.1.saturating_add(1).min(last);
            }
            KeyCode::Up | KeyCode::Char('k') => copy.cursor.0 = copy.cursor.0.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => copy.cursor.0 = (copy.cursor.0 + 1).min(last_row),
            KeyCode::PageUp | KeyCode::Char('b') => {
                copy.cursor.0 = copy.cursor.0.saturating_sub(page)
            }
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
                copy.cursor.0 = copy.cursor.0.saturating_add(page).min(last_row)
            }
            KeyCode::Home | KeyCode::Char('g') => copy.cursor.0 = 0,
            KeyCode::End | KeyCode::Char('G') => copy.cursor.0 = last_row,
            KeyCode::Char('0') => copy.cursor.1 = 0,
            KeyCode::Char('$') => {
                copy.cursor.1 = pane
                    .retained_row_layout(copy.cursor.0)
                    .map_or(0, |layout| layout.last_column())
            }
            KeyCode::Char('w') => {
                copy.cursor =
                    copy_word_forward(row_count, |row| pane.retained_row_layout(row), copy.cursor)
            }
            KeyCode::Char('B') => {
                copy.cursor = copy_word_back(|row| pane.retained_row_layout(row), copy.cursor)
            }
            _ => return true,
        }
        copy.cursor.1 = copy.cursor.1.min(
            pane.retained_row_layout(copy.cursor.0)
                .map_or(0, |layout| layout.last_column()),
        );
        self.copy_mode = Some(copy);
        self.reveal_copy_cursor();
        true
    }

    /// The pane whose **content** rect covers terminal cell `(x, y)`.
    /// Try to forward a button press at the event's position into a
    /// mouse-tracking pane app. On success: focuses the pane, snaps its
    /// viewport live, records the grab — the pressed button with its modifier
    /// bits, plus the app's drag/SGR flags — so the rest of the gesture is
    /// **lock-free** (one engine lock per gesture, at press), and sends the
    /// press. Returns whether the press was forwarded.
    fn begin_mouse_forward(
        &mut self,
        m: &ratatui::crossterm::event::MouseEvent,
        base_btn: u16,
    ) -> bool {
        let Some((id, _)) = self.pane_content_at(m.column, m.row) else {
            return false;
        };
        let Some(pane) = self.panes.get(&id) else {
            return false;
        };
        let mm = pane.mouse_mode();
        if !mm.report {
            return false;
        }
        pane.scroll_to_bottom(); // the app's coordinates are the live screen's
        self.scroll_pane = None;
        self.layout_mut().focus = id;
        self.mode = Mode::Normal;
        let g = crate::app::MouseGrab {
            pane: id,
            btn: base_btn + mouse_mod_bits(m.modifiers),
            drag: mm.drag,
            sgr: mm.sgr,
        };
        self.mouse_grab = Some(g);
        self.send_grabbed_mouse(g, MouseSeq::Press, m.column, m.row);
        true
    }

    /// Send one event of a forwarded gesture using the grab's cached flags —
    /// no engine lock. Coordinates are translated to pane-local 1-based cells,
    /// clamped into the pane's content so a drag that wanders outside still
    /// reports sane positions. Counts as user input for detection, like the
    /// forwarded wheel.
    fn send_grabbed_mouse(&mut self, g: crate::app::MouseGrab, kind: MouseSeq, x: u16, y: u16) {
        let Some(content) = self
            .pane_content_rects
            .iter()
            .find(|(pid, _)| *pid == g.pane)
            .map(|(_, r)| *r)
        else {
            return;
        };
        let cx = x.clamp(content.x, content.right().saturating_sub(1));
        let cy = y.clamp(content.y, content.bottom().saturating_sub(1));
        let col = cx - content.x + 1;
        let row = cy - content.y + 1;
        if let Some(pane) = self.panes.get(&g.pane) {
            pane.send(&mouse_button_seq(g.btn, kind, col, row, g.sgr));
        }
        self.mark_input_for(g.pane);
    }

    /// `pub(super)` so the resize hit-test in `app` can share this exact rule:
    /// a cell inside a pane's content belongs to the pane, never to a divider.
    pub(super) fn pane_content_at(&self, x: u16, y: u16) -> Option<(PaneId, Rect)> {
        self.pane_content_rects
            .iter()
            .find(|(_, r)| x >= r.x && x < r.right() && y >= r.y && y < r.bottom())
            .map(|(id, r)| (*id, *r))
    }

    /// Map one visible terminal cell into the pane's retained-history space.
    /// Native file and diff views intentionally stay in screen coordinates.
    fn retained_selection_point(
        &self,
        pane: PaneId,
        content: Rect,
        x: u16,
        y: u16,
    ) -> Option<(usize, usize)> {
        if self.views.contains_key(&pane) {
            return None;
        }
        let pane = self.panes.get(&pane)?;
        let (visible_top, row_count) = pane.retained_viewport()?;
        if row_count == 0 {
            return None;
        }
        let row = visible_top
            .saturating_add(y.saturating_sub(content.y) as usize)
            .min(row_count.saturating_sub(1));
        let col = x
            .saturating_sub(content.x)
            .min(content.width.saturating_sub(1)) as usize;
        Some((row, col))
    }

    /// Move the visible cursor endpoint and, for terminal panes, resolve that
    /// cell against the viewport *now*. The retained anchor is never rewritten.
    fn update_mouse_selection_cursor(&mut self, x: u16, y: u16) {
        let Some(selection) = self.selection else {
            return;
        };
        let content = selection.content;
        let screen = (
            x.clamp(content.x, content.right().saturating_sub(1)),
            y.clamp(content.y, content.bottom().saturating_sub(1)),
        );
        let retained = selection.retained.and_then(|_| {
            self.retained_selection_point(selection.pane, content, screen.0, screen.1)
        });
        if let Some(selection) = self.selection.as_mut() {
            selection.cursor = screen;
            if let (Some(cursor), Some(retained)) = (retained, selection.retained.as_mut()) {
                retained.cursor = cursor;
            }
        }
    }

    /// Extract the current selection's text from the pane's grid (linear, with
    /// trailing blanks trimmed). `None` for a click without a drag or empty text.
    pub(crate) fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        if !sel.has_range() {
            return None;
        }
        // A file-view leaf (docs/38) has no VT grid — pull the selected text from
        // its rendered lines instead, so drag-to-copy works just like a pane.
        if let Some(crate::app::ViewKind::File(v)) = self.views.get(&sel.pane) {
            return crate::files::selection_text(v, sel.content, sel.ordered());
        }
        if let Some(selection) = sel.retained {
            let range = selection.ordered();
            let text = self
                .panes
                .get(&sel.pane)?
                .retained_selection_text(range)
                .and_then(finish_selected_text)?;
            let is_codex = self
                .status
                .get(&sel.pane)
                .is_some_and(|status| status.agent == "codex");
            return Some(if range.0 .1 == 0 || is_codex {
                strip_uniform_single_cell_margin(text)
            } else {
                text
            });
        }
        let ((sx, sy), (ex, ey)) = sel.ordered();
        let (cx, cy) = (sel.content.x, sel.content.y);
        let text = self.panes.get(&sel.pane)?.visible_selection_text((
            (
                (sy as usize).saturating_sub(cy as usize),
                (sx as usize).saturating_sub(cx as usize),
            ),
            (
                (ey as usize).saturating_sub(cy as usize),
                (ex as usize).saturating_sub(cx as usize),
            ),
        ))?;
        // A drag may begin in the single blank pane cell before uniformly
        // aligned prose. Codex also emits that one-cell transcript gutter even
        // when the drag starts on its first visible character. It remains
        // visibly selected, but is padding rather than useful clipboard text.
        let is_codex = self
            .status
            .get(&sel.pane)
            .is_some_and(|status| status.agent == "codex");
        Some(if sx == cx || is_codex {
            strip_uniform_single_cell_margin(text)
        } else {
            text
        })
    }

    /// Copy the path, URL, or word under screen cell (`col`, `row`) to the
    /// clipboard and highlight it — the double-click gesture. A path or URL is
    /// copied as its full raw token (`src/main.rs:42:7` verbatim, a soft-wrapped
    /// path rejoined); anything else falls back to the whitespace-delimited word,
    /// and a whitespace or empty cell copies nothing. Returns whether it copied.
    ///
    /// Uses the pure [`crate::links::link_at`], not [`Self::link_at_screen`]:
    /// copying doesn't need the file to exist, so a `pwd` directory or a
    /// not-yet-created path still copies.
    fn copy_token_at(&mut self, col: u16, row: u16) -> bool {
        let Some((pane, content)) = self.pane_content_at(col, row) else {
            return false;
        };
        let rows = {
            let Some(p) = self.panes.get(&pane) else {
                return false;
            };
            let Ok(engine) = p.engine.lock() else {
                return false;
            };
            engine.visible_rows()
        };
        let (gcol, grow) = (col - content.x, row - content.y);
        let (text, spans) = match crate::links::link_at(&rows, gcol, grow) {
            Some(link) => {
                let text = match link.hit {
                    crate::links::Hit::Url(u) => u,
                    crate::links::Hit::Path { raw, .. } => raw,
                };
                (text, link.spans)
            }
            None => match word_at_grid(&rows, gcol, grow) {
                Some(pair) => pair,
                None => return false,
            },
        };
        if text.is_empty() {
            return false;
        }
        // Highlight exactly the copied cells: from the first covered cell to the
        // last, which for a rejoined soft-wrapped path runs through the full rows
        // between them (the same reading-order rule `Selection` copies with). The
        // highlight is transient (screen coordinates, cleared on the next click),
        // so it carries no retained-history span.
        if let (Some(first), Some(last)) = (spans.first(), spans.last()) {
            self.selection = Some(Selection {
                pane,
                content,
                anchor: (content.x + first.1, content.y + first.0),
                cursor: (content.x + last.2.saturating_sub(1), content.y + last.0),
                retained: None,
                scrolled: false,
                dragging: false,
            });
        }
        self.pending_clipboard = Some(text);
        let msg = self.catalog.copied;
        self.show_toast(msg);
        true
    }

    /// Show a transient toast (e.g. "Copied") bottom-center for ~1.4s.
    /// Open the "what's running here?" overlay for `id`, snapshotting the pane's
    /// process tree from the OS. Shelling out to `ps` is why this happens on the
    /// click and not per frame.
    pub fn open_cmd_inspect(&mut self, id: PaneId) {
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let cwd = pane.cwd.clone();
        let pid = pane.child_pid.load(std::sync::atomic::Ordering::SeqCst);
        let procs = if pid != 0 {
            crate::platform::process_tree(pid)
        } else {
            Vec::new()
        };
        self.cmd_inspect = Some(CmdInspect {
            pane: id,
            cwd,
            procs,
            scroll: 0,
        });
    }

    /// Re-read the process tree for the open overlay (`r`), so a long-running
    /// command's progress is visible without reopening.
    pub fn refresh_cmd_inspect(&mut self) {
        if let Some(c) = self.cmd_inspect.as_ref() {
            let id = c.pane;
            let scroll = c.scroll;
            self.open_cmd_inspect(id);
            if let Some(c) = self.cmd_inspect.as_mut() {
                c.scroll = scroll;
            }
        }
    }

    pub fn close_cmd_inspect(&mut self) {
        self.cmd_inspect = None;
    }

    /// The link under screen cell (`col`, `row`), **resolved**, if the cursor is
    /// over a pane and what it found is real.
    ///
    /// Takes the pane's VT engine lock, so it runs only on a deliberate gesture
    /// (`Ctrl` held, or a `Ctrl`+press) and never on plain mouse motion.
    pub fn link_at_screen(&self, col: u16, row: u16) -> Option<HoverLink> {
        let (pane, content) = self.pane_content_at(col, row)?;
        let rows = {
            let engine = self.panes.get(&pane)?.engine.lock().ok()?;
            engine.visible_rows()
        };
        let link = crate::links::link_at(&rows, col - content.x, row - content.y)?;
        let target = match &link.hit {
            crate::links::Hit::Url(u) => {
                crate::platform::is_openable_url(u).then(|| LinkTarget::Url(u.clone()))?
            }
            // A file on disk wins over a domain, which is what settles the genuine
            // ambiguity: `main.rs` and `README.md` are also valid domains (`.rs`
            // is Serbia, `.md` Moldova), so in a repo they open as files and
            // elsewhere they simply stay inert.
            crate::links::Hit::Path { raw, text, line } => {
                match self.resolve_pane_path(pane, text) {
                    Some(path) => LinkTarget::File { path, line: *line },
                    None => {
                        let url = crate::links::domain_url(crate::links::as_domain(raw)?);
                        crate::platform::is_openable_url(&url).then_some(LinkTarget::Url(url))?
                    }
                }
            }
        };
        Some(HoverLink { pane, link, target })
    }

    /// Resolve a path written in pane `id`'s grid against that pane's working
    /// directory, accepting it only if it names a real **file**.
    ///
    /// The existence check is what makes paths trustworthy to click: prose that
    /// merely looks path-shaped never lights up, and a directory never opens
    /// (`src` stays inert while `src/main.rs` does not).
    fn resolve_pane_path(&self, id: PaneId, text: &str) -> Option<PathBuf> {
        let p = match text.strip_prefix("~/") {
            Some(rest) => crate::platform::home_dir()?.join(rest),
            None => PathBuf::from(text),
        };
        let p = if p.is_absolute() {
            p
        } else {
            self.panes.get(&id)?.cwd.join(p)
        };
        p.is_file().then_some(p)
    }

    /// Act on a resolved link (docs/58): a URL goes to the client's browser, a
    /// file opens in luvus itself.
    pub fn activate_link(&mut self, target: LinkTarget) {
        match target {
            LinkTarget::Url(url) => self.open_url(url),
            LinkTarget::File { path, line } => self.open_file_at(path, line),
        }
    }

    /// Queue `url` to open in the *client's* browser (docs/58) and confirm with a
    /// toast, so a click that lands on a stale cell is visibly a no-op rather
    /// than a silent one.
    ///
    /// Re-checks the scheme here as well as at the spawn: this is reachable from
    /// the context menu and the socket, not only from the click path.
    pub fn open_url(&mut self, url: String) {
        if !crate::platform::is_openable_url(&url) {
            return;
        }
        self.show_toast(crate::ui::truncate(&url, 60));
        self.pending_open_url = Some(url);
    }

    pub fn show_toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now() + Duration::from_millis(1400)));
    }

    /// Clear an expired toast; returns true when it changed (so the loop redraws
    /// once to remove it, since idle frames aren't rendered).
    pub fn tick_toast(&mut self, now: Instant) -> bool {
        if self.toast.as_ref().is_some_and(|(_, exp)| now >= *exp) {
            self.toast = None;
            true
        } else {
            false
        }
    }

    /// Clear an expired search-jump flash (docs/63); returns true when it changed
    /// so the loop repaints once to remove the highlight.
    pub fn tick_search_flash(&mut self, now: Instant) -> bool {
        if self.search_flash.as_ref().is_some_and(|f| now >= f.until) {
            self.search_flash = None;
            true
        } else {
            false
        }
    }

    /// Record that the user just typed into the focused pane, so detection can
    /// tell typing (whose echo is PTY output) apart from the agent generating
    /// (docs/07). Only the focused pane receives typed input.
    fn mark_user_input(&mut self) {
        // Typing into the pane dismisses the search-jump marker (docs/63): you
        // have moved on, so the highlight should not linger.
        self.search_flash = None;
        let id = self.layout().focus;
        self.mark_input_for(id);
    }

    /// Same, for a specific pane — the wheel targets the pane under the cursor,
    /// which is not necessarily the focused one.
    fn mark_input_for(&mut self, id: PaneId) {
        if let Some(s) = self.status.get_mut(&id) {
            s.last_input = Instant::now();
        }
    }

    /// Returns whether this key changed the **luvus UI** (so the server should
    /// render). Plain input forwarded to a pane returns `false`: the pane's echo
    /// arrives as a separate `PtyData` event and renders then, so we don't burn a
    /// full render on the keystroke itself.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false; // ignored — nothing changed
        }
        if self.bar.overflow.take().is_some() {
            return true;
        }
        // Scroll mode belongs to one pane, not to the whole tab. A focus change
        // must never let the next key snap and type into the previously scrolled
        // pane. Pointer focus clears this eagerly below; this guard also covers
        // focus changes made by tabs, workspaces, the switcher, or API actions.
        if self
            .scroll_pane
            .is_some_and(|scrolling| scrolling != self.layout().focus)
        {
            self.scroll_pane = None;
        }
        // Copy mode belongs to its pane just like scroll mode. A focus change
        // cancels it rather than risking keys or a clipboard operation targeting
        // a different pane.
        if self
            .copy_mode
            .is_some_and(|copy| copy.pane != self.layout().focus)
        {
            self.cancel_copy_mode();
        }
        // The running-command overlay: scroll it, refresh it, or dismiss.
        if self.cmd_inspect.is_some() {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(c) = self.cmd_inspect.as_mut() {
                        c.scroll += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(c) = self.cmd_inspect.as_mut() {
                        c.scroll = c.scroll.saturating_sub(1);
                    }
                }
                KeyCode::Char('r') => self.refresh_cmd_inspect(),
                _ => self.close_cmd_inspect(),
            }
            return true;
        }
        // The help cheat-sheet overlay swallows the next key press and closes.
        if self.help_open {
            self.help_open = false;
            return true;
        }
        // The changelog modal captures keys: scroll with the arrows / j/k / page
        // keys, dismiss with esc / q / Enter.
        if self.changelog_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.changelog_open = false,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.changelog_scroll = self.changelog_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.changelog_scroll = self.changelog_scroll.saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.changelog_scroll = self.changelog_scroll.saturating_add(10)
                }
                KeyCode::PageUp => self.changelog_scroll = self.changelog_scroll.saturating_sub(10),
                KeyCode::Home | KeyCode::Char('g') => self.changelog_scroll = 0,
                _ => {}
            }
            return true;
        }
        // A module-setting prompt sits *inside* the Settings modal, so it must
        // take keys first (docs/13 §3.6).
        if self.module_setting_edit.is_some() {
            self.handle_module_setting_key(key);
            return true;
        }
        // The Settings modal captures all input while open.
        if self.settings.is_some() {
            self.handle_settings_key(key);
            return true;
        }
        // The global-search overlay (docs/63) captures all input while open.
        if self.search.is_some() {
            self.search_key(key);
            return true;
        }
        // The folder picker captures all input while open.
        if self.picker.is_some() {
            self.handle_picker_key(key);
            return true;
        }
        // The new-worktree branch prompt captures all input while open.
        if self.worktree_prompt.is_some() {
            self.handle_worktree_prompt_key(key);
            return true;
        }
        // The tab-rename modal (docs/28) captures all input while open.
        if self.tab_rename.is_some() {
            self.handle_tab_rename_key(key);
            return true;
        }
        // The tab context menu captures input while open.
        if self.tab_menu.is_some() {
            self.handle_tab_menu_key(key);
            return true;
        }
        // The workspace context menu / rename modal capture all input while open.
        if self.ws_menu.is_some() {
            self.handle_ws_menu_key(key);
            return true;
        }
        // The pane context menu (docs/28) captures all input while open.
        if self.pane_menu.is_some() {
            self.handle_pane_menu_key(key);
            return true;
        }
        // The AGENTS-list context menu (docs/28) captures all input while open.
        if self.agent_menu.is_some() {
            self.handle_agent_menu_key(key);
            return true;
        }
        // FILES-dock menu / prompt / delete-confirm capture input while open (docs/38).
        if self.file_prompt.is_some() {
            self.file_prompt_key(key);
            return true;
        }
        if self.file_delete.is_some() {
            self.file_delete_key(key);
            return true;
        }
        if self.worktree_delete.is_some() {
            self.worktree_delete_key(key);
            return true;
        }
        if self.file_menu.is_some() {
            if key.code == KeyCode::Esc {
                self.file_menu = None;
            }
            return true;
        }
        if self.diff_menu.is_some() {
            if key.code == KeyCode::Esc {
                self.diff_menu = None;
            }
            return true;
        }
        if self.dock_menu.is_some() {
            if key.code == KeyCode::Esc {
                self.dock_menu = None;
            }
            return true;
        }
        // The touch switcher overlay (docs/18) owns input while open.
        if self.switcher {
            self.switcher_key(key);
            return true;
        }
        if self.pane_rename.is_some() {
            self.handle_pane_rename_key(key);
            return true;
        }
        if self.ws_rename.is_some() {
            self.handle_ws_rename_key(key);
            return true;
        }
        // The board's new-task form captures all input while open (ORCH-7).
        if self.orch_form.is_some() {
            self.handle_orch_form_key(key);
            return true;
        }
        // Likewise the board's start-worker picker and task detail overlay.
        if self.orch_start.is_some() {
            self.handle_orch_start_key(key);
            return true;
        }
        if self.orch_detail.is_some() {
            self.handle_orch_detail_key(key);
            return true;
        }
        // Keyboard scroll mode owns every key until it's left (`q`/`Esc`/typing);
        // no `Ctrl+Space` prefix involved — the Mac-friendly path.
        if self.scroll_pane.is_some() {
            return self.handle_scroll_mode_key(key);
        }
        // Keyboard copy mode is deliberately ahead of prefix handling and all
        // pane input: its navigation must never reach the selected program.
        if self.copy_mode.is_some() {
            return self.handle_copy_mode_key(key);
        }
        // Keyboard resize mode (docs/27, RESIZE-3) likewise owns every key until
        // it's left (arrows/`hjkl` resize; `Esc`/`Enter`/`q` exit).
        if self.mode == Mode::Resize {
            return self.handle_resize_mode_key(key);
        }
        // A focused dashboard tab (git / orch / mission) captures normal-mode keys
        // (its own j/k/⏎/…); the `Ctrl+Space` prefix still works for global ops.
        if self.mode == Mode::Normal
            && (self.active_is_git() || self.active_is_orch() || self.active_is_mission())
        {
            if self.prefix.matches(&key) {
                self.mode = Mode::Prefix;
            } else if self.active_is_orch() {
                self.handle_orch_key(key);
            } else if self.active_is_mission() {
                self.handle_mission_key(key);
            } else {
                self.handle_git_key(key);
            }
            return true;
        }
        match self.mode {
            Mode::Prefix => {
                self.mode = Mode::Normal;
                // Pressing the prefix twice sends that exact key to the pane.
                // Use the normal PTY encoder so F-keys and modifiers retain the
                // same cross-platform escape sequence as ordinary input.
                if self.prefix.matches(&key) {
                    let prefix = self.prefix.key_event();
                    let newline = self.config.shift_enter_bytes().to_vec();
                    let app_cursor = self.focused().is_some_and(|p| p.application_cursor());
                    if let (Some(p), Some(bytes)) =
                        (self.focused(), encode_key(&prefix, &newline, app_cursor))
                    {
                        p.send(&bytes);
                    }
                    return true; // left prefix mode → the status bar updates
                }
                // Fixed convenience keys (not rebindable): `1`–`9` jump to a tab,
                // `?` opens the shortcut cheat-sheet.
                if let KeyCode::Char(c) = key.code {
                    if c.is_ascii_digit() && c != '0' {
                        self.switch_tab(c as usize - '1' as usize);
                        return true;
                    }
                    if c == '?' {
                        self.help_open = true;
                        return true;
                    }
                }
                // Fixed scrollback keys (like the digits above): scroll the
                // focused pane's history. `[`/`]` page up/down (no Fn needed on a
                // Mac), and so do PageUp/PageDown; Home/End jump to the top / live
                // bottom (Fn+↑/↓/←/→ on a MacBook).
                let scroll_code = match key.code {
                    KeyCode::Char('[') => Some(KeyCode::PageUp),
                    KeyCode::Char(']') => Some(KeyCode::PageDown),
                    c @ (KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End) => {
                        Some(c)
                    }
                    _ => None,
                };
                if let Some(code) = scroll_code {
                    self.scroll_focused_pane(code);
                    return true;
                }
                // Everything else resolves through the keybinding registry
                // (defaults + user overrides; see `app/keys.rs`). `key_string`
                // ignores modifiers, so the command key works whether you
                // released Ctrl after the prefix (`Ctrl+Space` then `c`) or kept
                // it held as a fast chord (`Ctrl+Space`+`Ctrl+c`).
                if let Some(cmd) = keys::key_string(&key).and_then(|s| self.keymap.get(&s).copied())
                {
                    self.run_cmd(cmd);
                }
                true // a prefix command (and leaving prefix mode) changes the UI
            }
            Mode::Normal => {
                if self.prefix.matches(&key) {
                    self.mode = Mode::Prefix;
                    return true; // entered prefix mode → the status bar updates
                }
                // A focused file view (docs/38 FILE-3) consumes keys itself
                // (scroll / wrap / close) — they never reach a PTY.
                let focus = self.layout().focus;
                match self.views.get(&focus) {
                    Some(crate::app::ViewKind::File(_)) => return self.handle_file_key(focus, key),
                    Some(crate::app::ViewKind::Diff(_)) => return self.handle_diff_key(focus, key),
                    None => {}
                }
                // `Shift+↑` / `Shift+PageUp` enter keyboard scroll mode (no prefix,
                // works on a stock Mac keyboard). From there plain keys navigate.
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                if shift && matches!(key.code, KeyCode::Up | KeyCode::PageUp) {
                    let by = if key.code == KeyCode::Up {
                        3
                    } else {
                        self.focused_page()
                    };
                    if self.enter_scroll_mode(by) {
                        return true;
                    }
                }
                // Plain page keys are convenient host-scroll shortcuts for a
                // normal transcript. Leave them untouched for full-screen TUIs,
                // mouse-reporting apps, and primary-screen pagers such as
                // `less -X`, which advertise application cursor mode.
                if key.modifiers.is_empty()
                    && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
                    && self.focused().is_some_and(|pane| pane.host_page_keys())
                {
                    self.scroll_focused_pane(key.code);
                    return true;
                }
                let newline = self.config.shift_enter_bytes();
                // Cursor keys follow the pane's DECCKM state: a `less` that
                // turned application cursor mode on only recognizes SS3 codes.
                let app_cursor = self.focused().is_some_and(|p| p.application_cursor());
                if let Some(bytes) = encode_key(&key, newline, app_cursor) {
                    if let Some(p) = self.focused() {
                        // Typing snaps the view back to the live bottom, so you
                        // always see what you type (like every terminal).
                        p.scroll_to_bottom();
                        p.send(&bytes);
                    }
                    self.mark_user_input(); // detection: this is typing, not work
                }
                false // plain input → the pane; its echo (PtyData) renders it
            }
            // Intercepted above (before this match); handled here too for safety.
            Mode::Resize => self.handle_resize_mode_key(key),
        }
    }
}

fn log_worker_failed(worker: crate::logging::Worker, error_code: &'static str) {
    let Some(error_code) = crate::logging::SafeId::new(error_code) else {
        return;
    };
    crate::logging::event(
        crate::logging::EventKind::WorkerFailed,
        &[
            crate::logging::Field::Worker(worker),
            crate::logging::Field::ErrorCode(error_code),
        ],
    );
}

/// Encode one mouse-wheel notch as the bytes a mouse-tracking app expects.
/// `up` selects the wheel-up/down button; `col`/`row` are 1-based, pane-local.
/// The phase of a forwarded button event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MouseSeq {
    Press,
    Drag,
    Release,
}

/// The xterm modifier bits ORed into a mouse button code: Shift +4, Alt +8,
/// Ctrl +16.
fn mouse_mod_bits(mods: ratatui::crossterm::event::KeyModifiers) -> u16 {
    use ratatui::crossterm::event::KeyModifiers;
    let mut bits = 0;
    if mods.contains(KeyModifiers::SHIFT) {
        bits += 4;
    }
    if mods.contains(KeyModifiers::ALT) {
        bits += 8;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        bits += 16;
    }
    bits
}

/// Encode a mouse button event (`btn`: 0 left · 1 middle · 2 right, plus any
/// [`mouse_mod_bits`]) at 1-based `col`/`row` as the terminal escape a
/// mouse-tracking app expects. SGR (1006): `ESC [< code;col;row M` for
/// press/drag (`m` for release), with +32 on the code while moving. Legacy
/// X10: `ESC [M` + three offset bytes, release encoded as button 3 (modifier
/// bits kept).
fn mouse_button_seq(btn: u16, kind: MouseSeq, col: u16, row: u16, sgr: bool) -> Vec<u8> {
    let motion = if kind == MouseSeq::Drag { 32 } else { 0 };
    if sgr {
        let end = if kind == MouseSeq::Release { 'm' } else { 'M' };
        format!("\x1b[<{};{col};{row}{end}", btn + motion).into_bytes()
    } else {
        let code = if kind == MouseSeq::Release {
            3 | (btn & !3)
        } else {
            btn + motion
        };
        let enc = |v: u16| (32 + v.min(223)) as u8;
        vec![0x1b, b'[', b'M', enc(code), enc(col), enc(row)]
    }
}

/// Encode an any-motion (DECSET 1003) hover report for a pane. The caller owns
/// hit-testing and delivery; keeping the encoding pure pins the exact bytes in
/// a unit test while the dirty-result path remains independent of forwarding.
fn hover_motion_seq(
    m: &ratatui::crossterm::event::MouseEvent,
    content: Rect,
    mode: crate::terminal::pty::MouseModes,
) -> Option<Vec<u8>> {
    if !mode.motion {
        return None;
    }
    let col = m.column.saturating_sub(content.x) + 1;
    let row = m.row.saturating_sub(content.y) + 1;
    Some(mouse_button_seq(
        3 + mouse_mod_bits(m.modifiers),
        MouseSeq::Drag,
        col,
        row,
        mode.sgr,
    ))
}

fn mouse_wheel_seq(up: bool, col: u16, row: u16, sgr: bool) -> Vec<u8> {
    let btn: u16 = if up { 64 } else { 65 };
    if sgr {
        // SGR (1006): ESC [ < btn ; col ; row M  (M = press; wheel has no release).
        format!("\x1b[<{btn};{col};{row}M").into_bytes()
    } else {
        // Legacy X10: ESC [ M then (32+btn) (32+col) (32+row), each byte capped.
        let enc = |v: u16| (32 + v.min(223)) as u8;
        vec![0x1b, b'[', b'M', enc(btn), enc(col), enc(row)]
    }
}

/// Encode a crossterm key event into the bytes a terminal program expects.
/// `newline` is the configured Shift/Alt+Enter sequence (`config::shift_enter`),
/// forwarded verbatim for "new line, don't submit" so it can be tuned per setup.
/// `app_cursor` mirrors the pane's DECCKM state: cursor keys go out as SS3
/// (`ESC O <letter>`) when the app enabled application cursor mode, exactly as a
/// real terminal would send them — some apps (`less`) only recognize the SS3
/// form once they've turned the mode on.
fn encode_key(key: &KeyEvent, newline: &[u8], app_cursor: bool) -> Option<Vec<u8>> {
    // AltGr arrives as Ctrl+Alt on Windows (`keys::is_ctrl_chord`) and types a
    // character — it is neither a Ctrl chord nor an `ESC`-prefixed Alt key.
    let ctrl = super::keys::is_ctrl_chord(key.modifiers);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // True exactly when `is_ctrl_chord` refused a Ctrl+Alt press as AltGr. Only
    // the `Char` arm may act on it: every other key keeps both modifiers, so
    // `Ctrl+Alt+Enter` is still a modified Enter and sends the newline sequence.
    let altgr = alt && !ctrl && key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let bytes: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = match c.to_ascii_lowercase() {
                    'a'..='z' => (c.to_ascii_uppercase() as u8) & 0x1f,
                    ' ' | '@' => 0,
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '^' => 0x1e,
                    '_' => 0x1f,
                    _ => return None,
                };
                if alt {
                    vec![0x1b, b]
                } else {
                    vec![b]
                }
            } else {
                let mut s = c.to_string().into_bytes();
                if alt && !altgr {
                    let mut v = vec![0x1b];
                    v.append(&mut s);
                    v
                } else {
                    s
                }
            }
        }
        // Shift/Alt+Enter means "new line, don't submit" in every agent CLI.
        // A terminal sends a bare `CR` for both Enter and Shift+Enter, so this
        // only ever fires when the terminal disambiguates modified keys — either
        // via the keyboard protocol (`main::push_key_protocol`, macOS/Linux) or
        // native console records (Windows). The bytes are configurable
        // (`config::shift_enter`); the default `ESC CR` is what agents expect out
        // of the box (Claude Code's `/terminal-setup`).
        KeyCode::Enter if shift || alt => newline.to_vec(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        // Keep navigation modifiers intact. Crossterm reports these directly
        // from Windows console records, while terminals on Unix report them via
        // xterm/Kitty escape sequences. Dropping the modifiers here turned
        // Alt+arrow and Ctrl+arrow into plain arrows in nested prompt editors.
        KeyCode::Left => cursor_key(b'D', key.modifiers, app_cursor),
        KeyCode::Right => cursor_key(b'C', key.modifiers, app_cursor),
        KeyCode::Up => cursor_key(b'A', key.modifiers, app_cursor),
        KeyCode::Down => cursor_key(b'B', key.modifiers, app_cursor),
        KeyCode::Home => cursor_key(b'H', key.modifiers, app_cursor),
        KeyCode::End => cursor_key(b'F', key.modifiers, app_cursor),
        KeyCode::Delete => csi_tilde_key(3, key.modifiers),
        KeyCode::Insert => csi_tilde_key(2, key.modifiers),
        KeyCode::PageUp => csi_tilde_key(5, key.modifiers),
        KeyCode::PageDown => csi_tilde_key(6, key.modifiers),
        KeyCode::F(n) => {
            let code = match n {
                1..=4 => n + 10, // 11..14
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                13 => 25,
                14 => 26,
                15 => 28,
                16 => 29,
                17 => 31,
                18 => 32,
                19 => 33,
                20 => 34,
                _ => return None,
            };
            csi_tilde_key(code, key.modifiers)
        }
        _ => return None,
    };
    Some(bytes)
}

fn csi(final_byte: u8) -> Vec<u8> {
    vec![0x1b, b'[', final_byte]
}

/// Encode a cursor key (arrows / Home / End). In application cursor mode
/// (DECCKM, `ESC[?1h`) an unmodified key is SS3 (`ESC O <letter>`) — the exact
/// bytes a real terminal sends after the app turned the mode on. Modified keys
/// keep the CSI `1;<mod>` form, since SS3 carries no modifier parameter.
fn cursor_key(final_byte: u8, modifiers: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    if app_cursor && key_modifier_param(modifiers) == 1 {
        vec![0x1b, b'O', final_byte]
    } else {
        csi_key(final_byte, modifiers)
    }
}

/// Xterm's modifier parameter: 1 + Shift + 2*Alt + 4*Ctrl + 8*Super +
/// 16*Hyper + 32*Meta. The extended bits match the Kitty keyboard protocol
/// values parsed by crossterm, so Command/Super survives when a terminal reports
/// it instead of translating the shortcut itself.
fn key_modifier_param(modifiers: KeyModifiers) -> u8 {
    1 + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
        + 8 * u8::from(modifiers.contains(KeyModifiers::SUPER))
        + 16 * u8::from(modifiers.contains(KeyModifiers::HYPER))
        + 32 * u8::from(modifiers.contains(KeyModifiers::META))
}

fn csi_key(final_byte: u8, modifiers: KeyModifiers) -> Vec<u8> {
    let modifier = key_modifier_param(modifiers);
    if modifier == 1 {
        csi(final_byte)
    } else {
        format!("\x1b[1;{modifier}{}", final_byte as char).into_bytes()
    }
}

fn csi_tilde_key(code: u8, modifiers: KeyModifiers) -> Vec<u8> {
    let modifier = key_modifier_param(modifiers);
    if modifier == 1 {
        format!("\x1b[{code}~").into_bytes()
    } else {
        format!("\x1b[{code};{modifier}~").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_modal_suppresses_hover_from_covered_bar_geometry() {
        let _env = crate::persist::test_env("modal-bar-hover");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.worktree_prompt = Some(String::new());
        app.bar.overflow = Some(crate::bar::OverflowPopup {
            region: crate::bar::BarRegion::BottomRight,
            keys: vec![crate::bar::CORE_RUNTIME.to_string()],
            rect: Rect::new(60, 20, 10, 3),
        });

        assert!(app.rendered_hover_rect(Some((61, 21))).is_none());
    }

    /// A resize event forces the next frame to be a full repaint, so a terminal
    /// damaged by a window move/resize/expose heals instead of keeping stale cells
    /// (the reported glitch). The render loop consumes `force_redraw`.
    #[test]
    fn resize_forces_a_full_repaint() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        assert!(!app.force_redraw, "starts off");
        let dirty = app.handle_event(AppEvent::Resize);
        assert!(dirty, "a resize warrants a redraw");
        assert!(
            app.force_redraw,
            "resize requests a full repaint, not just a diff"
        );
    }

    // A server that has closed its last node keeps running (docs/43 §3.3), so
    // control-API requests routed through the event channel must still be
    // answered — otherwise the reply channel drops and the CLI reads EOF.
    #[test]
    fn api_requests_are_answered_with_no_workspace_open() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.close_workspace(0);
        assert!(app.workspaces.is_empty(), "the only node is gone");
        let (reply, rx) = std::sync::mpsc::channel();
        let req = crate::ipc::api::ApiRequest {
            id: "ping".into(),
            method: "ping".into(),
            params: serde_json::Value::Null,
            reply,
        };
        let dirty = app.handle_event(AppEvent::Api(req));
        assert!(dirty, "an answered control request counts as activity");
        let resp = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("an empty server still answers its control API");
        assert!(resp.contains("pong"), "got a real pong, not EOF: {resp}");
    }

    #[test]
    fn closing_last_workspace_fails_parked_files_and_diff_requests() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        let root = app.ws().cwd.clone();

        let (files_reply, files_rx) = std::sync::mpsc::channel();
        app.pending_file_tree_api.push((
            root.clone(),
            crate::ipc::api::ApiRequest {
                id: "files".into(),
                method: "files.tree".into(),
                params: serde_json::Value::Null,
                reply: files_reply,
            },
        ));
        let (diff_reply, diff_rx) = std::sync::mpsc::channel();
        app.pending_diff_api.push((
            root,
            crate::ipc::api::ApiRequest {
                id: "diff".into(),
                method: "diff.list".into(),
                params: serde_json::Value::Null,
                reply: diff_reply,
            },
        ));

        app.close_workspace(0);

        let files = files_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("FILES waiter failed when its workspace closed");
        assert!(files.contains("files_error"), "unexpected reply: {files}");
        assert!(
            files.contains("no active workspace"),
            "unexpected reply: {files}"
        );
        let diff = diff_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("DIFF waiter failed when its workspace closed");
        assert!(diff.contains("diff_error"), "unexpected reply: {diff}");
        assert!(
            diff.contains("no active workspace"),
            "unexpected reply: {diff}"
        );
        assert!(app.pending_file_tree_api.is_empty());
        assert!(app.pending_diff_api.is_empty());
    }

    #[test]
    fn closing_one_workspace_fails_only_its_parked_files_and_diff_requests() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        let closed_root = app.ws().cwd.clone();
        let open_root = closed_root.join("still-open");
        app.workspaces.push(crate::app::Workspace {
            id: crate::ids::public_id("workspace"),
            name: "still-open".into(),
            cwd: open_root.clone(),
            branch: None,
            git_ahead_behind: None,
            worktree: None,
            tabs: vec![crate::app::Tab::panes(crate::layout::TileLayout::new(
                crate::ids::PaneId::alloc(),
            ))],
            active_tab: 0,
            pinned: false,
        });

        let request = |id: &str, method: &str| {
            let (reply, response) = std::sync::mpsc::channel();
            (
                crate::ipc::api::ApiRequest {
                    id: id.into(),
                    method: method.into(),
                    params: serde_json::Value::Null,
                    reply,
                },
                response,
            )
        };
        let (closed_files, closed_files_rx) = request("closed-files", "files.tree");
        let (open_files, open_files_rx) = request("open-files", "files.tree");
        app.pending_file_tree_api
            .push((closed_root.clone(), closed_files));
        app.pending_file_tree_api
            .push((open_root.clone(), open_files));
        let (closed_diff, closed_diff_rx) = request("closed-diff", "diff.list");
        let (open_diff, open_diff_rx) = request("open-diff", "diff.list");
        app.pending_diff_api
            .push((closed_root.clone(), closed_diff));
        app.pending_diff_api.push((open_root.clone(), open_diff));

        app.close_workspace(0);

        let closed_files = closed_files_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("closed workspace FILES request failed immediately");
        assert!(closed_files.contains("files_error"));
        assert!(closed_files.contains("workspace closed"));
        let closed_diff = closed_diff_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("closed workspace DIFF request failed immediately");
        assert!(closed_diff.contains("diff_error"));
        assert!(closed_diff.contains("workspace closed"));

        assert!(matches!(
            open_files_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(matches!(
            open_diff_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert_eq!(app.pending_file_tree_api.len(), 1);
        assert!(crate::platform::same_path(
            &app.pending_file_tree_api[0].0,
            &open_root
        ));
        assert_eq!(app.pending_diff_api.len(), 1);
        assert!(crate::platform::same_path(
            &app.pending_diff_api[0].0,
            &open_root
        ));
        assert_eq!(app.workspaces.len(), 1);
        assert!(crate::platform::same_path(&app.ws().cwd, &open_root));
    }

    // Agents treat Enter as "submit" and Shift+Enter as "new line". A terminal
    // sends a bare CR for both, so luvus asks for the disambiguating keyboard
    // protocol and forwards the modified form as `ESC CR` — the sequence agent
    // CLIs already understand.
    #[test]
    fn shift_enter_sends_a_newline_not_a_submit() {
        // The default newline sequence is `ESC CR`.
        let nl = b"\x1b\r";
        let enter = |m: KeyModifiers| encode_key(&KeyEvent::new(KeyCode::Enter, m), nl, false);
        assert_eq!(
            enter(KeyModifiers::NONE),
            Some(b"\r".to_vec()),
            "plain Enter still submits"
        );
        assert_eq!(
            enter(KeyModifiers::SHIFT),
            Some(b"\x1b\r".to_vec()),
            "Shift+Enter must be distinguishable from Enter"
        );
        assert_eq!(
            enter(KeyModifiers::ALT),
            Some(b"\x1b\r".to_vec()),
            "Alt/Option+Enter is the other common newline binding"
        );
        // Ctrl+Enter keeps the legacy submit byte — agents bind it to submit.
        assert_eq!(
            encode_key(
                &KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
                nl,
                false
            ),
            Some(b"\r".to_vec())
        );
    }

    /// AltGr must type its character, not a control byte. The Windows console
    /// reports AltGr as `Ctrl+Alt`, so the plain `contains(CONTROL)` test used
    /// to send `0x1c` for a backslash and a bare `ESC` for `[` — a Spanish or
    /// German keyboard could not type a Windows path into a pane at all.
    #[test]
    fn altgr_types_its_character_instead_of_a_control_byte() {
        let nl = b"\x1b\r";
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let enc =
            |c: char, m: KeyModifiers| encode_key(&KeyEvent::new(KeyCode::Char(c), m), nl, false);
        if cfg!(windows) {
            for c in ['\\', '@', '#', '[', ']', '{', '}', '|', '~', '€'] {
                assert_eq!(
                    enc(c, altgr),
                    Some(c.to_string().into_bytes()),
                    "AltGr+{c} must reach the pane as itself"
                );
            }
        } else {
            // Elsewhere AltGr arrives unmodified, so Ctrl+Alt stays the real
            // chord it is on current main: an Alt-prefixed control byte.
            assert_eq!(enc('\\', altgr), Some(vec![0x1b, 0x1c]));
        }
        // A real Ctrl chord is untouched on every platform.
        assert_eq!(enc('\\', KeyModifiers::CONTROL), Some(vec![0x1c]));
        assert_eq!(enc('c', KeyModifiers::CONTROL), Some(vec![0x03]));
        for c in ['ñ', 'á'] {
            assert_eq!(
                enc(c, KeyModifiers::NONE),
                Some(c.to_string().into_bytes()),
                "unmodified Unicode input must remain valid UTF-8"
            );
        }
        // The exception is for characters only: every other key keeps both
        // modifiers, so Ctrl+Alt+Enter is still a modified Enter.
        assert_eq!(
            encode_key(&KeyEvent::new(KeyCode::Enter, altgr), nl, false),
            Some(nl.to_vec()),
            "Ctrl+Alt+Enter still sends the configured newline"
        );
    }

    /// The Shift/Alt+Enter sequence is whatever `config::shift_enter` selects —
    /// so a setup whose agent wants a bare `LF` can get one.
    #[test]
    fn shift_enter_sequence_is_configurable() {
        let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(encode_key(&shift, b"\n", false), Some(b"\n".to_vec()));
        assert_eq!(
            encode_key(&shift, b"\x1b[13;2u", false),
            Some(b"\x1b[13;2u".to_vec())
        );
        // Plain Enter ignores the newline sequence and always submits.
        let plain = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(encode_key(&plain, b"\n", false), Some(b"\r".to_vec()));
    }

    #[test]
    fn character_keys_preserve_alt_with_control() {
        let key = |modifiers| {
            encode_key(
                &KeyEvent::new(KeyCode::Char('a'), modifiers),
                b"\x1b\r",
                false,
            )
        };
        assert_eq!(key(KeyModifiers::CONTROL), Some(vec![0x01]));
        assert_eq!(key(KeyModifiers::ALT), Some(b"\x1ba".to_vec()));
        assert_eq!(
            key(KeyModifiers::CONTROL | KeyModifiers::ALT),
            Some(vec![0x1b, 0x01])
        );
    }

    #[test]
    fn navigation_keys_preserve_modifiers_for_nested_prompt_editors() {
        let key = |code, modifiers| encode_key(&KeyEvent::new(code, modifiers), b"\x1b\r", false);

        // The existing unmodified sequences stay byte-for-byte compatible.
        assert_eq!(
            key(KeyCode::Left, KeyModifiers::NONE),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            key(KeyCode::Home, KeyModifiers::NONE),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            key(KeyCode::End, KeyModifiers::NONE),
            Some(b"\x1b[F".to_vec())
        );

        // Windows reports these modifiers on native arrow-key records. Xterm
        // parameters let ConPTY and nested TUIs reconstruct the original chord.
        assert_eq!(
            key(KeyCode::Left, KeyModifiers::ALT),
            Some(b"\x1b[1;3D".to_vec())
        );
        assert_eq!(
            key(KeyCode::Right, KeyModifiers::CONTROL),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            key(KeyCode::Home, KeyModifiers::SHIFT),
            Some(b"\x1b[1;2H".to_vec())
        );
        assert_eq!(
            key(KeyCode::End, KeyModifiers::ALT | KeyModifiers::CONTROL),
            Some(b"\x1b[1;7F".to_vec())
        );

        // Command/Super is available through terminals that speak the Kitty
        // keyboard protocol and must not silently degrade into a plain arrow.
        assert_eq!(
            key(KeyCode::Left, KeyModifiers::SUPER),
            Some(b"\x1b[1;9D".to_vec())
        );
    }

    #[test]
    fn application_cursor_mode_uses_ss3_cursor_keys() {
        // When the pane enabled DECCKM (`ESC[?1h`), unmodified cursor keys go out
        // as SS3 (`ESC O <letter>`) — the bytes a real terminal sends once the
        // app turned the mode on. `less` is strict about this and ignores CSI.
        let app = |code, modifiers| encode_key(&KeyEvent::new(code, modifiers), b"\x1b\r", true);
        assert_eq!(
            app(KeyCode::Up, KeyModifiers::NONE),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            app(KeyCode::Down, KeyModifiers::NONE),
            Some(b"\x1bOB".to_vec())
        );
        assert_eq!(
            app(KeyCode::Left, KeyModifiers::NONE),
            Some(b"\x1bOD".to_vec())
        );
        assert_eq!(
            app(KeyCode::Right, KeyModifiers::NONE),
            Some(b"\x1bOC".to_vec())
        );
        assert_eq!(
            app(KeyCode::Home, KeyModifiers::NONE),
            Some(b"\x1bOH".to_vec())
        );
        assert_eq!(
            app(KeyCode::End, KeyModifiers::NONE),
            Some(b"\x1bOF".to_vec())
        );
        // SS3 carries no modifier parameter, so a modified cursor key keeps the
        // CSI `1;<mod>` form even in application cursor mode.
        assert_eq!(
            app(KeyCode::Up, KeyModifiers::SHIFT),
            Some(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(
            app(KeyCode::Home, KeyModifiers::CONTROL),
            Some(b"\x1b[1;5H".to_vec())
        );
    }

    #[test]
    fn tilde_navigation_keys_preserve_modifiers() {
        let key = |code, modifiers| encode_key(&KeyEvent::new(code, modifiers), b"\x1b\r", false);
        assert_eq!(
            key(KeyCode::Delete, KeyModifiers::NONE),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            key(KeyCode::PageUp, KeyModifiers::CONTROL),
            Some(b"\x1b[5;5~".to_vec())
        );
        assert_eq!(
            key(KeyCode::Insert, KeyModifiers::SHIFT | KeyModifiers::ALT),
            Some(b"\x1b[2;4~".to_vec())
        );
    }

    #[test]
    fn function_keys_encode_to_tilde_codes() {
        let key =
            |n, modifiers| encode_key(&KeyEvent::new(KeyCode::F(n), modifiers), b"\x1b\r", false);
        // F1–F4 and F5–F12 carry the standard xterm CSI-tilde codes.
        assert_eq!(key(1, KeyModifiers::NONE), Some(b"\x1b[11~".to_vec()));
        assert_eq!(key(4, KeyModifiers::NONE), Some(b"\x1b[14~".to_vec()));
        assert_eq!(key(5, KeyModifiers::NONE), Some(b"\x1b[15~".to_vec()));
        assert_eq!(key(10, KeyModifiers::NONE), Some(b"\x1b[21~".to_vec()));
        assert_eq!(key(12, KeyModifiers::NONE), Some(b"\x1b[24~".to_vec()));
        // Extended function keys continue the same tilde-code numbering.
        assert_eq!(key(15, KeyModifiers::NONE), Some(b"\x1b[28~".to_vec()));
        assert_eq!(key(20, KeyModifiers::NONE), Some(b"\x1b[34~".to_vec()));
        // Modifiers ride in the `;mod` parameter like every other tilde key.
        assert_eq!(key(10, KeyModifiers::SHIFT), Some(b"\x1b[21;2~".to_vec()));
        assert_eq!(key(6, KeyModifiers::CONTROL), Some(b"\x1b[17;5~".to_vec()));
        // Keys past the recognized range are dropped rather than guessed at.
        assert_eq!(key(21, KeyModifiers::NONE), None);
    }

    #[test]
    fn sgr_wheel_encodes_button_and_coords() {
        // Each physical wheel event produces exactly one complete report.
        // Fullscreen TUIs may reject multiple reports coalesced into one read.
        // Wheel up = button 64, down = 65; coords are 1-based, pane-local.
        assert_eq!(mouse_wheel_seq(true, 5, 3, true), b"\x1b[<64;5;3M".to_vec());
        assert_eq!(
            mouse_wheel_seq(false, 12, 40, true),
            b"\x1b[<65;12;40M".to_vec()
        );
    }

    #[test]
    fn button_seq_encodes_press_drag_and_release() {
        // SGR press/drag/release: drag adds +32 to the code, release ends in `m`.
        assert_eq!(
            mouse_button_seq(0, MouseSeq::Press, 5, 3, true),
            b"\x1b[<0;5;3M".to_vec()
        );
        assert_eq!(
            mouse_button_seq(0, MouseSeq::Drag, 6, 3, true),
            b"\x1b[<32;6;3M".to_vec()
        );
        assert_eq!(
            mouse_button_seq(0, MouseSeq::Release, 6, 3, true),
            b"\x1b[<0;6;3m".to_vec()
        );
        // Middle button is code 1.
        assert_eq!(
            mouse_button_seq(1, MouseSeq::Press, 1, 1, true),
            b"\x1b[<1;1;1M".to_vec()
        );
        // Legacy X10: release is button 3; bytes are offset by 32 and capped.
        assert_eq!(
            mouse_button_seq(0, MouseSeq::Press, 1, 1, false),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        assert_eq!(
            mouse_button_seq(0, MouseSeq::Release, 1, 1, false),
            vec![0x1b, b'[', b'M', 35, 33, 33]
        );
        // Modifier bits ride on the code (Ctrl = +16) and survive an X10 release.
        assert_eq!(
            mouse_button_seq(16, MouseSeq::Press, 1, 1, true),
            b"\x1b[<16;1;1M".to_vec()
        );
        assert_eq!(
            mouse_button_seq(16, MouseSeq::Release, 1, 1, false),
            vec![0x1b, b'[', b'M', 32 + 19, 33, 33]
        );
        // Hover motion (1003): no button held = code 3, +32 while moving.
        assert_eq!(
            mouse_button_seq(3, MouseSeq::Drag, 2, 2, true),
            b"\x1b[<35;2;2M".to_vec()
        );
    }

    #[test]
    fn any_motion_hover_keeps_its_exact_forwarded_bytes() {
        use crate::terminal::pty::MouseModes;
        use ratatui::crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};

        let event = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 14,
            row: 9,
            modifiers: KeyModifiers::CONTROL,
        };
        let content = Rect::new(10, 5, 40, 20);
        let mode = MouseModes {
            report: true,
            drag: true,
            motion: true,
            sgr: true,
            alternate_scroll: false,
        };

        assert_eq!(
            hover_motion_seq(&event, content, mode),
            Some(b"\x1b[<51;5;5M".to_vec()),
            "1003 hover keeps code 3, Ctrl +16, motion +32, and pane-local coordinates"
        );
        assert_eq!(
            hover_motion_seq(
                &event,
                content,
                MouseModes {
                    motion: false,
                    ..mode
                }
            ),
            None,
            "1002 drag tracking alone does not receive buttonless hover"
        );
    }

    #[test]
    fn modifier_bits_follow_the_xterm_convention() {
        use ratatui::crossterm::event::KeyModifiers;
        assert_eq!(mouse_mod_bits(KeyModifiers::NONE), 0);
        assert_eq!(mouse_mod_bits(KeyModifiers::SHIFT), 4);
        assert_eq!(mouse_mod_bits(KeyModifiers::ALT), 8);
        assert_eq!(mouse_mod_bits(KeyModifiers::CONTROL), 16);
        assert_eq!(
            mouse_mod_bits(KeyModifiers::SHIFT | KeyModifiers::CONTROL),
            20
        );
    }

    #[test]
    fn legacy_wheel_encodes_offset_bytes_and_caps() {
        // X10: ESC [ M then 32+btn, 32+col, 32+row (each capped at 255).
        assert_eq!(
            mouse_wheel_seq(true, 1, 1, false),
            vec![0x1b, b'[', b'M', 32 + 64, 33, 33]
        );
        // Coordinates past 223 saturate so the byte never overflows.
        assert_eq!(
            mouse_wheel_seq(false, 500, 500, false),
            vec![0x1b, b'[', b'M', 32 + 65, 255, 255]
        );
    }
}

#[cfg(test)]
mod link_click_tests {
    use super::*;
    use crate::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;

    const URL: &str = "https://luvus.dev/docs";

    /// An app with one pane whose grid holds [`URL`], plus the screen cells that
    /// sit on it and on the prose beside it.
    struct Fixture {
        app: App,
        term: Terminal<TestBackend>,
        pane: crate::ids::PaneId,
        on_link: (u16, u16),
        off_link: (u16, u16),
    }

    fn fixture() -> Fixture {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let pane = app.layout().focus;
        app.panes
            .get(&pane)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(format!("\x1b[H\x1b[2Jsee {URL} ok\r\n").as_bytes());
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, r)| *r)
            .expect("pane content rect");
        Fixture {
            app,
            term,
            pane,
            // "see " is 4 cells, so the URL starts at grid column 4.
            on_link: (content.x + 6, content.y),
            off_link: (content.x + 1, content.y),
        }
    }

    fn mouse(kind: MouseEventKind, at: (u16, u16), mods: KeyModifiers) -> crate::event::AppEvent {
        crate::event::AppEvent::Mouse(MouseEvent {
            kind,
            column: at.0,
            row: at.1,
            modifiers: mods,
        })
    }

    /// The feature: `Ctrl`+click a URL in a pane and it goes to the client to be
    /// opened. Nothing is spawned here, only queued, which is what lets a remote
    /// attach open the browser in front of *you*.
    #[test]
    fn ctrl_click_on_a_link_queues_it_for_the_client() {
        let _env = crate::persist::test_env("link-click");
        let Fixture {
            mut app, on_link, ..
        } = fixture();
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            on_link,
            KeyModifiers::CONTROL,
        ));
        assert!(app.pending_open_url.is_none(), "not opened on press");
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            on_link,
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.pending_open_url.as_deref(), Some(URL));
    }

    /// The gesture it shares a modifier with must survive: `Ctrl`+*drag* is the
    /// RESIZE-5 divider grab, so moving off the press cell cancels the link.
    #[test]
    fn ctrl_drag_from_a_link_resizes_instead_of_opening() {
        let _env = crate::persist::test_env("link-drag");
        let Fixture {
            mut app, on_link, ..
        } = fixture();
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            on_link,
            KeyModifiers::CONTROL,
        ));
        app.handle_event(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            (on_link.0 + 6, on_link.1 + 3),
            KeyModifiers::CONTROL,
        ));
        // Asserted here, not after the release: the release takes `link_press`
        // either way, so checking it afterwards would pass even if the drag had
        // never handed the gesture over.
        assert!(
            app.link_press.is_none(),
            "moving off the press cell gives the gesture to the resize"
        );
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            (on_link.0 + 6, on_link.1 + 3),
            KeyModifiers::CONTROL,
        ));
        assert!(
            app.pending_open_url.is_none(),
            "a drag is a resize, never a link open"
        );
    }

    /// A plain click must stay a plain click: agent output is full of URLs, and
    /// reaching for a text selection cannot open a browser.
    #[test]
    fn a_click_without_ctrl_never_opens_anything() {
        let _env = crate::persist::test_env("link-plain");
        let Fixture {
            mut app, on_link, ..
        } = fixture();
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            on_link,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            on_link,
            KeyModifiers::NONE,
        ));
        assert!(app.pending_open_url.is_none());
    }

    /// `Ctrl`+click on ordinary text is still the divider grab, not a no-op that
    /// swallowed the gesture.
    #[test]
    fn ctrl_click_beside_a_link_opens_nothing() {
        let _env = crate::persist::test_env("link-miss");
        let Fixture {
            mut app, off_link, ..
        } = fixture();
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            off_link,
            KeyModifiers::CONTROL,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            off_link,
            KeyModifiers::CONTROL,
        ));
        assert!(app.pending_open_url.is_none());
        assert!(app.link_press.is_none());
    }

    /// Holding `Ctrl` lights the link up; letting go puts it out. Motion without
    /// the modifier must not scan at all, which is what keeps this off the hot
    /// path.
    #[test]
    fn ctrl_hover_underlines_the_link_and_plain_hover_clears_it() {
        let _env = crate::persist::test_env("link-hover");
        let Fixture {
            mut app,
            mut term,
            on_link,
            off_link,
            ..
        } = fixture();

        for i in 0..10_000 {
            let at = if i % 2 == 0 {
                off_link
            } else {
                (off_link.0 + 1, off_link.1 + 1)
            };
            assert!(
                !app.handle_event(mouse(MouseEventKind::Moved, at, KeyModifiers::NONE)),
                "ordinary pane motion {i} stays clean"
            );
        }
        assert!(app.hover_link.is_none(), "plain hover scans nothing");

        assert!(
            app.handle_event(mouse(MouseEventKind::Moved, on_link, KeyModifiers::CONTROL)),
            "entering a link changes its underline"
        );
        let hl = app.hover_link.as_ref().expect("ctrl hover found the link");
        assert_eq!(hl.target, LinkTarget::Url(URL.to_string()));
        assert!(
            !app.handle_event(mouse(MouseEventKind::Moved, on_link, KeyModifiers::CONTROL)),
            "resting inside the same link does not repaint"
        );

        // It is actually drawn underlined, not merely recorded.
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let cell = term.backend().buffer().cell(on_link).unwrap().clone();
        assert!(
            cell.modifier.contains(ratatui::style::Modifier::UNDERLINED),
            "the hovered link renders underlined"
        );

        // Moving off the link, still holding Ctrl, drops it.
        assert!(
            app.handle_event(mouse(
                MouseEventKind::Moved,
                off_link,
                KeyModifiers::CONTROL,
            )),
            "leaving a link removes its underline"
        );
        assert!(app.hover_link.is_none());
    }

    #[test]
    fn any_motion_forwarding_does_not_mark_luvus_dirty() {
        let _env = crate::persist::test_env("mouse-any-motion-dirty");
        let Fixture {
            mut app,
            pane,
            off_link,
            ..
        } = fixture();

        app.panes
            .get(&pane)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(b"\x1b[?1003h\x1b[?1006h");
        let mode = app.panes.get(&pane).unwrap().mouse_mode();
        assert!(mode.motion && mode.sgr, "the pane requested 1003 + 1006");

        assert!(
            !app.handle_event(mouse(MouseEventKind::Moved, off_link, KeyModifiers::NONE,)),
            "forwarding hover waits for the child's PtyData instead of rendering early"
        );
    }

    #[test]
    fn context_menu_hover_dirties_only_when_the_highlight_changes() {
        let _env = crate::persist::test_env("menu-hover-dirty");
        let Fixture {
            mut app,
            mut term,
            off_link,
            ..
        } = fixture();

        assert!(app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Right),
            off_link,
            KeyModifiers::NONE,
        )));
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let rows: Vec<Rect> = app
            .pane_menu
            .as_ref()
            .expect("pane menu opened")
            .items
            .iter()
            .map(|(_, rect)| *rect)
            .take(2)
            .collect();
        assert_eq!(rows.len(), 2, "the menu has multiple hoverable rows");
        let first = (rows[0].x + 1, rows[0].y);
        let second = (rows[1].x + 1, rows[1].y);

        assert!(
            app.handle_event(mouse(MouseEventKind::Moved, first, KeyModifiers::NONE)),
            "entering a menu row paints its highlight"
        );
        assert!(
            !app.handle_event(mouse(MouseEventKind::Moved, first, KeyModifiers::NONE)),
            "motion inside the same row leaves the frame unchanged"
        );
        assert!(
            app.handle_event(mouse(MouseEventKind::Moved, second, KeyModifiers::NONE)),
            "crossing rows moves the highlight"
        );
    }

    #[test]
    fn right_clicking_a_tab_opens_its_menu_instead_of_rename() {
        let _env = crate::persist::test_env("tab-right-click-menu");
        let Fixture { mut app, .. } = fixture();
        let tab = Rect::new(4, 0, 10, 1);
        app.tab_rects = vec![(0, tab)];

        assert!(app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Right),
            (tab.x + 1, tab.y),
            KeyModifiers::NONE,
        )));
        assert!(app.tab_menu.is_some());
        assert!(app.tab_rename.is_none());
    }

    #[test]
    fn tab_menu_renders_swap_with_submenu_for_other_tabs() {
        let _env = crate::persist::test_env("tab-switch-submenu");
        let Fixture {
            mut app, mut term, ..
        } = fixture();
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        term.draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let first_tab = app
            .tab_rects
            .iter()
            .find(|(index, _)| *index == 0)
            .map(|(_, rect)| *rect)
            .expect("first tab is visible");

        assert!(app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Right),
            (first_tab.x + 1, first_tab.y),
            KeyModifiers::NONE,
        )));
        term.draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let switch_row = app
            .tab_menu
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find(|(item, _)| *item == TabMenuItem::SwapWith)
            .map(|(_, rect)| *rect)
            .expect("Swap With row");

        assert!(app.handle_event(mouse(
            MouseEventKind::Moved,
            (switch_row.x + 1, switch_row.y),
            KeyModifiers::NONE,
        )));
        term.draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        assert_eq!(
            app.tab_menu.as_ref().unwrap().swap_rects.len(),
            1,
            "the other tab is available in the submenu"
        );
    }

    /// A fixture whose pane grid holds `text`, plus the screen cell sitting on
    /// the token that starts at `at` characters in.
    fn fixture_showing(text: &str, at: u16) -> (App, Terminal<TestBackend>, (u16, u16)) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let pane = app.layout().focus;
        app.panes
            .get(&pane)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(format!("\x1b[H\x1b[2J{text}\r\n").as_bytes());
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, r)| *r)
            .expect("pane content rect");
        (app, term, (content.x + at, content.y))
    }

    /// A path an agent printed opens **in luvus**, in a new tab, not at the OS.
    /// Tests run from the repo root, so `Cargo.toml` is a real relative path from
    /// the pane's working directory.
    #[test]
    fn ctrl_click_on_a_file_path_opens_it_in_a_tab() {
        let _env = crate::persist::test_env("link-file");
        let (mut app, _t, at) = fixture_showing("edit Cargo.toml now", 7);
        let tabs = app.ws().tabs.len();

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::CONTROL,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::CONTROL,
        ));

        assert!(
            app.pending_open_url.is_none(),
            "a file never goes to the browser"
        );
        assert_eq!(app.ws().tabs.len(), tabs + 1, "opened in a new tab");
        let id = app.layout().focus;
        match app.views.get(&id) {
            Some(crate::app::ViewKind::File(v)) => {
                assert!(v.path.ends_with("Cargo.toml"), "showing {:?}", v.path)
            }
            _ => panic!("the new tab holds a file view"),
        }
    }

    /// `src/main.rs:42` is one reference: the whole thing underlines, the path
    /// resolves without the suffix, and the viewer lands on that line.
    #[test]
    fn a_line_suffix_scrolls_the_viewer_to_that_line() {
        let _env = crate::persist::test_env("link-line");
        let (mut app, _t, at) = fixture_showing("at src/main.rs:42:7 boom", 5);

        let h = app.link_at_screen(at.0, at.1).expect("resolved");
        assert!(
            matches!(&h.target, LinkTarget::File { line: Some(42), .. }),
            "got {:?}",
            h.target
        );
        // The underline covers the whole reference including `:42:7`, not just the
        // path. `covers` is in *grid* coordinates: the token runs cols 3..=18.
        assert!(h.link.covers(3, 0), "underline starts at the path");
        assert!(h.link.covers(18, 0), "underline reaches the end of :42:7");
        assert!(!h.link.covers(19, 0), "and stops there");

        app.activate_link(h.target);
        let id = app.layout().focus;
        match app.views.get(&id) {
            Some(crate::app::ViewKind::File(v)) => assert_eq!(v.scroll, 41),
            _ => panic!("a file view opened"),
        }
    }

    /// The existence check is what makes paths safe to click: text that merely
    /// looks like a path must not light up, and a directory is not a file.
    #[test]
    fn only_paths_that_exist_as_files_resolve() {
        let _env = crate::persist::test_env("link-real");
        for (text, at) in [("see nope/missing.rs ok", 4), ("see src ok", 4)] {
            let (app, _t, cell) = fixture_showing(text, at);
            assert!(
                app.link_at_screen(cell.0, cell.1).is_none(),
                "{text:?} must not resolve"
            );
        }
    }

    /// A bare domain is as clickable as a written-out URL, and gets `https://`.
    #[test]
    fn ctrl_click_on_a_bare_domain_opens_it_over_https() {
        let _env = crate::persist::test_env("link-domain");
        let (mut app, _t, at) = fixture_showing("visit luvus.dev/docs now", 8);
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::CONTROL,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::CONTROL,
        ));
        assert_eq!(
            app.pending_open_url.as_deref(),
            Some("https://luvus.dev/docs")
        );
    }

    /// A dev server URL is the other half of what agents print, and it is `http`.
    #[test]
    fn a_localhost_port_opens_over_plain_http() {
        let _env = crate::persist::test_env("link-local");
        let (app, _t, at) = fixture_showing("Server running at localhost:3000", 20);
        assert_eq!(
            app.link_at_screen(at.0, at.1).map(|h| h.target),
            Some(LinkTarget::Url("http://localhost:3000".into()))
        );
    }

    /// The genuine ambiguity: `.rs` is Serbia and `.md` is Moldova, so a source
    /// filename is also a valid domain. A file that exists wins; the identical
    /// token resolves to a domain only when there is no such file.
    #[test]
    fn an_existing_file_beats_a_domain_of_the_same_name() {
        let _env = crate::persist::test_env("link-ambig");

        // `Cargo.toml` is really there (tests run from the repo root).
        let (app, _t, at) = fixture_showing("see Cargo.toml here", 6);
        match app.link_at_screen(at.0, at.1).map(|h| h.target) {
            Some(LinkTarget::File { path, .. }) => assert!(path.ends_with("Cargo.toml")),
            other => panic!("an existing file must win, got {other:?}"),
        }

        // Same shape, no such file, and `.dev` is a domain — so it is a link.
        let (app, _t, at) = fixture_showing("see luvus.dev here", 6);
        assert_eq!(
            app.link_at_screen(at.0, at.1).map(|h| h.target),
            Some(LinkTarget::Url("https://luvus.dev".into()))
        );

        // Same shape again, no such file and no known TLD — inert either way.
        let (app, _t, at) = fixture_showing("see absent.txt here", 6);
        assert_eq!(app.link_at_screen(at.0, at.1).map(|h| h.target), None);
    }

    /// The right-click row is the discoverable path, and it must carry the URL
    /// that was under the *click*, not whatever the grid says later.
    #[test]
    fn the_context_menu_offers_open_link_only_over_a_link() {
        let _env = crate::persist::test_env("link-menu");
        let Fixture {
            mut app,
            pane: id,
            on_link,
            off_link,
            ..
        } = fixture();

        app.open_pane_menu(id, off_link.0, off_link.1);
        assert!(
            !app.pane_menu_items().contains(&PaneMenuItem::OpenLink),
            "no row when the click missed a link"
        );

        app.open_pane_menu(id, on_link.0, on_link.1);
        assert_eq!(
            app.pane_menu.as_ref().unwrap().link,
            Some(LinkTarget::Url(URL.to_string())),
            "the URL is snapshotted at open"
        );
        assert!(app.pane_menu_items().contains(&PaneMenuItem::OpenLink));
        app.pane_menu_action(PaneMenuItem::OpenLink);
        assert_eq!(app.pending_open_url.as_deref(), Some(URL));
    }

    /// The double-click fallback: the whitespace-delimited word under a cell,
    /// with the single span it covers. Used when a cell isn't a path or URL.
    #[test]
    fn word_at_grid_takes_the_whitespace_word_under_the_cell() {
        let rows = vec!["  foo(bar) baz  ".to_string()];
        // Anywhere inside the token grabs the whole whitespace-delimited run,
        // punctuation included, and reports its exact span.
        for col in 2..=9 {
            assert_eq!(
                word_at_grid(&rows, col, 0),
                Some(("foo(bar)".to_string(), vec![(0, 2, 10)])),
                "col {col}"
            );
        }
        // A neighbouring word is its own token.
        assert_eq!(
            word_at_grid(&rows, 11, 0),
            Some(("baz".to_string(), vec![(0, 11, 14)]))
        );
        // Whitespace and out-of-range cells copy nothing.
        assert_eq!(word_at_grid(&rows, 1, 0), None, "leading blank");
        assert_eq!(word_at_grid(&rows, 10, 0), None, "gap between words");
        assert_eq!(word_at_grid(&rows, 99, 0), None, "past the line");
        assert_eq!(word_at_grid(&rows, 0, 5), None, "past the last row");
    }

    #[test]
    fn mouse_copy_drops_a_uniform_one_cell_pane_margin() {
        assert_eq!(
            strip_uniform_single_cell_margin(
                " Hello, rain on a windowpane\n Hello, wind with a traveling name\n Hello, all things we almost miss"
                    .into()
            ),
            "Hello, rain on a windowpane\nHello, wind with a traveling name\nHello, all things we almost miss"
        );
        assert_eq!(
            strip_uniform_single_cell_margin(
                "    let preserved = true;\n    run(preserved);".into()
            ),
            "    let preserved = true;\n    run(preserved);",
            "normal code indentation must stay intact"
        );
    }

    #[test]
    fn mouse_drag_copy_drops_the_pane_edge_margin_from_terminal_text() {
        let _env = crate::persist::test_env("mouse-copy-pane-margin");
        let source = " Morning arrives without ceremony,\r\n a thin gold line on the edge of the glass.\r\n The kettle speaks in its private language,";
        let (mut app, mut term, _) = fixture_showing(source, 0);
        // Hide the sidebars so the test deliberately starts at the pane's
        // leftmost content cell, matching the screenshot case.
        app.sidebars.left.visible = false;
        app.sidebars.right.visible = false;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let pane = app.layout().focus;
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("pane content rect");
        let start = (content.x, content.y);
        let end = (
            start.0
                + " The kettle speaks in its private language,"
                    .chars()
                    .count() as u16
                - 1,
            start.1 + 2,
        );

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            start,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            end,
            KeyModifiers::NONE,
        ));
        assert!(app.selection.is_some(), "the drag created a selection");
        assert_eq!(
            app.selection_text().as_deref(),
            Some(
                "Morning arrives without ceremony,\na thin gold line on the edge of the glass.\nThe kettle speaks in its private language,"
            )
        );
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            end,
            KeyModifiers::NONE,
        ));

        assert_eq!(
            app.pending_clipboard.as_deref(),
            Some(
                "Morning arrives without ceremony,\na thin gold line on the edge of the glass.\nThe kettle speaks in its private language,"
            )
        );
    }

    #[test]
    fn mouse_auto_copy_preserves_reverse_wide_character_selection() {
        let _env = crate::persist::test_env("mouse-copy-wide-unicode");
        let (mut app, _term, _) = fixture_showing("你好，hello.", 0);
        let pane = app.layout().focus;
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("pane content rect");
        let (visible_top, row_count) = app
            .panes
            .get(&pane)
            .and_then(|pane| pane.retained_viewport())
            .expect("retained viewport");
        let row = (visible_top..row_count)
            .find(|row| {
                app.panes
                    .get(&pane)
                    .and_then(|pane| pane.retained_row_text(*row))
                    .as_deref()
                    == Some("你好，hello.")
            })
            .expect("fixture row");
        let screen_row = content.y + (row - visible_top) as u16;
        let left = (content.x, screen_row);
        let right = (content.x + 3, screen_row);
        app.selection = Some(crate::app::Selection {
            pane,
            content,
            anchor: right,
            cursor: left,
            retained: Some(crate::app::RetainedSelection {
                anchor: (row, 3),
                cursor: (row, 0),
            }),
            scrolled: false,
            dragging: true,
        });

        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            left,
            KeyModifiers::NONE,
        ));

        assert_eq!(app.pending_clipboard.as_deref(), Some("你好"));
    }

    #[test]
    fn visible_selection_fallback_preserves_wide_characters() {
        let _env = crate::persist::test_env("mouse-copy-wide-visible-fallback");
        let (mut app, _term, _) = fixture_showing("你好，hello.", 0);
        let pane = app.layout().focus;
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("pane content rect");
        let visible_row = {
            let pane = app.panes.get(&pane).expect("pane");
            let engine = pane.engine.lock().expect("engine");
            engine
                .visible_rows()
                .iter()
                .position(|row| row.trim_end() == "你好，hello.")
                .expect("visible fixture row")
        };
        app.selection = Some(crate::app::Selection {
            pane,
            content,
            anchor: (content.x, content.y + visible_row as u16),
            cursor: (content.x + 3, content.y + visible_row as u16),
            retained: None,
            scrolled: false,
            dragging: false,
        });

        assert_eq!(app.selection_text().as_deref(), Some("你好"));
    }

    #[test]
    fn mouse_selection_anchor_survives_scrollback_while_dragging() {
        let _env = crate::persist::test_env("mouse-selection-scroll-anchor");
        let source = (0..100)
            .map(|line| format!("line-{line:03}\r\n"))
            .collect::<String>();
        let (mut app, _term, _) = fixture_showing(&source, 0);
        let pane = app.layout().focus;
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("pane content rect");
        let start = (
            content.x + "line-000".len() as u16 - 1,
            content.bottom().saturating_sub(4),
        );

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            start,
            KeyModifiers::NONE,
        ));
        let anchor = app
            .selection
            .and_then(|selection| selection.retained)
            .expect("terminal selection uses retained rows")
            .anchor;
        let anchor_text = app
            .panes
            .get(&pane)
            .and_then(|pane| pane.retained_row_text(anchor.0))
            .expect("anchored row")
            .trim_end()
            .to_string();
        assert!(!anchor_text.is_empty(), "fixture anchor contains text");

        // Extend beyond the original viewport. Each wheel event scrolls three
        // retained rows while the pointer remains part of the active gesture.
        for _ in 0..8 {
            app.handle_event(mouse(MouseEventKind::ScrollUp, start, KeyModifiers::NONE));
        }
        let end = (content.x, content.y + 1);
        app.handle_event(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            end,
            KeyModifiers::NONE,
        ));

        let retained = app
            .selection
            .and_then(|selection| selection.retained)
            .expect("retained selection after scrolling");
        assert_eq!(
            retained.anchor, anchor,
            "scrolling must never re-anchor the original press"
        );
        assert!(
            retained.cursor.0 < retained.anchor.0,
            "dragging after wheel scroll extends into older history"
        );
        let selected = app.selection_text().expect("selected history text");
        assert_eq!(
            selected.lines().last(),
            Some(anchor_text.as_str()),
            "the copied range still ends on the row where the drag began"
        );

        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            end,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.pending_clipboard.as_deref(), Some(selected.as_str()));
    }

    #[test]
    fn stationary_click_stays_empty_when_output_advances_history() {
        let _env = crate::persist::test_env("mouse-selection-output-drift");
        let source = (0..100)
            .map(|line| format!("line-{line:03}\r\n"))
            .collect::<String>();
        let (mut app, _term, _) = fixture_showing(&source, 0);
        let pane = app.layout().focus;
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("pane content rect");
        let at = (content.x + 2, content.bottom().saturating_sub(2));

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert!(app.selection.is_some(), "the press begins a selection");
        app.panes
            .get(&pane)
            .expect("pane")
            .engine
            .lock()
            .expect("terminal engine")
            .advance(b"incoming-output\r\n");
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));

        assert!(app.pending_clipboard.is_none());
        assert!(app.toast.is_none());
        assert!(app.selection.is_none(), "a plain click leaves no highlight");
    }

    #[test]
    fn wheel_alone_can_extend_an_active_selection() {
        let _env = crate::persist::test_env("mouse-selection-wheel-only");
        let source = (0..100)
            .map(|line| format!("line-{line:03}\r\n"))
            .collect::<String>();
        let (mut app, _term, _) = fixture_showing(&source, 0);
        let pane = app.layout().focus;
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("pane content rect");
        let at = (content.x + 2, content.bottom().saturating_sub(4));

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert!(app.selection.is_some(), "the press begins a selection");
        app.handle_event(mouse(MouseEventKind::ScrollUp, at, KeyModifiers::NONE));
        let selection = app.selection.expect("active selection");
        let retained = selection.retained.expect("retained endpoints");
        assert!(selection.scrolled);
        assert_ne!(retained.anchor, retained.cursor);
        let selected = app.selection_text().expect("wheel-extended text");

        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.pending_clipboard.as_deref(), Some(selected.as_str()));
    }

    #[test]
    fn wheel_returning_to_the_anchor_does_not_copy_one_cell() {
        let _env = crate::persist::test_env("mouse-selection-wheel-return");
        let source = (0..100)
            .map(|line| format!("line-{line:03}\r\n"))
            .collect::<String>();
        let (mut app, _term, _) = fixture_showing(&source, 0);
        let pane = app.layout().focus;
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("pane content rect");
        let at = (content.x + 2, content.bottom().saturating_sub(4));

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert!(app.selection.is_some(), "the press begins a selection");
        app.handle_event(mouse(MouseEventKind::ScrollUp, at, KeyModifiers::NONE));
        app.handle_event(mouse(MouseEventKind::ScrollDown, at, KeyModifiers::NONE));
        let retained = app
            .selection
            .and_then(|selection| selection.retained)
            .expect("retained endpoints");
        assert_eq!(retained.anchor, retained.cursor);
        assert!(app.selection_text().is_none());

        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert!(app.pending_clipboard.is_none());
        assert!(app.selection.is_none());
    }

    #[test]
    fn codex_copy_drops_its_one_cell_transcript_gutter() {
        let _env = crate::persist::test_env("codex-copy-gutter");
        let (mut app, _term, _) = fixture_showing("  hello\r\n  world", 0);
        let pane = app.layout().focus;
        app.status.get_mut(&pane).expect("pane status").agent = "codex".into();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("pane content rect");
        // The drag starts one cell in, so this verifies Codex detection rather
        // than the generic pane-edge case above.
        app.selection = Some(crate::app::Selection {
            pane,
            content,
            anchor: (content.x + 1, content.y),
            cursor: (content.x + 6, content.y + 1),
            retained: None,
            scrolled: false,
            dragging: false,
        });

        assert_eq!(app.selection_text().as_deref(), Some("hello\nworld"));
    }
}
