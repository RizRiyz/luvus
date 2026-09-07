//! Input handling for [`App`](super::App): key & mouse events, the prefix-key
//! command map, and crossterm→PTY key encoding.

use super::*;
use crate::files::view_text_w;
use unicode_width::UnicodeWidthChar;

/// Keep the command overlay useful when a just-spawned child has not appeared
/// in the platform process snapshot yet. The OS tree remains authoritative for
/// every process it reports; the pane's launch command only fills a missing
/// depth-zero root.
fn ensure_process_tree_root(
    pid: u32,
    command: &str,
    mut processes: Vec<crate::platform::ProcInfo>,
) -> Vec<crate::platform::ProcInfo> {
    if pid != 0
        && !command.trim().is_empty()
        && !processes
            .iter()
            .any(|process| process.pid == pid && process.depth == 0)
    {
        processes.insert(
            0,
            crate::platform::ProcInfo {
                pid,
                depth: 0,
                command: command.to_string(),
            },
        );
    }
    processes
}

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

/// Vim `E`: the last cell of the word under the cursor, or of the next word when
/// the cursor already sits on that cell.
///
/// Copy mode's whole word family is whitespace-delimited, with no `iskeyword`
/// notion: `w` is really vim's `W`, `B` is vim's `b`, and both `e` and `E` land
/// here. One rule for all three, so they always agree on where a word ends.
///
/// Row-local on purpose: a retained row is a physical terminal row, and `w`
/// already treats a row edge as a break, so the trailing scan never crosses one.
fn copy_word_end(
    row_count: usize,
    mut row_layout: impl FnMut(usize) -> Option<crate::terminal::vt::RetainedRowLayout>,
    at: (usize, usize),
) -> (usize, usize) {
    // Step off the current cell first, so pressing `e` again advances instead of
    // parking on the same word end.
    let mut cur = (at.0, at.1.saturating_add(1));
    while cur.0 < row_count {
        let Some(layout) = row_layout(cur.0) else {
            cur = (cur.0 + 1, 0);
            continue;
        };
        let last = layout.last_column();
        if cur.1 > last || layout.is_whitespace(cur.1) {
            if cur.1 >= last {
                cur = (cur.0 + 1, 0);
            } else {
                cur.1 += 1;
            }
            continue;
        }
        while cur.1 < last && !layout.is_whitespace(cur.1 + 1) {
            cur.1 += 1;
        }
        return cur;
    }
    let last = row_count.saturating_sub(1);
    (
        last,
        row_layout(last).map_or(0, |layout| layout.last_column()),
    )
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

/// Apply a copy-mode motion `count` times, stopping as soon as it stops moving.
/// A motion that has saturated at the edge of retained history must not keep
/// re-scanning the grid: every word step takes the engine lock the PTY reader
/// also needs, so `9999e` near the bottom has to cost one step, not 9999.
fn repeat_motion(
    count: usize,
    mut at: (usize, usize),
    mut step: impl FnMut((usize, usize)) -> (usize, usize),
) -> (usize, usize) {
    for _ in 0..count {
        let next = step(at);
        if next == at {
            break;
        }
        at = next;
    }
    at
}

fn finish_selected_text(mut out: String) -> Option<String> {
    let trimmed_len = out.trim_end_matches('\n').len();
    out.truncate(trimmed_len);
    (!out.trim().is_empty()).then_some(out)
}

/// A second left click within this of the first, on the same cell (±1), is a
/// double-click. Terminals emit no native double-click, so luvus times it.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const COPY_HIGHLIGHT_DURATION: Duration = Duration::from_millis(1400);

/// A run of grid cells on one row: `(row, start_col, end_col)`, `end_col`
/// exclusive — the same shape as [`crate::links::Link::spans`].
type CellSpan = (u16, u16, u16);

/// Convert rendered native-view rows to the same cell-aligned representation
/// used by terminal token lookup. This runs only on a double-click, never on a
/// frame, and keeps wide and zero-width characters from shifting the clicked
/// cell away from its word.
fn align_plain_rows(rows: Vec<String>) -> crate::terminal::vt::AlignedRows {
    let mut aligned = crate::terminal::vt::AlignedRows::new(rows.len());
    for (row, line) in rows.into_iter().enumerate() {
        let mut column = 0u16;
        let mut base: Option<(char, usize, Vec<char>)> = None;
        let flush = |base: &mut Option<(char, usize, Vec<char>)>,
                     aligned: &mut crate::terminal::vt::AlignedRows,
                     column: &mut u16| {
            let Some((character, width, zero_width)) = base.take() else {
                return;
            };
            aligned.push_cell(
                row as u16,
                *column,
                character,
                (!zero_width.is_empty()).then_some(zero_width.as_slice()),
            );
            for offset in 1..width {
                aligned.push_cell(
                    row as u16,
                    column.saturating_add(offset as u16),
                    crate::terminal::vt::ALIGNED_WIDE_CELL,
                    None,
                );
            }
            *column = column.saturating_add(width as u16);
        };

        for character in line.chars() {
            let width = character.width().unwrap_or(0);
            if width == 0 {
                if let Some((_, _, zero_width)) = base.as_mut() {
                    zero_width.push(character);
                }
                continue;
            }
            flush(&mut base, &mut aligned, &mut column);
            base = Some((character, width, Vec::new()));
        }
        flush(&mut base, &mut aligned, &mut column);
    }
    aligned
}

/// The whitespace-delimited word covering grid cell (`col`, `row`), and the one
/// span it occupies. `None` on a whitespace or out-of-range cell. Wide-cell
/// continuation markers participate in the word boundary and are removed from
/// the copied text, so either cell of `你` selects the same complete word. Char
/// indices are the columns, matching how [`crate::links::link_at`] and the grid
/// renderer address cells.
fn word_at_grid(
    rows: &crate::terminal::vt::AlignedRows,
    col: u16,
    row: u16,
) -> Option<(String, Vec<CellSpan>)> {
    let line = rows.rows().get(row as usize)?;
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
    let mut text = String::new();
    for (offset, character) in chars[lo..hi].iter().copied().enumerate() {
        if character == crate::terminal::vt::ALIGNED_WIDE_CELL {
            continue;
        }
        text.push(character);
        text.extend(rows.zero_width_at(row, (lo + offset) as u16));
    }
    if text.is_empty() {
        return None;
    }
    Some((text, vec![(row, lo as u16, hi as u16)]))
}

/// The URL or path covering a grid cell, reconstructed from its original
/// graphemes. Link parsing remains deliberately ASCII-only for opening
/// untrusted terminal output, but copying must also keep Unicode path segments.
/// A searchable projection maps Unicode letters/digits and wide-cell
/// continuations to ASCII while retaining one character per physical cell;
/// the returned spans are then used to recover the exact displayed text.
fn copy_link_at_grid(
    rows: &crate::terminal::vt::AlignedRows,
    col: u16,
    row: u16,
) -> Option<(String, Vec<CellSpan>)> {
    let searchable: Vec<String> = rows
        .rows()
        .iter()
        .map(|line| {
            line.chars()
                .map(|character| {
                    if character == crate::terminal::vt::ALIGNED_WIDE_CELL
                        || (!character.is_ascii() && character.is_alphanumeric())
                    {
                        'x'
                    } else {
                        character
                    }
                })
                .collect()
        })
        .collect();
    let link = crate::links::link_at(&searchable, col, row)?;

    let mut text = String::new();
    for (span_row, start, end) in &link.spans {
        let line: Vec<char> = rows.rows().get(*span_row as usize)?.chars().collect();
        for column in *start..*end {
            let character = *line.get(column as usize)?;
            if character == crate::terminal::vt::ALIGNED_WIDE_CELL {
                continue;
            }
            text.push(character);
            text.extend(rows.zero_width_at(*span_row, column));
        }
    }
    (!text.is_empty()).then_some((text, link.spans))
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
        if req.method == "task.merge" {
            return self.handle_task_merge_request(req);
        }
        let Some(req) = self.prepare_files_api(req) else {
            return true;
        };
        let Some(req) = self.prepare_diff_api(req) else {
            return true;
        };
        let response = self.handle_api(&req);
        self.reply_after_automation_save(req, response);
        true
    }

    /// Park `task.merge` until its off-loop Git job returns. This preserves the
    /// ordinary one-request/one-response API while keeping repository work away
    /// from the single app owner.
    fn handle_task_merge_request(&mut self, mut req: crate::ipc::api::ApiRequest) -> bool {
        if self.workspaces.is_empty() {
            let _ = req.reply.send(
                json!({"id":req.id,"error":{"code":"no_session","message":"no active session"}})
                    .to_string(),
            );
            return true;
        }
        let expected_revision = req
            .params
            .as_object_mut()
            .and_then(|params| params.remove("if_revision"));
        if let Some(expected) = expected_revision {
            let actual = crate::ipc::api::current_sequence(&self.events);
            if expected.as_u64() != Some(actual) {
                let _ = req.reply.send(
                    json!({"id":req.id,"error":{"code":"revision_conflict",
                        "message":"socket state changed before this mutation",
                        "expected":expected,"actual":actual}})
                    .to_string(),
                );
                return true;
            }
        }
        let Some(id) = req
            .params
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|id| !id.is_empty())
            .map(str::to_string)
        else {
            let _ = req.reply.send(
                json!({"id":req.id,"error":{"code":"invalid_request","message":"id is required"}})
                    .to_string(),
            );
            return true;
        };
        let error_reply = req.reply.clone();
        let request_id = req.id.clone();
        if let Err((code, message)) = self.start_task_merge(&id, Some((req.id, req.reply))) {
            let _ = error_reply
                .send(json!({"id":request_id,"error":{"code":code,"message":message}}).to_string());
        }
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
            AppEvent::IoCompleted(completion) => {
                return completion.apply(self);
            }
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
                self.runtime_cwd_dirty = true;
                self.runtime_proc_dirty = true;
                self.register_backend_terminal(id);
                self.reconcile_durable_active_targets(Some(id));
                if self.durable_target_requires_readiness_scan(id) {
                    self.request_proc_scan_if_stale(id);
                }
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
                if self.config_save_pending() {
                    self.defer_config_reload(id, reply);
                    return false;
                }
                let response = match self.apply_socket_config(*config, None) {
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
            AppEvent::NamedSessionsLoaded { generation, result } => {
                self.apply_named_sessions_loaded(generation, result);
                return true;
            }
            AppEvent::NamedSessionPrepared {
                generation,
                name,
                result,
            } => {
                self.apply_named_session_prepared(generation, name, result);
                return true;
            }
            AppEvent::NamedSessionStopped {
                generation,
                name,
                result,
            } => {
                self.apply_named_session_stopped(generation, name, result);
                return true;
            }
            other => other,
        };
        // Control-API requests and parked `wait.output` replies must be answered
        // even if restore or shell startup left no workspace. Normal close paths
        // immediately create a neutral home terminal; this guard keeps the
        // exceptional recovery path from dropping replies or indexing a layout.
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
                    self.reply_after_automation_save(req, resp);
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
                // Integration belongs to the server-owned task ledger, not a
                // workspace. Settle it even if replacement-pane startup left
                // the server temporarily without a workspace.
                AppEvent::TaskMergeFinished {
                    task,
                    branch,
                    previous,
                    integration_branch,
                    result,
                    reply,
                } => {
                    self.task_merge_finished(
                        task,
                        branch,
                        previous,
                        integration_branch,
                        result,
                        reply,
                    );
                    return true;
                }
                // A worker may finish after the previous workspace closes while
                // replacement shell startup fails. Drop its stale result, but
                // release the guard so CWD tracking can recover later.
                AppEvent::CwdScanned { .. } => {
                    self.cwd_scan_inflight = false;
                    return false;
                }
                // Late pane and worker events may arrive after the final project
                // closes. If replacement startup failed, ignore them rather than
                // indexing a layout that does not exist.
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
                // Otherwise it goes to the focused pane.
                self.paste_into_focused_pane(&s);
                false // goes to the pane; its echo (PtyData) renders it
            }
            AppEvent::Resize => {
                // A resize (or a same-size resize event a terminal emits on a
                // move/expose) may have damaged the screen — force a full repaint.
                self.force_redraw = true;
                true
            }
            AppEvent::PtyInputRejected(id) => {
                crate::logging::event(
                    crate::logging::EventKind::PtyInputRejected,
                    &[crate::logging::Field::PaneId(u64::from(id.0))],
                );
                if let Some(pane) = self.panes.get(&id) {
                    pane.acknowledge_input_rejection();
                    self.show_toast(format!(
                        "pane {} input queue full: input rejected; wait for the child to read",
                        id.0
                    ));
                }
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
                if self.panes.contains_key(&id) {
                    self.runtime_cwd_dirty_panes.insert(id);
                }
                self.runtime_proc_dirty = true;
                // A parked `wait.output` for this pane just got new output to
                // test against — resolve it on the same wake (docs/81).
                self.check_output_waits(id);
                self.backend_output_changed(id);
                true // the pane's screen advanced
            }
            AppEvent::PtyExit(id) => {
                self.runtime_cwd_dirty_panes.remove(&id);
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
                    Ok(Some(id)) => {
                        self.register_output_wait(
                            id, request_id, needle, reply, timeout, cancelled,
                        );
                    }
                    Ok(None) => {
                        let _ = reply.send(
                            json!({ "id": request_id, "error": {
                                "code": "not_found", "message": "pane not found"
                            }})
                            .to_string(),
                        );
                    }
                    Err((code, message)) => {
                        let _ = reply.send(
                            json!({ "id": request_id, "error": {
                                "code": code, "message": message
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
                states,
                reply,
                timeout,
                cancelled,
            } => {
                let params = json!({"pane":pane});
                let parsed_states: Option<Vec<_>> = (!states.is_empty())
                    .then(|| {
                        states
                            .iter()
                            .map(|state| crate::app::dispatch::parse_agent_wait_state(state))
                            .collect()
                    })
                    .flatten();
                match (self.resolve_pane(&params), parsed_states) {
                    (Ok(Some(id)), Some(states)) => {
                        self.register_agent_wait(id, request_id, states, reply, timeout, cancelled);
                    }
                    (Ok(None), _) => {
                        let _ = reply.send(
                            json!({"id":request_id,"error":{"code":"not_found","message":"pane not found"}})
                                .to_string(),
                        );
                    }
                    (Ok(Some(_)), None) => {
                        let _ = reply.send(
                            json!({"id":request_id,"error":{"code":"invalid_request","message":"statuses must be a non-empty set of idle, working, blocked, or done"}})
                                .to_string(),
                        );
                    }
                    (Err((code, message)), _) => {
                        let _ = reply.send(
                            json!({"id":request_id,"error":{"code":code,"message":message}})
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
            AppEvent::ProcScanned(found) => {
                let scan_succeeded = found.is_some();
                let changed = self.apply_proc_scan(found);
                if scan_succeeded {
                    changed | self.reconcile_durable_active_targets(None)
                } else {
                    changed
                }
            }
            AppEvent::CwdScanned {
                panes,
                branches,
                workspace_candidates,
            } => self.apply_cwd_scan(panes, branches, workspace_candidates),
            // Mission Control usage (docs/54, MC-2): replace a fleet scan or
            // merge only the keys covered by a workspace scan. Repaint so a
            // visible mission tab updates.
            AppEvent::UsageScanned {
                scope,
                scanned,
                mut usage,
                mut mtimes,
                report_owned,
            } => {
                self.usage_scan_inflight = false;
                self.prune_reported_usage();
                let excluded = report_owned
                    .into_iter()
                    .chain(self.reported_usage.keys().cloned())
                    .collect::<std::collections::HashSet<_>>();
                usage.retain(|key, _| !excluded.contains(key));
                mtimes.retain(|key, _| !excluded.contains(key));
                if scope == crate::mission::MissionScope::All {
                    let mut next = usage;
                    for key in self.reported_usage.keys() {
                        if let Some(value) = self.agent_usage.get(key) {
                            next.insert(key.clone(), value.clone());
                        }
                    }
                    self.agent_usage = next;
                    self.usage_mtimes = mtimes;
                } else {
                    for key in scanned {
                        if !self.reported_usage.contains_key(&key) {
                            self.agent_usage.remove(&key);
                        }
                        self.usage_mtimes.remove(&key);
                    }
                    self.agent_usage.extend(
                        usage
                            .into_iter()
                            .filter(|(key, _)| !self.reported_usage.contains_key(key)),
                    );
                    self.usage_mtimes.extend(mtimes);
                }
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
            AppEvent::FileChanges {
                id,
                path,
                token,
                changes,
            } => {
                match self.views.get_mut(&id) {
                    // Markers from a superseded read would tint the wrong lines
                    // — and they are the later half of their worker, so they are
                    // the likelier half to arrive stale.
                    Some(crate::app::ViewKind::File(v))
                        if v.read_token == token && v.path == path =>
                    {
                        v.changes = changes;
                        true
                    }
                    // The leaf closed, became a diff, or has asked for a newer
                    // read since: drop it.
                    _ => false,
                }
            }
            AppEvent::FileRead {
                id,
                path,
                token,
                load,
            } => {
                match self.views.get_mut(&id) {
                    // Only the newest read the leaf asked for may apply. One
                    // preview browsing A → B → A finishes those reads in any
                    // order, and the first A read landing last would quietly
                    // restore contents from before the file changed.
                    Some(crate::app::ViewKind::File(v))
                        if v.read_token == token && v.path == path =>
                    {
                        v.apply(load);
                        true
                    }
                    // The leaf closed, became a diff, or has asked for a newer
                    // read since: drop it.
                    _ => false,
                }
            }
            AppEvent::PreviewRead {
                id,
                path,
                kind,
                token,
                load,
            } => self.apply_preview_read(id, path, kind, token, load),
            AppEvent::PreviewLayout {
                id,
                path,
                kind,
                token,
                key,
                layout,
            } => self.apply_preview_layout(id, path, kind, token, key, layout),
            AppEvent::GitData { view, payload } => {
                self.git_data(view, payload);
                true
            }
            AppEvent::TaskGateFinished { task, code, out } => {
                self.task_gate_finished(&task, code, out);
                true
            }
            AppEvent::TaskMergeFinished {
                task,
                branch,
                previous,
                integration_branch,
                result,
                reply,
            } => {
                self.task_merge_finished(task, branch, previous, integration_branch, result, reply);
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
            | AppEvent::ClientInput { .. }
            | AppEvent::Shutdown => false,
            // Consumed by the pre-dispatch worker-result branch above.
            AppEvent::IoCompleted(_)
            | AppEvent::ThemeUninstalled { .. }
            | AppEvent::ConfigReloaded { .. }
            | AppEvent::ManifestsReloaded { .. }
            | AppEvent::BackendCreateReady { .. }
            | AppEvent::BackendObserve { .. }
            | AppEvent::PtyReady { .. }
            | AppEvent::SearchFilesIndexed { .. }
            | AppEvent::SearchResults { .. }
            | AppEvent::SearchFederatedResults { .. }
            | AppEvent::SearchHandoffReady { .. } => unreachable!(),
            AppEvent::NamedSessionsLoaded { .. }
            | AppEvent::NamedSessionPrepared { .. }
            | AppEvent::NamedSessionStopped { .. } => {
                unreachable!()
            }
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
    /// a modal consumed it. Control characters are dropped from single-line
    /// fields; the ORCH prompt preserves normalized line feeds.
    fn paste_into_modal(&mut self, s: &str) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        if self.named_session_menu.is_some() {
            return self.paste_named_session_prompt(s);
        }
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
        if self.orch_form.is_some() {
            let multiline = self
                .orch_form
                .as_ref()
                .is_some_and(|form| form.field == crate::app::OrchFormField::Prompt);
            for character in s.replace("\r\n", "\n").replace('\r', "\n").chars() {
                if character == '\n' && multiline {
                    if let Some(form) = self.orch_form.as_mut() {
                        form.push_char('\n');
                    }
                } else if !character.is_control() {
                    self.handle_orch_form_key(KeyEvent::new(
                        KeyCode::Char(character),
                        KeyModifiers::NONE,
                    ));
                }
            }
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

        if self.named_session_menu.is_some() {
            if self.session_menu.is_some() {
                if let Some(menu) = &self.session_menu {
                    if let Some(rect) = menu.items.iter().map(|(_, r)| *r).find(|r| hit(*r)) {
                        return Some(rect);
                    }
                }
                // Hover stays inside the Stop menu while it is open — do not
                // highlight the session list behind it when moving the mouse.
                return self.named_session_close_rect.filter(|rect| hit(*rect));
            }
            return self
                .named_session_close_rect
                .filter(|rect| hit(*rect))
                .or_else(|| {
                    self.named_session_row_rects
                        .iter()
                        .map(|(_, rect)| *rect)
                        .find(|rect| hit(*rect))
                });
        }

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
        if let Some(menu) = &self.orch_menu {
            return menu
                .items
                .iter()
                .map(|(_, rect)| *rect)
                .find(|rect| hit(*rect));
        }
        if self.orch_form.is_some() || self.orch_start.is_some() || self.orch_detail.is_some() {
            return self
                .orch_hits
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
                    self.named_session_button_rect,
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
        // A new primary-button gesture replaces any copied mouse selection,
        // even when a modal, menu, resize handle, or child TUI claims the press
        // below. This keeps the delayed highlight from surviving an unrelated
        // click through one of those early-return paths.
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.clear_selection();
        }
        // Track the cursor for hover affordances (e.g. the session delete ✕).
        self.hover = Some((m.column, m.row));
        if let MouseEventKind::Down(_) = m.kind {
            self.menu_scroll.press(m.column, m.row);
        }
        // The shortcut reference scrolls with the wheel; a click dismisses it.
        if self.help_open {
            match m.kind {
                MouseEventKind::ScrollUp => {
                    self.help_scroll = self.help_scroll.min(self.help_scroll_max).saturating_sub(2)
                }
                MouseEventKind::ScrollDown => {
                    self.help_scroll = self.help_scroll.saturating_add(2).min(self.help_scroll_max)
                }
                MouseEventKind::Down(MouseButton::Left) => self.help_open = false,
                _ => {}
            }
            return;
        }
        if self.named_session_menu.is_some() {
            // Context menu on a session row owns the click first.
            if self.session_menu.is_some() {
                match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        // If this press scrolls the session popup, keep the
                        // context menu under the cursor like other menus.
                        if self.menu_scroll.wheel(m.column, m.row, 0) {
                            // no-op: just update hover for menu_scroll internal state
                        }
                        self.session_menu_click(m.column, m.row);
                    }
                    MouseEventKind::Down(MouseButton::Right) => {
                        // Re-anchor: right-clicking another row moves the menu, like Agents.
                        if let Some(idx) = self
                            .named_session_row_rects
                            .iter()
                            .find(|(_, r)| {
                                m.column >= r.x
                                    && m.column < r.right()
                                    && m.row >= r.y
                                    && m.row < r.bottom()
                            })
                            .map(|(i, _)| *i)
                        {
                            if idx != 0 {
                                // Keep `menu` bound here so `menu.preparing` is in scope.
                                // The previous `.and_then(|menu| menu.rows.get(..))` moves
                                // `menu` into the closure, so a naive `&& !menu.preparing`
                                // at the row check would not compile.
                                if let Some(menu) = self.named_session_menu.as_ref() {
                                    if let Some(row) = menu.rows.get(idx - 1) {
                                        if row.running && !row.current && !menu.preparing {
                                            self.open_session_menu(
                                                row.name.clone(),
                                                m.column,
                                                m.row,
                                                row.running,
                                                row.current,
                                            );
                                        } else {
                                            self.session_menu = None;
                                        }
                                    } else {
                                        self.session_menu = None;
                                    }
                                } else {
                                    self.session_menu = None;
                                }
                            } else {
                                self.session_menu = None;
                            }
                        } else if !self.session_menu.as_ref().is_some_and(|menu| {
                            menu.items.iter().any(|(_, r)| {
                                m.column >= r.x
                                    && m.column < r.right()
                                    && m.row >= r.y
                                    && m.row < r.bottom()
                            })
                        }) {
                            self.session_menu = None;
                        }
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let _ = self.menu_scroll.wheel(m.column, m.row, 0);
                    }
                    _ => {}
                }
                return;
            }
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.named_session_click(m.column, m.row)
                }
                MouseEventKind::Down(MouseButton::Right) => {
                    // Right-click on a row → open Stop menu for running sessions only.
                    if let Some(idx) = self
                        .named_session_row_rects
                        .iter()
                        .find(|(_, r)| {
                            m.column >= r.x
                                && m.column < r.right()
                                && m.row >= r.y
                                && m.row < r.bottom()
                        })
                        .map(|(i, _)| *i)
                    {
                        if idx != 0 {
                            // Same reason as the guard above: bind `menu` first so
                            // `!menu.preparing` is available (`.and_then` would hide it).
                            if let Some(menu) = self.named_session_menu.as_ref() {
                                if let Some(row) = menu.rows.get(idx - 1) {
                                    if row.running && !row.current && !menu.preparing {
                                        self.open_session_menu(
                                            row.name.clone(),
                                            m.column,
                                            m.row,
                                            row.running,
                                            row.current,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                MouseEventKind::ScrollUp => self.move_named_session_cursor(-1),
                MouseEventKind::ScrollDown => self.move_named_session_cursor(1),
                _ => {}
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
        // ORCH overlays own the mouse. Visible controls act through the same
        // board methods as keyboard input; clicks elsewhere never fall through
        // to the dashboard behind the modal.
        if self.orch_form.is_some() || self.orch_start.is_some() || self.orch_detail.is_some() {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let hit = self
                        .orch_hits
                        .iter()
                        .find(|(_, rect)| {
                            m.column >= rect.x
                                && m.column < rect.right()
                                && m.row >= rect.y
                                && m.row < rect.bottom()
                        })
                        .map(|(hit, _)| hit.clone());
                    if let Some(hit) = hit {
                        self.orch_activate_hit(hit);
                    } else if self.orch_form.is_some() || self.orch_detail.is_some() {
                        // Match Settings and the folder picker: the modal
                        // surface is inert, while its dimmed backdrop cancels.
                        self.orch_form = None;
                        self.orch_detail = None;
                    }
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
        // A context menu taller than the space it has scrolls under the wheel:
        // menus are mouse-only, so this is the only way to reach a row that does
        // not fit. Only a popup actually under the cursor takes the event, so the
        // wheel goes on doing what it did before everywhere else.
        if let MouseEventKind::ScrollUp | MouseEventKind::ScrollDown = m.kind {
            let delta = if matches!(m.kind, MouseEventKind::ScrollUp) {
                -1
            } else {
                1
            };
            if self.menu_scroll.wheel(m.column, m.row, delta) {
                return;
            }
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
        if self.orch_menu.is_some() {
            if let MouseEventKind::Down(_) = m.kind {
                self.orch_menu_click(m.column, m.row);
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
        // Pointer interaction outside the FILES dock returns keyboard input to
        // the pane or control the user actually clicked. A click inside keeps
        // the tree focus and its cursor intact.
        if matches!(m.kind, MouseEventKind::Down(_)) && self.files_focused {
            let inside_files = m.column >= self.files_area.x
                && m.column < self.files_area.right()
                && m.row >= self.files_area.y
                && m.row < self.files_area.bottom();
            if !inside_files {
                self.files_focused = false;
            }
        }
        // WORKSPACES/AGENTS keyboard ownership follows the same rule as FILES:
        // a pointer press outside the focused list returns input to the pane.
        if matches!(m.kind, MouseEventKind::Down(_)) {
            let area = match self.sidebar_focus {
                Some(SidebarListFocus::Workspaces) => Some(self.workspaces_area),
                Some(SidebarListFocus::Agents) => Some(self.agents_area),
                None => None,
            };
            if area.is_some_and(|area| {
                m.column < area.x
                    || m.column >= area.right()
                    || m.row < area.y
                    || m.row >= area.bottom()
            }) {
                self.sidebar_focus = None;
            }
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
        // Right-click a pane tab, WORKSPACES row, live/scheduled agent, ORCH
        // row, file, dock row, or pane to open the matching context menu.
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
            } else if let Some((id, _)) = self.automation_rects.iter().find(|(_, rect)| hit(*rect))
            {
                let id = id.clone();
                if let Some(pane) = self.automation_live_pane(&id) {
                    self.open_agent_menu(AgentTarget::Live(pane), c, r);
                } else {
                    self.open_agent_menu(AgentTarget::Automation(id), c, r);
                }
            } else if let Some((i, _)) = self.session_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_agent_menu(AgentTarget::Session(*i), c, r); // session → Resume/Close
            } else if let Some((row, _)) = self.diff_row_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_diff_menu(*row, c, r);
            } else if let Some((i, _)) = self.file_tree_rects.iter().find(|(_, rect)| hit(*rect)) {
                self.open_file_menu(*i, c, r); // FILES-dock row → new/rename/delete (docs/38)
            } else if let Some(automation) =
                self.orch_hits
                    .iter()
                    .find_map(|(target, rect)| match target {
                        OrchHit::Automation(automation) if hit(*rect) => Some(automation.clone()),
                        _ => None,
                    })
            {
                if let Some(pane) = self.automation_live_pane(&automation) {
                    self.open_agent_menu(AgentTarget::Live(pane), c, r);
                } else {
                    self.open_agent_menu(AgentTarget::Automation(automation), c, r);
                }
            } else if let Some((task, _)) =
                self.orch_hits
                    .iter()
                    .find_map(|(target, rect)| match target {
                        OrchHit::Task(task) if hit(*rect) => Some((task.clone(), *rect)),
                        _ => None,
                    })
            {
                self.open_orch_menu(&task, c, r);
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
                // Native document links are exact rendered rectangles and only
                // activate through an explicit Ctrl/Cmd click. Claim that
                // gesture before resize or terminal forwarding can reinterpret
                // it; ordinary clicks remain text selection.
                if m.modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
                {
                    let preview_link = self
                        .preview_link_rects
                        .iter()
                        .find(|(_, _, rect)| {
                            m.column >= rect.x
                                && m.column < rect.right()
                                && m.row >= rect.y
                                && m.row < rect.bottom()
                        })
                        .map(|(pane, target, _)| (*pane, target.clone()));
                    if let Some((pane, target)) = preview_link {
                        self.activate_preview_link(pane, target);
                        return;
                    }
                }
                // A sidebar-edge drag (docs/29) claims the press first, but its
                // target is exactly the sidebar's rendered `│` rule. Pane borders
                // and content remain available for selection and child mouse input.
                if self.begin_sidebar_resize(m.column, m.row) {
                    return;
                }
                // Then the horizontal rule between two stacked docks. Checked
                // after the edge seam so a corner press still resizes the
                // sidebar, and before pane resize because this target lives
                // inside the sidebar, where no pane divider can be.
                if self.begin_dock_resize(m.column, m.row) {
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
                            beside: m.modifiers.contains(KeyModifiers::SHIFT),
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
                // Only a press inside a pane's content arms (and matches) the
                // detector, and both presses must land in the *same* pane: a title
                // or border click must never combine with a nearby body click into
                // a double-click, which would copy and swallow the pane's focus.
                let content_pane = self.pane_content_at(m.column, m.row).map(|(id, _)| id);
                let is_double = self.last_left_click.take().is_some_and(|(pane, at, when)| {
                    content_pane == Some(pane)
                        && now.duration_since(when) <= DOUBLE_CLICK
                        && m.column.abs_diff(at.0) <= 1
                        && m.row.abs_diff(at.1) <= 1
                });
                if is_double {
                    if self.copy_token_at(m.column, m.row) {
                        // Its release keeps the highlight instead of re-copying.
                        self.dbl_click_release = true;
                        return;
                    }
                } else if let Some(pane) = content_pane {
                    self.last_left_click = Some((pane, (m.column, m.row), now));
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
                if self.dock_resize.is_some() {
                    self.update_dock_resize(m.column, m.row);
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
                // A double-click already copied and scheduled its highlight
                // expiry on press. Its release only closes the gesture.
                if self.dbl_click_release {
                    self.dbl_click_release = false;
                    return;
                }
                if let Some(p) = self.link_press.take() {
                    if (m.column, m.row) == p.at {
                        if p.beside {
                            self.activate_link_in(p.target, crate::app::files::OpenTarget::Pane);
                        } else {
                            self.activate_link(p.target);
                        }
                    }
                    return;
                }
                if self.sidebar_resize.is_some() {
                    self.end_sidebar_resize();
                    return;
                }
                if self.dock_resize.is_some() {
                    self.end_dock_resize();
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
                // After a successful copy the highlight lingers briefly so you can
                // see what was copied; the toast times out on the same cadence.
                match self.selection_text() {
                    Some(text) => {
                        self.pending_clipboard = Some(text);
                        let msg = self.catalog.copied;
                        self.show_toast(msg);
                        self.schedule_copy_highlight_clear();
                    }
                    None => self.clear_selection(),
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
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => 0,
        };
        let hscroll: i32 = match m.kind {
            MouseEventKind::ScrollLeft => -3,
            MouseEventKind::ScrollRight => 3,
            _ => 0,
        };
        if scroll == 0 && hscroll == 0 && !matches!(m.kind, MouseEventKind::Down(MouseButton::Left))
        {
            return; // motion / release: hover updated, nothing else to do
        }
        let (c, r) = (m.column, m.row);
        let hit = |rect: Rect| c >= rect.x && c < rect.right() && r >= rect.y && r < rect.bottom();

        if scroll != 0 || hscroll != 0 {
            // Wheel over a sidebar list scrolls it one item per notch (the next
            // render clamps the offset to the list length).
            let step = |off: usize| {
                if scroll < 0 {
                    off.saturating_sub(1)
                } else {
                    off + 1
                }
            };
            if scroll != 0 && hit(self.workspaces_area) {
                self.workspaces_scroll = step(self.workspaces_scroll);
                return;
            }
            if scroll != 0 && hit(self.agents_area) {
                self.agents_scroll = step(self.agents_scroll);
                return;
            }
            if scroll != 0 && hit(self.files_area) {
                if self.files_mode == crate::diff::FilesMode::Diff {
                    self.diff_scroll_by(if scroll < 0 { -1 } else { 1 });
                } else {
                    self.file_tree.scroll = step(self.file_tree.scroll);
                }
                return;
            }
            // Wheel over a git tab scrolls its active view (docs/17).
            if scroll != 0 && self.active_is_git() && hit(self.last_pane_area) {
                self.git_scroll(scroll);
                return;
            }
            // Wheel over the orchestration board scrolls its list (docs/22).
            if scroll != 0 && self.active_is_orch() && hit(self.orch_area) {
                self.orch_scroll_by(scroll);
                return;
            }
            // Wheel over Mission Control scrolls its agent list (docs/54).
            if scroll != 0 && self.active_is_mission() && hit(self.mission_area) {
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
                let viewport = rect.height.saturating_sub(2) as usize;
                match self.views.get_mut(&id) {
                    Some(crate::app::ViewKind::File(v)) => {
                        let text_w = view_text_w(v, rect.width);
                        v.scroll_by(scroll, viewport, text_w);
                    }
                    Some(crate::app::ViewKind::Diff(v)) => {
                        let is_split = v.effective_split(rect.width);
                        if hscroll != 0 {
                            if hscroll < 0 {
                                v.horizontal = v.horizontal.saturating_sub(8);
                            } else {
                                v.horizontal = v.horizontal.saturating_add(8);
                            }
                            let marker_style = self.config.layout.diff_marker_style;
                            v.ensure_horizontal_visible(rect.width, marker_style, is_split);
                        } else {
                            let rows = if is_split {
                                v.split_rows.len()
                            } else {
                                v.stack_rows.len()
                            };
                            if is_split {
                                let current = v.split_row_for_stack(v.scroll);
                                let split = if scroll < 0 {
                                    current.saturating_sub(3)
                                } else {
                                    current.saturating_add(3).min(rows.saturating_sub(viewport))
                                };
                                if let Some(stack) = v.stack_row_for_split(split) {
                                    v.scroll = stack;
                                    v.selected = stack;
                                }
                            } else if scroll < 0 {
                                v.scroll = v.scroll.saturating_sub(3);
                                v.selected = v.scroll;
                            } else {
                                v.scroll = v
                                    .scroll
                                    .saturating_add(3)
                                    .min(rows.saturating_sub(viewport));
                                v.selected = v.scroll;
                            }
                        }
                    }
                    Some(crate::app::ViewKind::Preview(v)) => {
                        let key = crate::files::preview::LayoutKey {
                            width: rect.width.max(1),
                            ascii: false,
                        };
                        v.scroll_by(scroll, viewport, key);
                    }
                    None => {}
                }
                return;
            }
            // Otherwise the wheel scrolls the pane under the cursor.
            // Skip when only horizontal scroll is active — there is no
            // meaningful terminal horizontal-wheel protocol to forward.
            if scroll != 0 {
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
            } // scroll != 0
            return;
        }

        // The sidebar gear opens Settings.
        if self.named_session_button_rect.is_some_and(hit) {
            self.open_named_session_menu();
            return;
        }
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
            self.set_agents_filter(val);
            return;
        }
        // The "blocked in other workspaces" line jumps to a pane this scope hid,
        // rather than widening the list or cycling to a local blocked row.
        if let Some((id, _)) = self.agents_elsewhere_rect.filter(|(_, rect)| hit(*rect)) {
            self.sidebar_focus = None;
            self.focus_pane_global(id);
            return;
        }
        if let Some((id, _)) = self.agent_rects.iter().find(|(_, rect)| hit(*rect)) {
            let id = *id;
            self.sidebar_focus = None;
            self.focus_pane_global(id);
            return;
        }
        if let Some((id, _)) = self.automation_rects.iter().find(|(_, rect)| hit(*rect)) {
            let id = id.clone();
            self.sidebar_focus = None;
            self.open_automation_detail(&id);
            return;
        }
        // Clicking a resumable session row reopens it into a pane.
        if let Some((i, _)) = self.session_rects.iter().find(|(_, rect)| hit(*rect)) {
            let i = *i;
            self.sidebar_focus = None;
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
            self.files_focused = false;
            self.diff_row_activate(row, target);
            return;
        }
        // Clicking a FILES row expands/collapses a folder or opens a file (docs/38).
        // A plain click follows the `File click behavior` setting — reuse one
        // preview (the default) or open a whole tab; Shift is the permanent
        // read-only pane beside the focus, and never configurable.
        if let Some((i, _)) = self.file_tree_rects.iter().find(|(_, rect)| hit(*rect)) {
            let i = *i;
            let target = if m.modifiers.contains(KeyModifiers::SHIFT) {
                crate::app::files::OpenTarget::Pane
            } else {
                self.file_click_target()
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
            self.sidebar_focus = None;
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
        // ORCH controls use renderer-owned stable geometry. A double click is
        // the mouse equivalent of Enter and jumps to the task's worker pane.
        if self.active_is_orch() {
            let target = self
                .orch_hits
                .iter()
                .find(|(_, rect)| hit(*rect))
                .map(|(target, _)| target.clone());
            if let Some(OrchHit::Task(id)) = target {
                let now = Instant::now();
                let double = self.orch_last_click.take().is_some_and(|(previous, when)| {
                    previous == id && now.duration_since(when) <= DOUBLE_CLICK
                });
                if double {
                    self.orch_jump_to_task(&id);
                } else {
                    self.orch_select_task(&id);
                    self.orch_last_click = Some((id, now));
                }
            } else if let Some(OrchHit::Automation(id)) = target {
                let now = Instant::now();
                let double = self.orch_last_click.take().is_some_and(|(previous, when)| {
                    previous == id && now.duration_since(when) <= DOUBLE_CLICK
                });
                if self.orch_select_automation(&id) {
                    if double {
                        self.open_automation_detail(&id);
                    } else {
                        self.orch_last_click = Some((id, now));
                    }
                }
            } else if let Some(target) = target {
                self.orch_last_click = None;
                self.orch_activate_hit(target);
            }
            return;
        }
        // Mission Control mouse controls use geometry published by its renderer,
        // so responsive rows select the same stable item shown on screen.
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
            if let Some((id, _)) = self
                .mission_automation_rects
                .iter()
                .find(|(_, rect)| hit(*rect))
            {
                let id = id.clone();
                self.open_automation_detail(&id);
                return;
            }
            let row = self
                .mission_row_rects
                .iter()
                .find(|(_, rect)| hit(*rect))
                .map(|(index, _)| *index);
            if let Some(index) = row.filter(|index| *index < self.mission_rows.len()) {
                self.mission_cursor = index;
            }
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
                    let (app_cursor, disambiguate) = pane.key_encoding_modes();
                    if let Some(bytes) = encode_key(&key, newline, app_cursor, disambiguate) {
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
        self.clear_selection();
        self.scroll_pane = None;
        self.copy_mode = Some(CopyMode {
            pane: id,
            anchor: (history.saturating_sub(offset), 0),
            cursor: (history.saturating_sub(offset), 0),
            saved_scroll: offset,
            pending_count: 0,
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
        let text = self
            .panes
            .get(&copy.pane)
            .and_then(|pane| pane.retained_selection_text(copy.ordered()))
            .and_then(finish_selected_text);
        if let Some(text) = text {
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
    ///
    /// Motions take vim's count prefix (`12j`), `e` jumps to a word end, and
    /// `Ctrl+D`/`Ctrl+U` move by half a page. Anything unrecognised is swallowed
    /// rather than forwarded, so a stray key can never reach the selected program.
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
            copy.anchor = copy.cursor;
            copy.pending_count = 0;
            self.copy_mode = Some(copy);
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
        // Vim's count prefix, after the pane checks above: a digit must not keep
        // copy mode alive on a pane that has gone away. `0` joins a count already
        // being typed; on its own it keeps its older meaning of "first column",
        // so no existing key changes.
        if let KeyCode::Char(typed @ '0'..='9') = key.code {
            let digit = typed as usize - '0' as usize;
            if digit != 0 || copy.pending_count > 0 {
                copy.push_count_digit(digit);
                self.copy_mode = Some(copy);
                return true;
            }
        }
        let last_row = row_count.saturating_sub(1);
        let page = self.focused_page() as usize;
        let half_page = page.div_ceil(2).max(1);
        let count = copy.count();
        let rows = |per: usize| count.saturating_mul(per);
        // A count turns `g`/`G` into vim's absolute line jump. The rows a user
        // counts are 1-based, so `5G` is index 4. Read the raw count beside
        // `count` above, never through `copy`, so both derive from one snapshot.
        let explicit = copy.pending_count;
        let counted_row = |n: usize| n.saturating_sub(1).min(last_row);
        let ctrl = super::keys::is_ctrl_chord(key.modifiers);
        match key.code {
            // The only chords copy mode reads. Guarded so bare `d`/`u` stay unbound
            // instead of silently becoming half-page motions.
            KeyCode::Char('d') if ctrl => {
                copy.cursor.0 = copy.cursor.0.saturating_add(rows(half_page)).min(last_row)
            }
            KeyCode::Char('u') if ctrl => {
                copy.cursor.0 = copy.cursor.0.saturating_sub(rows(half_page))
            }
            KeyCode::Left | KeyCode::Char('h') => {
                copy.cursor.1 = copy.cursor.1.saturating_sub(count)
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let last = pane
                    .retained_row_layout(copy.cursor.0)
                    .map_or(0, |layout| layout.last_column());
                copy.cursor.1 = copy.cursor.1.saturating_add(count).min(last);
            }
            KeyCode::Up | KeyCode::Char('k') => copy.cursor.0 = copy.cursor.0.saturating_sub(count),
            KeyCode::Down | KeyCode::Char('j') => {
                copy.cursor.0 = copy.cursor.0.saturating_add(count).min(last_row)
            }
            KeyCode::PageUp | KeyCode::Char('b') => {
                copy.cursor.0 = copy.cursor.0.saturating_sub(rows(page))
            }
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
                copy.cursor.0 = copy.cursor.0.saturating_add(rows(page)).min(last_row)
            }
            KeyCode::Home | KeyCode::Char('g') => copy.cursor.0 = counted_row(explicit),
            KeyCode::End | KeyCode::Char('G') => {
                copy.cursor.0 = if explicit > 0 {
                    counted_row(explicit)
                } else {
                    last_row
                }
            }
            KeyCode::Char('0') => copy.cursor.1 = 0,
            KeyCode::Char('$') => {
                copy.cursor.1 = pane
                    .retained_row_layout(copy.cursor.0)
                    .map_or(0, |layout| layout.last_column())
            }
            KeyCode::Char('w') => {
                copy.cursor = repeat_motion(count, copy.cursor, |at| {
                    copy_word_forward(row_count, |row| pane.retained_row_layout(row), at)
                })
            }
            KeyCode::Char('e') => {
                copy.cursor = repeat_motion(count, copy.cursor, |at| {
                    copy_word_end(row_count, |row| pane.retained_row_layout(row), at)
                })
            }
            KeyCode::Char('B') => {
                copy.cursor = repeat_motion(count, copy.cursor, |at| {
                    copy_word_back(|row| pane.retained_row_layout(row), at)
                })
            }
            _ => {
                // An unrecognised key aborts the pending count the way vim does, so
                // a mistyped chord cannot silently multiply the next motion.
                copy.pending_count = 0;
                self.copy_mode = Some(copy);
                return true;
            }
        }
        copy.pending_count = 0;
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
        if let Some(crate::app::ViewKind::Preview(view)) = self.views.get(&sel.pane) {
            return super::preview::selection_text(view, sel.content, sel.ordered(), self.compact);
        }
        if let Some(selection) = sel.retained {
            let range = selection.ordered();
            return self
                .panes
                .get(&sel.pane)?
                .retained_selection_text(range)
                .and_then(finish_selected_text);
        }
        let ((sx, sy), (ex, ey)) = sel.ordered();
        let (cx, cy) = (sel.content.x, sel.content.y);
        self.panes
            .get(&sel.pane)?
            .visible_selection_text((
                (
                    (sy as usize).saturating_sub(cy as usize),
                    (sx as usize).saturating_sub(cx as usize),
                ),
                (
                    (ey as usize).saturating_sub(cy as usize),
                    (ex as usize).saturating_sub(cx as usize),
                ),
            ))
            .and_then(finish_selected_text)
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
        let rows = match self.views.get(&pane) {
            Some(crate::app::ViewKind::File(view)) => {
                let Some(rows) = crate::files::token_rows(view, content, self.compact) else {
                    return false;
                };
                align_plain_rows(rows)
            }
            Some(crate::app::ViewKind::Preview(view)) => {
                let Some(rows) = super::preview::token_rows(view, content, self.compact) else {
                    return false;
                };
                align_plain_rows(rows)
            }
            // DIFF owns its source-cell clicks for review and note selection.
            // Do not route it through generic word copying.
            Some(crate::app::ViewKind::Diff(_)) => return false,
            None => {
                let Some(p) = self.panes.get(&pane) else {
                    return false;
                };
                let Ok(engine) = p.engine.lock() else {
                    return false;
                };
                // Cell-aligned rows: a wide glyph before the token would otherwise
                // shift `gcol` off the intended character (and its highlight).
                engine.visible_rows_aligned()
            }
        };
        let (gcol, grow) = (col - content.x, row - content.y);
        let (text, spans) = match copy_link_at_grid(&rows, gcol, grow) {
            Some(pair) => pair,
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
        // highlight is transient (screen coordinates, cleared after copy or the
        // next click), so it carries no retained-history span.
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
        self.schedule_copy_highlight_clear();
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
            ensure_process_tree_root(pid, &pane.command, crate::platform::process_tree(pid))
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
            // Cell-aligned rows so a wide glyph before the link doesn't shift the
            // column the underline lands on (or which cells Ctrl-click opens).
            engine.visible_rows_aligned()
        };
        let grid_col = col - content.x;
        let grid_row = row - content.y;

        // OSC 8 carries an authoritative target which can differ from the label
        // Claude and other terminal programs render. Preserve it rather than
        // reclassifying a visible filename as a website. An unsupported or
        // malformed target is deliberately inert and never falls back to the
        // label, since that would undo the safety boundary.
        if let Some(hyperlink) = rows.hyperlink_at(grid_row, grid_col) {
            let uri = hyperlink.uri();
            let visible = crate::links::link_at(rows.rows(), grid_col, grid_row);
            let line = visible.as_ref().and_then(|link| match &link.hit {
                crate::links::Hit::Path { line, .. } => *line,
                crate::links::Hit::Url(_) => None,
            });
            let (hit, target) = if let Some(path) = crate::links::file_uri_path(uri) {
                if !path.is_file() {
                    return None;
                }
                (
                    crate::links::Hit::Path {
                        raw: uri.to_string(),
                        text: path.to_string_lossy().into_owned(),
                        line,
                    },
                    LinkTarget::File { path, line },
                )
            } else if crate::platform::is_openable_url(uri) {
                (
                    crate::links::Hit::Url(uri.to_string()),
                    LinkTarget::Url(uri.to_string()),
                )
            } else {
                return None;
            };
            let link = crate::links::Link {
                hit,
                spans: hyperlink.spans().to_vec(),
            };
            return Some(HoverLink { pane, link, target });
        }

        let link = crate::links::link_at(rows.rows(), grid_col, grid_row)?;
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
    /// file opens in luvus itself — where `File click behavior` says (docs/38),
    /// exactly as a click on its FILES row would. That is a reused preview
    /// pane beside the focus by default, so a path an agent printed lands
    /// next to the conversation rather than in a tab that hides it.
    pub fn activate_link(&mut self, target: LinkTarget) {
        let place = self.file_click_target();
        self.activate_link_in(target, place);
    }

    /// [`activate_link`](Self::activate_link) with the placement the gesture
    /// asked for: `Ctrl`+`Shift`+click is the tree's `Shift`+click, a
    /// permanent pane beside the focus. A URL has no placement and goes to the
    /// browser regardless.
    pub fn activate_link_in(&mut self, target: LinkTarget, place: crate::app::files::OpenTarget) {
        match target {
            LinkTarget::Url(url) => self.open_url(url),
            LinkTarget::File { path, line } => self.open_file_link(path, line, place),
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
        self.toast = Some((text.into(), Instant::now() + COPY_HIGHLIGHT_DURATION));
    }

    fn schedule_copy_highlight_clear(&mut self) {
        self.selection_clear_at = Some(Instant::now() + COPY_HIGHLIGHT_DURATION);
    }

    fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_clear_at = None;
    }

    /// Clear an expired copied-selection highlight; returns true when it changed
    /// so the loop repaints once to remove it, since idle frames aren't rendered.
    pub fn tick_copy_highlight(&mut self, now: Instant) -> bool {
        if self.selection_clear_at.is_some_and(|at| now >= at) {
            self.clear_selection();
            true
        } else {
            false
        }
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

    /// Deliver `text` to the focused pane exactly as a paste from the outer
    /// terminal does, and report which pane took it (`None` when the focused
    /// leaf is a native view or has no pane at all).
    ///
    /// `send_paste` re-wraps the text in the bracketed-paste markers crossterm
    /// stripped, so a child that distinguishes paste from typing (an agent CLI
    /// attaching a dropped file, vim not auto-indenting) still sees a paste.
    /// The pane also snaps to live and the input is marked as the user's, so
    /// detection doesn't read the echo as agent work.
    ///
    /// This is the one place that does it: `Insert Path` (docs/38) goes through
    /// here rather than writing at the pane, so bracketed paste, scroll
    /// position, and activity tracking cannot drift between the two.
    pub(crate) fn paste_into_focused_pane(&mut self, text: &str) -> Option<PaneId> {
        let id = self.layout().focus;
        let target = self.panes.get(&id).map(|p| {
            p.scroll_to_bottom(); // pasting is input → snap to live
            p.send_paste(text);
            id
        });
        self.mark_user_input(); // so the echo isn't misread as agent work
        target
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
        // The help cheat-sheet is a complete, scrollable shortcut reference.
        // Unknown keys still dismiss it and are swallowed, preserving the old
        // safety property that closing help cannot act on the focused pane.
        if self.help_open {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.min(self.help_scroll_max).saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.help_scroll = self.help_scroll.saturating_add(10)
                }
                KeyCode::PageUp => {
                    self.help_scroll = self
                        .help_scroll
                        .min(self.help_scroll_max)
                        .saturating_sub(10)
                }
                KeyCode::Home => self.help_scroll = 0,
                KeyCode::End => self.help_scroll = self.help_scroll_max,
                _ => self.help_open = false,
            }
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
        if self.named_session_menu.is_some() {
            // Context menu Esc should close the menu before the session popup.
            if self.session_menu.is_some() && key.code == KeyCode::Esc {
                self.session_menu = None;
                return true;
            }
            if self.session_menu.is_some() {
                self.handle_session_menu_key(key);
                return true;
            }
            self.named_session_key(key);
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
            self.handle_file_menu_key(key);
            return true;
        }
        if self.diff_menu.is_some() {
            self.handle_diff_menu_key(key);
            return true;
        }
        if self.orch_menu.is_some() {
            if key.code == KeyCode::Esc {
                self.orch_menu = None;
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
        // Explicit direct shortcuts are the only normal-mode keys Luvus takes
        // before pane/dashboard dispatch. The configured prefix retains
        // precedence, and an empty direct map makes this a cheap no-op.
        if self.mode == Mode::Normal && !self.prefix.matches(&key) {
            if let Some(command) = keys::direct_command(&self.direct_keymap, &key) {
                self.run_cmd(command);
                return true;
            }
        }
        // WORKSPACES/AGENTS dock focus is explicit and separate from pane focus.
        if let Some(focus) = self.sidebar_focus {
            if self.prefix.matches(&key) {
                self.sidebar_focus = None;
                self.mode = Mode::Prefix;
            } else {
                match focus {
                    SidebarListFocus::Workspaces => self.handle_workspaces_key(key),
                    SidebarListFocus::Agents => self.handle_agents_key(key),
                };
            }
            return true;
        }
        // FILES/DIFF dock focus is explicit and separate from terminal-pane
        // focus. The prefix remains available for global commands; ordinary
        // keys never leak into the pane until Esc/q returns control to it.
        if self.files_focused {
            if self.prefix.matches(&key) {
                self.files_focused = false;
                self.mode = Mode::Prefix;
            } else {
                match self.files_mode {
                    crate::diff::FilesMode::Files => self.handle_file_tree_key(key),
                    crate::diff::FilesMode::Diff => self.handle_diff_list_key(key),
                };
            }
            return true;
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
                    if let Some(pane) = self.focused() {
                        let (app_cursor, disambiguate) = pane.key_encoding_modes();
                        if let Some(bytes) = encode_key(&prefix, &newline, app_cursor, disambiguate)
                        {
                            pane.send(&bytes);
                        }
                    }
                    return true; // left prefix mode → the status bar updates
                }
                // Fixed convenience keys (not rebindable): unshifted `1`–`9`
                // jump to a tab, and `?` opens the shortcut cheat-sheet. Shifted
                // digits continue to the configurable command map below, where
                // their normalized number-row symbols select workspaces.
                if let KeyCode::Char(c) = key.code {
                    if c.is_ascii_digit()
                        && c != '0'
                        && !key.modifiers.contains(KeyModifiers::SHIFT)
                    {
                        self.switch_tab(c as usize - '1' as usize);
                        return true;
                    }
                    if c == '?' {
                        self.help_open = true;
                        self.help_scroll = 0;
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
                // ignores held Ctrl/Alt and normalizes shifted digits, so both
                // two-step and held-chord input resolve to the configured key.
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
                    Some(crate::app::ViewKind::Preview(_)) => {
                        return self.handle_preview_key(focus, key)
                    }
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
                let (app_cursor, disambiguate) = self
                    .focused()
                    .map(|pane| pane.key_encoding_modes())
                    .unwrap_or((false, false));
                if let Some(bytes) = encode_key(&key, newline, app_cursor, disambiguate) {
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
fn encode_key(
    key: &KeyEvent,
    newline: &[u8],
    app_cursor: bool,
    disambiguate: bool,
) -> Option<Vec<u8>> {
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
                if disambiguate {
                    // Once the nested application opts into Kitty keyboard
                    // disambiguation, every Ctrl+character chord uses CSI-u.
                    // This preserves the protocol's key identity instead of
                    // mixing negotiated CSI-u with legacy control bytes.
                    let codepoint = match c {
                        // Crossterm represents a legacy 0x1f input byte as
                        // Ctrl+7. The originating terminal could not
                        // distinguish it from Ctrl+/, so prefer the user-facing
                        // slash binding for the nested CSI-u client.
                        '/' | '7' => '/',
                        // Kitty reports the unshifted codepoint and carries
                        // Shift in the modifier parameter. Normalize Unicode
                        // letters too when their lowercase form is one scalar.
                        character if shift && character.is_alphabetic() => {
                            single_lowercase_codepoint(character)
                        }
                        _ => c,
                    };
                    return Some(csi_u_char(codepoint, key.modifiers));
                }
                let b = legacy_control_byte(c)?;
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
        KeyCode::Backspace => {
            if alt {
                vec![0x1b, 0x7f]
            } else {
                vec![0x7f]
            }
        }
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

/// Lowercase a key identity only when Unicode maps it to exactly one scalar.
fn single_lowercase_codepoint(character: char) -> char {
    let mut lowercase = character.to_lowercase();
    let first = lowercase.next().unwrap_or(character);
    if lowercase.next().is_none() {
        first
    } else {
        character
    }
}

fn legacy_control_byte(character: char) -> Option<u8> {
    Some(match character.to_ascii_lowercase() {
        'a'..='z' => (character.to_ascii_uppercase() as u8) & 0x1f,
        ' ' | '@' => 0,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        // Ctrl+/ is the user-facing chord for the US control byte. Legacy
        // terminal input arrives through crossterm as Ctrl+7, while enhanced
        // keyboard protocols preserve `/`.
        '_' | '/' | '7' => 0x1f,
        _ => return None,
    })
}

fn csi(final_byte: u8) -> Vec<u8> {
    vec![0x1b, b'[', final_byte]
}

fn csi_u_char(character: char, modifiers: KeyModifiers) -> Vec<u8> {
    format!(
        "\x1b[{};{}u",
        character as u32,
        key_modifier_param(modifiers)
    )
    .into_bytes()
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
    fn task_prompt_paste_preserves_normalized_newlines() {
        let _env = crate::persist::test_env("orch-prompt-paste");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.orch_form = Some(crate::app::OrchForm {
            kind: crate::app::OrchFormKind::Task,
            field: crate::app::OrchFormField::Prompt,
            ..crate::app::OrchForm::default()
        });

        assert!(app.paste_into_modal("first\r\nsecond\rthird\tline"));
        assert_eq!(
            app.orch_form.as_ref().unwrap().prompt,
            "first\nsecond\nthirdline"
        );
    }

    #[test]
    fn prefix_digits_jump_to_tabs_and_shifted_digits_jump_to_workspaces() {
        let _env = crate::persist::test_env("prefix-shifted-workspace-jump");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        let focus = app.layout().focus;
        for position in 2..=9 {
            app.workspaces[0]
                .tabs
                .push(Tab::panes(TileLayout::new(focus)));
            app.workspaces.push(Workspace {
                id: crate::ids::public_id("workspace"),
                name: format!("workspace-{position}"),
                cwd: std::path::PathBuf::from(format!("/tmp/workspace-{position}")),
                branch: None,
                git_ahead_behind: None,
                worktree: None,
                tabs: vec![Tab::panes(TileLayout::new(focus))],
                active_tab: 0,
                pinned: false,
            });
        }

        let prefix = || AppEvent::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let shifted = ['!', '@', '#', '$', '%', '^', '&', '*', '('];
        for (index, (digit, symbol)) in ('1'..='9').zip(shifted).enumerate() {
            app.active_ws = 0;
            app.handle_event(prefix());
            app.handle_event(AppEvent::Key(KeyEvent::new(
                KeyCode::Char(digit),
                KeyModifiers::NONE,
            )));
            assert_eq!(app.active_ws, 0, "plain digit stays in the workspace");
            assert_eq!(app.ws().active_tab, index, "plain digit jumps to a tab");

            // Unix legacy input reports the shifted symbol without modifiers,
            // Windows retains Shift on that symbol, and enhanced Unix input
            // reports the base digit with Shift. All three travel through the
            // real prefix path and resolve to the same configurable action.
            for key in [
                KeyEvent::new(KeyCode::Char(symbol), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char(symbol), KeyModifiers::SHIFT),
                KeyEvent::new(KeyCode::Char(digit), KeyModifiers::SHIFT),
            ] {
                app.active_ws = usize::from(index == 0);
                app.handle_event(prefix());
                app.handle_event(AppEvent::Key(key));
                assert_eq!(app.active_ws, index, "{key:?} jumps to workspace");
            }
        }
    }

    #[test]
    fn command_inspect_fills_only_a_missing_process_root() {
        let descendant = crate::platform::ProcInfo {
            pid: 43,
            depth: 1,
            command: "worker".into(),
        };
        let processes = ensure_process_tree_root(42, "shell --login", vec![descendant.clone()]);
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].pid, 42);
        assert_eq!(processes[0].depth, 0);
        assert_eq!(processes[0].command, "shell --login");
        assert_eq!(processes[1], descendant);

        let existing_root = crate::platform::ProcInfo {
            pid: 42,
            depth: 0,
            command: "os-reported shell --login".into(),
        };
        let processes = ensure_process_tree_root(42, "fallback", vec![existing_root.clone()]);
        assert_eq!(processes, vec![existing_root]);
        assert!(ensure_process_tree_root(0, "pending", Vec::new()).is_empty());
    }

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

    // A server that has closed its last project keeps running with a neutral
    // home terminal, so control-API requests routed through the event channel
    // must still be answered rather than dropping the CLI reply channel.
    #[test]
    fn api_requests_are_answered_after_the_last_project_closes() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.close_workspace(0);
        assert_eq!(app.workspaces.len(), 1, "the home terminal replaced it");
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
    fn merge_completion_settles_without_a_workspace() {
        let _env = crate::persist::test_env("merge-completion-no-workspace");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.orch
            .add_task("merge".into(), vec![], vec![], None)
            .unwrap();
        app.orch
            .set_status("t1", crate::orch::TaskStatus::Done)
            .unwrap();
        app.orch.begin_merge("t1").unwrap();
        app.workspaces.clear();
        let (reply, response) = std::sync::mpsc::channel();

        let dirty = app.handle_event(AppEvent::TaskMergeFinished {
            task: "t1".into(),
            branch: "luvus/t1".into(),
            previous: crate::orch::TaskStatus::Done,
            integration_branch: "luvus/integration".into(),
            result: Ok(crate::git::local::MergeOutcome::Merged {
                commit: "a".repeat(40),
            }),
            reply: Some(("merge-no-workspace".into(), reply)),
        });

        assert!(dirty);
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Merged
        );
        let response: serde_json::Value = serde_json::from_str(
            &response
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("the parked merge request is settled"),
        )
        .unwrap();
        assert_eq!(response["result"]["outcome"], "merged");
    }

    #[test]
    fn task_merge_api_parks_until_background_git_finishes() {
        let _env = crate::persist::test_env("task-merge-api");
        let (tx, events) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.orch
            .add_task("merge".into(), vec![], vec![], None)
            .unwrap();
        app.orch
            .set_status("t1", crate::orch::TaskStatus::Done)
            .unwrap();
        let missing = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/nonexistent-orch-merge-repository");
        app.orch.bind_worktree(
            "t1",
            Some(missing.display().to_string()),
            Some("luvus/t1".into()),
        );
        let (reply, response) = std::sync::mpsc::channel();

        app.handle_event(AppEvent::Api(crate::ipc::api::ApiRequest {
            id: "merge-api".into(),
            method: "task.merge".into(),
            params: json!({"id":"t1"}),
            reply,
        }));

        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Merging
        );
        assert!(response.try_recv().is_err(), "the API reply is parked");
        let event = (0..20)
            .find_map(|_| {
                events
                    .recv_timeout(std::time::Duration::from_millis(250))
                    .ok()
                    .filter(|event| matches!(event, AppEvent::TaskMergeFinished { .. }))
            })
            .expect("the background Git probe reports completion");
        app.handle_event(event);

        let response: serde_json::Value = serde_json::from_str(
            &response
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("the parked request receives the merge failure"),
        )
        .unwrap();
        assert_eq!(response["error"]["code"], "merge_error");
        assert_eq!(
            app.orch.task("t1").unwrap().status,
            crate::orch::TaskStatus::Done,
            "an infrastructure failure restores the safe pre-merge state"
        );
    }

    #[test]
    fn discarded_cwd_scan_releases_the_inflight_guard() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.cwd_scan_inflight = true;
        app.close_workspace(0);
        assert_eq!(app.workspaces.len(), 1, "the home terminal replaced it");

        let dirty = app.handle_event(AppEvent::CwdScanned {
            panes: Vec::new(),
            branches: Vec::new(),
            workspace_candidates: Vec::new(),
        });

        assert!(!dirty, "an empty scan needs no repaint");
        assert!(
            !app.cwd_scan_inflight,
            "a reopened workspace must be allowed to start another scan"
        );
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
        let enter =
            |m: KeyModifiers| encode_key(&KeyEvent::new(KeyCode::Enter, m), nl, false, false);
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
                false,
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
        let enc = |c: char, m: KeyModifiers| {
            encode_key(&KeyEvent::new(KeyCode::Char(c), m), nl, false, false)
        };
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
            encode_key(&KeyEvent::new(KeyCode::Enter, altgr), nl, false, false),
            Some(nl.to_vec()),
            "Ctrl+Alt+Enter still sends the configured newline"
        );
    }

    /// The Shift/Alt+Enter sequence is whatever `config::shift_enter` selects —
    /// so a setup whose agent wants a bare `LF` can get one.
    #[test]
    fn shift_enter_sequence_is_configurable() {
        let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(
            encode_key(&shift, b"\n", false, false),
            Some(b"\n".to_vec())
        );
        assert_eq!(
            encode_key(&shift, b"\x1b[13;2u", false, false),
            Some(b"\x1b[13;2u".to_vec())
        );
        // Plain Enter ignores the newline sequence and always submits.
        let plain = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            encode_key(&plain, b"\n", false, false),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn character_keys_preserve_alt_with_control() {
        let key = |modifiers| {
            encode_key(
                &KeyEvent::new(KeyCode::Char('a'), modifiers),
                b"\x1b\r",
                false,
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
    fn alt_backspace_sends_meta_delete_for_word_deletion() {
        let key = |modifiers, disambiguate| {
            encode_key(
                &KeyEvent::new(KeyCode::Backspace, modifiers),
                b"\x1b\r",
                false,
                disambiguate,
            )
        };

        for disambiguate in [false, true] {
            assert_eq!(key(KeyModifiers::NONE, disambiguate), Some(vec![0x7f]));
            assert_eq!(key(KeyModifiers::ALT, disambiguate), Some(vec![0x1b, 0x7f]));
        }
    }

    #[test]
    fn control_slash_reaches_nested_tuis_across_terminal_encodings() {
        let encode = |character, disambiguate| {
            encode_key(
                &KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
                b"\x1b\r",
                false,
                disambiguate,
            )
        };

        assert_eq!(encode('/', false), Some(vec![0x1f]));
        assert_eq!(
            encode('7', false),
            Some(vec![0x1f]),
            "crossterm decodes the legacy 0x1f byte as Ctrl+7"
        );
        assert_eq!(encode('_', false), Some(vec![0x1f]));

        assert_eq!(encode('/', true), Some(b"\x1b[47;5u".to_vec()));
        assert_eq!(
            encode('7', true),
            Some(b"\x1b[47;5u".to_vec()),
            "a legacy Ctrl+/ alias regains slash identity for a nested CSI-u client"
        );
        assert_eq!(encode('_', true), Some(b"\x1b[95;5u".to_vec()));
    }

    #[test]
    fn control_characters_use_full_csi_u_only_after_negotiation() {
        let encode = |character, disambiguate| {
            encode_key(
                &KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
                b"\x1b\r",
                false,
                disambiguate,
            )
        };

        // Legacy mode retains traditional control bytes and cannot represent
        // the remaining chords without losing their Ctrl modifier.
        assert_eq!(encode('a', false), Some(vec![0x01]));
        assert_eq!(encode('[', false), Some(vec![0x1b]));
        for character in [';', '\'', ',', '.', '-', '=', '`', '1', '8', '€'] {
            assert_eq!(
                encode(character, false),
                None,
                "Ctrl+{character} has no legacy representation"
            );
        }

        // After the nested application opts in, every Ctrl+character chord is
        // encoded consistently as CSI-u, including those with legacy bytes.
        for character in ['a', '[', ';', '\'', ',', '.', '-', '=', '`', '1', '8', '€'] {
            assert_eq!(
                encode(character, true),
                Some(format!("\x1b[{};5u", character as u32).into_bytes()),
                "Ctrl+{character} should use negotiated CSI-u"
            );
        }
        // Shifted Unicode letters use their unshifted, single-codepoint form
        // once CSI-u is negotiated. Legacy mode remains unable to represent
        // this chord and therefore keeps its previous no-output behavior.
        let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        let unicode = |disambiguate| {
            encode_key(
                &KeyEvent::new(KeyCode::Char('É'), ctrl_shift),
                b"\x1b\r",
                false,
                disambiguate,
            )
        };
        assert_eq!(unicode(false), None);
        assert_eq!(
            unicode(true),
            Some(format!("\x1b[{};6u", 'é' as u32).into_bytes())
        );
    }

    /// `Ctrl+Shift+<letter>` must survive the trip to a nested TUI. The legacy
    /// fold `to_ascii_uppercase() & 0x1f` is caseless, so it maps Ctrl+Shift+P
    /// and Ctrl+P onto the same 0x10 and an agent binding the shifted chord
    /// silently gets the unshifted action instead.
    #[test]
    fn control_shift_letters_reach_nested_tuis_distinctly() {
        let encode = |character, modifiers, disambiguate| {
            encode_key(
                &KeyEvent::new(KeyCode::Char(character), modifiers),
                b"\x1b\r",
                false,
                disambiguate,
            )
        };
        let ctrl = KeyModifiers::CONTROL;
        let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;

        // Legacy encoding cannot separate them; this is the collapse itself.
        assert_eq!(encode('p', ctrl, false), Some(vec![0x10]));
        assert_eq!(encode('p', ctrl_shift, false), Some(vec![0x10]));

        // A CSI-u client gets full negotiated encoding for both chords. The
        // codepoint stays lowercase `p` and Shift rides in the modifier param:
        // 5 = ctrl, 6 = ctrl+shift.
        assert_eq!(encode('p', ctrl, true), Some(b"\x1b[112;5u".to_vec()));
        assert_eq!(encode('p', ctrl_shift, true), Some(b"\x1b[112;6u".to_vec()));
        assert_ne!(encode('p', ctrl, true), encode('p', ctrl_shift, true));

        // Crossterm may report the shifted press as uppercase; it must still
        // report 112, never 80, or the ambiguity returns.
        assert_eq!(encode('P', ctrl_shift, true), Some(b"\x1b[112;6u".to_vec()));

        // Plain typing and Shift-only capitals remain untouched by the
        // negotiated Ctrl encoding.
        assert_eq!(encode('a', ctrl, true), Some(b"\x1b[97;5u".to_vec()));
        assert_eq!(encode('A', KeyModifiers::SHIFT, true), Some(b"A".to_vec()));
        assert_eq!(encode('a', KeyModifiers::NONE, true), Some(b"a".to_vec()));
    }

    #[test]
    fn navigation_keys_preserve_modifiers_for_nested_prompt_editors() {
        let key =
            |code, modifiers| encode_key(&KeyEvent::new(code, modifiers), b"\x1b\r", false, false);

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
        let app =
            |code, modifiers| encode_key(&KeyEvent::new(code, modifiers), b"\x1b\r", true, false);
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
        let key =
            |code, modifiers| encode_key(&KeyEvent::new(code, modifiers), b"\x1b\r", false, false);
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
        let key = |n, modifiers| {
            encode_key(
                &KeyEvent::new(KeyCode::F(n), modifiers),
                b"\x1b\r",
                false,
                false,
            )
        };
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
    fn wide_orchestration_double_click_jumps_to_the_worker() {
        let _env = crate::persist::test_env("orch-mouse-actions");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(180, 32, tx).unwrap();
        app.orch
            .add_task("mouse task".into(), vec![], vec![], None)
            .unwrap();
        let worker = *app.panes.keys().next().unwrap();
        app.orch.claim("t1", worker.0).unwrap();
        app.open_orch_board();
        let mut term = Terminal::new(TestBackend::new(180, 32)).unwrap();
        term.draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let rendered: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("SELECTED TASK · t1"));
        let row = app
            .orch_hits
            .iter()
            .find_map(|(hit, rect)| matches!(hit, OrchHit::Task(id) if id == "t1").then_some(*rect))
            .expect("task row is clickable");
        let at = (row.x + 1, row.y);

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Right),
            at,
            KeyModifiers::NONE,
        ));
        assert_eq!(
            app.orch_menu.as_ref().map(|menu| menu.task.as_str()),
            Some("t1")
        );
        assert!(app
            .orch_menu_items("t1")
            .contains(&crate::app::OrchMenuItem::Details));
        app.orch_menu = None;

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert!(!app.active_is_orch());
        assert_eq!(app.layout().focus, worker);
    }

    fn add_active_agent_automation(app: &mut App) -> (crate::ids::PaneId, String) {
        let pane = app.layout().focus;
        let terminal_id = app
            .panes
            .get(&pane)
            .and_then(|pane| pane.terminal_runtime())
            .expect("test pane has a live terminal")
            .terminal_id;
        app.status.get_mut(&pane).unwrap().agent = "codex".into();
        let workspace_id = app.workspace_of_pane(pane).unwrap().id.clone();
        let definition = app
            .automation
            .create(
                crate::automation::CreateAutomation {
                    name: "continue review".into(),
                    enabled: true,
                    trigger: crate::automation::Trigger::Once {
                        at_utc: 4_000_000_000,
                    },
                    target: crate::automation::AutomationTarget::ActiveAgent {
                        pane_id: pane.0,
                        terminal_id,
                        if_busy: crate::automation::ActiveAgentBusyPolicy::Wait,
                        durable: None,
                    },
                    task: crate::automation::TaskTemplate {
                        title: "continue review".into(),
                        prompt: "Review the current changes.".into(),
                        agent_id: "codex".into(),
                        workspace_id,
                        mode: crate::orch::TaskWorkerMode::Workspace,
                        access: crate::automation::AutomationAccess::Workspace,
                        paths: Vec::new(),
                        gate: None,
                    },
                    policy: crate::automation::AutomationPolicy::default(),
                },
                None,
                10,
            )
            .unwrap();
        (pane, definition.id)
    }

    #[test]
    fn automation_row_opens_details_and_uses_the_live_agent_context() {
        let _env = crate::persist::test_env("automation-row-actions");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 30, tx).unwrap();
        let (pane, automation) = add_active_agent_automation(&mut app);
        app.open_orch_board();
        app.orch_view = crate::app::OrchView::Automations;
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let row = app
            .orch_hits
            .iter()
            .find_map(|(hit, rect)| {
                matches!(hit, OrchHit::Automation(id) if id == &automation).then_some(*rect)
            })
            .expect("automation row is clickable");
        let at = (row.x + 1, row.y);

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Right),
            at,
            KeyModifiers::NONE,
        ));
        assert!(matches!(
            app.agent_menu.as_ref().map(|menu| menu.target.clone()),
            Some(AgentTarget::Live(target)) if target == pane
        ));
        assert!(app.orch_detail.is_none());
        app.agent_menu = None;

        double_click(&mut app, at);
        assert_eq!(app.orch_detail.as_deref(), Some(automation.as_str()));
        app.handle_orch_detail_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.orch_detail.is_none());
        assert!(!app.active_is_orch());
        assert_eq!(app.layout().focus, pane);
    }

    #[test]
    fn automation_detail_enter_follows_a_live_orch_worker() {
        let _env = crate::persist::test_env("automation-worker-detail");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 30, tx).unwrap();
        let worker = app.layout().focus;
        let workspace_id = app.workspaces[0].id.clone();
        let definition = app
            .automation
            .create(
                crate::automation::CreateAutomation {
                    name: "scheduled review".into(),
                    enabled: true,
                    trigger: crate::automation::Trigger::Once {
                        at_utc: 4_000_000_000,
                    },
                    target: crate::automation::AutomationTarget::NewWorker,
                    task: crate::automation::TaskTemplate {
                        title: "scheduled review".into(),
                        prompt: "Review changes".into(),
                        agent_id: "codex".into(),
                        workspace_id,
                        mode: crate::orch::TaskWorkerMode::Workspace,
                        access: crate::automation::AutomationAccess::Workspace,
                        paths: Vec::new(),
                        gate: None,
                    },
                    policy: crate::automation::AutomationPolicy::default(),
                },
                None,
                10,
            )
            .unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();
        let task = app
            .orch
            .add_task("scheduled review".into(), Vec::new(), Vec::new(), None)
            .unwrap();
        app.orch.claim(&task.id, worker.0).unwrap();
        app.automation
            .bind_task(&run.id, task.id.clone(), 21)
            .unwrap();
        app.open_orch_board();
        app.open_automation_detail(&definition.id);

        app.handle_orch_detail_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.orch_detail.is_none());
        assert!(!app.active_is_orch());
        assert_eq!(app.layout().focus, worker);
    }

    #[test]
    fn scheduled_sidebar_uses_detail_on_left_and_agent_menu_on_right() {
        let _env = crate::persist::test_env("automation-sidebar-actions");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 30, tx).unwrap();
        let (pane, automation) = add_active_agent_automation(&mut app);
        let row = Rect::new(2, 4, 24, 2);
        app.automation_rects = vec![(automation.clone(), row)];
        let at = (row.x + 1, row.y);

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Right),
            at,
            KeyModifiers::NONE,
        ));
        assert!(matches!(
            app.agent_menu.as_ref().map(|menu| menu.target.clone()),
            Some(AgentTarget::Live(target)) if target == pane
        ));
        assert!(app.orch_detail.is_none());
        app.agent_menu = None;

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.orch_detail.as_deref(), Some(automation.as_str()));
        assert!(app.agent_menu.is_none());
    }

    #[test]
    fn scheduled_sidebar_opens_automation_menu_without_a_live_pane() {
        let _env = crate::persist::test_env("automation-sidebar-placeholder-menu");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 30, tx).unwrap();
        let workspace_id = app.workspaces[0].id.clone();
        let definition = app
            .automation
            .create(
                crate::automation::CreateAutomation {
                    name: "scheduled review".into(),
                    enabled: true,
                    trigger: crate::automation::Trigger::Once {
                        at_utc: 4_000_000_000,
                    },
                    target: crate::automation::AutomationTarget::NewWorker,
                    task: crate::automation::TaskTemplate {
                        title: "scheduled review".into(),
                        prompt: "Review changes".into(),
                        agent_id: "codex".into(),
                        workspace_id,
                        mode: crate::orch::TaskWorkerMode::Workspace,
                        access: crate::automation::AutomationAccess::Workspace,
                        paths: Vec::new(),
                        gate: None,
                    },
                    policy: crate::automation::AutomationPolicy::default(),
                },
                None,
                10,
            )
            .unwrap();
        let row = Rect::new(2, 4, 24, 2);
        app.automation_rects = vec![(definition.id.clone(), row)];

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Right),
            (row.x + 1, row.y),
            KeyModifiers::NONE,
        ));

        let menu = app.agent_menu.as_ref().expect("automation menu opens");
        assert_eq!(menu.target, AgentTarget::Automation(definition.id.clone()));
        let items = app.agent_menu_items(menu.target.clone());
        assert!(items.contains(&crate::app::AgentMenuItem::AutomationDetails));
        assert!(items.contains(&crate::app::AgentMenuItem::AutomationRun));
        assert!(items.contains(&crate::app::AgentMenuItem::AutomationToggle));
        assert!(items.contains(&crate::app::AgentMenuItem::AutomationDelete));
        assert!(app.orch_detail.is_none());

        app.agent_menu_action(crate::app::AgentMenuItem::AutomationDetails);
        assert_eq!(app.orch_detail.as_deref(), Some(definition.id.as_str()));
        assert!(app.agent_menu.is_none());
    }

    #[test]
    fn new_task_form_stays_open_inside_and_closes_on_its_backdrop() {
        let _env = crate::persist::test_env("orch-form-backdrop");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 32, tx).unwrap();
        app.open_orch_board();
        app.open_orch_form();
        let mut term = Terminal::new(TestBackend::new(120, 32)).unwrap();
        term.draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let modal = app
            .orch_hits
            .iter()
            .find_map(|(hit, rect)| matches!(hit, OrchHit::FormModal).then_some(*rect))
            .expect("new-task modal surface is published");

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            (modal.x, modal.y),
            KeyModifiers::NONE,
        ));
        assert!(
            app.orch_form.is_some(),
            "a click inside keeps the form open"
        );

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            (modal.x.saturating_sub(1), modal.y),
            KeyModifiers::NONE,
        ));
        assert!(
            app.orch_form.is_none(),
            "a click on the dimmed backdrop closes the form"
        );
    }

    #[test]
    fn narrow_orchestration_double_click_also_jumps_to_the_worker() {
        let _env = crate::persist::test_env("orch-narrow-details");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 28, tx).unwrap();
        app.orch
            .add_task("narrow task".into(), vec![], vec![], None)
            .unwrap();
        let worker = *app.panes.keys().next().unwrap();
        app.orch.claim("t1", worker.0).unwrap();
        app.open_orch_board();
        let mut term = Terminal::new(TestBackend::new(80, 28)).unwrap();
        term.draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let row = app
            .orch_hits
            .iter()
            .find_map(|(hit, rect)| matches!(hit, OrchHit::Task(id) if id == "t1").then_some(*rect))
            .expect("task row is clickable");
        let at = (row.x + 1, row.y);

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));

        assert!(!app.active_is_orch());
        assert_eq!(app.layout().focus, worker);
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

    /// A fixture whose visible label and OSC 8 target intentionally differ.
    fn fixture_showing_osc8(
        label: &str,
        uri: &str,
        at: u16,
    ) -> (App, Terminal<TestBackend>, (u16, u16)) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let pane = app.layout().focus;
        let sequence = format!("\x1b[H\x1b[2J\x1b]8;id=agent;{uri}\x1b\\{label}\x1b]8;;\x1b\\\r\n");
        app.panes
            .get(&pane)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(sequence.as_bytes());
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, r)| *r)
            .expect("pane content rect");
        (app, term, (content.x + at, content.y))
    }

    fn double_click(app: &mut App, at: (u16, u16)) {
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
    }

    /// `Ctrl`+click a path, then return the view it landed on. Tests run from
    /// the repo root, so `Cargo.toml` is a real relative path from the pane's
    /// working directory.
    fn click_cargo_toml(mods: KeyModifiers) -> (App, crate::ids::PaneId, usize) {
        let (mut app, _t, at) = fixture_showing("edit Cargo.toml now", 7);
        let tabs = app.ws().tabs.len();
        app.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), at, mods));
        app.handle_event(mouse(MouseEventKind::Up(MouseButton::Left), at, mods));
        assert!(
            app.pending_open_url.is_none(),
            "a file never goes to the browser"
        );
        let id = app.layout().focus;
        match app.views.get(&id) {
            Some(crate::app::ViewKind::File(v)) => {
                assert!(v.path.ends_with("Cargo.toml"), "showing {:?}", v.path)
            }
            _ => panic!("the focus is a file view"),
        }
        (app, id, tabs)
    }

    /// A path an agent printed opens **in luvus**, not at the OS — and where
    /// `File click behavior` says, which by default is the preview pane beside
    /// the pane that printed it, so the agent stays on screen.
    #[test]
    fn ctrl_click_on_a_file_path_previews_it_beside_the_pane() {
        let _env = crate::persist::test_env("link-file");
        let (app, id, tabs) = click_cargo_toml(KeyModifiers::CONTROL);
        assert_eq!(app.ws().tabs.len(), tabs, "no new tab");
        assert!(app.preview_views.contains(&id), "it is the preview pane");
    }

    #[test]
    fn osc8_file_target_overrides_a_domain_shaped_label() {
        let _env = crate::persist::test_env("link-osc8-file");
        let path = std::env::current_dir().unwrap().join("Cargo.toml");
        let uri = format!("file://{}", path.display());
        let (app, _term, at) = fixture_showing_osc8("luvus.dev", &uri, 2);

        match app.link_at_screen(at.0, at.1).map(|hover| hover.target) {
            Some(LinkTarget::File {
                path: target,
                line: None,
            }) => assert_eq!(target, path),
            other => panic!("OSC 8 file target must win over its label, got {other:?}"),
        }
        assert!(app.pending_open_url.is_none());
    }

    #[test]
    fn osc8_file_label_preserves_a_visible_line_number() {
        let _env = crate::persist::test_env("link-osc8-line");
        let path = std::env::current_dir().unwrap().join("Cargo.toml");
        let uri = format!("file://{}", path.display());
        let (app, _term, at) = fixture_showing_osc8("Cargo.toml:42", &uri, 3);

        assert!(matches!(
            app.link_at_screen(at.0, at.1).map(|hover| hover.target),
            Some(LinkTarget::File {
                path: target,
                line: Some(42)
            }) if target == path
        ));
    }

    #[test]
    fn unsupported_osc8_target_is_inert_without_label_fallback() {
        let _env = crate::persist::test_env("link-osc8-inert");
        let (app, _term, at) = fixture_showing_osc8("luvus.dev", "vscode://file/repo/main.rs", 2);

        assert_eq!(app.link_at_screen(at.0, at.1), None);
    }

    #[test]
    fn http_osc8_target_opens_even_when_its_label_looks_like_a_file() {
        let _env = crate::persist::test_env("link-osc8-http");
        let (app, _term, at) = fixture_showing_osc8("Cargo.toml", "https://example.com/actual", 2);

        assert_eq!(
            app.link_at_screen(at.0, at.1).map(|hover| hover.target),
            Some(LinkTarget::Url("https://example.com/actual".into()))
        );
    }

    /// `Open in tab` is the other click behavior, and the only placement that
    /// can reach a configured editor: the tab path is unchanged for it.
    #[test]
    fn ctrl_click_on_a_file_path_opens_a_tab_when_that_is_the_click_behavior() {
        let _env = crate::persist::test_env("link-file-tab");
        let (mut app, _t, at) = fixture_showing("edit Cargo.toml now", 7);
        app.config.layout.file_click = crate::config::FILE_CLICK_TAB.to_string();
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
        assert_eq!(app.ws().tabs.len(), tabs + 1, "opened in a new tab");
        let id = app.layout().focus;
        assert!(
            matches!(app.views.get(&id), Some(crate::app::ViewKind::File(v)) if v.path.ends_with("Cargo.toml")),
            "the new tab holds the file"
        );
        assert!(!app.preview_views.contains(&id), "a tab is permanent");
    }

    /// `Ctrl`+`Shift`+click is the tree's `Shift`+click: a permanent pane
    /// beside the focus, whatever the click behavior is set to.
    #[test]
    fn ctrl_shift_click_on_a_file_path_opens_a_permanent_pane_beside() {
        let _env = crate::persist::test_env("link-file-beside");
        let (app, id, tabs) = click_cargo_toml(KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(app.ws().tabs.len(), tabs, "no new tab");
        assert!(!app.preview_views.contains(&id), "not the recycled preview");
        assert!(
            app.layout().leaves().len() >= 2,
            "split beside the pane that printed the path"
        );
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
        let rows =
            crate::terminal::vt::AlignedRows::from_rows(vec!["  foo(bar) baz  ".to_string()]);
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
    fn word_at_grid_keeps_wide_glyph_cells_in_one_word() {
        let spacer = crate::terminal::vt::ALIGNED_WIDE_CELL;
        let rows = crate::terminal::vt::AlignedRows::from_rows(vec![format!(
            "你{spacer}好{spacer} code 编{spacer}码{spacer}42"
        )]);

        for col in 0..4 {
            assert_eq!(
                word_at_grid(&rows, col, 0),
                Some(("你好".to_string(), vec![(0, 0, 4)])),
                "either cell of each CJK glyph selects the complete word at col {col}"
            );
        }
        for col in 10..16 {
            assert_eq!(
                word_at_grid(&rows, col, 0),
                Some(("编码42".to_string(), vec![(0, 10, 16)])),
                "mixed wide and narrow characters remain one word at col {col}"
            );
        }
    }

    #[test]
    fn copy_link_at_grid_keeps_unicode_paths_complete() {
        let spacer = crate::terminal::vt::ALIGNED_WIDE_CELL;
        let rows = crate::terminal::vt::AlignedRows::from_rows(vec![format!(
            "dir/日{spacer}本{spacer}語{spacer}.rs next"
        )]);

        for col in 0..13 {
            assert_eq!(
                copy_link_at_grid(&rows, col, 0),
                Some(("dir/日本語.rs".to_string(), vec![(0, 0, 13)])),
                "the complete Unicode path is copied from physical column {col}"
            );
        }
    }

    #[test]
    fn mouse_drag_copy_preserves_selected_terminal_cells() {
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
                " Morning arrives without ceremony,\n a thin gold line on the edge of the glass.\n The kettle speaks in its private language,"
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
                " Morning arrives without ceremony,\n a thin gold line on the edge of the glass.\n The kettle speaks in its private language,"
            )
        );
        assert!(
            app.selection.is_some(),
            "the copied drag keeps its highlight briefly"
        );
        assert!(
            !app.tick_copy_highlight(Instant::now()),
            "the highlight stays until the toast cadence elapses"
        );
        assert!(app.selection.is_some());
        assert!(
            app.tick_copy_highlight(Instant::now() + COPY_HIGHLIGHT_DURATION),
            "the highlight clears once the timer expires"
        );
        assert!(app.selection.is_none());
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

    /// A title-strip click must never arm the double-click detector. Otherwise a
    /// following body click one row down is read as a double-click, copies the
    /// token under it, and `return`s before the focus cascade — the exact shape
    /// that failed Linux CI in `stacked_bottom_pane_title_zoom_and_body_are_all_clickable`.
    /// Here it is pinned deterministically by putting a token under the body cell,
    /// so a regressed detector copies on every platform instead of only where the
    /// body cell happens to be non-empty.
    #[test]
    fn a_title_click_then_a_body_click_focuses_the_pane_not_a_double_click() {
        let _env = crate::persist::test_env("title-then-body-focus");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let top = app.layout().focus;
        app.run_cmd(crate::app::keys::Cmd::SplitDown);
        let bottom = app.layout().focus;
        assert_ne!(top, bottom);
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        // A token under the bottom pane's body: a false double-click would copy it.
        app.panes
            .get(&bottom)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(b"\x1b[H\x1b[2Jhello");
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let (_, title) = *app
            .pane_title_rects
            .iter()
            .max_by_key(|(_, r)| r.y)
            .expect("bottom pane has a title strip");
        let body = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == bottom)
            .map(|(_, r)| *r)
            .expect("bottom pane has a content rect");
        // Same column, and the body sits one row under the title, so a detector
        // that armed on the title would read the body click as a double-click.
        let col = body.x + 1;
        assert!(
            body.y.abs_diff(title.y) <= 1,
            "body is adjacent to the title"
        );

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            (col, title.y),
            KeyModifiers::NONE,
        ));
        assert!(
            app.cmd_inspect.is_none(),
            "the title click did not open the command overlay"
        );
        assert_eq!(
            app.layout().focus,
            bottom,
            "the title click focused the pane"
        );
        // Reset focus *after* the title click, so only the body click can move it.
        app.layout_mut().focus = top;
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            (col, body.y),
            KeyModifiers::NONE,
        ));

        assert_eq!(
            app.layout().focus,
            bottom,
            "the body click focused the pane instead of being a double-click"
        );
        assert!(
            app.pending_clipboard.is_none(),
            "the body click did not copy — it was not a double-click"
        );
    }

    /// A real timed double-click (press, release, press) copies the word under the
    /// cursor; a lone press first copies nothing.
    #[test]
    fn a_double_click_copies_the_word_under_the_cursor() {
        let _env = crate::persist::test_env("double-click-copy");
        // Click the second word, well clear of the sidebar-resize divider at the
        // pane's left edge (a press there grabs the divider, not the grid).
        let (mut app, _t, at) = fixture_showing("hello world", 6);

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert!(
            app.pending_clipboard.is_none(),
            "one press and release copies nothing"
        );

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert_eq!(
            app.pending_clipboard.as_deref(),
            Some("world"),
            "the second press copies the whitespace word"
        );
        assert!(app.selection.is_some(), "and highlights it");
        let clear_at = app
            .selection_clear_at
            .expect("the second press schedules highlight expiry");

        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        assert!(!app.dbl_click_release, "release closes the gesture");
        assert_eq!(
            app.selection_clear_at,
            Some(clear_at),
            "release does not restart the press-time expiry"
        );
        assert!(app.tick_copy_highlight(clear_at));
        assert!(app.selection.is_none(), "the press-time deadline clears it");
    }

    #[test]
    fn a_new_left_press_clears_a_copied_highlight_before_overlay_handling() {
        let _env = crate::persist::test_env("copy-highlight-overlay-click");
        let (mut app, _t, at) = fixture_showing("hello world", 6);
        let end = (at.0 + 4, at.1);

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            end,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            end,
            KeyModifiers::NONE,
        ));
        assert!(app.selection.is_some(), "the copied drag is highlighted");
        assert!(
            app.selection_clear_at.is_some(),
            "the copied drag has a pending expiry"
        );

        // The help overlay returns near the start of `apply_mouse`. Its click
        // must still replace the delayed terminal highlight immediately.
        app.help_open = true;
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            (0, 0),
            KeyModifiers::NONE,
        ));
        assert!(!app.help_open, "the overlay handled the click");
        assert!(app.selection.is_none(), "the old highlight cleared first");
        assert!(
            app.selection_clear_at.is_none(),
            "its obsolete timer cleared with it"
        );
    }

    #[test]
    fn a_double_click_copies_a_word_from_a_native_file_view() {
        let _env = crate::persist::test_env("double-click-file-view");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let pane = app.layout().focus;
        let mut view = crate::files::FileView::new(PathBuf::from("sample.txt"));
        view.wrap = false;
        view.apply(crate::files::FileLoad::Text(vec![
            "你好 file-view world".into()
        ]));
        app.panes.remove(&pane);
        app.views.insert(pane, crate::app::ViewKind::File(view));

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("file content rect");
        let text_x = content.x + crate::files::gutter_width(1) + 1;

        // Two wide glyphs occupy four cells before the separating space.
        double_click(&mut app, (text_x + 6, content.y));

        assert_eq!(app.pending_clipboard.as_deref(), Some("file-view"));
        assert!(
            app.selection.is_some(),
            "the copied file word is highlighted"
        );
    }

    #[test]
    fn a_double_click_copies_a_word_from_a_document_preview() {
        use crate::files::preview::{
            layout, Block, DocumentView, LayoutKey, PreviewDocument, PreviewKind, PreviewLoad,
        };

        let _env = crate::persist::test_env("double-click-document-preview");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let pane = app.layout().focus;
        let source = Arc::<str>::from("hello rendered-preview world");
        let document = Arc::new(PreviewDocument::new(
            Arc::clone(&source),
            vec![Block::Code {
                language: None,
                text: source.to_string(),
                range: 0..source.len(),
            }],
        ));
        let mut view = DocumentView::new(PathBuf::from("README.md"), PreviewKind::Markdown);
        view.apply(PreviewLoad::Ready(Arc::clone(&document)));
        app.panes.remove(&pane);
        app.views.insert(pane, crate::app::ViewKind::Preview(view));

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("preview content rect");
        let key = LayoutKey {
            width: content.width,
            ascii: false,
        };
        if let Some(crate::app::ViewKind::Preview(view)) = app.views.get_mut(&pane) {
            view.apply_layout(key, Arc::new(layout::build(document, key)));
        }
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();

        // Code preview rows have a two-cell presentation indent.
        double_click(&mut app, (content.x + 2 + 7, content.y));

        assert_eq!(app.pending_clipboard.as_deref(), Some("rendered-preview"));
        assert!(
            app.selection.is_some(),
            "the copied preview word is highlighted"
        );
    }

    /// The wide-character case for copying: a CJK glyph before a token shifts every
    /// following terminal column by its spacer cell, so both the copied text and
    /// the highlight must be addressed by cell column, not string index.
    #[test]
    fn a_double_click_copies_a_token_after_wide_characters() {
        let _env = crate::persist::test_env("double-click-copy-wide");
        // 你(cols 0-1) 好(cols 2-3) space(col 4), then src/main.rs at cols 5..16.
        let (mut app, _t, at) = fixture_showing("你好 src/main.rs", 5);

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));

        assert_eq!(
            app.pending_clipboard.as_deref(),
            Some("src/main.rs"),
            "the whole path copies, not a wide-char-shifted fragment"
        );
        let sel = app.selection.expect("the copied path is highlighted");
        assert_eq!(
            (sel.anchor, sel.cursor),
            ((at.0, at.1), (at.0 + 10, at.1)),
            "the highlight covers the path's true cells (cols 5..16)"
        );
    }

    /// Double-clicking either terminal cell of a wide glyph copies its complete
    /// whitespace-delimited word, without leaking the internal cell marker.
    #[test]
    fn a_double_click_copies_a_wide_character_word() {
        let _env = crate::persist::test_env("double-click-copy-wide-word");
        // Click the continuation cell of `你`; `你好` occupies cols 0..4.
        let (mut app, _t, at) = fixture_showing("你好 next", 1);

        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));
        app.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            at,
            KeyModifiers::NONE,
        ));

        assert_eq!(app.pending_clipboard.as_deref(), Some("你好"));
        let sel = app.selection.expect("the full wide word is highlighted");
        assert_eq!(
            (sel.anchor, sel.cursor),
            ((at.0 - 1, at.1), (at.0 + 2, at.1))
        );
    }

    /// Zero-width grapheme components live on the base terminal cell. Copying
    /// must retain them without letting them shift cell-coordinate lookup.
    #[test]
    fn a_double_click_preserves_complete_graphemes() {
        let _env = crate::persist::test_env("double-click-copy-graphemes");
        for (shown, expected) in [
            ("go 👩‍💻 next", "👩‍💻"),
            ("go 🖥️ next", "🖥️"),
            ("go e\u{301}lan next", "e\u{301}lan"),
        ] {
            // Keep the click clear of the pane-resize target at the left edge.
            let (mut app, _t, at) = fixture_showing(shown, 3);
            app.handle_event(mouse(
                MouseEventKind::Down(MouseButton::Left),
                at,
                KeyModifiers::NONE,
            ));
            app.handle_event(mouse(
                MouseEventKind::Up(MouseButton::Left),
                at,
                KeyModifiers::NONE,
            ));
            app.handle_event(mouse(
                MouseEventKind::Down(MouseButton::Left),
                at,
                KeyModifiers::NONE,
            ));

            assert_eq!(
                app.pending_clipboard.as_deref(),
                Some(expected),
                "{shown:?}"
            );
        }
    }

    /// The wide-character case for Ctrl-hover/Ctrl-click link resolution, which
    /// shares the same `visible_rows` indexing: a CJK glyph before a path must not
    /// shift where the underline lands or which cells open.
    #[test]
    fn ctrl_hover_after_wide_characters_underlines_the_real_cells() {
        let _env = crate::persist::test_env("ctrl-hover-wide");
        // 你好 then a real on-disk path: Cargo.toml occupies grid cols 5..15.
        let (app, _t, at) = fixture_showing("你好 Cargo.toml", 5);

        let h = app
            .link_at_screen(at.0, at.1)
            .expect("the path after the CJK glyphs resolves");
        assert!(
            matches!(&h.target, LinkTarget::File { .. }),
            "got {:?}",
            h.target
        );
        assert!(
            h.link.covers(5, 0),
            "underline starts at the path's true column"
        );
        assert!(h.link.covers(14, 0), "and reaches its end");
        assert!(!h.link.covers(4, 0), "not the space before it");
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
    fn mouse_copy_is_independent_of_detected_agent() {
        let _env = crate::persist::test_env("mouse-copy-agent-independent");
        let (mut app, _term, _) = fixture_showing("  hello", 0);
        let pane = app.layout().focus;
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
            .expect("pane content rect");
        app.selection = Some(crate::app::Selection {
            pane,
            content,
            anchor: (content.x + 1, content.y),
            cursor: (content.x + 6, content.y),
            retained: None,
            scrolled: false,
            dragging: false,
        });

        let shell = app.selection_text();
        app.status.get_mut(&pane).expect("pane status").agent = "codex".into();
        assert_eq!(app.selection_text(), shell);
        assert_eq!(shell.as_deref(), Some(" hello"));
    }
}
