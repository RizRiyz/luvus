//! Dependency-free global fuzzy finder (docs/90).
//!
//! Small navigation metadata is ranked immediately. Complete file-path
//! catalogs and retained terminal output are scored on workers and merged by a
//! query generation, so typing never scans scrollback on the app loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use super::App;
use crate::ids::PaneId;
use crate::search::{
    rank_entries, rank_entry_refs_where, FuzzyQuery, SearchEntry, SearchKind, SearchMatch,
    SearchScope, SearchTarget, OUTPUT_INDEX_BYTES, OUTPUT_PER_PANE_CAP, OUTPUT_ROW_CAP,
    OUTPUT_SOURCE_BYTES, RESULT_CAP,
};

static NEXT_SEARCH_INSTANCE: AtomicU64 = AtomicU64::new(1);

pub struct SearchFlash {
    pub pane: PaneId,
    pub row: u16,
    pub scroll: usize,
    pub until: std::time::Instant,
}

pub struct LegacySearchHit {
    pub pane: PaneId,
    pub ws: usize,
    pub ws_name: String,
    pub offset: usize,
    pub line: String,
    pub col: usize,
}

struct OutputTarget {
    pane: PaneId,
    detail: Arc<str>,
    workspace: Arc<str>,
    engine: Arc<Mutex<dyn crate::terminal::vt::VtEngine>>,
}

struct SearchRequest {
    generation: u64,
    query: String,
    case_sensitive: bool,
    scope: SearchScope,
    files: Arc<Vec<SearchEntry>>,
}

struct FederationRequest {
    generation: u64,
    query: String,
    case_sensitive: bool,
    scope: SearchScope,
}

pub struct GlobalSearch {
    pub instance: u64,
    pub query: String,
    pub case_sensitive: bool,
    pub scope: SearchScope,
    pub results: Vec<SearchMatch>,
    pub total: usize,
    pub cursor: usize,
    pub capped: bool,
    pub loading: bool,
    pub rects: Vec<(usize, Rect)>,
    pub scope_rects: Vec<(SearchScope, Rect)>,
    generation: u64,
    metadata: Vec<SearchEntry>,
    metadata_matches: Vec<SearchMatch>,
    metadata_total: usize,
    files: Arc<Vec<SearchEntry>>,
    file_catalog_partial: bool,
    worker_matches: Vec<SearchMatch>,
    worker_total: usize,
    worker_capped: bool,
    worker: mpsc::Sender<SearchRequest>,
    federated_matches: Vec<SearchMatch>,
    federated_total: usize,
    federated_partial: bool,
    federation_loading: bool,
    federation: Option<mpsc::Sender<FederationRequest>>,
    recent_files: Vec<SearchEntry>,
}

fn tab_name(tab: &crate::app::Tab, index: usize) -> String {
    if let Some(name) = tab.name.as_deref() {
        name.to_string()
    } else if tab.is_git() {
        "git".to_string()
    } else if tab.is_orch() {
        "orchestration".to_string()
    } else if tab.is_mission() {
        "mission control".to_string()
    } else {
        format!("tab {}", index + 1)
    }
}

fn state_name(state: crate::ui::theme::State) -> &'static str {
    use crate::ui::theme::State;
    match state {
        State::Working => "working",
        State::Blocked => "blocked",
        State::Done => "done",
        State::Idle => "idle",
        State::Unknown => "unknown",
    }
}

impl App {
    fn resolve_search_tab(
        &self,
        workspace: usize,
        workspace_cwd: &std::path::Path,
        leaves: &[PaneId],
    ) -> Option<usize> {
        if self.workspaces.get(workspace)?.cwd != workspace_cwd {
            return None;
        }
        self.resolve_tab_menu_target(&super::TabMenuTarget {
            workspace,
            leaves: leaves.to_vec(),
        })
        .map(|(_, tab)| tab)
    }

    fn search_recent_files(&self) -> Vec<SearchEntry> {
        let session = crate::session::display_name();
        self.recent_files
            .iter()
            .filter_map(|(workspace_cwd, path)| {
                let ws = self
                    .workspaces
                    .iter()
                    .position(|workspace| workspace.cwd == *workspace_cwd)?;
                let workspace = &self.workspaces[ws];
                let relative = path
                    .strip_prefix(workspace_cwd)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                let label = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| relative.clone());
                Some(SearchEntry::new(
                    format!("file:{ws}:{relative}"),
                    SearchKind::File,
                    label,
                    format!("{} › {} › {}", session, workspace.name, relative),
                    [relative],
                    SearchTarget::File {
                        ws,
                        path: path.clone(),
                        workspace_cwd: workspace_cwd.clone(),
                    },
                    false,
                ))
            })
            .collect()
    }

    fn search_metadata(&self) -> Vec<SearchEntry> {
        let session = crate::session::display_name();
        let mut out = Vec::new();

        if let Ok(sessions) = crate::session::list_sessions() {
            for info in sessions {
                let current = info.name == session;
                let state = if info.running { "running" } else { "stopped" };
                out.push(SearchEntry::new(
                    format!("session:{}", info.name),
                    SearchKind::Session,
                    info.name.clone(),
                    state.into(),
                    [],
                    SearchTarget::Session {
                        name: info.name,
                        running: info.running,
                        current,
                    },
                    current,
                ));
            }
        }

        for (wi, ws) in self.workspaces.iter().enumerate() {
            out.push(SearchEntry::new(
                format!("workspace:{wi}"),
                SearchKind::Workspace,
                ws.name.clone(),
                format!("{} › {}", session, ws.cwd.display()),
                [
                    ws.cwd.to_string_lossy().into_owned(),
                    ws.branch.clone().unwrap_or_default(),
                ],
                SearchTarget::Workspace {
                    ws: wi,
                    cwd: ws.cwd.clone(),
                },
                wi == self.active_ws,
            ));
            for (ti, tab) in ws.tabs.iter().enumerate() {
                let detail = format!("{} › {} › tab {}", session, ws.name, ti + 1);
                out.push(SearchEntry::new(
                    format!("tab:{wi}:{ti}"),
                    SearchKind::Tab,
                    tab_name(tab, ti),
                    detail.clone(),
                    [ws.name.clone(), (ti + 1).to_string()],
                    SearchTarget::Tab {
                        ws: wi,
                        tab: ti,
                        workspace_cwd: ws.cwd.clone(),
                        tab_leaves: tab.layout.leaves(),
                    },
                    wi == self.active_ws && ti == ws.active_tab,
                ));
                for id in tab.layout.leaves() {
                    let Some(pane) = self.panes.get(&id) else {
                        continue;
                    };
                    let alias = self
                        .agent_names
                        .iter()
                        .find_map(|(name, pane_id)| (*pane_id == id).then(|| name.clone()));
                    let title = self
                        .status
                        .get(&id)
                        .and_then(|status| status.detected_title.as_deref())
                        .map(str::to_owned)
                        .unwrap_or_else(|| pane.command.clone());
                    let pane_detail =
                        format!("{} › {} › tab {} › pane {}", session, ws.name, ti + 1, id.0);
                    out.push(SearchEntry::new(
                        format!("pane:{}", id.0),
                        SearchKind::Pane,
                        alias.clone().unwrap_or_else(|| title.clone()),
                        pane_detail.clone(),
                        [
                            title,
                            pane.cwd.to_string_lossy().into_owned(),
                            pane.command.clone(),
                            alias.clone().unwrap_or_default(),
                        ],
                        SearchTarget::Pane { pane: id },
                        wi == self.active_ws && ti == ws.active_tab && tab.layout.focus == id,
                    ));
                    if let Some(status) = self.status.get(&id) {
                        let is_agent = self.manifests.is_agent(&status.agent)
                            || status.agent_session.is_some();
                        if is_agent {
                            out.push(SearchEntry::new(
                                format!("agent:{}", id.0),
                                SearchKind::Agent,
                                alias.clone().unwrap_or_else(|| status.agent.clone()),
                                pane_detail,
                                [status.agent.clone(), state_name(status.state).into()],
                                SearchTarget::Agent { pane: id },
                                wi == self.active_ws
                                    && ti == ws.active_tab
                                    && tab.layout.focus == id,
                            ));
                        }
                    }
                }
            }
        }
        out
    }

    /// Start a public fuzzy query without blocking the single-writer app loop.
    /// The connection's reply channel is fulfilled by this bounded worker.
    pub fn start_search_api(&self, request: crate::ipc::api::ApiRequest) {
        let query = request
            .params
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let scope = match request
            .params
            .get("scope")
            .and_then(|value| value.as_str())
            .unwrap_or("all")
        {
            "all" => SearchScope::All,
            "navigate" => SearchScope::Navigate,
            "files" => SearchScope::Files,
            "output" => SearchScope::Output,
            _ => {
                let _ = request.reply.send(api_error(
                    &request.id,
                    "scope must be all, navigate, files, or output",
                ));
                return;
            }
        };
        let limit = request
            .params
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(RESULT_CAP as u64);
        if query.is_empty() || query.len() > 256 || limit == 0 || limit > RESULT_CAP as u64 {
            let _ = request.reply.send(api_error(
                &request.id,
                "query must be 1..256 bytes and limit must be 1..200",
            ));
            return;
        }
        let case_sensitive = request
            .params
            .get("case_sensitive")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let all_sessions = request
            .params
            .get("all_sessions")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let metadata = self.search_metadata();
        let output_targets = self.search_output_targets();
        let roots: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(ws, workspace)| (ws, workspace.name.clone(), workspace.cwd.clone()))
            .collect();
        std::thread::spawn(move || {
            let fuzzy = FuzzyQuery::new(&query, false);
            let (mut matches, mut total) = rank_entries(&metadata, &fuzzy, scope, RESULT_CAP);
            let mut capped = false;
            if matches!(scope, SearchScope::All | SearchScope::Files) {
                let (files, partial) = file_entries(roots);
                let (mut found, count) = rank_entries(&files, &fuzzy, scope, RESULT_CAP);
                matches.append(&mut found);
                total = total.saturating_add(count);
                capped |= partial;
            }
            if fuzzy.char_count() >= 2 && matches!(scope, SearchScope::All | SearchScope::Output) {
                let (output, partial) = build_output_entries(output_targets);
                let output_query = FuzzyQuery::new(&query, case_sensitive);
                let (mut found, count) = rank_output(&output, &output_query, scope, RESULT_CAP);
                matches.append(&mut found);
                total = total.saturating_add(count);
                capped |= partial;
            }
            if all_sessions {
                let (sessions, session_cap) = crate::search::federation::running_sessions();
                capped |= session_cap;
                for session in sessions {
                    if !crate::search::federation::session_supports_search(&session) {
                        capped = true;
                        continue;
                    }
                    match crate::search::federation::query_session(
                        &session,
                        &query,
                        &scope.label().to_ascii_lowercase(),
                        case_sensitive,
                        RESULT_CAP,
                    ) {
                        Ok(mut result) => {
                            total = total.saturating_add(result.total);
                            capped |= result.partial;
                            matches.append(&mut result.matches);
                        }
                        Err(_) => capped = true,
                    }
                }
            }
            sort_and_cap(&mut matches);
            matches.truncate(limit as usize);
            capped |= total > matches.len();
            let results: Vec<_> = matches.iter().map(search_match_json).collect();
            let response = serde_json::json!({
                "id": request.id,
                "result": {
                    "type": "search_query",
                    "query": query,
                    "scope": scope.label().to_ascii_lowercase(),
                    "total": total,
                    "shown": results.len(),
                    "partial": capped,
                    "matches": results,
                }
            })
            .to_string();
            let _ = request.reply.send(response);
        });
    }

    fn search_output_targets(&self) -> Vec<OutputTarget> {
        let session = crate::session::display_name();
        let mut targets = Vec::new();
        for ws in &self.workspaces {
            for (ti, tab) in ws.tabs.iter().enumerate() {
                for id in tab.layout.leaves() {
                    let Some(pane) = self.panes.get(&id) else {
                        continue;
                    };
                    targets.push(OutputTarget {
                        pane: id,
                        detail: Arc::<str>::from(format!(
                            "{} › {} › tab {} › pane {}",
                            session,
                            ws.name,
                            ti + 1,
                            id.0
                        )),
                        workspace: Arc::<str>::from(ws.name.clone()),
                        engine: Arc::clone(&pane.engine),
                    });
                }
            }
        }
        targets
    }

    pub fn open_search(&mut self) {
        let instance = NEXT_SEARCH_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let recent_files = self.search_recent_files();
        let metadata = self.search_metadata();
        let output_targets = self.search_output_targets();
        let roots: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(ws, workspace)| (ws, workspace.name.clone(), workspace.cwd.clone()))
            .collect();
        let (tx, rx) = mpsc::channel::<SearchRequest>();
        let app_tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let catalogs = roots
                .into_iter()
                .map(|(ws, name, root)| {
                    let catalog = (*crate::search::files::index_cached(&root)).clone();
                    (ws, name, root, catalog)
                })
                .collect();
            let _ = app_tx.send(crate::event::AppEvent::SearchFilesIndexed { instance, catalogs });
            let (output, output_capped) = build_output_entries(output_targets);
            while let Ok(mut request) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    request = newer;
                }
                let query = FuzzyQuery::new(&request.query, false);
                let mut matches = Vec::new();
                let mut total = 0usize;
                if matches!(request.scope, SearchScope::All | SearchScope::Files) {
                    let (mut found, count) =
                        rank_entries(&request.files, &query, request.scope, RESULT_CAP);
                    total = total.saturating_add(count);
                    matches.append(&mut found);
                }
                if query.char_count() >= 2
                    && matches!(request.scope, SearchScope::All | SearchScope::Output)
                {
                    let output_query = FuzzyQuery::new(&request.query, request.case_sensitive);
                    let (mut found, count) =
                        rank_output(&output, &output_query, request.scope, RESULT_CAP);
                    total = total.saturating_add(count);
                    matches.append(&mut found);
                }
                sort_and_cap(&mut matches);
                let _ = app_tx.send(crate::event::AppEvent::SearchResults {
                    instance,
                    generation: request.generation,
                    matches,
                    total,
                    capped: output_capped,
                });
            }
        });

        let (session_names, session_list_partial) = crate::search::federation::running_sessions();
        let federation = if session_names.is_empty() {
            None
        } else {
            let (federation_tx, federation_rx) = mpsc::channel::<FederationRequest>();
            let app_tx = self.app_tx.clone();
            std::thread::spawn(move || {
                let mut partial_owner_set = session_list_partial;
                let sessions: Vec<_> = session_names
                    .into_iter()
                    .filter(|session| {
                        let supported = crate::search::federation::session_supports_search(session);
                        partial_owner_set |= !supported;
                        supported
                    })
                    .collect();
                while let Ok(mut request) = federation_rx.recv() {
                    // A short debounce prevents a slow sibling session from
                    // receiving one expensive catalog query per typed byte.
                    std::thread::sleep(std::time::Duration::from_millis(60));
                    while let Ok(newer) = federation_rx.try_recv() {
                        request = newer;
                    }
                    let mut matches = Vec::new();
                    let mut total = 0usize;
                    let mut partial = partial_owner_set;
                    for session in &sessions {
                        match crate::search::federation::query_session(
                            session,
                            &request.query,
                            &request.scope.label().to_ascii_lowercase(),
                            request.case_sensitive,
                            RESULT_CAP,
                        ) {
                            Ok(mut result) => {
                                total = total.saturating_add(result.total);
                                partial |= result.partial;
                                matches.append(&mut result.matches);
                            }
                            Err(_) => partial = true,
                        }
                    }
                    sort_and_cap(&mut matches);
                    let _ = app_tx.send(crate::event::AppEvent::SearchFederatedResults {
                        instance,
                        generation: request.generation,
                        matches,
                        total,
                        partial,
                    });
                }
            });
            Some(federation_tx)
        };

        let mut search = GlobalSearch {
            instance,
            query: String::new(),
            case_sensitive: false,
            scope: SearchScope::All,
            results: Vec::new(),
            total: 0,
            cursor: 0,
            capped: false,
            loading: true,
            rects: Vec::new(),
            scope_rects: Vec::new(),
            generation: 0,
            metadata,
            metadata_matches: Vec::new(),
            metadata_total: 0,
            files: Arc::new(Vec::new()),
            file_catalog_partial: false,
            worker_matches: Vec::new(),
            worker_total: 0,
            worker_capped: false,
            worker: tx,
            federated_matches: Vec::new(),
            federated_total: 0,
            federated_partial: session_list_partial,
            federation_loading: false,
            federation,
            recent_files,
        };
        search.recommendations();
        self.search = Some(search);
    }

    pub fn apply_search_files(
        &mut self,
        instance: u64,
        catalogs: Vec<(
            usize,
            String,
            std::path::PathBuf,
            crate::search::files::FileCatalog,
        )>,
    ) -> bool {
        let Some(search) = self.search.as_mut().filter(|s| s.instance == instance) else {
            return false;
        };
        let (entries, partial) = file_entries_from_catalogs(catalogs);
        search.files = Arc::new(entries);
        search.file_catalog_partial = partial;
        self.search_recompute();
        true
    }

    pub fn apply_search_results(
        &mut self,
        instance: u64,
        generation: u64,
        matches: Vec<SearchMatch>,
        total: usize,
        capped: bool,
    ) -> bool {
        let Some(search) = self
            .search
            .as_mut()
            .filter(|s| s.instance == instance && s.generation == generation)
        else {
            return false;
        };
        let selected = search
            .results
            .get(search.cursor)
            .map(|m| m.entry.id.clone());
        search.worker_matches = matches;
        search.worker_total = total;
        search.worker_capped = capped;
        search.loading = search.federation_loading;
        search.merge(selected.as_deref());
        true
    }

    pub fn apply_search_federated_results(
        &mut self,
        instance: u64,
        generation: u64,
        matches: Vec<SearchMatch>,
        total: usize,
        partial: bool,
    ) -> bool {
        let Some(search) = self
            .search
            .as_mut()
            .filter(|search| search.instance == instance && search.generation == generation)
        else {
            return false;
        };
        let selected = search
            .results
            .get(search.cursor)
            .map(|result| result.entry.id.clone());
        search.federated_matches = matches;
        search.federated_total = total;
        search.federated_partial = partial;
        search.federation_loading = false;
        search.loading = false;
        search.merge(selected.as_deref());
        true
    }

    pub fn close_search(&mut self) {
        self.search = None;
    }

    pub fn toggle_search(&mut self) {
        if self.search.is_some() {
            self.close_search();
        } else {
            self.open_search();
        }
    }

    /// Preserve the exact-scrollback CLI/API contract while the interactive
    /// overlay uses the fuzzy worker.
    pub fn search_all(&self, query: &str, case_sensitive: bool) -> (Vec<LegacySearchHit>, usize) {
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        if needle.is_empty() {
            return (Vec::new(), 0);
        }
        let mut hits = Vec::new();
        let mut total = 0usize;
        for (wi, ws) in self.workspaces.iter().enumerate() {
            for tab in &ws.tabs {
                for id in tab.layout.leaves() {
                    let Some(pane) = self.panes.get(&id) else {
                        continue;
                    };
                    let mut pane_hits = 0usize;
                    pane.for_each_retained_row(&mut |row, history, _row_count, line| {
                        let folded;
                        let haystack = if case_sensitive {
                            line
                        } else {
                            folded = line.to_lowercase();
                            &folded
                        };
                        let Some(col) = haystack.find(&needle) else {
                            return;
                        };
                        total = total.saturating_add(1);
                        if pane_hits >= 50 || hits.len() >= 500 {
                            return;
                        }
                        pane_hits += 1;
                        hits.push(LegacySearchHit {
                            pane: id,
                            ws: wi,
                            ws_name: ws.name.clone(),
                            offset: history.saturating_sub(row),
                            line: line.to_string(),
                            col,
                        });
                    });
                }
            }
        }
        (hits, total)
    }

    fn search_recompute(&mut self) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        search.generation = search.generation.wrapping_add(1);
        search.worker_matches.clear();
        search.worker_total = 0;
        search.worker_capped = false;
        search.federated_matches.clear();
        search.federated_total = 0;
        search.federation_loading = false;
        if search.query.trim().is_empty() {
            search.loading = false;
            search.recommendations();
            return;
        }
        let query = FuzzyQuery::new(&search.query, false);
        let (metadata, total) = rank_entries(&search.metadata, &query, search.scope, RESULT_CAP);
        search.metadata_matches = metadata;
        search.metadata_total = total;
        search.loading = true;
        search.federation_loading = search.federation.is_some();
        search.merge(None);
        let _ = search.worker.send(SearchRequest {
            generation: search.generation,
            query: search.query.clone(),
            case_sensitive: search.case_sensitive,
            scope: search.scope,
            files: Arc::clone(&search.files),
        });
        if let Some(worker) = &search.federation {
            let _ = worker.send(FederationRequest {
                generation: search.generation,
                query: search.query.clone(),
                case_sensitive: search.case_sensitive,
                scope: search.scope,
            });
        }
    }

    pub fn search_move(&mut self, delta: i32) {
        if let Some(search) = self.search.as_mut() {
            if search.results.is_empty() {
                search.cursor = 0;
                return;
            }
            search.cursor =
                (search.cursor as i32 + delta).clamp(0, search.results.len() as i32 - 1) as usize;
        }
    }

    pub fn search_set_scope(&mut self, scope: SearchScope) {
        if let Some(search) = self.search.as_mut() {
            search.scope = scope;
            search.cursor = 0;
        }
        self.search_recompute();
    }

    pub fn search_activate(&mut self) {
        let selected = self
            .search
            .as_ref()
            .and_then(|search| search.results.get(search.cursor))
            .map(|result| result.entry.target.clone());
        self.close_search();
        let Some(target) = selected else {
            return;
        };
        match target {
            SearchTarget::Session { name, current, .. } => {
                if !current {
                    self.pending_session_switch = Some(name);
                }
            }
            SearchTarget::Workspace { ws, cwd } => {
                if self
                    .workspaces
                    .get(ws)
                    .is_some_and(|workspace| workspace.cwd == cwd)
                {
                    self.active_ws = ws;
                }
            }
            SearchTarget::Tab {
                ws,
                tab: _,
                workspace_cwd,
                tab_leaves,
            } => {
                if let Some(tab) = self.resolve_search_tab(ws, &workspace_cwd, &tab_leaves) {
                    self.active_ws = ws;
                    self.workspaces[ws].active_tab = tab;
                }
            }
            SearchTarget::Pane { pane } | SearchTarget::Agent { pane } => {
                self.focus_pane_global(pane)
            }
            SearchTarget::File {
                ws,
                path,
                workspace_cwd,
            } => {
                if self
                    .workspaces
                    .get(ws)
                    .is_some_and(|workspace| workspace.cwd == workspace_cwd)
                {
                    self.active_ws = ws;
                    self.open_file_search_result(path);
                }
            }
            SearchTarget::Output {
                pane,
                row,
                offset,
                above,
                line,
            } => self.activate_output(pane, row, offset, above, &line),
            SearchTarget::Remote {
                session,
                kind,
                target,
            } => {
                let tx = self.app_tx.clone();
                std::thread::spawn(move || {
                    let result =
                        crate::search::federation::activate_session(&session, kind, target);
                    let _ = tx.send(crate::event::AppEvent::SearchHandoffReady { session, result });
                });
            }
        }
    }

    /// Revalidate and activate one structured result returned by this session's
    /// `search.query`. Used only by another owner before a client handoff.
    pub fn handle_search_activate(&mut self, request: &crate::ipc::api::ApiRequest) -> String {
        let result = (|| -> Result<(), String> {
            let kind = request
                .params
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "kind is required".to_string())?;
            let target = request
                .params
                .get("target")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| "target must be an object".to_string())?;
            let workspace = || {
                let ws = target
                    .get("workspace")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value as usize)
                    .ok_or_else(|| "target.workspace is required".to_string())?;
                let expected = target
                    .get("workspace_path")
                    .and_then(serde_json::Value::as_str)
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| "target.workspace_path is required".to_string())?;
                self.workspaces
                    .get(ws)
                    .filter(|workspace| workspace.cwd == expected)
                    .map(|_| ws)
                    .ok_or_else(|| "target workspace no longer exists".to_string())
            };
            let pane = || {
                target
                    .get("pane")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| value.parse::<u32>().ok())
                    .map(PaneId)
                    .filter(|pane| self.panes.contains_key(pane))
                    .ok_or_else(|| "target pane no longer exists".to_string())
            };
            match kind {
                "folder" => {
                    let ws = workspace()?;
                    self.active_ws = ws;
                }
                "tab" => {
                    let ws = workspace()?;
                    // Keep accepting the display position for schema clarity,
                    // but resolve the live tab from its stable leaf snapshot.
                    let _tab = target
                        .get("tab")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .and_then(|value| value.checked_sub(1))
                        .ok_or_else(|| "target.tab must be a 1-based index".to_string())?;
                    let leaves = target
                        .get("tab_panes")
                        .and_then(serde_json::Value::as_array)
                        .ok_or_else(|| "target.tab_panes must be an array".to_string())?
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .and_then(|value| value.parse::<u32>().ok())
                                .map(PaneId)
                                .ok_or_else(|| "target.tab_panes must contain pane IDs".to_string())
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let workspace_cwd = self.workspaces[ws].cwd.clone();
                    let tab = self
                        .resolve_search_tab(ws, &workspace_cwd, &leaves)
                        .ok_or_else(|| "target tab no longer exists".to_string())?;
                    self.active_ws = ws;
                    self.workspaces[ws].active_tab = tab;
                }
                "pane" | "agent" => self.focus_pane_global(pane()?),
                "file" => {
                    let ws = workspace()?;
                    let path = target
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| "target.path is required".to_string())?;
                    let root = self
                        .workspaces
                        .get(ws)
                        .map(|workspace| workspace.cwd.clone())
                        .ok_or_else(|| "target workspace no longer exists".to_string())?;
                    let canonical_root = root
                        .canonicalize()
                        .map_err(|_| "workspace path is unavailable".to_string())?;
                    let canonical_path = path
                        .canonicalize()
                        .map_err(|_| "target file no longer exists".to_string())?;
                    if !canonical_path.starts_with(&canonical_root) || !canonical_path.is_file() {
                        return Err("target file is outside its workspace".to_string());
                    }
                    self.active_ws = ws;
                    self.open_file_search_result(canonical_path);
                }
                "output" => {
                    let pane = pane()?;
                    let row = target
                        .get("row")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0) as usize;
                    let offset = target
                        .get("line_offset")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0) as usize;
                    let above = target
                        .get("above")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0) as usize;
                    let line = target
                        .get("line")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| "target output line is required".to_string())?;
                    self.activate_output(pane, row, offset, above, line);
                }
                _ => return Err("kind must be folder, tab, pane, agent, file, or output".into()),
            }
            Ok(())
        })();
        match result {
            Ok(()) => serde_json::json!({
                "id": request.id,
                "result": { "type": "search_activation", "activated": true }
            })
            .to_string(),
            Err(message) => api_error(&request.id, &message),
        }
    }

    fn activate_output(
        &mut self,
        pane_id: PaneId,
        old_row: usize,
        fallback_offset: usize,
        fallback_above: usize,
        line: &str,
    ) {
        self.focus_pane_global(pane_id);
        let (offset, above) = match self.panes.get(&pane_id) {
            Some(pane) => {
                let mut closest = None;
                let mut distance = usize::MAX;
                let mut retained = 0;
                let mut total_rows = 0;
                pane.for_each_retained_row(&mut |row, history, row_count, candidate| {
                    retained = history;
                    total_rows = row_count;
                    if candidate == line && row.abs_diff(old_row) < distance {
                        closest = Some(row);
                        distance = row.abs_diff(old_row);
                    }
                });
                closest.map_or((fallback_offset, fallback_above), |row| {
                    (
                        retained.saturating_sub(row),
                        total_rows.saturating_sub(1).saturating_sub(row),
                    )
                })
            }
            None => return,
        };
        if let Some(pane) = self.panes.get(&pane_id) {
            pane.scroll_to(offset);
        }
        let height = self
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, rect)| rect.height);
        if let Some(height) = height.filter(|height| *height > 0) {
            let row = (height as i32 - 1) - (above as i32 - offset as i32);
            if (0..height as i32).contains(&row) {
                self.search_flash = Some(SearchFlash {
                    pane: pane_id,
                    row: row as u16,
                    scroll: offset,
                    until: std::time::Instant::now() + std::time::Duration::from_secs(60),
                });
            }
        }
    }

    pub fn search_click(&mut self, col: u16, row: u16) {
        let scope = self.search.as_ref().and_then(|search| {
            search
                .scope_rects
                .iter()
                .find(|(_, rect)| {
                    col >= rect.x && col < rect.right() && row >= rect.y && row < rect.bottom()
                })
                .map(|(scope, _)| *scope)
        });
        if let Some(scope) = scope {
            self.search_set_scope(scope);
            return;
        }
        let hit = self.search.as_ref().and_then(|search| {
            search
                .rects
                .iter()
                .find(|(_, rect)| {
                    col >= rect.x && col < rect.right() && row >= rect.y && row < rect.bottom()
                })
                .map(|(index, _)| *index)
        });
        match hit {
            Some(index) => {
                if let Some(search) = self.search.as_mut() {
                    search.cursor = index;
                }
                self.search_activate();
            }
            None => self.close_search(),
        }
    }

    pub fn search_key(&mut self, key: KeyEvent) {
        let ctrl = super::keys::is_ctrl_chord(key.modifiers); // not AltGr
        match key.code {
            KeyCode::Esc => {
                if self.search.as_ref().is_some_and(|s| !s.query.is_empty()) {
                    if let Some(search) = self.search.as_mut() {
                        search.query.clear();
                    }
                    self.search_recompute();
                } else {
                    self.close_search();
                }
            }
            KeyCode::Enter => self.search_activate(),
            KeyCode::Up => self.search_move(-1),
            KeyCode::Down => self.search_move(1),
            KeyCode::PageUp => self.search_move(-10),
            KeyCode::PageDown => self.search_move(10),
            KeyCode::Char('p') if ctrl => self.search_move(-1),
            KeyCode::Char('n') if ctrl => self.search_move(1),
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some(scope) = self.search.as_ref().map(|s| s.scope.previous()) {
                    self.search_set_scope(scope);
                }
            }
            KeyCode::Tab => {
                if let Some(scope) = self.search.as_ref().map(|s| s.scope.next()) {
                    self.search_set_scope(scope);
                }
            }
            KeyCode::Char('i') if ctrl => {
                if let Some(search) = self
                    .search
                    .as_mut()
                    .filter(|search| matches!(search.scope, SearchScope::All | SearchScope::Output))
                {
                    search.case_sensitive = !search.case_sensitive;
                    self.search_recompute();
                }
            }
            KeyCode::Backspace => {
                if let Some(search) = self.search.as_mut() {
                    search.query.pop();
                }
                self.search_recompute();
            }
            KeyCode::Char(ch) if !ctrl => {
                if let Some(search) = self.search.as_mut() {
                    search.query.push(ch);
                }
                self.search_recompute();
            }
            _ => {}
        }
    }

    /// Append pasted text to the fuzzy query and recompute once. Replaying a
    /// paste as keys would rank metadata and queue worker requests per character.
    pub fn search_paste(&mut self, pasted: &str) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let previous_len = search.query.len();
        search
            .query
            .extend(pasted.chars().filter(|character| !character.is_control()));
        if search.query.len() != previous_len {
            self.search_recompute();
        }
    }
}

impl GlobalSearch {
    fn recommendations(&mut self) {
        let mut recommendations: Vec<_> = self
            .metadata
            .iter()
            .filter(|entry| self.scope.includes(entry.kind))
            .filter(|entry| {
                entry.active || matches!(entry.kind, SearchKind::Session | SearchKind::Agent)
            })
            .take(RESULT_CAP)
            .cloned()
            .map(|entry| SearchMatch {
                score: entry.kind.priority() + if entry.active { 15 } else { 0 },
                entry,
                label_positions: Vec::new(),
            })
            .collect();
        recommendations.extend(
            self.recent_files
                .iter()
                .filter(|entry| self.scope.includes(entry.kind))
                .cloned()
                .enumerate()
                .map(|(index, entry)| SearchMatch {
                    score: entry.kind.priority() + 12i64.saturating_sub(index as i64),
                    entry,
                    label_positions: Vec::new(),
                }),
        );
        recommendations.truncate(RESULT_CAP);
        self.metadata_matches = recommendations;
        self.metadata_total = self.metadata_matches.len();
        self.worker_matches.clear();
        self.worker_total = 0;
        self.worker_capped = false;
        self.federated_matches.clear();
        self.federated_total = 0;
        self.federation_loading = false;
        self.merge(None);
    }

    fn merge(&mut self, selected: Option<&str>) {
        self.results.clear();
        self.results.extend(self.metadata_matches.iter().cloned());
        self.results.extend(self.worker_matches.iter().cloned());
        self.results.extend(self.federated_matches.iter().cloned());
        sort_and_cap(&mut self.results);
        self.total = self
            .metadata_total
            .saturating_add(self.worker_total)
            .saturating_add(self.federated_total);
        self.capped = self.file_catalog_partial
            || self.worker_capped
            || self.federated_partial
            || self.total > self.results.len();
        if let Some(id) = selected {
            if let Some(index) = self.results.iter().position(|result| result.entry.id == id) {
                self.cursor = index;
                return;
            }
        }
        self.cursor = self.cursor.min(self.results.len().saturating_sub(1));
    }
}

fn rank_output(
    entries: &[SearchEntry],
    query: &FuzzyQuery,
    scope: SearchScope,
    cap: usize,
) -> (Vec<SearchMatch>, usize) {
    let mut by_pane = std::collections::HashMap::<PaneId, Vec<&SearchEntry>>::new();
    for entry in entries {
        if let SearchTarget::Output { pane, .. } = &entry.target {
            by_pane.entry(*pane).or_default().push(entry);
        }
    }
    let mut ranked = Vec::new();
    let mut total = 0usize;
    for pane_entries in by_pane.into_values() {
        let (mut found, count) = rank_entry_refs_where(
            pane_entries.into_iter(),
            query,
            scope,
            OUTPUT_PER_PANE_CAP,
            |score| score.output_safe(),
        );
        total = total.saturating_add(count);
        ranked.append(&mut found);
    }
    sort_and_cap(&mut ranked);
    ranked.truncate(cap);
    (ranked, total)
}

fn build_output_entries(targets: Vec<OutputTarget>) -> (Arc<Vec<SearchEntry>>, bool) {
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    let mut index_bytes = 0usize;
    let mut capped = false;
    for target in targets {
        if bytes >= OUTPUT_SOURCE_BYTES
            || index_bytes >= OUTPUT_INDEX_BYTES
            || entries.len() >= OUTPUT_ROW_CAP
        {
            capped = true;
            break;
        }
        let detail_field = Arc::new(crate::search::PreparedText::from_shared(Arc::clone(
            &target.detail,
        )));
        let workspace_field = Arc::new(crate::search::PreparedText::from_shared(Arc::clone(
            &target.workspace,
        )));
        index_bytes = index_bytes
            .saturating_add(detail_field.index_bytes())
            .saturating_add(workspace_field.index_bytes());
        let Ok(engine) = target.engine.lock() else {
            continue;
        };
        let history = engine.history_len();
        let row_count = engine.retained_row_count();
        engine.for_each_retained_row(&mut |row, line| {
            if bytes >= OUTPUT_SOURCE_BYTES
                || index_bytes >= OUTPUT_INDEX_BYTES
                || entries.len() >= OUTPUT_ROW_CAP
            {
                capped = true;
                return;
            }
            bytes = bytes.saturating_add(line.len());
            if bytes > OUTPUT_SOURCE_BYTES {
                capped = true;
                return;
            }
            let line = Arc::<str>::from(line);
            let line_field = Arc::new(crate::search::PreparedText::from_shared(Arc::clone(&line)));
            index_bytes = index_bytes.saturating_add(line_field.index_bytes());
            if index_bytes > OUTPUT_INDEX_BYTES {
                capped = true;
                return;
            }
            entries.push(SearchEntry::new_with_prepared_fields(
                Arc::clone(&line),
                Arc::clone(&target.detail),
                vec![
                    line_field,
                    Arc::clone(&detail_field),
                    Arc::clone(&workspace_field),
                ],
                SearchTarget::Output {
                    pane: target.pane,
                    row,
                    offset: history.saturating_sub(row),
                    above: row_count.saturating_sub(1).saturating_sub(row),
                    line,
                },
                false,
                format!("output:{}:{row}", target.pane.0),
                SearchKind::Output,
            ));
        });
    }
    (Arc::new(entries), capped)
}

fn file_entries(roots: Vec<(usize, String, std::path::PathBuf)>) -> (Vec<SearchEntry>, bool) {
    let catalogs = roots
        .into_iter()
        .map(|(ws, name, root)| {
            let catalog = (*crate::search::files::index_cached(&root)).clone();
            (ws, name, root, catalog)
        })
        .collect();
    file_entries_from_catalogs(catalogs)
}

fn file_entries_from_catalogs(
    catalogs: Vec<(
        usize,
        String,
        std::path::PathBuf,
        crate::search::files::FileCatalog,
    )>,
) -> (Vec<SearchEntry>, bool) {
    let session = crate::session::display_name();
    let mut entries = Vec::new();
    let mut partial = false;
    for (ws, workspace, workspace_cwd, catalog) in catalogs {
        partial |= catalog.partial || catalog.truncated;
        for record in catalog.records {
            let relative = record.relative.to_string_lossy().into_owned();
            let label = record
                .relative
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| relative.clone());
            entries.push(SearchEntry::new(
                format!("file:{ws}:{relative}"),
                SearchKind::File,
                label,
                format!("{} › {} › {}", session, workspace, relative),
                [relative],
                SearchTarget::File {
                    ws,
                    path: record.path,
                    workspace_cwd: workspace_cwd.clone(),
                },
                false,
            ));
        }
    }
    (entries, partial)
}

fn api_error(id: &str, message: &str) -> String {
    serde_json::json!({
        "id": id,
        "error": { "code": "invalid_request", "message": message }
    })
    .to_string()
}

fn search_match_json(found: &SearchMatch) -> serde_json::Value {
    let target = match &found.entry.target {
        SearchTarget::Session {
            name,
            running,
            current,
        } => serde_json::json!({
            "session": name,
            "running": running,
            "current": current,
        }),
        SearchTarget::Workspace { ws, cwd } => {
            serde_json::json!({ "workspace": ws, "workspace_path": cwd.to_string_lossy() })
        }
        SearchTarget::Tab {
            ws,
            tab,
            workspace_cwd,
            tab_leaves,
        } => {
            serde_json::json!({
                "workspace": ws,
                "workspace_path": workspace_cwd.to_string_lossy(),
                "tab": tab + 1,
                "tab_panes": tab_leaves.iter().map(|pane| pane.0.to_string()).collect::<Vec<_>>(),
            })
        }
        SearchTarget::Pane { pane } | SearchTarget::Agent { pane } => {
            serde_json::json!({ "pane": pane.0.to_string() })
        }
        SearchTarget::File {
            ws,
            path,
            workspace_cwd,
        } => serde_json::json!({
            "workspace": ws,
            "workspace_path": workspace_cwd.to_string_lossy(),
            "path": path.to_string_lossy(),
        }),
        SearchTarget::Output {
            pane,
            row,
            offset,
            above,
            line,
        } => serde_json::json!({
            "pane": pane.0.to_string(),
            "row": row,
            "line_offset": offset,
            "above": above,
            "line": line,
        }),
        SearchTarget::Remote { target, .. } => target.clone(),
    };
    serde_json::json!({
        "id": found.entry.id,
        "kind": found.entry.kind.label(),
        "label": found.entry.label,
        "detail": found.entry.detail,
        "score": found.score,
        "target": target,
    })
}

fn sort_and_cap(matches: &mut Vec<SearchMatch>) {
    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.entry.active.cmp(&a.entry.active))
            .then_with(|| a.entry.label.cmp(&b.entry.label))
            .then_with(|| a.entry.id.cmp(&b.entry.id))
    });
    matches.truncate(RESULT_CAP);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    #[test]
    fn overlay_opens_with_navigation_recommendations_and_fuzzy_types() {
        let _env = crate::persist::test_env("fuzzy-search-open");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[0].name = "Backend API".into();
        app.open_search();
        assert!(app.search.as_ref().is_some_and(|s| !s.results.is_empty()));
        for ch in ['b', 'a', 'p', 'i'] {
            app.search_key(key(ch));
        }
        assert!(app
            .search
            .as_ref()
            .unwrap()
            .metadata_matches
            .iter()
            .any(|result| result.entry.kind == SearchKind::Workspace));
    }

    #[test]
    fn altgr_character_is_text_on_windows_and_a_ctrl_chord_elsewhere() {
        let _env = crate::persist::test_env("search-altgr");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_search();
        app.search_key(KeyEvent::new(
            KeyCode::Char('€'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));

        let query = &app.search.as_ref().unwrap().query;
        if cfg!(windows) {
            assert_eq!(query, "€");
        } else {
            assert!(query.is_empty(), "Ctrl+Alt remains a real chord");
        }
    }

    #[test]
    fn metadata_uses_cached_pane_title_without_terminal_inspection() {
        let _env = crate::persist::test_env("fuzzy-search-cached-pane-title");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().detected_title = Some(Arc::from("Cached pane title"));

        let metadata = app.search_metadata();
        let entry = metadata
            .iter()
            .find(|entry| matches!(entry.target, SearchTarget::Pane { pane: id } if id == pane))
            .expect("active pane must be searchable");
        assert_eq!(entry.label.as_ref(), "Cached pane title");
    }

    #[test]
    fn file_catalog_keeps_its_explicit_workspace_root() {
        let root = std::path::PathBuf::from("/workspace/root");
        let relative = std::path::PathBuf::from("src/main.rs");
        let catalog = crate::search::files::FileCatalog {
            records: vec![crate::search::files::FileRecord {
                path: root.join(&relative),
                relative,
            }],
            truncated: false,
            partial: false,
        };

        let (entries, partial) =
            file_entries_from_catalogs(vec![(3, "workspace".into(), root.clone(), catalog)]);
        assert!(!partial);
        assert!(matches!(
            &entries[0].target,
            SearchTarget::File { ws: 3, workspace_cwd, .. } if workspace_cwd == &root
        ));
    }

    #[test]
    fn escape_clears_then_closes() {
        let _env = crate::persist::test_env("fuzzy-search-escape");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_search();
        app.search_key(key('x'));
        app.search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.search.as_ref().unwrap().query, "");
        app.search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.search.is_none());
    }

    #[test]
    fn tab_cycles_finder_scopes_in_both_directions() {
        let _env = crate::persist::test_env("fuzzy-search-scopes");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_search();
        app.search_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.search.as_ref().unwrap().scope, SearchScope::Navigate);
        app.search_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(app.search.as_ref().unwrap().scope, SearchScope::All);
    }

    #[test]
    fn case_toggle_is_available_only_when_output_is_in_scope() {
        let _env = crate::persist::test_env("fuzzy-search-case-scope");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let ctrl_i = KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL);
        app.open_search();

        app.search_set_scope(SearchScope::Files);
        app.search_key(ctrl_i);
        assert!(!app.search.as_ref().unwrap().case_sensitive);

        app.search_set_scope(SearchScope::All);
        app.search_key(ctrl_i);
        assert!(app.search.as_ref().unwrap().case_sensitive);

        app.search_set_scope(SearchScope::Navigate);
        app.search_key(ctrl_i);
        assert!(
            app.search.as_ref().unwrap().case_sensitive,
            "Navigate ignores the shortcut instead of changing Output's saved choice"
        );
    }

    #[test]
    fn empty_file_scope_recommends_recently_opened_files() {
        let _env = crate::persist::test_env("fuzzy-search-recent-files");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let path = std::env::current_dir().unwrap().join("Cargo.toml");
        app.open_file_view(path, crate::app::files::OpenTarget::Preview);
        app.open_search();
        app.search_set_scope(SearchScope::Files);

        let search = app.search.as_ref().unwrap();
        assert!(search.query.is_empty());
        assert!(search.results.iter().any(|result| {
            result.entry.kind == SearchKind::File && result.entry.label.as_ref() == "Cargo.toml"
        }));
    }

    #[test]
    fn output_ranking_drops_noisy_sparse_subsequences() {
        let entry = |label: &str| {
            SearchEntry::new(
                format!("output:1:{label}"),
                SearchKind::Output,
                label.into(),
                "default › workspace › tab 1 › pane 1".into(),
                [],
                SearchTarget::Output {
                    pane: PaneId(1),
                    row: 0,
                    offset: 0,
                    above: 0,
                    line: label.into(),
                },
                false,
            )
        };
        let entries = vec![entry("fuzzy-benchmark-pane-1-auth-failure")];

        let (sparse, sparse_total) = rank_output(
            &entries,
            &FuzzyQuery::new("fzpn1fl", false),
            SearchScope::Output,
            10,
        );
        assert!(sparse.is_empty());
        assert_eq!(sparse_total, 0);

        let (dense, dense_total) = rank_output(
            &entries,
            &FuzzyQuery::new("auth fail", false),
            SearchScope::Output,
            10,
        );
        assert_eq!(dense.len(), 1);
        assert_eq!(dense_total, 1);
    }

    #[test]
    fn fuzzy_api_is_async_and_returns_typed_navigation_results() {
        let _env = crate::persist::test_env("fuzzy-search-api");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[0].name = "Backend API".into();
        let (reply, response) = std::sync::mpsc::channel();
        app.start_search_api(crate::ipc::api::ApiRequest {
            id: "search-test".into(),
            method: "search.query".into(),
            params: serde_json::json!({
                "query": "bapi",
                "scope": "navigate",
                "limit": 20,
            }),
            reply,
        });
        let line = response
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("search worker must reply");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["result"]["type"], "search_query");
        assert!(value["result"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "folder" && item["label"] == "Backend API"));
    }

    #[test]
    fn structured_activation_revalidates_targets() {
        let _env = crate::persist::test_env("fuzzy-search-activate");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let (reply, _response) = std::sync::mpsc::channel();
        let request = crate::ipc::api::ApiRequest {
            id: "activate-test".into(),
            method: "search.activate".into(),
            params: serde_json::json!({
                "kind": "pane",
                "target": { "pane": pane.0.to_string() },
            }),
            reply,
        };
        let value: serde_json::Value =
            serde_json::from_str(&app.handle_search_activate(&request)).unwrap();
        assert_eq!(value["result"]["activated"], true);

        let bad = crate::ipc::api::ApiRequest {
            id: "bad-target".into(),
            method: "search.activate".into(),
            params: serde_json::json!({
                "kind": "pane",
                "target": { "pane": "4294967295" },
            }),
            reply: std::sync::mpsc::channel().0,
        };
        let value: serde_json::Value =
            serde_json::from_str(&app.handle_search_activate(&bad)).unwrap();
        assert_eq!(value["error"]["code"], "invalid_request");
    }

    #[test]
    fn unnamed_tab_target_follows_the_tab_across_reordering() {
        let _env = crate::persist::test_env("fuzzy-search-tab-identity");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let first = PaneId::alloc();
        let second = PaneId::alloc();
        app.workspaces[0].tabs = [first, second]
            .into_iter()
            .map(|pane| super::super::Tab::panes(crate::layout::TileLayout::new(pane)))
            .collect();
        app.workspaces[0].active_tab = 0;
        let cwd = app.workspaces[0].cwd.clone();

        app.move_tab(0, 1).unwrap();
        assert_eq!(
            app.resolve_search_tab(0, &cwd, &[first]),
            Some(1),
            "the target follows the original unnamed tab instead of index 1"
        );

        let request = crate::ipc::api::ApiRequest {
            id: "tab-activate".into(),
            method: "search.activate".into(),
            params: serde_json::json!({
                "kind": "tab",
                "target": {
                    "workspace": 0,
                    "workspace_path": cwd,
                    "tab": 1,
                    "tab_panes": [first.0.to_string()],
                },
            }),
            reply: std::sync::mpsc::channel().0,
        };
        let value: serde_json::Value =
            serde_json::from_str(&app.handle_search_activate(&request)).unwrap();
        assert_eq!(value["result"]["activated"], true);
        assert_eq!(app.workspaces[0].active_tab, 1);
    }

    #[test]
    fn pasted_text_updates_the_fuzzy_query_once() {
        let _env = crate::persist::test_env("fuzzy-search-paste");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_search();
        let generation = app.search.as_ref().unwrap().generation;

        assert!(app.handle_event(crate::event::AppEvent::Paste("Cargo\n.toml".into())));

        let search = app.search.as_ref().unwrap();
        assert_eq!(search.query, "Cargo.toml");
        assert_eq!(search.generation, generation.wrapping_add(1));
    }

    #[test]
    fn selecting_a_known_session_requests_one_client_handoff() {
        let _env = crate::persist::test_env("fuzzy-search-session-handoff");
        std::fs::create_dir_all(crate::session::session_dir_for(Some("sibling"))).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_search();
        let index = app
            .search
            .as_ref()
            .unwrap()
            .results
            .iter()
            .position(|result| result.entry.id == "session:sibling")
            .unwrap();
        app.search.as_mut().unwrap().cursor = index;
        app.search_activate();
        assert_eq!(app.pending_session_switch.as_deref(), Some("sibling"));
        assert!(app.search.is_none());
    }

    #[test]
    fn selecting_a_file_uses_the_configured_plain_click_default() {
        let _env = crate::persist::test_env("fuzzy-search-file-open");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let path = std::env::current_dir().unwrap().join("Cargo.toml");
        let workspace_cwd = app.workspaces[0].cwd.clone();
        app.open_file_view(path.clone(), crate::app::files::OpenTarget::Preview);
        let preview = app.layout().focus;
        let tabs_before = app.workspaces[0].tabs.len();

        app.open_search();
        let search = app.search.as_mut().unwrap();
        search.results = vec![SearchMatch {
            entry: SearchEntry::new(
                "file:0:Cargo.toml".into(),
                SearchKind::File,
                "Cargo.toml".into(),
                "default › Cargo.toml".into(),
                [],
                SearchTarget::File {
                    ws: 0,
                    path: path.clone(),
                    workspace_cwd,
                },
                false,
            ),
            score: 1,
            label_positions: Vec::new(),
        }];
        search.cursor = 0;

        app.search_activate();

        assert_eq!(
            app.workspaces[0].tabs.len(),
            tabs_before + 1,
            "the read-only default opens a full file tab, never a preview"
        );
        assert!(app
            .views
            .values()
            .any(|view| matches!(view, crate::app::ViewKind::File(file) if file.path == path)));
        let file_tab = app.workspaces[0].active_tab;
        let leaves = app.workspaces[0].tabs[file_tab].layout.leaves();
        assert_eq!(leaves.len(), 1, "the fuzzy result owns a whole tab");
        assert_ne!(leaves[0], preview, "the preview pane was not reused");

        let tabs_after = app.workspaces[0].tabs.len();
        let file_tab_view = leaves[0];
        app.open_file_search_result(path);
        assert_eq!(app.workspaces[0].tabs.len(), tabs_after);
        assert_eq!(app.layout().focus, file_tab_view, "the whole tab is reused");
    }
}
