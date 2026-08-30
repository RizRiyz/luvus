//! Module registry operations + the action/command runner, driven from the
//! `module.*` UHP (docs/13 MOD-1). Registry edits persist immediately;
//! command runs are fire-and-forget with a `Running` log filled in when the
//! subprocess finishes (`AppEvent::ModuleCommandFinished`).

use std::path::Path;

use super::*;
use crate::module::context::Target;
use crate::module::manifest::ModuleManifest;
use crate::module::runtime::{ModuleCommandLog, ModuleStatus};
use crate::module::{
    context, paths, registry, runtime, settings, InstalledModule, ModulePaneRecord,
};

/// One module action offered in a right-click menu (docs/13 §3.8). A menu
/// snapshots these when it opens, so the rows it draws and the action a click
/// runs are the same list even if the registry changes underneath.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModuleMenuAction {
    pub module_id: String,
    pub action_id: String,
    pub title: String,
}

impl App {
    /// Resolve a module `spec` (its id, or the `owner/repo[/sub]` shorthand it
    /// was installed with) to its canonical id.
    ///
    /// Everything downstream keys off the real id: the registry, the config and
    /// state directories, the startup-hook set. Resolving once at the entry
    /// point keeps `owner/repo` from leaking into any of them.
    pub fn module_id_for(&self, spec: &str) -> Result<String, String> {
        self.modules
            .find(spec)
            .map(|m| m.id.clone())
            .ok_or_else(|| format!("no module {spec}"))
    }

    /// Register a module dir, recording its install `source` (a git install
    /// passes `owner/repo@<sha>`; a local link passes `None`).
    pub fn module_link_with(
        &mut self,
        path: &Path,
        enabled: bool,
        source: Option<String>,
    ) -> Result<String, String> {
        let root = path
            .canonicalize()
            .map_err(|e| format!("cannot resolve {}: {e}", path.display()))?;
        let manifest = ModuleManifest::load(&root)?;
        let id = manifest.id.clone();
        if self.modules.find(&id).is_some() {
            return Err(format!("module {id} is already registered"));
        }
        self.modules.modules.push(InstalledModule {
            id: id.clone(),
            root,
            enabled,
            source,
            manifest,
            warning: None,
        });
        registry::save(&self.modules);
        self.bar.sync_modules(&self.modules);
        // A freshly linked module gets its startup hooks now rather than at the
        // next restart, so its docks and bar widgets appear immediately.
        self.run_module_startup_hooks();
        Ok(id)
    }

    /// Uninstall a git-installed module: remove it from the registry **and**
    /// delete its managed checkout (guarded — refuses for locally-linked modules).
    pub fn module_uninstall(&mut self, spec: &str) -> Result<(), String> {
        let id = &self.module_id_for(spec)?;
        let root = self
            .modules
            .find(id)
            .map(|m| m.root.clone())
            .ok_or_else(|| format!("no module {id}"))?;
        if !crate::module::install::is_removable(&root) {
            return Err(format!(
                "{id} is a linked module (its files aren't managed by luvus) — use `module unlink`"
            ));
        }
        let dock_ids = self.module_dock_ids(id);
        self.modules.modules.retain(|m| &m.id != id);
        registry::save(&self.modules);
        let _ = std::fs::remove_dir_all(&root);
        self.remove_module_docks(&dock_ids);
        self.bar.clear_owner(id);
        self.bar.sync_modules(&self.modules);
        Ok(())
    }

    /// Remove a module from the registry (does not touch its files).
    pub fn module_unlink(&mut self, spec: &str) -> Result<(), String> {
        let id = &self.module_id_for(spec)?;
        let dock_ids = self.module_dock_ids(id);
        let before = self.modules.modules.len();
        self.modules.modules.retain(|m| &m.id != id);
        if self.modules.modules.len() == before {
            return Err(format!("no module {id}"));
        }
        registry::save(&self.modules);
        self.remove_module_docks(&dock_ids);
        self.bar.clear_owner(id);
        self.bar.sync_modules(&self.modules);
        Ok(())
    }

    pub fn module_set_enabled(&mut self, spec: &str, on: bool) -> Result<(), String> {
        let id = &self.module_id_for(spec)?;
        let m = self
            .modules
            .find_mut(id)
            .ok_or_else(|| format!("no module {id}"))?;
        m.enabled = on;
        registry::save(&self.modules);
        // Disabling a module retires its docks; re-enabling re-runs its startup
        // hooks so it can repaint them (docs/29, DOCK-4).
        if !on {
            let dock_ids = self.module_dock_ids(id);
            self.remove_module_docks(&dock_ids);
            self.bar.clear_owner(id);
            self.module_startup_done.remove(id);
            self.bar.sync_modules(&self.modules);
        } else {
            // Make declarations visible before the asynchronous startup command
            // can call `luvus bar push` (`ui.bar.push` on the UHP).
            self.bar.sync_modules(&self.modules);
            self.run_module_startup_hooks();
        }
        Ok(())
    }

    /// The dock ids a module declares in its manifest (docs/29, DOCK-4).
    fn module_dock_ids(&self, id: &str) -> Vec<String> {
        self.modules
            .find(id)
            .map(|m| m.manifest.docks.iter().map(|d| d.id.clone()).collect())
            .unwrap_or_default()
    }

    /// The id of the module that declares dock `dock_id`, if any.
    pub fn module_owning_dock(&self, dock_id: &str) -> Option<String> {
        self.modules
            .modules
            .iter()
            .find(|m| m.manifest.docks.iter().any(|d| d.id == dock_id))
            .map(|m| m.id.clone())
    }

    /// Ensure (and return) a module's config dir.
    pub fn module_config_dir(&self, spec: &str) -> Result<std::path::PathBuf, String> {
        let id = self.module_id_for(spec)?;
        let dir = paths::config_dir(&id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        Ok(dir)
    }

    // ── right-click menus (docs/13 §3.8) ─────────────────────────────────────

    /// The module actions offered in right-click menu `context` (`pane`,
    /// `workspace`, `agent`, `tab`), in registry order. Called when a menu opens,
    /// not per frame.
    pub fn module_menu_actions(&self, context: &str) -> Vec<ModuleMenuAction> {
        self.modules
            .modules
            .iter()
            .filter(|m| m.is_runnable())
            .flat_map(|m| {
                m.manifest
                    .actions_for_context(context)
                    .into_iter()
                    .map(move |a| ModuleMenuAction {
                        module_id: m.id.clone(),
                        action_id: a.id.clone(),
                        title: a.title.clone(),
                    })
            })
            .collect()
    }

    /// Run a module action picked from a right-click menu, against `target`.
    ///
    /// `a` comes from the menu's own snapshot rather than a fresh lookup, so a
    /// module enabled or disabled while the menu was open can't shift which
    /// action a click runs. The module is re-resolved here, so one that went
    /// away in the meantime is a no-op with a toast instead of a wrong action.
    pub fn run_module_menu_action(&mut self, context: &str, a: ModuleMenuAction, target: Target) {
        let argv = self
            .modules
            .find(&a.module_id)
            .filter(|m| m.is_runnable())
            .and_then(|m| m.manifest.action(&a.action_id).map(|x| x.command.clone()));
        let Some(argv) = argv else {
            self.show_toast(format!("{} is unavailable", a.module_id));
            return;
        };
        let extra = vec![("LUVUS_MODULE_ACTION_ID".to_string(), a.action_id.clone())];
        let label = format!("action:{}", a.action_id);
        let source = format!("menu:{context}");
        if let Err(e) =
            self.run_module_command_for(&a.module_id, argv, label, extra, &source, target)
        {
            self.show_toast(e);
        } else {
            self.show_toast(a.title);
        }
    }

    // ── dock-row right-click menus (docs/52) ─────────────────────────────────

    /// Open the context menu for row `row_i` of module dock `dock_id`.
    ///
    /// Everything the click will need is captured *now* — the items, the row's
    /// text and value, and the owning module — because the row list can be
    /// replaced by any `ui.dock.push` while the menu sits open. A row with no
    /// declared menu opens nothing, so a right-click there falls through as a
    /// plain miss rather than showing an empty popup.
    pub fn open_dock_menu(&mut self, dock_id: &str, row_i: usize, col: u16, row: u16) {
        let Some(r) = self
            .module_docks
            .get(dock_id)
            .and_then(|d| d.rows.get(row_i))
        else {
            return;
        };
        if r.menu.is_empty() {
            return;
        }
        self.dock_menu = Some(crate::app::DockMenu {
            dock_id: dock_id.to_string(),
            row_index: row_i,
            anchor: (col, row),
            items: r.menu.clone(),
            row_text: r.text.clone(),
            row_value: r.value.clone(),
            owner: self.module_owning_dock(dock_id),
            rects: Vec::new(),
        });
    }

    /// Route a click at `(col,row)` to a dock-menu item, or dismiss the menu.
    pub fn dock_menu_click(&mut self, col: u16, row: u16) {
        let hit = self.dock_menu.as_ref().and_then(|m| {
            m.rects
                .iter()
                .position(|r| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
        });
        match hit {
            // A divider is inert: keep the menu open rather than closing on a
            // click that meant nothing.
            Some(i)
                if self
                    .dock_menu
                    .as_ref()
                    .is_some_and(|m| m.items[i].is_divider()) => {}
            Some(i) => self.dock_menu_action(i),
            None => self.dock_menu = None,
        }
    }

    /// Invoke dock-menu item `i` against the row the menu was opened on, then
    /// close the menu.
    ///
    /// The row env is rebuilt from the menu's snapshot, so the action receives
    /// the row the user actually right-clicked even if the dock has since been
    /// repainted — the same identity a left-click would have passed
    /// (docs/13 §3.10).
    pub fn dock_menu_action(&mut self, i: usize) {
        let Some(menu) = self.dock_menu.take() else {
            return;
        };
        let Some(item) = menu.items.get(i).filter(|it| !it.is_divider()) else {
            return;
        };
        let extra = vec![
            ("LUVUS_MODULE_DOCK_ID".to_string(), menu.dock_id.clone()),
            (
                "LUVUS_MODULE_ROW_INDEX".to_string(),
                menu.row_index.to_string(),
            ),
            ("LUVUS_MODULE_ROW_TEXT".to_string(), menu.row_text.clone()),
            (
                "LUVUS_MODULE_ROW_VALUE".to_string(),
                // An item's own value wins: the row says *which board*, the
                // item says *which variant*.
                item.value
                    .clone()
                    .or_else(|| menu.row_value.clone())
                    .unwrap_or_else(|| menu.row_text.clone()),
            ),
            ("LUVUS_MODULE_ACTION_ID".to_string(), item.action.clone()),
        ];
        if let Err(e) = self.module_invoke_dock_action(&item.action, menu.owner.as_deref(), extra) {
            self.show_toast(e);
        }
    }

    // ── settings (docs/13 §3.6) ──────────────────────────────────────────────

    /// A module's effective settings (manifest defaults under stored values).
    pub fn module_settings(&self, spec: &str) -> Result<serde_json::Map<String, Value>, String> {
        let m = self
            .modules
            .find(spec)
            .ok_or_else(|| format!("no module {spec}"))?;
        Ok(settings::effective(&m.manifest, &m.id))
    }

    /// Set one declared setting, validated against its spec.
    pub fn module_set_setting(&mut self, spec: &str, key: &str, v: Value) -> Result<Value, String> {
        let m = self
            .modules
            .find(spec)
            .ok_or_else(|| format!("no module {spec}"))?;
        settings::set(&m.manifest, &m.id, key, v)
    }

    // ── startup hooks (docs/13 §3.7) ─────────────────────────────────────────

    /// Run every enabled module's `[[startup]]` commands once per process.
    ///
    /// Called when the session is up and the API socket is listening, and again
    /// when a module is linked or re-enabled mid-session — a module that paints
    /// a dock needs somewhere to repaint it after a restart, and `ui.dock.push`
    /// state is deliberately not persisted.
    pub fn run_module_startup_hooks(&mut self) {
        let pending: Vec<(String, Vec<Vec<String>>)> = self
            .modules
            .modules
            .iter()
            .filter(|m| m.is_runnable() && !self.module_startup_done.contains(&m.id))
            .map(|m| {
                let cmds = m
                    .manifest
                    .startup
                    .iter()
                    .filter(|s| crate::module::manifest::allowed_on(s.platforms.as_ref()))
                    .map(|s| s.command.clone())
                    .collect();
                (m.id.clone(), cmds)
            })
            .collect();
        for (id, cmds) in pending {
            // Mark done even with no commands, so the scan stays cheap.
            self.module_startup_done.insert(id.clone());
            for argv in cmds {
                let extra = vec![("LUVUS_MODULE_EVENT".to_string(), "startup".to_string())];
                let _ = self.run_module_command(&id, argv, "startup".to_string(), extra, "startup");
            }
        }
    }

    /// Invoke an action by id, optionally constrained to one module. Resolves by
    /// action id when unambiguous, else requires `module`. Returns the log id.
    pub fn module_invoke_action(
        &mut self,
        action_id: &str,
        module_filter: Option<&str>,
        source: &str,
    ) -> Result<u64, String> {
        self.module_invoke_action_with(action_id, module_filter, source, Vec::new())
    }

    /// Invoke a dock row's action, carrying that row's identity in the env so a
    /// single action can back a whole list of rows (docs/13 §3.10).
    pub fn module_invoke_dock_action(
        &mut self,
        action_id: &str,
        module_filter: Option<&str>,
        row_env: Vec<(String, String)>,
    ) -> Result<u64, String> {
        self.module_invoke_action_with(action_id, module_filter, "dock", row_env)
    }

    /// Invoke an action, appending `extra_env` to the usual injected environment.
    pub fn module_invoke_action_with(
        &mut self,
        action_id: &str,
        module_filter: Option<&str>,
        source: &str,
        extra_env: Vec<(String, String)>,
    ) -> Result<u64, String> {
        // When a specific module is named, validate it up front for a clear
        // error (e.g. "disabled") instead of a generic "no runnable module …".
        if let Some(mid) = module_filter {
            match self.modules.find(mid) {
                None => return Err(format!("no module {mid}")),
                Some(m) if !m.is_runnable() => {
                    return Err(m
                        .warning
                        .clone()
                        .unwrap_or_else(|| format!("module {mid} is disabled")))
                }
                Some(m) if m.manifest.action(action_id).is_none() => {
                    return Err(format!("module {mid} has no action {action_id}"))
                }
                _ => {}
            }
        }
        let matches: Vec<(String, Vec<String>)> = self
            .modules
            .modules
            .iter()
            .filter(|m| m.is_runnable())
            .filter(|m| module_filter.is_none_or(|f| m.id == f))
            .filter_map(|m| {
                m.manifest
                    .action(action_id)
                    .map(|a| (m.id.clone(), a.command.clone()))
            })
            .collect();
        let (module_id, argv) = match matches.len() {
            0 => return Err(format!("no runnable module has action {action_id}")),
            1 => matches.into_iter().next().unwrap(),
            _ => {
                return Err(format!(
                    "action {action_id} is ambiguous — pass a module id"
                ))
            }
        };
        let mut extra = vec![("LUVUS_MODULE_ACTION_ID".to_string(), action_id.to_string())];
        extra.extend(extra_env);
        self.run_module_command(
            &module_id,
            argv,
            format!("action:{action_id}"),
            extra,
            source,
        )
    }

    /// Publish a lifecycle event to `events.subscribe` subscribers **and** run
    /// any enabled module's matching `[[events]]` hook (MOD-3). The payload is
    /// passed to hooks as `LUVUS_MODULE_EVENT_JSON`.
    pub fn emit_event(&mut self, name: &str, data: serde_json::Value) {
        let event_json = data.to_string();
        let backend_data = data.clone();
        api::publish_event(&self.events, name, data);
        self.emit_backend_lifecycle_from_pane_event(name, &backend_data);
        let mut targets: Vec<(String, Vec<String>)> = Vec::new();
        for m in &self.modules.modules {
            if !m.is_runnable() {
                continue;
            }
            for e in &m.manifest.events {
                // `workspace.*` also answers to the legacy `node.*` spelling, so
                // a module written against the old names keeps working.
                let matches = e.on == name
                    || e.on
                        .strip_prefix("node.")
                        .is_some_and(|suffix| name.strip_prefix("workspace.") == Some(suffix));
                if matches && crate::module::manifest::allowed_on(e.platforms.as_ref()) {
                    targets.push((m.id.clone(), e.command.clone()));
                }
            }
        }
        for (module_id, argv) in targets {
            let extra = vec![
                ("LUVUS_MODULE_EVENT".to_string(), name.to_string()),
                ("LUVUS_MODULE_EVENT_JSON".to_string(), event_json.clone()),
            ];
            let _ =
                self.run_module_command(&module_id, argv, format!("event:{name}"), extra, "event");
        }
    }

    /// Open a module's `[[panes]]` entrypoint as a real luvus pane (MOD-2),
    /// placed per `placement` (split | overlay | tab; default split). Returns the
    /// new pane id.
    pub fn module_open_pane(
        &mut self,
        module_id: &str,
        entrypoint: &str,
        placement: Option<&str>,
        source: &str,
    ) -> Result<PaneId, String> {
        let argv = {
            let m = self
                .modules
                .find(module_id)
                .ok_or_else(|| format!("no module {module_id}"))?;
            if !m.is_runnable() {
                return Err(m
                    .warning
                    .clone()
                    .unwrap_or_else(|| format!("module {module_id} is disabled")));
            }
            m.manifest
                .panes
                .iter()
                .find(|p| {
                    p.id == entrypoint && crate::module::manifest::allowed_on(p.platforms.as_ref())
                })
                .map(|p| p.command.clone())
                .ok_or_else(|| format!("module {module_id} has no pane {entrypoint}"))?
        };
        let placement = placement.unwrap_or("split");

        let ctx = context::build(self, source);
        let (root, env) = {
            let m = self.modules.find(module_id).unwrap();
            (
                m.root.clone(),
                runtime::env(
                    m,
                    &ctx,
                    vec![(
                        "LUVUS_MODULE_ENTRYPOINT_ID".to_string(),
                        entrypoint.to_string(),
                    )],
                ),
            )
        };

        // The pane runs the argv in the module root (so relative paths resolve);
        // the script reads the workspace cwd from the context.
        let id = PaneId::alloc();
        let history_budget_bytes = self.config.scrollback_bytes();
        let pane = Pane::spawn_command(
            id,
            80,
            24,
            root,
            self.app_tx.clone(),
            &argv,
            &env,
            history_budget_bytes,
            self.pane_appearance,
        )
        .map_err(|e| format!("cannot spawn module pane: {e}"))?;
        let cmd = pane.command.clone();
        self.panes.insert(id, pane);
        self.status.insert(id, PaneStatus::new(cmd));
        self.session_dirty = true;

        match placement {
            "tab" => {
                let ws = &mut self.workspaces[self.active_ws];
                ws.tabs.push(Tab::panes(TileLayout::new(id)));
                ws.active_tab = ws.tabs.len() - 1;
                self.zoomed = false;
            }
            "overlay" => {
                self.layout_mut().split_focused(Axis::Col, id);
                self.zoomed = true; // fill the screen, overlay-style
            }
            _ => {
                self.layout_mut().split_focused(Axis::Col, id);
                self.zoomed = false;
            }
        }
        self.module_panes.insert(
            id,
            ModulePaneRecord {
                module_id: module_id.to_string(),
                entrypoint: entrypoint.to_string(),
            },
        );
        self.emit_event(
            "pane.created",
            json!({"pane": id.0.to_string(), "module": module_id}),
        );
        Ok(id)
    }

    /// Run an argv command for a module against the focused workspace/pane.
    pub fn run_module_command(
        &mut self,
        module_id: &str,
        argv: Vec<String>,
        label: String,
        extra_env: Vec<(String, String)>,
        source: &str,
    ) -> Result<u64, String> {
        self.run_module_command_for(module_id, argv, label, extra_env, source, Target::default())
    }

    /// Run an argv command for a module against an explicit `target`: build the
    /// env and context, enforce the in-flight cap, push a `Running` log, and
    /// spawn the subprocess.
    pub fn run_module_command_for(
        &mut self,
        module_id: &str,
        argv: Vec<String>,
        label: String,
        extra_env: Vec<(String, String)>,
        source: &str,
        target: Target,
    ) -> Result<u64, String> {
        {
            let module = self
                .modules
                .find(module_id)
                .ok_or_else(|| format!("no module {module_id}"))?;
            if !module.is_runnable() {
                return Err(module
                    .warning
                    .clone()
                    .unwrap_or_else(|| format!("module {module_id} is disabled")));
            }
        }
        let in_flight = self
            .module_logs
            .iter()
            .filter(|l| l.status == ModuleStatus::Running)
            .count();
        if in_flight >= runtime::MAX_IN_FLIGHT {
            return Err(format!(
                "too many module commands in flight (max {})",
                runtime::MAX_IN_FLIGHT
            ));
        }
        let ctx = context::build_for(self, source, &target);
        let (root, env) = {
            let module = self.modules.find(module_id).unwrap();
            (module.root.clone(), runtime::env(module, &ctx, extra_env))
        };
        let log_id = runtime::next_log_id();
        self.push_module_log(ModuleCommandLog {
            id: log_id,
            module_id: module_id.to_string(),
            label,
            argv: argv.clone(),
            status: ModuleStatus::Running,
            code: None,
            out: String::new(),
            err: String::new(),
        });
        runtime::spawn(log_id, root, argv, env, self.app_tx.clone());
        Ok(log_id)
    }

    fn push_module_log(&mut self, log: ModuleCommandLog) {
        self.module_logs.push(log);
        let n = self.module_logs.len();
        if n > runtime::LOG_LIMIT {
            self.module_logs.drain(0..n - runtime::LOG_LIMIT);
        }
    }

    /// Fill in a command log when its subprocess finishes.
    pub fn module_command_finished(
        &mut self,
        log_id: u64,
        code: Option<i32>,
        out: String,
        err: String,
    ) {
        if let Some(log) = self.module_logs.iter_mut().find(|l| l.id == log_id) {
            log.status = if code == Some(0) {
                ModuleStatus::Succeeded
            } else {
                ModuleStatus::Failed
            };
            log.code = code;
            log.out = out;
            log.err = err;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::persist::TEST_ENV_LOCK;
    use std::time::{Duration, Instant};

    /// End-to-end: a click on a dock row's right-click menu (docs/52) really
    /// spawns that module's action, with the **snapshotted** row's identity in
    /// its environment — even though the dock was repainted while the menu was
    /// open. This is the last link the render/parse tests don't cover: that a
    /// menu click reaches a real subprocess with the right env.
    #[test]
    fn dock_menu_click_spawns_the_action_with_the_clicked_rows_env() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-dockmenu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let dir = home.join("boards-mod");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("luvus-module.toml"),
            r#"
id = "you.boards"
name = "Boards"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[docks]]
id = "boards"
title = "BOARDS"
placement = "sidebar.left"

[[actions]]
id = "erase"
title = "Erase"
command = ["sh", "-c", "echo ran=$LUVUS_MODULE_ACTION_ID port=$LUVUS_MODULE_ROW_VALUE row=$LUVUS_MODULE_ROW_TEXT dock=$LUVUS_MODULE_DOCK_ID"]
"#,
        )
        .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.module_link_with(&dir, true, None).unwrap();
        assert_eq!(id, "you.boards");

        let mk = |text: &str, port: &str| crate::app::DockRow {
            text: text.into(),
            dot: None,
            action: Some("select".into()),
            value: Some(port.into()),
            menu: vec![crate::app::DockRowMenuItem {
                title: "Erase this board".into(),
                action: "erase".into(),
                value: None,
                destructive: true,
            }],
        };
        app.push_module_dock(
            "boards",
            Some("BOARDS".into()),
            crate::app::Side::Left,
            vec![mk("esp32s3", "/dev/ttyA")],
        );

        app.open_dock_menu("boards", 0, 4, 4);
        assert!(app.dock_menu.is_some(), "menu opened");
        // The dock is fully repainted underneath the open menu, as a poller does.
        app.push_module_dock(
            "boards",
            None,
            crate::app::Side::Left,
            vec![mk("esp32c6", "/dev/ttyZ")],
        );

        let before = app.module_logs.len();
        app.dock_menu_action(0);
        assert!(app.dock_menu.is_none(), "menu closed");
        assert_eq!(app.module_logs.len(), before + 1, "a command was spawned");
        let log_id = app.module_logs.last().unwrap().id;

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                app.handle_event(ev);
            }
            let resolved = app
                .module_logs
                .iter()
                .find(|l| l.id == log_id)
                .is_some_and(|l| l.status != ModuleStatus::Running);
            if resolved || Instant::now() > deadline {
                break;
            }
        }

        let log = app.module_logs.iter().find(|l| l.id == log_id).unwrap();
        assert_eq!(log.status, ModuleStatus::Succeeded, "stderr: {}", log.err);
        assert!(log.out.contains("ran=erase"), "stdout: {:?}", log.out);
        assert!(log.out.contains("dock=boards"), "stdout: {:?}", log.out);
        // The port the user right-clicked, not the one the repaint installed.
        assert!(
            log.out.contains("port=/dev/ttyA") && log.out.contains("row=esp32s3"),
            "menu ran against the repainted row instead of the clicked one: {:?}",
            log.out
        );

        // ── an item with its own `value` overrides the row's ────────────────
        // One action ("erase" here stands in for a `run`-style dispatcher) backs
        // a menu of variants without an action id per entry.
        app.push_module_dock(
            "boards",
            None,
            crate::app::Side::Left,
            vec![crate::app::DockRow {
                text: "build".into(),
                dot: None,
                action: Some("erase".into()),
                value: Some("build".into()),
                menu: vec![crate::app::DockRowMenuItem {
                    title: "App only".into(),
                    action: "erase".into(),
                    value: Some("app".into()),
                    destructive: false,
                }],
            }],
        );
        app.open_dock_menu("boards", 0, 4, 4);
        let before = app.module_logs.len();
        app.dock_menu_action(0);
        assert_eq!(app.module_logs.len(), before + 1);
        let log_id = app.module_logs.last().unwrap().id;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                app.handle_event(ev);
            }
            let resolved = app
                .module_logs
                .iter()
                .find(|l| l.id == log_id)
                .is_some_and(|l| l.status != ModuleStatus::Running);
            if resolved || Instant::now() > deadline {
                break;
            }
        }
        let log = app.module_logs.iter().find(|l| l.id == log_id).unwrap();
        assert!(
            log.out.contains("port=app"),
            "the item's own value should win over the row's: {:?}",
            log.out
        );

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A dock the user turns **off** stays off across re-pushes (docs/29): a
    /// module re-pushes on startup and on `workspace.created`, and before the
    /// `docks_off` flag that re-push resurrected the dock on its default side
    /// because "off" looked identical to "never placed".
    #[test]
    fn turned_off_module_dock_is_not_resurrected_by_a_repush() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-dockoff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let kind = crate::app::DockKind::Module("esp-idf".into());
        let row = || crate::app::DockRow {
            text: "esp32s3".into(),
            dot: None,
            action: Some("select".into()),
            value: None,
            menu: vec![],
        };

        // First push (startup) auto-mounts to the module's default side.
        app.push_module_dock(
            "esp-idf",
            Some("ESP-IDF".into()),
            crate::app::Side::Left,
            vec![row()],
        );
        assert_eq!(app.sidebars.side_of(&kind), Some(crate::app::Side::Left));

        // User turns it off in Settings → Layout.
        app.unmount_dock(&kind);
        assert_eq!(app.sidebars.side_of(&kind), None);
        assert!(app.config.docks_off.iter().any(|d| d == "esp-idf"));

        // A re-push (as `workspace.created` / a refresh fires) must NOT bring it
        // back — this is the reported bug.
        app.push_module_dock("esp-idf", None, crate::app::Side::Left, vec![row()]);
        assert_eq!(
            app.sidebars.side_of(&kind),
            None,
            "an off dock was resurrected by a re-push"
        );

        // Re-placing it from Settings opts back in and clears the off flag; a
        // later push then leaves it where the user put it.
        app.move_dock(&kind, crate::app::Side::Right);
        assert!(!app.config.docks_off.iter().any(|d| d == "esp-idf"));
        app.push_module_dock("esp-idf", None, crate::app::Side::Left, vec![row()]);
        assert_eq!(app.sidebars.side_of(&kind), Some(crate::app::Side::Right));

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn link_then_run_action_captures_output() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        // A module dir: manifest + one echo action.
        let dir = home.join("echo-mod");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("luvus-module.toml"),
            r#"
id = "you.echo"
name = "Echo"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[actions]]
id = "refresh"
title = "Refresh"
command = ["sh", "-c", "echo hello-from-module; echo oops 1>&2"]
"#,
        )
        .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        // Link it, then it shows runnable and exposes the action.
        let id = app.module_link_with(&dir, true, None).unwrap();
        assert_eq!(id, "you.echo");
        assert!(app.modules.find(&id).unwrap().is_runnable());

        // Invoke the action; pump the loop until its log resolves.
        let log_id = app.module_invoke_action("refresh", None, "test").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                app.handle_event(ev);
            }
            let resolved = app
                .module_logs
                .iter()
                .find(|l| l.id == log_id)
                .is_some_and(|l| l.status != ModuleStatus::Running);
            if resolved || Instant::now() > deadline {
                break;
            }
        }

        let log = app.module_logs.iter().find(|l| l.id == log_id).unwrap();
        assert_eq!(log.status, ModuleStatus::Succeeded, "stderr: {}", log.err);
        assert_eq!(log.code, Some(0));
        assert!(
            log.out.contains("hello-from-module"),
            "captured stdout: {:?}",
            log.out
        );
        assert!(log.err.contains("oops"), "captured stderr: {:?}", log.err);

        // Disabling makes it non-runnable; unlink removes it.
        app.module_set_enabled(&id, false).unwrap();
        assert!(!app.modules.find(&id).unwrap().is_runnable());
        assert!(app.module_invoke_action("refresh", None, "test").is_err());
        // Naming the module explicitly gives a clear "disabled" error.
        let err = app
            .module_invoke_action("refresh", Some(&id), "test")
            .unwrap_err();
        assert!(err.contains("disabled"), "got: {err}");
        app.module_unlink(&id).unwrap();
        assert!(app.modules.find(&id).is_none());

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn open_module_pane_tracks_and_cleans_up() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-panetest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let dir = home.join("board-mod");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("luvus-module.toml"),
            r#"
id = "you.board"
name = "Board"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[panes]]
id = "board"
title = "Board"
command = ["sh", "-c", "sleep 5"]
"#,
        )
        .unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.module_link_with(&dir, true, None).unwrap();

        let before = app.panes.len();
        let pid = app
            .module_open_pane("you.board", "board", Some("split"), "test")
            .unwrap();
        assert_eq!(app.panes.len(), before + 1, "a real pane was spawned");
        assert!(
            app.module_panes.contains_key(&pid),
            "tracked as a module pane"
        );
        assert!(
            app.layout().leaves().contains(&pid),
            "the module pane is in the layout"
        );

        // A missing entrypoint is an error.
        assert!(app
            .module_open_pane("you.board", "nope", None, "test")
            .is_err());

        // Closing the pane untracks it.
        app.close_pane(pid);
        assert!(!app.panes.contains_key(&pid));
        assert!(!app.module_panes.contains_key(&pid), "record auto-removed");

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn event_hook_runs_with_event_env() {
        use std::time::{Duration, Instant};
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-evtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let dir = home.join("notify-mod");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("luvus-module.toml"),
            r#"
id = "you.notify"
name = "Notify"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[events]]
on = "pane.agent_status_changed"
command = ["sh", "-c", "echo event=$LUVUS_MODULE_EVENT json=$LUVUS_MODULE_EVENT_JSON"]
"#,
        )
        .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.module_link_with(&dir, true, None).unwrap();

        // Firing the event runs the hook (and publishes to subscribers).
        app.emit_event(
            "pane.agent_status_changed",
            serde_json::json!({"pane": "1", "status": "blocked", "agent": "claude"}),
        );
        let log_id = app
            .module_logs
            .iter()
            .find(|l| l.label == "event:pane.agent_status_changed")
            .map(|l| l.id)
            .expect("a hook command was queued");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                app.handle_event(ev);
            }
            let resolved = app
                .module_logs
                .iter()
                .find(|l| l.id == log_id)
                .is_some_and(|l| l.status != ModuleStatus::Running);
            if resolved || Instant::now() > deadline {
                break;
            }
        }

        let log = app.module_logs.iter().find(|l| l.id == log_id).unwrap();
        assert_eq!(log.status, ModuleStatus::Succeeded, "stderr: {}", log.err);
        assert!(
            log.out.contains("event=pane.agent_status_changed"),
            "event name injected: {:?}",
            log.out
        );
        assert!(
            log.out.contains("blocked"),
            "event json injected: {:?}",
            log.out
        );

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn module_pane_survives_snapshot_restore() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-restoretest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let dir = home.join("board-mod");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("luvus-module.toml"),
            r#"
id = "you.board"
name = "Board"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[panes]]
id = "board"
title = "Board"
command = ["sh", "-c", "sleep 5"]
"#,
        )
        .unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.module_link_with(&dir, true, None).unwrap();
        let pid = app
            .module_open_pane("you.board", "board", Some("split"), "test")
            .unwrap();
        assert!(app.module_panes.contains_key(&pid));

        // Snapshot, then restore into a fresh App.
        let snap = crate::persist::snapshot(&app);
        let (tx2, _rx2) = std::sync::mpsc::channel();
        let restored = App::from_snapshot(snap, tx2).expect("restore");

        // The module pane came back as a module pane (not a plain shell).
        let rec = restored
            .module_panes
            .iter()
            .find(|(_, r)| r.module_id == "you.board" && r.entrypoint == "board");
        assert!(rec.is_some(), "module pane was restored as a module pane");
        let (rid, _) = rec.unwrap();
        assert_eq!(
            restored.panes.get(rid).map(|p| p.command.as_str()),
            Some("sh"),
            "it re-ran the module command, not the login shell"
        );

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn settings_modules_tab_lists_and_toggles() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modtab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        for n in ["alpha", "beta"] {
            let dir = home.join(n);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("luvus-module.toml"),
                format!("id = \"you.{n}\"\nname = \"{n}\"\nversion = \"0.1.0\"\nmin_luvus_version = \"0.1.0\"\n"),
            )
            .unwrap();
        }

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.module_link_with(&home.join("alpha"), true, None)
            .unwrap();
        app.module_link_with(&home.join("beta"), true, None)
            .unwrap();

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.open_settings();
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('5'),
            KeyModifiers::NONE,
        ))); // Modules (Theme/Layout/Notify/Keys/Modules)
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        assert_eq!(app.settings_ctl_rects.len(), 2, "one row per module");
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("you.alpha") && text.contains("you.beta"));

        // Clicking a module row toggles its enabled flag (and persists).
        let before = app.modules.find("you.alpha").unwrap().enabled;
        let row = app
            .settings_ctl_rects
            .iter()
            .find(|(i, _)| *i == 0)
            .unwrap()
            .1;
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: row.x + 2,
            row: row.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_ne!(app.modules.find("you.alpha").unwrap().enabled, before);
        assert_eq!(
            crate::module::registry::load()
                .find("you.alpha")
                .unwrap()
                .enabled,
            !before
        );

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Link a module whose manifest is `toml`, in a scratch home.
    fn link(app: &mut App, home: &Path, name: &str, toml: &str) -> String {
        let dir = home.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("luvus-module.toml"), toml).unwrap();
        app.module_link_with(&dir, true, None).unwrap()
    }

    /// Pump the loop until `log_id` resolves (or we give up).
    fn settle(app: &mut App, rx: &std::sync::mpsc::Receiver<AppEvent>, log_id: u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                app.handle_event(ev);
            }
            let done = app
                .module_logs
                .iter()
                .find(|l| l.id == log_id)
                .is_some_and(|l| l.status != ModuleStatus::Running);
            if done || Instant::now() > deadline {
                return;
            }
        }
    }

    #[test]
    fn module_actions_appear_in_the_right_click_menus() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modmenu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        // Before any module, the menus are exactly the built-ins.
        let pane_builtins = app.pane_menu_items().len();
        let ws_builtins = app.ws_menu_items(0).len();

        let id = link(
            &mut app,
            &home,
            "ctx-mod",
            r#"
id = "you.ctx"
name = "Ctx"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[actions]]
id = "on-pane"
title = "Do pane thing"
contexts = ["pane"]
command = ["sh", "-c", "echo pane=$LUVUS_PANE_ID src=$LUVUS_MODULE_CONTEXT_JSON"]

[[actions]]
id = "on-node"
title = "Do node thing"
contexts = ["node"]
command = ["sh", "-c", "echo ws=$LUVUS_WORKSPACE_ID"]

[[actions]]
id = "headless"
title = "Never in a menu"
command = ["true"]
"#,
        );

        // Each menu offers only the actions declaring its context.
        assert_eq!(app.module_menu_actions("pane").len(), 1);
        assert_eq!(app.module_menu_actions("workspace").len(), 1, "node alias");
        assert_eq!(app.module_menu_actions("agent").len(), 0);

        // Opening a menu snapshots them, and adds a divider above the rows.
        let target = app.layout().focus;
        app.open_pane_menu(target, 1, 1);
        assert_eq!(app.pane_menu_items().len(), pane_builtins + 2);
        app.open_ws_menu(0, 1, 1);
        assert_eq!(app.ws_menu_items(0).len(), ws_builtins + 2);
        app.ws_menu = None;

        // The rows actually render, with the module's own title (never translated).
        {
            use ratatui::backend::TestBackend;
            use ratatui::Terminal;
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
            let text: String = term
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Do pane thing"), "the module row is drawn");
            assert!(
                !text.contains("Never in a menu"),
                "an action with no contexts stays out of the menu"
            );
            // Every row got a clickable rect, module rows included.
            let rects = app.pane_menu.as_ref().unwrap().items.len();
            assert_eq!(rects, app.pane_menu_items().len());
        }

        // Clicking the pane entry runs it against the right-clicked pane, and the
        // injected context names that pane rather than whatever had focus.
        app.pane_menu_action(PaneMenuItem::Module(0));
        assert!(app.pane_menu.is_none(), "the menu closed");
        let log_id = app
            .module_logs
            .iter()
            .find(|l| l.label == "action:on-pane")
            .map(|l| l.id)
            .expect("the action was queued");
        settle(&mut app, &rx, log_id);
        let log = app.module_logs.iter().find(|l| l.id == log_id).unwrap();
        assert_eq!(log.status, ModuleStatus::Succeeded, "stderr: {}", log.err);
        assert!(
            log.out.contains(&format!("pane={}", target.0)),
            "flat LUVUS_PANE_ID points at the clicked pane: {:?}",
            log.out
        );
        assert!(
            log.out.contains("\"invocation_source\":\"menu:pane\""),
            "the context records where it came from: {:?}",
            log.out
        );

        // Disabling the module retires its menu rows immediately.
        app.module_set_enabled(&id, false).unwrap();
        assert_eq!(app.module_menu_actions("pane").len(), 0);
        app.open_pane_menu(target, 1, 1);
        assert_eq!(app.pane_menu_items().len(), pane_builtins, "no module rows");

        // A module disabled *while its menu is open* must not shift what a click
        // runs: the snapshot still names the action, but it no longer resolves,
        // so the click is a no-op with a toast rather than a wrong action.
        app.module_set_enabled(&id, true).unwrap();
        app.open_pane_menu(target, 1, 1);
        assert_eq!(app.pane_menu.as_ref().unwrap().module_actions.len(), 1);
        app.module_set_enabled(&id, false).unwrap();
        let before = app.module_logs.len();
        app.pane_menu_action(PaneMenuItem::Module(0));
        assert_eq!(app.module_logs.len(), before, "nothing was run");
        assert!(app.toast.is_some(), "and the user was told why");

        // An index past the snapshot is ignored rather than panicking.
        app.open_pane_menu(target, 1, 1);
        app.pane_menu_action(PaneMenuItem::Module(99));

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn startup_hooks_run_once_and_again_after_re_enable() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modboot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = link(
            &mut app,
            &home,
            "boot-mod",
            r#"
id = "you.boot"
name = "Boot"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[startup]]
command = ["sh", "-c", "echo booted event=$LUVUS_MODULE_EVENT"]
"#,
        );

        // Linking already ran it, so the module's dock can paint straight away.
        let count = |a: &App| {
            a.module_logs
                .iter()
                .filter(|l| l.label == "startup")
                .count()
        };
        assert_eq!(count(&app), 1, "linking runs the hook");
        let log_id = app
            .module_logs
            .iter()
            .find(|l| l.label == "startup")
            .unwrap()
            .id;
        settle(&mut app, &rx, log_id);
        let log = app.module_logs.iter().find(|l| l.id == log_id).unwrap();
        assert_eq!(log.status, ModuleStatus::Succeeded, "stderr: {}", log.err);
        assert!(log.out.contains("event=startup"), "got: {:?}", log.out);

        // Re-running the sweep (a second server tick) must not double-fire it.
        app.run_module_startup_hooks();
        app.run_module_startup_hooks();
        assert_eq!(count(&app), 1, "once per process");

        // Disable then re-enable: the module gets a fresh chance to repaint.
        app.module_set_enabled(&id, false).unwrap();
        assert_eq!(count(&app), 1, "disabling runs nothing");
        app.module_set_enabled(&id, true).unwrap();
        assert_eq!(count(&app), 2, "re-enabling re-runs it");

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn declared_settings_reach_a_command_as_env() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modsetenv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        link(
            &mut app,
            &home,
            "cfg-mod",
            r#"
id = "you.cfg"
name = "Cfg"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[settings]]
key = "token"
title = "Token"
type = "string"
secret = true

[[settings]]
key = "limit"
title = "Limit"
type = "number"
default = 5
min = 1
max = 10

[[actions]]
id = "show"
title = "Show"
command = ["sh", "-c", "echo t=$LUVUS_SETTING_TOKEN l=$LUVUS_SETTING_LIMIT old_t=$BOHAY_SETTING_TOKEN old_l=$BOHAY_SETTING_LIMIT"]
"#,
        );

        // Defaults apply before the user touches anything.
        let vals = app.module_settings("you.cfg").unwrap();
        assert_eq!(vals.get("limit").unwrap(), 5);
        assert_eq!(vals.get("token").unwrap(), "");

        app.module_set_setting("you.cfg", "token", "abc123".into())
            .unwrap();
        // Over-max clamps instead of failing the whole write.
        assert_eq!(
            app.module_set_setting("you.cfg", "limit", 99.into())
                .unwrap(),
            10
        );

        let log_id = app.module_invoke_action("show", None, "test").unwrap();
        settle(&mut app, &rx, log_id);
        let log = app.module_logs.iter().find(|l| l.id == log_id).unwrap();
        assert_eq!(log.status, ModuleStatus::Succeeded, "stderr: {}", log.err);
        assert!(
            log.out.contains("t=abc123 l=10 old_t=abc123 old_l=10"),
            "settings reached the command: {:?}",
            log.out
        );

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn listing_settings_masks_secrets_but_get_returns_them() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modsec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        link(
            &mut app,
            &home,
            "sec-mod",
            r#"
id = "you.sec"
name = "Sec"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[settings]]
key = "token"
title = "Token"
type = "string"
secret = true

[[settings]]
key = "host"
title = "Host"
type = "string"
default = "example.com"
"#,
        );
        app.module_set_setting("you.sec", "token", "s3cret".into())
            .unwrap();

        let list = app
            .dispatch("module.settings.list", &json!({"id": "you.sec"}))
            .unwrap();
        let text = list.to_string();
        assert!(
            !text.contains("s3cret"),
            "a listing must not print a secret: {text}"
        );
        let entries = list["settings"].as_array().unwrap();
        let token = entries.iter().find(|e| e["key"] == "token").unwrap();
        assert_eq!(token["value"], Value::Null, "masked");
        assert_eq!(token["set"], true, "but reported as configured");
        // A non-secret is unaffected.
        let host = entries.iter().find(|e| e["key"] == "host").unwrap();
        assert_eq!(host["value"], "example.com");

        // Asking for the one key by name still returns it, for scripting.
        let got = app
            .dispatch(
                "module.settings.get",
                &json!({"id": "you.sec", "key": "token"}),
            )
            .unwrap();
        assert_eq!(got["value"], "s3cret");

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn item_platforms_gate_panes_and_events() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modplat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        link(
            &mut app,
            &home,
            "plat-mod",
            r#"
id = "you.plat"
name = "Plat"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[panes]]
id = "nope"
title = "Elsewhere only"
platforms = ["plan9"]
command = ["sh", "-c", "sleep 5"]

[[events]]
on = "tab.created"
platforms = ["plan9"]
command = ["sh", "-c", "echo should-not-run"]

# A module written against the old event spelling still fires.
[[events]]
on = "node.created"
command = ["sh", "-c", "echo legacy-alias-fired"]
"#,
        );

        // A pane entrypoint gated to another OS is simply not there.
        let err = app
            .module_open_pane("you.plat", "nope", None, "test")
            .unwrap_err();
        assert!(err.contains("no pane nope"), "got: {err}");

        // Nor does a gated event hook run.
        app.emit_event("tab.created", json!({"tab": "1"}));
        assert!(
            !app.module_logs
                .iter()
                .any(|l| l.label == "event:tab.created"),
            "a platform-gated hook stays out of the queue"
        );

        // `workspace.created` still reaches a hook declared as `node.created`.
        app.emit_event("workspace.created", json!({"workspace": "0"}));
        assert!(
            app.module_logs
                .iter()
                .any(|l| l.label == "event:workspace.created"),
            "the legacy node.* alias still fires"
        );

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn settings_tab_renders_and_edits_module_settings() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modsettab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        link(
            &mut app,
            &home,
            "ui-mod",
            r#"
id = "you.ui"
name = "Ui"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[settings]]
key = "loud"
title = "Play a sound"
type = "bool"

[[settings]]
key = "mode"
title = "Mode"
type = "enum"
options = ["fast", "slow"]

[[settings]]
key = "token"
title = "API token"
type = "string"
secret = true
"#,
        );

        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        app.open_settings();
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('5'),
            KeyModifiers::NONE,
        ))); // Modules
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let screen = |t: &Terminal<TestBackend>| -> String {
            t.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };
        let text = screen(&term);
        assert!(text.contains("you.ui"), "the module row renders");
        assert!(
            text.contains("Play a sound"),
            "its settings render under it"
        );
        assert!(text.contains("Mode"));
        // 1 module row + 3 settings rows.
        assert_eq!(app.module_rows().len(), 4);
        assert_eq!(app.settings_ctl_rects.len(), 4);

        // Row 2 is the enum: `›` steps it to the next option.
        assert_eq!(
            app.module_rows()[2],
            crate::app::ModuleRow::Setting(0, 1),
            "row 2 is the enum setting"
        );
        for _ in 0..2 {
            app.handle_event(AppEvent::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )));
        }
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        assert_eq!(
            app.module_settings("you.ui").unwrap().get("mode").unwrap(),
            "slow"
        );

        // Row 3 is the secret string: Enter opens the prompt, typed text saves,
        // and the screen shows bullets rather than the value.
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(app.module_setting_edit.is_some(), "the prompt opened");
        for c in "hunter2".chars() {
            app.handle_event(AppEvent::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let typed = screen(&term);
        assert!(typed.contains("•••••••"), "a secret echoes as bullets");
        assert!(!typed.contains("hunter2"), "and never in the clear");

        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(app.module_setting_edit.is_none(), "the prompt closed");
        assert_eq!(
            app.module_settings("you.ui").unwrap().get("token").unwrap(),
            "hunter2"
        );

        // Disabling the module collapses its settings rows back to one.
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )));
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )));
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )));
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(!app.modules.find("you.ui").unwrap().enabled);
        assert_eq!(
            app.module_rows().len(),
            1,
            "settings collapse when disabled"
        );
        // The cursor came back inside the shrunken list.
        assert!(app.settings.as_ref().unwrap().cursor < app.module_rows().len());
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn modules_resolve_by_owner_repo_as_well_as_id() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modspec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        let dir = home.join("spec-mod");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("luvus-module.toml"),
            r#"
id = "example.agent-ping"
name = "Ping"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[settings]]
key = "token"
title = "Token"
type = "string"
"#,
        )
        .unwrap();
        // Registered as if installed from GitHub: source is `<spec>@<sha>`.
        app.module_link_with(&dir, true, Some("Riz/luvus-agent-ping@abc123".into()))
            .unwrap();

        // Both names resolve to the same module.
        assert_eq!(
            app.module_id_for("example.agent-ping").unwrap(),
            "example.agent-ping"
        );
        assert_eq!(
            app.module_id_for("Riz/luvus-agent-ping").unwrap(),
            "example.agent-ping"
        );
        // GitHub owner/repo is case-insensitive.
        assert_eq!(
            app.module_id_for("riz/LUVUS-agent-ping").unwrap(),
            "example.agent-ping"
        );
        // A trailing slash is forgiven.
        assert!(app.module_id_for("Riz/luvus-agent-ping/").is_ok());
        // Something that matches neither is still an error.
        assert!(app.module_id_for("someone/else").is_err());
        assert!(app.module_id_for("no.such.module").is_err());

        // Settings and the config dir key off the *canonical* id either way, so
        // `owner/repo` can't create a second directory beside the real one.
        app.module_set_setting("Riz/luvus-agent-ping", "token", "t1".into())
            .unwrap();
        assert_eq!(
            app.module_settings("example.agent-ping")
                .unwrap()
                .get("token")
                .unwrap(),
            "t1"
        );
        assert_eq!(
            app.module_config_dir("Riz/luvus-agent-ping").unwrap(),
            app.module_config_dir("example.agent-ping").unwrap()
        );

        // Enable/disable resolves too.
        app.module_set_enabled("Riz/luvus-agent-ping", false)
            .unwrap();
        assert!(!app.modules.find("example.agent-ping").unwrap().enabled);
        app.module_set_enabled("Riz/luvus-agent-ping", true)
            .unwrap();

        // And unlink by owner/repo actually removes it, rather than reporting
        // success while the module stays registered.
        app.module_unlink("Riz/luvus-agent-ping").unwrap();
        assert!(app.modules.find("example.agent-ping").is_none());
        assert!(app.module_unlink("Riz/luvus-agent-ping").is_err());

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_linked_module_has_no_source_to_resolve_by() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modnosrc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        link(
            &mut app,
            &home,
            "local-mod",
            "id = \"you.local\"\nname = \"Local\"\nversion = \"0.1.0\"\nmin_luvus_version = \"0.1.0\"\n",
        );

        // A locally linked module was never installed from anywhere, so only its
        // id names it. This must not panic or match some other module.
        assert!(app.module_id_for("you.local").is_ok());
        assert!(app.module_id_for("someone/you.local").is_err());

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn git_install_builds_and_uninstall_removes_checkout() {
        use std::process::Command;
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-gittest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        // A local "remote" git repo with a manifest + a build step.
        let remote = home.join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::write(
            remote.join("luvus-module.toml"),
            r#"
id = "you.installed"
name = "Installed"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[build]]
command = ["sh", "-c", "touch built.txt"]

[[actions]]
id = "hello"
title = "Hello"
command = ["echo", "hi"]
"#,
        )
        .unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&remote)
                .output()
                .expect("git available")
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ]);

        // Install from the local repo (file:// supports --depth).
        let url = format!("file://{}", remote.display());
        let installed = crate::module::install::install(&url, None, true).expect("install");
        assert_eq!(installed.id, "you.installed");
        assert!(
            installed.source.contains('@'),
            "pinned source: {}",
            installed.source
        );
        assert!(installed.root.exists());
        assert!(
            crate::module::install::is_removable(&installed.root),
            "landed in the managed dir"
        );
        assert!(
            installed.root.join("built.txt").exists(),
            "the [[build]] step ran"
        );

        // Register + uninstall via the App; the checkout is deleted.
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.module_link_with(&installed.root, true, Some(installed.source.clone()))
            .unwrap();
        assert!(app.modules.find("you.installed").is_some());
        // Uninstall by the *source* it was installed with, not its id: that is
        // the name the user typed, so it is the one they will remember.
        let spec = installed.source.rsplit_once('@').unwrap().0.to_string();
        assert!(app.modules.find(&spec).is_some(), "resolvable by source");
        app.module_uninstall(&spec).unwrap();
        assert!(app.modules.find("you.installed").is_none());
        assert!(!installed.root.exists(), "managed checkout removed");

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
